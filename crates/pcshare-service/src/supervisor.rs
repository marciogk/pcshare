//! O laço que mantém sempre um agente no desktop certo.
//!
//! Regra que a medição estabeleceu (`docs/FASE-3.md`): um processo só duplica
//! a tela quando está preso ao desktop que **recebe entrada**, e um processo
//! nasce preso a um desktop sem poder trocar depois. Logo, quando o desktop de
//! entrada muda, a única resposta é matar o agente e lançar outro.
//!
//! Lançar custa 7,5 ms com o binário quente. O que ainda não se sabia — e este
//! módulo passa a medir — é quanto tempo se leva para **perceber** a troca.
//! Se a percepção for lenta, ela vira o gargalo no lugar do lançamento.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_QUERY,
    TokenPrimary, TokenSessionId, SetTokenInformation,
};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CREATE_SUSPENDED, CreateProcessAsUserW, CreateProcessW,
    GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW,
    TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};

use pcshare_capture::canal;
use pcshare_capture::desktop::{self, InputDesktop};
use pcshare_capture::linha_do_tempo;

/// Com que frequência se pergunta ao Windows qual é o desktop de entrada.
///
/// É o limite superior do tempo de percepção. Barato de consultar, então vale
/// ser generoso — mas não vale girar em falso: 50 ms dá percepção de no
/// máximo 50 ms contra os 4 a 5 segundos de cegueira que existem hoje.
const INTERVALO: Duration = Duration::from_millis(50);
/// Abaixo disto, o agente não viveu — nasceu e morreu.
const VIDA_CURTA: Duration = Duration::from_millis(500);
/// Teto do recuo. Acima disso a percepção de uma troca legítima ficaria lenta
/// demais para valer a pena.
const RECUO_MAXIMO: Duration = Duration::from_secs(3);
/// Teto de agentes vivos ao mesmo tempo.
///
/// Na prática o Windows tem poucos desktops de entrada — `Default`,
/// `Winlogon`, `Screen-saver`. O limite existe para que um nome inesperado não
/// faça a coleção crescer sem fim.
const MAX_AGENTES: usize = 4;

/// Como o agente é lançado.
#[derive(Clone, Copy, PartialEq)]
pub enum Modo {
    /// Do próprio usuário, para experimentar sem instalar nada.
    Console,
    /// Da sessão 0, como SYSTEM, roubando o token do usuário da sessão ativa.
    /// É o único caminho que alcança o `Winlogon`.
    Servico,
}

