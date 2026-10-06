//! O encontro: como duas máquinas em redes distintas acham uma à outra.
//!
//! # O problema que isto resolve
//!
//! Hoje o viewer conecta por `IP:porta`. Na mesma rede, ou por Tailscale, isso
//! basta. Na internet aberta não: a máquina remota está atrás de um NAT que não
//! tem endereço público fixo nem porta aberta, e ninguém de fora consegue
//! iniciar uma conexão para ela.
//!
//! A saída é a mesma do AnyDesk e do TeamViewer, e depende de um fato simples:
//! **conexões de saída atravessam qualquer NAT**. Um roteador bloqueia quem
//! chega de fora, mas deixa sair livremente e mantém o caminho de volta aberto.
//! Então:
//!
//! 1. O **host** abre uma conexão de saída para um servidor público e se
//!    registra sob o seu ID.
//! 2. O **viewer** abre uma conexão de saída para o mesmo servidor e pede o ID.
//! 3. O servidor casa os dois e passa a **repassar bytes** de um para o outro.
//!
//! Nenhum dos dois precisou aceitar conexão de entrada. O ID que a pessoa dita
//! por telefone substitui o endereço de rede.
//!
//! # Por que o servidor não é uma ameaça
//!
//! Ele vê passar apenas o que o [`pcshare_core::secure`] cifrou — o Noise roda
//! **ponta a ponta, por cima** deste repasse (decisão D7). O servidor casa
//! conexões e copia bytes opacos; não tem a senha, não deriva a chave, não
//! decifra um quadro sequer. É um cano cego, e é o que permite hospedá-lo sem
//! que ele vire o ponto onde toda a confiança se concentra.
//!
//! # O que este módulo é, e o que não é
//!
//! Aqui mora só o **protocolo de sinalização**: as poucas mensagens trocadas
//! com o servidor antes de o repasse começar. O servidor em si é o binário
//! `main.rs`; religar host e viewer para falá-lo é dos crates deles.

use pcshare_core::quic::{QuicEndpoint, QuicTransport};
use pcshare_core::{Channel, Transport};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

pub mod admin;
pub mod licencas;
pub mod servidor;

/// Tempo máximo esperando uma resposta do servidor de encontro.
///
/// Vale para cada troca da sinalização, não para a sessão. Um servidor mudo
/// por mais que isto é um servidor que não vai responder.
pub const TIMEOUT_SINAL: Duration = Duration::from_secs(15);

/// Prazo para completar a conexão ao servidor de encontro.
const TIMEOUT_CONECTA: Duration = Duration::from_secs(10);

/// Prazo do furo de NAT: quanto se espera pela conexão direta antes de desistir
/// e usar o relay. Curto de propósito — a direta, quando funciona, sobe em um
/// par de RTTs; esperar mais só atrasaria a queda para o relay.
const TIMEOUT_FURO: Duration = Duration::from_secs(3);

const T_REGISTRAR: u8 = 1;
const T_CONECTAR: u8 = 2;
const T_REGISTRO_OK: u8 = 3;
const T_EMPARELHADO: u8 = 4;
const T_INDISPONIVEL: u8 = 5;
const T_OCUPADO: u8 = 6;
const T_CONECTAR_LIC: u8 = 7;
const T_LICENCA_INVALIDA: u8 = 8;
const T_LICENCA_EXPIRADA: u8 = 9;
const T_EMPARELHADO_COM: u8 = 10;

/// Teto do ID na sinalização. O ID real tem nove dígitos; o limite existe só
/// para um cabeçalho corrompido não pedir uma alocação absurda.
const MAX_ID: usize = 64;

/// Teto da chave de licença na sinalização. Mesma razão do [`MAX_ID`].
const MAX_LICENCA: usize = 256;

