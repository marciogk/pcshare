//! Sprint 0, etapa 4: a pipeline inteira, ponta a ponta.
//!
//! captura DXGI â†’ BGRA â†’ NV12 â†’ H.264 â†’ TCP â†’ H.264 â†’ NV12 â†’ BGRA â†’ janela
//!
//!     cargo run --release -p pcshare-spike               # tudo em 127.0.0.1
//!     cargo run --release -p pcshare-spike -- host 0.0.0.0:7000
//!     cargo run --release -p pcshare-spike -- viewer 192.168.0.10:7000
//!
//! O modo padrÃ£o roda host e viewer no mesmo processo, o que permite medir a
//! latÃªncia interna com um relÃ³gio sÃ³. Esse nÃºmero **nÃ£o** Ã© glass-to-glass:
//! ele ignora o tempo que o compositor leva para pÃ´r o pixel no monitor e o
//! que o monitor leva para acendÃª-lo. O protocolo com cronÃ´metro e cÃ¢mera, em
//! `docs/SPRINT-0.md`, continua sendo a medida que vale.

mod input;
mod window;

use std::collections::VecDeque;
use std::net::TcpListener;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pcshare_capture::{CaptureError, DesktopCapture};
use pcshare_gpu::GpuContext;
use pcshare_codec::convert::{bgra_to_nv12, nv12_to_bgra};
use pcshare_codec::{H264Decoder, H264Encoder};
use pcshare_core::secure;
use pcshare_core::quic::QuicListener;
use pcshare_core::{
    Channel, Identity, InputEvent, Message, QuicTransport, TcpTransport, Transport,
};
use windows::core::w;

/// Taxa alvo. SobreponÃ­vel por `PCSHARE_FPS` para experimentaÃ§Ã£o: o ritmo Ã©
/// suspeito de dominar a latÃªncia, jÃ¡ que uma mudanÃ§a espera em mÃ©dia meio
/// intervalo sÃ³ pela vez dela.
fn fps() -> u32 {
    std::env::var("PCSHARE_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (1..=120).contains(v))
        .unwrap_or(30)
}
const BITRATE: u32 = 8_000_000;
/// Espera mÃ¡xima por um frame novo. Ã‰ espera bloqueante em evento (decisÃ£o
/// D8), mas limitada para que o mesmo laÃ§o tambÃ©m atenda o transporte.
const CAPTURE_TIMEOUT_MS: u32 = 8;
/// Acima deste acÃºmulo na fila de saÃ­da, o host para de codificar atÃ© o link
/// escoar. Meio megabyte a 3 Mbps jÃ¡ representa mais de um segundo de atraso.
const BACKLOG_LIMIT: usize = 512 * 1024;

/// Base de tempo comum. SÃ³ faz sentido no modo de processo Ãºnico.
static START: OnceLock<Instant> = OnceLock::new();

/// Somente visualizaÃ§Ã£o: o host recebe e contabiliza os eventos, mas nÃ£o os
/// injeta.
///
/// NÃ£o Ã© sÃ³ chave de teste. Ã‰ requisito de produto por dois motivos: em modo
/// loopback a injeÃ§Ã£o realimenta a prÃ³pria janela e deixa o cursor errÃ¡tico,
/// e uma sessÃ£o de suporte que sÃ³ observa Ã© o padrÃ£o mÃ­nimo de privacidade
/// para quem estÃ¡ do outro lado.
static VIEW_ONLY: AtomicBool = AtomicBool::new(false);

/// Host e viewer no mesmo processo, portanto com a mesma base de tempo.
///
/// SÃ³ nesse caso a latÃªncia capturaâ†’exibiÃ§Ã£o significa alguma coisa: o
/// carimbo do host e o relÃ³gio do viewer saem do mesmo `Instant`. Em
/// processos separados â€” e mais ainda em mÃ¡quinas separadas â€” subtrair um do
/// outro produz lixo, e jÃ¡ produziu: âˆ’2073 ms numa mediÃ§Ã£o real, que era
/// exatamente a diferenÃ§a entre os instantes de inÃ­cio dos dois processos.
static SAME_PROCESS: AtomicBool = AtomicBool::new(false);

/// Cala os relatÃ³rios periÃ³dicos do host. Na mediÃ§Ã£o de latÃªncia eles
/// atropelariam a linha de progresso.
static QUIET: AtomicBool = AtomicBool::new(false);

fn now_us() -> i64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as i64
}

