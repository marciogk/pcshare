//! Decodificador H.264 via Media Foundation Transform.
//!
//! Espelha o encoder em estrutura, com uma diferença que não é detalhe: aqui
//! `MF_E_TRANSFORM_STREAM_CHANGE` é o caminho normal, não exceção. O
//! decodificador só descobre as dimensões reais do vídeo ao encontrar o SPS
//! no bitstream, e é renegociando o tipo de saída que ele as informa. Tratar
//! isso como erro faria o decodificador nunca produzir um único frame.

use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::*;
use windows::core::{Interface, Result};

use crate::{Backend, ensure_mf_started, find_transform, pack};

/// Um frame decodificado em NV12 **compactado**: `stride == width`.
///
/// O decodificador entrega linhas alinhadas ao hardware; empacotamos na
/// cópia para que quem exibe não precise carregar o passo de linha adiante.
#[derive(Debug, Clone)]
pub struct DecodedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub time_100ns: i64,
}

pub struct H264Decoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    name: String,
    width: u32,
    height: u32,
    provides_samples: bool,
    out_size: u32,
    ready: Vec<DecodedFrame>,
}

impl H264Decoder {
    /// `width`/`height` são uma dica inicial. As dimensões efetivas vêm do
    /// bitstream e podem substituí-las na primeira renegociação.
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        ensure_mf_started();
        match Self::with_backend(width, height, fps, Backend::Hardware) {
            Ok(d) => Ok(d),
            Err(hw_err) => match Self::with_backend(width, height, fps, Backend::Software) {
                Ok(d) => Ok(d),
                Err(_) => Err(hw_err),
            },
        }
    }

    pub fn with_backend(width: u32, height: u32, fps: u32, backend: Backend) -> Result<Self> {
        ensure_mf_started();
        unsafe {
            let (transform, name) = find_transform(
                MFT_CATEGORY_VIDEO_DECODER,
                MFVideoFormat_H264,
                MFVideoFormat_NV12,
                backend,
            )?;

            let attrs = transform.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // Decodificador: entrada primeiro. O oposto do encoder.
            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &in_type, 0)?;

            let mut me = Self {
                transform,
                events: None,
                name,
                width,
                height,
                provides_samples: false,
                out_size: width * height * 3 / 2,
                ready: Vec::new(),
            };
            me.select_nv12_output()?;

            if is_async {
                me.events = Some(me.transform.cast()?);
            }
            me.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            me.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(me)
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Percorre os tipos de saída oferecidos e fixa o primeiro em NV12.
    ///
    /// Não dá para montar o tipo à mão: o MFT só aceita um tipo que ele mesmo
    /// tenha listado, com todos os atributos internos preenchidos.
    unsafe fn select_nv12_output(&mut self) -> Result<()> {
        unsafe {
            let mut i = 0u32;
            loop {
                let Ok(candidate) = self.transform.GetOutputAvailableType(0, i) else {
                    return Err(windows::core::Error::new(
                        MF_E_INVALIDMEDIATYPE,
                        "o decodificador não ofereceu saída NV12",
                    ));
                };
                if candidate.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_NV12 {
                    self.transform.SetOutputType(0, &candidate, 0)?;

                    if let Ok(size) = candidate.GetUINT64(&MF_MT_FRAME_SIZE) {
                        self.width = (size >> 32) as u32;
                        self.height = (size & 0xFFFF_FFFF) as u32;
                    }
                    let info = self.transform.GetOutputStreamInfo(0)?;
                    let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
                    self.provides_samples = info.dwFlags & provides != 0;
                    self.out_size = info.cbSize.max(self.width * self.height * 3 / 2);
                    return Ok(());
                }
                i += 1;
            }
        }
    }

    /// Alimenta um pacote H.264. Pode não produzir frame: o decodificador
    /// precisa de SPS/PPS antes do primeiro, e tem pipeline.
    pub fn decode(&mut self, data: &[u8], time_100ns: i64) -> Result<()> {
        unsafe {
            let sample = crate::make_sample(data, time_100ns, 0)?;

            if self.events.is_some() {
                self.pump_until_need_input()?;
                self.transform.ProcessInput(0, &sample, 0)?;
            } else {
                self.transform.ProcessInput(0, &sample, 0)?;
                self.drain_sync()?;
            }
            Ok(())
        }
    }

    pub fn take_frames(&mut self) -> Vec<DecodedFrame> {
        std::mem::take(&mut self.ready)
    }

    unsafe fn pump_until_need_input(&mut self) -> Result<()> {
        unsafe {
            let events = self.events.clone().expect("modo assíncrono");
            loop {
                let kind = events.GetEvent(MF_EVENT_FLAG_NONE)?.GetType()?;
                if kind == METransformNeedInput.0 as u32 {
                    return Ok(());
                } else if kind == METransformHaveOutput.0 as u32 {
                    self.pull_one()?;
                }
            }
        }
    }

    unsafe fn drain_sync(&mut self) -> Result<()> {
        unsafe {
            loop {
                match self.pull_one() {
                    Ok(()) => {}
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(()),
                    Err(e) => return Err(e),
                }
            }
        }
    }

    unsafe fn pull_one(&mut self) -> Result<()> {
        unsafe {
            let allocated = if self.provides_samples {
                None
            } else {
                let buffer = MFCreateMemoryBuffer(self.out_size)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            };

            let mut out = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(allocated),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            };
            let mut status = 0u32;

            let result = self
                .transform
                .ProcessOutput(0, std::slice::from_mut(&mut out), &mut status);

            let sample = ManuallyDrop::take(&mut out.pSample);
            let _ = ManuallyDrop::take(&mut out.pEvents);

            // Caminho normal: o SPS acabou de revelar as dimensões reais.
            if let Err(e) = &result
                && e.code() == MF_E_TRANSFORM_STREAM_CHANGE
            {
                drop(sample);
                self.select_nv12_output()?;
                return Ok(());
            }
            result?;

            let Some(sample) = sample else { return Ok(()) };
            let time_100ns = sample.GetSampleTime().unwrap_or(0);
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (w, h) = (self.width as usize, self.height as usize);
            let mut data = vec![0u8; w * h * 3 / 2];

            // IMF2DBuffer expõe o passo de linha do hardware. Sem ele
            // teríamos que adivinhar o alinhamento, e errar embaralha a
            // imagem em qualquer largura não alinhada.
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                let mut scanline: *mut u8 = std::ptr::null_mut();
                let mut pitch: i32 = 0;
                two_d.Lock2D(&mut scanline, &mut pitch)?;
                let pitch = pitch as usize;

                for y in 0..h {
                    std::ptr::copy_nonoverlapping(
                        scanline.add(y * pitch),
                        data.as_mut_ptr().add(y * w),
                        w,
                    );
                }
                // O plano UV começa logo após `height` linhas do plano Y, no
                // mesmo passo.
                for y in 0..h / 2 {
                    std::ptr::copy_nonoverlapping(
                        scanline.add((h + y) * pitch),
                        data.as_mut_ptr().add(w * h + y * w),
                        w,
                    );
                }
                two_d.Unlock2D()?;
            } else {
                let mut ptr: *mut u8 = std::ptr::null_mut();
                let mut len: u32 = 0;
                buffer.Lock(&mut ptr, None, Some(&mut len))?;
                let n = (len as usize).min(data.len());
                std::ptr::copy_nonoverlapping(ptr, data.as_mut_ptr(), n);
                buffer.Unlock()?;
            }

            self.ready.push(DecodedFrame {
                data,
                width: self.width,
                height: self.height,
                time_100ns,
            });
            Ok(())
        }
    }
}
