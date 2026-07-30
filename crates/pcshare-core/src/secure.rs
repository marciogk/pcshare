//! Criptografia ponta a ponta e autenticação por ID + senha.
//!
//! Implementa a decisão D7 de `docs/ARQUITETURA.md`.
//!
//! # Por que Noise, e não algo escrito aqui
//!
//! Handshake criptográfico é a área onde código próprio falha de forma
//! silenciosa: funciona nos testes, protege contra ninguém. O
//! [Noise Protocol Framework](https://noiseprotocol.org) resolve exatamente
//! este problema — troca de chaves autenticada com sigilo futuro — e tem
//! análise formal publicada. Usamos a implementação `snow`.
//!
//! Padrão: `Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s`
//!
//! - **XX** — autenticação mútua sem nenhum dos lados conhecer a chave do
//!   outro de antemão. Necessário porque na primeira conexão o viewer não
//!   sabe nada sobre o host.
//! - **psk3** — a senha entra como chave pré-compartilhada na terceira
//!   mensagem. Quem não a tem não conclui o handshake: um atacante não chega
//!   nem a ver um pixel.
//! - **25519 / ChaChaPoly / BLAKE2s** — curva, cifra autenticada e hash.
//!
//! # Como o MITM é barrado
//!
//! O host anuncia sua chave pública em claro antes do handshake — ela não é
//! segredo, é a identidade dele. O viewer deriva a PSK de
//! `Argon2id(senha, sal = chave pública do host)`.
//!
//! Um intermediário que troque essa chave pela dele faz o viewer derivar uma
//! PSK diferente, e o handshake falha na terceira mensagem. Para passar, o
//! atacante precisaria da senha — que nunca trafega, em forma alguma.
//!
//! # Limite conhecido
//!
//! O modo de transporte do Noise numera as mensagens implicitamente, então
//! elas precisam ser decifradas **na ordem**. Sobre TCP isso é garantido. O
//! canal de mídia da Fase 2, que será melhor-esforço, vai exigir números de
//! sequência explícitos.

use std::io;
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use blake2::{Blake2s256, Digest};
use snow::{Builder, HandshakeState, TransportState};

use crate::transport::{Channel, Transport};

const PATTERN: &str = "Noise_XXpsk3_25519_ChaChaPoly_BLAKE2s";

/// Teto de uma mensagem no modo de transporte do Noise, menos a etiqueta de
/// autenticação e o nosso byte de continuação.
const MAX_CHUNK: usize = 65535 - 16 - 1;

/// Marca o último pedaço de uma mensagem fatiada.
const CHUNK_FINAL: u8 = 1;
const CHUNK_MORE: u8 = 0;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Chave estática de longa duração de uma máquina. É a identidade dela.
pub struct Identity {
    private: Vec<u8>,
    public: Vec<u8>,
}

impl Identity {
    pub fn generate() -> io::Result<Self> {
        let builder = Builder::new(PATTERN.parse().map_err(bad)?);
        let kp = builder.generate_keypair().map_err(bad)?;
        Ok(Self {
            private: kp.private,
            public: kp.public,
        })
    }

    /// Reconstrói a partir de uma chave privada guardada em disco.
    ///
    /// A identidade precisa sobreviver a reinícios: se ela mudasse, o ID da
    /// máquina mudaria junto e todo pareamento anterior seria invalidado.
    pub fn from_private(private: Vec<u8>) -> io::Result<Self> {
        let sk: [u8; 32] = private
            .as_slice()
            .try_into()
            .map_err(|_| bad("chave privada precisa ter 32 bytes"))?;
        let public = x25519_base(sk).to_vec();
        Ok(Self { private, public })
    }

