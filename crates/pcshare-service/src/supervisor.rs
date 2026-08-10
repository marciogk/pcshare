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
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CreateProcessAsUserW, CreateProcessW, GetCurrentProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};

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

    let mut atual: Option<AgenteVivo> = None;
    let mut trocas = 0u64;
    let mut mortes_rapidas = 0u32;

    // O alvo começa no desktop do usuário, que é o caso comum. A partir daí
    // quem manda é o agente: o supervisor **não** consulta o desktop.
    //
    // Não é preguiça, é impossibilidade. Rodando na sessão 0, que tem estação
    // de janelas própria, `OpenInputDesktop` sempre falha aqui. A versão
    // anterior lia essa falha como "Secure Desktop ativo" e ficou relançando
    // um agente do Winlogon a cada 3 segundos, para sempre, mesmo com o
    // usuário de volta ao desktop normal.
    let mut alvo = InputDesktop::Default;

    while !parar.load(Ordering::Relaxed) {
        // Distinguir os dois motivos importa: relançar porque o desktop mudou
        // é o funcionamento normal; relançar porque o agente morreu é sintoma
        // de defeito. Confundir os dois escondeu, por um tempo, um agente que
        // nascia e morria 10 vezes por segundo.
        let mut motivo = match &atual {
            None => Some("primeiro agente"),
            Some(a) if !a.vivo() => Some("agente encerrou"),
            Some(_) => None,
        };

        // O código de saída é o recado: dentro da tabela significa "o desktop
        // de entrada virou este"; fora dela, o agente simplesmente morreu.
        if motivo.is_some()
            && let Some(a) = &atual
            && let Some(novo) = a.codigo_de_saida().and_then(desktop::codigo::para)
            && novo != alvo
        {
            alvo = novo;
            motivo = Some("agente informou troca de desktop");
        }
        let desktop_agora = alvo.clone();

        if let Some(motivo) = motivo {
            let percebido = Instant::now();
            // Sem este carimbo, o intervalo entre a saída do agente e a
            // percepção do supervisor fica invisível — e foi exatamente ali
            // que apareceram 565 ms sem explicação na primeira medição.
            if !linha_do_tempo::registrar("supervisor", "percebeu") {
                log("AVISO: não foi possível escrever na linha do tempo");
            }

            if let Some(a) = atual.take() {
                let anterior = a.desktop.clone();
                // Morrer logo ao nascer é sintoma de defeito, não de troca de
                // desktop. Sem recuo, o supervisor vira uma bomba de
                // processos: observado criando ~20 por segundo e engordando o
                // log em 1,8 MB em poucos minutos.
                if !a.vivo() && a.nasceu.elapsed() < VIDA_CURTA {
                    mortes_rapidas += 1;
                } else {
                    mortes_rapidas = 0;
                }
                a.encerrar();
                trocas += 1;
                if mortes_rapidas <= 3 || mortes_rapidas % 20 == 0 {
                    log(&format!(
                        "{motivo}: {} para {}{}",
                        anterior.label(),
                        desktop_agora.label(),
                        if mortes_rapidas > 0 {
                            format!(" (morte rápida #{mortes_rapidas})")
                        } else {
                            String::new()
                        }
                    ));
                }
            } else {
                log(&format!("{motivo}: {}", desktop_agora.label()));
            }

            // Recuo exponencial, com teto. Dá tempo de o desktop voltar ao
            // normal sozinho em vez de insistir num alvo que não aceita
            // agente nenhum.
            if mortes_rapidas > 0 {
                let espera = INTERVALO * (1u32 << mortes_rapidas.min(6));
                std::thread::sleep(espera.min(RECUO_MAXIMO));
            }

            match lancar(&agente, &desktop_agora, modo) {
                Ok(a) => {
                    // Fecha o outro trecho cego: daqui até o agente escrever
                    // a primeira linha é custo de criação de processo, que
                    // ninguém estava medindo.
                    let _ = linha_do_tempo::registrar(
                        "supervisor",
                        &format!("lancou-{}", nome_do_desktop(&desktop_agora)),
                    );
                    log(&format!(
                        "agente lançado em '{}' (pid {}) — {:.1} ms desde a percepção",
                        nome_do_desktop(&desktop_agora),
                        a.pid,
                        percebido.elapsed().as_secs_f64() * 1000.0
                    ));
                    atual = Some(a);
                }
                Err(e) => {
                    // Falhar aqui é esperado no `Winlogon` sem privilégio, e
                    // não pode derrubar o supervisor: o desktop volta ao
                    // normal e a próxima tentativa dá certo.
                    log(&format!(
                        "não foi possível lançar em '{}': {e}",
                        nome_do_desktop(&desktop_agora)
                    ));
                    std::thread::sleep(INTERVALO);
                }
            }
        }

        std::thread::sleep(INTERVALO);
    }

    if let Some(a) = atual.take() {
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
                CREATE_NO_WINDOW,
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
                    CREATE_NEW_CONSOLE,
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

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(caminho_do_log())
    {
        let _ = f.write_all(linha.as_bytes());
    }
}

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