pub fn supervisionar(modo: Modo, parar: &AtomicBool) {
    let Some(agente) = caminho_do_agente() else {
        log("pcshare-agent.exe não encontrado ao lado do executável");
        return;
    };
    log(&format!("supervisor iniciado ({} )", rotulo_modo(modo)));
    log(&format!("agente: {}", agente.display()));

    // Um agente por desktop, **vivos ao mesmo tempo**.
    //
    // Matar e recriar a cada troca punha o lançamento no caminho crítico, e a
    // medição mostrou que criar o agente com token de usuário custa cerca de
    // 700 ms. Mantendo-os vivos, o agente do desktop que saiu apenas fica cego
    // e volta sozinho quando o desktop retorna — sem custo de criação.
    let mut agentes: std::collections::HashMap<String, AgenteVivo> =
        std::collections::HashMap::new();
    // Zera a linha do tempo a cada execução. Sem isso, medições de sessões
    // diferentes se misturam — e como os arquivos nascem de SYSTEM e de
    // usuário, ninguém consegue limpar todos manualmente depois.
    limpar_linha_do_tempo();

    // Se este descritor fechar — inclusive por morte violenta do supervisor —
    // o Windows encerra todos os agentes junto.
    let grupo = criar_grupo_de_agentes();
    if grupo.is_none() {
        log("AVISO: sem grupo de processos; agentes podem sobreviver ao supervisor");
    }

    let (tx, rx) = std::sync::mpsc::channel::<String>();
    // Só os processos que este supervisor lançar falam pelo cano. Ver
    // `canal::Autorizados`: sem isto, qualquer programa do usuário poderia ler
    // os quadros do `Winlogon`.
    let autorizados = canal::Autorizados::default();
    let servidor = canal::Servidor::abrir(canal::CANO_PADRAO, autorizados.clone());
    log("canal aberto para os agentes");

    // A sessão de rede leva o `Servidor` embora: a partir daqui é ela que fala
    // com os agentes, e o que o supervisor ainda precisa saber — trocas de
    // desktop — volta por `rx`.
    crate::sessao::iniciar(servidor, tx, endereco_de_escuta(), senha_da_sessao());


    // O alvo começa no desktop do usuário, que é o caso comum. A partir daí
    // quem manda é o agente: o supervisor **não** consulta o desktop.
    //
    // Não é preguiça, é impossibilidade. Rodando na sessão 0, que tem estação
    // de janelas própria, `OpenInputDesktop` sempre falha aqui. Uma versão
    // anterior lia essa falha como "Secure Desktop ativo" e ficou relançando
    // um agente do Winlogon a cada 3 segundos, para sempre, mesmo com o
    // usuário de volta ao desktop normal.
    let mut alvo = "Default".to_string();
    let mut trocas = 0u64;
    let mut mortes_rapidas = 0u32;

    while !parar.load(Ordering::Relaxed) {
        // ---- 1. o que os agentes relataram pelo canal
        while let Ok(nome) = rx.try_recv() {
            if nome != alvo {
                log(&format!("agente relatou: desktop de entrada é '{nome}'"));
                alvo = nome;
                trocas += 1;
            }
        }

        // ---- 2. recolhe os que morreram
        let mortos: Vec<String> = agentes
            .iter()
            .filter(|(_, a)| !a.vivo())
            .map(|(n, _)| n.clone())
            .collect();
        for nome in mortos {
            if let Some(a) = agentes.remove(&nome) {
                // Código de saída ainda é aceito: é o caminho de reserva do
                // agente quando o canal não está disponível.
                if let Some(novo) = a.codigo_de_saida().and_then(desktop::codigo::para) {
                    let n = nome_do_desktop(&novo).to_string();
                    if n != alvo {
                        log(&format!("agente de '{nome}' encerrou relatando '{n}'"));
                        alvo = n;
                        trocas += 1;
                    }
                }
                if a.nasceu.elapsed() < VIDA_CURTA {
                    mortes_rapidas += 1;
                } else {
                    mortes_rapidas = 0;
                }
                log(&format!("agente de '{nome}' encerrou"));
                // O Windows reaproveita PIDs; deixar o número autorizado abriria
                // a porta para um processo qualquer que herdasse esse PID.
                autorizados.revogar(a.pid);
                a.encerrar();
            }
        }

        // ---- 3. garante um agente vivo para o desktop ativo
        if !agentes.contains_key(&alvo) {
            if mortes_rapidas > 0 {
                let espera = INTERVALO * (1u32 << mortes_rapidas.min(6));
                std::thread::sleep(espera.min(RECUO_MAXIMO));
            }
            let desejado = desktop_por_nome(&alvo);
            let percebido = Instant::now();
            let _ = linha_do_tempo::registrar("supervisor", "percebeu");

            match lancar(&agente, &desejado, modo) {
                Ok(a) => {
                    if let Some(job) = grupo {
                        use windows::Win32::System::JobObjects::AssignProcessToJobObject;
                        unsafe {
                            if AssignProcessToJobObject(job, a.processo).is_err() {
                                log("AVISO: agente não entrou no grupo; pode ficar órfão");
                            }
                        }
                    }
                    // Autorizar e só então soltar: o agente ainda não executou
                    // nenhuma instrução, então não há corrida com a conexão.
                    autorizados.permitir(a.pid);
                    a.liberar();
                    let _ = linha_do_tempo::registrar("supervisor", &format!("lancou-{alvo}"));
                    log(&format!(
                        "agente lançado em '{alvo}' (pid {}) — {:.1} ms desde a percepção",
                        a.pid,
                        percebido.elapsed().as_secs_f64() * 1000.0
                    ));
                    agentes.insert(alvo.clone(), a);
                }
                Err(e) => {
                    log(&format!("não foi possível lançar em '{alvo}': {e}"));
                    std::thread::sleep(INTERVALO);
                }
            }
        }

        // ---- 4. limita quantos ficam vivos
        if agentes.len() > MAX_AGENTES
            && let Some(sacrificado) = agentes
                .iter()
                .filter(|(n, _)| *n != &alvo)
                .min_by_key(|(_, a)| a.nasceu)
                .map(|(n, _)| n.clone())
            && let Some(a) = agentes.remove(&sacrificado)
        {
            log(&format!("encerrando agente ocioso de '{sacrificado}'"));
            autorizados.revogar(a.pid);
            a.encerrar();
        }

        std::thread::sleep(INTERVALO);
    }

    for (nome, a) in agentes.drain() {
        log(&format!("encerrando agente de '{nome}'"));
        a.encerrar();
    }
    log(&format!("supervisor encerrado ({trocas} trocas de desktop)"));
}


