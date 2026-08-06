//! Fase 3, passo 1: quanto tempo a captura fica cega num prompt de UAC?
//!
//! Quando o Windows troca para o Secure Desktop (`Winlogon`), um processo
//! comum deixa de ver a tela. O técnico do outro lado vê a imagem congelar
//! exatamente no instante em que precisaria clicar. Isso está documentado no
//! projeto desde o primeiro dia — mas como **suposição**, nunca como
//! observação.
//!
//! Esta ferramenta mede. O número que importa é a **janela de cegueira**: do
//! último quadro entregue antes do prompt até o primeiro quadro depois dele.
//! É exatamente o que o usuário do outro lado sente.
//!
//!     cargo run --release -p pcshare-capture --bin uacwatch
//!     cargo run --release -p pcshare-capture --bin uacwatch -- --manual
//!
//! O modo padrão dispara o prompt sozinho — basta clicar **Não**. O `--manual`
//! só observa, para medir a tela de bloqueio (Win+L) ou o Ctrl+Alt+Del, que
//! não dá para acionar por programa.

use std::time::{Duration, Instant};

use pcshare_capture::desktop;
use pcshare_capture::{CaptureError, DesktopCapture};

/// Espera curta por quadro: queremos consultar o desktop com frequência alta,
/// para que a resolução da medição seja melhor que um intervalo de tela.
const TIMEOUT_MS: u32 = 15;
const LIMITE: Duration = Duration::from_secs(90);
/// Tempo observando antes de disparar o prompt, para a linha de base assentar.
const ASSENTAR: Duration = Duration::from_secs(3);

fn main() {
    let manual = std::env::args().any(|a| a == "--manual");

    println!("PCShare — Fase 3: medindo a cegueira no Secure Desktop\n");

    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("falha ao abrir a captura: {e}");
            std::process::exit(1);
        }
    };
    println!("captura aberta: {}x{}", cap.width(), cap.height());
    let (sessao, console) = desktop::sessions();
    println!("sessão do processo: {sessao} | sessão do console: {console}");
    println!("desktop inicial: {}\n", desktop::input_desktop().label());

    if manual {
        println!("MODO MANUAL — acione você mesmo: Win+L, Ctrl+Alt+Del, ou");
        println!("qualquer ação que peça elevação. Encerra sozinho depois.\n");
    } else {
        println!("Vou abrir um prompt de UAC em {}s. **Clique em Não.**", ASSENTAR.as_secs());
        println!("Nada será executado — só preciso que o prompt apareça.\n");
    }

    let t0 = Instant::now();
    let mut eventos: Vec<(f64, String)> = Vec::new();
    let anotar = |eventos: &mut Vec<(f64, String)>, texto: String| {
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        println!("  [{ms:>8.1} ms] {texto}");
        eventos.push((ms, texto));
    };

    let mut desktop_anterior = desktop::input_desktop();
    let mut ultimo_quadro: Option<Instant> = None;
    let mut inicio_cegueira: Option<Instant> = None;
    let mut fim_cegueira: Option<Instant> = None;
    let mut inicio_seguro: Option<Instant> = None;
    let mut fim_seguro: Option<Instant> = None;
    let mut access_lost = 0u32;
    let mut recover_ok = 0u32;
    let mut recover_falhou = 0u32;
    let mut quadros = 0u64;
    let mut disparado = manual;

    while t0.elapsed() < LIMITE {
        // ---- estado do desktop
        let agora_desktop = desktop::input_desktop();
        if agora_desktop != desktop_anterior {
            anotar(
                &mut eventos,
                format!(
                    "desktop: {} -> {}",
                    desktop_anterior.label(),
                    agora_desktop.label()
                ),
            );
            if agora_desktop.is_secure() && inicio_seguro.is_none() {
                inicio_seguro = Some(Instant::now());
                // A cegueira começa no último quadro que chegou a ser
                // entregue, não agora: é dali que a tela remota congelou.
                inicio_cegueira = ultimo_quadro.or_else(|| Some(Instant::now()));
                anotar(&mut eventos, "  ^ Secure Desktop assumiu".into());
                anotar(
                    &mut eventos,
                    format!("  ^ captura ativa? {}", cap.is_active()),
                );
            }
            if !agora_desktop.is_secure() && inicio_seguro.is_some() && fim_seguro.is_none() {
                fim_seguro = Some(Instant::now());
                anotar(&mut eventos, "  ^ voltou ao desktop do usuário".into());
            }
            desktop_anterior = agora_desktop;
        }

        // ---- estado da captura
        match cap.next_frame(TIMEOUT_MS) {
            Ok(Some(_)) => {
                quadros += 1;
                ultimo_quadro = Some(Instant::now());

                // A cegueira só termina depois que o Secure Desktop sai.
                //
                // Um quadro que chega **durante** o prompt não significa que
                // estamos vendo a tela: é o desktop do usuário, que continua
                // lá atrás, congelado. A primeira versão desta medição contou
                // esse quadro como fim da cegueira e reportou 3 ms — número
                // que não descrevia nada.
                if fim_seguro.is_some() && fim_cegueira.is_none() {
                    fim_cegueira = ultimo_quadro;
                    anotar(
                        &mut eventos,
                        "  ^ primeiro quadro DEPOIS de o desktop voltar".into(),
                    );
                }
            }
            // Timeout é o normal numa tela parada; não diz nada sozinho.
            Ok(None) => {}
            Err(CaptureError::AccessLost) => {
                access_lost += 1;
                if access_lost <= 3 || access_lost % 50 == 0 {
                    anotar(&mut eventos, format!("AccessLost #{access_lost}"));
                }
                match cap.recover() {
                    Ok(()) => {
                        recover_ok += 1;
                        anotar(
                            &mut eventos,
                            format!("  ^ recover() teve sucesso (tentativa {})", recover_falhou + 1),
                        );
                    }
                    Err(e) => {
                        recover_falhou += 1;
                        // Falhar aqui é o esperado enquanto o Secure Desktop
                        // está na frente. Insistir é o comportamento correto —
                        // e é o que um host de verdade fará.
                        if recover_falhou <= 3 || recover_falhou % 50 == 0 {
                            anotar(
                                &mut eventos,
                                format!("  ^ recover() falhou #{recover_falhou}: {e}"),
                            );
                        }
                    }
                }
            }
            Err(e) => {
                // Não interrompe mais a medição: o episódio precisa ser
                // observado até o fim, e foi justamente aqui que a primeira
                // tentativa abortou sem medir a recuperação.
                anotar(&mut eventos, format!("erro inesperado: {e}"));
                let _ = cap.recover();
            }
        }

        // ---- dispara o prompt depois da linha de base
        if !disparado && t0.elapsed() >= ASSENTAR {
            disparado = true;
            anotar(&mut eventos, "disparando prompt de UAC (clique em Não)".into());
            disparar_uac();
        }

        // ---- encerra quando o episódio fechou
        if let (Some(_), Some(fim)) = (inicio_seguro, fim_cegueira)
            && fim_seguro.is_some()
            && fim.elapsed() > Duration::from_secs(2)
        {
            break;
        }
    }

    relatorio(
        &eventos,
        inicio_cegueira,
        fim_cegueira,
        inicio_seguro,
        fim_seguro,
        quadros,
        access_lost,
        recover_ok,
        recover_falhou,
        manual,
    );
}

