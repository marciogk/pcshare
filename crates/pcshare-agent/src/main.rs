//! Agente do PCShare — o processo que enxerga a tela.
//!
//! Existe por causa da decisão D5. Um serviço do Windows roda na sessão 0, que
//! não tem interface gráfica desde o Vista: ele não vê tela nenhuma. Quem vê é
//! o agente, lançado pelo serviço **dentro** da sessão do usuário e amarrado a
//! um desktop específico.
//!
//! A amarração é o ponto. Um processo do Windows nasce preso ao desktop
//! indicado em `STARTUPINFO.lpDesktop` e não troca depois. É por isso que a
//! resposta ao Secure Desktop é lançar **outro** agente, não reconfigurar o que
//! já existe.
//!
//! Medido em 05/08/2026: um processo comum falha 100% das vezes ao tentar
//! duplicar o `Winlogon` — `0x80070005`, acesso negado. Ver `docs/FASE-3.md`.
//!
//! # O que ele faz agora
//!
//! Captura, converte na GPU, codifica em H.264 e entrega os quadros ao
//! supervisor pelo canal; no sentido inverso, recebe entrada e injeta.
//!
//! O que ele **não** faz é rede. A sessão QUIC e o handshake Noise vivem no
//! supervisor, porque toda troca de desktop mata e recria agentes: se a sessão
//! morasse aqui, cada prompt de UAC derrubaria o viewer e desfaria a Fase 3.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use pcshare_capture::DesktopCapture;
use pcshare_capture::canal::{self, Mensagem};
use pcshare_capture::desktop::{self, InputDesktop};
use pcshare_capture::linha_do_tempo::{agora_us, registrar as linha_do_tempo};
use pcshare_codec::H264Encoder;
use pcshare_core::Message;
use pcshare_gpu::GpuContext;

/// Espera máxima por um quadro novo. É espera bloqueante em evento (decisão
/// D8), mas limitada para que o laço também atenda o canal.
const CAPTURA_TIMEOUT_MS: u32 = 8;
/// Pausa entre tentativas de recuperar a captura enquanto ela está cega.
///
/// Exceção deliberada à decisão D8, que proíbe esperar por relógio: aqui não
/// há evento algum para aguardar — o Windows não avisa quando sai do Secure
/// Desktop. Sem esta pausa o laço tenta ~2000 vezes por segundo e queima um
/// núcleo inteiro durante cada prompt de UAC. Medido.
///
/// É também o que domina os 91 ms de cegueira que restam. Ver `docs/FASE-3.md`.
const BACKOFF_CEGO: Duration = Duration::from_millis(50);
const BITRATE: u32 = 8_000_000;

fn fps() -> u32 {
    std::env::var("PCSHARE_FPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| (1..=120).contains(v))
        .unwrap_or(60)
}

/// Registra em arquivo, e no console **se** houver um.
///
/// O agente é lançado pelo supervisor sem console. Nesse contexto `println!`
/// entra em pânico ao falhar a escrita, e o processo morre no primeiro relato
/// — foi exatamente o que aconteceu: o supervisor relançava o agente a cada
/// 100 ms sem perceber que ele nascia e morria.
fn log(rotulo: &str, msg: &str) {
    let linha = format!(
        "[{}] [{rotulo}] {msg}\n",
        pcshare_capture::linha_do_tempo::hora_local()
    );
    // Ignora falha de propósito: sem console não há para onde escrever, e
    // isso não é motivo para derrubar o agente.
    let _ = std::io::stdout().write_all(linha.as_bytes());
    let _ = std::io::stdout().flush();

    let caminho = caminho_do_log(rotulo);
    if let Some(dir) = caminho.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&caminho)
    {
        let _ = f.write_all(linha.as_bytes());
    }
}

/// Nome que o Windows usa, para relatar ao supervisor.
fn nome_do_desktop(d: &InputDesktop) -> &str {
    match d {
        InputDesktop::Default => "Default",
        // `Denied` é o que se vê quando o Secure Desktop está à frente.
        InputDesktop::Winlogon | InputDesktop::Denied => "Winlogon",
        InputDesktop::ScreenSaver => "Screen-saver",
        InputDesktop::Other(n) => n,
    }
}

fn caminho_do_log(rotulo: &str) -> PathBuf {
    // `ProgramData` porque o agente pode nascer de um serviço SYSTEM, que não
    // tem perfil de usuário.
    let seguro: String = rotulo
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join(format!("agente-{seguro}.log"))
}