    pub fn private_key(&self) -> &[u8] {
        &self.private
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Identificador curto que a pessoa dita por telefone.
    ///
    /// Derivado da chave pública, então não exige servidor para ser atribuído.
    /// **Um produto real precisa de atribuição central**: nove dígitos derivados
    /// de hash colidem por aniversário bem antes do que a intuição sugere —
    /// com ~37 mil máquinas a chance de alguma colisão já passa de 50%.
    pub fn display_id(&self) -> String {
        let d = Blake2s256::digest(&self.public);
        let n = u64::from_be_bytes(d[..8].try_into().unwrap()) % 1_000_000_000;
        let s = format!("{n:09}");
        format!("{} {} {}", &s[0..3], &s[3..6], &s[6..9])
    }

    pub fn fingerprint(&self) -> String {
        fingerprint_of(&self.public)
    }
}

/// Senha de sessão de seis dígitos, sorteada pelo gerador do sistema.
///
/// Curta porque alguém vai ditá-la por telefone. É o KDF caro (Argon2id) que
/// compensa o espaço pequeno — e uma senha de sessão deve ser trocada a cada
/// atendimento, não reaproveitada.
pub fn random_password() -> String {
    use rand::RngExt;
    format!("{:06}", rand::rng().random_range(0..1_000_000u32))
}

/// Impressão digital legível, para conferência fora de banda.
pub fn fingerprint_of(public_key: &[u8]) -> String {
    let d = Blake2s256::digest(public_key);
    d[..8]
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Deriva a chave pública da privada (multiplicação escalar pela base da
/// Curve25519), usando a implementação do `snow` em vez de reescrever a curva.
fn x25519_base(sk: [u8; 32]) -> [u8; 32] {
    use snow::params::DHChoice;
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let resolver = DefaultResolver;
    let mut dh = resolver
        .resolve_dh(&DHChoice::Curve25519)
        .expect("Curve25519 sempre disponível");
    dh.set(&sk);
    let mut out = [0u8; 32];
    out.copy_from_slice(&dh.pubkey()[..32]);
    out
}

/// Deriva a chave pré-compartilhada a partir da senha.
///
/// Argon2id porque a senha é curta e digitada por gente: sem um KDF caro, um
/// atacante que capture o tráfego testa o dicionário inteiro em minutos. O
/// sal é a chave pública do host, que é única por máquina e conhecida das
/// duas pontas.
fn derive_psk(password: &str, host_public: &[u8]) -> io::Result<[u8; 32]> {
    let params = Params::new(19 * 1024, 2, 1, Some(32)).map_err(bad)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut psk = [0u8; 32];
    argon
        .hash_password_into(password.as_bytes(), host_public, &mut psk)
        .map_err(bad)?;
    Ok(psk)
}

/// Transporte cifrado, ponta a ponta.
///
/// Envolve outro [`Transport`] e cifra cada mensagem. O de baixo passa a ver
/// apenas bytes opacos — nem o relay TURN da Fase 2 verá conteúdo.
pub struct SecureTransport<T: Transport> {
    inner: T,
    noise: TransportState,
    remote_static: Vec<u8>,
    /// Remontagem por canal. Mensagens grandes viram vários pedaços, e manter
    /// buffers separados evita que um vídeo fatiado se misture a um evento de
    /// entrada que chegue no meio.
    partial_control: Vec<u8>,
    partial_media: Vec<u8>,
    scratch: Vec<u8>,
}

impl<T: Transport> SecureTransport<T> {
    /// Chave pública estática do outro lado, autenticada pelo handshake.
    ///
    /// Guardar na primeira conexão e comparar nas seguintes é o que detecta
    /// troca de máquina ou tentativa de intermediação.
    pub fn remote_static(&self) -> &[u8] {
        &self.remote_static
    }

    pub fn remote_fingerprint(&self) -> String {
        fingerprint_of(&self.remote_static)
    }

    fn buffer_for(&mut self, channel: Channel) -> &mut Vec<u8> {
        match channel {
            Channel::Control => &mut self.partial_control,
            Channel::Media => &mut self.partial_media,
        }
    }
}

impl<T: Transport> Transport for SecureTransport<T> {
    fn send(&mut self, channel: Channel, payload: &[u8]) -> io::Result<()> {
        // Uma mensagem do Noise não passa de 64 KB; um keyframe passa. Fatiar
        // aqui, e não no chamador, mantém o limite do protocolo invisível para
        // o resto do código.
        let mut rest = payload;
        loop {
            let take = rest.len().min(MAX_CHUNK);
            let (chunk, tail) = rest.split_at(take);
            let final_chunk = tail.is_empty();

            self.scratch.clear();
            self.scratch
                .push(if final_chunk { CHUNK_FINAL } else { CHUNK_MORE });
            self.scratch.extend_from_slice(chunk);

            let mut out = vec![0u8; self.scratch.len() + 16];
            let n = self
                .noise
                .write_message(&self.scratch, &mut out)
                .map_err(bad)?;
            out.truncate(n);
            self.inner.send(channel, &out)?;

            if final_chunk {
                return Ok(());
            }
            rest = tail;
        }
    }

    fn poll(&mut self) -> io::Result<Option<(Channel, Vec<u8>)>> {
        loop {
            let Some((channel, cipher)) = self.inner.poll()? else {
                return Ok(None);
            };

            let mut plain = vec![0u8; cipher.len()];
            let n = self
                .noise
                .read_message(&cipher, &mut plain)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            plain.truncate(n);

            let Some((&flag, body)) = plain.split_first() else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pedaço cifrado sem cabeçalho",
                ));
            };

            let buf = self.buffer_for(channel);
            buf.extend_from_slice(body);
            if flag == CHUNK_FINAL {
                return Ok(Some((channel, std::mem::take(buf))));
            }
            // Pedaço intermediário: continua lendo.
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }

    fn pending_out(&self) -> usize {
        self.inner.pending_out()
    }

    fn bytes_sent(&self) -> u64 {
        self.inner.bytes_sent()
    }

    fn bytes_received(&self) -> u64 {
        self.inner.bytes_received()
    }
}

/// Lado do host: anuncia a identidade e espera o viewer se autenticar.
pub fn accept<T: Transport>(
    mut inner: T,
    identity: &Identity,
    password: &str,
) -> io::Result<SecureTransport<T>> {
    // A chave pública vai em claro de propósito: é a identidade anunciada, e
    // o viewer precisa dela para derivar a PSK antes da terceira mensagem.
    inner.send(Channel::Control, identity.public_key())?;
    inner.flush()?;

    let psk = derive_psk(password, identity.public_key())?;
    let state = Builder::new(PATTERN.parse().map_err(bad)?)
        .local_private_key(identity.private_key())
        .map_err(bad)?
        .psk(3, &psk)
        .map_err(bad)?
        .build_responder()
        .map_err(bad)?;

    run_handshake(inner, state, false)
}

/// Lado do viewer. Falha se a senha estiver errada — sem revelar em que ponto.
pub fn connect<T: Transport>(
    mut inner: T,
    identity: &Identity,
    password: &str,
) -> io::Result<SecureTransport<T>> {
    let announced = recv_blocking(&mut inner)?;
    if announced.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "host anunciou chave com tamanho inválido",
        ));
    }

    let psk = derive_psk(password, &announced)?;
    let state = Builder::new(PATTERN.parse().map_err(bad)?)
        .local_private_key(identity.private_key())
        .map_err(bad)?
        .psk(3, &psk)
        .map_err(bad)?
        .build_initiator()
        .map_err(bad)?;

    let secure = run_handshake(inner, state, true)?;

    // A chave que o handshake autenticou tem de ser a mesma que foi
    // anunciada. Divergência aqui é intermediação ativa.
    if secure.remote_static() != announced {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "chave do host diverge da anunciada — possível intermediação",
        ));
    }
    Ok(secure)
}

