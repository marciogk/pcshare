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
//! # Ordem de chegada
//!
//! O modo de transporte do Noise numera as mensagens implicitamente, o que
//! exigiria decifrá-las na ordem exata de envio. Sobre TCP isso valia de
//! graça; sobre QUIC não vale mais — cada quadro vai num fluxo independente,
//! justamente para que um atrasado não segure o próximo, e a chegada fora de
//! ordem passa a ser o comportamento desejado.
//!
//! Por isso o número vai **explícito** em cada mensagem, e o receptor o fixa
//! antes de decifrar. Isso abriria espaço para repetição de mensagens, então
//! entra também uma janela deslizante anti-repetição, no mesmo desenho usado
//! por IPsec e DTLS: aceita qualquer número ainda não visto dentro das
//! últimas 64 posições e rejeita o resto.

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

/// Cabeçalho de cada pedaço, dentro da parte cifrada:
/// `[id da mensagem u32][índice u16][total u16]`.
///
/// Nada aqui é enfeite, e cada campo custou um defeito observado em execução:
///
/// - **id da mensagem** — sobre TCP os pedaços de uma mensagem chegavam
///   grudados e bastava um buffer por canal. Sobre QUIC cada pedaço viaja num
///   fluxo próprio e os de mensagens diferentes se intercalam; concatená-los
///   no mesmo buffer produzia lixo.
/// - **índice e total** — uma simples marca de "último pedaço" também não
///   basta: o último pode chegar **primeiro**, e aí ele sozinho parecia uma
///   mensagem completa. Só sabendo quantos são e onde cada um entra dá para
///   remontar sem depender da ordem.
const CHUNK_HEADER: usize = 4 + 2 + 2;

/// Teto de mensagens sendo remontadas ao mesmo tempo. Um par hostil não pode
/// fazer a memória crescer indefinidamente mandando só pedaços iniciais.
const MAX_PARTIAL: usize = 16;

/// Mensagem em remontagem. Os pedaços podem chegar em qualquer ordem, então
/// cada um vai para o seu lugar e contamos quantos ainda faltam.
struct Partial {
    chunks: Vec<Option<Vec<u8>>>,
    faltam: usize,
}

impl Partial {
    fn new(total: usize) -> Self {
        Self {
            chunks: vec![None; total],
            faltam: total,
        }
    }
}

/// Prefixo com o número da mensagem, em little-endian.
const NONCE_LEN: usize = 8;

/// Janela deslizante anti-repetição.
///
/// Sem ela, os números explícitos deixariam um atacante reenviar uma mensagem
/// capturada — um clique, por exemplo — e o receptor a aceitaria como nova.
struct ReplayWindow {
    highest: u64,
    /// Bit `i` marca o número `highest - i` como já visto.
    seen: u64,
}

impl ReplayWindow {
    fn new() -> Self {
        Self {
            highest: 0,
            seen: 0,
        }
    }

    /// `true` se o número é novo e aceitável.
    fn accept(&mut self, n: u64) -> bool {
        if n > self.highest {
            let avanco = n - self.highest;
            self.seen = if avanco >= 64 { 0 } else { self.seen << avanco };
            self.seen |= 1;
            self.highest = n;
            return true;
        }
        let atraso = self.highest - n;
        if atraso >= 64 {
            // Velha demais para provar que não é repetição.
            return false;
        }
        let bit = 1u64 << atraso;
        if self.seen & bit != 0 {
            return false;
        }
        self.seen |= bit;
        true
    }
}

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
    /// Remontagem indexada por identificador de mensagem.
    partial: std::collections::HashMap<u32, Partial>,
    /// Contador dos identificadores que emitimos.
    next_msg_id: u32,
    scratch: Vec<u8>,
    replay: ReplayWindow,
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

}