/// Pede elevação para um comando que não faz nada.
///
/// `cmd /c exit` é inofensivo mesmo se o usuário clicar em Sim por engano: o
/// processo elevado encerra imediatamente sem tocar em nada. O que interessa é
/// só o prompt aparecer, porque é ele que troca o desktop.
fn disparar_uac() {
    let r = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Start-Process cmd -ArgumentList '/c exit' -Verb RunAs -WindowStyle Hidden",
        ])
        .spawn();
    if let Err(e) = r {
        eprintln!("não foi possível disparar o prompt: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
fn relatorio(
    eventos: &[(f64, String)],
    inicio_cegueira: Option<Instant>,
    fim_cegueira: Option<Instant>,
    inicio_seguro: Option<Instant>,
    fim_seguro: Option<Instant>,
    quadros: u64,
    access_lost: u32,
    recover_ok: u32,
    recover_falhou: u32,
    manual: bool,
) {
    println!("\n{:=<64}", "");
    println!("RESULTADO");
    println!("{:=<64}", "");

    if inicio_seguro.is_none() {
        println!("  O Secure Desktop nunca assumiu durante a medição.");
        if manual {
            println!("  Modo manual: era preciso acionar Win+L ou Ctrl+Alt+Del.");
        } else {
            println!("  O prompt de UAC não apareceu, ou apareceu e sumiu rápido");
            println!("  demais para ser detectado. Vale repetir.");
        }
        println!("\n  Quadros capturados: {quadros}");
        return;
    }

    match (inicio_cegueira, fim_cegueira) {
        (Some(i), Some(f)) => {
            let ms = f.duration_since(i).as_secs_f64() * 1000.0;
            println!("  JANELA DE CEGUEIRA : {ms:.0} ms");
            println!("     Do último quadro antes do prompt até o primeiro quadro");
            println!("     depois de o desktop do usuário voltar. É o tempo em que");
            println!("     a tela remota fica congelada e o clique não chega.");
        }
        (Some(_), None) => {
            println!("  JANELA DE CEGUEIRA : não fechou dentro do limite.");
            println!("     A captura não voltou a entregar quadros. É o pior caso.");
        }
        _ => println!("  Não foi possível delimitar a janela de cegueira."),
    }

    if let (Some(i), Some(f)) = (inicio_seguro, fim_seguro) {
        println!(
            "\n  Secure Desktop ativo : {:.0} ms",
            f.duration_since(i).as_secs_f64() * 1000.0
        );
    } else if inicio_seguro.is_some() {
        println!("\n  Secure Desktop ainda estava ativo ao fim da medição.");
    }

    println!("\n  Quadros capturados   : {quadros}");
    println!("  AccessLost           : {access_lost}");
    println!("  recover() com sucesso: {recover_ok}");
    println!("  recover() falhou     : {recover_falhou}");

    println!("\n  Leitura:");
    if recover_falhou > 0 {
        println!("  · recover() falhou {recover_falhou}x com acesso negado enquanto o Secure");
        println!("    Desktop esteve na frente. Confirma que um processo comum");
        println!("    não consegue duplicá-lo, e que a saída é a arquitetura");
        println!("    serviço + agente da decisão D5.");
    }
    if recover_ok > 0 {
        println!("  · recover() voltou a funcionar assim que o desktop do usuário");
        println!("    retornou — insistir é a estratégia correta no host.");
    }
    if access_lost == 0 {
        println!("  · Sem AccessLost: a duplicação não chegou a ser invalidada.");
    }

    println!("\n  Linha do tempo ({} eventos):", eventos.len());
    for (ms, texto) in eventos {
        println!("    {ms:>9.1} ms  {texto}");
    }
}