fn run_handshake<T: Transport>(
    mut inner: T,
    mut state: HandshakeState,
    initiator: bool,
) -> io::Result<SecureTransport<T>> {
    let mut buf = vec![0u8; 65535];

    if initiator {
        let n = state.write_message(&[], &mut buf).map_err(bad)?;
        inner.send(Channel::Control, &buf[..n])?;
        inner.flush()?;
    }

    while !state.is_handshake_finished() {
        let msg = recv_blocking(&mut inner)?;
        let mut plain = vec![0u8; 65535];
        state.read_message(&msg, &mut plain).map_err(|e| {
            // Senha errada aparece aqui, como falha de decifragem. A mensagem
            // é deliberadamente vaga: distinguir "senha errada" de "protocolo
            // incompatível" ajudaria quem está tentando adivinhar.
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("handshake recusado ({e})"),
            )
        })?;

        if !state.is_handshake_finished() {
            let n = state.write_message(&[], &mut buf).map_err(bad)?;
            inner.send(Channel::Control, &buf[..n])?;
            inner.flush()?;
        }
    }

    let remote_static = state
        .get_remote_static()
        .ok_or_else(|| bad("handshake terminou sem chave estática do par"))?
        .to_vec();

    // Impressão do handshake inteiro. Serve de testemunha da sessão: só
    // coincide se os dois lados derivaram exatamente o mesmo estado.
    let witness = state.get_handshake_hash().to_vec();
    let noise = state.into_transport_mode().map_err(bad)?;

    let mut secure = SecureTransport {
        inner,
        noise,
        remote_static,
        partial_control: Vec::new(),
        partial_media: Vec::new(),
        scratch: Vec::new(),
    };

    // Confirmação de chave, e não é formalidade.
    //
    // Em `XXpsk3` a senha entra na terceira mensagem — que o iniciador
    // **envia**. Ele conclui o handshake sem nunca comprovar que o outro lado
    // a conhece, e com senha errada declararia sucesso enquanto o host já
    // recusou. Observado em teste antes desta etapa existir.
    //
    // Não havia vazamento (as chaves divergem e nada trafega), mas um viewer
    // que anuncia "autenticado" e só descobre o erro quando a conexão cai
    // é inaceitável num produto onde a autenticação é a promessa central.
    if initiator {
        let echo = recv_secure_blocking(&mut secure)?;
        if echo != witness {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "host não confirmou a sessão — senha incorreta",
            ));
        }
    } else {
        secure.send(Channel::Control, &witness)?;
        secure.flush()?;
    }

    Ok(secure)
}

