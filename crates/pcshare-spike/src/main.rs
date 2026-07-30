//! Sprint 0, etapa 4: a pipeline inteira, ponta a ponta.
//!
//! captura DXGI → BGRA → NV12 → H.264 → TCP → H.264 → NV12 → BGRA → janela
//!
//!     cargo run --release -p pcshare-spike               # tudo em 127.0.0.1
//!     cargo run --release -p pcshare-spike -- host 0.0.0.0:7000
//!     cargo run --release -p pcshare-spike -- viewer 192.168.0.10:7000
//!
//! O modo padrão roda host e viewer no mesmo processo, o que permite medir a
//! latência interna com um relógio só. Esse número **não** é glass-to-glass:
//! ele ignora o tempo que o compositor leva para pôr o pixel no monitor e o
//! que o monitor leva para acendê-lo. O protocolo com cronômetro e câmera, em
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
use pcshare_core::{Channel, Identity, InputEvent, Message, TcpTransport, Transport};
use windows::core::w;

const FPS: u32 = 30;
const BITRATE: u32 = 8_000_000;
/// Espera máxima por um frame novo. É espera bloqueante em evento (decisão
/// D8), mas limitada para que o mesmo laço também atenda o transporte.
const CAPTURE_TIMEOUT_MS: u32 = 8;
/// Acima deste acúmulo na fila de saída, o host para de codificar até o link
/// escoar. Meio megabyte a 3 Mbps já representa mais de um segundo de atraso.
const BACKLOG_LIMIT: usize = 512 * 1024;

/// Base de tempo comum. Só faz sentido no modo de processo único.
static START: OnceLock<Instant> = OnceLock::new();

/// Somente visualização: o host recebe e contabiliza os eventos, mas não os
/// injeta.
///
/// Não é só chave de teste. É requisito de produto por dois motivos: em modo
/// loopback a injeção realimenta a própria janela e deixa o cursor errático,
/// e uma sessão de suporte que só observa é o padrão mínimo de privacidade
/// para quem está do outro lado.
static VIEW_ONLY: AtomicBool = AtomicBool::new(false);

/// Host e viewer no mesmo processo, portanto com a mesma base de tempo.
///
/// Só nesse caso a latência captura→exibição significa alguma coisa: o
/// carimbo do host e o relógio do viewer saem do mesmo `Instant`. Em
/// processos separados — e mais ainda em máquinas separadas — subtrair um do
/// outro produz lixo, e já produziu: −2073 ms numa medição real, que era
/// exatamente a diferença entre os instantes de início dos dois processos.
static SAME_PROCESS: AtomicBool = AtomicBool::new(false);

fn now_us() -> i64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as i64
}

fn main() {
    START.get_or_init(Instant::now);
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--view-only") {
        VIEW_ONLY.store(true, Ordering::SeqCst);
        println!("modo somente visualização: entrada não será injetada");
    }

    // `--sem-cripto` mantém o caminho em texto claro para comparação de
    // custo. Nunca deve existir num binário distribuído.
    let plaintext = args.iter().any(|a| a == "--sem-cripto");

    match args.first().map(String::as_str) {
        Some("host") => {
            let bind = args.get(1).cloned().unwrap_or_else(|| "0.0.0.0:7000".into());
            let identity = load_identity().expect("identidade");
            let password = arg_value("--password").unwrap_or_else(secure::random_password);

            println!("\n  ID desta máquina : {}", identity.display_id());
            println!("  Senha da sessão  : {password}");
            println!("  Impressão digital: {}\n", identity.fingerprint());

            let listener = TcpListener::bind(&bind).expect("bind falhou");
            println!("host aguardando viewer em {bind}");
            let tcp = TcpTransport::accept(&listener).expect("accept falhou");

            let result = if plaintext {
                eprintln!("AVISO: sessão SEM criptografia");
                run_host(tcp)
            } else {
                match secure::accept(tcp, &identity, &password) {
                    Ok(sec) => {
                        println!("host: sessão cifrada com {}", sec.remote_fingerprint());
                        run_host(sec)
                    }
                    Err(e) => {
                        eprintln!("host: autenticação recusada ({e})");
                        return;
                    }
                }
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
            let tcp = TcpTransport::connect(&addr).expect("connect falhou");
            println!("viewer conectado a {addr}");

            if plaintext {
                eprintln!("AVISO: sessão SEM criptografia");
                run_viewer(tcp);
                return;
            }

            let Some(password) = arg_value("--password") else {
                eprintln!("viewer: informe --password <senha exibida pelo host>");
                return;
            };
            let identity = load_identity().expect("identidade");
            match secure::connect(tcp, &identity, &password) {
                Ok(sec) => {
                    // Conferir esta impressão com quem está do outro lado é o
                    // que fecha o buraco de intermediação — o servidor de
                    // sinalização da Fase 2 não é confiável por natureza.
                    println!("viewer: host autenticado, impressão {}", sec.remote_fingerprint());
                    run_viewer(sec);
                }
                Err(e) => eprintln!("viewer: não foi possível autenticar ({e})"),
            }
        }
        _ => run_loopback(),
    }
}

