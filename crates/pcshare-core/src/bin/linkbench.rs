//! Sprint 0, etapa 3: transporte em 127.0.0.1.
//!
//! Roda host e viewer no mesmo PC para tirar a rede da equação, com pacotes
//! do tamanho que o encoder realmente produziu na etapa 2 (~8 KB por frame,
//! keyframe de ~120 KB a cada 10 s).
//!
//! A pergunta central não é vazão — loopback tem vazão de sobra. É se o fluxo
//! de vídeo **atrasa o input**. Host e viewer dividem uma única conexão TCP, e
//! TCP entrega em ordem estrita: um pedaço de vídeo à frente na fila segura o
//! clique que vem atrás. Medimos o RTT do canal de controle com e sem vídeo
//! correndo; a diferença entre os dois é o custo do compartilhamento.
//!
//!     cargo run --release -p pcshare-core --bin linkbench

use pcshare_core::{Channel, Message, TcpTransport, Transport};
use std::net::TcpListener;
use std::time::{Duration, Instant};

const FPS: u64 = 30;
const FRAME_BYTES: usize = 8_000;
const KEYFRAME_BYTES: usize = 120_000;
const KEYFRAME_INTERVAL: u64 = FPS * 10;
const PING_INTERVAL: Duration = Duration::from_millis(20);

fn main() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind falhou");
    let addr = listener.local_addr().unwrap();

    // `--spin` troca o sleep do laço por cessão de fatia de CPU. Serve para
    // separar a latência do transporte da latência do nosso próprio
    // agendamento: no Windows, `sleep` de 200 µs costuma dormir ~1 ms.
    let spin = std::env::args().any(|a| a == "--spin");

    let viewer = std::thread::spawn(move || run_viewer(&listener, spin));
    let mut host = TcpTransport::connect(addr).expect("connect falhou");
    println!("PCShare — Sprint 0, etapa 3: transporte local");
    println!("conectado a {}", host.peer());
    println!(
        "espera do laço: {}\n",
        if spin { "yield_now (--spin)" } else { "sleep 200 µs" }
    );

    let hello = Message::Hello {
        width: 1920,
        height: 1080,
        fps: FPS as u32,
    };
    host.send(hello.channel(), &hello.encode()).unwrap();

    let idle = measure(&mut host, Duration::from_secs(3), false, spin);
    let loaded = measure(&mut host, Duration::from_secs(8), true, spin);

    drop(host);
    let received = viewer.join().unwrap();

    println!("{:=<66}", "");
    println!("RTT DO CANAL DE CONTROLE (ida e volta, sem decodificar nada)");
    println!("{:=<66}", "");
    idle.print("sem vídeo    ");
    loaded.print("com vídeo    ");

    let delta = loaded.p99() - idle.p99();
    println!("\n  Custo do compartilhamento no p99: {delta:+.2} ms");
    if delta > 5.0 {
        println!("  → O vídeo está atrasando o input. Em TCP isso é esperado:");
        println!("    entrega ordenada faz o clique esperar o frame da frente.");
    } else {
        println!("  → Sem impacto relevante em loopback. Não conclua daí que");
        println!("    não haverá impacto na internet: aqui não há perda nem");
        println!("    limite de banda, que são justamente o que dispara o");
        println!("    bloqueio de cabeça de fila do TCP.");
    }

    println!("\n{:=<66}", "");
    println!("VÍDEO");
    println!("{:=<66}", "");
    println!("  Frames enviados : {}", loaded.frames);
    println!("  Frames recebidos: {}", received.frames);
    println!(
        "  Bytes recebidos : {:.2} MB  ({:.2} Mbps)",
        received.bytes as f64 / 1e6,
        received.bytes as f64 * 8.0 / loaded.elapsed_s / 1e6
    );
    println!("  Fila de saída   : máx {} KB", loaded.max_pending / 1024);
    println!(
        "  Perdidos        : {}",
        loaded.frames.saturating_sub(received.frames)
    );
}

struct Stats {
    rtt_ms: Vec<f64>,
    frames: u64,
    max_pending: usize,
    elapsed_s: f64,
}