struct AgenteVivo {
    processo: HANDLE,
    thread: HANDLE,
    pid: u32,
    desktop: InputDesktop,
    nasceu: Instant,
}

impl AgenteVivo {
    fn vivo(&self) -> bool {
        self.codigo_bruto().is_none()
    }

    /// `None` enquanto o processo roda.
    fn codigo_bruto(&self) -> Option<u32> {
        use windows::Win32::System::Threading::GetExitCodeProcess;
        const STILL_ACTIVE: u32 = 259;
        let mut code = 0u32;
        unsafe {
            if GetExitCodeProcess(self.processo, &mut code).is_ok() && code != STILL_ACTIVE {
                Some(code)
            } else {
                None
            }
        }
    }

    fn codigo_de_saida(&self) -> Option<u32> {
        self.codigo_bruto()
    }

    /// Solta o agente, que nasce suspenso.
    ///
    /// Nascer suspenso não é detalhe: o PID precisa entrar na lista de
    /// autorizados **antes** da primeira instrução do agente, ou ele conecta
    /// no cano e é recusado por uma corrida que só apareceria de vez em quando.
    fn liberar(&self) {
        unsafe {
            // -1 significa falha; qualquer outro valor é a contagem anterior.
            if ResumeThread(self.thread) == u32::MAX {
                log(&format!("AVISO: agente {} não saiu da suspensão", self.pid));
            }
        }
    }

    fn encerrar(self) {
        unsafe {
            let _ = TerminateProcess(self.processo, 0);
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.processo);
        }
    }
}