/// As mensagens trocadas com o servidor **antes** de o repasse começar.
///
/// Depois do [`Mensagem::Emparelhado`] o servidor sai de cena como interlocutor
/// e vira cano: tudo o que trafega passa a ser o handshake Noise e a sessão,
/// que ele não interpreta.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mensagem {
    /// Host → servidor: sou este ID, segure conexões destinadas a mim.
    Registrar(String),
    /// Viewer → servidor: quero falar com este ID.
    Conectar(String),
    /// Servidor → host: você está registrado e à espera.
    RegistroOk,
    /// Servidor → ambos: seu par chegou. A partir daqui, tudo é repassado.
    Emparelhado,
    /// Servidor → ambos: seu par chegou, e este é o **endereço público** dele
    /// (`ip:porta`, como o servidor o vê). Com ele, os dois tentam furar o NAT
    /// e conectar **direto**, sem o relay. Se o furo falhar, caem no repasse,
    /// que segue disponível — daí esta mensagem não substituir o
    /// [`Emparelhado`](Self::Emparelhado), e sim carregar o extra.
    EmparelhadoCom(String),
    /// Servidor → viewer: esse ID não está registrado em lugar nenhum.
    ///
    /// A máquina remota pode estar desligada, sem rede, ou com o serviço
    /// parado — daqui não dá para distinguir, e a mensagem ao usuário deve
    /// dizer isso em vez de fingir precisão.
    Indisponivel,
    /// Servidor → viewer: esse ID existe, mas já está numa sessão.
    ///
    /// Uma máquina atende um viewer por vez. Distinguir "ocupado" de
    /// "inexistente" evita que o operador fique trocando o ID à toa quando o
    /// problema é que um colega já está conectado.
    Ocupado,
    /// Viewer → servidor: quero falar com este ID, e esta é minha licença.
    ///
    /// É o que sustenta a cobrança: quem **acessa** (operador) apresenta a
    /// licença; quem é **acessado** (host) só faz [`Registrar`](Self::Registrar),
    /// de graça. O servidor só confere a licença quando roda em modo de
    /// cobrança (com um arquivo de licenças configurado).
    ConectarComLicenca { id: String, licenca: String },
    /// Servidor → viewer: licença ausente, desconhecida ou desativada.
    LicencaInvalida,
    /// Servidor → viewer: licença existe, mas a assinatura venceu.
    LicencaExpirada,
}

impl Mensagem {
    fn tag(&self) -> u8 {
        match self {
            Self::Registrar(_) => T_REGISTRAR,
            Self::Conectar(_) => T_CONECTAR,
            Self::RegistroOk => T_REGISTRO_OK,
            Self::Emparelhado => T_EMPARELHADO,
            Self::Indisponivel => T_INDISPONIVEL,
            Self::Ocupado => T_OCUPADO,
            Self::ConectarComLicenca { .. } => T_CONECTAR_LIC,
            Self::LicencaInvalida => T_LICENCA_INVALIDA,
            Self::LicencaExpirada => T_LICENCA_EXPIRADA,
            Self::EmparelhadoCom(_) => T_EMPARELHADO_COM,
        }
    }

    fn corpo(&self) -> &str {
        match self {
            Self::Registrar(id) | Self::Conectar(id) | Self::EmparelhadoCom(id) => id,
            _ => "",
        }
    }

    /// Mensagens de um campo: `[tag u8][tamanho u16 LE][id utf8]`.
    /// `ConectarComLicenca` tem dois campos: `[tag][len_id][id][len_lic][lic]`.
    pub fn codificar(&self) -> Vec<u8> {
        if let Self::ConectarComLicenca { id, licenca } = self {
            let id = id.as_bytes();
            let lic = licenca.as_bytes();
            let mut v = Vec::with_capacity(id.len() + lic.len() + 5);
            v.push(self.tag());
            v.extend_from_slice(&(id.len() as u16).to_le_bytes());
            v.extend_from_slice(id);
            v.extend_from_slice(&(lic.len() as u16).to_le_bytes());
            v.extend_from_slice(lic);
            return v;
        }
        let corpo = self.corpo().as_bytes();
        let mut v = Vec::with_capacity(corpo.len() + 3);
        v.push(self.tag());
        v.extend_from_slice(&(corpo.len() as u16).to_le_bytes());
        v.extend_from_slice(corpo);
        v
    }

