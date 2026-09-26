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

/// Sem tráfego por este tempo, a conexão é considerada morta. Baixo de propósito
/// (com folga sobre o keep-alive de 2 s): é o que faz o host **perceber que o
/// viewer saiu** e voltar a se registrar rápido, para a reconexão não falhar por
/// segundos a fio. Um viewer que fecha sem aviso é notado em ~5 s, não em 10.
const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Mantém a associação NAT viva quando a tela está parada.
const KEEP_ALIVE: Duration = Duration::from_secs(2);

pub struct QuicTransport {
    to_io: UnboundedSender<Outgoing>,
    from_io: Receiver<(Channel, Vec<u8>)>,
    pending: Arc<AtomicUsize>,
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
    remote: SocketAddr,
    /// Mantém o runtime vivo enquanto o transporte existir. Quando o último
    /// `QuicTransport` e o `QuicListener` somem, o runtime é derrubado junto.
    _runtime: Arc<tokio::runtime::Runtime>,
}

enum Outgoing {
    Control(Vec<u8>),
    Media(Vec<u8>),
}

/// Ponto de escuta. Separado de [`QuicTransport`] para que o endereço
/// efetivamente vinculado possa ser lido antes de alguém conectar — útil com
/// porta efêmera.
pub struct QuicListener {
    endpoint: Endpoint,
    /// Compartilhado com cada sessão aceita. A conexão do quinn é conduzida
    /// pelo runtime do `Endpoint` que a originou; usar outro faria a sessão
    /// morrer assim que o primeiro parasse de ser conduzido.
    rt: Arc<tokio::runtime::Runtime>,
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
        Ok(Self {
            endpoint,
            rt: Arc::new(rt),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Bloqueia até alguém conectar.
    ///
    /// Recebe `&self` de propósito: o host precisa voltar a escutar quando uma
    /// sessão termina. A versão anterior consumia o listener, e por isso o
    /// programa aceitava **uma única conexão** na vida — se ela caísse, só
    /// reiniciando o processo na máquina remota, justamente a que o usuário
    /// não alcança.
    /// Só retorna com uma sessão pronta, ou com erro de fato fatal.
    ///
    /// Tentativa de conexão que não completa o handshake **não** é fatal e não
    /// pode chegar ao chamador: pacotes tardios de uma conexão encerrada
    /// chegam a criar tentativas que nunca completam, e propagar isso derrubava
    /// o host logo depois de uma sessão normal terminar. Observado em teste
    /// entre duas máquinas.
    pub fn accept(&self) -> io::Result<QuicTransport> {
        loop {
            let endpoint = self.endpoint.clone();
            let resultado = self.rt.block_on(async move {
                let incoming = endpoint.accept().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotConnected, "endpoint fechado")
                })?;
                incoming.await.map_err(other)
            });

            match resultado {
                Ok(conn) => return QuicTransport::spawn(self.rt.clone(), conn, true),
                // O endpoint acabou: não há mais o que escutar.
                Err(e) if e.kind() == io::ErrorKind::NotConnected => return Err(e),
                // Tentativa abortada. Segue escutando.
                Err(_) => continue,
            }
        }
    }
}

/// Um endpoint reutilizado para **duas** conexões ao mesmo tempo: a do servidor
/// de encontro e a **direta** com o outro lado (furo de NAT).
///
/// O ponto todo é que as duas saem do **mesmo socket** — logo, da mesma porta
/// local. O NAT dá a essa porta um único mapeamento externo, o mesmo que o
/// encontro observou; então, quando os dois lados furam um ao outro, os pacotes
/// caem no mapeamento certo e a conexão direta se forma. Um endpoint por
/// conexão (como faz [`QuicTransport::connect`]) usaria portas diferentes e o
/// furo nunca casaria.
pub struct QuicEndpoint {
    endpoint: Endpoint,
    rt: Arc<tokio::runtime::Runtime>,
}

