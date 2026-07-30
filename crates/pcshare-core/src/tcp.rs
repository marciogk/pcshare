//! Transporte TCP para o Sprint 0.
//!
//! Descartável por construção: existe para tirar a rede da equação enquanto
//! medimos captura, encode e latência. A Fase 2 troca por WebRTC.
//!
//! Vale registrar por que TCP é a escolha errada para o produto final, já que
//! o número que ele produzir vai parecer bom: sob perda de pacotes, o TCP
//! retransmite e **segura tudo que veio depois** até o pacote perdido chegar.
//! Numa LAN sem perda isso nunca aparece. Na internet, um único pacote de
//! vídeo perdido congela a tela inteira por um RTT — exatamente o sintoma que
//! o canal [`Channel::Media`](crate::Channel::Media) existe para evitar.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};

use crate::proto::MAX_PAYLOAD;
use crate::transport::{Channel, Transport};

/// `[u8 canal][u32 comprimento]`
const HEADER: usize = 5;

/// Acima disto, compacta o buffer de entrada em vez de deixá-lo crescer.
const COMPACT_THRESHOLD: usize = 1024 * 1024;

pub struct TcpTransport {
    stream: TcpStream,
    peer: SocketAddr,
    in_buf: Vec<u8>,
    /// Quanto de `in_buf` já foi consumido. Evita um `drain` por mensagem,
    /// que seria O(n²) num fluxo de vídeo.
    in_head: usize,
    out_buf: Vec<u8>,
    bytes_sent: u64,
    bytes_received: u64,
}

impl TcpTransport {
    pub fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        Self::from_stream(stream)
    }

    /// Aceita uma conexão. Bloqueia até alguém conectar.
    pub fn accept(listener: &TcpListener) -> io::Result<Self> {
        let (stream, _) = listener.accept()?;
        Self::from_stream(stream)
    }

    pub fn from_stream(stream: TcpStream) -> io::Result<Self> {
        // Sem isto o Nagle segura pacotes pequenos esperando companhia,
        // acrescentando dezenas de milissegundos justamente aos eventos de
        // mouse e teclado, que são pequenos e urgentes.
        stream.set_nodelay(true)?;
        stream.set_nonblocking(true)?;
        let peer = stream.peer_addr()?;
        Ok(Self {
            stream,
            peer,
            in_buf: Vec::with_capacity(256 * 1024),
            in_head: 0,
            out_buf: Vec::with_capacity(256 * 1024),
            bytes_sent: 0,
            bytes_received: 0,
        })
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// Bytes ainda não aceitos pelo sistema operacional. Crescendo sem parar,
    /// significa que estamos produzindo vídeo mais rápido que o link escoa.
    pub fn pending_out(&self) -> usize {
        self.out_buf.len()
    }

    fn try_write(&mut self) -> io::Result<()> {
        while !self.out_buf.is_empty() {
            match self.stream.write(&self.out_buf) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => {
                    self.out_buf.drain(..n);
                    self.bytes_sent += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Extrai uma mensagem completa do buffer, se houver.
    fn take_frame(&mut self) -> io::Result<Option<(Channel, Vec<u8>)>> {
        let avail = &self.in_buf[self.in_head..];
        if avail.len() < HEADER {
            return Ok(None);
        }

        let channel = match avail[0] {
            0 => Channel::Control,
            1 => Channel::Media,
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("canal desconhecido: {other}"),
                ));
            }
        };
        let len = u32::from_le_bytes(avail[1..5].try_into().unwrap()) as usize;

        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame de {len} bytes excede o limite"),
            ));
        }
        if avail.len() < HEADER + len {
            return Ok(None);
        }

        let payload = avail[HEADER..HEADER + len].to_vec();
        self.in_head += HEADER + len;

        if self.in_head == self.in_buf.len() {
            self.in_buf.clear();
            self.in_head = 0;
        } else if self.in_head > COMPACT_THRESHOLD {
            self.in_buf.drain(..self.in_head);
            self.in_head = 0;
        }

        Ok(Some((channel, payload)))
    }
}