    /// `None` para cabeçalho truncado, tamanho absurdo ou tag desconhecida —
    /// nunca em pânico. É o mesmo cuidado do protocolo principal: bytes vindos
    /// da rede não podem derrubar quem os lê.
    pub fn decodificar(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        if bytes[0] == T_CONECTAR_LIC {
            // [tag][len_id u16][id][len_lic u16][licenca]
            if bytes.len() < 3 {
                return None;
            }
            let len_id = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
            if len_id > MAX_ID {
                return None;
            }
            let fim_id = 3 + len_id;
            if bytes.len() < fim_id + 2 {
                return None;
            }
            let id = std::str::from_utf8(&bytes[3..fim_id]).ok()?;
            let len_lic = u16::from_le_bytes([bytes[fim_id], bytes[fim_id + 1]]) as usize;
            if len_lic > MAX_LICENCA {
                return None;
            }
            let fim_lic = fim_id + 2 + len_lic;
            if bytes.len() < fim_lic {
                return None;
            }
            let lic = std::str::from_utf8(&bytes[fim_id + 2..fim_lic]).ok()?;
            return Some(Self::ConectarComLicenca {
                id: id.to_string(),
                licenca: lic.to_string(),
            });
        }
        if bytes.len() < 3 {
            return None;
        }
        let tam = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
        if tam > MAX_ID || bytes.len() < 3 + tam {
            return None;
        }
        let corpo = std::str::from_utf8(&bytes[3..3 + tam]).ok()?;
        match bytes[0] {
            T_REGISTRAR => Some(Self::Registrar(corpo.to_string())),
            T_CONECTAR => Some(Self::Conectar(corpo.to_string())),
            T_REGISTRO_OK => Some(Self::RegistroOk),
            T_EMPARELHADO => Some(Self::Emparelhado),
            T_INDISPONIVEL => Some(Self::Indisponivel),
            T_OCUPADO => Some(Self::Ocupado),
            T_LICENCA_INVALIDA => Some(Self::LicencaInvalida),
            T_LICENCA_EXPIRADA => Some(Self::LicencaExpirada),
            T_EMPARELHADO_COM => Some(Self::EmparelhadoCom(corpo.to_string())),
            _ => None,
        }
    }
}

