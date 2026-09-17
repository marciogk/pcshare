//! Motor de sessão do PCShare: captura, codec, transporte, host e viewer.
//!
//! captura DXGI → BGRA → NV12 → H.264 → QUIC → H.264 → NV12 → BGRA → janela
//!
//! A interface (`pcshare-ui`) chama [`assistir_por_id`] e [`hospedar_por_id`]
//! numa thread; a linha de comando (`pcshare-spike`) chama [`run`]. Exemplos:
//!
//! ```text
//! pcshare-spike                                  # tudo em 127.0.0.1
//! pcshare-spike host 0.0.0.0:7000
//! pcshare-spike viewer 192.168.0.10:7000
//! ```
//!
//! O modo padrão roda host e viewer no mesmo processo, o que permite medir a
//! latência interna com um relógio só. Esse número **não** é glass-to-glass:
//! ele ignora o tempo que o compositor leva para pôr o pixel no monitor e o
//! que o monitor leva para acendê-lo. O protocolo com cronômetro e câmera, em
//! `docs/SPRINT-0.md`, continua sendo a medida que vale.

use pcshare_input as input;
pub mod window;

use std::collections::VecDeque;
use std::net::{SocketAddr, TcpListener};
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

/// Taxa alvo, sobreponível por `PCSHARE_FPS`.
///
/// 60 e não 30, por medição: a 30 fps uma mudança espera em média meio
/// intervalo só pela vez dela, e a latência medida caiu de 66,7 para 53,6 ms
/// ao dobrar o alvo. O custo foi desprezível — CPU de 6,9% para 7,3% de um
/// núcleo, banda comparável.
///
/// Na prática o valor funciona como teto: o encoder satura em torno de
/// 40 fps nesta máquina, e o laço simplesmente roda o mais rápido que
/// consegue.
fn fps() -> u32 {
    std::env::var("PCSHARE_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (1..=120).contains(v))
        .unwrap_or(60)
}
/// Taxa de bits do H.264, em bits por segundo, sobreponível por
/// `PCSHARE_BITRATE` (em Mbps).
///
/// Subiu de 8 para 12 Mbps como padrão em 10/09/2026: 8 bastavam na LAN, mas
/// texto de desktop — bordas finas de alta frequência — pede mais bits para
/// não sair borrado, e num link rápido (LAN, Tailscale) sobra folga. É o host
/// quem codifica, então esta variável se define na máquina **controlada**.
fn bitrate() -> u32 {
    std::env::var("PCSHARE_BITRATE")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .filter(|v| (1..=100).contains(v))
        .map(|mbps| mbps * 1_000_000)
        .unwrap_or(12_000_000)
}
/// Espera máxima por um frame novo. É espera bloqueante em evento (decisão
/// D8), mas limitada para que o mesmo laço também atenda o transporte.
const CAPTURE_TIMEOUT_MS: u32 = 8;
/// Acima deste acúmulo na fila de saída, o host para de codificar até o link
/// escoar. Meio megabyte a 3 Mbps já representa mais de um segundo de atraso.
const BACKLOG_LIMIT: usize = 512 * 1024;
/// Pausa entre tentativas de recuperar a captura enquanto ela está cega.
///
/// Exceção deliberada à decisão D8, que proíbe esperar por relógio: aqui não
/// há evento algum para aguardar — o Windows não avisa quando sai do Secure
/// Desktop. Sem esta pausa o laço tenta ~2000 vezes por segundo e queima um
/// núcleo inteiro durante cada prompt de UAC. Medido.
const BACKOFF_CEGO: Duration = Duration::from_millis(50);

/// Códigos de varredura das teclas que formam o atalho do Ctrl+Alt+Del.
///
/// Comparados pelo byte baixo: as versões da direita chegam com o prefixo
/// estendido `0xE0`, e para este atalho tanto faz qual das duas o operador
/// usou.
const SCAN_CTRL: u32 = 0x1D;
const SCAN_ALT: u32 = 0x38;
const SCAN_END: u32 = 0x4F;
const SCAN_F: u32 = 0x21; // enviar arquivo ao host (Ctrl+Alt+F)
const SCAN_G: u32 = 0x22; // pedir arquivo ao host, para baixar (Ctrl+Alt+G)

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

/// Cala os relatórios periódicos do host. Na medição de latência eles
/// atropelariam a linha de progresso.
static QUIET: AtomicBool = AtomicBool::new(false);

fn now_us() -> i64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as i64
}

/// O ID e a impressão digital desta máquina, para a interface exibir. Lê (ou
/// cria) a mesma identidade que o host anuncia.
pub fn identidade_local() -> Option<(String, String)> {
    let id = load_identity().ok()?;
    Some((id.display_id(), id.fingerprint()))
}

/// Estágios de uma sessão, para a interface mostrar status ao vivo. Grosso de
/// propósito: o pareamento e o fim são o que a pessoa precisa ver.
#[derive(Debug, Clone)]
pub enum Progresso {
    /// Procurando a máquina pelo encontro.
    Procurando,
    /// Pareado e em sessão (o vídeo está na tela).
    EmSessao,
    /// A sessão terminou (janela fechada ou queda).
    Encerrou,
    /// Não deu para nem parear (ID errado, offline, ocupada).
    Falhou(String),
}

/// Conecta a uma máquina pelo ID, pelo servidor de encontro, e roda a sessão do
/// viewer (abre a janela e bloqueia até ela fechar). Feito para a interface
/// chamar **numa thread**, no lugar de lançar o executável.
///
/// `titulo` vai para a barra de título da janela de sessão (o apelido da
/// máquina, quando houver). `relatar` recebe cada mudança de estágio.
pub fn assistir_por_id(
    encontro: SocketAddr,
    id: &str,
    senha: &str,
    titulo: &str,
    mut relatar: impl FnMut(Progresso),
) -> std::io::Result<()> {
    START.get_or_init(Instant::now);
    let identity = load_identity().map_err(erro_io)?;
    relatar(Progresso::Procurando);
    let t = match pcshare_encontro::conectar_por_id(encontro, id) {
        Ok(t) => t,
        Err(e) => {
            relatar(Progresso::Falhou(e.to_string()));
            return Err(e);
        }
    };
    relatar(Progresso::EmSessao);
    assistir(t, &identity, senha, false, titulo);
    relatar(Progresso::Encerrou);
    Ok(())
}

/// Registra esta máquina no encontro e atende sessões indefinidamente
/// (compartilha esta tela). Também bloqueia — a interface chama numa thread.
pub fn hospedar_por_id(
    encontro: SocketAddr,
    senha: &str,
    mut relatar: impl FnMut(Progresso),
) -> std::io::Result<()> {
    START.get_or_init(Instant::now);
    let identity = load_identity().map_err(erro_io)?;
    let meu_id = identity.display_id();
    relatar(Progresso::EmSessao);
    atender(
        || pcshare_encontro::registrar_e_esperar(encontro, &meu_id),
        &identity,
        senha,
        false,
    );
    Ok(())
}