fn run_loopback() {
    SAME_PROCESS.store(true, Ordering::SeqCst);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind falhou");
    let addr = listener.local_addr().unwrap();
    println!("PCShare — Sprint 0, etapa 4: pipeline completa em {addr}");
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

/// Carrega a identidade da máquina, criando na primeira execução.
///
/// Precisa persistir: o ID que a pessoa dita por telefone é derivado da chave
/// pública, então gerar uma nova a cada início trocaria o ID da máquina e
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
    // TODO(Fase 3): a chave fica em texto claro no perfil do usuário. O
    // produto precisa protegê-la com DPAPI e permissões restritas.
    Ok(id)
}

fn arg_value(nome: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == nome).and_then(|i| a.get(i + 1).cloned())
}

fn run_host<T: Transport>(mut t: T) -> Result<(), Box<dyn std::error::Error>> {
    // `--cpu-path` força o caminho antigo (leitura da GPU + conversão na CPU),
    // para comparação A/B com o caminho sem cópia.
    let force_cpu = std::env::args().any(|a| a == "--cpu-path");

    let gpu = GpuContext::new()?;
    let mut cap = DesktopCapture::with_gpu(gpu.clone(), 0)?;
    let (w, h) = (cap.width(), cap.height());
    println!("host: capturando {w}x{h}");

    let mut enc = if force_cpu {
        H264Encoder::new(w, h, FPS, BITRATE)?
    } else {
        H264Encoder::with_gpu(&gpu, w, h, FPS, BITRATE)?
    };
    println!("host: encoder [{}] ({})", enc.name(), enc.tuning().join(", "));

    // O conversor na GPU só entra se o encoder aceitar texturas. Encoder de
    // software e driver antigo caem no caminho por memória de sistema, e é
    // melhor perder desempenho do que não funcionar.
    let mut conv = match enc.is_gpu_direct() {
        true => Some(pcshare_gpu::Nv12Converter::new(gpu.clone(), w, h)?),
        false => None,
    };
    println!(
        "host: caminho de pixels = {}",
        if conv.is_some() {
            "GPU direto (sem cópia)"
        } else {
            "memória de sistema (leitura da GPU + conversão na CPU)"
        }
    );

    let hello = Message::Hello {
        width: w,
        height: h,
        fps: FPS,
    };
    t.send(hello.channel(), &hello.encode())?;

    let mut bgra = Vec::new();
    let mut nv12 = Vec::new();
    let mut seq = 0u64;
    let mut stats = HostStats::default();
    let mut next_report = Instant::now() + Duration::from_secs(2);

    let frame_period = Duration::from_micros(1_000_000 / FPS as u64);
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

        // Nada mudou de fato: nem pixels novos, nem regiões deslocadas.
        //
        // Esta é a supressão que o achado da etapa 1 exigiu. O compositor
        // entrega frames a 60 Hz mesmo com o desktop ocioso, e medimos na
        // etapa 2 que numa tela imóvel o encoder gasta banda refinando
        // qualidade sozinho. A resposta não é ajustar o encoder — é não
        // chamá-lo.
        if !frame.has_changes() {
            stats.skipped += 1;
            t.flush()?;
            continue;
        }

        // A captura entrega a 60 Hz; o alvo é 30. Sem esta trava o host
        // produz o dobro do que o viewer consome, e a diferença não vira
        // fluidez — vira fila, e fila vira latência.
        if last_encoded.elapsed() < frame_period {
            stats.paced += 1;
            t.flush()?;
            continue;
        }

        // Contrapressão. Se o link não escoou o que já mandamos, mandar mais
        // só afunda a latência. Numa tela remota, **descartar é melhor que
        // atrasar**: ninguém quer ver o que a tela era há dois segundos.
        if t.pending_out() > BACKLOG_LIMIT {
            stats.dropped += 1;
            t.flush()?;
            continue;
        }

        last_encoded = Instant::now();
        let captured_us = now_us();

        if let Some(conv) = conv.as_mut() {
            // Caminho sem cópia: a textura nunca sai da memória de vídeo.
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
                // Carimbo do instante da *captura*, não do envio: é ele que
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
            stats.report(&t);
            next_report += Duration::from_secs(2);
        }
    }
}

