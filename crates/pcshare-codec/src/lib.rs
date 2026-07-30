//! Encoder H.264 via Media Foundation Transform (MFT).
//!
//! Ver decisão D3 em `docs/ARQUITETURA.md`. H.264 e não AV1/HEVC porque é o
//! único com encoder por hardware universalmente presente — um produto cujo
//! cliente instala na máquina alheia não pode exigir GPU de 2021 em diante.
//!
//! Prefere um MFT de hardware (Quick Sync / NVENC / AMF) e cai para software
//! quando não houver. Encoders de hardware quase sempre são MFTs *assíncronos*,
//! que exigem um laço dirigido por eventos em vez do par ProcessInput /
//! ProcessOutput direto; ambos os modos estão implementados aqui.
//!
//! Limitação conhecida desta versão: os frames entram por memória de sistema.
//! No produto, a textura da captura deve ir direto da GPU para o encoder, sem
//! passar pela CPU. A leitura de volta da GPU é custo real e **não** está
//! medida aqui — este módulo mede o encoder isoladamente.

pub mod convert;
pub mod decoder;

pub use decoder::{DecodedFrame, H264Decoder};

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Once;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{GUID, Interface, PWSTR, Result};

static MF_INIT: Once = Once::new();

pub(crate) fn ensure_mf_started() {
    MF_INIT.call_once(|| unsafe {
        // RPC_E_CHANGED_MODE é aceitável: significa que a thread já estava em
        // apartamento COM diferente, o que não impede o Media Foundation.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
    });
}

