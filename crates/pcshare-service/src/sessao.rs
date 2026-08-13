//! A sessão de rede: QUIC, handshake Noise, e o repasse entre viewer e agente.
//!
//! # Por que isto mora no supervisor
//!
//! Toda troca de desktop mata e recria agentes. Se a sessão morasse neles,
//! cada prompt de UAC derrubaria o viewer — trocaríamos "tela congelada por um
//! instante" por "conexão caiu", que é pior, e desfaria o que a Fase 3
//! conquistou. O supervisor é o único processo que atravessa as trocas.
//!
//! A divisão de trabalho que sai daí:
//!
//! - **agente**: captura, converte e codifica. Precisa estar no desktop.
//! - **supervisor**: cifra e transmite. Precisa sobreviver ao desktop.
//!
//! # Por que numa thread própria
//!
//! O laço do supervisor acorda a cada 50 ms, ritmo pensado para *perceber
//! trocas de desktop*. Repassar quadros nesse ritmo somaria até 50 ms de
//! atraso a cada quadro — mais que a latência inteira medida no `pcshare-spike`
//! (53,6 ms). O repasse tem de ter o ritmo dos quadros, não o da supervisão.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use pcshare_capture::canal::{self, Mensagem};
use pcshare_core::quic::QuicListener;
use pcshare_core::{Channel, Identity, Message, Transport, secure};

use crate::supervisor::log;

/// Acima deste acúmulo na fila de saída, o quadro é descartado em vez de
/// enfileirado.
///
/// Numa tela remota **descartar é melhor que atrasar**: ninguém quer ver o que
/// a tela era há dois segundos. Meio megabyte a 3 Mbps já passa de um segundo.
const LIMITE_DE_FILA: usize = 512 * 1024;

/// Quanto o laço espera por mensagem do agente antes de dar uma volta para
/// atender o viewer. Curto o bastante para não somar atraso perceptível à
/// entrada, longo o bastante para o laço não girar em falso.
const ESPERA: Duration = Duration::from_millis(2);

/// Sobe a thread que atende viewers, para sempre.
///
/// Consome o `Servidor` do canal: a partir daqui é esta thread que fala com os
/// agentes. O que o supervisor ainda precisa saber — trocas de desktop — sobe
/// por `tx_desktop`.
pub fn iniciar(servidor: canal::Servidor, tx_desktop: Sender<String>, endereco: SocketAddr, senha: String) {
    std::thread::Builder::new()
        .name("pcshare-sessao".into())
        .spawn(move || atender_sempre(servidor, tx_desktop, endereco, senha))
        .ok();
}

fn atender_sempre(
    servidor: canal::Servidor,
    tx_desktop: Sender<String>,
    endereco: SocketAddr,
    senha: String,
) {
    let identidade = match identidade_da_maquina() {
        Ok(i) => i,
        Err(e) => {
            log(&format!("sessão: sem identidade ({e}); rede desativada"));
            // Sem rede ainda é preciso escoar o canal, ou a fila do agente
            // cresce sem fim e ele acaba bloqueado escrevendo para ninguém.
            escoar_sem_viewer(&servidor, &tx_desktop);
            return;
        }
    };
    let escuta = match QuicListener::bind(endereco) {
        Ok(l) => l,
        Err(e) => {
            log(&format!("sessão: não consegui escutar em {endereco} ({e})"));
            escoar_sem_viewer(&servidor, &tx_desktop);
            return;
        }
    };

    log(&format!("sessão: escutando em {endereco}"));
    log(&format!("sessão: ID desta máquina {}", identidade.display_id()));
    log(&format!("sessão: senha {senha}"));

    // Aceitar numa thread à parte, e não aqui, por um motivo concreto: este
    // laço é o **único** que esvazia o canal dos agentes. Bloqueando em
    // `accept` à espera de um viewer que talvez nunca venha, os relatos de
    // troca de desktop parariam de ser lidos, e a Fase 3 pararia junto.
    let (tx_viewer, rx_viewer) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pcshare-aceita".into())
        .spawn(move || {
            let mut falhas = 0u32;
            loop {
                match escuta.accept() {
                    Ok(t) => {
                        falhas = 0;
                        if tx_viewer.send(t).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        // Uma tentativa de conexão que morre no meio não pode
                        // derrubar o atendimento — foi assim que o host do
                        // spike encerrou sozinho num teste real.
                        falhas += 1;
                        log(&format!("sessão: conexão incompleta ({e})"));
                        if falhas >= 50 {
                            log("sessão: 50 falhas seguidas ao aceitar; desistindo");
                            return;
                        }
                    }
                }
            }
        })
        .ok();

    // Uma sessão que acaba não pode encerrar o atendimento: o host está na
    // máquina que o usuário justamente não alcança. Foi o defeito mais grave
    // encontrado no teste entre duas máquinas.
    let mut n = 0u64;
    loop {
        // Sem viewer, escoa o canal e volta a olhar. É aqui que os relatos de
        // desktop continuam chegando ao supervisor com a tela ociosa.
        let Ok(t) = rx_viewer.try_recv() else {
            if escoar(&servidor, &tx_desktop).is_err() {
                return; // supervisor encerrou
            }
            continue;
        };

        n += 1;
        log(&format!("--- sessão {n} iniciada ---"));
        let resultado = match secure::accept(t, &identidade, &senha) {
            Ok(sec) => {
                log(&format!("sessão {n}: cifrada com {}", sec.remote_fingerprint()));
                let ligados = servidor.enviar_a_todos(&Mensagem::Capturar(true));
                log(&format!("sessão {n}: {ligados} agentes transmitindo"));
                let r = servir(sec, &servidor, &tx_desktop);
                servidor.enviar_a_todos(&Mensagem::Capturar(false));
                r
            }
            Err(e) => Err(Box::new(e) as Box<dyn std::error::Error>),
        };
        match resultado {
            Ok(()) => log(&format!("--- sessão {n} encerrada ---")),
            Err(e) => log(&format!("--- sessão {n} encerrada: {} ---", motivo(e.as_ref()))),
        }
    }
}

