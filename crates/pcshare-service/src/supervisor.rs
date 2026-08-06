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
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_NO_WINDOW, CreateProcessAsUserW, CreateProcessW,
    PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
};
use windows::core::{PCWSTR, PWSTR};

use pcshare_capture::desktop::{self, InputDesktop};

/// Com que frequência se pergunta ao Windows qual é o desktop de entrada.
///
/// É o limite superior do tempo de percepção. Barato de consultar, então vale
/// ser generoso — mas não vale girar em falso: 50 ms dá percepção de no
/// máximo 50 ms contra os 4 a 5 segundos de cegueira que existem hoje.
const INTERVALO: Duration = Duration::from_millis(50);

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

    while !parar.load(Ordering::Relaxed) {
        let desktop_agora = desktop::input_desktop();

        // Distinguir os dois motivos importa: relançar porque o desktop mudou
        // é o funcionamento normal; relançar porque o agente morreu é sintoma
        // de defeito. Confundir os dois escondeu, por um tempo, um agente que
        // nascia e morria 10 vezes por segundo.
        let motivo = match &atual {
            None => Some("primeiro agente"),
            Some(a) if a.desktop != desktop_agora => Some("desktop mudou"),
            Some(a) if !a.vivo() => Some("agente morreu"),
            Some(_) => None,
        };

        if let Some(motivo) = motivo {
            let percebido = Instant::now();

            if let Some(a) = atual.take() {
                let anterior = a.desktop.clone();
                a.encerrar();
                trocas += 1;
                log(&format!(
                    "{motivo}: {} -> {}",
                    anterior.label(),
                    desktop_agora.label()
                ));
            } else {
                log(&format!("{motivo}: {}", desktop_agora.label()));
            }

            match lancar(&agente, &desktop_agora, modo) {
                Ok(a) => {
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
}

impl AgenteVivo {
    fn vivo(&self) -> bool {
        use windows::Win32::System::Threading::GetExitCodeProcess;
        const STILL_ACTIVE: u32 = 259;
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(self.processo, &mut code).is_ok() && code == STILL_ACTIVE }
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
        "\"{}\" --rotulo {alvo} --segundos 0",
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
                // O serviço vive na sessão 0, que não tem tela. Para o agente
                // nascer na sessão de quem está no console, é preciso o token
                // desse usuário — e obtê-lo exige SYSTEM.
                let sessao = WTSGetActiveConsoleSessionId();
                let mut token = HANDLE::default();
                WTSQueryUserToken(sessao, &mut token)
                    .map_err(|e| format!("WTSQueryUserToken(sessão {sessao}): {e}"))?;

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
    })
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
    let linha = format!(
        "[{:>10.3}s] {msg}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64() % 100_000.0)
            .unwrap_or(0.0)
    );
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

pub fn caminho_do_log() -> PathBuf {
    // `ProgramData` porque o serviço roda como SYSTEM e não tem perfil de
    // usuário; escrever em `LOCALAPPDATA` cairia no perfil errado.
    PathBuf::from(std::env::var("ProgramData").unwrap_or_else(|_| "C:\\".into()))
        .join("PCShare")
        .join("supervisor.log")
}
