//! Canal entre o supervisor e os agentes.
//!
//! # O que trafega, e por quê
//!
//! O **agente** captura e codifica; o **supervisor** detém a sessão de rede.
//! A divisão não é arbitrária:
//!
//! - A textura capturada vive na GPU, dentro da sessão do usuário. Mandar
//!   quadro cru para o supervisor seria GPU → CPU → canal → CPU → GPU, o mesmo
//!   desvio que a Fase 1 eliminou por custar 11,7 ms por quadro. Quadro cru
//!   tem ~4 MB; codificado, ~8 KB.
//! - A sessão de rede precisa sobreviver às trocas de desktop, que matam e
//!   recriam agentes. Se ela morasse no agente, cada prompt de UAC derrubaria
//!   o viewer — desfazendo o que a Fase 3 conquistou.
//! - A injeção de entrada precisa acontecer no agente, porque só ele está
//!   preso ao desktop que recebe entrada.
//!
//! Daí o canal ser **bidirecional e persistente**: vídeo sobe, entrada desce,
//! e a conexão dura o que durar o agente.
//!
//! # Enquadramento
//!
//! Fluxo de bytes com cabeçalho próprio — `[tipo u8][tamanho u32]` — e não
//! modo mensagem do Windows. Um keyframe passa de 100 KB, e no modo mensagem
//! uma leitura com buffer menor devolve erro parcial que complica o laço sem
//! ganhar nada.
//!
//! # Sobre a permissão do cano
//!
//! O supervisor o cria como SYSTEM; os agentes do desktop do usuário rodam
//! como usuário. Sem descritor de segurança explícito eles não conseguiriam
//! abrir — foi assim que a linha do tempo perdeu metade dos eventos em
//! silêncio, e a lição foi cara o bastante para não se repetir.
//!
//! # Por que E/S sobreposta
//!
//! Um descritor **síncrono** do Windows serializa toda a E/S feita sobre ele:
//! uma segunda operação espera a primeira terminar, ainda que sejam de
//! direções opostas. Num canal duplex isso trava de imediato — a thread
//! leitora fica parada em `ReadFile` esperando dados, e a escrita que traria
//! esses dados fica atrás dela na fila do mesmo descritor.
//!
//! Foi um travamento real, não hipótese: o teste `vai_e_volta_nos_dois_sentidos`
//! parou com 0% de CPU. Daí `FILE_FLAG_OVERLAPPED` nas duas pontas, com um
//! evento por operação. Cada operação passa a ser independente, que é a
//! premissa do canal inteiro.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
};
use windows::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateEventW;
use windows::core::PCWSTR;

pub const CANO_PADRAO: &str = r"\\.\pipe\pcshare-supervisor";

/// Descritor de segurança do cano: SYSTEM e o usuário da sessão do console.
///
/// Uma DACL sem ACE para alguém nega esse alguém — então listar só os dois já
/// exclui todo o resto da máquina.
///
/// # Por que isto sozinho não bastaria
///
/// Enquanto o canal carregava apenas o nome do desktop ativo, `Everyone` era
/// defensável: qualquer processo local obtém esse nome sozinho. Deixou de ser
/// no momento em que ele passou a carregar a **tela**.
///
/// E a ameaça concreta não é um estranho: um processo comum do usuário **não
/// consegue** duplicar o desktop `Winlogon`, mas o agente que roda como SYSTEM
/// consegue — e despeja esses quadros aqui. Como o agente do desktop do
/// usuário precisa do token do usuário para existir, o usuário precisa ter
/// acesso ao cano. Ou seja: estreitar a permissão **não fecha** essa brecha,
/// só a reduz a quem já está logado.
///
/// Quem a fecha é a verificação de PID em [`Autorizados`]. As duas coisas se
/// complementam — a permissão barra quem não deveria sequer tentar, o PID
/// barra quem poderia.
fn descritor_do_cano() -> String {
    match sid_do_usuario_do_console() {
        Some(sid) => format!("D:(A;;GA;;;SY)(A;;GRGW;;;{sid})"),
        // Sem SID, restringir a SYSTEM travaria os agentes do usuário — o
        // canal inteiro pararia em silêncio, que é exatamente o modo de falha
        // que já custou caro nesta base. Prefere-se largo e ruidoso.
        None => "D:(A;;GRGW;;;WD)".to_string(),
    }
}