/// Reduz um ID ao que importa para casar: só os dígitos.
///
/// O ID é ditado por telefone e digitado à mão. "895 570 225", "895570225" e
/// "895-570-225" são a mesma máquina, e casar por igualdade literal recusaria
/// duas delas. O servidor normaliza os dois lados antes de comparar.
pub fn normalizar_id(id: &str) -> String {
    id.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Envia uma mensagem de sinalização pelo canal de controle.
///
/// A sinalização inteira trafega pelo canal de controle, que é confiável e
/// ordenado — perder ou embaralhar um "registre-me" arruinaria o encontro.
pub fn enviar<T: Transport>(t: &mut T, m: &Mensagem) -> io::Result<()> {
    t.send(Channel::Control, &m.codificar())?;
    t.flush()
}

/// Bloqueia até uma mensagem de sinalização chegar, ou esgotar [`TIMEOUT_SINAL`].
///
/// Ignora o que vier pelo canal de mídia: antes do emparelhamento não deveria
/// existir mídia, e se existir é ruído que não pode confundir a sinalização.
pub fn receber<T: Transport>(t: &mut T) -> io::Result<Mensagem> {
    let limite = std::time::Instant::now() + TIMEOUT_SINAL;
    loop {
        if let Some((canal, dados)) = t.poll()? {
            if canal == Channel::Control {
                return Mensagem::decodificar(&dados).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "sinalização ilegível")
                });
            }
            continue;
        }
        if std::time::Instant::now() >= limite {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "servidor de encontro não respondeu",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

// -------------------------------------------------- lado do host e do viewer

/// Host: conecta ao encontro, registra-se sob `id`, e **bloqueia até um viewer
/// chegar**. Devolve o transporte já emparelhado, pronto para `secure::accept`.
///
/// A espera pelo par não tem prazo — um viewer pode levar horas —, ao contrário
/// da espera pela confirmação do registro, que o servidor responde na hora. Se
/// a conexão com o servidor cair enquanto se espera, isto retorna erro, e o
/// host deve reconectar e registrar-se de novo.
pub fn registrar_e_esperar(servidor: SocketAddr, id: &str) -> io::Result<QuicTransport> {
    // Um endpoint só, reusado para o encontro e para a direta — é o que faz o
    // furo casar. O host aceita a conexão direta do viewer, então precisa da
    // capacidade de servidor.
    let endpoint = QuicEndpoint::abrir_servidor()?;
    let mut relay = endpoint.conectar(servidor, TIMEOUT_CONECTA)?;
    enviar(&mut relay, &Mensagem::Registrar(id.to_string()))?;
    match receber(&mut relay)? {
        Mensagem::RegistroOk => {}
        outra => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("servidor de encontro respondeu {outra:?} ao registro"),
            ));
        }
    }
    // Espera o viewer (sem prazo). Devolve o endereço público dele, ou `None`
    // se o servidor for antigo (só `Emparelhado`) — aí não há furo, só relay.
    let par = esperar_par(&mut relay)?;
    Ok(furar_ou_relay(par, relay, |peer| endpoint.aceitar_furando(peer, TIMEOUT_FURO)))
}

/// Viewer: conecta ao encontro e pede `id`. Devolve o transporte emparelhado,
/// pronto para `secure::connect`.
///
/// Os erros são **específicos**, e isso é o ponto: um viewer que só diz
/// "não deu" manda o operador tentar de novo às cegas. Aqui, "máquina não
/// encontrada" e "máquina ocupada" dizem o que fazer — conferir o ID, ou
/// esperar o colega desconectar.
/// `licenca`: a chave do operador (assinante). `None` manda o `Conectar` antigo,
/// sem licença — vale para a CLI, medições, e servidores em modo aberto. Em modo
/// de cobrança o servidor recusa quem não apresenta uma licença válida.
pub fn conectar_por_id(
    servidor: SocketAddr,
    id: &str,
    licenca: Option<&str>,
) -> io::Result<QuicTransport> {
    let pedido = match licenca {
        Some(l) => Mensagem::ConectarComLicenca {
            id: id.to_string(),
            licenca: l.to_string(),
        },
        None => Mensagem::Conectar(id.to_string()),
    };
    let endpoint = QuicEndpoint::abrir_cliente()?;
    let mut relay = endpoint.conectar(servidor, TIMEOUT_CONECTA)?;
    enviar(&mut relay, &pedido)?;
    let par = match receber(&mut relay)? {
        Mensagem::EmparelhadoCom(addr) => Some(addr),
        // Servidor antigo, sem endereço: sem furo, só relay.
        Mensagem::Emparelhado => None,
        Mensagem::Indisponivel => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "máquina não encontrada — confira o ID, ou ela pode estar desligada ou sem rede",
            ));
        }
        Mensagem::Ocupado => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "máquina já está em outra sessão — tente de novo em instantes",
            ));
        }
        Mensagem::LicencaInvalida => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "licença inválida — confira a chave em Configurações, ou assine em pcshare.com.br",
            ));
        }
        Mensagem::LicencaExpirada => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "licença expirada — assine (ou renove) em pcshare.com.br para continuar",
            ));
        }
        outra => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("servidor de encontro respondeu {outra:?}"),
            ));
        }
    };
    // O viewer é o iniciador da direta: conecta ao endereço do host.
    Ok(furar_ou_relay(par, relay, |peer| {
        endpoint.conectar(peer, TIMEOUT_FURO)
    }))
}