fn erro_io(e: Box<dyn std::error::Error>) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Ponto de entrada da linha de comando. O binário `pcshare-spike` é só uma
/// casca que chama isto; toda a lógica de despacho mora aqui.
pub fn run(args: Vec<String>) {
    START.get_or_init(Instant::now);

    if args.iter().any(|a| a == "--view-only") {
        VIEW_ONLY.store(true, Ordering::SeqCst);
        println!("modo somente visualização: entrada não será injetada");
    }

    // `--sem-cripto` mantém o caminho em texto claro para comparação de
    // custo. Nunca deve existir num binário distribuído.
    let plaintext = args.iter().any(|a| a == "--sem-cripto");
    // `--tcp` volta ao transporte antigo, para comparação.
    let quic = !args.iter().any(|a| a == "--tcp");

    match args.first().map(String::as_str) {
        Some("host") => {
            let identity = load_identity().expect("identidade");
            let password = arg_value("--password").unwrap_or_else(secure::random_password);

            println!("\n  ID desta máquina : {}", identity.display_id());
            println!("  Senha da sessão  : {password}");
            println!("  Impressão digital: {}\n", identity.fingerprint());

            // `--encontro <servidor>`: em vez de escutar numa porta — que
            // ninguém alcança atrás de um NAT —, o host conecta **de saída** a
            // um servidor de encontro e se registra sob o próprio ID. É o
            // caminho que funciona pela internet.
            //
            // A mesma função `atender` serve aos dois modos: ela só quer um
            // fechamento que devolva um transporte pronto. Escutar ou
            // registrar-se são duas formas de obter o mesmo.
            if let Some(enc) = arg_value("--encontro") {
                let enc: SocketAddr = enc.parse().expect("endereço do encontro inválido");
                let meu_id = identity.display_id();
                println!("registrando no encontro {enc} como {meu_id}\n");
                atender(
                    || pcshare_encontro::registrar_e_esperar(enc, &meu_id),
                    &identity,
                    &password,
                    plaintext,
                );
            } else {
                let bind = args.get(1).cloned().unwrap_or_else(|| "0.0.0.0:7000".into());
                println!("host escutando em {bind} ({})\n", transporte(quic));
                if quic {
                    let listener =
                        QuicListener::bind(bind.parse().expect("endereço inválido")).expect("bind");
                    atender(|| listener.accept(), &identity, &password, plaintext);
                } else {
                    let listener = TcpListener::bind(&bind).expect("bind falhou");
                    atender(
                        || TcpTransport::accept(&listener),
                        &identity,
                        &password,
                        plaintext,
                    );
                }
            }
        }
        Some("viewer") => {
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

            // `--encontro <servidor> --id <id>`: conecta pela internet,
            // digitando o ID da máquina em vez do endereço de rede dela.
            if let Some(enc) = arg_value("--encontro") {
                let enc: SocketAddr = enc.parse().expect("endereço do encontro inválido");
                let Some(id) = arg_value("--id") else {
                    eprintln!("viewer: informe --id <ID da máquina> junto com --encontro");
                    return;
                };
                println!("procurando a máquina {id} pelo encontro {enc}");
                match pcshare_encontro::conectar_por_id(enc, &id) {
                    Ok(t) => assistir(t, &identity, &password, plaintext, &format!("PCShare — {id}")),
                    // Mensagem clara em vez de pânico: aqui o erro é operacional
                    // (ID errado, máquina offline, ocupada), não um defeito.
                    Err(e) => eprintln!("não foi possível conectar: {e}"),
                }
                return;
            }

            let addr = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:7000".into());
            println!("viewer conectando a {addr} ({})", transporte(quic));
            if quic {
                let t = QuicTransport::connect(addr.parse().expect("endereço inválido"))
                    .expect("connect falhou");
                assistir(t, &identity, &password, plaintext, "PCShare — viewer");
            } else {
                let t = TcpTransport::connect(&addr).expect("connect falhou");
                assistir(t, &identity, &password, plaintext, "PCShare — viewer");
            }
        }
        Some("latencia-captura") => {
            let amostras: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(30);
            run_capture_latency(amostras);
        }
        Some("latencia") => {
            let amostras: usize = args
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            run_latency(amostras);
        }
        // Mede a arquitetura de verdade: agente, supervisor e viewer, três
        // processos. Exige o serviço rodando na **mesma máquina**.
        Some("latencia-sessao") => {
            let endereco = args
                .get(1)
                .cloned()
                .unwrap_or_else(|| "127.0.0.1:7000".into());
            let Some(senha) = arg_value("--password") else {
                eprintln!("informe --password <senha do supervisor>");
                return;
            };
            let amostras: usize = arg_value("--amostras")
                .and_then(|v| v.parse().ok())
                .unwrap_or(20);
            latencia_de_sessao(&endereco, &senha, amostras);
        }
        _ => run_loopback(),
    }
}

// ------------------------------------------------- medição de latência

/// Onde o marcador fica na tela, e de que tamanho.
const MARK_X: i32 = 24;
const MARK_Y: i32 = 24;
const MARK_SIZE: i32 = 96;
/// Luma acima disto conta como branco. Preto fica perto de 16, branco de 235.
const LUMA_LIMIAR: u8 = 128;

/// Mede **só** o trecho tela → captura DXGI, sem encoder, rede nem
/// decodificador.
///
/// Existe para separar duas explicações muito diferentes para os ~60 ms que a
/// decomposição atribuiu a esse trecho:
///
/// - se o Windows for o responsável (composição do DWM mais a latência
///   própria da Desktop Duplication), o piso é do sistema e o produto tem
///   pouco a fazer;
/// - se for o laço do host — ritmo, supressão, espera — é nosso e dá para
///   consertar.
///
/// Aqui não há laço de host: pinta e pergunta à captura, direto.
fn run_capture_latency(amostras: usize) {
    println!("PCShare — latência só da captura (sem encoder, rede ou decode)");
    println!("Não mexa no mouse nem cubra o quadrado.\n");

    let mut cap = match DesktopCapture::new(0) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("falha ao abrir a captura: {e}");
            return;
        }
    };
    let (w, h) = (cap.width() as usize, cap.height() as usize);
    let mut marcador = match window::MarkerWindow::new(MARK_X, MARK_Y, MARK_SIZE) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("falha ao criar o marcador: {e}");
            return;
        }
    };

    let sx = (MARK_X + MARK_SIZE / 2) as usize;
    let sy = (MARK_Y + MARK_SIZE / 2) as usize;
    let mut bgra = Vec::new();
    let mut medidas: Vec<f64> = Vec::with_capacity(amostras);
    let mut alvo_branco = true;
    let mut t0: Option<Instant> = None;

    marcador.paint(false);
    let assentar = Instant::now() + Duration::from_millis(400);
    let limite = Instant::now() + Duration::from_secs(60);

    while medidas.len() < amostras && Instant::now() < limite {
        marcador.pump();

        match cap.next_frame(50) {
            Ok(Some(_)) => {}
            Ok(None) => continue,
            Err(CaptureError::AccessLost) => {
                let _ = cap.recover();
                continue;
            }
            Err(e) => {
                eprintln!("erro na captura: {e}");
                return;
            }
        }
        if !cap.copy_bgra(&mut bgra).unwrap_or(false) {
            continue;
        }
        if Instant::now() < assentar {
            continue;
        }

        // BGRA: basta um canal para distinguir preto de branco.
        let idx = (sy * w + sx) * 4;
        let Some(&azul) = bgra.get(idx) else { continue };
        let esta_branco = azul > LUMA_LIMIAR;

        match t0 {
            None => {
                alvo_branco = !esta_branco;
                marcador.paint(alvo_branco);
                t0 = Some(Instant::now());
            }
            Some(inicio) => {
                if esta_branco == alvo_branco {
                    medidas.push(inicio.elapsed().as_secs_f64() * 1000.0);
                    t0 = None;
                }
            }
        }
    }

    let _ = h;
    if medidas.len() < 3 {
        eprintln!("amostras insuficientes ({})", medidas.len());
        return;
    }
    medidas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    println!("\n{:=<56}", "");
    println!("TELA → CAPTURA DXGI  ({} amostras)", medidas.len());
    println!("{:=<56}", "");
    println!(
        "  p50 {:.1} ms   p90 {:.1} ms   mín {:.1} ms   máx {:.1} ms",
        pct(&medidas, 0.50),
        pct(&medidas, 0.90),
        medidas[0],
        medidas[medidas.len() - 1],
    );
    println!("\n  Só DWM + Desktop Duplication. Nada do nosso laço de host.");
}