/// SID de quem está no console, para montar a permissão do cano.
///
/// Tenta pelo token da sessão do console, que é o certo quando o supervisor é
/// um serviço SYSTEM. Em modo console o `WTSQueryUserToken` falha por falta de
/// privilégio, e aí o token do próprio processo dá a mesma resposta.
fn sid_do_usuario_do_console() -> Option<String> {
    use windows::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        let sessao = WTSGetActiveConsoleSessionId();
        if WTSQueryUserToken(sessao, &mut token).is_err()
            && OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err()
        {
            return None;
        }

        let mut tamanho = 0u32;
        let _ = GetTokenInformation(token, TokenUser, None, 0, &mut tamanho);
        let mut buffer = vec![0u8; tamanho as usize];
        let obtido = GetTokenInformation(
            token,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            tamanho,
            &mut tamanho,
        );
        let sid = if obtido.is_ok() {
            let info = &*(buffer.as_ptr() as *const TOKEN_USER);
            let mut texto = windows::core::PWSTR::null();
            ConvertSidToStringSidW(info.User.Sid, &mut texto)
                .ok()
                .and_then(|()| texto.to_string().ok())
        } else {
            None
        };
        let _ = CloseHandle(token);
        sid
    }
}

/// Teto por mensagem. Um keyframe de 1080p não chega perto disto, e o limite
/// impede que um cabeçalho corrompido peça uma alocação absurda.
const MAX_MENSAGEM: usize = 8 * 1024 * 1024;

const T_DESKTOP: u8 = 1;
const T_QUADRO: u8 = 2;
const T_ENTRADA: u8 = 3;
const T_PRONTO: u8 = 4;
const T_KEYFRAME: u8 = 5;
const T_CAPTURAR: u8 = 6;

/// `seq` (8) + `tempo_us` (8) + `chave` (1), antes dos dados do quadro.
const CAB_QUADRO: usize = 17;

#[derive(Debug, Clone, PartialEq)]
pub enum Mensagem {
    /// Agente → supervisor: o desktop de entrada agora é este.
    Desktop(String),
    /// Agente → supervisor: quadro já codificado, pronto para a rede.
    Quadro {
        seq: u64,
        /// Instante da **captura**, não do envio.
        ///
        /// É o carimbo que segue em `Message::Video` e permite ao viewer medir
        /// a travessia inteira. Ele tem de nascer no agente: se o supervisor o
        /// aplicasse ao repassar, a medição perderia captura e encode em
        /// silêncio — e ficaria menor que a real, que é o pior tipo de erro
        /// num instrumento.
        tempo_us: i64,
        chave: bool,
        dados: Vec<u8>,
    },
    /// Agente → supervisor: a captura abriu, e estas são as dimensões.
    ///
    /// O viewer não desenha nada antes de `Message::Hello`, e só o agente
    /// conhece a resolução — o supervisor está na sessão 0, cego.
    Pronto { largura: u32, altura: u32 },
    /// Supervisor → agente: evento de entrada, já serializado pelo protocolo.
    Entrada(Vec<u8>),
    /// Supervisor → agente: o viewer pediu um quadro-chave.
    PedirKeyframe,
    /// Supervisor → agente: transmitir quadros, ou parar de transmitir.
    ///
    /// Sem isto o agente empurra vídeo mesmo sem ninguém assistindo. O cano
    /// enche em torno de meio segundo — 256 KB de buffer contra ~8 KB por
    /// quadro a 60 por segundo — e a escrita seguinte **bloqueia para sempre**,
    /// congelando o agente. Junto com ele param os relatos de troca de desktop,
    /// e a Fase 3 inteira deixa de funcionar.
    ///
    /// De quebra, não codificar sem público poupa GPU na máquina do cliente,
    /// que é onde esse custo é sentido.
    Capturar(bool),
}

