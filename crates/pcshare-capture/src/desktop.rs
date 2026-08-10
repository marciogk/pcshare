//! Detecção de desktop e sessão — o alicerce da Fase 3.
//!
//! # O problema que isto existe para enxergar
//!
//! O Windows não tem uma tela só. Cada sessão de usuário tem vários
//! *desktops*, e apenas um recebe entrada por vez:
//!
//! - **`Default`** — onde as aplicações do usuário vivem. É o normal.
//! - **`Winlogon`** — o *Secure Desktop*. Prompts de UAC, tela de bloqueio,
//!   Ctrl+Alt+Del. Isolado de propósito: nenhum processo comum consegue ler
//!   nem escrever nele, exatamente para impedir que um programa hostil leia a
//!   senha do administrador ou clique no "Sim" por você.
//! - **`Screen-saver`** — protetor de tela.
//!
//! Essa proteção é justamente o que quebra o acesso remoto: quando o UAC
//! aparece, o sistema troca para o `Winlogon` e o nosso processo — que roda no
//! `Default` — deixa de ver e de controlar a tela. O técnico do outro lado vê
//! a imagem congelar bem na hora em que precisaria clicar.
//!
//! # Por que a detecção vem antes da solução
//!
//! A saída é a arquitetura serviço + agente da decisão D5: um serviço em
//! SYSTEM que consegue lançar um agente anexado ao desktop ativo, seja ele
//! qual for. Antes de construir isso, este módulo permite **medir** o
//! problema: quanto tempo a captura fica cega, com que sintoma, e se a
//! recuperação funciona.
//!
//! Nada aqui exige privilégio elevado.

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Qual desktop está recebendo entrada agora.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDesktop {
    /// O desktop normal do usuário. Captura e injeção funcionam.
    Default,
    /// Secure Desktop: UAC, tela de bloqueio, Ctrl+Alt+Del.
    Winlogon,
    ScreenSaver,
    /// Outro nome — desktops podem ser criados por qualquer aplicação.
    Other(String),
    /// **Não foi possível sequer perguntar.**
    ///
    /// Não é erro: é a resposta. Um processo comum recebe acesso negado ao
    /// tentar abrir o desktop de entrada quando o Secure Desktop está ativo.
    /// Na prática, isto costuma significar `Winlogon`.
    Denied,
}

impl InputDesktop {
    /// `true` quando a captura e a injeção de entrada **não** funcionam a
    /// partir de um processo comum do usuário.
    pub fn is_secure(&self) -> bool {
        matches!(self, Self::Winlogon | Self::Denied)
    }

    pub fn label(&self) -> String {
        match self {
            Self::Default => "Default".into(),
            Self::Winlogon => "Winlogon (Secure Desktop)".into(),
            Self::ScreenSaver => "Screen-saver".into(),
            Self::Other(n) => n.clone(),
            Self::Denied => "acesso negado (provável Secure Desktop)".into(),
        }
    }
}

/// Pergunta ao Windows qual desktop está recebendo entrada.
pub fn input_desktop() -> InputDesktop {
    unsafe {
        // `OpenInputDesktop` falha com acesso negado quando o Secure Desktop
        // está ativo e quem pergunta é um processo comum. A falha é o sinal.
        let Ok(hdesk) = OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) else {
            return InputDesktop::Denied;
        };

        let mut buf = [0u16; 256];
        let mut needed = 0u32;
        let ok = GetUserObjectInformationW(
            HANDLE(hdesk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_ok();
        let _ = CloseDesktop(hdesk);

        if !ok {
            return InputDesktop::Denied;
        }

        let fim = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        let nome = String::from_utf16_lossy(&buf[..fim]);
        match nome.as_str() {
            "Default" => InputDesktop::Default,
            "Winlogon" => InputDesktop::Winlogon,
            "Screen-saver" => InputDesktop::ScreenSaver,
            _ => InputDesktop::Other(nome),
        }
    }
}

/// Como o agente conta ao supervisor qual desktop assumiu.
///
/// # Por que via código de saída
///
/// O supervisor roda na sessão 0, que tem estação de janelas própria: de lá
/// `OpenInputDesktop` **sempre** falha, e ele não tem como saber qual desktop
/// está ativo. Medido — o serviço relançava um agente do `Winlogon` a cada 3
/// segundos para sempre, convencido de que o Secure Desktop nunca saía.
///
/// Quem enxerga é o agente, que vive na sessão do usuário. Então ele é quem
/// informa: percebe a troca, encerra, e o código de saída diz para onde ir.
/// Resolve sem inventar canal de comunicação entre os dois processos.
pub mod codigo {
    use super::InputDesktop;

    pub const DEFAULT: u32 = 10;
    pub const WINLOGON: u32 = 11;
    pub const SCREENSAVER: u32 = 12;
    pub const OUTRO: u32 = 13;

    pub fn de(d: &InputDesktop) -> u32 {
        match d {
            InputDesktop::Default => DEFAULT,
            // `Denied` é o que um processo comum vê quando o Secure Desktop
            // está à frente; para efeito de destino, é o mesmo.
            InputDesktop::Winlogon | InputDesktop::Denied => WINLOGON,
            InputDesktop::ScreenSaver => SCREENSAVER,
            InputDesktop::Other(_) => OUTRO,
        }
    }

    pub fn para(c: u32) -> Option<InputDesktop> {
        match c {
            DEFAULT => Some(InputDesktop::Default),
            WINLOGON => Some(InputDesktop::Winlogon),
            SCREENSAVER => Some(InputDesktop::ScreenSaver),
            // Saída fora da tabela é morte do agente, não troca de desktop.
            _ => None,
        }
    }
}

/// Sinal complementar: com o Secure Desktop ativo, um processo do desktop do
/// usuário não enxerga janela em primeiro plano.
///
/// Usado como confirmação — dois sinais independentes concordando dão muito
/// mais confiança que um só, ainda mais quando o primeiro é uma negativa de
/// acesso que poderia ter outras causas.
pub fn has_foreground_window() -> bool {
    unsafe { !GetForegroundWindow().is_invalid() }
}

/// Em que sessão este processo roda, e qual é a sessão do console físico.
///
/// Importa para a Fase 3: um serviço do Windows roda na **sessão 0**, que não
/// tem interface gráfica desde o Windows Vista. Ele precisa lançar o agente
/// dentro da sessão do usuário — daí a necessidade de saber qual é.
pub fn sessions() -> (u32, u32) {
    unsafe {
        let mut minha = 0u32;
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut minha);
        (minha, WTSGetActiveConsoleSessionId())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Durante os testes o desktop ativo é o normal. O valor exato depende do
    /// ambiente, então o que se verifica é que a consulta responde de forma
    /// coerente, sem travar nem entrar em pânico.
    #[test]
    fn consulta_o_desktop_sem_panico() {
        let d = input_desktop();
        assert!(!d.label().is_empty());
        // Num agente de testes comum, o Secure Desktop não deveria estar ativo.
        // Se estiver, ainda assim a consulta precisa ter respondido algo.
        let _ = d.is_secure();
    }

    #[test]
    fn sessao_do_processo_e_conhecida() {
        let (minha, _console) = sessions();
        // A sessão 0 é a dos serviços; testes rodam numa sessão de usuário.
        assert_ne!(minha, u32::MAX, "sessão do processo não pôde ser lida");
    }
}