/// Mede a latência que os cronômetros internos não enxergam.
///
/// Os 16,8 ms medidos até aqui iam da captura até a entrega dos pixels ao
/// Windows. Faltava o compositor — e ele não é pequeno.
///
/// O truque é fechar o laço pela própria tela: um marcador muda de cor, e
/// esperamos essa mudança reaparecer num quadro **decodificado**. O caminho
/// percorrido é descarga na tela → compositor → captura DXGI → encode → rede →
/// decode.
///
/// **O que ainda falta para ser glass-to-glass de verdade:** a apresentação
/// final no viewer e o tempo de resposta físico dos monitores, que nenhum
/// software alcança. Some algo entre 10 e 25 ms ao resultado para estimar o
/// número que a pessoa sente — ou meça com câmera, como descrito em
/// `docs/PROXIMOS-PASSOS.md`.
fn run_latency(amostras: usize) {
    QUIET.store(true, Ordering::SeqCst);
    println!("PCShare — medição de latência pelo compositor");
    println!("Não mexa no mouse nem cubra o quadrado no canto superior esquerdo.\n");

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind falhou");
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let t = TcpTransport::accept(&listener).expect("accept falhou");
        let _ = run_host(t);
    });

    let t = TcpTransport::connect(addr).expect("connect falhou");
    // Host embutido, mesmo processo: `now_us` de ambos os lados tem a mesma
    // origem, então a decomposição é válida.
    medir_latencia(t, amostras, now_us);
}

/// Mede a latência de uma sessão **real**, contra o serviço.
///
/// Existe porque o `latencia` acima embute o host no próprio processo, e desde
/// que a pipeline migrou para o agente isso deixou de ser a arquitetura que
/// interessa medir. Aqui os três processos são os de verdade: agente,
/// supervisor e este viewer.
///
/// Só faz sentido com o serviço na **mesma máquina**. O marcador precisa estar
/// na tela que está sendo capturada, e o relógio de parede precisa ser o mesmo.
fn latencia_de_sessao(endereco: &str, senha: &str, amostras: usize) {
    QUIET.store(true, Ordering::SeqCst);
    println!("PCShare — latência da sessão real (agente + supervisor + viewer)");
    println!("Não mexa no mouse nem cubra o quadrado no canto superior esquerdo.\n");

    let identity = match load_identity() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("sem identidade: {e}");
            return;
        }
    };
    let endereco = match endereco.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("endereço inválido: {e}");
            return;
        }
    };
    let t = match QuicTransport::connect(endereco) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("não conectou: {e}");
            return;
        }
    };
    let sec = match secure::connect(t, &identity, senha) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("handshake recusado: {e} (senha errada?)");
            return;
        }
    };
    // O agente carimba em relógio de parede justamente para atravessar
    // processos. Numa máquina só, isto é comparável; entre máquinas não seria,
    // e a decomposição é descartada mais adiante se os números não fecharem.
    medir_latencia(sec, amostras, || {
        pcshare_capture::linha_do_tempo::agora_us() as i64
    });
}