/// Espera a primeira mensagem de controle já decifrada.
fn recv_secure_blocking<T: Transport>(secure: &mut SecureTransport<T>) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        secure.flush()?;
        match secure.poll() {
            Ok(Some((Channel::Control, payload))) => return Ok(payload),
            Ok(_) => {}
            // Dois sintomas, uma causa. Falha de decifragem: as chaves de
            // transporte divergiram. Fim de arquivo: o host detectou a senha
            // errada primeiro e desligou. Nesta janela específica — entre o
            // handshake e a confirmação — as duas coisas significam recusa,
            // não problema de rede.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                ) =>
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "sessão não confirmada — senha incorreta",
                ));
            }
            Err(e) => return Err(e),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "host não confirmou a sessão a tempo",
            ));
        }
        std::thread::yield_now();
    }
}

/// Espera uma mensagem de controle. O handshake é sequencial por natureza.
fn recv_blocking<T: Transport>(inner: &mut T) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        inner.flush()?;
        if let Some((Channel::Control, payload)) = inner.poll()? {
            return Ok(payload);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "handshake não completou a tempo",
            ));
        }
        std::thread::yield_now();
    }
}

fn bad(e: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Message;
    use crate::tcp::TcpTransport;
    use std::net::TcpListener;

    fn par_seguro(senha_host: &str, senha_viewer: &str) -> io::Result<(
        SecureTransport<TcpTransport>,
        SecureTransport<TcpTransport>,
    )> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let id_host = Identity::generate().unwrap();
        let pub_host = id_host.public_key().to_vec();
        let sh = senha_host.to_string();

        let host = std::thread::spawn(move || {
            let t = TcpTransport::accept(&listener).unwrap();
            accept(t, &id_host, &sh)
        });

        let id_viewer = Identity::generate().unwrap();
        let t = TcpTransport::connect(addr).unwrap();
        let viewer = connect(t, &id_viewer, senha_viewer);

        let host = host.join().unwrap();
        match (host, viewer) {
            (Ok(h), Ok(v)) => {
                assert_eq!(v.remote_static(), pub_host, "viewer autenticou outra chave");
                Ok((h, v))
            }
            (Err(e), _) | (_, Err(e)) => Err(e),
        }
    }

    fn troca(de: &mut SecureTransport<TcpTransport>, para: &mut SecureTransport<TcpTransport>, m: &Message) {
        de.send(m.channel(), &m.encode()).unwrap();
        de.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            de.flush().unwrap();
            if let Some((_, p)) = para.poll().unwrap() {
                assert_eq!(&Message::decode(&p).unwrap(), m);
                return;
            }
            assert!(Instant::now() < deadline, "mensagem não chegou");
            std::thread::yield_now();
        }
    }

    #[test]
    fn senha_correta_estabelece_sessao() {
        let (mut h, mut v) = par_seguro("123456", "123456").expect("handshake deveria passar");
        assert_eq!(h.remote_fingerprint().len(), 23);

        troca(&mut h, &mut v, &Message::Hello {
            width: 1920,
            height: 1080,
            fps: 30,
        });
        troca(&mut v, &mut h, &Message::RequestKeyframe);
    }

    /// **As duas pontas** precisam recusar. Antes da confirmação de chave, o
    /// viewer declarava sucesso com a senha errada enquanto o host recusava.
    #[test]
    fn senha_errada_e_recusada_pelos_dois_lados() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let id_host = Identity::generate().unwrap();

        let host = std::thread::spawn(move || {
            let t = TcpTransport::accept(&listener).unwrap();
            accept(t, &id_host, "123456").map(|_| ())
        });

        let id_viewer = Identity::generate().unwrap();
        let t = TcpTransport::connect(addr).unwrap();
        let viewer = connect(t, &id_viewer, "999999").map(|_| ());

        let erro_viewer = viewer.expect_err("viewer deveria recusar");
        assert!(
            matches!(
                erro_viewer.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::TimedOut
            ),
            "viewer: erro inesperado {erro_viewer:?}"
        );
        assert!(host.join().unwrap().is_err(), "host deveria recusar");
    }

    /// Um keyframe passa do limite de 64 KB de uma mensagem do Noise. Sem
    /// fatiamento correto, o vídeo simplesmente não trafega.
    #[test]
    fn mensagem_maior_que_o_limite_do_noise_atravessa() {
        let (mut h, mut v) = par_seguro("senha", "senha").unwrap();
        let grande = Message::Video {
            seq: 1,
            time_us: 42,
            is_keyframe: true,
            data: (0..300_000u32).map(|i| (i % 251) as u8).collect(),
        };
        troca(&mut h, &mut v, &grande);
    }

    #[test]
    fn identidade_gera_id_e_impressao_estaveis() {
        let id = Identity::generate().unwrap();
        assert_eq!(id.display_id(), id.display_id());
        assert_eq!(id.display_id().len(), 11, "formato 000 000 000");
        assert_eq!(id.fingerprint(), fingerprint_of(id.public_key()));
    }

    /// A identidade precisa sobreviver a reinícios do host, senão o ID da
    /// máquina muda e todo pareamento anterior se perde.
    #[test]
    fn identidade_recarregada_do_disco_e_a_mesma() {
        let original = Identity::generate().unwrap();
        let recarregada = Identity::from_private(original.private_key().to_vec()).unwrap();

        assert_eq!(original.public_key(), recarregada.public_key());
        assert_eq!(original.display_id(), recarregada.display_id());
        assert_eq!(original.fingerprint(), recarregada.fingerprint());
    }

    #[test]
    fn chave_privada_de_tamanho_errado_e_rejeitada() {
        assert!(Identity::from_private(vec![0u8; 16]).is_err());
        assert!(Identity::from_private(Vec::new()).is_err());
    }
}