fn main() {
    START.get_or_init(Instant::now);
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--view-only") {
        VIEW_ONLY.store(true, Ordering::SeqCst);
        println!("modo somente visualizaÃ§Ã£o: entrada nÃ£o serÃ¡ injetada");
    }

    // `--sem-cripto` mantÃ©m o caminho em texto claro para comparaÃ§Ã£o de
    // custo. Nunca deve existir num binÃ¡rio distribuÃ­do.
    let plaintext = args.iter().any(|a| a == "--sem-cripto");
    // `--tcp` volta ao transporte antigo, para comparaÃ§Ã£o.
    let quic = !args.iter().any(|a| a == "--tcp");

    match args.first().map(String::as_str) {
        Some("host") => {
            let bind = args.get(1).cloned().unwrap_or_else(|| "0.0.0.0:7000".into());
            let identity = load_identity().expect("identidade");
            let password = arg_value("--password").unwrap_or_else(secure::random_password);

            println!("\n  ID desta mÃ¡quina : {}", identity.display_id());
            println!("  Senha da sessÃ£o  : {password}");
            println!("  ImpressÃ£o digital: {}\n", identity.fingerprint());

            println!("host aguardando viewer em {bind} ({})", transporte(quic));
            let result = if quic {
                let listener =
                    QuicListener::bind(bind.parse().expect("endereÃ§o invÃ¡lido")).expect("bind");
                let t = listener.accept().expect("accept falhou");
                servir(t, &identity, &password, plaintext)
            } else {
                let listener = TcpListener::bind(&bind).expect("bind falhou");
                let t = TcpTransport::accept(&listener).expect("accept falhou");
                servir(t, &identity, &password, plaintext)
            };
            if let Err(e) = result {
                eprintln!("host encerrou: {e}");
            }
        }
        Some("viewer") => {
            let addr = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:7000".into());
            let password = if plaintext {
                String::new()
            } else {
                match arg_value("--password") {
                    Some(p) => p,
                    None => {
                        eprintln!("viewer: informe --password <senha exibida pelo host>");
                        return;
                    }
                }
            };
            let identity = load_identity().expect("identidade");
            println!("viewer conectando a {addr} ({})", transporte(quic));

            if quic {
                let t = QuicTransport::connect(addr.parse().expect("endereÃ§o invÃ¡lido"))
                    .expect("connect falhou");
                assistir(t, &identity, &password, plaintext);
            } else {
                let t = TcpTransport::connect(&addr).expect("connect falhou");
                assistir(t, &identity, &password, plaintext);
            }
        }
        Some("latencia") => {
            let amostras: usize = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            run_latency(amostras);
        }
        _ => run_loopback(),
    }
}

// ------------------------------------------------- mediÃ§Ã£o de latÃªncia

/// Onde o marcador fica na tela, e de que tamanho.
const MARK_X: i32 = 24;
const MARK_Y: i32 = 24;
const MARK_SIZE: i32 = 96;
/// Luma acima disto conta como branco. Preto fica perto de 16, branco de 235.
const LUMA_LIMIAR: u8 = 128;