/// O laço do marcador, comum às duas medições.
///
/// `relogio` precisa ter a **mesma origem** do carimbo que vem em
/// `Message::Video`. Quando não tem, a decomposição sai absurda — e é por isso
/// que ela é conferida antes de ser impressa em vez de sair como se fosse
/// medida.
fn medir_latencia<T: Transport>(mut t: T, amostras: usize, relogio: fn() -> i64) {
    let Some((w, h)) = wait_hello(&mut t) else {
        eprintln!("conexão encerrada antes do Hello");
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
            eprintln!("não foi possível criar o marcador: {e}");
            return;
        }
    };

    // Amostra o centro do marcador, longe das bordas — a compressão suaviza
    // transições bruscas, e o miolo do quadrado é o ponto mais confiável.
    let sx = (MARK_X + MARK_SIZE / 2) as usize;
    let sy = (MARK_Y + MARK_SIZE / 2) as usize;
    if sx >= w as usize || sy >= h as usize {
        eprintln!("marcador fora da tela capturada");
        return;
    }

    // Duas medidas do mesmo evento. A diferença entre elas isola o trecho
    // anterior à captura — compositor e ritmo — do trecho da nossa pipeline.
    let mut medidas: Vec<f64> = Vec::with_capacity(amostras);
    let mut pipeline: Vec<f64> = Vec::with_capacity(amostras);
    let mut capturas: VecDeque<i64> = VecDeque::new();
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
                    if let Ok(Message::Video { data, time_us, .. }) = Message::decode(&payload) {
                        capturas.push_back(time_us);
                        // A fila casa carimbo com quadro **por ordem**, e todo
                        // quadro que entra no decodificador sem sair — os que
                        // antecedem o primeiro keyframe, por exemplo —
                        // desalinha a fila para sempre.
                        //
                        // Sem este teto a deriva foi medida crescendo de 10 ms
                        // para 60 ms ao longo de cinco execuções seguidas, e as
                        // primeiras passaram por medida boa. Sem B-frames o
                        // decodificador não segura mais que um ou dois quadros,
                        // então o que passar disso é desalinho, não pipeline.
                        while capturas.len() > 3 {
                            capturas.pop_front();
                        }
                        let _ = dec.decode(&data, 0);
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    eprintln!("conexão encerrada ({e})");
                    return;
                }
            }
        }

        for f in dec.take_frames() {
            // O decodificador preserva a ordem, então o carimbo mais antigo
            // pendente é o deste quadro.
            let capturado_us = capturas.pop_front();

            let idx = sy * f.width as usize + sx;
            let Some(&luma) = f.data.get(idx) else { continue };
            let esta_branco = luma > LUMA_LIMIAR;

            // Espera a tela assentar antes de começar a cronometrar.
            if Instant::now() < estabilizou_ate {
                continue;
            }

            match flip_em {
                // Ocioso: o alvo é sempre o oposto do que está na tela agora.
                // Derivar do estado observado, e não de um alvo guardado, é o
                // que garante progresso — a primeira versão comparava os dois
                // e travava assim que coincidiam.
                None => {
                    esperando_branco = !esta_branco;
                    marcador.paint(esperando_branco);
                    flip_em = Some(Instant::now());
                }
                Some(t0) => {
                    if esta_branco == esperando_branco {
                        medidas.push(t0.elapsed().as_secs_f64() * 1000.0);
                        if let Some(cap) = capturado_us {
                            pipeline.push((relogio() - cap) as f64 / 1000.0);
                        }
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
    println!("LATÊNCIA ATÉ O QUADRO DECODIFICADO  ({} amostras)", medidas.len());
    println!("{:=<58}", "");
    println!("  p50 {:.1} ms   p99 {:.1} ms   mín {:.1} ms   máx {:.1} ms",
        p50,
        pct(&medidas, 0.99),
        medidas[0],
        medidas[medidas.len() - 1],
    );
    if !pipeline.is_empty() {
        pipeline.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let pipe50 = pct(&pipeline, 0.50);

        // O trecho captura→decodificação é parte do total: tem de ser positivo
        // e menor que ele. Fora disso, o carimbo do quadro e este relógio não
        // partem da mesma origem — máquinas diferentes, ou processos com
        // `Instant` próprio.
        //
        // Isto não é zelo excessivo. Uma medição anterior deste projeto
        // reportou −2073 ms, que era exatamente a diferença entre os instantes
        // de partida de dois processos, e passou por número real por um tempo.
        // Um instrumento que não sabe reconhecer a própria falha é pior que
        // nenhum.
        if pipe50 <= 0.0 || pipe50 > p50 {
            println!("\n  DECOMPOSIÇÃO INDISPONÍVEL");
            println!("    O carimbo dos quadros não compartilha origem com este relógio");
            println!("    (captura→decodificação daria {pipe50:.1} ms, contra {p50:.1} ms de total).");
            println!("    O total acima continua válido: ele sai de um relógio só.");
            println!("\n  Falta: apresentação no viewer e resposta física do monitor.");
            println!("\n  Estimativa glass-to-glass: {:.0} a {:.0} ms", p50 + 10.0, p50 + 25.0);
            return;
        }

        println!("\n  DECOMPOSIÇÃO (p50)");
        println!(
            "    tela → captura DXGI : {:>6.1} ms   compositor + ritmo",
            p50 - pipe50
        );
        println!(
            "    captura → decodific.: {:>6.1} ms   encode + rede + decode",
            pipe50
        );
        println!("    {:-<44}", "");
        println!("    total               : {p50:>6.1} ms");
    }
    println!("\n  Falta: apresentação no viewer e resposta física do monitor.");
    println!("\n  Estimativa glass-to-glass: {:.0} a {:.0} ms", p50 + 10.0, p50 + 25.0);
    println!("  Meta do Sprint 0: abaixo de 80 ms.");
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
    run_viewer(t, "PCShare — loopback");
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

fn transporte(quic: bool) -> &'static str {
    if quic { "QUIC" } else { "TCP" }
}

/// Atende sessões indefinidamente, uma após a outra.
///
/// O host **precisa** voltar a escutar quando uma sessão acaba. A versão
/// anterior aceitava uma conexão e encerrava: se a sessão caísse — queda de
/// rede, viewer fechado, suspensão da máquina — só reiniciando o programa na
/// máquina remota, que é justamente a que o usuário não alcança. Foi o defeito
/// mais grave encontrado no teste entre duas máquinas.
///
/// Uma falha de autenticação também não pode derrubar o host: senha errada
/// deve custar uma sessão, não o atendimento inteiro.
fn atender<T, F>(mut aceitar: F, identity: &Identity, password: &str, plaintext: bool)
where
    T: Transport,
    F: FnMut() -> std::io::Result<T>,
{
    /// Desistir só depois de muitas falhas seguidas evita tanto o encerramento
    /// prematuro quanto um laço girando em falso para sempre.
    const MAX_FALHAS_SEGUIDAS: u32 = 50;

    let mut n = 0u64;
    let mut falhas = 0u32;
    loop {
        println!("aguardando conexão...");
        let t = match aceitar() {
            Ok(t) => {
                falhas = 0;
                t
            }
            Err(e) => {
                // Uma tentativa de conexão que morre no meio não pode derrubar
                // o host — foi exatamente assim que ele encerrou sozinho num
                // teste real, logo após uma sessão normal terminar.
                falhas += 1;
                eprintln!("conexão recusada ou incompleta ({e}); continuando a escutar");
                if falhas >= MAX_FALHAS_SEGUIDAS {
                    eprintln!("{MAX_FALHAS_SEGUIDAS} falhas seguidas ao aceitar; desistindo");
                    return;
                }
                continue;
            }
        };

        n += 1;
        println!("--- sessão {n} iniciada ---");
        match servir(t, identity, password, plaintext) {
            Ok(()) => println!("--- sessão {n} encerrada ---\n"),
            Err(e) => match classificar(e.as_ref()) {
                Some(motivo) => println!("--- sessão {n} encerrada: {motivo} ---\n"),
                None => println!("--- sessão {n} encerrada com falha: {e} ---\n"),
            },
        }
        // TODO(produto): senha de sessão deveria ser sorteada a cada
        // atendimento, como fazem AnyDesk e TeamViewer. Mantida fixa aqui para
        // não invalidar a senha que o usuário já anotou durante os testes.
    }
}

/// Traduz os fins de sessão esperados para linguagem de gente.
///
/// Nem todo `Err` é defeito. O viewer fechar a janela chega aqui como
/// "unexpected end of file", e uma senha errada como "decrypt error" — dois
/// desfechos perfeitamente normais que, escritos assim no log, fariam qualquer
/// um procurar um problema que não existe.
///
/// Devolve `None` quando o erro é de fato inesperado e merece atenção.
fn classificar(e: &(dyn std::error::Error + 'static)) -> Option<&'static str> {
    use std::io::ErrorKind::*;
    let io = e.downcast_ref::<std::io::Error>()?;
    match io.kind() {
        UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted => {
            Some("viewer desconectou")
        }
        PermissionDenied => Some("autenticação recusada (senha incorreta)"),
        TimedOut => Some("tempo esgotado esperando o viewer"),
        _ => None,
    }
}

/// Handshake do lado do host e entrada no laço de captura.
fn servir<T: Transport>(
    t: T,
    identity: &Identity,
    password: &str,
    plaintext: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if plaintext {
        eprintln!("AVISO: sessão SEM criptografia");
        return run_host(t);
    }
    let sec = secure::accept(t, identity, password)?;
    println!("host: sessão cifrada com {}", sec.remote_fingerprint());
    run_host(sec)
}

/// Handshake do lado do viewer e entrada no laço de exibição.
fn assistir<T: Transport>(
    t: T,
    identity: &Identity,
    password: &str,
    plaintext: bool,
    titulo: &str,
) {
    if plaintext {
        eprintln!("AVISO: sessão SEM criptografia");
        run_viewer(t, titulo);
        return;
    }
    match secure::connect(t, identity, password) {
        Ok(sec) => {
            // Conferir esta impressão com quem está do outro lado é o que
            // fecha o buraco de intermediação — o servidor de sinalização da
            // Fase 2 não é confiável por natureza.
            println!(
                "viewer: host autenticado, impressão {}",
                sec.remote_fingerprint()
            );
            run_viewer(sec, titulo);
        }
        Err(e) => eprintln!("viewer: não foi possível autenticar ({e})"),
    }
}

fn arg_value(nome: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == nome).and_then(|i| a.get(i + 1).cloned())
}

/// Espera o viewer anunciar a resolução do monitor dele, até `prazo`. Ignora
/// outras mensagens iniciais (um ping adiantado, por exemplo). Devolve `None`
/// se nada chegar a tempo — aí o host mantém a resolução nativa.
fn esperar_resolucao<T: Transport>(t: &mut T, prazo: Duration) -> Option<(u32, u32)> {
    let limite = Instant::now() + prazo;
    loop {
        match t.poll() {
            Ok(Some((Channel::Control, payload))) => {
                if let Ok(Message::SetResolution { width, height }) = Message::decode(&payload) {
                    return Some((width, height));
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                if Instant::now() >= limite {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => return None,
        }
    }
}

/// Reduz (nunca amplia) `n` para caber em `r`, preservando a proporção. As
/// dimensões saem **pares**, que o NV12 exige.
fn resolucao_alvo(nw: u32, nh: u32, rw: u32, rh: u32) -> (u32, u32) {
    if rw == 0 || rh == 0 {
        return (nw, nh);
    }
    let fator = (rw as f64 / nw as f64)
        .min(rh as f64 / nh as f64)
        .min(1.0);
    let ow = ((nw as f64 * fator).round() as u32) & !1;
    let oh = ((nh as f64 * fator).round() as u32) & !1;
    (ow.max(2), oh.max(2))
}

/// Tamanho do monitor principal, em pixels. É o que o viewer pede ao host, para
/// não receber mais pixels do que a tela dele mostra.
fn tamanho_monitor() -> (u32, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN).max(1) as u32;
        let h = GetSystemMetrics(SM_CYSCREEN).max(1) as u32;
        (w, h)
    }
}

/// Resolução que o viewer pede ao host: o monitor, ou o que `PCSHARE_VIEWER_RES`
/// (formato `LARGURAxALTURA`) forçar — útil para limitar a banda num link lento
/// e para testar a redução sem depender do tamanho da tela.
fn resolucao_desejada() -> (u32, u32) {
    if let Ok(s) = std::env::var("PCSHARE_VIEWER_RES")
        && let Some((w, h)) = s.split_once(['x', 'X'])
        && let (Ok(w), Ok(h)) = (w.trim().parse::<u32>(), h.trim().parse::<u32>())
        && w > 0
        && h > 0
    {
        return (w, h);
    }
    tamanho_monitor()
}

fn run_host<T: Transport>(mut t: T) -> Result<(), Box<dyn std::error::Error>> {
    // `--cpu-path` força o caminho antigo (leitura da GPU + conversão na CPU),
    // para comparação A/B com o caminho sem cópia.
    let force_cpu = std::env::args().any(|a| a == "--cpu-path");

    let gpu = GpuContext::new()?;
    let mut cap = DesktopCapture::with_gpu(gpu.clone(), 0)?;
    let (nw, nh) = (cap.width(), cap.height());
    println!("host: capturando {nw}x{nh}");

    // O viewer manda a resolução do monitor dele logo ao conectar. Esperamos
    // um instante por ela antes de montar o encoder, para já codificar no
    // tamanho que ele vai exibir — e não mandar pixels a mais, que só borrariam
    // ao encolher na tela do viewer.
    let pedido = esperar_resolucao(&mut t, Duration::from_millis(400));

    // Encoder primeiro em tamanho nativo, só para saber se o caminho é GPU
    // direto — é nele que o processador de vídeo reduz a imagem por hardware.
    let mut enc = if force_cpu {
        H264Encoder::new(nw, nh, fps(), bitrate())?
    } else {
        H264Encoder::with_gpu(&gpu, nw, nh, fps(), bitrate())?
    };
    let gpu_direct = enc.is_gpu_direct();

    // Só reduz no caminho GPU. No caminho por CPU, mantém o nativo.
    let (ow, oh) = match pedido {
        Some((rw, rh)) if gpu_direct => resolucao_alvo(nw, nh, rw, rh),
        _ => (nw, nh),
    };
    if (ow, oh) != (nw, nh) {
        println!("host: reduzindo {nw}x{nh} -> {ow}x{oh} para o monitor do viewer");
        enc = H264Encoder::with_gpu(&gpu, ow, oh, fps(), bitrate())?;
    }
    println!("host: encoder [{}] ({})", enc.name(), enc.tuning().join(", "));

    // O conversor na GPU só entra se o encoder aceitar texturas. Encoder de
    // software e driver antigo caem no caminho por memória de sistema, e é
    // melhor perder desempenho do que não funcionar.
    let mut conv = match gpu_direct {
        true => Some(pcshare_gpu::Nv12Converter::new(gpu.clone(), nw, nh, ow, oh)?),
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
        width: ow,
        height: oh,
        fps: fps(),
    };
    t.send(hello.channel(), &hello.encode())?;

    let mut bgra = Vec::new();
    let mut nv12 = Vec::new();
    let mut seq = 0u64;
    let mut stats = HostStats::default();
    let mut receptor = ReceptorArquivo::novo();

    // Download (host→viewer): quando o viewer pede um arquivo (`RequestFile`),
    // o diálogo de escolha abre **numa thread à parte** para o laço de captura
    // não parar — assim o operador vê o próprio diálogo pela sessão e escolhe
    // por controle remoto. O caminho escolhido volta por este canal.
    let (tx_arquivo, rx_arquivo) = std::sync::mpsc::channel::<Option<String>>();
    let mut remetente: Option<RemetenteArquivo> = None;
    let mut dialogo_aberto = false;

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
                        // Coordenadas do viewer vêm no espaço da resolução que
                        // ele exibe (a do stream), não a nativa.
                        input::inject(&ev, ow, oh);
                    }
                    stats.input_events += 1;
                }
                Ok(Message::RequestKeyframe) => {
                    enc.request_keyframe();
                    stats.keyframe_requests += 1;
                }
                Ok(Message::FileOffer { name, size }) => receptor.oferta(&name, size),
                Ok(Message::FileChunk { data }) => receptor.pedaco(&data),
                Ok(Message::FileEnd) => receptor.fim(),
                Ok(Message::RequestFile) => {
                    // Um pedido por vez: ignora repetições enquanto o diálogo
                    // está aberto ou um envio está em curso, para não abrir
                    // várias janelas nem truncar a transferência anterior.
                    if !dialogo_aberto && remetente.is_none() {
                        println!("host: viewer pediu um arquivo — abrindo o seletor na tela desta máquina");
                        dialogo_aberto = true;
                        let tx = tx_arquivo.clone();
                        // Sem janela dona (é console): passa None.
                        std::thread::spawn(move || {
                            let _ = tx.send(escolher_arquivo(None));
                        });
                    } else {
                        println!("host: pedido de arquivo ignorado (seletor já aberto ou envio em curso)");
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }

        // ---- resultado do diálogo de download e um passo do envio
        // Fora do laço de mensagens de propósito: precisa rodar em toda volta,
        // inclusive nas que a captura aborta com `continue` (tela ociosa, cega).
        if let Ok(escolha) = rx_arquivo.try_recv() {
            dialogo_aberto = false;
            match escolha {
                Some(caminho) => {
                    println!("host: operador escolheu '{caminho}' — enviando ao viewer");
                    remetente = RemetenteArquivo::abrir(&caminho);
                }
                None => println!("host: nenhum arquivo escolhido para baixar"),
            }
        }
        if let Some(r) = remetente.as_mut()
            && !r.passo(&mut t)
        {
            remetente = None;
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
                if cap.recover().is_err() {
                    // Falhar aqui é o **caso normal** durante o Secure
                    // Desktop: prompt de UAC, tela de bloqueio, Ctrl+Alt+Del.
                    // Medido em 05/08/2026 — acesso negado, 0x80070005.
                    //
                    // Antes havia um `?` nesta linha, e o erro derrubava a
                    // sessão inteira: todo prompt de UAC desconectaria o
                    // viewer. A sessão precisa sobreviver cega e voltar
                    // sozinha quando o desktop retornar.
                    stats.cego += 1;
                    std::thread::sleep(BACKOFF_CEGO);
                }
                // Continua atendendo o transporte mesmo sem imagem: pings e
                // eventos de entrada não podem parar junto.
                t.flush()?;
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

        // Controle de ritmo, com tolerância — e a tolerância não é detalhe.
        //
        // A captura entrega a 60 Hz e o alvo pode ser 60. Uma comparação
        // exata (`elapsed < period`) descartava o quadro que chegasse meio
        // milissegundo adiantado, e o próximo só viria um intervalo inteiro
        // depois: um jitter de 0,5 ms custava 16,7 ms de latência.
        //
        // Medido: o trecho tela→captura marcava 60 ms na pipeline contra
        // 33,6 ms na captura isolada. A diferença era quase toda este
        // batimento.
        if last_encoded.elapsed() + frame_period / 8 < frame_period {
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
            bgra_to_nv12(&bgra, nw as usize, nh as usize, &mut nv12);
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
            if !QUIET.load(Ordering::Relaxed) {
                stats.report(&t);
            }
            next_report += Duration::from_secs(2);
        }
    }
}

/// Teto de um arquivo recebido. Um peer autenticado não deveria ser hostil,
/// mas "não deveria" não é garantia: sem o teto, um peer comprometido encheria
/// o disco da máquina controlada.
const MAX_ARQUIVO: u64 = 2 * 1024 * 1024 * 1024; // 2 GB

/// Recebe um arquivo e o grava em disco. Serve às duas direções: o host o usa
/// para o que o viewer envia, o viewer para o que baixa do host.
///
/// # O cuidado que não é opcional
///
/// O nome vem da rede, e é reduzido ao **nome-base** antes de virar caminho.
/// Aceitar `..\..\Windows\System32\algo.dll` seria deixar o outro lado
/// escrever onde quisesse — a brecha clássica de path traversal. O receptor
/// também não sobrescreve: se já existe um arquivo com o nome, acrescenta um
/// sufixo numérico.
///
/// **Aceita sem perguntar.** Os dois lados se autenticaram por senha, e o
/// operador pediu para dispensar a confirmação na ponta remota. Um produto
/// pode oferecer essa confirmação como opção — anotado, não imposto aqui.
struct ReceptorArquivo {
    arquivo: Option<std::io::BufWriter<std::fs::File>>,
    caminho: std::path::PathBuf,
    esperado: u64,
    recebido: u64,
}

impl ReceptorArquivo {
    fn novo() -> Self {
        Self {
            arquivo: None,
            caminho: std::path::PathBuf::new(),
            esperado: 0,
            recebido: 0,
        }
    }

    fn oferta(&mut self, nome: &str, tamanho: u64) {
        if tamanho > MAX_ARQUIVO {
            eprintln!(
                "arquivo: recusando '{nome}': {tamanho} bytes acima do teto de {MAX_ARQUIVO}"
            );
            return;
        }
        let destino = pasta_de_recebidos();
        let _ = std::fs::create_dir_all(&destino);
        let caminho = caminho_sem_colisao(&destino, nome);

        match std::fs::File::create(&caminho) {
            Ok(f) => {
                println!("arquivo: recebendo '{}' ({} bytes)...", caminho.display(), tamanho);
                self.arquivo = Some(std::io::BufWriter::new(f));
                self.caminho = caminho;
                self.esperado = tamanho;
                self.recebido = 0;
            }
            Err(e) => eprintln!("arquivo: não consegui criar '{}': {e}", caminho.display()),
        }
    }

    fn pedaco(&mut self, dados: &[u8]) {
        use std::io::Write;
        let Some(f) = self.arquivo.as_mut() else { return };
        // Um peer que mande mais do que ofereceu é encerrado aqui, não deixado
        // escrever à vontade.
        if self.recebido + dados.len() as u64 > self.esperado {
            eprintln!("arquivo: transferência excedeu o tamanho ofertado; abortando");
            self.arquivo = None;
            let _ = std::fs::remove_file(&self.caminho);
            return;
        }
        if f.write_all(dados).is_err() {
            eprintln!("arquivo: erro gravando o arquivo; abortando");
            self.arquivo = None;
            return;
        }
        self.recebido += dados.len() as u64;
    }

    fn fim(&mut self) {
        use std::io::Write;
        let Some(mut f) = self.arquivo.take() else { return };
        let _ = f.flush();
        println!(
            "arquivo: ARQUIVO RECEBIDO — '{}' ({} de {} bytes)",
            self.caminho.display(),
            self.recebido,
            self.esperado
        );
    }
}

/// Onde os arquivos recebidos são gravados: `Downloads\PCShare`.
fn pasta_de_recebidos() -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    base.join("Downloads").join("PCShare")
}

/// Reduz o nome ao nome-base e resolve colisão com sufixo numérico.
fn caminho_sem_colisao(dir: &std::path::Path, nome_cru: &str) -> std::path::PathBuf {
    // `file_name` descarta qualquer componente de diretório. Se sobrar algo
    // vazio ou `..`, cai num nome neutro em vez de arriscar.
    let base = std::path::Path::new(nome_cru)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty() && *s != "..")
        .unwrap_or("arquivo-recebido");

    let primeiro = dir.join(base);
    if !primeiro.exists() {
        return primeiro;
    }
    let p = std::path::Path::new(base);
    let tronco = p.file_stem().and_then(|s| s.to_str()).unwrap_or(base);
    let ext = p.extension().and_then(|s| s.to_str());
    for n in 1..10_000 {
        let tentativa = match ext {
            Some(e) => dir.join(format!("{tronco} ({n}).{e}")),
            None => dir.join(format!("{tronco} ({n})")),
        };
        if !tentativa.exists() {
            return tentativa;
        }
    }
    primeiro // desistência improvável; sobrescreve
}

/// Abre o diálogo padrão do Windows para o operador escolher um arquivo.
///
/// Modal: enquanto está aberto, o laço não desenha — o vídeo pausa e volta
/// assim que a escolha é feita. Para escolher um arquivo, é um custo aceitável.
///
/// `owner` é o handle da janela dona (como `isize`, para poder atravessar
/// threads — o `HWND` cru não é `Send`). Com dono, o diálogo devolve o foco à
/// janela ao fechar; sem, o viewer para de receber teclas depois. No host não
/// há janela: passa-se `None`.
fn escolher_arquivo(owner: Option<isize>) -> Option<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Controls::Dialogs::{
        CommDlgExtendedError, GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_HIDEREADONLY,
        OFN_NOCHANGEDIR, OPENFILENAMEW,
    };
    use windows::core::PWSTR;

    let dono = owner
        .map(|h| HWND(h as *mut std::ffi::c_void))
        .unwrap_or_default();

    // Buffer que o diálogo preenche com o caminho escolhido. 32 KB comporta
    // qualquer caminho do Windows com folga.
    let mut buf = vec![0u16; 32 * 1024];
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: dono,
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        // OFN_NOCHANGEDIR: não deixa o diálogo trocar o diretório atual do
        // processo, que confundiria caminhos relativos usados depois.
        Flags: OFN_FILEMUSTEXIST | OFN_HIDEREADONLY | OFN_NOCHANGEDIR,
        ..Default::default()
    };

    let escolheu = unsafe { GetOpenFileNameW(&mut ofn).as_bool() };
    if !escolheu {
        // Distingue cancelamento de falha real: zero é cancelamento; qualquer
        // outro código é erro do próprio diálogo, e sem isto ele some calado.
        let err = unsafe { CommDlgExtendedError() };
        if err.0 == 0 {
            println!("seletor: operador cancelou");
        } else {
            eprintln!("seletor: GetOpenFileNameW falhou (CDERR=0x{:04X})", err.0);
        }
        return None;
    }
    let fim = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..fim]))
}