fn lancar(exe: &Path, desktop: &InputDesktop, modo: Modo) -> Result<AgenteVivo, String> {
    let alvo = nome_do_desktop(desktop);
    // Da sessão 0 o nome precisa incluir a estação de janelas; de dentro da
    // sessão do usuário, o nome do desktop basta.
    let caminho_desktop = match modo {
        Modo::Servico => format!("WinSta0\\{alvo}"),
        Modo::Console => alvo.to_string(),
    };

    let mut linha = wide(&format!(
        "\"{}\" --rotulo {alvo} --desktop {alvo} --segundos 0",
        exe.display()
    ));
    let mut desk = wide(&caminho_desktop);

    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desk.as_mut_ptr()),
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();

    let r = unsafe {
        match modo {
            Modo::Console => CreateProcessW(
                PCWSTR::null(),
                Some(PWSTR(linha.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW | CREATE_SUSPENDED,
                None,
                PCWSTR::null(),
                &si,
                &mut pi,
            ),
            Modo::Servico => {
                // O serviço vive na sessão 0, que não tem tela. O agente
                // precisa nascer na sessão de quem está no console — e com o
                // token certo, que depende do desktop de destino.
                let sessao = WTSGetActiveConsoleSessionId();
                let token = if precisa_de_system(desktop) {
                    // O `Winlogon` pertence ao Windows, não ao usuário.
                    //
                    // Passar o token do usuário cria o processo — o
                    // `CreateProcessAsUserW` até devolve um PID — mas ele
                    // morre antes de executar a primeira instrução, porque
                    // não tem acesso ao desktop. Observado: o agente lançado
                    // no Winlogon nunca chegou a escrever uma linha de log.
                    //
                    // A saída é duplicar o token do próprio serviço (SYSTEM)
                    // e mudá-lo para a sessão do console: dá um processo
                    // SYSTEM dentro da sessão do usuário, que alcança o
                    // Secure Desktop.
                    token_system_na_sessao(sessao)?
                } else {
                    // No desktop do usuário, o token dele é o certo: respeita
                    // suas permissões e seu perfil.
                    let mut t = HANDLE::default();
                    WTSQueryUserToken(sessao, &mut t)
                        .map_err(|e| format!("WTSQueryUserToken(sessão {sessao}): {e}"))?;
                    t
                };

                let resultado = CreateProcessAsUserW(
                    Some(token),
                    PCWSTR::null(),
                    Some(PWSTR(linha.as_mut_ptr())),
                    None,
                    None,
                    false,
                    CREATE_NEW_CONSOLE | CREATE_SUSPENDED,
                    None,
                    PCWSTR::null(),
                    &si,
                    &mut pi,
                );
                let _ = CloseHandle(token);
                resultado
            }
        }
    };

    r.map_err(|e| e.to_string())?;
    Ok(AgenteVivo {
        processo: pi.hProcess,
        thread: pi.hThread,
        pid: pi.dwProcessId,
        desktop: desktop.clone(),
        nasceu: Instant::now(),
    })
}

/// O desktop pertence ao Windows, e não ao usuário logado?
fn precisa_de_system(d: &InputDesktop) -> bool {
    matches!(
        d,
        InputDesktop::Winlogon | InputDesktop::Denied | InputDesktop::ScreenSaver
    )
}

/// Cópia do token deste serviço (SYSTEM), realocada para `sessao`.
///
/// Um token traz a sessão gravada dentro dele. O do serviço aponta para a
/// sessão 0, que não tem tela; mudá-lo para a do console é o que faz o
/// processo nascer onde há monitor, mantendo os privilégios de SYSTEM.
unsafe fn token_system_na_sessao(sessao: u32) -> Result<HANDLE, String> {
    unsafe {
        let mut atual = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut atual,
        )
        .map_err(|e| format!("OpenProcessToken: {e}"))?;

        let mut copia = HANDLE::default();
        let dup = DuplicateTokenEx(
            atual,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            // Precisa ser primário: token de impersonação não cria processo.
            TokenPrimary,
            &mut copia,
        );
        let _ = CloseHandle(atual);
        dup.map_err(|e| format!("DuplicateTokenEx: {e}"))?;

        SetTokenInformation(
            copia,
            TokenSessionId,
            &sessao as *const u32 as *const std::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .map_err(|e| {
            let _ = CloseHandle(copia);
            format!("SetTokenInformation(sessão {sessao}): {e}")
        })?;

        Ok(copia)
    }
}

/// Objeto de trabalho que leva os agentes junto quando o supervisor morre.
///
/// Sem isto, matar o supervisor — ou ele travar — deixa os agentes vivos,
/// **capturando a tela indefinidamente**, sem ninguém para encerrá-los.
/// Observado: um agente SYSTEM sobreviveu ao supervisor e só saiu com
/// privilégio de administrador.
///
/// Num produto de acesso remoto isso é inaceitável: processo órfão que lê a
/// tela é exatamente o que um usuário teme ao instalar software do tipo.
///
/// `KILL_ON_JOB_CLOSE` faz o Windows encerrar todos os processos do grupo
/// quando o último descritor dele fecha — o que acontece mesmo se o supervisor
/// for morto à força, porque quem fecha é o sistema.
fn criar_grupo_de_agentes() -> Option<HANDLE> {
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    unsafe {
        let job = CreateJobObjectW(None, PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .is_err()
        {
            let _ = CloseHandle(job);
            return None;
        }
        Some(job)
    }
}

fn limpar_linha_do_tempo() {
    let pasta = linha_do_tempo::pasta();
    let Ok(entradas) = std::fs::read_dir(&pasta) else {
        return;
    };
    let mut apagados = 0;
    for e in entradas.flatten() {
        if std::fs::remove_file(e.path()).is_ok() {
            apagados += 1;
        }
    }
    if apagados > 0 {
        log(&format!("linha do tempo zerada ({apagados} arquivos)"));
    }
}

/// Caminho inverso de [`nome_do_desktop`], para reconstruir o alvo a partir do
/// que o agente relatou pelo canal.
fn desktop_por_nome(nome: &str) -> InputDesktop {
    match nome {
        "Default" => InputDesktop::Default,
        "Winlogon" => InputDesktop::Winlogon,
        "Screen-saver" => InputDesktop::ScreenSaver,
        outro => InputDesktop::Other(outro.to_string()),
    }
}

/// Nome do desktop para o Windows.
///
/// `Denied` significa que não conseguimos sequer consultar, o que na prática é
/// o Secure Desktop. Apostar em `Winlogon` é o palpite certo — e se estiver
/// errado, o lançamento falha e o supervisor tenta de novo no ciclo seguinte.
fn nome_do_desktop(d: &InputDesktop) -> &str {
    match d {
        InputDesktop::Default => "Default",
        InputDesktop::Winlogon | InputDesktop::Denied => "Winlogon",
        InputDesktop::ScreenSaver => "Screen-saver",
        InputDesktop::Other(n) => n,
    }
}

/// Onde a sessão escuta. Um serviço não tem linha de comando útil depois de
/// instalado, então a configuração vem do ambiente.
fn endereco_de_escuta() -> std::net::SocketAddr {
    const PADRAO: &str = "0.0.0.0:7000";
    let bruto = std::env::var("PCSHARE_BIND").unwrap_or_else(|_| PADRAO.into());
    bruto.parse().unwrap_or_else(|_| {
        log(&format!("PCSHARE_BIND inválido ('{bruto}'); usando {PADRAO}"));
        PADRAO.parse().expect("endereço padrão é válido")
    })
}

/// Senha da sessão.
///
/// Sorteada quando não vem do ambiente, e registrada no log — que é o único
/// lugar onde alguém pode lê-la num serviço sem interface. Isto é arranjo de
/// desenvolvimento, não de produto: um serviço de verdade sorteia a cada
/// atendimento e mostra a senha na tela da máquina que está sendo assistida.
fn senha_da_sessao() -> String {
    std::env::var("PCSHARE_SENHA").unwrap_or_else(|_| pcshare_core::secure::random_password())
}

fn rotulo_modo(m: Modo) -> &'static str {
    match m {
        Modo::Console => "modo console, sem elevação",
        Modo::Servico => "modo serviço, SYSTEM",
    }
}

fn caminho_do_agente() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let p = dir.join("pcshare-agent.exe");
    p.exists().then_some(p)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Um serviço não tem console: tudo o que importa precisa ir para arquivo.
pub fn log(msg: &str) {
    // Data e hora, e não segundos desde uma origem arbitrária: a versão
    // anterior usava o relógio do sistema módulo 100.000, e o contador virava
    // no meio de um teste — uma linha de 99.981s seguida de outra de 10s, sem
    // explicação para quem lesse o log depois.
    let agora = std::time::SystemTime::now();
    let linha = format!("[{}] {msg}\n", formatar_hora(agora));
    print!("{linha}");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    let escreveu = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(caminho_do_log())
        .and_then(|mut f| f.write_all(linha.as_bytes()))
        .is_ok();

    // Falhar em silêncio aqui já custou caro: o arquivo da linha do tempo era
    // criado por SYSTEM com `Usuários:(RX)`, e os processos em contexto de
    // usuário perdiam metade dos eventos sem que nada indicasse isso.
    //
    // O mesmo vale para este log: rodando `supervisor` em modo console depois
    // de o serviço ter criado o arquivo, o usuário não tem permissão de
    // escrita, e tudo o que sobra é a saída no console — que ninguém guarda.
    if !escreveu && !AVISOU_DO_LOG.swap(true, Ordering::Relaxed) {
        print!(
            "AVISO: não consigo escrever em {} — só o console verá estas linhas.\n\
             Provavelmente o arquivo pertence a SYSTEM; apague-o como administrador.\n",
            caminho_do_log().display()
        );
        let _ = std::io::stdout().flush();
    }
}

/// Para que o aviso saia uma vez, e não a cada linha de log.
static AVISOU_DO_LOG: AtomicBool = AtomicBool::new(false);

/// `HH:MM:SS.mmm` em horário local, sem depender de biblioteca de datas.
fn formatar_hora(t: std::time::SystemTime) -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let _ = t;
    let s = unsafe { GetLocalTime() };
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        s.wHour, s.wMinute, s.wSecond, s.wMilliseconds
    )
}

pub fn caminho_do_log() -> PathBuf {
    // `ProgramData` porque o serviço roda como SYSTEM e não tem perfil de
    // usuário; escrever em `LOCALAPPDATA` cairia no perfil errado.
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("supervisor.log")
}
