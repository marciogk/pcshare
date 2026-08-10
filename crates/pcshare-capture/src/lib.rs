//! Captura de tela via DXGI Desktop Duplication.
//!
//! Ver decisão D2 em `docs/ARQUITETURA.md`. O ponto central aqui não é obter
//! a imagem — é obter as *dirty rects*, a lista de regiões que mudaram desde
//! o frame anterior. Numa tela de escritório parada quase nenhum pixel muda,
//! e codificar a tela inteira a 30 fps desperdiça banda e CPU.

pub mod desktop;
pub mod linha_do_tempo;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_STAGING, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_OUTDUPL_MOVE_RECT, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

use std::sync::Arc;

pub use pcshare_gpu::GpuContext;

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
    /// Regiões que apenas **mudaram de lugar**, sem conteúdo novo: janelas
    /// arrastadas, rolagem por blit.
    ///
    /// Contam como alteração. Um frame pode ter zero dirty rects e ainda
    /// assim ter mudado, e tratar isso como "nada aconteceu" congelaria a
    /// tela do outro lado durante um arraste de janela.
    pub move_rects: u32,
    /// `true` quando só o cursor se moveu e a imagem do desktop não mudou.
    /// Estes frames não devem gerar quadro de vídeo.
    pub mouse_only: bool,
}

impl FrameStats {
    /// Se este frame precisa mesmo ser codificado.
    ///
    /// Base da supressão por área alterada, que o achado da etapa 1 exigiu:
    /// o compositor entrega frames a 60 Hz mesmo com o desktop ocioso, e
    /// codificar todos eles gasta banda para transmitir nada.
    pub fn has_changes(&self) -> bool {
        !self.mouse_only && (!self.dirty_rects.is_empty() || self.move_rects > 0)
    }
}

pub struct DesktopCapture {
    /// Dispositivo compartilhado com o conversor e o encoder. Texturas não
    /// atravessam dispositivos D3D11 diferentes sem compartilhamento
    /// explícito, então o caminho sem cópia depende de todos usarem este.
    gpu: Arc<GpuContext>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// `None` enquanto não há duplicação válida.
    ///
    /// Depois de `DXGI_ERROR_ACCESS_LOST` o objeto está morto e chamá-lo de
    /// novo devolve `INVALID_CALL`, um erro que não diz nada sobre a causa
    /// real. Guardar `None` faz [`Self::next_frame`] responder `AccessLost`
    /// de imediato, que é a verdade e diz ao chamador o que fazer.
    duplication: Option<IDXGIOutputDuplication>,
    /// Textura em memória acessível pela CPU. Criada uma vez e reaproveitada:
    /// alocar por frame no caminho quente seria desperdício puro.
    staging: Option<ID3D11Texture2D>,
    /// Textura do frame atualmente retido, se houver.
    current: Option<ID3D11Texture2D>,
    desc: DXGI_OUTDUPL_DESC,
    output_index: u32,
    /// Reutilizado entre frames: a API pede um buffer nosso, e alocar a cada
    /// frame no caminho quente é exatamente o custo que queremos evitar.
    dirty_buf: Vec<RECT>,
    frame_held: bool,
}

impl DesktopCapture {
    /// Abre a duplicação da saída de vídeo `output_index` (0 = monitor
    /// primário), criando um dispositivo D3D11 próprio.
    ///
    /// Use [`Self::with_gpu`] quando a textura for seguir para o encoder sem
    /// passar pela CPU: nesse caso todos precisam do mesmo dispositivo.
    pub fn new(output_index: u32) -> Result<Self, CaptureError> {
        Self::with_gpu(GpuContext::new()?, output_index)
    }

    /// Idem, sobre um dispositivo D3D11 já existente.
    pub fn with_gpu(gpu: Arc<GpuContext>, output_index: u32) -> Result<Self, CaptureError> {
        unsafe {
            let device = gpu.device().clone();
            let context = gpu.context().clone();
            let duplication = Self::duplicate_output(&device, output_index)?;
            let desc = duplication.GetDesc();

            Ok(Self {
                gpu,
                device,
                context,
                duplication: Some(duplication),
                staging: None,
                current: None,
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
    ///
    /// **Pode ser chamada quantas vezes for preciso.** Falhar aqui é o caso
    /// esperado enquanto o Secure Desktop está na frente: um processo comum
    /// recebe acesso negado ao tentar duplicá-lo. Medido em 05/08/2026 durante
    /// um prompt de UAC — `0x80070005`, acesso negado.
    ///
    /// A versão anterior deixava a duplicação morta no lugar quando falhava, e
    /// a chamada seguinte quebrava com `INVALID_CALL`, escondendo a causa real
    /// atrás de um erro sem sentido. Agora o objeto morto é solto antes da
    /// tentativa, e o chamador pode simplesmente insistir até o desktop voltar.
    pub fn recover(&mut self) -> Result<(), CaptureError> {
        self.frame_held = false;
        self.current = None;
        // Solta a duplicação morta antes de tentar: se a tentativa falhar,
        // ficamos sem duplicação, que é a verdade.
        self.duplication = None;

        unsafe {
            let nova = Self::duplicate_output(&self.device, self.output_index)?;
            self.desc = nova.GetDesc();
            self.duplication = Some(nova);
        }
        // A resolução pode ter mudado — foi um dos motivos da perda de sessão.
        self.staging = None;
        Ok(())
    }

    /// `true` quando há duplicação válida. `false` significa que `recover`
    /// precisa ter sucesso antes de qualquer captura.
    pub fn is_active(&self) -> bool {
        self.duplication.is_some()
    }

    /// Aguarda o próximo frame. `Ok(None)` significa que nada mudou na tela
    /// dentro do timeout — o caso comum e desejável numa tela parada.
    pub fn next_frame(&mut self, timeout_ms: u32) -> Result<Option<FrameStats>, CaptureError> {
        // Sem duplicação viva não há o que capturar, e insistir numa que
        // morreu produz `INVALID_CALL` — erro que esconde a causa.
        let dup = self
            .duplication
            .clone()
            .ok_or(CaptureError::AccessLost)?;

        unsafe {
            // A API exige liberar o frame anterior antes de pedir o próximo.
            if self.frame_held {
                self.current = None;
                dup.ReleaseFrame()?;
                self.frame_held = false;
            }

            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            match dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
                Err(e) => return Err(e.into()),
            }
            self.frame_held = true;
            self.current = resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok());

            // LastPresentTime zerado: o desktop não foi redesenhado, apenas o
            // cursor se moveu. Emitir vídeo aqui seria banda desperdiçada.
            let mouse_only = info.LastPresentTime == 0;

            self.dirty_buf.clear();
            let mut move_rects = 0;
            if !mouse_only {
                self.read_dirty_rects()?;
                move_rects = self.count_move_rects()?;
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
                move_rects,
                mouse_only,
            }))
        }
    }