/// Mede a latÃªncia que os cronÃ´metros internos nÃ£o enxergam.
///
/// Os 16,8 ms medidos atÃ© aqui iam da captura atÃ© a entrega dos pixels ao
/// Windows. Faltava o compositor â€” e ele nÃ£o Ã© pequeno.
///
/// O truque Ã© fechar o laÃ§o pela prÃ³pria tela: um marcador muda de cor, e
/// esperamos essa mudanÃ§a reaparecer num quadro **decodificado**. O caminho
/// percorrido Ã© descarga na tela â†’ compositor â†’ captura DXGI â†’ encode â†’ rede â†’
/// decode.
///
/// **O que ainda falta para ser glass-to-glass de verdade:** a apresentaÃ§Ã£o
/// final no viewer e o tempo de resposta fÃ­sico dos monitores, que nenhum
/// software alcanÃ§a. Some algo entre 10 e 25 ms ao resultado para estimar o
/// nÃºmero que a pessoa sente â€” ou meÃ§a com cÃ¢mera, como descrito em
/// `docs/PROXIMOS-PASSOS.md`.
fn run_latency(amostras: usize) {
    QUIET.store(true, Ordering::SeqCst);
    println!("PCShare â€” mediÃ§Ã£o de latÃªncia pelo compositor");
    println!("NÃ£o mexa no mouse nem cubra o quadrado no canto superior esquerdo.\n");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind falhou");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let t = TcpTransport::accept(&listener).expect("accept falhou");
        let _ = run_host(t);
    });

    let mut t = TcpTransport::connect(addr).expect("connect falhou");
    let Some((w, h)) = wait_hello(&mut t) else {
        eprintln!("conexÃ£o encerrada antes do Hello");
        return;
    };
    let mut dec = match H264Decoder::new(w, h, fps()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("sem decodificador: {e}");
            return;
        }
    };
    let req = Message::RequestKeyframe;
    let _ = t.send(req.channel(), &req.encode());

    let mut marcador = match window::MarkerWindow::new(MARK_X, MARK_Y, MARK_SIZE) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("nÃ£o foi possÃ­vel criar o marcador: {e}");
            return;
        }
    };

    // Amostra o centro do marcador, longe das bordas â€” a compressÃ£o suaviza
    // transiÃ§Ãµes bruscas, e o miolo do quadrado Ã© o ponto mais confiÃ¡vel.
    let sx = (MARK_X + MARK_SIZE / 2) as usize;
    let sy = (MARK_Y + MARK_SIZE / 2) as usize;
    if sx >= w as usize || sy >= h as usize {
        eprintln!("marcador fora da tela capturada");
        return;
    }

    let mut medidas: Vec<f64> = Vec::with_capacity(amostras);
    let mut esperando_branco = true;
    let mut flip_em: Option<Instant> = None;
    marcador.paint(false);

    let limite = Instant::now() + Duration::from_secs(60);
    let mut estabilizou_ate = Instant::now() + Duration::from_millis(400);

    while medidas.len() < amostras && Instant::now() < limite {
        marcador.pump();

        // Consome tudo que chegou e fica com o quadro mais recente.
        loop {
            match t.poll() {
                Ok(Some((_, payload))) => {
                    if let Ok(Message::Video { data, .. }) = Message::decode(&payload) {
                        let _ = dec.decode(&data, 0);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("conexÃ£o encerrada ({e})");
                    return;
                }
            }
        }

        for f in dec.take_frames() {
            let idx = sy * f.width as usize + sx;
            let Some(&luma) = f.data.get(idx) else { continue };
            let esta_branco = luma > LUMA_LIMIAR;

            // Espera a tela assentar antes de comeÃ§ar a cronometrar.
            if Instant::now() < estabilizou_ate {
                continue;
            }

            match flip_em {
                // Ocioso: o alvo Ã© sempre o oposto do que estÃ¡ na tela agora.
                // Derivar do estado observado, e nÃ£o de um alvo guardado, Ã© o
                // que garante progresso â€” a primeira versÃ£o comparava os dois
                // e travava assim que coincidiam.
                None => {
                    esperando_branco = !esta_branco;
                    marcador.paint(esperando_branco);
                    flip_em = Some(Instant::now());
                }
                Some(t0) => {
                    if esta_branco == esperando_branco {
                        medidas.push(t0.elapsed().as_secs_f64() * 1000.0);
                        flip_em = None;
                        print!("\r  {} de {amostras} amostras ", medidas.len());
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                }
            }
        }

        std::thread::yield_now();
    }
    println!();

    if medidas.len() < 3 {
        eprintln!("\nAmostras insuficientes ({}). O marcador pode ter sido", medidas.len());
        eprintln!("encoberto, ou a tela estava mudando demais.");
        return;
    }

    medidas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = pct(&medidas, 0.50);
    println!("\n{:=<58}", "");
    println!("LATÃŠNCIA ATÃ‰ O QUADRO DECODIFICADO  ({} amostras)", medidas.len());
    println!("{:=<58}", "");
    println!("  p50 {:.1} ms   p99 {:.1} ms   mÃ­n {:.1} ms   mÃ¡x {:.1} ms",
        p50,
        pct(&medidas, 0.99),
        medidas[0],
        medidas[medidas.len() - 1],
    );
    println!("\n  Inclui: compositor do Windows, captura, encode, rede, decode.");
    println!("  Falta : apresentaÃ§Ã£o no viewer e resposta fÃ­sica do monitor.");
    println!("\n  Estimativa glass-to-glass: {:.0} a {:.0} ms", p50 + 10.0, p50 + 25.0);
    println!("  Meta do Sprint 0: abaixo de 80 ms.");
}

fn run_loopback() {
    SAME_PROCESS.store(true, Ordering::SeqCst);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind falhou");
    let addr = listener.local_addr().unwrap();
    println!("PCShare â€” Sprint 0, etapa 4: pipeline completa em {addr}");
    println!("Feche a janela para encerrar.\n");

    std::thread::spawn(move || {
        let t = TcpTransport::accept(&listener).expect("accept falhou");
        if let Err(e) = run_host(t) {
            eprintln!("host encerrou: {e}");
        }
    });

    let t = TcpTransport::connect(addr).expect("connect falhou");
    run_viewer(t);
}

// ---------------------------------------------------------------- host

/// Carrega a identidade da mÃ¡quina, criando na primeira execuÃ§Ã£o.
///
/// Precisa persistir: o ID que a pessoa dita por telefone Ã© derivado da chave
/// pÃºblica, entÃ£o gerar uma nova a cada inÃ­cio trocaria o ID da mÃ¡quina e
/// invalidaria qualquer pareamento anterior.
fn load_identity() -> Result<Identity, Box<dyn std::error::Error>> {
    let dir = std::path::PathBuf::from(
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into()),
    )
    .join("PCShare");
    let path = dir.join("identity.key");

    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(id) = Identity::from_private(bytes)
    {
        return Ok(id);
    }

    let id = Identity::generate()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, id.private_key())?;
    println!("identidade criada em {}", path.display());
    // TODO(Fase 3): a chave fica em texto claro no perfil do usuÃ¡rio. O
    // produto precisa protegÃª-la com DPAPI e permissÃµes restritas.
    Ok(id)
}