/// Luminância média, amostrando de 64 em 64 pixels.
///
/// Não precisa ser exata: a pergunta é "isto é preto?", não "qual o brilho".
fn luminancia(bgra: &[u8]) -> f64 {
    let mut soma = 0f64;
    let mut n = 0u32;
    for p in bgra.chunks_exact(4).step_by(64) {
        soma += 0.114 * p[0] as f64 + 0.587 * p[1] as f64 + 0.299 * p[2] as f64;
        n += 1;
    }
    if n == 0 { -1.0 } else { soma / n as f64 }
}

/// Move o log anterior para `-anterior.log` e começa um vazio.
///
/// Mesma política do supervisor, pelo mesmo motivo: ler o topo de um arquivo é
/// o reflexo natural de qualquer pessoa, e um arquivo que acumula execuções
/// premia esse reflexo com informação de semanas atrás.
fn rotacionar_log(rotulo: &str) {
    let atual = caminho_do_log(rotulo);
    if !atual.exists() {
        return;
    }
    let Some(nome) = atual.file_stem().and_then(|n| n.to_str()) else {
        return;
    };
    let anterior = atual.with_file_name(format!("{nome}-anterior.log"));
    // Falhar aqui não impede nada: no pior caso o arquivo continua crescendo,
    // que é como era antes.
    let _ = std::fs::remove_file(&anterior);
    let _ = std::fs::rename(&atual, &anterior);
}

fn main() {
    // Primeiríssima instrução: este carimbo mede o custo de criação do
    // processo, e qualquer coisa antes dele o contamina. A versão anterior o
    // escrevia depois de quatro gravações em disco, e assim media o log em
    // vez do nascimento.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let vinculo_cru = args
        .iter()
        .position(|a| a == "--desktop")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "desconhecido".into());
    let instrumento_ok = linha_do_tempo(&vinculo_cru, "inicio");
    let rotulo = args
        .iter()
        .position(|a| a == "--rotulo")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "sem-rótulo".into());
    let segundos: u64 = args
        .iter()
        .position(|a| a == "--segundos")
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);

    // A qual desktop este agente foi amarrado no nascimento. É contra isto
    // que se compara o desktop de entrada para detectar a troca.
    let vinculo = args
        .iter()
        .position(|a| a == "--desktop")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| rotulo.clone());
    let codigo_do_vinculo = match vinculo.as_str() {
        "Default" => desktop::codigo::DEFAULT,
        "Winlogon" => desktop::codigo::WINLOGON,
        "Screen-saver" => desktop::codigo::SCREENSAVER,
        _ => desktop::codigo::OUTRO,
    };

    let (sessao, console) = desktop::sessions();

    // Guarda o log da execução anterior e começa um limpo.
    //
    // Sem isto o arquivo cresce sem fim, e os desta máquina chegaram a 8,8 MB
    // em duas semanas. O produto roda no computador de outra pessoa, onde
    // encher o disco é defeito, não detalhe.
    rotacionar_log(&rotulo);

    log(&rotulo, "=== início ===");
    if !instrumento_ok {
        // Sem este aviso, o agente some da medição sem deixar rastro — e o
        // analisador passa a casar eventos de agentes errados.
        log(
            &rotulo,
            "AVISO: não consegui escrever na linha do tempo; a medição ficará incompleta",
        );
    }
    log(&rotulo, &format!("pid {}", std::process::id()));
    log(&rotulo, &format!("sessão {sessao} (console: {console})"));
    log(
        &rotulo,
        &format!("desktop de entrada: {}", desktop::input_desktop().label()),
    );

    let canal = canal::Cliente::conectar(canal::CANO_PADRAO).map(Arc::new);
    if canal.is_none() {
        log(&rotulo, "AVISO: canal indisponível — sem supervisor, sem vídeo");
    }

    match rodar(&rotulo, &vinculo, codigo_do_vinculo, segundos, canal.as_ref()) {
        Ok(quadros) => log(&rotulo, &format!("encerrando após {quadros} quadros")),
        Err(e) => log(&rotulo, &format!("encerrando por erro: {e}")),
    }

    log(&rotulo, "=== fim ===");
}