/// Tamanho de cada pedaço enviado. Pequeno de propósito: o arquivo trafega
/// pelo mesmo canal de controle que a entrada, e um pedaço por volta do laço
/// deixa os cliques passarem entre eles, em vez de a transferência travar o
/// controle. É a mesma lógica de intercalar que o resto do laço já usa.
const PEDACO_ARQUIVO: usize = 64 * 1024;

/// Envia um arquivo local ao host, um pedaço por vez.
///
/// O ritmo — um passo por volta do laço do viewer — é o ponto: a entrada
/// continua fluindo enquanto o arquivo sobe. Uma transferência que congelasse
/// o mouse seria pior que uma lenta.
struct RemetenteArquivo {
    leitor: std::io::BufReader<std::fs::File>,
    nome: String,
    restante: u64,
    ofertou: bool,
}

impl RemetenteArquivo {
    fn abrir(caminho: &str) -> Option<Self> {
        let arq = std::fs::File::open(caminho)
            .inspect_err(|e| eprintln!("arquivo: não consegui abrir '{caminho}': {e}"))
            .ok()?;
        let tamanho = arq.metadata().ok()?.len();
        let nome = std::path::Path::new(caminho)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("arquivo")
            .to_string();
        println!("arquivo: enviando '{nome}' ({tamanho} bytes)...");
        Some(Self {
            leitor: std::io::BufReader::new(arq),
            nome,
            restante: tamanho,
            ofertou: false,
        })
    }

