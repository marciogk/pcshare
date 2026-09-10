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

use pcshare_core::{Channel, Transport};
use std::io;

/// Tempo máximo esperando uma resposta do servidor de encontro.
///
/// Vale para cada troca da sinalização, não para a sessão. Um servidor mudo
/// por mais que isto é um servidor que não vai responder.
pub const TIMEOUT_SINAL: std::time::Duration = std::time::Duration::from_secs(15);

const T_REGISTRAR: u8 = 1;
const T_CONECTAR: u8 = 2;
const T_REGISTRO_OK: u8 = 3;
const T_EMPARELHADO: u8 = 4;
const T_INDISPONIVEL: u8 = 5;
const T_OCUPADO: u8 = 6;

/// Teto do ID na sinalização. O ID real tem nove dígitos; o limite existe só
/// para um cabeçalho corrompido não pedir uma alocação absurda.
const MAX_ID: usize = 64;

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
        }
    }

    fn corpo(&self) -> &str {
        match self {
            Self::Registrar(id) | Self::Conectar(id) => id,
            _ => "",
        }
    }

    /// `[tag u8][tamanho u16 LE][id utf8]`.
    pub fn codificar(&self) -> Vec<u8> {
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
