//! Formato de fio das mensagens.
//!
//! Serialização escrita à mão, sem serde. Não é preciosismo: o formato de fio
//! de um produto de acesso remoto é superfície de ataque e precisa ser lido
//! por qualquer um que audite o projeto. Derivar de uma macro esconderia
//! exatamente o que um atacante consegue mandar.
//!
//! Todo inteiro é little-endian. Todo campo de tamanho variável é precedido
//! do seu comprimento em `u32` e validado contra o que resta do buffer.

use std::fmt;

/// Teto de sanidade para campos de tamanho variável. Um frame H.264 de 1080p
/// não passa disso nem em keyframe, e o limite impede que um peer hostil peça
/// uma alocação gigante.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// Faltam bytes: o buffer acabou no meio de um campo.
    Truncated,
    UnknownTag(u8),
    /// Comprimento declarado maior que [`MAX_PAYLOAD`] ou que o buffer.
    BadLength(usize),
    TrailingBytes(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => write!(f, "mensagem truncada"),
            Self::UnknownTag(t) => write!(f, "tag desconhecida: 0x{t:02x}"),
            Self::BadLength(n) => write!(f, "comprimento inválido: {n}"),
            Self::TrailingBytes(n) => write!(f, "{n} bytes sobrando após a mensagem"),
        }
    }
}

impl std::error::Error for ProtoError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputEvent {
    /// Coordenadas em pixels **da tela do host**. O viewer converte a partir
    /// das dimensões recebidas em [`Message::Hello`]; centralizar a conversão
    /// nele evita que o host precise saber do zoom da janela remota.
    MouseMove { x: i32, y: i32 },
    MouseButton { button: MouseButton, down: bool },
    /// Múltiplos de um entalhe padrão de roda (120), como o Windows reporta.
    MouseWheel { delta: i32 },
    /// Código de varredura do teclado, não caractere: o host reinjeta o
    /// evento físico e deixa o layout de teclado dele decidir a letra.
    Key { scancode: u32, down: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// Primeira mensagem do host. O viewer não pode desenhar nada antes dela.
    Hello { width: u32, height: u32, fps: u32 },
    Video {
        seq: u64,
        /// Microssegundos desde o início da sessão, relógio do host.
        time_us: i64,
        is_keyframe: bool,
        data: Vec<u8>,
    },
    /// Viewer → host. Enviado ao entrar na sessão e após perda detectada.
    RequestKeyframe,
    Input(InputEvent),
    /// `sent_us` volta intacto no Pong: o RTT sai de uma subtração local, sem
    /// depender de relógios sincronizados entre as duas máquinas.
    Ping { nonce: u64, sent_us: i64 },
    Pong { nonce: u64, sent_us: i64 },
    /// Viewer → host: envie um Ctrl+Alt+Del na máquina remota.
    ///
    /// Não é um `Input` comum, e não poderia ser. O Windows reserva essa
    /// combinação: `SendInput` não a gera, de propósito, porque é ela que
    /// garante ao usuário que a tela de senha é a verdadeira. Gerá-la exige
    /// `SendSAS`, que só o serviço pode chamar — daí a mensagem separada.
    RequestSAS,
    /// Início de uma transferência de arquivo: nome e tamanho total.
    ///
    /// Vai pelo canal de controle, que é confiável e ordenado — um pedaço
    /// perdido ou fora de ordem corromperia o arquivo em silêncio. O `nome` é
    /// reduzido ao nome-base pelo receptor antes de gravar: aceitar um caminho
    /// vindo da rede seria deixar o outro lado escrever onde quisesse.
    FileOffer { name: String, size: u64 },
    /// Um pedaço do arquivo em transferência, na ordem em que sai.
    FileChunk { data: Vec<u8> },
    /// Fim do arquivo. O receptor fecha o que estava gravando.
    FileEnd,
    /// Viewer → host: quero copiar um arquivo **daí**. Abra o seletor.
    ///
    /// O host abre o diálogo de arquivo na própria tela — que o operador já
    /// está vendo e controlando pelo viewer — e devolve o arquivo escolhido
    /// pelas mesmas mensagens `FileOffer`/`FileChunk`/`FileEnd`, agora no
    /// sentido host → viewer.
    RequestFile,
}

const TAG_HELLO: u8 = 0x01;
const TAG_VIDEO: u8 = 0x02;
const TAG_REQUEST_KEYFRAME: u8 = 0x03;
const TAG_INPUT: u8 = 0x04;
const TAG_PING: u8 = 0x05;
const TAG_PONG: u8 = 0x06;
const TAG_REQUEST_SAS: u8 = 0x07;
const TAG_FILE_OFFER: u8 = 0x08;
const TAG_FILE_CHUNK: u8 = 0x09;
const TAG_FILE_END: u8 = 0x0A;
const TAG_REQUEST_FILE: u8 = 0x0B;

const IN_MOUSE_MOVE: u8 = 0x01;
const IN_MOUSE_BUTTON: u8 = 0x02;
const IN_MOUSE_WHEEL: u8 = 0x03;
const IN_KEY: u8 = 0x04;

impl Message {
    /// Em que canal esta mensagem deve trafegar. Fica aqui, e não no ponto de
    /// envio, para que a resposta seja a mesma em todos eles.
    pub fn channel(&self) -> crate::Channel {
        match self {
            Message::Video { .. } => crate::Channel::Media,
            _ => crate::Channel::Control,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64);
        match self {
            Message::Hello { width, height, fps } => {
                b.push(TAG_HELLO);
                put_u32(&mut b, *width);
                put_u32(&mut b, *height);
                put_u32(&mut b, *fps);
            }
            Message::Video {
                seq,
                time_us,
                is_keyframe,
                data,
            } => {
                b.reserve(data.len() + 32);
                b.push(TAG_VIDEO);
                put_u64(&mut b, *seq);
                put_i64(&mut b, *time_us);
                b.push(u8::from(*is_keyframe));
                put_u32(&mut b, data.len() as u32);
                b.extend_from_slice(data);
            }
            Message::RequestKeyframe => b.push(TAG_REQUEST_KEYFRAME),
            Message::RequestSAS => b.push(TAG_REQUEST_SAS),
            Message::FileOffer { name, size } => {
                let nome = name.as_bytes();
                b.reserve(nome.len() + 16);
                b.push(TAG_FILE_OFFER);
                put_u32(&mut b, nome.len() as u32);
                b.extend_from_slice(nome);
                put_u64(&mut b, *size);
            }
            Message::FileChunk { data } => {
                b.reserve(data.len() + 8);
                b.push(TAG_FILE_CHUNK);
                put_u32(&mut b, data.len() as u32);
                b.extend_from_slice(data);
            }
            Message::FileEnd => b.push(TAG_FILE_END),
            Message::RequestFile => b.push(TAG_REQUEST_FILE),
            Message::Input(ev) => {
                b.push(TAG_INPUT);
                encode_input(&mut b, ev);
            }
            Message::Ping { nonce, sent_us } => {
                b.push(TAG_PING);
                put_u64(&mut b, *nonce);
                put_i64(&mut b, *sent_us);
            }
            Message::Pong { nonce, sent_us } => {
                b.push(TAG_PONG);
                put_u64(&mut b, *nonce);
                put_i64(&mut b, *sent_us);
            }
        }
        b
    }