impl Mensagem {
    fn tipo(&self) -> u8 {
        match self {
            Self::Desktop(_) => T_DESKTOP,
            Self::Quadro { .. } => T_QUADRO,
            Self::Entrada(_) => T_ENTRADA,
            Self::Pronto { .. } => T_PRONTO,
            Self::PedirKeyframe => T_KEYFRAME,
            Self::Capturar(_) => T_CAPTURAR,
        }
    }

    fn corpo(&self) -> Vec<u8> {
        match self {
            Self::Desktop(nome) => nome.as_bytes().to_vec(),
            Self::Quadro {
                seq,
                tempo_us,
                chave,
                dados,
            } => {
                let mut v = Vec::with_capacity(dados.len() + CAB_QUADRO);
                v.extend_from_slice(&seq.to_le_bytes());
                v.extend_from_slice(&tempo_us.to_le_bytes());
                v.push(u8::from(*chave));
                v.extend_from_slice(dados);
                v
            }
            Self::Pronto { largura, altura } => {
                let mut v = Vec::with_capacity(8);
                v.extend_from_slice(&largura.to_le_bytes());
                v.extend_from_slice(&altura.to_le_bytes());
                v
            }
            Self::Entrada(bytes) => bytes.clone(),
            Self::PedirKeyframe => Vec::new(),
            Self::Capturar(ligado) => vec![u8::from(*ligado)],
        }
    }

    fn de(tipo: u8, corpo: Vec<u8>) -> Option<Self> {
        match tipo {
            T_DESKTOP => Some(Self::Desktop(String::from_utf8_lossy(&corpo).into_owned())),
            T_QUADRO if corpo.len() >= CAB_QUADRO => Some(Self::Quadro {
                seq: u64::from_le_bytes(corpo[..8].try_into().ok()?),
                tempo_us: i64::from_le_bytes(corpo[8..16].try_into().ok()?),
                chave: corpo[16] != 0,
                dados: corpo[CAB_QUADRO..].to_vec(),
            }),
            T_PRONTO if corpo.len() >= 8 => Some(Self::Pronto {
                largura: u32::from_le_bytes(corpo[..4].try_into().ok()?),
                altura: u32::from_le_bytes(corpo[4..8].try_into().ok()?),
            }),
            T_ENTRADA => Some(Self::Entrada(corpo)),
            T_KEYFRAME => Some(Self::PedirKeyframe),
            T_CAPTURAR if !corpo.is_empty() => Some(Self::Capturar(corpo[0] != 0)),
            _ => None,
        }
    }

    fn enquadrar(&self) -> Vec<u8> {
        let corpo = self.corpo();
        let mut v = Vec::with_capacity(corpo.len() + 5);
        v.push(self.tipo());
        v.extend_from_slice(&(corpo.len() as u32).to_le_bytes());
        v.extend_from_slice(&corpo);
        v
    }
}

// ------------------------------------------------------------ lado do agente

pub struct Cliente {
    cano: HANDLE,
}

// O agente compartilha um `Cliente` entre o laço de captura, que escreve
// quadros, e a thread de entrada, que lê — daí precisar de `Sync`, não só de
// `Send`.
//
// O que sustenta isto é o cano estar em modo sobreposto: cada operação carrega
// o próprio `OVERLAPPED` e o próprio evento, então duas threads não disputam
// estado nenhum dentro do descritor. Com um descritor síncrono a afirmação
// seria falsa, e o preço já foi pago uma vez.
unsafe impl Send for Cliente {}
unsafe impl Sync for Cliente {}

impl Cliente {
    pub fn conectar(cano: &str) -> Option<Self> {
        let nome = wide(cano);
        unsafe {
            let h = CreateFileW(
                PCWSTR(nome.as_ptr()),
                FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
            .ok()?;
            Some(Self { cano: h })
        }
    }

    #[must_use]
    pub fn enviar(&self, m: &Mensagem) -> bool {
        escrever_tudo(self.cano, &m.enquadrar())
    }

    /// Bloqueia até chegar uma mensagem, ou `None` se a conexão caiu.
    pub fn receber(&self) -> Option<Mensagem> {
        ler_mensagem(self.cano)
    }
}

impl Drop for Cliente {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.cano);
        }
    }
}

// --------------------------------------------------------- lado do supervisor

