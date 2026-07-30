//! Captura de tela via DXGI Desktop Duplication.
//!
//! Ver decisão D2 em `docs/ARQUITETURA.md`. O ponto central aqui não é obter
//! a imagem — é obter as *dirty rects*, a lista de regiões que mudaram desde
//! o frame anterior. Numa tela de escritório parada quase nenhum pixel muda,
//! e codificar a tela inteira a 30 fps desperdiça banda e CPU.

use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

#[derive(Debug)]
pub enum CaptureError {
    /// A sessão de duplicação foi invalidada. Acontece em troca de resolução,
    /// entrada/saída de modo exclusivo de tela cheia, mudança de usuário e ao
    /// passar pelo Secure Desktop (UAC). Não é fatal: exige recriar a captura.
    AccessLost,
    Windows(windows::core::Error),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AccessLost => write!(f, "sessão de duplicação perdida (recriar a captura)"),
            Self::Windows(e) => write!(f, "erro do Windows: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

impl From<windows::core::Error> for CaptureError {
    fn from(e: windows::core::Error) -> Self {
        if e.code() == DXGI_ERROR_ACCESS_LOST {
            Self::AccessLost
        } else {
            Self::Windows(e)
        }
    }
}

/// O que mudou num frame. Ainda não carrega pixels — a etapa 1 do Sprint 0
/// mede apenas o custo e o comportamento da captura em si.
#[derive(Debug, Clone)]
pub struct FrameStats {
    /// Frames que o compositor acumulou desde a última chamada. Maior que 1
    /// significa que não estamos consumindo tão rápido quanto a tela produz.
    pub accumulated_frames: u32,
    pub dirty_rects: Vec<RECT>,
    /// Soma da área das dirty rects. Pode passar do total da tela quando os
    /// retângulos se sobrepõem — não é uma fração exata, e sim um indicador
    /// de quanto trabalho o encoder teria.
    pub dirty_pixels: u64,
    /// `true` quando só o cursor se moveu e a imagem do desktop não mudou.
    /// Estes frames não devem gerar quadro de vídeo.
    pub mouse_only: bool,
}

pub struct DesktopCapture {
    device: ID3D11Device,
    _context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    desc: DXGI_OUTDUPL_DESC,
    output_index: u32,
    /// Reutilizado entre frames: a API pede um buffer nosso, e alocar a cada
    /// frame no caminho quente é exatamente o custo que queremos evitar.
    dirty_buf: Vec<RECT>,
    frame_held: bool,
}

impl DesktopCapture {
    /// Abre a duplicação da saída de vídeo `output_index` (0 = monitor primário).
    pub fn new(output_index: u32) -> Result<Self, CaptureError> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                // Só usado com D3D_DRIVER_TYPE_SOFTWARE.
                HMODULE::default(),
                // BGRA_SUPPORT é exigido para interoperar com Direct2D e é o
                // formato que a Desktop Duplication entrega.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.expect("D3D11CreateDevice retornou Ok sem device");
            let context = context.expect("D3D11CreateDevice retornou Ok sem context");

            let duplication = Self::duplicate_output(&device, output_index)?;
            let desc = duplication.GetDesc();

            Ok(Self {
                device,
                _context: context,
                duplication,
                desc,
                output_index,
                dirty_buf: Vec::new(),
                frame_held: false,
            })
        }
    }

    unsafe fn duplicate_output(
        device: &ID3D11Device,
        output_index: u32,
    ) -> Result<IDXGIOutputDuplication, CaptureError> {
        unsafe {
            let dxgi_device: IDXGIDevice = device.cast()?;
            let adapter = dxgi_device.GetAdapter()?;
            let output = adapter.EnumOutputs(output_index)?;
            let output1: IDXGIOutput1 = output.cast()?;
            Ok(output1.DuplicateOutput(device)?)
        }
    }

    pub fn width(&self) -> u32 {
        self.desc.ModeDesc.Width
    }

    pub fn height(&self) -> u32 {
        self.desc.ModeDesc.Height
    }

    /// Recria a sessão após `CaptureError::AccessLost`, reaproveitando o device.
    pub fn recover(&mut self) -> Result<(), CaptureError> {
        self.frame_held = false;
        unsafe {
            self.duplication = Self::duplicate_output(&self.device, self.output_index)?;
            self.desc = self.duplication.GetDesc();
        }
        Ok(())
    }

    /// Aguarda o próximo frame. `Ok(None)` significa que nada mudou na tela
    /// dentro do timeout — o caso comum e desejável numa tela parada.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<FrameStats>, CaptureError> {
        unsafe {
            // A API exige liberar o frame anterior antes de pedir o próximo.
            if self.frame_held {
                self.duplication.ReleaseFrame()?;
                self.frame_held = false;
            }

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            match self
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
            {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            self.frame_held = true;

            // LastPresentTime zerado: o desktop não foi redesenhado, apenas o
            // cursor se moveu. Emitir vídeo aqui seria banda desperdiçada.
            let mouse_only = info.LastPresentTime == 0;

            self.dirty_buf.clear();
            if !mouse_only {
                self.read_dirty_rects()?;
            }

            let dirty_pixels = self
                .dirty_buf
                .iter()
                .map(|r| {
                    let w = (r.right - r.left).max(0) as u64;
                    let h = (r.bottom - r.top).max(0) as u64;
                    w * h
                })
                .sum();

            Ok(Some(FrameStats {
                accumulated_frames: info.AccumulatedFrames,
                dirty_rects: self.dirty_buf.clone(),
                dirty_pixels,
                mouse_only,
            }))
        }
    }

    /// A API informa o tamanho necessário quando o buffer é pequeno, então
    /// crescemos sob demanda e reaproveitamos nas chamadas seguintes.
    unsafe fn read_dirty_rects(&mut self) -> Result<(), CaptureError> {
        unsafe {
            loop {
                let capacity_bytes =
                    (self.dirty_buf.capacity() * std::mem::size_of::<RECT>()) as u32;
                let mut needed_bytes: u32 = 0;

                let result = self.duplication.GetFrameDirtyRects(
                    capacity_bytes,
                    self.dirty_buf.as_mut_ptr(),
                    &mut needed_bytes,
                );

                let needed_rects = needed_bytes as usize / std::mem::size_of::<RECT>();

                match result {
                    Ok(()) => {
                        self.dirty_buf.set_len(needed_rects);
                        return Ok(());
                    }
                    // Buffer pequeno demais: crescer e repetir.
                    Err(_) if self.dirty_buf.capacity() < needed_rects => {
                        self.dirty_buf.reserve(needed_rects);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
}

impl Drop for DesktopCapture {
    fn drop(&mut self) {
        if self.frame_held {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}