/// Escolhe o transporte da sessão. Com o endereço do par e uma `tentativa`
/// direta que sobe, usa a **direta** (e derruba o relay para não segurá-lo à
/// toa); senão, o **relay** pelo servidor. Os dois lados usam a mesma lógica com
/// papéis diferentes (o viewer conecta; o host aceita e fura).
fn furar_ou_relay(
    par: Option<String>,
    relay: QuicTransport,
    tentativa: impl FnOnce(SocketAddr) -> io::Result<QuicTransport>,
) -> QuicTransport {
    let Some(addr) = par else {
        return relay;
    };
    let Ok(peer) = addr.parse::<SocketAddr>() else {
        eprintln!("encontro: endereço do par ilegível ('{addr}'); usando relay");
        return relay;
    };
    match tentativa(peer) {
        Ok(direta) => {
            println!("encontro: conexão DIRETA com {peer} — sem passar pelo servidor");
            drop(relay);
            direta
        }
        Err(_) => {
            println!("encontro: furo de NAT não vingou; usando o relay pelo servidor");
            relay
        }
    }
}

/// Espera o par chegar, **sem prazo**, e devolve o endereço público dele
/// (`Some`), ou `None` se o servidor mandar o `Emparelhado` antigo (sem
/// endereço). Separado de [`receber`] de propósito: aquele tem teto de 15 s,
/// errado para uma espera que dura o que durar até alguém querer conectar.
fn esperar_par<T: Transport>(t: &mut T) -> io::Result<Option<String>> {
    loop {
        match t.poll()? {
            Some((Channel::Control, dados)) => match Mensagem::decodificar(&dados) {
                Some(Mensagem::EmparelhadoCom(addr)) => return Ok(Some(addr)),
                Some(Mensagem::Emparelhado) => return Ok(None),
                Some(outra) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("esperava emparelhamento, veio {outra:?}"),
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sinalização ilegível enquanto esperava o par",
                    ));
                }
            },
            // Mídia antes do emparelhamento é ruído; ignora e segue esperando.
            Some(_) => {}
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ida_e_volta_de_todas_as_mensagens() {
        for m in [
            Mensagem::Registrar("895 570 225".into()),
            Mensagem::Conectar("895570225".into()),
            Mensagem::RegistroOk,
            Mensagem::Emparelhado,
            Mensagem::Indisponivel,
            Mensagem::Ocupado,
            Mensagem::ConectarComLicenca {
                id: "895 570 225".into(),
                licenca: "ABCD-1234-EFGH-5678".into(),
            },
            Mensagem::ConectarComLicenca {
                id: "895570225".into(),
                licenca: String::new(),
            },
            Mensagem::LicencaInvalida,
            Mensagem::LicencaExpirada,
            Mensagem::EmparelhadoCom("189.40.122.7:51820".into()),
        ] {
            let bytes = m.codificar();
            assert_eq!(Mensagem::decodificar(&bytes), Some(m));
        }
    }

    #[test]
    fn cabecalho_truncado_nao_causa_panico() {
        for n in 0..3 {
            assert_eq!(Mensagem::decodificar(&vec![T_CONECTAR; n]), None, "{n} bytes");
        }
        // Tamanho declarado maior que o corpo presente.
        assert_eq!(Mensagem::decodificar(&[T_CONECTAR, 10, 0, b'9']), None);
        // Tamanho acima do teto.
        let mut absurdo = vec![T_CONECTAR];
        absurdo.extend_from_slice(&(MAX_ID as u16 + 1).to_le_bytes());
        assert_eq!(Mensagem::decodificar(&absurdo), None);
        // Tag desconhecida.
        assert_eq!(Mensagem::decodificar(&[200, 0, 0]), None);
    }

    #[test]
    fn normalizacao_iguala_as_formas_ditadas() {
        let formas = ["895 570 225", "895570225", "895-570-225", "895.570.225"];
        for f in formas {
            assert_eq!(normalizar_id(f), "895570225", "forma {f:?}");
        }
    }
}