fn transporte(quic: bool) -> &'static str {
    if quic { "QUIC" } else { "TCP" }
}

/// Handshake do lado do host e entrada no laÃ§o de captura.
fn servir<T: Transport>(
    t: T,
    identity: &Identity,
    password: &str,
    plaintext: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if plaintext {
        eprintln!("AVISO: sessÃ£o SEM criptografia");
        return run_host(t);
    }
    let sec = secure::accept(t, identity, password)?;
    println!("host: sessÃ£o cifrada com {}", sec.remote_fingerprint());
    run_host(sec)
}

/// Handshake do lado do viewer e entrada no laÃ§o de exibiÃ§Ã£o.
fn assistir<T: Transport>(t: T, identity: &Identity, password: &str, plaintext: bool) {
    if plaintext {
        eprintln!("AVISO: sessÃ£o SEM criptografia");
        run_viewer(t);
        return;
    }
    match secure::connect(t, identity, password) {
        Ok(sec) => {
            // Conferir esta impressÃ£o com quem estÃ¡ do outro lado Ã© o que
            // fecha o buraco de intermediaÃ§Ã£o â€” o servidor de sinalizaÃ§Ã£o da
            // Fase 2 nÃ£o Ã© confiÃ¡vel por natureza.
            println!(
                "viewer: host autenticado, impressÃ£o {}",
                sec.remote_fingerprint()
            );
            run_viewer(sec);
        }
        Err(e) => eprintln!("viewer: nÃ£o foi possÃ­vel autenticar ({e})"),
    }
}

fn arg_value(nome: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == nome).and_then(|i| a.get(i + 1).cloned())
}

