//! Injeção de mouse e teclado no host (etapa 6).
//!
//! Usa `SendInput`, que injeta na fila de entrada do sistema como se viesse
//! do hardware. Duas limitações que **não** são contornáveis daqui e definem
//! a Fase 3:
//!
//! 1. Não alcança o Secure Desktop. Durante um prompt de UAC ou na tela de
//!    bloqueio, o Windows troca de desktop e esta injeção some no vazio.
//! 2. Um processo de integridade normal não injeta em janelas de processos
//!    elevados (UIPI). Controlar uma janela aberta como administrador exige
//!    que o host também seja elevado.
//!
//! A arquitetura serviço + agente da decisão D5 existe para resolver as duas.

use pcshare_core::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};

/// Injeta um evento recebido do viewer.
///
/// `screen_w`/`screen_h` são as dimensões do host: as coordenadas chegam em
/// pixels da tela dele e `SendInput` exige o sistema normalizado de 0..65535.
pub fn inject(ev: &InputEvent, screen_w: u32, screen_h: u32) {
    let input = match *ev {
        InputEvent::MouseMove { x, y } => {
            // O denominador é `dimensão - 1`: a normalização mapeia o último
            // pixel em 65535, não o primeiro pixel fora da tela. Errar isso
            // deixa o cursor sem alcançar a borda direita e inferior.
            let nx = normalize(x, screen_w);
            let ny = normalize(y, screen_h);
            mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)
        }
        InputEvent::MouseButton { button, down } => {
            let flags = match (button, down) {
                (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
            };
            mouse(0, 0, 0, flags)
        }
        InputEvent::MouseWheel { delta } => mouse(0, 0, delta, MOUSEEVENTF_WHEEL),
        InputEvent::Key { scancode, down } => {
            let mut flags = KEYEVENTF_SCANCODE;
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            // Scancodes acima de 0xE000 são teclas estendidas (setas, Ctrl e
            // Alt da direita, teclado numérico). Sem a marcação, o Windows as
            // confunde com as equivalentes do bloco numérico.
            if scancode & 0xE000 == 0xE000 {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            key((scancode & 0xFF) as u16, flags)
        }
    };

    unsafe {
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
}

fn normalize(v: i32, extent: u32) -> i32 {
    let max = extent.saturating_sub(1).max(1) as i64;
    ((v.max(0) as i64 * 65535) / max).clamp(0, 65535) as i32
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: Default::default(),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}