/// O laço de trabalho. Separado de `main` só para poder usar `?`.
fn rodar(
    rotulo: &str,
    vinculo: &str,
    codigo_do_vinculo: u32,
    segundos: u64,
    canal: Option<&Arc<canal::Cliente>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let gpu = GpuContext::new()?;

    // A pergunta que o passo 2 respondeu: a captura funciona a partir daqui?
    // No desktop do usuário, sim. No Winlogon, só se o agente tiver sido
    // lançado pelo serviço com privilégio de SYSTEM.
    let mut cap = match DesktopCapture::with_gpu(gpu.clone(), 0) {
        Ok(c) => {
            log(rotulo, &format!("captura ABRIU {}x{}", c.width(), c.height()));
            // Se o primeiro registro passou, os seguintes passam: mesmo
            // arquivo, mesmo dono.
            let _ = linha_do_tempo(vinculo, "captura-abriu");
            c
        }
        // **Nascer cego é condição normal, não falha.**
        //
        // O agente do `Winlogon` é criado antes do primeiro prompt de UAC,
        // quando o desktop de entrada ainda é o do usuário — e duplicar o
        // `Winlogon` dali dá acesso negado. Encerrar aqui é o que transformou
        // o pré-lançamento num laço de seis processos por segundo em
        // 17/08/2026.
        //
        // Ele segue vivo, monta o encoder com as dimensões que a saída de
        // vídeo informa, e o laço de recuperação o liga quando o desktop
        // ficar alcançável. É a mesma coisa que ele já sabia fazer ao ficar
        // cego no meio da vida; só o nascimento era assimétrico.
        Err(e) => match DesktopCapture::cega(gpu.clone(), 0) {
            Ok(c) => {
                log(
                    rotulo,
                    &format!(
                        "captura indisponível ({e}); nascendo cego em {}x{} e aguardando o desktop",
                        c.width(),
                        c.height()
                    ),
                );
                let _ = linha_do_tempo(vinculo, "nasceu-cego");
                c
            }
            // Sem sequer conseguir descrever a saída de vídeo não há o que
            // fazer: não é o desktop que está inacessível, é a GPU.
            Err(e2) => {
                log(rotulo, &format!("captura FALHOU: {e} / saída de vídeo: {e2}"));
                let _ = linha_do_tempo(vinculo, "captura-falhou");
                return Err(e2.into());
            }
        },
    };
    let (w, h) = (cap.width(), cap.height());

    let mut enc = H264Encoder::with_gpu(&gpu, w, h, fps(), BITRATE)?;
    log(
        rotulo,
        &format!("encoder [{}] ({})", enc.name(), enc.tuning().join(", ")),
    );

    // O conversor na GPU só entra se o encoder aceitar texturas. Encoder de
    // software e driver antigo caem no caminho por memória de sistema, e é
    // melhor perder desempenho do que não funcionar.
    let mut conv = match enc.is_gpu_direct() {
        true => Some(pcshare_gpu::Nv12Converter::new(gpu.clone(), w, h)?),
        false => None,
    };
    log(
        rotulo,
        &format!(
            "caminho de pixels = {}",
            if conv.is_some() {
                "GPU direto (sem cópia)"
            } else {
                "memória de sistema (leitura da GPU + conversão na CPU)"
            }
        ),
    );

    // O supervisor não conhece a resolução — está na sessão 0, cego. Sem isto
    // ele não tem como montar o `Message::Hello`, e o viewer não desenha nada
    // antes dele.
    if let Some(c) = canal
        && !c.enviar(&Mensagem::Pronto {
            largura: w,
            altura: h,
        })
    {
        log(rotulo, "AVISO: não consegui anunciar as dimensões");
    }

    let pedido_de_keyframe = Arc::new(AtomicBool::new(false));
    // Começa desligado: quem manda transmitir é o supervisor, quando há um
    // viewer. Transmitir sem público enche o cano e trava o agente — ver
    // `Mensagem::Capturar`.
    let transmitindo = Arc::new(AtomicBool::new(false));
    if let Some(c) = canal {
        atender_o_supervisor(
            c.clone(),
            pedido_de_keyframe.clone(),
            transmitindo.clone(),
            w,
            h,
            rotulo.to_string(),
        );
    }

    // `--segundos 0` roda até ser encerrado: é como o supervisor usa o agente,
    // já que quem decide o fim da vida dele é a troca de desktop.
    let perpetuo = segundos == 0;
    let ate = Instant::now() + Duration::from_secs(segundos.clamp(1, 3600));

    let mut quadros = 0u64;
    let mut seq = 0u64;
    let mut perdas = 0u32;
    let mut ultimo_quadro_us: Option<u128> = None;
    let mut capturando = false;
    // Uma linha de motivo por episódio de cegueira, e não por tentativa.
    let mut recusa_registrada = false;
    // Mede o **primeiro quadro depois de voltar a enxergar**.
    //
    // Existe por causa de um preto que apareceu no viewer durante um bloqueio
    // de tela em 19/08/2026, com a captura provadamente entregando imagem
    // (luminância 110,3, medida com `pcshare-capture --bin bloqueio`). Se a
    // captura está certa e o viewer vê preto, o problema está entre os dois —
    // e o caminho GPU direto, que o diagnóstico não exercita, é o suspeito.
    //
    // Duas medidas resolvem a dúvida sem opinião: a luminância do que foi
    // capturado, e o tamanho do primeiro pacote codificado. Imagem real com
    // pacote minúsculo significa que o encode perdeu o conteúdo.
    //
    // Uma vez por episódio, fora do caminho quente: o readback custa alguns
    // milissegundos e não pode entrar no laço de 60 quadros por segundo.
    let mut medir_este = false;
    let mut aguardando_pacote = false;
    let mut ultimo_relatado = codigo_do_vinculo;
    // Batimento raro de propósito. A cada dois segundos, que era o ritmo do
    // desenvolvimento da Fase 3, dá 43 mil linhas por dia **por agente**. As
    // transições que importam — trocar de desktop, começar a transmitir, ficar
    // cego — são registradas na hora; ao batimento resta só provar que o
    // processo continua vivo.
    const BATIMENTO: Duration = Duration::from_secs(60);
    let mut proximo_relato = Instant::now() + BATIMENTO;

    let periodo = Duration::from_micros(1_000_000 / fps() as u64);
    let mut ultimo_encode = Instant::now() - periodo;
    let mut bgra = Vec::new();
    let mut nv12 = Vec::new();

    while perpetuo || Instant::now() < ate {
        // ---- o desktop de entrada mudou?
        //
        // O agente está preso a um desktop e não pode trocar. Quando o desktop
        // de entrada vira outro, ele deixa de servir: fica cego, acumulando
        // perdas. Quem percebe a troca é ele — o supervisor, na sessão 0, não
        // enxerga desktop nenhum.
        //
        // Relata **sem morrer**. Antes o agente encerrava e o supervisor criava
        // outro no caminho crítico; medido, recriar com token de usuário custa
        // cerca de 700 ms, 90% da cegueira ao voltar do UAC. Vivo, ele fica
        // cego por alguns segundos e volta sozinho quando o desktop retorna.
        let agora = desktop::input_desktop();
        let codigo_agora = desktop::codigo::de(&agora);
        if perpetuo && codigo_agora != ultimo_relatado {
            ultimo_relatado = codigo_agora;
            let nome = nome_do_desktop(&agora);
            let relatou = canal.is_some_and(|c| c.enviar(&Mensagem::Desktop(nome.to_string())));
            if relatou {
                log(rotulo, &format!("relatei ao supervisor: desktop '{nome}'"));
            } else {
                // Sem canal, volta ao comportamento antigo: encerrar com o
                // destino no código de saída. Mais lento, mas o supervisor
                // continua sabendo para onde ir.
                log(
                    rotulo,
                    "canal indisponível — encerrando para relatar pelo código de saída",
                );
                if let Some(us) = ultimo_quadro_us {
                    let _ = linha_do_tempo(vinculo, &format!("cego-desde {us}"));
                }
                std::process::exit(codigo_agora as i32);
            }
        }

        if perpetuo && Instant::now() >= proximo_relato {
            log(
                rotulo,
                &format!("vivo em '{}' | {quadros} quadros | {perdas} perdas", agora.label()),
            );
            proximo_relato += BATIMENTO;
        }

        if pedido_de_keyframe.swap(false, Ordering::Relaxed) {
            enc.request_keyframe();
        }

        // ---- captura
        let quadro = match cap.next_frame(CAPTURA_TIMEOUT_MS) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(_) => {
                perdas += 1;
                if capturando {
                    capturando = false;
                    // Abre a janela de cegueira no instante do último quadro
                    // entregue, que é quando a tela remota congelou de fato.
                    if let Some(us) = ultimo_quadro_us {
                        let _ = linha_do_tempo(vinculo, &format!("cego-desde {us}"));
                    }
                }
                // Registra **por que** a captura foi recusada, uma vez por
                // episódio de cegueira.
                //
                // Antes isto era `let _ = cap.recover()`, e o motivo se
                // perdia. Quando o teste de bloqueio mostrou que o agente do
                // `Winlogon` nunca consegue duplicar durante uma tela
                // bloqueada, não havia como saber se a recusa era a mesma do
                // prompt de UAC ou outra coisa — e a diferença muda o que se
                // deve construir.
                //
                // Uma vez por episódio, não por tentativa: são 20 por segundo,
                // e um log por tentativa afogaria o arquivo.
                if let Err(motivo) = cap.recover()
                    && !recusa_registrada
                {
                    recusa_registrada = true;
                    log(rotulo, &format!("captura recusada: {motivo}"));
                }
                std::thread::sleep(BACKOFF_CEGO);
                continue;
            }
        };

        ultimo_quadro_us = Some(agora_us());
        // Marca a **transição** para capturando, não o primeiro quadro da vida
        // do processo: com agentes persistentes, quem fecha a janela de
        // cegueira costuma ser um agente que retoma, não um que nasce.
        if !capturando {
            capturando = true;
            recusa_registrada = false;
            // Uma amostra por episódio: o que a captura entrega, e o que o
            // encoder produz a partir dela. Ver `medir_este`.
            medir_este = true;
            let _ = linha_do_tempo(vinculo, "capturando");
        }

        // Sem viewer, para por aqui. O `next_frame` acima continua sendo feito
        // de propósito: é ele que diz se o agente enxerga ou está cego, e a
        // medição de cegueira da Fase 3 depende dessa transição. O que se
        // poupa é o encode, que é o caro.
        if !transmitindo.load(Ordering::Relaxed) {
            // Desarma a medição junto. Sem isto a flag sobrevive à espera e
            // dispara no primeiro quadro da sessão, muitos minutos depois do
            // episódio que a armou — foi o que aconteceu em 08/09/2026: a
            // linha dizia "primeiro quadro após cegueira" e media a área de
            // trabalho no instante em que o viewer conectou.
            //
            // Sem viewer não há encode, então não há o que medir de qualquer
            // forma; o que não pode é medir e rotular errado.
            medir_este = false;
            continue;
        }

        // Nada mudou de fato: nem pixels novos, nem regiões deslocadas.
        //
        // O compositor entrega quadros a 60 Hz mesmo com o desktop ocioso, e a
        // etapa 2 mediu que numa tela imóvel o encoder gasta banda refinando
        // qualidade sozinho. A resposta não é ajustar o encoder — é não
        // chamá-lo.
        if !quadro.has_changes() {
            continue;
        }

        // Controle de ritmo, com tolerância — e a tolerância não é detalhe.
        //
        // A captura entrega a 60 Hz e o alvo pode ser 60. Uma comparação exata
        // descartava o quadro que chegasse meio milissegundo adiantado, e o
        // próximo só viria um intervalo inteiro depois: um jitter de 0,5 ms
        // custava 16,7 ms de latência. Medido.
        if ultimo_encode.elapsed() + periodo / 8 < periodo {
            continue;
        }
        ultimo_encode = Instant::now();

        // Carimbo do instante da **captura**, em relógio de parede. Precisa ser
        // de parede, e não relativo ao início do processo: ele atravessa para
        // o supervisor, que é outro processo, e um `Instant` local viraria
        // lixo do outro lado. Já produziu −2073 ms numa medição real.
        let capturado_us = agora_us() as i64;

        if let Some(conv) = conv.as_mut() {
            // Caminho sem cópia: a textura nunca sai da memória de vídeo.
            let Some(src) = cap.current_texture() else {
                continue;
            };
            enc.submit_texture(conv.convert(src)?)?;
        } else {
            if !cap.copy_bgra(&mut bgra)? {
                continue;
            }
            pcshare_codec::convert::bgra_to_nv12(&bgra, w as usize, h as usize, &mut nv12);
            enc.submit(&nv12)?;
        }

        if medir_este {
            medir_este = false;
            let luz = match cap.copy_bgra(&mut bgra) {
                Ok(true) => luminancia(&bgra),
                _ => -1.0,
            };
            log(
                rotulo,
                &format!("primeiro quadro após cegueira: luminância {luz:.1}"),
            );
            aguardando_pacote = true;
        }

        for p in enc.take_packets() {
            if aguardando_pacote {
                aguardando_pacote = false;
                log(
                    rotulo,
                    &format!(
                        "primeiro pacote após cegueira: {} bytes{}",
                        p.data.len(),
                        if p.is_keyframe { " (chave)" } else { "" }
                    ),
                );
            }
            let m = Mensagem::Quadro {
                seq,
                tempo_us: capturado_us,
                chave: p.is_keyframe,
                dados: p.data,
            };
            // Se o canal caiu, o supervisor morreu ou nos descartou. Continuar
            // codificando para ninguém só queima CPU na máquina do cliente.
            if !canal.is_some_and(|c| c.enviar(&m)) {
                log(rotulo, "canal fechou — encerrando");
                return Ok(quadros);
            }
            seq += 1;
            quadros += 1;
        }
    }

    log(rotulo, &format!("perdas de sessão: {perdas}"));
    Ok(quadros)
}