/// Traduz os fins de sessão esperados para linguagem de gente.
///
/// Nem todo `Err` é defeito: o viewer fechar a janela chega como "unexpected
/// end of file", e senha errada como "decrypt error". Escritos assim no log,
/// fariam qualquer um procurar um problema que não existe.
fn motivo(e: &(dyn std::error::Error + 'static)) -> String {
    use std::io::ErrorKind::*;
    match e.downcast_ref::<std::io::Error>().map(|io| io.kind()) {
        Some(UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted) => {
            "viewer desconectou".into()
        }
        Some(PermissionDenied) => "autenticação recusada (senha incorreta)".into(),
        Some(TimedOut) => "tempo esgotado esperando o viewer".into(),
        _ => e.to_string(),
    }
}

/// O laço de uma sessão: quadros sobem, entrada desce.
fn servir<T: Transport>(
    mut t: T,
    servidor: &canal::Servidor,
    tx_desktop: &Sender<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // O viewer não desenha nada antes do `Hello`, e só o agente conhece a
    // resolução — o supervisor está na sessão 0, cego. Então a sessão começa
    // esperando o `Pronto` de algum agente.
    let mut dimensoes: Option<(u32, u32)> = None;
    // Quem produziu o último quadro. É para ele que a entrada vai: agentes
    // cegos não produzem quadro nenhum, então nenhuma coordenação extra é
    // necessária para saber qual está ativo.
    let mut agente_ativo: Option<u64> = None;
    // Numeração da sessão, não do agente. Cada agente numera a partir de zero,
    // e ao trocar de desktop a sequência do viewer andaria para trás.
    let mut seq = 0u64;
    let mut descartados = 0u64;
    let mut enviados = 0u64;
    let mut proximo_relato = Instant::now() + Duration::from_secs(5);

    loop {
        // ---- o que os agentes mandaram
        let mut m = servidor.receber_ate(ESPERA);
        while let Some((id, msg)) = m.take() {
            match msg {
                Mensagem::Quadro {
                    tempo_us,
                    chave,
                    dados,
                    ..
                } => {
                    agente_ativo = Some(id);
                    if dimensoes.is_none() {
                        // Quadro antes do `Pronto` não deveria acontecer, mas
                        // se acontecer, descartar é melhor que mandar vídeo
                        // que o viewer não sabe desenhar.
                        descartados += 1;
                    } else if t.pending_out() > LIMITE_DE_FILA {
                        // O encode já foi gasto no agente e se perde aqui.
                        // Aceitável por ora: mandar o agente pausar acopla os
                        // dois processos, e só compensa se a medição mostrar
                        // desperdício relevante.
                        descartados += 1;
                    } else {
                        let v = Message::Video {
                            seq,
                            time_us: tempo_us,
                            is_keyframe: chave,
                            data: dados,
                        };
                        t.send(v.channel(), &v.encode())?;
                        seq += 1;
                        enviados += 1;
                    }
                }
                Mensagem::Pronto { largura, altura } => {
                    if dimensoes.is_none() {
                        dimensoes = Some((largura, altura));
                        let hello = Message::Hello {
                            width: largura,
                            height: altura,
                            fps: 60,
                        };
                        t.send(hello.channel(), &hello.encode())?;
                        log(&format!("sessão: anunciada tela {largura}x{altura}"));
                        // O agente pode já estar codificando há bastante tempo;
                        // sem isto o viewer esperaria o próximo keyframe
                        // espontâneo para ter imagem.
                        let _ = servidor.enviar(id, &Mensagem::PedirKeyframe);
                    }
                    // Trocas de resolução no meio da sessão ficam para depois:
                    // exigiriam recriar o decodificador no viewer, que hoje
                    // nasce com as dimensões do `Hello`.
                }
                Mensagem::Desktop(nome) => {
                    if tx_desktop.send(nome).is_err() {
                        return Ok(()); // supervisor encerrou
                    }
                }
                outra => log(&format!("sessão: mensagem inesperada do agente: {outra:?}")),
            }
            // Esvazia o resto sem voltar a bloquear.
            m = servidor.receber();
        }

        // ---- o que o viewer mandou
        while let Some((canal_, carga)) = t.poll()? {
            if canal_ != Channel::Control {
                continue;
            }
            match Message::decode(&carga) {
                Ok(Message::Ping { nonce, sent_us }) => {
                    let p = Message::Pong { nonce, sent_us };
                    t.send(p.channel(), &p.encode())?;
                }
                Ok(m @ Message::Input(_)) => {
                    // Reencaminha já serializado: o agente decodifica do outro
                    // lado com o mesmo protocolo, sem tradução no meio.
                    if let Some(id) = agente_ativo {
                        let _ = servidor.enviar(id, &Mensagem::Entrada(m.encode()));
                    }
                }
                Ok(Message::RequestKeyframe) => {
                    if let Some(id) = agente_ativo {
                        let _ = servidor.enviar(id, &Mensagem::PedirKeyframe);
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }

        t.flush()?;

        if Instant::now() >= proximo_relato {
            log(&format!(
                "sessão: {enviados} quadros repassados, {descartados} descartados, \
                 {} agentes no canal",
                servidor.conectados()
            ));
            proximo_relato += Duration::from_secs(5);
        }
    }
}

/// Escoa uma rodada do canal sem viewer conectado.
///
/// Bloqueia por até `ESPERA`, então quem chama pode voltar a olhar se um
/// viewer chegou sem girar em falso. Devolve `Err` quando o supervisor sumiu.
fn escoar(servidor: &canal::Servidor, tx_desktop: &Sender<String>) -> Result<(), ()> {
    let mut m = servidor.receber_ate(ESPERA);
    while let Some((_, msg)) = m.take() {
        // Quadros são descartados: os agentes deveriam estar em espera, e um
        // que ainda esteja transmitindo não pode encher o cano até travar.
        if let Mensagem::Desktop(nome) = msg
            && tx_desktop.send(nome).is_err()
        {
            return Err(());
        }
        m = servidor.receber();
    }
    Ok(())
}

/// Sem rede, o supervisor ainda precisa dos relatos de desktop: é o que mantém
/// a Fase 3 funcionando mesmo que ninguém consiga se conectar.
fn escoar_sem_viewer(servidor: &canal::Servidor, tx_desktop: &Sender<String>) {
    while escoar(servidor, tx_desktop).is_ok() {}
}

/// Identidade da **máquina**, não do usuário.
///
/// Vive em `ProgramData` porque o supervisor roda como SYSTEM, sem perfil de
/// usuário — e porque o ID precisa ser o mesmo independentemente de quem está
/// logado. A chave é a metade privada de um par cuja metade pública vira o ID,
/// então gerar uma nova a cada início trocaria o ID e invalidaria qualquer
/// pareamento anterior.
fn identidade_da_maquina() -> Result<Identity, Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare");
    let caminho = dir.join("identity.key");

    if let Ok(bytes) = std::fs::read(&caminho)
        && let Ok(id) = Identity::from_private(bytes)
    {
        return Ok(id);
    }

    let id = Identity::generate()?;
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&caminho, id.private_key())?;
    log(&format!("identidade criada em {}", caminho.display()));
    // TODO(produto): a chave fica em texto claro. Precisa de DPAPI e de uma
    // ACL que só SYSTEM leia — hoje qualquer processo local a lê.
    Ok(id)
}
