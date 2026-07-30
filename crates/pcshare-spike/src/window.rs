//! Janela mínima do viewer.
//!
//! GDI com `StretchDIBits`, deliberadamente. Não é o caminho de apresentação
//! do produto — Direct3D com swapchain é — mas é o menor código que põe uma
//! imagem na tela, e o Sprint 0 precisa da imagem, não da apresentação ótima.
//!
//! Consequência a lembrar ao ler a latência medida: o blit por GDI acrescenta
//! custo próprio, e ele **está incluído** no número glass-to-glass. O produto
//! deve ficar abaixo do que medirmos aqui, não acima.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, HDC, InvalidateRect, ReleaseDC,
    SRCCOPY, StretchDIBits, ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

/// O `wndproc` é uma função livre chamada pelo Windows; não há `self` para
/// carregar estado. Como o spike tem exatamente uma janela, uma flag global
/// resolve sem introduzir mapa de HWND.
static CLOSED: AtomicBool = AtomicBool::new(false);

pub struct Window {
    hwnd: HWND,
    hdc: HDC,
}

impl Window {
    pub fn new(title: PCWSTR, width: i32, height: i32) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("PcshareViewerWindow");

            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: instance.into(),
                lpszClassName: class,
                hCursor: LoadCursorW(None, IDC_ARROW)?,
                ..Default::default()
            };
            // Retorno 0 pode significar "já registrada" numa segunda execução
            // no mesmo processo; só falha de verdade na criação da janela.
            RegisterClassW(&wc);

            // Ajusta para que a *área cliente* tenha o tamanho pedido, e não a
            // janela com bordas — senão a imagem sai esticada.
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            let _ = AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false);

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                class,
                title,
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                rect.right - rect.left,
                rect.bottom - rect.top,
                None,
                None,
                Some(instance.into()),
                None,
            )?;

            CLOSED.store(false, Ordering::SeqCst);
            let hdc = GetDC(Some(hwnd));
            Ok(Self { hwnd, hdc })
        }
    }

    /// Processa a fila de mensagens. Devolve `false` quando o usuário fechou.
    pub fn pump(&mut self) -> bool {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        !CLOSED.load(Ordering::SeqCst)
    }

    /// Área cliente atual, para saber em que tamanho desenhar.
    pub fn client_size(&self) -> (i32, i32) {
        unsafe {
            let mut r = RECT::default();
            if GetClientRect(self.hwnd, &mut r).is_err() {
                return (0, 0);
            }
            (r.right - r.left, r.bottom - r.top)
        }
    }

    /// Desenha um quadro BGRA, escalando para a área cliente.
    ///
    /// O escalonamento não é enfeite: a tela de desenvolvimento tem menos
    /// pixels que a maioria das máquinas que o produto vai controlar, então
    /// o viewer sendo menor que o host é o caso comum, não a exceção.
    pub fn present(&mut self, bgra: &[u8], width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let (cw, ch) = self.client_size();
        if cw <= 0 || ch <= 0 {
            return;
        }

        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                // Altura negativa: bitmap top-down. Sem isso a imagem aparece
                // de cabeça para baixo, porque o padrão do Windows é
                // bottom-up.
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        unsafe {
            StretchDIBits(
                self.hdc,
                0,
                0,
                cw,
                ch,
                0,
                0,
                width,
                height,
                Some(bgra.as_ptr() as *const _),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(Some(self.hwnd), self.hdc);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE | WM_DESTROY => {
                CLOSED.store(true, Ordering::SeqCst);
                LRESULT(0)
            }
            // WM_PAINT precisa ser validado, senão o Windows reenvia sem
            // parar e a fila nunca esvazia. Quem desenha é o `present`.
            WM_PAINT => {
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