/// Empacota dois u32 no formato que os atributos do MF esperam
/// (dimensões, taxa de quadros e proporção de pixel).
pub(crate) fn pack(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

#[derive(Debug, Clone)]
pub struct Packet {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub time_100ns: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Hardware,
    Software,
}

pub struct H264Encoder {
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    backend: Backend,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    /// Quando falso, precisamos alocar nós mesmos o sample de saída.
    provides_samples: bool,
    out_size: u32,
    frame_index: i64,
    ready: Vec<Packet>,
    tuning: Vec<&'static str>,
    force_keyframe: bool,
}

impl H264Encoder {
    /// `bitrate` em bits por segundo. Tenta hardware e cai para software.
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32) -> Result<Self> {
        ensure_mf_started();
        match Self::with_backend(width, height, fps, bitrate, Backend::Hardware) {
            Ok(e) => Ok(e),
            Err(hw_err) => match Self::with_backend(width, height, fps, bitrate, Backend::Software)
            {
                Ok(e) => Ok(e),
                // O erro de hardware é o informativo: diz por que o caminho
                // rápido não serviu nesta máquina.
                Err(_) => Err(hw_err),
            },
        }
    }

    pub fn with_backend(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        backend: Backend,
    ) -> Result<Self> {
        ensure_mf_started();
        unsafe {
            let (transform, name) = find_encoder(backend)?;

            let attrs = transform.GetAttributes()?;
            let is_async = attrs.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) == 1;
            if is_async {
                // Sem isto o MFT assíncrono recusa qualquer configuração.
                attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }
            // Desabilita reordenação e buffering interno. Para controle remoto
            // a latência importa mais do que os poucos % de taxa de compressão
            // que os B-frames trariam.
            let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);

            // Encoders exigem o tipo de saída definido antes do de entrada.
            let out_type = MFCreateMediaType()?;
            out_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            out_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            out_type.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            out_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            out_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            out_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            out_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            out_type.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            transform.SetOutputType(0, &out_type, 0)?;

            let in_type = MFCreateMediaType()?;
            in_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            in_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            in_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            in_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            in_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            in_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            transform.SetInputType(0, &in_type, 0)?;

            let stream_info = transform.GetOutputStreamInfo(0)?;
            let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
            let provides_samples = stream_info.dwFlags & provides != 0;

            let events: Option<IMFMediaEventGenerator> =
                if is_async { Some(transform.cast()?) } else { None };

            let tuning = tune_rate_control(&transform, fps);

            // Sem FLUSH aqui: o encoder de software o rejeita antes de o
            // streaming começar, e não há nada para descartar.
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                events,
                backend,
                name,
                width,
                height,
                fps,
                provides_samples,
                // Alguns MFTs reportam 0; um frame comprimido nunca deveria
                // passar do tamanho do frame cru.
                out_size: stream_info.cbSize.max(width * height),
                frame_index: 0,
                ready: Vec::new(),
                tuning,
                // O primeiro frame precisa ser keyframe: sem ele o
                // decodificador não tem por onde começar.
                force_keyframe: true,
            })
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Quais ajustes de controle de taxa o encoder **aceitou** — o que não é
    /// o mesmo que obedeceu.
    ///
    /// Medido: o encoder de software da Microsoft aceita `GOP 10s` retornando
    /// sucesso e mesmo assim emite um keyframe por segundo. Só o bitstream
    /// resultante prova o que de fato valeu.
    pub fn tuning(&self) -> &[&'static str] {
        &self.tuning
    }

    /// Tamanho esperado do buffer NV12 de entrada: plano Y seguido do plano
    /// UV entrelaçado em metade da altura.
    pub fn nv12_len(&self) -> usize {
        (self.width as usize * self.height as usize) * 3 / 2
    }

    /// Faz o próximo frame ser keyframe.
    ///
    /// Chamar quando um viewer entra na sessão ou quando há perda: são os
    /// únicos dois momentos em que um keyframe se paga. Fora deles, ele é
    /// banda gasta para retransmitir uma imagem que o outro lado já tem.
    pub fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    /// Envia um frame. A saída pode não sair imediatamente — o encoder tem
    /// pipeline. Use [`Self::take_packets`] para coletar o que já ficou pronto.
    pub fn submit(&mut self, nv12: &[u8]) -> Result<()> {
        debug_assert_eq!(nv12.len(), self.nv12_len(), "buffer NV12 com tamanho errado");
        unsafe {
            if self.force_keyframe {
                if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
                    let _ = codec
                        .SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
                self.force_keyframe = false;
            }

            let duration = 10_000_000i64 / self.fps as i64;
            let time = self.frame_index * duration;
            self.frame_index += 1;

            let sample = make_sample(nv12, time, duration)?;

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

    /// Sinaliza fim de fluxo e coleta o que restou no pipeline.
    pub fn finish(&mut self) -> Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;

            if self.events.is_some() {
                self.pump_until_drained()?;
            } else {
                self.drain_sync()?;
            }
            Ok(())
        }
    }

    pub fn take_packets(&mut self) -> Vec<Packet> {
        std::mem::take(&mut self.ready)
    }

    /// MFT assíncrono: consome eventos até que ele peça entrada, guardando
    /// qualquer saída que apareça no caminho.
    unsafe fn pump_until_need_input(&mut self) -> Result<()> {
        unsafe {
            let events = self.events.clone().expect("chamado apenas no modo assíncrono");
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

    unsafe fn pump_until_drained(&mut self) -> Result<()> {
        unsafe {
            let events = self.events.clone().expect("chamado apenas no modo assíncrono");
            loop {
                let kind = events.GetEvent(MF_EVENT_FLAG_NONE)?.GetType()?;
                if kind == METransformDrainComplete.0 as u32 {
                    return Ok(());
                } else if kind == METransformHaveOutput.0 as u32 {
                    self.pull_one()?;
                }
            }
        }
    }

    /// MFT síncrono: puxa saídas até o encoder pedir mais entrada.
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

    /// Aceita o novo tipo de saída que o MFT passou a oferecer e reajusta o
    /// tamanho do buffer que alocamos para ele.
    unsafe fn renegotiate_output(&mut self) -> Result<()> {
        unsafe {
            let new_type = self.transform.GetOutputAvailableType(0, 0)?;
            self.transform.SetOutputType(0, &new_type, 0)?;

            let info = self.transform.GetOutputStreamInfo(0)?;
            let provides = MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32;
            self.provides_samples = info.dwFlags & provides != 0;
            self.out_size = info.cbSize.max(self.width * self.height);
            Ok(())
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

            // Não é falha: o MFT quer redefinir o tipo de saída antes de
            // produzir o primeiro pacote. Encoders de hardware da Intel fazem
            // isso rotineiramente. Renegociar e seguir — este frame não gera
            // saída, o próximo gera.
            if let Err(e) = &result
                && e.code() == MF_E_TRANSFORM_STREAM_CHANGE
            {
                drop(sample);
                self.renegotiate_output()?;
                return Ok(());
            }
            result?;

            let Some(sample) = sample else { return Ok(()) };

            let is_keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint as *const GUID)
                .unwrap_or(0)
                == 1;
            let time_100ns = sample.GetSampleTime().unwrap_or(0);

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len: u32 = 0;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            buffer.Unlock()?;

            self.ready.push(Packet {
                data,
                is_keyframe,
                time_100ns,
            });
            Ok(())
        }
    }
}