impl QuicEndpoint {
    /// Endpoint **só-cliente** (o viewer): conecta ao encontro e ao host, mas
    /// não aceita — o que também impede o *connect* de furo do host de virar
    /// uma segunda conexão.
    pub fn abrir_cliente() -> io::Result<Self> {
        Self::abrir(false)
    }

    /// Endpoint **cliente + servidor** (o host): conecta ao encontro, aceita a
    /// conexão direta do viewer, e ainda conecta ao viewer só para furar o NAT.
    pub fn abrir_servidor() -> io::Result<Self> {
        Self::abrir(true)
    }

    fn abrir(servidor: bool) -> io::Result<Self> {
        let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let rt = runtime()?;
        let endpoint = {
            let _guard = rt.enter();
            if servidor {
                let mut ep = server_endpoint(bind)?;
                ep.set_default_client_config(client_config()?);
                ep
            } else {
                client_endpoint(bind)?
            }
        };
        Ok(Self {
            endpoint,
            rt: Arc::new(rt),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }

    /// Conecta a `remote`, desistindo depois de `prazo`. Serve para o encontro
    /// (VPS) e para o viewer→host direto.
    pub fn conectar(&self, remote: SocketAddr, prazo: Duration) -> io::Result<QuicTransport> {
        let ep = self.endpoint.clone();
        let conn = self.rt.block_on(async move {
            tokio::time::timeout(prazo, async move {
                ep.connect(remote, "pcshare").map_err(other)?.await.map_err(other)
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "conexão não completou no prazo"))?
        })?;
        QuicTransport::spawn(self.rt.clone(), conn, false)
    }

    /// Host: abre o próprio NAT com um *connect* descartável a `peer` (o viewer
    /// é só-cliente e não aceita, então esse connect falha sozinho — serve só de
    /// soco no NAT) e, ao mesmo tempo, **aceita** a conexão real do viewer, com
    /// `prazo`.
    pub fn aceitar_furando(&self, peer: SocketAddr, prazo: Duration) -> io::Result<QuicTransport> {
        // Soco no NAT, em segundo plano.
        let ep_soco = self.endpoint.clone();
        self.rt.spawn(async move {
            if let Ok(c) = ep_soco.connect(peer, "pcshare") {
                let _ = c.await; // não completa; só os pacotes de saída importam
            }
        });

        let ep = self.endpoint.clone();
        let conn = self.rt.block_on(async move {
            tokio::time::timeout(prazo, async move {
                let inc = ep
                    .accept()
                    .await
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "endpoint fechado"))?;
                inc.await.map_err(other)
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ninguém conectou no prazo"))?
        })?;
        QuicTransport::spawn(self.rt.clone(), conn, true)
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
        Self::spawn(Arc::new(rt), conn, false)
    }

    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    fn spawn(
        rt: Arc<tokio::runtime::Runtime>,
        conn: Connection,
        server: bool,
    ) -> io::Result<Self> {
        let remote = conn.remote_address();
        let (to_io, mut rx_io) = tokio::sync::mpsc::unbounded_channel::<Outgoing>();
        let (tx_app, from_io) = std::sync::mpsc::channel::<(Channel, Vec<u8>)>();

        let pending = Arc::new(AtomicUsize::new(0));
        let sent = Arc::new(AtomicU64::new(0));
        let received = Arc::new(AtomicU64::new(0));

        let (p, s, r) = (pending.clone(), sent.clone(), received.clone());

        // Tarefa no runtime compartilhado, não uma thread própria com
        // `block_on`. É o que permite várias sessões ao longo do tempo sobre o
        // mesmo `Endpoint`.
        rt.spawn(async move {
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

                    // Sem isto o host não percebe o viewer sumir.
                    //
                    // As duas tarefas de leitura acima terminam quando a
                    // conexão cai, e cada uma solta a sua cópia do remetente.
                    // Mas se esta tarefa segurar a original, o canal nunca
                    // fecha: `poll` devolve "nada novo" para sempre e o host
                    // segue capturando e codificando para ninguém, sem nunca
                    // voltar a escutar. Observado travando exatamente assim.
                    drop(tx_app);

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

        Ok(Self {
            to_io,
            from_io,
            pending,
            sent,
            received,
            remote,
            _runtime: rt,
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

/// Runtime com threads próprias, e não `current_thread`.
///
/// Um runtime de thread única só avança tarefas enquanto alguém está dentro de
/// `block_on`. Como o mesmo runtime passou a ser compartilhado entre o ponto de
/// escuta e as sessões, as tarefas de E/S precisam continuar rodando **entre**
/// os `accept` — senão a sessão em curso congelaria toda vez que o host
/// voltasse a escutar.
fn runtime() -> io::Result<tokio::runtime::Runtime> {
    ensure_crypto_provider();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("pcshare-quic")
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

fn client_config() -> io::Result<ClientConfig> {
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyServer))
        .with_no_client_auth();

    let mut client_config = ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).map_err(other)?,
    ));
    client_config.transport_config(transport_config());
    Ok(client_config)
}

fn client_endpoint(bind: SocketAddr) -> io::Result<Endpoint> {
    let mut endpoint = Endpoint::client(bind)?;
    endpoint.set_default_client_config(client_config()?);
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

    /// O defeito observado no teste entre duas máquinas: o host aceitava uma
    /// conexão e parava de escutar. Se a sessão caísse, só reiniciando o
    /// processo na máquina remota — justamente a que o usuário não alcança.
    #[test]
    fn listener_aceita_varias_sessoes_em_sequencia() {
        let listener = QuicListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();

        for rodada in 1..=3u8 {
            // O handshake exige as duas pontas ao mesmo tempo.
            let conectando = std::thread::spawn(move || QuicTransport::connect(addr).unwrap());
            let mut servidor = listener
                .accept()
                .unwrap_or_else(|e| panic!("accept da rodada {rodada} falhou: {e}"));
            let mut cliente = conectando.join().unwrap();

            cliente.send(Channel::Control, &[rodada]).unwrap();
            assert_eq!(
                wait(&mut servidor).1,
                vec![rodada],
                "dados da rodada {rodada}"
            );

            // Encerra a sessão antes da próxima, que é o cenário real:
            // a conexão cai e alguém precisa reconectar.
            drop(cliente);
            drop(servidor);
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

    /// Furo de NAT pelo caminho novo: o viewer conecta, o host aceita, e os dois
    /// trocam dados pela conexão **direta** (sem relay). No loopback não há NAT,
    /// mas o percurso — endpoint compartilhado, `conectar` + `aceitar_furando` —
    /// é o mesmo que vai rodar entre as duas máquinas.
    #[test]
    fn furo_de_nat_em_loopback() {
        let host = QuicEndpoint::abrir_servidor().unwrap();
        let viewer = QuicEndpoint::abrir_cliente().unwrap();
        let porta_host = host.local_addr().unwrap().port();
        let porta_viewer = viewer.local_addr().unwrap().port();
        let host_addr: SocketAddr = format!("127.0.0.1:{porta_host}").parse().unwrap();
        let viewer_addr: SocketAddr = format!("127.0.0.1:{porta_viewer}").parse().unwrap();

        let aceitando = std::thread::spawn(move || {
            host.aceitar_furando(viewer_addr, Duration::from_secs(8))
                .expect("host devia aceitar")
        });
        let mut cliente = viewer
            .conectar(host_addr, Duration::from_secs(8))
            .expect("viewer devia conectar");
        let mut servidor = aceitando.join().unwrap();

        cliente.send(Channel::Control, b"oi").unwrap();
        assert_eq!(wait(&mut servidor).1, b"oi".to_vec());
        servidor.send(Channel::Control, b"ok").unwrap();
        assert_eq!(wait(&mut cliente).1, b"ok".to_vec());
    }
}
