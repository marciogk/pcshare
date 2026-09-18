//! O servidor de encontro: casa dois lados por ID e repassa bytes cifrados.
//!
//! A lógica vive aqui, na biblioteca, e não no `main.rs`, por um motivo
//! concreto: assim ela pode ser exercitada por um teste de integração que sobe
//! um servidor de verdade e prova que dois clientes se acham e conversam. Um
//! servidor testável só pela mão é um servidor cujo defeito só aparece em
//! produção.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use pcshare_core::Transport;
use pcshare_core::quic::{QuicListener, QuicTransport};

use crate::licencas::{Licencas, Veredito};
use crate::{Mensagem, enviar, normalizar_id, receber};

/// O que o servidor sabe a cada instante.
#[derive(Default)]
struct Estado {
    /// Hosts registrados e à espera de um viewer, por ID normalizado.
    ///
    /// O `QuicTransport` fica aqui parado; o quinn mantém a conexão viva por
    /// conta própria (keep-alive). Quando um viewer chega, ele é retirado
    /// daqui e entregue ao repasse.
    registrados: HashMap<String, QuicTransport>,
    /// IDs em sessão. Existe só para distinguir "ocupado" de "inexistente" na
    /// resposta ao viewer — a diferença poupa o operador de trocar o ID à toa.
    ocupados: HashSet<String>,
}

type Compartilhado = Arc<Mutex<Estado>>;

/// Atende para sempre. Só retorna se o ponto de escuta morrer.
///
/// `licencas`: `Some` liga a **cobrança** — todo viewer precisa apresentar uma
/// licença válida para conectar. `None` é o **modo aberto**: qualquer um conecta
/// (o comportamento de sempre, e o padrão em desenvolvimento e testes).
pub fn servir(escuta: QuicListener, licencas: Option<Licencas>) {
    let estado: Compartilhado = Arc::new(Mutex::new(Estado::default()));
    if licencas.is_some() {
        log("cobrança LIGADA: viewers precisam de licença válida");
    } else {
        log("modo aberto: sem cobrança (nenhum arquivo de licenças)");
    }
    loop {
        match escuta.accept() {
            Ok(t) => {
                // Cada conexão num thread próprio: ler a primeira mensagem
                // bloqueia até 15 s, e fazer isso no laço de aceitação
                // impediria a próxima conexão de entrar.
                let estado = estado.clone();
                let licencas = licencas.clone();
                std::thread::spawn(move || atender(t, estado, licencas));
            }
            Err(e) => {
                // `accept` já engole tentativas que não completam o handshake;
                // um erro que chega aqui é do próprio endpoint.
                log(&format!("aceitação falhou: {e}; encerrando"));
                return;
            }
        }
    }
}

/// Lê a intenção de quem acabou de conectar e a atende.
fn atender(mut t: QuicTransport, estado: Compartilhado, licencas: Option<Licencas>) {
    let quem = t.remote();
    let primeira = match receber(&mut t) {
        Ok(m) => m,
        Err(e) => {
            log(&format!("{quem}: não disse ao que veio ({e})"));
            return;
        }
    };

    match primeira {
        // O host se registra de graça — nunca apresenta licença.
        Mensagem::Registrar(id) => registrar(t, &id, estado),
        // Viewer sem licença: em modo aberto conecta; em cobrança, é recusado.
        Mensagem::Conectar(id) => conectar(t, &id, None, estado, licencas),
        // Viewer com licença: o operador que assina.
        Mensagem::ConectarComLicenca { id, licenca } => {
            conectar(t, &id, Some(licenca), estado, licencas)
        }
        // Só host e viewer iniciam conversa; qualquer outra coisa é ruído ou
        // uma ponta confusa, e não há o que fazer com ela.
        outra => log(&format!("{quem}: primeira mensagem inesperada ({outra:?})")),
    }
}

/// Host se registra e passa a esperar. Fica guardado até um viewer chegar.
fn registrar(mut host: QuicTransport, id: &str, estado: Compartilhado) {
    let chave = normalizar_id(id);
    if chave.is_empty() {
        log(&format!("{}: registro com ID vazio, recusado", host.remote()));
        return;
    }
    if enviar(&mut host, &Mensagem::RegistroOk).is_err() {
        return;
    }
    log(&format!("{} registrou-se como '{chave}'", host.remote()));
    // Uma segunda conexão com o mesmo ID substitui a primeira: é o caso de um
    // host que caiu e voltou antes de o servidor perceber a queda. A conexão
    // velha, se ainda existir, morre sozinha por keep-alive.
    if let Ok(mut e) = estado.lock() {
        e.registrados.insert(chave, host);
    }
}

