//! Transporte QUIC — a troca prevista pela decisão D4.
//!
//! # Por que sair do TCP
//!
//! O TCP entrega em ordem estrita. Um único pacote de vídeo perdido **segura
//! tudo que veio depois** até a retransmissão chegar: a tela inteira congela
//! por um tempo de ida e volta. Numa LAN sem perda isso nunca aparece — e foi
//! por isso que a medição da etapa 3, feita em 127.0.0.1, não flagrou nada.
//! Na internet aparece o tempo todo.
//!
//! # Como o QUIC resolve
//!
//! Fluxos QUIC são independentes entre si: perda em um não atrasa os outros.
//! O mapeamento fica direto sobre o [`Channel`]:
//!
//! - **Controle** — um fluxo bidirecional, confiável e ordenado, vivo pela
//!   sessão inteira. Entrada e handshake não podem se perder nem trocar de
//!   ordem.
//! - **Mídia** — **um fluxo unidirecional por quadro**. Cada quadro é
//!   confiável em si, mas independente dos demais: um quadro atrasado não
//!   segura o próximo. É a semântica que o canal de mídia sempre quis e que o
//!   TCP não sabia dar.
//!
//! # Sobre a verificação de certificado
//!
//! O cliente **não** valida o certificado do servidor, e isso é deliberado: a
//! identidade real vem da camada Noise acima (decisão D7), com autenticação
//! por senha e fixação de impressão digital. Uma PKI aqui autenticaria um nome
//! de domínio que não existe — as máquinas se identificam por chave pública,
//! não por DNS. O TLS do QUIC serve só para cifrar o transporte.
//!
//! **Isto só é seguro porque o Noise está por cima.** Usar `QuicTransport`
//! sem ele deixa a sessão aberta a intermediação.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use quinn::{ClientConfig, Connection, Endpoint, ServerConfig, TransportConfig};
use tokio::sync::mpsc::UnboundedSender;

use crate::proto::MAX_PAYLOAD;
use crate::transport::{Channel, Transport};

/// Sem tráfego por este tempo, a conexão é considerada morta.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// Mantém a associação NAT viva quando a tela está parada.
const KEEP_ALIVE: Duration = Duration::from_secs(2);

pub struct QuicTransport {
    to_io: UnboundedSender<Outgoing>,
    from_io: Receiver<(Channel, Vec<u8>)>,
    pending: Arc<AtomicUsize>,
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
    remote: SocketAddr,
    /// Mantém a thread de E/S viva enquanto o transporte existir.
    _runtime: Arc<RuntimeGuard>,
}

enum Outgoing {
    Control(Vec<u8>),
    Media(Vec<u8>),
}

struct RuntimeGuard {
    shutdown: UnboundedSender<Outgoing>,
}

impl Drop for RuntimeGuard {
    fn drop(&mut self) {
        // Fechar o canal faz o laço de E/S terminar sozinho.
        let _ = &self.shutdown;
    }
}

/// Ponto de escuta. Separado de [`QuicTransport`] para que o endereço
/// efetivamente vinculado possa ser lido antes de alguém conectar — útil com
/// porta efêmera.
pub struct QuicListener {
    endpoint: Endpoint,
    rt: tokio::runtime::Runtime,
}

impl QuicListener {
    pub fn bind(addr: SocketAddr) -> io::Result<Self> {
        let rt = runtime()?;
        // O `Endpoint` do quinn registra o próprio driver no runtime ativo, e
        // sem entrar nele antes a construção falha com "no async runtime
        // found".
        let endpoint = {
            let _guard = rt.enter();
            server_endpoint(addr)?
        };
        Ok(Self { endpoint, rt })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Bloqueia até alguém conectar.
    pub fn accept(self) -> io::Result<QuicTransport> {
        let endpoint = self.endpoint;
        let conn = self.rt.block_on(async {
            let incoming = endpoint
                .accept()
                .await
                .ok_or_else(|| other("endpoint fechado antes de qualquer conexão"))?;
            incoming.await.map_err(other)
        })?;
        QuicTransport::spawn(self.rt, conn, true)
    }
}

impl QuicTransport {