/// Ajusta o controle de taxa para conteúdo de tela.
///
/// O padrão dos encoders é CBR, pensado para transmissão de vídeo em canal de
/// banda fixa: quando a cena não muda, o encoder **preenche** o bitstream com
/// dados de enchimento para manter a taxa constante. Numa tela parada isso
/// gasta megabits por segundo para transmitir nada — e, como o tráfego que não
/// fecha P2P passa pelo nosso relay TURN, é dinheiro queimado direto.
///
/// Cada ajuste é aplicado em regime de melhor esforço: drivers antigos
/// recusam alguns, e recusa não deve impedir o encoder de funcionar.
unsafe fn tune_rate_control(transform: &IMFTransform, fps: u32) -> Vec<&'static str> {
    unsafe {
        let Ok(codec) = transform.cast::<ICodecAPI>() else {
            return vec!["ICodecAPI indisponível"];
        };
        let mut applied = Vec::new();

        // VBR irrestrito: gasta bits quando a cena muda, e praticamente nada
        // quando ela está parada.
        let mode = VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32);
        if codec
            .SetValue(&CODECAPI_AVEncCommonRateControlMode, &mode)
            .is_ok()
        {
            applied.push("VBR");
        }

        // Keyframe periódico é desperdício em controle remoto. Numa tela
        // parada, ele é praticamente *todo* o tráfego: a cada intervalo o
        // encoder retransmite a imagem inteira que ninguém pediu.
        //
        // O intervalo vai para dez minutos — na prática, "nunca". Quem manda
        // gerar keyframe é o viewer, via `request_keyframe`, nos dois momentos
        // em que ele realmente precisa: ao entrar na sessão e ao detectar
        // perda. Ver `Message::RequestKeyframe`.
        // `PCSHARE_GOP` sobrepõe o intervalo, em frames, para experimentação.
        let gop_frames = std::env::var("PCSHARE_GOP")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            // 30 s. Escolhido por varredura, não por intuição: em 3 s e 10 s a
            // cena de uso ativo custava 186 e 125 kbps; em 30 s, 93 kbps, e
            // alongar mais não melhorou nada.
            .unwrap_or(fps * 30);
        let gop = VARIANT::from(gop_frames);
        if codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop).is_ok() {
            applied.push("keyframe sob demanda");
        }

        // Sem B-frames: reordenação adiciona latência de pipeline, que é
        // exatamente o que não podemos pagar.
        let bframes = VARIANT::from(0u32);
        if codec
            .SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &bframes)
            .is_ok()
        {
            applied.push("sem B-frames");
        }

        if applied.is_empty() {
            applied.push("nenhum ajuste aceito");
        }
        applied
    }
}

pub(crate) unsafe fn make_sample(nv12: &[u8], time: i64, duration: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(nv12.len() as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
    }
}

unsafe fn find_encoder(backend: Backend) -> Result<(IMFTransform, String)> {
    unsafe {
        find_transform(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
            backend,
        )
    }
}

/// Localiza e ativa um MFT que converta `in_subtype` em `out_subtype`.
pub(crate) unsafe fn find_transform(
    category: GUID,
    in_subtype: GUID,
    out_subtype: GUID,
    backend: Backend,
) -> Result<(IMFTransform, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: in_subtype,
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: out_subtype,
        };

        let flags = match backend {
            Backend::Hardware => MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
            Backend::Software => MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
        };

        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG(flags),
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )?;

        if count == 0 || activates.is_null() {
            return Err(windows::core::Error::new(
                MF_E_TOPO_CODEC_NOT_FOUND,
                "nenhum MFT disponível para este backend",
            ));
        }

        // MFT_ENUM_FLAG_SORTANDFILTER já ordena por preferência; o primeiro
        // que ativar serve.
        let list = std::slice::from_raw_parts(activates, count as usize);
        let mut chosen: Option<(IMFTransform, String)> = None;
        let mut last_err = None;

        for slot in list {
            let Some(activate) = slot else { continue };
            match activate.ActivateObject::<IMFTransform>() {
                Ok(t) => {
                    if chosen.is_none() {
                        chosen = Some((t, friendly_name(activate)));
                    }
                }
                Err(e) => last_err = Some(e),
            }
        }

        // Os ponteiros COM foram clonados pelos Option acima; resta liberar o
        // array que o MFTEnumEx alocou.
        CoTaskMemFree(Some(activates as *const c_void));

        match chosen {
            Some(v) => Ok(v),
            None => Err(last_err.unwrap_or_else(|| {
                windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "nenhum MFT ativou")
            })),
        }
    }
}

pub(crate) unsafe fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut pw = PWSTR::null();
        let mut len: u32 = 0;
        if activate
            .GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut len)
            .is_err()
        {
            return "(sem nome)".into();
        }
        let name = pw.to_string().unwrap_or_else(|_| "(nome inválido)".into());
        CoTaskMemFree(Some(pw.0 as *const c_void));
        name
    }
}