impl<T: Transport> Transport for SecureTransport<T> {
    fn send(&mut self, channel: Channel, payload: &[u8]) -> io::Result<()> {
        // Uma mensagem do Noise não passa de 64 KB; um keyframe passa. Fatiar
        // aqui, e não no chamador, mantém o limite do protocolo invisível para
        // o resto do código.
        let msg_id = self.next_msg_id;
        self.next_msg_id = self.next_msg_id.wrapping_add(1);

        let usable = MAX_CHUNK - CHUNK_HEADER;
        let total = payload.len().div_ceil(usable).max(1);
        if total > u16::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mensagem exige pedaços demais",
            ));
        }

        let mut rest = payload;
        let mut index = 0u16;
        loop {
            let take = rest.len().min(usable);
            let (chunk, tail) = rest.split_at(take);

            self.scratch.clear();
            self.scratch.extend_from_slice(&msg_id.to_le_bytes());
            self.scratch.extend_from_slice(&index.to_le_bytes());
            self.scratch.extend_from_slice(&(total as u16).to_le_bytes());
            self.scratch.extend_from_slice(chunk);
            index += 1;

            // Lido antes de cifrar: `write_message` incrementa o contador.
            let nonce = self.noise.sending_nonce();

            let mut out = vec![0u8; NONCE_LEN + self.scratch.len() + 16];
            out[..NONCE_LEN].copy_from_slice(&nonce.to_le_bytes());
            let n = self
                .noise
                .write_message(&self.scratch, &mut out[NONCE_LEN..])
                .map_err(bad)?;
            out.truncate(NONCE_LEN + n);
            self.inner.send(channel, &out)?;

            if tail.is_empty() {
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

            if cipher.len() < NONCE_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mensagem cifrada sem número de sequência",
                ));
            }
            let (nonce_bytes, body) = cipher.split_at(NONCE_LEN);
            let nonce = u64::from_le_bytes(nonce_bytes.try_into().unwrap());

            if !self.replay.accept(nonce) {
                // Repetida ou velha demais. Descartar em silêncio é o certo:
                // responder daria a um atacante um oráculo para sondar quais
                // números já passaram.
                continue;
            }
            self.noise.set_receiving_nonce(nonce);

            let mut plain = vec![0u8; body.len()];
            let n = self
                .noise
                .read_message(body, &mut plain)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            plain.truncate(n);

            if plain.len() < CHUNK_HEADER {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "pedaço cifrado sem cabeçalho",
                ));
            }
            let msg_id = u32::from_le_bytes(plain[0..4].try_into().unwrap());
            let index = u16::from_le_bytes(plain[4..6].try_into().unwrap()) as usize;
            let total = u16::from_le_bytes(plain[6..8].try_into().unwrap()) as usize;
            let body = &plain[CHUNK_HEADER..];

            if total == 0 || index >= total {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "índice de pedaço fora do total declarado",
                ));
            }

            // Caminho comum: mensagem inteira num pedaço só. A esmagadora
            // maioria cabe aqui e nem toca no mapa de remontagem.
            if total == 1 {
                return Ok(Some((channel, body.to_vec())));
            }

            if self.partial.len() >= MAX_PARTIAL && !self.partial.contains_key(&msg_id) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "mensagens incompletas demais em remontagem",
                ));
            }

            let slot = self
                .partial
                .entry(msg_id)
                .or_insert_with(|| Partial::new(total));
            if slot.chunks.len() != total {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "total de pedaços divergente para a mesma mensagem",
                ));
            }
            if slot.chunks[index].is_none() {
                slot.faltam -= 1;
            }
            slot.chunks[index] = Some(body.to_vec());

            if slot.faltam == 0 {
                let pronta = self.partial.remove(&msg_id).expect("acabou de existir");
                let mut completa = Vec::new();
                for parte in pronta.chunks {
                    completa.extend_from_slice(&parte.expect("nenhum pedaço faltando"));
                }
                return Ok(Some((channel, completa)));
            }
            // Ainda falta pedaço: continua lendo.
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
        partial: std::collections::HashMap::new(),
        next_msg_id: 0,
        scratch: Vec::new(),
        replay: ReplayWindow::new(),
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

    /// Cifra um pedaço solto, no formato do fio, para poder embaralhar a
    /// entrega nos testes.
    fn cifra_pedaco(
        de: &mut SecureTransport<TcpTransport>,
        msg_id: u32,
        index: u16,
        total: u16,
        corpo: &[u8],
    ) -> Vec<u8> {
        let nonce = de.noise.sending_nonce();
        let mut plain = Vec::new();
        plain.extend_from_slice(&msg_id.to_le_bytes());
        plain.extend_from_slice(&index.to_le_bytes());
        plain.extend_from_slice(&total.to_le_bytes());
        plain.extend_from_slice(corpo);

        let mut out = vec![0u8; NONCE_LEN + plain.len() + 16];
        out[..NONCE_LEN].copy_from_slice(&nonce.to_le_bytes());
        let k = de.noise.write_message(&plain, &mut out[NONCE_LEN..]).unwrap();
        out.truncate(NONCE_LEN + k);
        out
    }

    fn espera(
        rx: &mut SecureTransport<TcpTransport>,
        tx: &mut SecureTransport<TcpTransport>,
    ) -> (Channel, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            tx.inner.flush().unwrap();
            if let Some(v) = rx.poll().unwrap() {
                return v;
            }
            assert!(Instant::now() < deadline, "nada chegou");
            std::thread::yield_now();
        }
    }

    /// O defeito que a mudança para QUIC expôs.
    ///
    /// Com um fluxo por pedaço, os pedaços de mensagens diferentes se
    /// intercalam. Antes do identificador de mensagem eles eram concatenados
    /// no mesmo buffer e produziam lixo.
    #[test]
    fn pedacos_intercalados_de_mensagens_diferentes_remontam() {
        let (mut h, mut v) = par_seguro("senha", "senha").unwrap();

        let a1 = cifra_pedaco(&mut h, 10, 0, 2, &[0xA1; 50]);
        let b1 = cifra_pedaco(&mut h, 20, 0, 2, &[0xB1; 50]);
        let a2 = cifra_pedaco(&mut h, 10, 1, 2, &[0xA2; 50]);
        let b2 = cifra_pedaco(&mut h, 20, 1, 2, &[0xB2; 50]);

        for c in [&a1, &b1, &a2, &b2] {
            h.inner.send(Channel::Media, c).unwrap();
        }
        h.inner.flush().unwrap();

        let (_, primeira) = espera(&mut v, &mut h);
        let mut esperada_a = vec![0xA1u8; 50];
        esperada_a.extend_from_slice(&[0xA2; 50]);
        assert_eq!(primeira, esperada_a, "mensagem 10 remontada errado");

        let (_, segunda) = espera(&mut v, &mut h);
        let mut esperada_b = vec![0xB1u8; 50];
        esperada_b.extend_from_slice(&[0xB2; 50]);
        assert_eq!(segunda, esperada_b, "mensagem 20 remontada errado");
    }

    /// O defeito que sobrou depois da primeira correção: com fluxos
    /// independentes o **último** pedaço pode chegar primeiro, e uma simples
    /// marca de "final" o fazia parecer uma mensagem completa sozinho.
    #[test]
    fn ultimo_pedaco_chegando_primeiro_nao_trunca() {
        let (mut h, mut v) = par_seguro("senha", "senha").unwrap();

        let p0 = cifra_pedaco(&mut h, 7, 0, 3, &[0x01; 40]);
        let p1 = cifra_pedaco(&mut h, 7, 1, 3, &[0x02; 40]);
        let p2 = cifra_pedaco(&mut h, 7, 2, 3, &[0x03; 40]);

        // Ordem de chegada: último, primeiro, meio.
        for c in [&p2, &p0, &p1] {
            h.inner.send(Channel::Media, c).unwrap();
        }
        h.inner.flush().unwrap();

        let (_, msg) = espera(&mut v, &mut h);
        let mut esperada = vec![0x01u8; 40];
        esperada.extend_from_slice(&[0x02; 40]);
        esperada.extend_from_slice(&[0x03; 40]);
        assert_eq!(msg, esperada, "remontagem fora de ordem falhou");
    }

    #[test]
    fn janela_anti_repeticao_aceita_novos_e_rejeita_repetidos() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(0), "primeira mensagem");
        assert!(!w.accept(0), "repetição imediata");

        assert!(w.accept(1));
        assert!(w.accept(5), "salto para frente");
        assert!(w.accept(3), "atrasada, mas dentro da janela");
        assert!(!w.accept(3), "atrasada repetida");
        assert!(!w.accept(5), "a mais recente, repetida");

        // Muito antiga: fora da janela de 64.
        assert!(w.accept(500));
        assert!(!w.accept(400));
        assert!(w.accept(490), "ainda dentro das últimas 64");
    }

    /// Sobre QUIC cada quadro vai num fluxo independente, então a chegada
    /// fora de ordem é o comportamento **desejado** — e a decifragem precisa
    /// sobreviver a ela.
    #[test]
    fn midia_fora_de_ordem_e_decifrada() {
        let (mut h, mut v) = par_seguro("senha", "senha").unwrap();

        // Cifra três mensagens com o estado de envio do host, sem passar pelo
        // transporte — é o que permite reordená-las antes da entrega.
        let mut cifradas = Vec::new();
        for n in 0..3u32 {
            cifradas.push(cifra_pedaco(&mut h, n, 0, 1, &[n as u8; 100]));
        }

        // Entrega na ordem inversa, pelo transporte de baixo do host.
        cifradas.reverse();
        for c in &cifradas {
            h.inner.send(Channel::Media, c).unwrap();
        }
        h.inner.flush().unwrap();

        for esperado in [2u8, 1, 0] {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                h.inner.flush().unwrap();
                if let Some((canal, dados)) = v.poll().unwrap() {
                    assert_eq!(canal, Channel::Media);
                    assert_eq!(dados, vec![esperado; 100]);
                    break;
                }
                assert!(Instant::now() < deadline, "mensagem {esperado} não chegou");
                std::thread::yield_now();
            }
        }
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