    pub fn connect(remote: SocketAddr) -> io::Result<Self> {
        let bind: SocketAddr = if remote.is_ipv4() {
            "0.0.0.0:0".parse().unwrap()
        } else {
            "[::]:0".parse().unwrap()
        };
        let rt = runtime()?;
        let endpoint = {
            let _guard = rt.enter();
            client_endpoint(bind)?
        };
        let conn = rt.block_on(async {
            endpoint
                .connect(remote, "pcshare")
                .map_err(other)?
                .await
                .map_err(other)
        })?;
        Self::spawn(rt, conn, false)
    }

    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    fn spawn(rt: tokio::runtime::Runtime, conn: Connection, server: bool) -> io::Result<Self> {
        let remote = conn.remote_address();
        let (to_io, mut rx_io) = tokio::sync::mpsc::unbounded_channel::<Outgoing>();
        let (tx_app, from_io) = std::sync::mpsc::channel::<(Channel, Vec<u8>)>();

        let pending = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(AtomicU64::new(0));
        let received = Arc::new(AtomicU64::new(0));

        let (p, s, r) = (pending.clone(), sent.clone(), received.clone());
        let (shutdown_tx, _shutdown_rx) = tokio::sync::mpsc::unbounded_channel::<Outgoing>();

        std::thread::Builder::new()
            .name("pcshare-quic".into())
            .spawn(move || {
                rt.block_on(async move {
                    // Quem aceita abre o fluxo de controle; quem conecta espera.
                    // Alguém precisa começar, e fixar o papel evita corrida.
                    let (mut ctrl_tx, mut ctrl_rx) = if server {
                        match conn.open_bi().await {
                            Ok(v) => v,
                            Err(_) => return,
                        }
                    } else {
                        match conn.accept_bi().await {
                            Ok(v) => v,
                            Err(_) => return,
                        }
                    };
                    // O fluxo bidirecional só existe do outro lado depois do
                    // primeiro byte.
                    if server && ctrl_tx.write_all(&[0u8]).await.is_err() {
                        return;
                    }
                    if !server {
                        let mut hello = [0u8; 1];
                        if read_exact(&mut ctrl_rx, &mut hello).await.is_err() {
                            return;
                        }
                    }

                    let conn_recv = conn.clone();
                    let tx_uni = tx_app.clone();
                    let r_uni = r.clone();
                    // Cada quadro chega no seu próprio fluxo; ler cada um numa
                    // tarefa é o que mantém a independência entre eles.
                    tokio::spawn(async move {
                        while let Ok(mut uni) = conn_recv.accept_uni().await {
                            let tx = tx_uni.clone();
                            let rc = r_uni.clone();
                            tokio::spawn(async move {
                                if let Ok(data) = uni.read_to_end(MAX_PAYLOAD).await {
                                    rc.fetch_add(data.len() as u64, Ordering::Relaxed);
                                    let _ = tx.send((Channel::Media, data));
                                }
                            });
                        }
                    });

                    let tx_ctrl = tx_app.clone();
                    let r_ctrl = r.clone();
                    tokio::spawn(async move {
                        loop {
                            let mut len = [0u8; 4];
                            if read_exact(&mut ctrl_rx, &mut len).await.is_err() {
                                return;
                            }
                            let n = u32::from_le_bytes(len) as usize;
                            if n > MAX_PAYLOAD {
                                return;
                            }
                            let mut buf = vec![0u8; n];
                            if read_exact(&mut ctrl_rx, &mut buf).await.is_err() {
                                return;
                            }
                            r_ctrl.fetch_add((n + 4) as u64, Ordering::Relaxed);
                            if tx_ctrl.send((Channel::Control, buf)).is_err() {
                                return;
                            }
                        }
                    });

                    while let Some(msg) = rx_io.recv().await {
                        match msg {
                            Outgoing::Control(data) => {
                                let mut framed =
                                    Vec::with_capacity(data.len() + 4);
                                framed.extend_from_slice(
                                    &(data.len() as u32).to_le_bytes(),
                                );
                                framed.extend_from_slice(&data);
                                if ctrl_tx.write_all(&framed).await.is_err() {
                                    return;
                                }
                                s.fetch_add(framed.len() as u64, Ordering::Relaxed);
                                p.fetch_sub(data.len(), Ordering::Relaxed);
                            }
                            Outgoing::Media(data) => {
                                let conn = conn.clone();
                                let s = s.clone();
                                let p = p.clone();
                                // Um fluxo por quadro, escrito em paralelo:
                                // é o que impede um quadro grande de segurar
                                // o seguinte.
                                tokio::spawn(async move {
                                    if let Ok(mut uni) = conn.open_uni().await {
                                        let n = data.len();
                                        if uni.write_all(&data).await.is_ok() {
                                            let _ = uni.finish();
                                            s.fetch_add(n as u64, Ordering::Relaxed);
                                        }
                                        p.fetch_sub(n, Ordering::Relaxed);
                                    } else {
                                        p.fetch_sub(data.len(), Ordering::Relaxed);
                                    }
                                });
                            }
                        }
                    }
                });
            })?;

        Ok(Self {
            to_io,
            from_io,
            pending,
            sent,
            received,
            remote,
            _runtime: Arc::new(RuntimeGuard {
                shutdown: shutdown_tx,
            }),
        })
    }
}

impl Transport for QuicTransport {
    fn send(&mut self, channel: Channel, payload: &[u8]) -> io::Result<()> {
        if payload.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("payload de {} bytes excede o limite", payload.len()),
            ));
        }
        self.pending.fetch_add(payload.len(), Ordering::Relaxed);
        let msg = match channel {
            Channel::Control => Outgoing::Control(payload.to_vec()),
            Channel::Media => Outgoing::Media(payload.to_vec()),
        };
        self.to_io
            .send(msg)
            .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn poll(&mut self) -> io::Result<Option<(Channel, Vec<u8>)>> {
        match self.from_io.try_recv() {
            Ok(v) => Ok(Some(v)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        // A thread de E/S escreve por conta própria; não há o que empurrar.
        Ok(())
    }

    fn pending_out(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    fn bytes_sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    fn bytes_received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }
}

async fn read_exact(rx: &mut quinn::RecvStream, buf: &mut [u8]) -> io::Result<()> {
    rx.read_exact(buf).await.map_err(other)
}

/// Fixa o provedor criptográfico do rustls.
///
/// Com mais de um provedor compilado, o rustls se recusa a escolher sozinho e
/// entra em pânico no primeiro uso. Escolher aqui, uma vez, evita que a
/// decisão dependa de qual dependência puxou qual feature.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn runtime() -> io::Result<tokio::runtime::Runtime> {
    ensure_crypto_provider();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
}