    pub fn decode(buf: &[u8]) -> Result<Message, ProtoError> {
        let mut r = Reader::new(buf);
        let msg = match r.u8()? {
            TAG_HELLO => Message::Hello {
                width: r.u32()?,
                height: r.u32()?,
                fps: r.u32()?,
            },
            TAG_VIDEO => {
                let seq = r.u64()?;
                let time_us = r.i64()?;
                let is_keyframe = r.u8()? != 0;
                let len = r.u32()? as usize;
                Message::Video {
                    seq,
                    time_us,
                    is_keyframe,
                    data: r.bytes(len)?.to_vec(),
                }
            }
            TAG_REQUEST_KEYFRAME => Message::RequestKeyframe,
            TAG_REQUEST_SAS => Message::RequestSAS,
            TAG_FILE_OFFER => {
                let len = r.u32()? as usize;
                let nome = String::from_utf8_lossy(r.bytes(len)?).into_owned();
                Message::FileOffer {
                    name: nome,
                    size: r.u64()?,
                }
            }
            TAG_FILE_CHUNK => {
                let len = r.u32()? as usize;
                Message::FileChunk {
                    data: r.bytes(len)?.to_vec(),
                }
            }
            TAG_FILE_END => Message::FileEnd,
            TAG_REQUEST_FILE => Message::RequestFile,
            TAG_INPUT => Message::Input(decode_input(&mut r)?),
            TAG_PING => Message::Ping {
                nonce: r.u64()?,
                sent_us: r.i64()?,
            },
            TAG_PONG => Message::Pong {
                nonce: r.u64()?,
                sent_us: r.i64()?,
            },
            other => return Err(ProtoError::UnknownTag(other)),
        };

        // Bytes sobrando significam divergência de versão ou peer malicioso.
        // Aceitar em silêncio esconderia as duas coisas.
        if r.remaining() != 0 {
            return Err(ProtoError::TrailingBytes(r.remaining()));
        }
        Ok(msg)
    }
}

fn encode_input(b: &mut Vec<u8>, ev: &InputEvent) {
    match ev {
        InputEvent::MouseMove { x, y } => {
            b.push(IN_MOUSE_MOVE);
            put_i32(b, *x);
            put_i32(b, *y);
        }
        InputEvent::MouseButton { button, down } => {
            b.push(IN_MOUSE_BUTTON);
            b.push(match button {
                MouseButton::Left => 0,
                MouseButton::Right => 1,
                MouseButton::Middle => 2,
            });
            b.push(u8::from(*down));
        }
        InputEvent::MouseWheel { delta } => {
            b.push(IN_MOUSE_WHEEL);
            put_i32(b, *delta);
        }
        InputEvent::Key { scancode, down } => {
            b.push(IN_KEY);
            put_u32(b, *scancode);
            b.push(u8::from(*down));
        }
    }
}

fn decode_input(r: &mut Reader<'_>) -> Result<InputEvent, ProtoError> {
    Ok(match r.u8()? {
        IN_MOUSE_MOVE => InputEvent::MouseMove {
            x: r.i32()?,
            y: r.i32()?,
        },
        IN_MOUSE_BUTTON => {
            let button = match r.u8()? {
                0 => MouseButton::Left,
                1 => MouseButton::Right,
                2 => MouseButton::Middle,
                other => return Err(ProtoError::UnknownTag(other)),
            };
            InputEvent::MouseButton {
                button,
                down: r.u8()? != 0,
            }
        }
        IN_MOUSE_WHEEL => InputEvent::MouseWheel { delta: r.i32()? },
        IN_KEY => InputEvent::Key {
            scancode: r.u32()?,
            down: r.u8()? != 0,
        },
        other => return Err(ProtoError::UnknownTag(other)),
    })
}

fn put_u32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_i32(b: &mut Vec<u8>, v: i32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_u64(b: &mut Vec<u8>, v: u64) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put_i64(b: &mut Vec<u8>, v: i64) {
    b.extend_from_slice(&v.to_le_bytes());
}

/// Leitor que trata falta de bytes como erro, nunca como pânico. Toda entrada
/// aqui vem da rede.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ProtoError> {
        if n > MAX_PAYLOAD {
            return Err(ProtoError::BadLength(n));
        }
        if self.remaining() < n {
            return Err(ProtoError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, ProtoError> {
        Ok(self.bytes(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, ProtoError> {
        Ok(i32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, ProtoError> {
        Ok(i64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: Message) {
        let encoded = m.encode();
        assert_eq!(Message::decode(&encoded), Ok(m));
    }

    #[test]
    fn roundtrip_todas_as_mensagens() {
        roundtrip(Message::Hello {
            width: 1920,
            height: 1080,
            fps: 30,
        });
        roundtrip(Message::Video {
            seq: u64::MAX,
            time_us: -1,
            is_keyframe: true,
            data: vec![0, 1, 2, 250],
        });
        roundtrip(Message::RequestKeyframe);
        roundtrip(Message::RequestSAS);
        roundtrip(Message::FileOffer {
            name: "relatório final.pdf".into(),
            size: 1_234_567,
        });
        roundtrip(Message::FileChunk {
            data: vec![0, 1, 2, 250, 255],
        });
        roundtrip(Message::FileEnd);
        roundtrip(Message::RequestFile);
        roundtrip(Message::Ping {
            nonce: 7,
            sent_us: 123456789,
        });
        roundtrip(Message::Pong {
            nonce: 7,
            sent_us: 123456789,
        });
    }

    #[test]
    fn roundtrip_eventos_de_entrada() {
        roundtrip(Message::Input(InputEvent::MouseMove { x: -5, y: 1079 }));
        roundtrip(Message::Input(InputEvent::MouseButton {
            button: MouseButton::Middle,
            down: true,
        }));
        roundtrip(Message::Input(InputEvent::MouseWheel { delta: -240 }));
        roundtrip(Message::Input(InputEvent::Key {
            scancode: 0x1C,
            down: false,
        }));
    }

    #[test]
    fn entrada_truncada_nao_causa_panico() {
        let full = Message::Hello {
            width: 1,
            height: 2,
            fps: 3,
        }
        .encode();
        // Todo prefixo próprio precisa falhar de forma limpa.
        for n in 0..full.len() {
            assert!(Message::decode(&full[..n]).is_err(), "prefixo de {n} bytes");
        }
    }

    #[test]
    fn comprimento_absurdo_e_rejeitado_sem_alocar() {
        let mut b = vec![TAG_VIDEO];
        b.extend_from_slice(&0u64.to_le_bytes());
        b.extend_from_slice(&0i64.to_le_bytes());
        b.push(0);
        // Declara 4 GB de payload num buffer de poucos bytes.
        b.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            Message::decode(&b),
            Err(ProtoError::BadLength(_)) | Err(ProtoError::Truncated)
        ));
    }

    #[test]
    fn bytes_sobrando_sao_rejeitados() {
        let mut b = Message::RequestKeyframe.encode();
        b.push(0xFF);
        assert_eq!(Message::decode(&b), Err(ProtoError::TrailingBytes(1)));
    }

    #[test]
    fn tag_desconhecida_e_rejeitada() {
        assert_eq!(Message::decode(&[0x7F]), Err(ProtoError::UnknownTag(0x7F)));
        assert_eq!(Message::decode(&[]), Err(ProtoError::Truncated));
    }
}
