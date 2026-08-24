//! Quanto custa ficar cego, por hora.
//!
//! O agente que perde o desktop tenta `recover()` a cada 50 ms, para sempre.
//! Num prompt de UAC de dois segundos são 40 tentativas e ninguém nota. Numa
//! tela bloqueada a noite inteira são 72 mil por hora — e desde que o
//! `Winlogon` passou a ser pré-aquecido existe um agente cego **o tempo
//! todo**, não só durante bloqueios.
//!
//! Isso importa porque o produto roda na máquina de outra pessoa, que vai
//! reparar num processo consumindo CPU sem ninguém assistindo.
//!
//!     cargo run --release -p pcshare-capture --bin custo-cego -- 200

use pcshare_capture::DesktopCapture;
use std::time::Instant;
use windows::Win32::System::StationsAndDesktops::{
    CreateDesktopW, DESKTOP_CREATEWINDOW, SetThreadDesktop,
};
use windows::core::PCWSTR;

/// Prende esta thread a um desktop que **não** recebe entrada.
///
/// É o que reproduz, sem serviço e sem elevação, a negação que o agente do
/// `Winlogon` sofre enquanto o usuário está no desktop dele. Medir o caminho
/// que dá certo responderia a pergunta errada: o custo que importa é o de ser
/// recusado, que é o que acontece o tempo todo.
fn prender_a_desktop_inalcancavel() -> bool {
    let nome: Vec<u16> = "pcshare-medicao\0".encode_utf16().collect();
    unsafe {
        let Ok(d) = CreateDesktopW(
            PCWSTR(nome.as_ptr()),
            PCWSTR::null(),
            None,
            Default::default(),
            DESKTOP_CREATEWINDOW.0,
            None,
        ) else {
            return false;
        };
        SetThreadDesktop(d).is_ok()
    }
}

fn main() {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200);

    if cfg!(debug_assertions) {
        eprintln!("AVISO: build de debug. Use --release.\n");
    }

    let gpu = match pcshare_capture::GpuContext::new() {
        Ok(g) => g,
        Err(e) => {
            eprintln!("sem GPU: {e}");
            return;
        }
    };

    // Nasce cega para que a primeira tentativa já seja igual às outras.
    let mut cap = match DesktopCapture::cega(gpu, 0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sem saída de vídeo: {e}");
            return;
        }
    };

    let isolado = prender_a_desktop_inalcancavel();
    println!("PCShare — custo de uma tentativa de recuperar a captura\n");
    println!(
        "  desktop de medição: {}",
        if isolado {
            "isolado (a duplicação deve ser negada)"
        } else {
            "NÃO consegui isolar"
        }
    );

    let mut amostras: Vec<f64> = Vec::with_capacity(n as usize);
    let mut falhas = 0u32;
    for _ in 0..n {
        let t = Instant::now();
        let ok = cap.recover().is_ok();
        amostras.push(t.elapsed().as_secs_f64() * 1e6);
        if !ok {
            falhas += 1;
        }
    }

    amostras.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = amostras[amostras.len() / 2];
    let p99 = amostras[(amostras.len() * 99 / 100).min(amostras.len() - 1)];

    // 50 ms entre tentativas é o `BACKOFF_CEGO` do agente.
    let por_hora = 3600.0 / 0.050;
    let segundos_cpu = p50 * 1e-6 * por_hora;

    println!("  {n} tentativas, {falhas} negadas\n");
    println!("  p50 {p50:.0} us   p99 {p99:.0} us");
    println!("  mín {:.0} us   máx {:.0} us\n", amostras[0], amostras[amostras.len() - 1]);
    println!("  A 50 ms por tentativa, uma hora cego custa:");
    println!("    {por_hora:.0} tentativas");
    println!(
        "    {segundos_cpu:.1} s de CPU  ({:.2}% de um núcleo)",
        segundos_cpu / 36.0
    );

    if falhas == n {
        println!("\n  Todas negadas: é o caminho do agente cego de verdade.");
    } else if falhas == 0 {
        println!("\n  ATENÇÃO: nenhuma negada. Isto mediu o caminho que dá certo,");
        println!("  não o do agente cego, e o número não responde a pergunta.");
    } else {
        println!("\n  ATENÇÃO: {falhas} de {n} negadas — mistura de caminhos.");
    }
}