fn run_host<T: Transport>(mut t: T) -> Result<(), Box<dyn std::error::Error>> {
    // `--cpu-path` forÃ§a o caminho antigo (leitura da GPU + conversÃ£o na CPU),
    // para comparaÃ§Ã£o A/B com o caminho sem cÃ³pia.
    let force_cpu = std::env::args().any(|a| a == "--cpu-path");

    let gpu = GpuContext::new()?;
    let mut cap = DesktopCapture::with_gpu(gpu.clone(), 0)?;
    let (w, h) = (cap.width(), cap.height());
    println!("host: capturando {w}x{h}");

    let mut enc = if force_cpu {
        H264Encoder::new(w, h, fps(), BITRATE)?
    } else {
        H264Encoder::with_gpu(&gpu, w, h, fps(), BITRATE)?
    };
    println!("host: encoder [{}] ({})", enc.name(), enc.tuning().join(", "));

    // O conversor na GPU sÃ³ entra se o encoder aceitar texturas. Encoder de
    // software e driver antigo caem no caminho por memÃ³ria de sistema, e Ã©
    // melhor perder desempenho do que nÃ£o funcionar.
    let mut conv = match enc.is_gpu_direct() {
        true => Some(pcshare_gpu::Nv12Converter::new(gpu.clone(), w, h)?),
        false => None,
    };
    println!(
        "host: caminho de pixels = {}",
        if conv.is_some() {
            "GPU direto (sem cÃ³pia)"
        } else {
            "memÃ³ria de sistema (leitura da GPU + conversÃ£o na CPU)"
        }
    );

    let hello = Message::Hello {
        width: w,
        height: h,
        fps: fps(),
    };
    t.send(hello.channel(), &hello.encode())?;

    let mut bgra = Vec::new();
    let mut nv12 = Vec::new();
    let mut seq = 0u64;
    let mut stats = HostStats::default();
    let mut next_report = Instant::now() + Duration::from_secs(2);

    let frame_period = Duration::from_micros(1_000_000 / fps() as u64);
    let mut last_encoded = Instant::now() - frame_period;

    loop {
        // ---- mensagens vindas do viewer
        while let Some((channel, payload)) = t.poll()? {
            if channel != Channel::Control {
                continue;
            }
            match Message::decode(&payload) {
                Ok(Message::Ping { nonce, sent_us }) => {
                    let m = Message::Pong { nonce, sent_us };
                    t.send(m.channel(), &m.encode())?;
                }
                Ok(Message::Input(ev)) => {
                    if !VIEW_ONLY.load(Ordering::Relaxed) {
                        input::inject(&ev, w, h);
                    }
                    stats.input_events += 1;
                }
                Ok(Message::RequestKeyframe) => {
                    enc.request_keyframe();
                    stats.keyframe_requests += 1;
                }
                Ok(_) | Err(_) => {}
            }
        }

        // ---- captura
        let frame = match cap.next_frame(CAPTURE_TIMEOUT_MS) {
            Ok(Some(f)) => f,
            Ok(None) => {
                t.flush()?;
                continue;
            }
            Err(CaptureError::AccessLost) => {
                stats.access_lost += 1;
                cap.recover()?;
                continue;
            }
            Err(e) => return Err(e.into()),
        };

        // Nada mudou de fato: nem pixels novos, nem regiÃµes deslocadas.
        //
        // Esta Ã© a supressÃ£o que o achado da etapa 1 exigiu. O compositor
        // entrega frames a 60 Hz mesmo com o desktop ocioso, e medimos na
        // etapa 2 que numa tela imÃ³vel o encoder gasta banda refinando
        // qualidade sozinho. A resposta nÃ£o Ã© ajustar o encoder â€” Ã© nÃ£o
        // chamÃ¡-lo.
        if !frame.has_changes() {
            stats.skipped += 1;
            t.flush()?;
            continue;
        }

        // A captura entrega a 60 Hz; o alvo Ã© 30. Sem esta trava o host
        // produz o dobro do que o viewer consome, e a diferenÃ§a nÃ£o vira
        // fluidez â€” vira fila, e fila vira latÃªncia.
        if last_encoded.elapsed() < frame_period {
            stats.paced += 1;
            t.flush()?;
            continue;
        }

        // ContrapressÃ£o. Se o link nÃ£o escoou o que jÃ¡ mandamos, mandar mais
        // sÃ³ afunda a latÃªncia. Numa tela remota, **descartar Ã© melhor que
        // atrasar**: ninguÃ©m quer ver o que a tela era hÃ¡ dois segundos.
        if t.pending_out() > BACKLOG_LIMIT {
            stats.dropped += 1;
            t.flush()?;
            continue;
        }

        last_encoded = Instant::now();
        let captured_us = now_us();

        if let Some(conv) = conv.as_mut() {
            // Caminho sem cÃ³pia: a textura nunca sai da memÃ³ria de vÃ­deo.
            let Some(src) = cap.current_texture() else {
                continue;
            };
            let t1 = Instant::now();
            let nv12_tex = conv.convert(src)?;
            stats.convert_ms.push(ms(t1));

            let t2 = Instant::now();
            enc.submit_texture(nv12_tex)?;
            stats.encode_ms.push(ms(t2));
        } else {
            let t0 = Instant::now();
            if !cap.copy_bgra(&mut bgra)? {
                continue;
            }
            stats.readback_ms.push(ms(t0));

            let t1 = Instant::now();
            bgra_to_nv12(&bgra, w as usize, h as usize, &mut nv12);
            stats.convert_ms.push(ms(t1));

            let t2 = Instant::now();
            enc.submit(&nv12)?;
            stats.encode_ms.push(ms(t2));
        }

        for p in enc.take_packets() {
            let m = Message::Video {
                seq,
                // Carimbo do instante da *captura*, nÃ£o do envio: Ã© ele que
                // permite ao viewer medir a travessia inteira.
                time_us: captured_us,
                is_keyframe: p.is_keyframe,
                data: p.data,
            };
            t.send(m.channel(), &m.encode())?;
            seq += 1;
            stats.frames += 1;
        }
        t.flush()?;

        if Instant::now() >= next_report {
            if !QUIET.load(Ordering::Relaxed) {
                stats.report(&t);
            }
            next_report += Duration::from_secs(2);
        }
    }
}