/// Quem tem direito de falar pelo cano: os processos que o supervisor lançou.
///
/// A permissão do cano não basta. O agente do desktop do usuário roda com o
/// token do usuário, então o usuário tem de ter acesso — e sem mais nada,
/// qualquer processo dele poderia conectar e receber os quadros do `Winlogon`,
/// que ele não consegue capturar por conta própria. O cano viraria exatamente
/// a brecha que o Windows fecha.
///
/// Um PID não é segredo, mas também não é o que está sendo verificado: o
/// Windows garante que `GetNamedPipeClientProcessId` devolve o processo real
/// do outro lado, e só o supervisor sabe quais ele criou. Forjar exigiria
/// primeiro ser um processo que o supervisor lançou.
#[derive(Clone, Default)]
pub struct Autorizados(Arc<Mutex<std::collections::HashSet<u32>>>);

impl Autorizados {
    pub fn permitir(&self, pid: u32) {
        if let Ok(mut s) = self.0.lock() {
            s.insert(pid);
        }
    }

    pub fn revogar(&self, pid: u32) {
        if let Ok(mut s) = self.0.lock() {
            s.remove(&pid);
        }
    }

    fn aceita(&self, pid: u32) -> bool {
        self.0.lock().is_ok_and(|s| s.contains(&pid))
    }
}

/// PID do processo do outro lado de uma instância conectada.
fn pid_do_cliente(cano: HANDLE) -> Option<u32> {
    use windows::Win32::System::Pipes::GetNamedPipeClientProcessId;
    let mut pid = 0u32;
    unsafe { GetNamedPipeClientProcessId(cano, &mut pid) }.ok()?;
    Some(pid)
}

/// Conexões vivas, por identificador de agente.
///
/// O descritor é guardado como `usize`, não como `HANDLE`. Um invólucro com
/// `unsafe impl Send` parecia mais expressivo, mas não funciona: a captura
/// precisa de closures pega o *campo* quando o fechamento faz `.0`, e o
/// `HANDLE` cru atravessa a fronteira sem que o `Send` do invólucro se
/// aplique. `usize` é `Send` de verdade, sem depender de como o fechamento
/// escreve o acesso.
type Conexoes = Arc<Mutex<HashMap<u64, usize>>>;

/// Reconstrói o descritor guardado como número.
fn como_cano(bruto: usize) -> HANDLE {
    HANDLE(bruto as *mut std::ffi::c_void)
}

pub struct Servidor {
    conexoes: Conexoes,
    recebidas: Receiver<(u64, Mensagem)>,
}

impl Servidor {
    /// Sobe as threads de aceitação e leitura.
    ///
    /// Só conversa com processos previamente postos em `autorizados`. Quem
    /// abre o cano precisa autorizar cada agente **antes** de lançá-lo — o
    /// agente conecta em milissegundos, e autorizar depois seria uma corrida.
    pub fn abrir(cano: &str, autorizados: Autorizados) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let conexoes: Conexoes = Arc::new(Mutex::new(HashMap::new()));
        let cano = cano.to_string();
        let c = conexoes.clone();

        std::thread::Builder::new()
            .name("pcshare-canal-aceita".into())
            .spawn(move || aceitar_sempre(&cano, tx, c, autorizados))
            .ok();

        Self {
            conexoes,
            recebidas: rx,
        }
    }

    /// Não bloqueia.
    pub fn receber(&self) -> Option<(u64, Mensagem)> {
        self.recebidas.try_recv().ok()
    }

    /// Bloqueia até chegar mensagem ou esgotar a espera.
    ///
    /// É o ponto de bloqueio do laço de sessão. Sem ele, quem repassa quadros
    /// giraria em falso consumindo um núcleo enquanto a tela estivesse parada
    /// — e a tela fica parada a maior parte do tempo.
    pub fn receber_ate(&self, limite: std::time::Duration) -> Option<(u64, Mensagem)> {
        self.recebidas.recv_timeout(limite).ok()
    }

    #[must_use]
    pub fn enviar(&self, agente: u64, m: &Mensagem) -> bool {
        let bytes = m.enquadrar();
        let cano = self.conexoes.lock().ok().and_then(|c| c.get(&agente).copied());
        match cano {
            Some(bruto) => escrever_tudo(como_cano(bruto), &bytes),
            None => false,
        }
    }

    /// Manda a mesma mensagem a todos os agentes conectados.
    ///
    /// Existe para ligar e desligar a transmissão: quando um viewer chega, não
    /// se sabe qual agente está enxergando, e perguntar custaria uma volta a
    /// mais. Os cegos recebem e não têm o que transmitir.
    pub fn enviar_a_todos(&self, m: &Mensagem) -> usize {
        let bytes = m.enquadrar();
        let canos: Vec<usize> = match self.conexoes.lock() {
            Ok(c) => c.values().copied().collect(),
            Err(_) => return 0,
        };
        canos
            .into_iter()
            .filter(|&bruto| escrever_tudo(como_cano(bruto), &bytes))
            .count()
    }

    pub fn conectados(&self) -> usize {
        self.conexoes.lock().map(|c| c.len()).unwrap_or(0)
    }
}