    pub fn gpu(&self) -> &Arc<GpuContext> {
        &self.gpu
    }

    /// A textura BGRA do frame retido, ainda na GPU.
    ///
    /// Este é o caminho sem cópia: entregue-a ao conversor NV12 e daí ao
    /// encoder, sem que nenhum pixel passe pela CPU. Compare com
    /// [`Self::copy_bgra`], que é o desvio caro.
    pub fn current_texture(&self) -> Option<&ID3D11Texture2D> {
        self.current.as_ref()
    }

    /// Copia o frame retido para `out` em BGRA, uma linha por `width()*4`
    /// bytes.
    ///
    /// **Este é o gargalo que a decisão D3 manda eliminar no produto.** A
    /// textura está na GPU; trazê-la para a CPU custa uma cópia da tela
    /// inteira por frame e serializa GPU e CPU. Existe aqui porque o encoder
    /// do Sprint 0 recebe NV12 em memória de sistema. O caminho definitivo
    /// entrega a textura direto ao encoder, sem passar pela CPU.
    ///
    /// Devolve `Ok(false)` se não há frame retido.
    pub fn copy_bgra(&mut self, out: &mut Vec<u8>) -> Result<bool, CaptureError> {
        let Some(src) = self.current.clone() else {
            return Ok(false);
        };
        let (w, h) = (self.width() as usize, self.height() as usize);

        unsafe {
            let staging = self.ensure_staging()?;
            self.context.CopyResource(&staging, &src);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let row_bytes = w * 4;
            out.resize(row_bytes * h, 0);

            // O passo de linha da GPU quase nunca é igual à largura útil: o
            // driver alinha as linhas. Copiar em bloco único embaralharia a
            // imagem em qualquer resolução não alinhada.
            let pitch = mapped.RowPitch as usize;
            let base = mapped.pData as *const u8;
            for y in 0..h {
                std::ptr::copy_nonoverlapping(
                    base.add(y * pitch),
                    out.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }

            self.context.Unmap(&staging, 0);
        }
        Ok(true)
    }

    unsafe fn ensure_staging(&mut self) -> Result<ID3D11Texture2D, CaptureError> {
        unsafe {
            if let Some(s) = &self.staging {
                return Ok(s.clone());
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.width(),
                Height: self.height(),
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.expect("CreateTexture2D retornou Ok sem textura");
            self.staging = Some(tex.clone());
            Ok(tex)
        }
    }

    /// Quantas regiões foram apenas deslocadas neste frame.
    ///
    /// Só a contagem interessa por ora: serve para decidir se houve mudança.
    /// Aproveitá-las de fato — mandando o deslocamento em vez dos pixels — é
    /// otimização de banda para a Fase 1.
    unsafe fn count_move_rects(&mut self) -> Result<u32, CaptureError> {
        let dup = self.duplication.clone().ok_or(CaptureError::AccessLost)?;
        unsafe {
            let mut buf = [DXGI_OUTDUPL_MOVE_RECT::default(); 64];
            let mut needed: u32 = 0;
            let capacity = (std::mem::size_of_val(&buf)) as u32;

            match dup.GetFrameMoveRects(capacity, buf.as_mut_ptr(), &mut needed) {
                Ok(()) => Ok(needed / std::mem::size_of::<DXGI_OUTDUPL_MOVE_RECT>() as u32),
                // Buffer pequeno: não vale crescer, já sabemos que houve
                // movimento, que é tudo que a decisão precisa.
                Err(_) if needed > capacity => Ok(buf.len() as u32),
                Err(e) => Err(e.into()),
            }
        }
    }

    /// A API informa o tamanho necessário quando o buffer é pequeno, então
    /// crescemos sob demanda e reaproveitamos nas chamadas seguintes.
    unsafe fn read_dirty_rects(&mut self) -> Result<(), CaptureError> {
        let dup = self
            .duplication
            .clone()
            .ok_or(CaptureError::AccessLost)?;
        unsafe {
            loop {
                let capacity_bytes =
                    (self.dirty_buf.capacity() * std::mem::size_of::<RECT>()) as u32;
                let mut needed_bytes: u32 = 0;

                let result = dup.GetFrameDirtyRects(
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
        if self.frame_held
            && let Some(dup) = &self.duplication
        {
            unsafe {
                let _ = dup.ReleaseFrame();
            }
        }
    }
}
