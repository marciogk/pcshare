//! O que a captura enxerga durante uma tela bloqueada.
//!
//! O teste de 19/08/2026 mostrou que, com a máquina bloqueada, o viewer remoto
//! vê **preto** — e não a tela de bloqueio. Três coisas ficaram por explicar, e
//! todas são suposição enquanto ninguém as registrar:
//!
//! 1. O que o detector de desktop responde durante o bloqueio? A medição diz
//!    `Default`, o que contraria a expectativa de que a tela de bloqueio viva
//!    no `Winlogon`.
//! 2. A duplicação continua funcionando, ou passa a falhar em silêncio?
//! 3. O conteúdo é mesmo preto, ou é a última imagem congelada?
//!
//! A terceira distinção importa mais do que parece. Congelado significa que a
//! captura parou; preto significa que ela está entregando preto — e são
//! problemas diferentes, com soluções diferentes.
//!
//!     cargo run --release -p pcshare-capture --bin bloqueio -- 120
//!
//! Rode, bloqueie com Win+L, espere, desbloqueie. O relatório sai ao final e
//! também vai para `%ProgramData%\PCShare\bloqueio.tsv`.

use std::io::Write;
use std::time::{Duration, Instant};

use pcshare_capture::linha_do_tempo::hora_local;
use pcshare_capture::{CaptureError, DesktopCapture, desktop};

/// Amostragem folgada: aqui não se mede latência, se observa estado.
const INTERVALO: Duration = Duration::from_millis(250);

fn main() {
    let segundos: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(120);

    let caminho = std::path::PathBuf::from(
        std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()),
    )
    .join("PCShare")
    .join("bloqueio.tsv");
    let mut arquivo = std::fs::File::create(&caminho).ok();

    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("captura não abriu: {e}");
            return;
        }
    };

    println!("PCShare — o que a captura enxerga durante o bloqueio\n");
    println!("  {}x{}, {segundos} s de observação", cap.width(), cap.height());
    println!("  Bloqueie com Win+L, espere, desbloqueie.");
    println!("  Registro em {}\n", caminho.display());

    let cabecalho = "hora\tdesktop\tcaptura\tluminancia\tobservacao";
    println!("{cabecalho}");
    if let Some(f) = arquivo.as_mut() {
        let _ = writeln!(f, "{cabecalho}");
    }

    let ate = Instant::now() + Duration::from_secs(segundos);
    let mut bgra = Vec::new();
    let mut anterior = String::new();

    while Instant::now() < ate {
        let d = desktop::input_desktop();
        let nome = d.label().to_string();

        // O estado da captura, **com o motivo**. O agente descarta esse erro
        // (`let _ = cap.recover()`), e por isso ninguém sabia se a recusa
        // durante o bloqueio é a mesma do prompt de UAC ou outra coisa.
        let (estado, luz) = match cap.next_frame(50) {
            Ok(Some(_)) => {
                let l = match cap.copy_bgra(&mut bgra) {
                    Ok(true) => luminancia(&bgra),
                    _ => -1.0,
                };
                ("quadro-novo".to_string(), l)
            }
            Ok(None) => {
                // Sem mudança: o conteúdo é o mesmo de antes. Ainda assim vale
                // ler o retido, para distinguir "parado em preto" de "parado
                // na última imagem".
                let l = match cap.copy_bgra(&mut bgra) {
                    Ok(true) => luminancia(&bgra),
                    _ => -1.0,
                };
                ("sem-mudanca".to_string(), l)
            }
            Err(e) => {
                let motivo = match &e {
                    CaptureError::AccessLost => "acesso-perdido".to_string(),
                    outro => format!("{outro}"),
                };
                let r = cap.recover();
                let recuperou = match r {
                    Ok(()) => "recuperou".to_string(),
                    Err(e2) => format!("recusado: {e2}"),
                };
                (format!("{motivo} / {recuperou}"), -1.0)
            }
        };

        let luz_txt = if luz < 0.0 {
            "-".to_string()
        } else {
            format!("{luz:.1}")
        };
        let observacao = if luz >= 0.0 && luz < 3.0 {
            "PRETO"
        } else {
            ""
        };
        let linha = format!("{}\t{nome}\t{estado}\t{luz_txt}\t{observacao}", hora_local());

        // Só imprime quando algo muda, para o relatório caber na tela — mas
        // grava tudo no arquivo.
        let chave = format!("{nome}|{estado}|{observacao}");
        if chave != anterior {
            println!("{linha}");
            anterior = chave;
        }
        if let Some(f) = arquivo.as_mut() {
            let _ = writeln!(f, "{linha}");
        }

        std::thread::sleep(INTERVALO);
    }

    println!("\nfim. Registro completo em {}", caminho.display());
}

/// Luminância média, amostrando de 64 em 64 pixels.
///
/// Não precisa ser exata: a pergunta é "isto é preto?", não "qual o brilho".
fn luminancia(bgra: &[u8]) -> f64 {
    let mut soma = 0f64;
    let mut n = 0u32;
    for p in bgra.chunks_exact(4).step_by(64) {
        // Aproximação de luma; os pesos exatos não mudam a resposta.
        soma += 0.114 * p[0] as f64 + 0.587 * p[1] as f64 + 0.299 * p[2] as f64;
        n += 1;
    }
    if n == 0 { -1.0 } else { soma / n as f64 }
}
