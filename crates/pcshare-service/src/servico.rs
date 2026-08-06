//! Integração com o Gerenciador de Serviços do Windows.
//!
//! O serviço existe por um motivo só: ele roda como **SYSTEM na sessão 0**, e
//! é o único contexto capaz de obter o token do usuário do console
//! (`WTSQueryUserToken`) e de abrir o desktop `Winlogon`. Um processo comum
//! recebe acesso negado nos dois casos — medido, ver `docs/FASE-3.md`.
//!
//! Instalado como **início manual**, de propósito: enquanto o produto é
//! protótipo, ninguém quer um serviço não assinado subindo sozinho a cada
//! boot.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use windows::Win32::System::Services::{
    CloseServiceHandle, CreateServiceW, DeleteService, OpenSCManagerW, OpenServiceW,
    RegisterServiceCtrlHandlerW, SERVICE_ALL_ACCESS, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DEMAND_START, SERVICE_ERROR_NORMAL, SERVICE_RUNNING,
    SERVICE_START_PENDING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOP_PENDING,
    SERVICE_STOPPED, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, SC_MANAGER_ALL_ACCESS,
    SetServiceStatus, StartServiceCtrlDispatcherW,
};
use windows::core::{PCWSTR, PWSTR, w};

use crate::supervisor::{self, Modo};

pub const NOME: PCWSTR = w!("PCShareSupervisor");
const EXIBICAO: PCWSTR = w!("PCShare — supervisor de agentes");

static PARAR: AtomicBool = AtomicBool::new(false);
/// `SERVICE_STATUS_HANDLE` guardado como inteiro: o manipulador de controle é
/// uma função livre chamada pelo Windows, sem `self` para carregar estado.
static STATUS: AtomicUsize = AtomicUsize::new(0);

/// Entrega o processo ao Gerenciador de Serviços.
///
/// Só funciona quando o Windows iniciou o processo como serviço; rodar isto
/// pela linha de comando falha, e a falha é a forma de descobrir que se está
/// no contexto errado.
pub fn rodar_como_servico() -> Result<(), String> {
    let tabela = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(NOME.0 as *mut u16),
            lpServiceProc: Some(main_do_servico),
        },
        SERVICE_TABLE_ENTRYW::default(),
    ];
    unsafe { StartServiceCtrlDispatcherW(tabela.as_ptr()).map_err(|e| e.to_string()) }
}

unsafe extern "system" fn main_do_servico(_argc: u32, _argv: *mut PWSTR) {
    unsafe {
        let Ok(h) = RegisterServiceCtrlHandlerW(NOME, Some(manipulador)) else {
            return;
        };
        STATUS.store(h.0 as usize, Ordering::SeqCst);

        anunciar(SERVICE_START_PENDING.0, 0);
        anunciar(SERVICE_RUNNING.0, 0);

        supervisor::log("serviço iniciado como SYSTEM");
        supervisor::supervisionar(Modo::Servico, &PARAR);

        anunciar(SERVICE_STOPPED.0, 0);
    }
}

unsafe extern "system" fn manipulador(controle: u32) {
    if controle == SERVICE_CONTROL_STOP || controle == SERVICE_CONTROL_SHUTDOWN {
        supervisor::log("parada solicitada pelo Windows");
        // Avisa que a parada está em curso antes de pedir ao laço que saia,
        // senão o Windows considera o serviço travado.
        unsafe { anunciar(SERVICE_STOP_PENDING.0, 3000) };
        PARAR.store(true, Ordering::SeqCst);
    }
}

unsafe fn anunciar(estado: u32, espera_ms: u32) {
    let h = STATUS.load(Ordering::SeqCst);
    if h == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE(estado),
        dwControlsAccepted: if estado == SERVICE_RUNNING.0 {
            windows::Win32::System::Services::SERVICE_ACCEPT_STOP
                | windows::Win32::System::Services::SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWaitHint: espera_ms,
        ..Default::default()
    };
    unsafe {
        let _ = SetServiceStatus(SERVICE_STATUS_HANDLE(h as *mut _), &status);
    }
}

pub fn instalar() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // Aspas porque o caminho tem espaço ("C:\Projeto Pcshare\..."), e sem elas
    // o Gerenciador de Serviços interpreta o resto como argumento.
    let caminho: Vec<u16> = format!("\"{}\"", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("OpenSCManager: {e} (é preciso ser administrador)"))?;

        let r = CreateServiceW(
            scm,
            NOME,
            EXIBICAO,
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            // Início manual enquanto isto é protótipo.
            SERVICE_DEMAND_START,
            SERVICE_ERROR_NORMAL,
            PCWSTR(caminho.as_ptr()),
            PCWSTR::null(),
            None,
            PCWSTR::null(),
            PCWSTR::null(),
            PCWSTR::null(),
        );
        let saida = match r {
            Ok(h) => {
                let _ = CloseServiceHandle(h);
                Ok(())
            }
            Err(e) => Err(format!("CreateService: {e}")),
        };
        let _ = CloseServiceHandle(scm);
        saida
    }
}

pub fn desinstalar() -> Result<(), String> {
    unsafe {
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("OpenSCManager: {e} (é preciso ser administrador)"))?;
        let saida = match OpenServiceW(scm, NOME, SERVICE_ALL_ACCESS) {
            Ok(svc) => {
                let r = DeleteService(svc).map_err(|e| format!("DeleteService: {e}"));
                let _ = CloseServiceHandle(svc);
                r
            }
            Err(e) => Err(format!("OpenService: {e}")),
        };
        let _ = CloseServiceHandle(scm);
        saida
    }
}