/// Thread que escuta o supervisor: entrada para injetar, pedidos de keyframe.
///
/// Precisa ser thread separada porque `receber` bloqueia, e o laço de captura
/// não pode parar para esperar entrada que talvez nunca venha. As duas usam o
/// mesmo descritor de cano ao mesmo tempo — é o que a E/S sobreposta do canal
/// existe para permitir.
fn atender_o_supervisor(
    cliente: Arc<canal::Cliente>,
    pedido_de_keyframe: Arc<AtomicBool>,
    transmitindo: Arc<AtomicBool>,
    w: u32,
    h: u32,
    rotulo: String,
) {
    let _ = std::thread::Builder::new()
        .name("pcshare-agente-entrada".into())
        .spawn(move || {
            while let Some(m) = cliente.receber() {
                match m {
                    Mensagem::Entrada(bytes) => match Message::decode(&bytes) {
                        Ok(Message::Input(ev)) => pcshare_input::inject(&ev, w, h),
                        _ => log(&rotulo, "entrada malformada, ignorada"),
                    },
                    Mensagem::PedirKeyframe => {
                        pedido_de_keyframe.store(true, Ordering::Relaxed);
                    }
                    Mensagem::Capturar(ligado) => {
                        // Só age na **transição**, e a razão é concreta: o
                        // supervisor manda `Capturar(true)` ao receber um
                        // `Pronto`, e reanunciar `Pronto` a cada `Capturar`
                        // fecha um laço de realimentação entre os dois
                        // processos.
                        //
                        // Medido em 18/08/2026, antes desta guarda: 176.187
                        // ordens de transmitir numa sessão de dois minutos, e
                        // um pedido de keyframe em cada volta. Como keyframe é
                        // ordens de grandeza maior que um quadro normal, o link
                        // saturou e a contrapressão passou a descartar mais da
                        // metade dos quadros — 4094 descartados contra 3989
                        // repassados.
                        //
                        // Comparar antes de agir é o que torna a mensagem
                        // idempotente, e uma mensagem idempotente é a única
                        // que pode ser reenviada sem consequência.
                        if transmitindo.swap(ligado, Ordering::Relaxed) == ligado {
                            continue;
                        }
                        if ligado {
                            // Reanuncia as dimensões. O anúncio do nascimento
                            // costuma ser bem anterior a existir viewer, e sem
                            // ele o supervisor não monta o `Hello` — e descarta
                            // todo o vídeo em silêncio.
                            if !cliente.enviar(&Mensagem::Pronto {
                                largura: w,
                                altura: h,
                            }) {
                                log(&rotulo, "AVISO: falhei ao reanunciar as dimensões");
                            }
                            // Um viewer que chega precisa de quadro-chave para
                            // ter imagem; sem isto esperaria o próximo
                            // espontâneo.
                            pedido_de_keyframe.store(true, Ordering::Relaxed);
                        }
                        log(&rotulo, if ligado { "transmitindo" } else { "em espera" });
                    }
                    // As demais são do sentido oposto; se chegarem aqui é
                    // defeito, não condição normal.
                    outra => log(&rotulo, &format!("mensagem inesperada: {outra:?}")),
                }
            }
            log(&rotulo, "canal de entrada encerrado");
        });
}