#[derive(Default)]
struct HostStats {
    frames: u64,
    skipped: u64,
    /// Descartados por pacing: a captura vinha mais rÃ¡pido que o alvo.
    paced: u64,
    /// Descartados por contrapressÃ£o: o link nÃ£o estava escoando.
    dropped: u64,
    keyframe_requests: u64,
    access_lost: u64,
    input_events: u64,
    readback_ms: Vec<f64>,
    convert_ms: Vec<f64>,
    encode_ms: Vec<f64>,
}

impl HostStats {
    fn report<T: Transport>(&mut self, t: &T) {
        println!(
            "host: {} frames | leitura GPU {:.2} ms | BGRAâ†’NV12 {:.2} ms | encode {:.2} ms \
             | {:.2} Mbps | fila {} KB | ritmo {} desc {} sem-mudanca {}{}{}",
            self.frames,
            med(&mut self.readback_ms),
            med(&mut self.convert_ms),
            med(&mut self.encode_ms),
            t.bytes_sent() as f64 * 8.0 / now_us().max(1) as f64,
            t.pending_out() / 1024,
            self.paced,
            self.dropped,
            self.skipped,
            if self.input_events > 0 {
                format!(
                    " | entrada {}{}",
                    self.input_events,
                    if VIEW_ONLY.load(Ordering::Relaxed) {
                        " (nÃ£o injetada)"
                    } else {
                        ""
                    }
                )
            } else {
                String::new()
            },
            if self.access_lost > 0 {
                format!(" | sessÃµes perdidas {}", self.access_lost)
            } else {
                String::new()
            },
        );
        self.readback_ms.clear();
        self.convert_ms.clear();
        self.encode_ms.clear();
    }
}

// -------------------------------------------------------------- viewer

