//! Supervisor do PCShare — quem lança os agentes.
//!
//! # O modelo, e por que ele é assim
//!
//! A captura via Desktop Duplication só funciona quando o processo está preso
//! ao desktop que **está recebendo entrada**. Não é o monitor que manda, é o
//! desktop: com o `Winlogon` à frente, um processo do `Default` recebe acesso
//! negado. Medido, 180 tentativas, todas falharam — ver `docs/FASE-3.md`.
//!
//! E um processo do Windows nasce preso ao desktop indicado em
//! `STARTUPINFO.lpDesktop`, sem trocar depois. Daí a arquitetura: quando o
//! desktop de entrada muda, o supervisor **lança outro agente**, amarrado ao
//! novo desktop. Não há como reconfigurar o que já está rodando.
//!
//! # O que dá para provar sem privilégio elevado
//!
//! Tudo, menos o `Winlogon` em si:
//!
//! - lançar um agente amarrado a um desktop nomeado — mecânica idêntica;
//! - que a captura funciona quando esse desktop é o de entrada;
//! - que a captura **falha** quando não é.
//!
//! O último é o controle negativo, e vale tanto quanto o positivo: sem ele,
//! um teste que passa não distingue "a amarração funciona" de "a amarração é
//! irrelevante".
//!
//! Só o acesso ao `Winlogon` exige SYSTEM, e portanto o serviço instalado.
//!
//!     cargo run --release -p pcshare-service -- experimento

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEWINDOW, DESKTOP_SWITCHDESKTOP,
    DESKTOP_WRITEOBJECTS, SwitchDesktop,
};
use windows::Win32::System::Threading::{
    CreateProcessW, INFINITE, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, STARTUPINFOW,
    WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use pcshare_capture::desktop;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("experimento") => experimento(),
        _ => {
            println!("PCShare — supervisor de agentes\n");
            println!("  experimento   prova o lançamento amarrado a desktop");
            println!("\nO modo de serviço, que exige instalação com elevação,");
            println!("vem no próximo passo.");
        }
    }
}

/// Prova, sem elevação, que a amarração a desktop é o que decide a captura.
fn experimento() {
    println!("PCShare — provando o lançamento de agente amarrado a desktop\n");

    let agente = match caminho_do_agente() {
        Some(p) => p,
        None => {
            eprintln!("pcshare-agent.exe não encontrado ao lado deste executável.");
            eprintln!("Compile o workspace inteiro: cargo build --release --workspace");
            return;
        }
    };
    println!("agente: {}\n", agente.display());
    println!("desktop de entrada agora: {}\n", desktop::input_desktop().label());

    // --- Caso 1: o desktop do usuário, que é o de entrada.
    println!("{:-<62}", "");
    println!("CASO 1 — agente no 'Default' (que É o desktop de entrada)");
    println!("Esperado: a captura funciona.");
    println!("{:-<62}", "");
    lancar_e_esperar(&agente, "Default", "no-Default", 3);

    // --- Caso 2: um desktop nosso, que existe mas não recebe entrada.
    println!("\n{:-<62}", "");
    println!("CASO 2 — agente num desktop que NÃO é o de entrada");
    println!("Esperado: a captura falha com acesso negado.");
    println!("É o controle negativo: sem ele, o caso 1 não prova nada.");
    println!("{:-<62}", "");

    let nome = "PcshareExperimento";
    let nome_w = wide(nome);
    let hdesk = unsafe {
        CreateDesktopW(
            PCWSTR(nome_w.as_ptr()),
            PCWSTR::null(),
            None,
            Default::default(),
            DESKTOP_CREATEWINDOW.0 | DESKTOP_WRITEOBJECTS.0 | DESKTOP_SWITCHDESKTOP.0,
            None,
        )
    };
    match hdesk {
        Ok(h) => {
            lancar_e_esperar(&agente, nome, "no-desktop-isolado", 3);
            unsafe {
                let _ = CloseDesktop(h);
            }
        }
        Err(e) => eprintln!("não foi possível criar o desktop de teste: {e}"),
    }

    println!("\n{:=<62}", "");
    println!("LEITURA");
    println!("{:=<62}", "");
    println!("Se o caso 1 capturou e o caso 2 falhou, está provado que o");
    println!("desktop ao qual o processo está preso é o que decide — e não");
    println!("o monitor, nem o usuário, nem a sessão.");
    println!();
    println!("É a base da solução: quando o Secure Desktop assume, o serviço");
    println!("lança um agente amarrado ao 'Winlogon'. Esse último passo exige");
    println!("SYSTEM, e portanto o serviço instalado.");
}

/// Lança o agente preso a `desktop` e espera ele terminar.
///
/// O campo que importa é `lpDesktop`: é ele que amarra o processo. Herdamos os
/// descritores para que a saída do agente apareça neste console.
fn lancar_e_esperar(exe: &std::path::Path, desktop: &str, rotulo: &str, segundos: u64) {
    let mut linha = wide(&format!(
        "\"{}\" --rotulo {rotulo} --segundos {segundos}",
        exe.display()
    ));
    let mut desk = wide(desktop);

    let mut si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desk.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let inicio = Instant::now();
    let r = unsafe {
        CreateProcessW(
            PCWSTR::null(),
            Some(PWSTR(linha.as_mut_ptr())),
            None,
            None,
            true,
            PROCESS_CREATION_FLAGS(0),
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    };
    si.lpDesktop = PWSTR::null();

    match r {
        Ok(()) => {
            println!(
                "  lançado em {:.1} ms (pid {})\n",
                inicio.elapsed().as_secs_f64() * 1000.0,
                pi.dwProcessId
            );
            unsafe {
                WaitForSingleObject(pi.hProcess, INFINITE);
                let _ = CloseHandle(pi.hThread);
                let _ = CloseHandle(pi.hProcess);
            }
        }
        Err(e) => println!("  FALHOU ao lançar no desktop '{desktop}': {e}"),
    }
}

fn caminho_do_agente() -> Option<std::path::PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let p = dir.join("pcshare-agent.exe");
    p.exists().then_some(p)
}

/// UTF-16 terminado em zero, como as APIs W do Windows exigem.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reservado para o passo seguinte: trocar o desktop de entrada exige o
/// privilégio que só o serviço terá.
#[allow(dead_code)]
unsafe fn trocar_desktop(h: HANDLE) {
    unsafe {
        let _ = SwitchDesktop(std::mem::transmute(h));
    }
}
