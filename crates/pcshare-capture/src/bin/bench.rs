//! Sprint 0, etapa 1: captura DXGI pura, descartando os frames.
//!
//! Responde a duas perguntas antes de investir em encoder ou rede:
//!   1. Quantos frames por segundo a Desktop Duplication entrega nesta máquina?
//!   2. Quantas dirty rects vêm por frame, e que fração da tela elas cobrem?
//!
//! A segunda pergunta importa mais. Se numa tela de escritório parada as
//! dirty rects cobrem uma fração pequena, a estratégia de codificar só o que
//! mudou se sustenta — e todo o orçamento de banda do produto depende disso.
//!
//! Rodar em release; em debug as medições não significam nada:
//!     cargo run --release -p pcshare-capture --bin bench -- 20

use pcshare_capture::{CaptureError, DesktopCapture};
use std::time::{Duration, Instant};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);

    if cfg!(debug_assertions) {
        eprintln!("AVISO: build de debug. Use --release ou ignore os números.\n");
    }

    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Falha ao abrir a captura: {e}");
            std::process::exit(1);
        }
    };

    let (w, h) = (cap.width(), cap.height());
    let screen_px = u64::from(w) * u64::from(h);

    println!("PCShare — Sprint 0, etapa 1: captura DXGI pura");
    println!("Monitor 0: {w}x{h}  ({screen_px} px)");
    println!("Medindo por {secs}s.\n");
    println!("Durante a medição: deixe a tela parada uns segundos, depois role");
    println!("uma página, arraste uma janela e rode um vídeo em tela cheia.\n");

    let mut m = Metrics::default();
    let start = Instant::now();
    let deadline = start + Duration::from_secs(secs);
    let mut next_tick = start + Duration::from_secs(1);
    let mut tick = Tick::default();

    while Instant::now() < deadline {
        let t0 = Instant::now();
        let result = cap.next_frame(100);
        let poll_ms = t0.elapsed().as_secs_f64() * 1000.0;

        match result {
            Ok(Some(f)) if f.mouse_only => {
                m.mouse_only += 1;
                tick.mouse_only += 1;
            }
            Ok(Some(f)) => {
                m.frames += 1;
                m.rects += f.dirty_rects.len() as u64;
                m.dirty_px += f.dirty_pixels;
                m.accumulated += u64::from(f.accumulated_frames.saturating_sub(1));
                m.poll_ms.push(poll_ms);
                m.max_rects = m.max_rects.max(f.dirty_rects.len() as u64);

                tick.frames += 1;
                tick.rects += f.dirty_rects.len() as u64;
                tick.dirty_px += f.dirty_pixels;
            }
            Ok(None) => {
                m.timeouts += 1;
                tick.timeouts += 1;
            }
            Err(CaptureError::AccessLost) => {
                // Esperado ao passar por UAC, troca de resolução ou tela cheia
                // exclusiva. Um produto real precisa sobreviver a isso.
                m.access_lost += 1;
                if let Err(e) = cap.recover() {
                    eprintln!("Não foi possível recriar a captura: {e}");
                    break;
                }
            }
            Err(e) => {
                eprintln!("Erro na captura: {e}");
                break;
            }
        }

        if Instant::now() >= next_tick {
            let t = start.elapsed().as_secs();
            let area = pct(tick.dirty_px, screen_px * tick.frames.max(1));
            println!(
                "  t={t:>3}s  frames={:>4}  timeout={:>3}  só-mouse={:>3}  \
                 rects/frame={:>5.1}  área={area:>5.1}%",
                tick.frames,
                tick.timeouts,
                tick.mouse_only,
                ratio(tick.rects, tick.frames),
            );
            tick = Tick::default();
            next_tick += Duration::from_secs(1);
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    m.poll_ms
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    println!("\n{:=<64}", "");
    println!("RESUMO  ({elapsed:.1}s, {w}x{h})");
    println!("{:=<64}", "");
    println!("  Frames com mudança real : {}", m.frames);
    println!("  Frames por segundo      : {:.1}", m.frames as f64 / elapsed);
    println!(
        "  Timeouts (tela parada)  : {}  — {} são desejáveis: banda zero",
        m.timeouts,
        if m.timeouts > 0 { "estes" } else { "nenhum;" }
    );
    println!("  Só-mouse (sem redesenho): {}", m.mouse_only);
    println!("  Frames perdidos/acumul. : {}", m.accumulated);
    println!("  Sessões perdidas (UAC…) : {}", m.access_lost);
    println!();
    println!("  Dirty rects por frame   : {:.1} (máx {})", ratio(m.rects, m.frames), m.max_rects);
    println!(
        "  Área média alterada     : {:.1}% da tela",
        pct(m.dirty_px, screen_px * m.frames.max(1))
    );
    println!();
    println!("  Tempo em next_frame     : p50 {:.2} ms   p99 {:.2} ms   máx {:.2} ms",
        percentile(&m.poll_ms, 0.50),
        percentile(&m.poll_ms, 0.99),
        m.poll_ms.last().copied().unwrap_or(0.0),
    );

    println!("\nLeitura:");
    println!("  · 'Área média' baixa (< 20%) valida a estratégia de dirty rects.");
    println!("  · 'Frames perdidos' alto significa que não consumimos no ritmo");
    println!("    da tela — vira latência assim que o encoder entrar no caminho.");
    println!("  · 'Tempo em next_frame' inclui a espera pelo próximo redesenho;");
    println!("    não é custo de CPU. O que importa é o p99 sob tela ativa.");
}

#[derive(Default)]
struct Metrics {
    frames: u64,
    timeouts: u64,
    mouse_only: u64,
    accumulated: u64,
    access_lost: u64,
    rects: u64,
    max_rects: u64,
    dirty_px: u64,
    poll_ms: Vec<f64>,
}

#[derive(Default)]
struct Tick {
    frames: u64,
    timeouts: u64,
    mouse_only: u64,
    rects: u64,
    dirty_px: u64,
}

fn ratio(num: u64, den: u64) -> f64 {
    if den == 0 { 0.0 } else { num as f64 / den as f64 }
}

fn pct(num: u64, den: u64) -> f64 {
    if den == 0 { 0.0 } else { num as f64 * 100.0 / den as f64 }
}

/// `sorted` precisa estar ordenado. Retorna 0.0 se vazio.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}