fn run_viewer<T: Transport>(mut t: T) {
    // Nada pode ser desenhado antes de saber as dimensÃµes do host.
    let Some((w, h)) = wait_hello(&mut t) else {
        eprintln!("viewer: conexÃ£o encerrada antes do Hello");
        return;
    };
    println!("viewer: host anunciou {w}x{h}");

    let mut dec = match H264Decoder::new(w, h, fps()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("viewer: sem decodificador: {e}");
            return;
        }
    };
    println!("viewer: decoder [{}]", dec.name());

    // Sem keyframe periÃ³dico, este pedido Ã© a Ãºnica coisa que dÃ¡ ao
    // decodificador um ponto de partida. Sem ele, tela preta para sempre.
    let req = Message::RequestKeyframe;
    if t.send(req.channel(), &req.encode()).is_err() {
        eprintln!("viewer: nÃ£o foi possÃ­vel pedir o keyframe inicial");
        return;
    }

    // `--headless <segundos>`: sem janela.
    //
    // Existe para medir a banda de uma tela realmente parada. Com a janela
    // aberta Ã© impossÃ­vel: ela exibe a tela que estÃ¡ sendo capturada, entÃ£o
    // cada quadro exibido altera a tela e gera o prÃ³ximo. A mediÃ§Ã£o mediria a
    // prÃ³pria realimentaÃ§Ã£o.
    let headless: Option<u64> = {
        let a: Vec<String> = std::env::args().collect();
        a.iter().position(|x| x == "--headless").map(|i| {
            a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20)
        })
    };

    let mut win = match headless {
        Some(_) => None,
        None => match window::Window::new(w!("PCShare â€” viewer (Sprint 0)"), 960, 540) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("viewer: nÃ£o foi possÃ­vel criar a janela: {e}");
                return;
            }
        },
    };
    let deadline = headless.map(|s| Instant::now() + Duration::from_secs(s));

    // `--dump-frame <arquivo.bmp>`: grava o primeiro quadro decodificado e sai.
    let dump_to: Option<String> = {
        let a: Vec<String> = std::env::args().collect();
        a.iter()
            .position(|x| x == "--dump-frame")
            .and_then(|i| a.get(i + 1).cloned())
    };

    let mut rgb = Vec::new();
    let mut frames = 0u64;
    let mut stale = 0u64;
    let mut sent_input = 0u64;
    let mut latency_ms: Vec<f64> = Vec::new();
    let mut rtt_ms: Vec<f64> = Vec::new();
    let mut pending_time: VecDeque<i64> = VecDeque::new();
    let mut next_report = Instant::now() + Duration::from_secs(2);
    let mut next_ping = Instant::now();
    let mut nonce = 0u64;
    let mut first = true;

    loop {
        if let Some(w) = win.as_mut()
            && !w.pump()
        {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        let mut got_any = false;

        // ---- sondagem de latÃªncia, vÃ¡lida entre mÃ¡quinas quaisquer
        if Instant::now() >= next_ping {
            nonce += 1;
            let m = Message::Ping {
                nonce,
                sent_us: now_us(),
            };
            if t.send(m.channel(), &m.encode()).is_err() {
                return;
            }
            next_ping += Duration::from_millis(250);
        }

        // ---- entrada do usuÃ¡rio, de volta para o host
        let (cw, ch) = win.as_ref().map_or((0, 0), |w| w.client_size());
        for raw in win.as_mut().map(|w| w.take_input()).unwrap_or_default() {
            let ev = match raw {
                // A janela quase nunca tem o tamanho da tela remota. Mapear
                // pela proporÃ§Ã£o Ã© o que faz o cursor pousar onde o usuÃ¡rio
                // apontou, e nÃ£o deslocado.
                window::RawEvent::Move { x, y } => {
                    if cw <= 0 || ch <= 0 {
                        continue;
                    }
                    InputEvent::MouseMove {
                        x: (x as i64 * w as i64 / cw as i64) as i32,
                        y: (y as i64 * h as i64 / ch as i64) as i32,
                    }
                }
                window::RawEvent::Button { button, down } => {
                    InputEvent::MouseButton { button, down }
                }
                window::RawEvent::Wheel { delta } => InputEvent::MouseWheel { delta },
                window::RawEvent::Key { scancode, down } => InputEvent::Key { scancode, down },
            };
            let m = Message::Input(ev);
            if t.send(m.channel(), &m.encode()).is_err() {
                return;
            }
            sent_input += 1;
        }

        loop {
            match t.poll() {
                Ok(Some((_, payload))) => match Message::decode(&payload) {
                    Ok(Message::Video { time_us, data, .. }) => {
                        pending_time.push_back(time_us);
                        if let Err(e) = dec.decode(&data, 0) {
                            eprintln!("viewer: decode falhou: {e}");
                        }
                        got_any = true;
                    }
                    // `sent_us` volta intacto: o RTT sai de uma subtraÃ§Ã£o
                    // local, entÃ£o vale mesmo entre mÃ¡quinas com relÃ³gios
                    // sem qualquer sincronia.
                    Ok(Message::Pong { sent_us, .. }) => {
                        rtt_ms.push((now_us() - sent_us) as f64 / 1000.0);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("viewer: mensagem invÃ¡lida: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    println!("viewer: conexÃ£o encerrada ({e})");
                    return;
                }
            }
        }

        // Todo frame precisa ser *decodificado* â€” H.264 Ã© interdependente e
        // pular um estraga os seguintes. Mas sÃ³ o mais recente precisa ser
        // *exibido*: converter e desenhar quadros jÃ¡ obsoletos Ã© trabalho que
        // atrasa o prÃ³ximo sem que ninguÃ©m veja o resultado.
        let batch = dec.take_frames();
        let mut sent_us = None;
        for _ in 0..batch.len() {
            sent_us = pending_time.pop_front();
        }
        stale += batch.len().saturating_sub(1) as u64;

        if let Some(f) = batch.last() {
            if let Some(sent) = sent_us
                && SAME_PROCESS.load(Ordering::Relaxed)
            {
                latency_ms.push((now_us() - sent) as f64 / 1000.0);
            }
            // Em modo headless a conversÃ£o para BGRA ainda vale a pena: ela
            // faz parte do custo do viewer, e omiti-la falsearia a mediÃ§Ã£o.
            nv12_to_bgra(
                &f.data,
                f.width as usize,
                f.height as usize,
                f.width as usize,
                &mut rgb,
            );
            if let Some(v) = win.as_mut() {
                v.present(&rgb, f.width as i32, f.height as i32);
            }
            frames += 1;

            if first {
                println!("viewer: primeira imagem na tela");
                first = false;

                // VerificaÃ§Ã£o visual do ida-e-volta completo. Contadores de
                // fps() provam que a pipeline *corre*; sÃ³ olhar um quadro prova
                // que ela corre com a imagem certa.
                if let Some(path) = dump_to.as_deref() {
                    match save_bmp(path, &rgb, f.width as i32, f.height as i32) {
                        Ok(()) => println!("viewer: quadro gravado em {path}"),
                        Err(e) => eprintln!("viewer: falha ao gravar {path}: {e}"),
                    }
                    return;
                }
            }
        }

        if Instant::now() >= next_report {
            latency_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            rtt_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

            // A latÃªncia capturaâ†’exibiÃ§Ã£o some quando os relÃ³gios nÃ£o sÃ£o o
            // mesmo. Melhor nÃ£o reportar do que reportar um nÃºmero que
            // parece medida e Ã© diferenÃ§a de instantes de partida.
            let travessia = if SAME_PROCESS.load(Ordering::Relaxed) {
                format!(
                    " | capturaâ†’exibiÃ§Ã£o p50 {:.1} ms  p99 {:.1} ms",
                    pct(&latency_ms, 0.50),
                    pct(&latency_ms, 0.99),
                )
            } else {
                String::new()
            };

            println!(
                "viewer: {frames} exibidos, {stale} obsoletos, {sent_input} eventos \
                 | RTT p50 {:.1} ms  p99 {:.1} ms{travessia}",
                pct(&rtt_ms, 0.50),
                pct(&rtt_ms, 0.99),
            );
            latency_ms.clear();
            rtt_ms.clear();
            next_report += Duration::from_secs(2);
        }

        if !got_any {
            std::thread::yield_now();
        }
    }

    if headless.is_some() {
        let secs = now_us() as f64 / 1e6;
        println!(
            "\nviewer headless: {frames} quadros em {secs:.1} s  |  {:.0} kbps recebidos",
            t.bytes_received() as f64 * 8.0 / secs / 1000.0
        );
    }
}

fn wait_hello<T: Transport>(t: &mut T) -> Option<(u32, u32)> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        match t.poll() {
            Ok(Some((_, payload))) => {
                if let Ok(Message::Hello { width, height, .. }) = Message::decode(&payload) {
                    return Some((width, height));
                }
            }
            Ok(None) => std::thread::yield_now(),
            Err(_) => return None,
        }
    }
    None
}

/// Grava BGRA como BMP de 32 bits. Formato escolhido por ser o Ãºnico que se
/// escreve sem dependÃªncia: cabeÃ§alho fixo e pixels crus, sem compressÃ£o.
fn save_bmp(path: &str, bgra: &[u8], w: i32, h: i32) -> std::io::Result<()> {
    let pixels = (w as usize) * (h as usize) * 4;
    let mut f = Vec::with_capacity(54 + pixels);

    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + pixels) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());

    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    // Altura negativa: linhas de cima para baixo, como o buffer estÃ¡. Sem
    // isso a imagem sai invertida.
    f.extend_from_slice(&(-h).to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&32u16.to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&(pixels as u32).to_le_bytes());
    f.extend_from_slice(&[0u8; 16]);

    f.extend_from_slice(&bgra[..pixels.min(bgra.len())]);
    std::fs::write(path, f)
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

fn med(v: &mut Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[v.len() / 2]
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}