#[derive(Default)]
struct HostStats {
    frames: u64,
    skipped: u64,
    /// Descartados por pacing: a captura vinha mais rápido que o alvo.
    paced: u64,
    /// Descartados por contrapressão: o link não estava escoando.
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
            "host: {} frames | leitura GPU {:.2} ms | BGRA→NV12 {:.2} ms | encode {:.2} ms \
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
                        " (não injetada)"
                    } else {
                        ""
                    }
                )
            } else {
                String::new()
            },
            if self.access_lost > 0 {
                format!(" | sessões perdidas {}", self.access_lost)
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
    // Nada pode ser desenhado antes de saber as dimensões do host.
    let Some((w, h)) = wait_hello(&mut t) else {
        eprintln!("viewer: conexão encerrada antes do Hello");
        return;
    };
    println!("viewer: host anunciou {w}x{h}");

    let mut dec = match H264Decoder::new(w, h, FPS) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("viewer: sem decodificador: {e}");
            return;
        }
    };
    println!("viewer: decoder [{}]", dec.name());

    // Sem keyframe periódico, este pedido é a única coisa que dá ao
    // decodificador um ponto de partida. Sem ele, tela preta para sempre.
    let req = Message::RequestKeyframe;
    if t.send(req.channel(), &req.encode()).is_err() {
        eprintln!("viewer: não foi possível pedir o keyframe inicial");
        return;
    }

    // `--headless <segundos>`: sem janela.
    //
    // Existe para medir a banda de uma tela realmente parada. Com a janela
    // aberta é impossível: ela exibe a tela que está sendo capturada, então
    // cada quadro exibido altera a tela e gera o próximo. A medição mediria a
    // própria realimentação.
    let headless: Option<u64> = {
        let a: Vec<String> = std::env::args().collect();
        a.iter().position(|x| x == "--headless").map(|i| {
            a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20)
        })
    };

    let mut win = match headless {
        Some(_) => None,
        None => match window::Window::new(w!("PCShare — viewer (Sprint 0)"), 960, 540) {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("viewer: não foi possível criar a janela: {e}");
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

        // ---- sondagem de latência, válida entre máquinas quaisquer
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

        // ---- entrada do usuário, de volta para o host
        let (cw, ch) = win.as_ref().map_or((0, 0), |w| w.client_size());
        for raw in win.as_mut().map(|w| w.take_input()).unwrap_or_default() {
            let ev = match raw {
                // A janela quase nunca tem o tamanho da tela remota. Mapear
                // pela proporção é o que faz o cursor pousar onde o usuário
                // apontou, e não deslocado.
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
                    // `sent_us` volta intacto: o RTT sai de uma subtração
                    // local, então vale mesmo entre máquinas com relógios
                    // sem qualquer sincronia.
                    Ok(Message::Pong { sent_us, .. }) => {
                        rtt_ms.push((now_us() - sent_us) as f64 / 1000.0);
                    }
                    Ok(_) => {}
                    Err(e) => eprintln!("viewer: mensagem inválida: {e}"),
                },
                Ok(None) => break,
                Err(e) => {
                    println!("viewer: conexão encerrada ({e})");
                    return;
                }
            }
        }

        // Todo frame precisa ser *decodificado* — H.264 é interdependente e
        // pular um estraga os seguintes. Mas só o mais recente precisa ser
        // *exibido*: converter e desenhar quadros já obsoletos é trabalho que
        // atrasa o próximo sem que ninguém veja o resultado.
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
            // Em modo headless a conversão para BGRA ainda vale a pena: ela
            // faz parte do custo do viewer, e omiti-la falsearia a medição.
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

                // Verificação visual do ida-e-volta completo. Contadores de
                // fps provam que a pipeline *corre*; só olhar um quadro prova
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

            // A latência captura→exibição some quando os relógios não são o
            // mesmo. Melhor não reportar do que reportar um número que
            // parece medida e é diferença de instantes de partida.
            let travessia = if SAME_PROCESS.load(Ordering::Relaxed) {
                format!(
                    " | captura→exibição p50 {:.1} ms  p99 {:.1} ms",
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

/// Grava BGRA como BMP de 32 bits. Formato escolhido por ser o único que se
/// escreve sem dependência: cabeçalho fixo e pixels crus, sem compressão.
fn save_bmp(path: &str, bgra: &[u8], w: i32, h: i32) -> std::io::Result<()> {
    let pixels = (w as usize) * (h as usize) * 4;
    let mut f = Vec::with_capacity(54 + pixels);

    f.extend_from_slice(b"BM");
    f.extend_from_slice(&((54 + pixels) as u32).to_le_bytes());
    f.extend_from_slice(&0u32.to_le_bytes());
    f.extend_from_slice(&54u32.to_le_bytes());

    f.extend_from_slice(&40u32.to_le_bytes());
    f.extend_from_slice(&w.to_le_bytes());
    // Altura negativa: linhas de cima para baixo, como o buffer está. Sem
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