impl Stats {
    fn p(&self, q: f64) -> f64 {
        if self.rtt_ms.is_empty() {
            return 0.0;
        }
        self.rtt_ms[((self.rtt_ms.len() - 1) as f64 * q).round() as usize]
    }
    fn p99(&self) -> f64 {
        self.p(0.99)
    }

    fn print(&self, label: &str) {
        println!(
            "  {label} p50 {:>6.3} ms   p99 {:>6.3} ms   máx {:>6.3} ms   ({} amostras)",
            self.p(0.50),
            self.p(0.99),
            self.rtt_ms.last().copied().unwrap_or(0.0),
            self.rtt_ms.len(),
        );
    }
}

/// Envia pings continuamente e, se `with_video`, também vídeo no ritmo alvo.
fn measure(host: &mut TcpTransport, duration: Duration, with_video: bool, spin: bool) -> Stats {
    let start = Instant::now();
    let deadline = start + duration;
    let frame_period = Duration::from_micros(1_000_000 / FPS);

    let mut rtt_ms = Vec::new();
    let mut nonce = 0u64;
    let mut seq = 0u64;
    let mut max_pending = 0usize;
    let mut next_ping = start;
    let mut next_frame = start;
    let mut payload = vec![0u8; KEYFRAME_BYTES];

    while Instant::now() < deadline {
        let now = Instant::now();

        if now >= next_ping {
            nonce += 1;
            let m = Message::Ping {
                nonce,
                sent_us: start.elapsed().as_micros() as i64,
            };
            host.send(m.channel(), &m.encode()).unwrap();
            next_ping += PING_INTERVAL;
        }

        if with_video && now >= next_frame {
            let is_keyframe = seq % KEYFRAME_INTERVAL == 0;
            let len = if is_keyframe {
                KEYFRAME_BYTES
            } else {
                // Variação de tamanho como num fluxo real, para não medir um
                // caso artificialmente regular.
                FRAME_BYTES + (seq as usize * 131) % 4_000
            };
            payload.resize(len, 0xAB);
            let m = Message::Video {
                seq,
                time_us: start.elapsed().as_micros() as i64,
                is_keyframe,
                data: payload.clone(),
            };
            host.send(m.channel(), &m.encode()).unwrap();
            seq += 1;
            next_frame += frame_period;
        }

        // Bombear é obrigatório: `send` só enfileira quando o buffer do
        // socket enche. Ver o teste `excedente_fica_pendente_sem_bloquear`.
        host.flush().unwrap();
        max_pending = max_pending.max(host.pending_out());

        while let Some((channel, payload)) = host.poll().unwrap() {
            if channel != Channel::Control {
                continue;
            }
            if let Ok(Message::Pong { sent_us, .. }) = Message::decode(&payload) {
                let now_us = start.elapsed().as_micros() as i64;
                rtt_ms.push((now_us - sent_us) as f64 / 1000.0);
            }
        }

        idle_wait(spin);
    }

    rtt_ms.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Stats {
        rtt_ms,
        frames: seq,
        max_pending,
        elapsed_s: start.elapsed().as_secs_f64(),
    }
}

#[derive(Default)]
struct Received {
    frames: u64,
    bytes: u64,
}

/// Reflete cada Ping como Pong e contabiliza o vídeo. Encerra quando o host
/// fecha a conexão.
fn idle_wait(spin: bool) {
    if spin {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn run_viewer(listener: &TcpListener, spin: bool) -> Received {
    let mut t = TcpTransport::accept(listener).expect("accept falhou");
    let mut got = Received::default();

    loop {
        match t.poll() {
            Ok(Some((_, payload))) => match Message::decode(&payload) {
                Ok(Message::Ping { nonce, sent_us }) => {
                    let pong = Message::Pong { nonce, sent_us };
                    if t.send(pong.channel(), &pong.encode()).is_err() {
                        return got;
                    }
                }
                Ok(Message::Video { data, .. }) => {
                    got.frames += 1;
                    got.bytes += data.len() as u64;
                }
                Ok(_) => {}
                Err(e) => eprintln!("viewer: mensagem inválida: {e}"),
            },
            Ok(None) => {
                if t.flush().is_err() {
                    return got;
                }
                idle_wait(spin);
            }
            // Host fechou: fim normal do benchmark.
            Err(_) => return got,
        }
    }
}
