//! Dispositivo Direct3D 11 compartilhado e conversão de cor na GPU.
//!
//! Existe por causa da decisão D3 em `docs/ARQUITETURA.md`. Até a etapa 4 a
//! pipeline trazia a textura da GPU para a memória de sistema, convertia
//! BGRA→NV12 na CPU e devolvia o resultado ao encoder — que roda na GPU. Medido
//! a 1360×768: **4,1 ms de leitura + 2,1 ms de conversão**, seis dos onze
//! milissegundos do host gastos em um desvio de ida e volta. A 1080p mais que
//! dobra.
//!
//! Aqui a textura nunca sai da GPU: o processador de vídeo do D3D11 converte
//! para NV12 e a textura resultante vai direto ao encoder.
//!
//! Para isso, captura e encoder precisam **compartilhar o mesmo dispositivo
//! D3D11** — texturas não atravessam dispositivos sem serem compartilhadas
//! explicitamente. Daí este crate existir separado dos dois.

use std::mem::ManuallyDrop;
use std::sync::Arc;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::{Interface, Result};

/// Dispositivo D3D11 usado por toda a pipeline.
///
/// Compartilhado via [`Arc`]: a captura escreve nele, o conversor lê e
/// escreve, o encoder lê. Um dispositivo por processo.
pub struct GpuContext {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl GpuContext {
    pub fn new() -> Result<Arc<Self>> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;

            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                // VIDEO_SUPPORT é o que habilita ID3D11VideoDevice, sem o
                // qual não há processador de vídeo nem encoder acelerado.
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;

            let device = device.expect("D3D11CreateDevice retornou Ok sem device");
            let context = context.expect("D3D11CreateDevice retornou Ok sem context");

            // Obrigatório: o Media Foundation chama o encoder a partir das
            // próprias threads dele. Sem proteção multithread, o dispositivo
            // corrompe silenciosamente sob concorrência — e o sintoma aparece
            // como imagem embaralhada ou travamento, longe da causa.
            let mt: ID3D11Multithread = context.cast()?;
            // Devolve o estado anterior, que não nos interessa.
            let _ = mt.SetMultithreadProtected(true);

            Ok(Arc::new(Self { device, context }))
        }
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.context
    }
}

/// Converte BGRA para NV12 sem sair da GPU, usando o processador de vídeo de
/// função fixa do D3D11.
///
/// Usa o mesmo espaço de cor da conversão por CPU em `pcshare-codec`
/// (BT.601, faixa limitada). A consistência é o que importa: trocar a matriz
/// de um lado só produz imagem lavada ou saturada demais no outro.
pub struct Nv12Converter {
    gpu: Arc<GpuContext>,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    /// Textura de saída, reaproveitada entre quadros.
    output: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    width: u32,
    height: u32,
}

/// BT.601, luma 16..235. Layout de bits: `Usage`(0), `RGB_Range`(1),
/// `YCbCr_Matrix`(2), `YCbCr_xvYCC`(3), `Nominal_Range`(4..5).
const YCBCR_601_LIMITED: u32 = 1 << 4;
/// RGB de faixa cheia, uso de reprodução.
const RGB_FULL: u32 = 0;

impl Nv12Converter {
    pub fn new(gpu: Arc<GpuContext>, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = gpu.device().cast()?;
            let video_context: ID3D11VideoContext = gpu.context().cast()?;

            let desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 60,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                // Conteúdo de tela: prioriza velocidade sobre filtragem
                // elaborada, que aqui não tem o que melhorar (sem
                // redimensionamento, sem desentrelaçamento).
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };

            let enumerator = video_device.CreateVideoProcessorEnumerator(&desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            let (output, output_view) =
                Self::make_output(&gpu, &video_device, &enumerator, width, height)?;

            let me = Self {
                gpu,
                video_device,
                video_context,
                enumerator,
                processor,
                output,
                output_view,
                width,
                height,
            };

            me.video_context.VideoProcessorSetStreamColorSpace(
                &me.processor,
                0,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: RGB_FULL,
                },
            );
            me.video_context.VideoProcessorSetOutputColorSpace(
                &me.processor,
                &D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                    _bitfield: YCBCR_601_LIMITED,
                },
            );
            // Sem isso o processador pode tentar suavizar entre quadros, o
            // que em conteúdo de tela borra texto.
            me.video_context
                .VideoProcessorSetStreamAutoProcessingMode(&me.processor, 0, false);

            Ok(me)
        }
    }

    unsafe fn make_output(
        gpu: &GpuContext,
        video_device: &ID3D11VideoDevice,
        enumerator: &ID3D11VideoProcessorEnumerator,
        width: u32,
        height: u32,
    ) -> Result<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // RENDER_TARGET é exigido para criar a view de saída do
                // processador de vídeo.
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };

            let mut tex: Option<ID3D11Texture2D> = None;
            gpu.device().CreateTexture2D(&desc, None, Some(&mut tex))?;
            let tex = tex.expect("CreateTexture2D retornou Ok sem textura");

            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device.CreateVideoProcessorOutputView(
                &tex,
                enumerator,
                &view_desc,
                Some(&mut view),
            )?;
            Ok((tex, view.expect("view de saída ausente")))
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Converte `src` (BGRA) para NV12. A textura devolvida pertence ao
    /// conversor e é sobrescrita na chamada seguinte.
    pub fn convert(&mut self, src: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
        unsafe {
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device.CreateVideoProcessorInputView(
                src,
                &self.enumerator,
                &in_desc,
                Some(&mut input_view),
            )?;

            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: ManuallyDrop::new(input_view),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            }];

            let result =
                self.video_context
                    .VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams);

            // O struct carrega a view em ManuallyDrop; liberar à mão evita
            // vazar uma referência COM por quadro.
            let _ = ManuallyDrop::take(&mut streams[0].pInputSurface);
            let _ = ManuallyDrop::take(&mut streams[0].pInputSurfaceRight);
            result?;

            Ok(&self.output)
        }
    }

    pub fn gpu(&self) -> &Arc<GpuContext> {
        &self.gpu
    }
}