/// Viewer pede um ID. Confere a licença (se houver cobrança), casa com o host,
/// ou explica por que não deu.
fn conectar(
    mut viewer: QuicTransport,
    id: &str,
    licenca: Option<String>,
    estado: Compartilhado,
    licencas: Option<Licencas>,
) {
    // Portão da cobrança: antes de qualquer pareamento. Só roda quando há um
    // arquivo de licenças; em modo aberto, passa direto.
    if let Some(store) = &licencas {
        let apresentada = licenca.as_deref().unwrap_or("");
        match store.conferir(apresentada) {
            Veredito::Ok => {}
            Veredito::Invalida => {
                log(&format!("{}: licença inválida, recusado", viewer.remote()));
                let _ = enviar(&mut viewer, &Mensagem::LicencaInvalida);
                return;
            }
            Veredito::Expirada => {
                log(&format!("{}: licença expirada, recusado", viewer.remote()));
                let _ = enviar(&mut viewer, &Mensagem::LicencaExpirada);
                return;
            }
        }
    }

    let chave = normalizar_id(id);

    let host = {
        let mut e = match estado.lock() {
            Ok(e) => e,
            Err(_) => return,
        };
        e.registrados.remove(&chave).inspect(|_| {
            e.ocupados.insert(chave.clone());
        })
    };

    let mut host = match host {
        Some(h) => h,
        None => {
            let ocupado = estado
                .lock()
                .map(|e| e.ocupados.contains(&chave))
                .unwrap_or(false);
            let resposta = if ocupado {
                Mensagem::Ocupado
            } else {
                Mensagem::Indisponivel
            };
            log(&format!(
                "{}: pediu '{chave}' — {}",
                viewer.remote(),
                if ocupado { "ocupado" } else { "indisponível" }
            ));
            let _ = enviar(&mut viewer, &resposta);
            return;
        }
    };

    // Avisa cada um que o par chegou, e passa a ele o **endereço público do
    // outro** — o que cada lado precisa para tentar furar o NAT e conectar
    // direto. Se o furo falhar, os dois voltam a usar estas mesmas conexões e o
    // repasse abaixo os atende: o relay segue como reserva.
    let endereco_host = host.remote().to_string();
    let endereco_viewer = viewer.remote().to_string();
    let ok_host = enviar(&mut host, &Mensagem::EmparelhadoCom(endereco_viewer)).is_ok();
    let ok_viewer = enviar(&mut viewer, &Mensagem::EmparelhadoCom(endereco_host)).is_ok();
    if !ok_host || !ok_viewer {
        log(&format!("'{chave}': um dos lados caiu antes de emparelhar"));
        soltar_ocupado(&estado, &chave);
        return;
    }

    log(&format!("'{chave}': emparelhado, repassando (reserva)"));
    repassar(host, viewer);
    soltar_ocupado(&estado, &chave);
    log(&format!("'{chave}': sessão encerrada"));
}

fn soltar_ocupado(estado: &Compartilhado, chave: &str) {
    if let Ok(mut e) = estado.lock() {
        e.ocupados.remove(chave);
    }
}

/// Copia mensagens de um lado para o outro até uma das pontas cair.
///
/// Preserva o canal: mídia continua mídia, controle continua controle. É o que
/// mantém, através do relay, a independência de fluxos que a decisão D4
/// conquistou — um quadro grande não segura um clique.
///
/// O Noise viaja aqui dentro como payload opaco. O servidor move os bytes sem
/// nunca saber o que são.
fn repassar(mut a: QuicTransport, mut b: QuicTransport) {
    loop {
        let mut andou = false;

        match a.poll() {
            Ok(Some((canal, dados))) => {
                andou = true;
                if b.send(canal, &dados).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => return,
        }

        match b.poll() {
            Ok(Some((canal, dados))) => {
                andou = true;
                if a.send(canal, &dados).is_err() {
                    return;
                }
            }
            Ok(None) => {}
            Err(_) => return,
        }

        // Sem tráfego, uma pausa curta evita queimar um núcleo. Com tráfego, o
        // laço gira sem dormir, para não somar atraso ao vídeo. 1 ms é o teto
        // de latência acrescentada, e some quando há o que repassar.
        if !andou {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Data e hora locais na frente de cada linha. Um servidor de produção manda
/// isto para o journald ou um arquivo; o carimbo é o mínimo para casar um
/// evento com o relato de quem estava do outro lado.
fn log(msg: &str) {
    let agora = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("[{agora}] {msg}");
}