fn aceitar_sempre(
    cano: &str,
    tx: Sender<(u64, Mensagem)>,
    conexoes: Conexoes,
    autorizados: Autorizados,
) {
    let mut proximo_id = 1u64;
    loop {
        let Some(h) = criar_instancia(cano) else {
            return;
        };
        // Com o cano em modo sobreposto, `ConnectNamedPipe` exige um
        // `OVERLAPPED`: sem ele a chamada é recusada de saída.
        let Some(evento) = Evento::novo() else {
            unsafe { CloseHandle(h).ok() };
            return;
        };
        let mut ov = OVERLAPPED {
            hEvent: evento.0,
            ..Default::default()
        };
        let conectou = match unsafe { ConnectNamedPipe(h, Some(&mut ov)) } {
            Ok(()) => true,
            // `ERROR_PIPE_CONNECTED` significa que o cliente chegou antes da
            // chamada — é sucesso, não falha.
            Err(e) if e.code().0 as u32 & 0xFFFF == ERROR_PIPE_CONNECTED.0 => true,
            Err(e) if e_pendente(&e) => {
                let mut n = 0u32;
                unsafe { GetOverlappedResult(h, &ov, &mut n, true) }.is_ok()
            }
            Err(_) => false,
        };
        if !conectou {
            unsafe { CloseHandle(h).ok() };
            continue;
        }

        // Quem é o outro lado? Um processo que o supervisor não lançou não tem
        // o que fazer aqui — sobretudo porque daqui saem os quadros do
        // `Winlogon`, que ele não conseguiria capturar sozinho.
        let cliente = pid_do_cliente(h);
        if !cliente.is_some_and(|p| autorizados.aceita(p)) {
            unsafe {
                let _ = DisconnectNamedPipe(h);
                CloseHandle(h).ok();
            }
            continue;
        }

        let id = proximo_id;
        proximo_id += 1;
        let bruto = h.0 as usize;
        if let Ok(mut c) = conexoes.lock() {
            c.insert(id, bruto);
        }

        let tx = tx.clone();
        let conexoes = conexoes.clone();
        std::thread::Builder::new()
            .name(format!("pcshare-canal-{id}"))
            .spawn(move || {
                let h = como_cano(bruto);
                while let Some(m) = ler_mensagem(h) {
                    if tx.send((id, m)).is_err() {
                        break;
                    }
                }
                if let Ok(mut c) = conexoes.lock() {
                    c.remove(&id);
                }
                unsafe {
                    let _ = DisconnectNamedPipe(h);
                    let _ = CloseHandle(h);
                }
            })
            .ok();
    }
}

// ------------------------------------------------------------------ E/S crua

/// Evento de sincronização, fechado ao sair de escopo.
///
/// Cada operação sobreposta precisa do seu. Criar um por chamada custa um
/// syscall; a 60 quadros por segundo isso é ruído perto do encode.
struct Evento(HANDLE);

impl Evento {
    fn novo() -> Option<Self> {
        // Manual, e começando não sinalizado. O próprio `ReadFile`/`WriteFile`
        // o volta a não sinalizado ao iniciar a operação, então reaproveitá-lo
        // entre as iterações do laço é seguro.
        let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }.ok()?;
        Some(Self(h))
    }
}