impl Transport for TcpTransport {
    fn send(&mut self, channel: Channel, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("payload de {} bytes excede o limite", payload.len()),
            ));
        }
        self.out_buf.push(match channel {
            Channel::Control => 0,
            Channel::Media => 1,
        });
        self.out_buf
            .extend_from_slice(&(payload.len() as u32).to_le_bytes());
        self.out_buf.extend_from_slice(payload);
        self.try_write()
    }

    fn poll(&mut self) -> io::Result<Option<(Channel, Vec<u8>)>> {
        self.try_write()?;

        loop {
            if let Some(frame) = self.take_frame()? {
                return Ok(Some(frame));
            }

            let mut tmp = [0u8; 64 * 1024];
            match self.stream.read(&mut tmp) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(n) => {
                    self.in_buf.extend_from_slice(&tmp[..n]);
                    self.bytes_received += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.try_write()
    }

    fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    fn bytes_received(&self) -> u64 {
        self.bytes_received
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Message;
    use std::time::{Duration, Instant};

    /// Espera uma mensagem chegar. `poll` não bloqueia, então o teste precisa
    /// insistir — com prazo, para nunca travar a suíte.
    fn wait(t: &mut TcpTransport) -> (Channel, Vec<u8>) {
        wait_pumping(t, None)
    }

    /// Idem, mas também bombeia o remetente.
    ///
    /// Necessário sempre que o volume enviado passa do buffer do socket: o
    /// `send` deixa o excedente pendente, e sem novas chamadas a `flush` ele
    /// nunca sai. Esquecer isso trava as duas pontas — o receptor esperando
    /// bytes que o remetente ainda tem na memória.
    fn wait_pumping(rx: &mut TcpTransport, tx: Option<&mut TcpTransport>) -> (Channel, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut tx = tx;
        loop {
            if let Some(t) = tx.as_deref_mut() {
                t.flush().expect("flush falhou");
            }
            if let Some(v) = rx.poll().expect("poll falhou") {
                return v;
            }
            assert!(Instant::now() < deadline, "nada chegou em 5s");
            std::thread::yield_now();
        }
    }

    fn par() -> (TcpTransport, TcpTransport) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || TcpTransport::accept(&listener).unwrap());
        let client = TcpTransport::connect(addr).unwrap();
        (client, server.join().unwrap())
    }

    #[test]
    fn mensagem_atravessa_o_loopback() {
        let (mut a, mut b) = par();
        let msg = Message::Hello {
            width: 1360,
            height: 768,
            fps: 30,
        };
        a.send(msg.channel(), &msg.encode()).unwrap();

        let (channel, payload) = wait(&mut b);
        assert_eq!(channel, Channel::Control);
        assert_eq!(Message::decode(&payload).unwrap(), msg);
    }

    #[test]
    fn preserva_ordem_e_limites_das_mensagens() {
        let (mut a, mut b) = par();
        // Tamanhos variados no mesmo fluxo: o receptor precisa reconstruir os
        // limites sem depender de como o TCP fatiou os segmentos.
        for seq in 0..64u64 {
            let m = Message::Video {
                seq,
                time_us: seq as i64 * 1000,
                is_keyframe: seq == 0,
                data: vec![(seq & 0xFF) as u8; (seq as usize * 977) % 40_000],
            };
            a.send(m.channel(), &m.encode()).unwrap();
        }
        a.flush().unwrap();

        for expected in 0..64u64 {
            let (channel, payload) = wait_pumping(&mut b, Some(&mut a));
            assert_eq!(channel, Channel::Media);
            match Message::decode(&payload).unwrap() {
                Message::Video { seq, data, .. } => {
                    assert_eq!(seq, expected);
                    assert_eq!(data.len(), (expected as usize * 977) % 40_000);
                }
                other => panic!("esperava Video, veio {other:?}"),
            }
        }
    }

    #[test]
    fn canais_diferentes_chegam_rotulados() {
        let (mut a, mut b) = par();
        a.send(Channel::Media, b"video").unwrap();
        a.send(Channel::Control, b"input").unwrap();

        assert_eq!(wait(&mut b), (Channel::Media, b"video".to_vec()));
        assert_eq!(wait(&mut b), (Channel::Control, b"input".to_vec()));
    }

    #[test]
    fn poll_nao_bloqueia_quando_nao_ha_nada() {
        let (_a, mut b) = par();
        assert_eq!(b.poll().unwrap(), None);
    }

    /// Trava esta propriedade: quando o receptor não drena, o excedente fica
    /// visível em `pending_out` em vez de bloquear o remetente. É o sinal que
    /// o host vai usar para saber que está produzindo vídeo mais rápido do
    /// que o link escoa — e que precisa descartar frames em vez de acumular.
    #[test]
    fn excedente_fica_pendente_sem_bloquear() {
        let (mut a, mut b) = par();
        let big = vec![0u8; 512 * 1024];
        for _ in 0..8 {
            a.send(Channel::Media, &big).unwrap();
        }
        assert!(
            a.pending_out() > 0,
            "4 MB deveriam exceder o buffer do socket"
        );

        // E escoam assim que o receptor consome e o remetente é bombeado.
        let mut received = 0;
        let deadline = Instant::now() + Duration::from_secs(10);
        while received < 8 {
            a.flush().unwrap();
            while let Some((_, p)) = b.poll().unwrap() {
                assert_eq!(p.len(), big.len());
                received += 1;
            }
            assert!(Instant::now() < deadline, "escoou só {received} de 8");
        }
        assert_eq!(a.pending_out(), 0);
    }
}