fn transport_config() -> Arc<TransportConfig> {
    let mut cfg = TransportConfig::default();
    cfg.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("timeout válido")));
    cfg.keep_alive_interval(Some(KEEP_ALIVE));
    Arc::new(cfg)
}

fn server_endpoint(bind: SocketAddr) -> io::Result<Endpoint> {
    let cert = rcgen::generate_simple_self_signed(vec!["pcshare".into()]).map_err(other)?;
    let cert_der = cert.cert.der().to_vec();
    let key_der = cert.signing_key.serialize_der();

    let mut server_config = ServerConfig::with_single_cert(
        vec![cert_der.into()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(key_der).into(),
    )
    .map_err(other)?;
    server_config.transport_config(transport_config());

    let endpoint = Endpoint::server(server_config, bind)?;
    Ok(endpoint)
}

fn client_endpoint(bind: SocketAddr) -> io::Result<Endpoint> {
    let mut endpoint = Endpoint::client(bind)?;

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_no_client_auth();

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(other)?,
    ));
    client_config.transport_config(transport_config());
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

/// Aceita qualquer certificado.
///
/// Não é descuido: ver a nota sobre verificação no topo do módulo. A
/// identidade da máquina é a chave pública autenticada pelo Noise, não um
/// nome de domínio. **Exige a camada Noise por cima.**
#[derive(Debug)]
struct AcceptAnyServer;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServer {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::CryptoProvider::get_default()
            .map(|p| p.signature_verification_algorithms.supported_schemes())
            .unwrap_or_default()
    }
}

fn other(e: impl ToString) -> io::Error {
    io::Error::other(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn par() -> (QuicTransport, QuicTransport) {
        let listener = QuicListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || listener.accept().unwrap());
        let client = QuicTransport::connect(addr).unwrap();
        (client, server.join().unwrap())
    }

    fn wait(t: &mut QuicTransport) -> (Channel, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = t.poll().expect("poll falhou") {
                return v;
            }
            assert!(Instant::now() < deadline, "nada chegou em 10s");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn controle_e_midia_atravessam() {
        let (mut a, mut b) = par();
        a.send(Channel::Control, b"comando").unwrap();
        assert_eq!(wait(&mut b), (Channel::Control, b"comando".to_vec()));

        a.send(Channel::Media, b"quadro").unwrap();
        assert_eq!(wait(&mut b), (Channel::Media, b"quadro".to_vec()));
    }

    #[test]
    fn ambos_os_sentidos_funcionam() {
        let (mut a, mut b) = par();
        a.send(Channel::Control, b"ida").unwrap();
        assert_eq!(wait(&mut b).1, b"ida".to_vec());
        b.send(Channel::Control, b"volta").unwrap();
        assert_eq!(wait(&mut a).1, b"volta".to_vec());
    }

    /// Um keyframe tem centenas de KB e precisa atravessar inteiro.
    #[test]
    fn quadro_grande_atravessa_intacto() {
        let (mut a, mut b) = par();
        let grande: Vec<u8> = (0..400_000u32).map(|i| (i % 251) as u8).collect();
        a.send(Channel::Media, &grande).unwrap();

        let (canal, recebido) = wait(&mut b);
        assert_eq!(canal, Channel::Media);
        assert_eq!(recebido.len(), grande.len());
        assert_eq!(recebido, grande);
    }

    /// O ponto da mudança: o canal de controle não pode esperar a mídia.
    ///
    /// Sobre TCP, um quadro grande à frente na fila seguraria o comando atrás
    /// dele. Com fluxos QUIC independentes, o comando passa na frente.
    #[test]
    fn controle_nao_espera_a_midia() {
        let (mut a, mut b) = par();
        let grande = vec![0xAB; 2_000_000];
        a.send(Channel::Media, &grande).unwrap();
        a.send(Channel::Control, b"clique").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some((canal, dados)) = b.poll().unwrap() {
                if canal == Channel::Control {
                    assert_eq!(dados, b"clique".to_vec());
                    return;
                }
                panic!("a mídia chegou primeiro — sem independência de fluxos");
            }
            assert!(Instant::now() < deadline, "controle nunca chegou");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