impl Drop for Evento {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn e_pendente(e: &windows::core::Error) -> bool {
    e.code().0 as u32 & 0xFFFF == ERROR_IO_PENDING.0
}

/// Espera uma operação sobreposta terminar e devolve quantos bytes andaram.
///
/// `resultado` é o que `ReadFile`/`WriteFile` devolveu: sucesso imediato ou
/// `ERROR_IO_PENDING`. Qualquer outro erro é a conexão caindo.
fn esperar_io(cano: HANDLE, ov: &OVERLAPPED, resultado: windows::core::Result<()>) -> Option<u32> {
    if let Err(e) = resultado
        && !e_pendente(&e)
    {
        return None;
    }
    let mut n = 0u32;
    unsafe { GetOverlappedResult(cano, ov, &mut n, true) }.ok()?;
    Some(n)
}

fn escrever_tudo(cano: HANDLE, bytes: &[u8]) -> bool {
    let Some(evento) = Evento::novo() else {
        return false;
    };
    let mut escrito = 0usize;
    while escrito < bytes.len() {
        let mut ov = OVERLAPPED {
            hEvent: evento.0,
            ..Default::default()
        };
        // Com operação sobreposta a contagem sai de `GetOverlappedResult`, não
        // daqui: o valor só existe depois que a operação termina.
        let r = unsafe { WriteFile(cano, Some(&bytes[escrito..]), None, Some(&mut ov)) };
        match esperar_io(cano, &ov, r) {
            Some(0) | None => return false,
            Some(n) => escrito += n as usize,
        }
    }
    true
}

fn ler_exato(cano: HANDLE, buf: &mut [u8]) -> bool {
    let Some(evento) = Evento::novo() else {
        return false;
    };
    let mut lido = 0usize;
    while lido < buf.len() {
        let mut ov = OVERLAPPED {
            hEvent: evento.0,
            ..Default::default()
        };
        let r = unsafe { ReadFile(cano, Some(&mut buf[lido..]), None, Some(&mut ov)) };
        match esperar_io(cano, &ov, r) {
            Some(0) | None => return false,
            Some(n) => lido += n as usize,
        }
    }
    true
}

fn ler_mensagem(cano: HANDLE) -> Option<Mensagem> {
    let mut cabecalho = [0u8; 5];
    if !ler_exato(cano, &mut cabecalho) {
        return None;
    }
    let tipo = cabecalho[0];
    let tamanho = u32::from_le_bytes(cabecalho[1..5].try_into().ok()?) as usize;
    if tamanho > MAX_MENSAGEM {
        return None;
    }
    let mut corpo = vec![0u8; tamanho];
    if tamanho > 0 && !ler_exato(cano, &mut corpo) {
        return None;
    }
    Mensagem::de(tipo, corpo)
}

fn criar_instancia(cano: &str) -> Option<HANDLE> {
    let nome = wide(cano);
    let sddl = wide(&descritor_do_cano());
    unsafe {
        let mut descritor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descritor,
            None,
        )
        .ok()?;

        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descritor.0,
            bInheritHandle: false.into(),
        };

        let h = CreateNamedPipeW(
            PCWSTR(nome.as_ptr()),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_BYTE | PIPE_WAIT,
            // Um por desktop, com folga.
            8,
            256 * 1024,
            256 * 1024,
            0,
            Some(&sa),
        );
        if h.is_invalid() { None } else { Some(h) }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cano_unico(nome: &str) -> String {
        format!(r"\\.\pipe\pcshare-teste-{nome}")
    }

    /// Nos testes o "agente" é este mesmo processo, então é ele que precisa de
    /// autorização. Passar pelo caminho real de verificação é de propósito: um
    /// atalho aqui deixaria a checagem de PID sem cobertura nenhuma.
    fn autorizando_este_processo() -> Autorizados {
        let a = Autorizados::default();
        a.permitir(std::process::id());
        a
    }