    /// Faz um passo: a oferta na primeira vez, depois um pedaço por chamada.
    /// Devolve `false` quando não há mais nada a enviar.
    fn passo<T: Transport>(&mut self, t: &mut T) -> bool {
        use std::io::Read;
        if !self.ofertou {
            let m = Message::FileOffer {
                name: self.nome.clone(),
                size: self.restante,
            };
            if t.send(m.channel(), &m.encode()).is_err() {
                return false;
            }
            self.ofertou = true;
            return true;
        }
        if self.restante == 0 {
            let m = Message::FileEnd;
            let _ = t.send(m.channel(), &m.encode());
            println!("arquivo: '{}' enviado.", self.nome);
            return false;
        }
        let n = (self.restante as usize).min(PEDACO_ARQUIVO);
        let mut buf = vec![0u8; n];
        match self.leitor.read(&mut buf) {
            Ok(0) => {
                // Fim inesperado do arquivo: encerra com o que foi.
                self.restante = 0;
                true
            }
            Ok(lidos) => {
                buf.truncate(lidos);
                let m = Message::FileChunk { data: buf };
                if t.send(m.channel(), &m.encode()).is_err() {
                    return false;
                }
                self.restante -= lidos as u64;
                true
            }
            Err(e) => {
                eprintln!("arquivo: erro lendo o arquivo: {e}");
                false
            }
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
    /// Iterações em que a captura estava cega e `recover` não deu conta —
    /// tipicamente com o Secure Desktop na frente.
    cego: u64,
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
                format!(
                    " | captura perdida {}{}",
                    self.access_lost,
                    if self.cego > 0 {
                        // Sinal de Secure Desktop: a captura se perdeu e não
                        // pôde ser recriada. A sessão segue viva, sem imagem.
                        format!(", cego {}x", self.cego)
                    } else {
                        String::new()
                    }
                )
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

fn run_viewer<T: Transport>(mut t: T, titulo: &str) {
    // `--headless <segundos>`: sem janela. Existe para medir a banda de uma
    // tela realmente parada — com janela, ela exibe o que é capturado e cada
    // quadro gera o próximo (realimentação). No headless não pedimos redução de
    // resolução: a medição quer o tamanho nativo.
    let headless: Option<u64> = {
        let a: Vec<String> = std::env::args().collect();
        a.iter()
            .position(|x| x == "--headless")
            .map(|i| a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(20))
    };

    // Auto-resolução: pede ao host para já mandar no tamanho deste monitor. O
    // host reduz a captura antes de codificar, então a imagem chega quase 1:1
    // em vez de encolhida na exibição (o que borra). O host nunca amplia.
    if headless.is_none() {
        let (rw, rh) = resolucao_desejada();
        let m = Message::SetResolution {
            width: rw,
            height: rh,
        };
        let _ = t.send(m.channel(), &m.encode());
    }

    // Nada pode ser desenhado antes de saber as dimensões (já reduzidas) do host.
    let Some((w, h)) = wait_hello(&mut t) else {
        eprintln!("viewer: conexão encerrada antes do Hello");
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

    // Sem keyframe periódico, este pedido é a única coisa que dá ao
    // decodificador um ponto de partida. Sem ele, tela preta para sempre.
    let req = Message::RequestKeyframe;
    if t.send(req.channel(), &req.encode()).is_err() {
        eprintln!("viewer: não foi possível pedir o keyframe inicial");
        return;
    }

    let mut win = match headless {
        Some(_) => None,
        None => {
            // Janela do tamanho do stream + a barra, sem passar do monitor —
            // assim o vídeo aparece perto de 1:1.
            let (mw, mh) = tamanho_monitor();
            let cw = (w as i32).min(mw as i32 - 60).max(320);
            let ch = (h as i32 + 40).min(mh as i32 - 120).max(240);
            match window::Window::new(titulo, cw, ch) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("viewer: não foi possível criar a janela: {e}");
                    return;
                }
            }
        }
    };
    let deadline = headless.map(|s| Instant::now() + Duration::from_secs(s));

    // `--dump-frame <arquivo.bmp>`: grava o primeiro quadro decodificado e sai.
    let dump_to: Option<String> = {
        let a: Vec<String> = std::env::args().collect();
        a.iter()
            .position(|x| x == "--dump-frame")
            .and_then(|i| a.get(i + 1).cloned())
    };

    // Estado das modificadoras, para reconhecer Ctrl+Alt+End. O viewer recebe
    // teclas soltas, não combinações — quem junta é quem escuta.
    let mut ctrl_pressionado = false;
    let mut alt_pressionado = false;

    // `--send-file <caminho>`: envia um arquivo ao host assim que a sessão sobe.
    // É a via testável do v1; o atalho com seletor de arquivo vem depois.
    let mut remetente = arg_value("--send-file").and_then(|p| RemetenteArquivo::abrir(&p));

    // Recebe arquivos baixados do host (resposta a um `RequestFile`), gravados
    // em `Downloads\PCShare` desta máquina.
    let mut receptor = ReceptorArquivo::novo();

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

        // ---- um passo do envio de arquivo, se houver um em curso
        if let Some(r) = remetente.as_mut()
            && !r.passo(&mut t)
        {
            remetente = None;
        }

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
        let (cw, ch) = win.as_ref().map_or((0, 0), |w| w.video_size());
        for raw in win.as_mut().map(|w| w.take_input()).unwrap_or_default() {
            // Botões da barra de ferramentas: mesmas ações dos atalhos, mas por
            // clique — e sem depender de combinação que outro programa possa
            // sequestrar (foi o que o AnyDesk fez com o Ctrl+Alt+F/G).
            if let window::RawEvent::Command(cmd) = raw {
                match cmd {
                    window::SessionCmd::SendFile => {
                        let dono = win.as_ref().map(|w| w.hwnd().0 as isize);
                        if let Some(caminho) = escolher_arquivo(dono) {
                            remetente = RemetenteArquivo::abrir(&caminho);
                        }
                    }
                    window::SessionCmd::DownloadFile => {
                        let m = Message::RequestFile;
                        if t.send(m.channel(), &m.encode()).is_err() {
                            return;
                        }
                        println!("viewer: pedido de download enviado ao host (barra)");
                    }
                    window::SessionCmd::CtrlAltDel => {
                        let m = Message::RequestSAS;
                        if t.send(m.channel(), &m.encode()).is_err() {
                            return;
                        }
                        println!("viewer: Ctrl+Alt+Del enviado ao host (barra)");
                    }
                }
                continue;
            }
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
                // Já tratado acima com `continue`; presente só para o match
                // continuar exaustivo.
                window::RawEvent::Command(_) => continue,
                window::RawEvent::Key { scancode, down } => {
                    // Ctrl+Alt+End vira Ctrl+Alt+Del na máquina remota.
                    //
                    // O atalho não pode ser o próprio Ctrl+Alt+Del: a máquina
                    // **local** o intercepta antes de qualquer programa ver —
                    // é exatamente essa interceptação que dá à sequência o
                    // valor de garantir uma tela de senha autêntica. O RDP usa
                    // Ctrl+Alt+End pelo mesmo motivo, e seguir a convenção
                    // poupa o operador de aprender mais uma.
                    match scancode & 0xFF {
                        SCAN_CTRL => ctrl_pressionado = down,
                        SCAN_ALT => alt_pressionado = down,
                        _ => {}
                    }
                    if down && scancode & 0xFF == SCAN_END && ctrl_pressionado && alt_pressionado {
                        let m = Message::RequestSAS;
                        if t.send(m.channel(), &m.encode()).is_err() {
                            return;
                        }
                        println!("viewer: Ctrl+Alt+Del enviado ao host");
                        // Não repassa as teclas: o host receberia um End solto
                        // no meio da sequência.
                        continue;
                    }
                    // Ctrl+Alt+F: escolher um arquivo e enviá-lo ao host.
                    if down && scancode & 0xFF == SCAN_F && ctrl_pressionado && alt_pressionado {
                        // Solta Ctrl e Alt no host antes de abrir o diálogo: ele
                        // recebeu os "down" e, sem os "up" (que vão para o
                        // diálogo, não para cá), ficaria com as teclas grudadas.
                        for solto in [SCAN_CTRL, SCAN_ALT] {
                            let m = Message::Input(InputEvent::Key { scancode: solto, down: false });
                            let _ = t.send(m.channel(), &m.encode());
                        }
                        ctrl_pressionado = false;
                        alt_pressionado = false;
                        // O diálogo é modal: o vídeo pausa enquanto ele está
                        // aberto e volta assim que o arquivo é escolhido. A
                        // janela do viewer é a dona, para o foco (e os atalhos)
                        // voltarem a ela ao fechar.
                        let dono = win.as_ref().map(|w| w.hwnd().0 as isize);
                        if let Some(caminho) = escolher_arquivo(dono) {
                            remetente = RemetenteArquivo::abrir(&caminho);
                        }
                        continue;
                    }
                    // Ctrl+Alt+G: pedir ao host um arquivo para baixar. O
                    // diálogo abre lá, e o operador o opera por controle remoto.
                    if down && scancode & 0xFF == SCAN_G && ctrl_pressionado && alt_pressionado {
                        // Solta Ctrl e Alt no host, como no Ctrl+Alt+F: senão as
                        // teclas ficam grudadas lá enquanto o operador digita o
                        // nome do arquivo no diálogo remoto.
                        for solto in [SCAN_CTRL, SCAN_ALT] {
                            let m = Message::Input(InputEvent::Key { scancode: solto, down: false });
                            let _ = t.send(m.channel(), &m.encode());
                        }
                        ctrl_pressionado = false;
                        alt_pressionado = false;
                        let m = Message::RequestFile;
                        if t.send(m.channel(), &m.encode()).is_err() {
                            return;
                        }
                        println!("viewer: pedido de download enviado ao host (Ctrl+Alt+G)");
                        continue;
                    }
                    InputEvent::Key { scancode, down }
                }
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
                    // Download vindo do host, gravado localmente.
                    Ok(Message::FileOffer { name, size }) => receptor.oferta(&name, size),
                    Ok(Message::FileChunk { data }) => receptor.pedaco(&data),
                    Ok(Message::FileEnd) => receptor.fim(),
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
                // fps() provam que a pipeline *corre*; só olhar um quadro prova
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


