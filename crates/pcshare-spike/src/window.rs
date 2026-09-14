//! Janela mínima do viewer.
//!
//! GDI com `StretchDIBits`, deliberadamente. Não é o caminho de apresentação
//! do produto — Direct3D com swapchain é — mas é o menor código que põe uma
//! imagem na tela, e o Sprint 0 precisa da imagem, não da apresentação ótima.
//!
//! Consequência a lembrar ao ler a latência medida: o blit por GDI acrescenta
//! custo próprio, e ele **está incluído** no número glass-to-glass. O produto
//! deve ficar abaixo do que medirmos aqui, não acima.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use pcshare_core::MouseButton;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, DIB_RGB_COLORS, FillRect, GdiFlush, GetDC,
    GetStockObject, HALFTONE, HBRUSH, HDC, InvalidateRect, ReleaseDC, SRCCOPY, SetBrushOrgEx,
    SetStretchBltMode, StretchDIBits, ValidateRect, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, Result, w};

/// O `wndproc` é uma função livre chamada pelo Windows; não há `self` para
/// carregar estado. Como o spike tem exatamente uma janela, uma flag global
/// resolve sem introduzir mapa de HWND.
static CLOSED: AtomicBool = AtomicBool::new(false);

/// Fila de entrada preenchida pelo `wndproc`, drenada por [`Window::take_input`].
static EVENTS: Mutex<Vec<RawEvent>> = Mutex::new(Vec::new());

/// Evento cru da janela. As coordenadas estão em **pixels da área cliente**;
/// a conversão para pixels do host acontece em `main.rs`, onde as duas
/// dimensões são conhecidas.
#[derive(Debug, Clone, Copy)]
pub enum RawEvent {
    Move { x: i32, y: i32 },
    Button { button: MouseButton, down: bool },
    Wheel { delta: i32 },
    Key { scancode: u32, down: bool },
}

fn push(ev: RawEvent) {
    if let Ok(mut q) = EVENTS.lock() {
        // Um viewer que não drena não pode fazer a fila crescer sem limite.
        // Entrada velha não tem valor: descartamos a mais antiga.
        if q.len() > 512 {
            q.remove(0);
        }
        q.push(ev);
    }
}

fn lo_word(v: isize) -> i32 {
    (v & 0xFFFF) as u16 as i16 as i32
}

fn hi_word(v: isize) -> i32 {
    ((v >> 16) & 0xFFFF) as u16 as i16 as i32
}

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

    /// Drena os eventos de mouse e teclado acumulados desde a última chamada.
    pub fn take_input(&mut self) -> Vec<RawEvent> {
        EVENTS.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }

    /// Handle da janela, para ser dono de diálogos modais (o seletor de
    /// arquivo). Sem dono, o diálogo não devolve o foco à janela ao fechar, e o
    /// viewer para de receber teclas depois — inclusive os próprios atalhos.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
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
            // Esticamento de qualidade. Sem isto, o Windows usa o modo grosseiro
            // (`COLORONCOLOR`), que joga linhas e colunas fora ao encolher — e é
            // o que borra o texto quando a tela do host é maior que a janela.
            // `HALFTONE` faz média dos pixels; a Microsoft pede um
            // `SetBrushOrgEx` logo depois, para o alinhamento do meio-tom não
            // introduzir um leve deslocamento.
            SetStretchBltMode(self.hdc, HALFTONE);
            let _ = SetBrushOrgEx(self.hdc, 0, 0, None);
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

/// Quadrado sólido, sempre no topo, que alterna entre preto e branco.
///
/// É o instrumento da medição de latência: o host o captura como parte da
/// tela, e o viewer detecta em que quadro decodificado a troca de cor
/// apareceu. A diferença de tempo atravessa o compositor do Windows, a
/// captura, o encoder, a rede e o decodificador — praticamente tudo que os
/// cronômetros internos não enxergavam.
pub struct MarkerWindow {
    hwnd: HWND,
    hdc: HDC,
    size: i32,
}

impl MarkerWindow {
    /// Cria o marcador em `(x, y)` da tela, com `size` pixels de lado.
    pub fn new(x: i32, y: i32, size: i32) -> Result<Self> {
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = w!("PcshareMarkerWindow");
            let wc = WNDCLASSW {
                lpfnWndProc: Some(marker_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            };
            RegisterClassW(&wc);

            let hwnd = CreateWindowExW(
                // TOPMOST para nunca ser encoberto; NOACTIVATE para não
                // roubar o foco a cada repintura.
                WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                class,
                w!("marcador"),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                size,
                size,
                None,
                None,
                Some(instance.into()),
                None,
            )?;

            let hdc = GetDC(Some(hwnd));
            Ok(Self { hwnd, hdc, size })
        }
    }

    /// Pinta o marcador e **só retorna depois de o GDI ter despachado**.
    ///
    /// Importante para a medição: o instante marcado logo após esta chamada
    /// precisa ser o momento em que a mudança foi entregue ao sistema, não o
    /// momento em que ela entrou numa fila.
    pub fn paint(&mut self, white: bool) {
        unsafe {
            let rect = RECT {
                left: 0,
                top: 0,
                right: self.size,
                bottom: self.size,
            };
            let brush = GetStockObject(if white { WHITE_BRUSH } else { BLACK_BRUSH });
            FillRect(self.hdc, &rect, HBRUSH(brush.0));
            let _ = GdiFlush();
        }
    }

    /// Processa mensagens para a janela não ser considerada travada.
    pub fn pump(&mut self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(self.hwnd), 0, 0, PM_REMOVE).as_bool() {
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for MarkerWindow {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(Some(self.hwnd), self.hdc);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

extern "system" fn marker_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_PAINT {
            // Quem pinta é `paint`; validar aqui evita repintura infinita.
            let _ = ValidateRect(Some(hwnd), None);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
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

            WM_MOUSEMOVE => {
                push(RawEvent::Move {
                    x: lo_word(lp.0),
                    y: hi_word(lp.0),
                });
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
            | WM_MBUTTONUP => {
                let (button, down) = match msg {
                    WM_LBUTTONDOWN => (MouseButton::Left, true),
                    WM_LBUTTONUP => (MouseButton::Left, false),
                    WM_RBUTTONDOWN => (MouseButton::Right, true),
                    WM_RBUTTONUP => (MouseButton::Right, false),
                    WM_MBUTTONDOWN => (MouseButton::Middle, true),
                    _ => (MouseButton::Middle, false),
                };
                // A posição vem junto com o clique: sem isso, um clique
                // rápido depois de um movimento pode ser injetado na posição
                // anterior do cursor.
                push(RawEvent::Move {
                    x: lo_word(lp.0),
                    y: hi_word(lp.0),
                });
                push(RawEvent::Button { button, down });
                LRESULT(0)
            }
            WM_MOUSEWHEEL => {
                push(RawEvent::Wheel {
                    delta: hi_word(wp.0 as isize),
                });
                LRESULT(0)
            }

            // As variantes SYS existem para combinações com Alt e para F10.
            // Ignorá-las deixaria Alt+Tab e Alt+F4 sem funcionar na máquina
            // remota.
            WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
                let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
                let scan = ((lp.0 >> 16) & 0xFF) as u32;
                let extended = (lp.0 >> 24) & 1 != 0;
                push(RawEvent::Key {
                    // Marca a tecla estendida no próprio código; `input.rs`
                    // reconhece o prefixo e repõe a flag na injeção.
                    scancode: if extended { 0xE000 | scan } else { scan },
                    down,
                });
                LRESULT(0)
            }

            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