    /// Um processo não autorizado não deve conseguir falar pelo cano — é o que
    /// impede que qualquer programa do usuário leia os quadros do `Winlogon`,
    /// que ele não conseguiria capturar por conta própria.
    #[test]
    fn processo_nao_autorizado_nao_e_atendido() {
        let cano = cano_unico("sem-autorizacao");
        let servidor = Servidor::abrir(&cano, Autorizados::default());
        std::thread::sleep(Duration::from_millis(200));

        // Conectar pode até funcionar — quem barra é a verificação de PID, não
        // a permissão, já que este processo é o próprio usuário do console.
        if let Some(cliente) = Cliente::conectar(&cano) {
            let _ = cliente.enviar(&Mensagem::Desktop("Winlogon".into()));
        }

        // O que não pode acontecer é a mensagem ser entregue.
        let limite = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < limite {
            assert!(servidor.receber().is_none(), "mensagem de não autorizado passou");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(servidor.conectados(), 0, "conexão não autorizada ficou viva");
    }

    #[test]
    fn enquadramento_e_reversivel() {
        for m in [
            Mensagem::Desktop("Winlogon".into()),
            Mensagem::Quadro {
                seq: u64::MAX,
                tempo_us: -1,
                chave: true,
                dados: vec![1, 2, 3, 250],
            },
            Mensagem::Entrada(vec![9, 8, 7]),
            Mensagem::Pronto {
                largura: 1920,
                altura: 1080,
            },
            Mensagem::PedirKeyframe,
            Mensagem::Capturar(true),
            Mensagem::Capturar(false),
        ] {
            let bytes = m.enquadrar();
            let tamanho = u32::from_le_bytes(bytes[1..5].try_into().unwrap()) as usize;
            assert_eq!(tamanho, bytes.len() - 5, "tamanho declarado bate");
            assert_eq!(Mensagem::de(bytes[0], bytes[5..].to_vec()), Some(m));
        }
    }

    #[test]
    fn quadro_truncado_nao_causa_panico() {
        for n in 0..CAB_QUADRO {
            assert_eq!(Mensagem::de(T_QUADRO, vec![0u8; n]), None, "{n} bytes");
        }
        for n in 0..8 {
            assert_eq!(Mensagem::de(T_PRONTO, vec![0u8; n]), None, "pronto {n}");
        }
        assert_eq!(Mensagem::de(200, vec![1, 2, 3]), None, "tipo desconhecido");
    }

    #[test]
    fn vai_e_volta_nos_dois_sentidos() {
        let cano = cano_unico("duplex");
        let servidor = Servidor::abrir(&cano, autorizando_este_processo());
        std::thread::sleep(Duration::from_millis(200));

        let cliente = Cliente::conectar(&cano).expect("agente não conectou");

        // Agente -> supervisor
        assert!(cliente.enviar(&Mensagem::Desktop("Winlogon".into())));
        let (id, m) = esperar(&servidor);
        assert_eq!(m, Mensagem::Desktop("Winlogon".into()));

        // Supervisor -> agente
        assert!(servidor.enviar(id, &Mensagem::Entrada(vec![42, 43])));
        assert_eq!(cliente.receber(), Some(Mensagem::Entrada(vec![42, 43])));
    }

    /// O canal precisa carregar um keyframe inteiro, que passa de 100 KB — e
    /// o enquadramento próprio existe exatamente para isso.
    #[test]
    fn quadro_grande_atravessa_intacto() {
        let cano = cano_unico("grande");
        let servidor = Servidor::abrir(&cano, autorizando_este_processo());
        std::thread::sleep(Duration::from_millis(200));
        let cliente = Cliente::conectar(&cano).expect("agente não conectou");

        let dados: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        assert!(cliente.enviar(&Mensagem::Quadro {
            seq: 7,
            tempo_us: 1_234_567_890,
            chave: true,
            dados: dados.clone(),
        }));

        match esperar(&servidor).1 {
            Mensagem::Quadro {
                seq,
                tempo_us,
                chave,
                dados: d,
            } => {
                assert_eq!(seq, 7);
                assert_eq!(tempo_us, 1_234_567_890, "carimbo da captura preservado");
                assert!(chave);
                assert_eq!(d, dados, "quadro chegou diferente");
            }
            outro => panic!("esperava Quadro, veio {outro:?}"),
        }
    }

    /// As duas direções **ao mesmo tempo**, que é como o canal será usado de
    /// verdade: o agente empurra quadros enquanto recebe entrada.
    ///
    /// Este é o teste que o desenho anterior não tinha, e a falta dele deixou
    /// passar um travamento de projeto — descritor síncrono serializa toda a
    /// E/S, e ler e escrever ao mesmo tempo trancava as duas pontas. Trocar a
    /// ordem para sequencial faria este teste passar sem provar nada.
    #[test]
    fn as_duas_direcoes_ao_mesmo_tempo_nao_travam() {
        let cano = cano_unico("simultaneo");
        let servidor = Servidor::abrir(&cano, autorizando_este_processo());
        std::thread::sleep(Duration::from_millis(200));
        let cliente = Arc::new(Cliente::conectar(&cano).expect("agente não conectou"));

        // Descobre o identificador pela primeira mensagem, em vez de supor que
        // é 1: supor amarraria o teste à ordem interna de numeração.
        assert!(cliente.enviar(&Mensagem::Pronto {
            largura: 1920,
            altura: 1080
        }));
        let (id, m) = esperar(&servidor);
        assert_eq!(
            m,
            Mensagem::Pronto {
                largura: 1920,
                altura: 1080
            }
        );

        const N: u64 = 30;

        // O agente empurra quadros por um thread e recebe pedidos de keyframe
        // pelo outro, sobre o **mesmo** descritor — a forma exata do laço de
        // captura com injeção de entrada em paralelo.
        let escritor = {
            let cliente = cliente.clone();
            std::thread::spawn(move || {
                for seq in 0..N {
                    assert!(
                        cliente.enviar(&Mensagem::Quadro {
                            seq,
                            tempo_us: seq as i64,
                            chave: false,
                            dados: vec![7u8; 4096],
                        }),
                        "escrita do agente travou em {seq}"
                    );
                }
            })
        };
        let leitor = {
            let cliente = cliente.clone();
            std::thread::spawn(move || {
                let mut recebidas = 0u64;
                while recebidas < N {
                    match cliente.receber() {
                        Some(Mensagem::PedirKeyframe) => recebidas += 1,
                        Some(_) => {}
                        None => break,
                    }
                }
                recebidas
            })
        };

        // E o supervisor faz o mesmo: escreve daqui enquanto sua thread
        // leitora está parada em `ReadFile` no mesmo descritor.
        for _ in 0..N {
            assert!(servidor.enviar(id, &Mensagem::PedirKeyframe), "escrita do supervisor travou");
        }

        let mut vistos = 0u64;
        while vistos < N {
            if let Mensagem::Quadro { seq, tempo_us, .. } = esperar(&servidor).1 {
                assert_eq!(tempo_us, seq as i64, "carimbo embaralhou");
                assert_eq!(seq, vistos, "ordem trocou");
                vistos += 1;
            }
        }

        escritor.join().expect("thread de escrita entrou em pânico");
        assert_eq!(leitor.join().unwrap(), N, "nem tudo desceu");
    }

    /// Vários quadros seguidos, para provar que o fluxo não perde limites de
    /// mensagem — é o caso real, a 30 ou 60 por segundo.
    #[test]
    fn sequencia_de_quadros_preserva_ordem_e_limites() {
        let cano = cano_unico("sequencia");
        let servidor = Servidor::abrir(&cano, autorizando_este_processo());
        std::thread::sleep(Duration::from_millis(200));
        let cliente = Cliente::conectar(&cano).expect("agente não conectou");

        for seq in 0..40u64 {
            assert!(cliente.enviar(&Mensagem::Quadro {
                seq,
                tempo_us: 1_000 * seq as i64,
                chave: seq == 0,
                dados: vec![(seq & 0xFF) as u8; (seq as usize * 997) % 20_000],
            }));
        }
        for esperado in 0..40u64 {
            match esperar(&servidor).1 {
                Mensagem::Quadro { seq, dados, .. } => {
                    assert_eq!(seq, esperado);
                    assert_eq!(dados.len(), (esperado as usize * 997) % 20_000);
                }
                outro => panic!("esperava Quadro, veio {outro:?}"),
            }
        }
    }

    fn esperar(s: &Servidor) -> (u64, Mensagem) {
        let limite = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(v) = s.receber() {
                return v;
            }
            assert!(std::time::Instant::now() < limite, "nada chegou em 5s");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
