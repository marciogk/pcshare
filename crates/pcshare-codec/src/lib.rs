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
use std::sync::{Arc, Once};

use pcshare_gpu::GpuContext;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

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
    gpu_direct: bool,
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

    /// Encoder ligado a um dispositivo D3D11, aceitando texturas via
    /// [`Self::submit_texture`].
    ///
    /// Cai para o caminho por memória de sistema se o MFT não for capaz de
    /// D3D11 — o que acontece com o encoder de software e com drivers
    /// antigos. Verifique com [`Self::is_gpu_direct`] antes de mandar
    /// texturas.
    pub fn with_gpu(
        gpu: &Arc<GpuContext>,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
    ) -> Result<Self> {
        ensure_mf_started();
        Self::build(width, height, fps, bitrate, Backend::Hardware, Some(gpu))
    }

    pub fn with_backend(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        backend: Backend,
    ) -> Result<Self> {
        ensure_mf_started();
        Self::build(width, height, fps, bitrate, backend, None)
    }

    /// `true` se o encoder aceita texturas D3D11 diretamente.
    pub fn is_gpu_direct(&self) -> bool {
        self.gpu_direct
    }

    fn build(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        backend: Backend,
        gpu: Option<&Arc<GpuContext>>,
    ) -> Result<Self> {
        unsafe {
            // Enumera TODOS os encoders candidatos e tenta configurar um a um,
            // parando no primeiro que der certo. O motivo é concreto: num PC com
            // duas GPUs (Intel integrada + NVIDIA), o sistema oferece um MFT de
            // cada fabricante. O primeiro da lista pode ser de um fabricante
            // enquanto o nosso dispositivo D3D é do outro (o que aciona a tela),
            // e o par não casa — E_FAIL na hora de amarrar o gerenciador D3D.
            // Tentar só o primeiro fazia a máquina do usuário (Intel+NVIDIA)
            // cair para software com 0x80004005, com o NVENC ali do lado, só
            // precisando ser o candidato escolhido.
            let candidatos = find_activates(
                MFT_CATEGORY_VIDEO_ENCODER,
                MFVideoFormat_NV12,
                MFVideoFormat_H264,
                backend,
            )?;

            let mut ultimo_erro: Option<windows::core::Error> = None;
            for activate in candidatos {
                let nome = friendly_name(&activate);
                // Uma linha por candidato, só no caminho de hardware: é o que
                // revela, no console da máquina remota, quais encoders existem e
                // qual falhou.
                if gpu.is_some() {
                    eprintln!("codec: encoder de hardware candidato: '{nome}'");
                }
                let transform = match activate.ActivateObject::<IMFTransform>() {
                    Ok(t) => t,
                    Err(e) => {
                        if gpu.is_some() {
                            eprintln!("codec: '{nome}' não ativou ({e})");
                        }
                        ultimo_erro = Some(e);
                        continue;
                    }
                };
                match Self::configure_transform(
                    transform, &nome, width, height, fps, bitrate, backend, gpu,
                ) {
                    Ok(enc) => return Ok(enc),
                    Err(e) => {
                        if gpu.is_some() {
                            eprintln!("codec: '{nome}' não configurou ({e}); tentando o próximo");
                        }
                        let _ = activate.ShutdownObject();
                        ultimo_erro = Some(e);
                    }
                }
            }

            Err(ultimo_erro.unwrap_or_else(|| {
                windows::core::Error::new(
                    MF_E_TOPO_CODEC_NOT_FOUND,
                    "nenhum encoder configurou para este backend",
                )
            }))
        }
    }

    /// Amarra um MFT já ativado à nossa GPU e tipos de mídia. Falha se o par
    /// MFT↔dispositivo não casar (o caso de duas GPUs) — o chamador tenta então
    /// o próximo candidato.
    unsafe fn configure_transform(
        transform: IMFTransform,
        name: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        backend: Backend,
        gpu: Option<&Arc<GpuContext>>,
    ) -> Result<Self> {
        unsafe {
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

            // O gerenciador D3D precisa chegar **antes** dos tipos de mídia:
            // é ele que faz o MFT passar a aceitar superfícies de vídeo em vez
            // de buffers de memória de sistema.
            let mut gpu_direct = false;
            if let Some(gpu) = gpu
                && attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) == 1
            {
                let mut token = 0u32;
                let mut manager: Option<IMFDXGIDeviceManager> = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.expect("MFCreateDXGIDeviceManager sem manager");
                manager.ResetDevice(gpu.device(), token)?;

                transform
                    .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
                gpu_direct = true;
            }

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

            let tuning = tune_rate_control(&transform, fps, bitrate, backend);

            // Sem FLUSH aqui: o encoder de software o rejeita antes de o
            // streaming começar, e não há nada para descartar.
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                transform,
                events,
                backend,
                name: name.to_string(),
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
                gpu_direct,
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
        let (time, duration) = self.next_timestamp();
        let sample = unsafe { make_sample(nv12, time, duration)? };
        self.push(sample)
    }

    /// Envia um frame que já está na GPU, sem cópia para a memória de sistema.
    ///
    /// `tex` precisa ser NV12 e pertencer ao **mesmo dispositivo D3D11** que
    /// foi passado em [`Self::with_gpu`]. Só funciona se o encoder tiver sido
    /// criado por lá — verificável em [`Self::is_gpu_direct`].
    pub fn submit_texture(&mut self, tex: &ID3D11Texture2D) -> Result<()> {
        debug_assert!(
            self.is_gpu_direct(),
            "submit_texture exige encoder criado com with_gpu"
        );
        let (time, duration) = self.next_timestamp();
        let sample = unsafe { make_texture_sample(tex, time, duration)? };
        self.push(sample)
    }

    /// Marca de tempo e duração do próximo frame, aplicando o pedido de
    /// keyframe pendente.
    fn next_timestamp(&mut self) -> (i64, i64) {
        if self.force_keyframe {
            unsafe {
                if let Ok(codec) = self.transform.cast::<ICodecAPI>() {
                    let _ =
                        codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
                }
            }
            self.force_keyframe = false;
        }
        let duration = 10_000_000i64 / self.fps as i64;
        let time = self.frame_index * duration;
        self.frame_index += 1;
        (time, duration)
    }

    fn push(&mut self, sample: IMFSample) -> Result<()> {
        unsafe {
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
unsafe fn tune_rate_control(
    transform: &IMFTransform,
    fps: u32,
    bitrate: u32,
    backend: Backend,
) -> Vec<&'static str> {
    unsafe {
        let Ok(codec) = transform.cast::<ICodecAPI>() else {
            return vec!["ICodecAPI indisponível"];
        };
        let mut applied = Vec::new();

        // O controle de taxa é escolhido POR backend, de propósito:
        //
        // - **Hardware** (Quick Sync / NVENC / AMF): VBR irrestrito, o modo que
        //   rendeu imagem "ótima" no teste real de 17/09/2026. É o caminho bom;
        //   não se mexe no que funciona.
        //
        // - **Software** (encoder da Microsoft, o fallback quando não há
        //   hardware): qualidade constante. O VBR do software raciona os bits
        //   para uma média baixa e a imagem fica MOLE quando a cena muda —
        //   medido em 22/09/2026 na máquina sem encoder de hardware, onde subir
        //   o teto de bitrate não mudava nada. Quality mode aloca para manter a
        //   qualidade quadro a quadro, que é o certo para tela. (Esse encoder
        //   aceita o modo mas IGNORA o valor fino de qualidade — 60, 75 e 90
        //   deram o mesmo bitstream; ainda assim aloca melhor que o VBR.)
        let usar_quality = backend == Backend::Software && {
            let mode = VARIANT::from(eAVEncCommonRateControlMode_Quality.0 as u32);
            codec
                .SetValue(&CODECAPI_AVEncCommonRateControlMode, &mode)
                .is_ok()
        };
        if usar_quality {
            applied.push("qualidade constante");
            // 0..100. O encoder de software ignora o valor, mas o de hardware o
            // respeita — deixado aqui para quando o quality mode valer também no
            // hardware. `PCSHARE_QUALIDADE` sobrepõe.
            let q = std::env::var("PCSHARE_QUALIDADE")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(75)
                .min(100);
            let qv = VARIANT::from(q);
            if codec.SetValue(&CODECAPI_AVEncCommonQuality, &qv).is_ok() {
                applied.push("nitidez alta");
            }
            // Teto de banda, melhor esforço: nem mirando qualidade deixa um
            // scroll pesado estourar o upload do host. Encoder que ignora, a
            // contrapressão cobre.
            let maxbr = VARIANT::from(bitrate);
            if codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &maxbr).is_ok() {
                applied.push("teto de banda");
            }
        } else {
            // VBR com TETO DE PICO. O irrestrito rende ótima imagem em link
            // rápido, mas numa mudança grande (trocar de slide, rolar a tela) o
            // encoder estoura para muito acima da média por um instante — e num
            // link apertado (relay, upload de casa baixo) esse pico entope o cano
            // e a tela TRAVA por segundos até escoar. Observado como travadas de
            // ~10 s "sem lógica" em troca de slide. O teto limita o pico e ele
            // ACOMPANHA o bitrate: bitrate baixo (link apertado) → teto baixo,
            // protege; bitrate alto (link rápido) → teto alto, quase não se nota.
            // `PCSHARE_PICO` sobrepõe o multiplicador (padrão 1.5×).
            let mult = std::env::var("PCSHARE_PICO")
                .ok()
                .and_then(|v| v.parse::<f32>().ok())
                .filter(|m| (1.0..=8.0).contains(m))
                .unwrap_or(1.5);
            let pico = ((bitrate as f32) * mult) as u32;
            let mode = VARIANT::from(eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32);
            if codec
                .SetValue(&CODECAPI_AVEncCommonRateControlMode, &mode)
                .is_ok()
            {
                applied.push("VBR c/ teto");
                let media = VARIANT::from(bitrate);
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &media);
                let teto = VARIANT::from(pico);
                if codec.SetValue(&CODECAPI_AVEncCommonMaxBitRate, &teto).is_ok() {
                    applied.push("pico limitado");
                }
            } else {
                // Driver antigo sem PeakConstrained: volta ao irrestrito.
                let m2 = VARIANT::from(eAVEncCommonRateControlMode_UnconstrainedVBR.0 as u32);
                if codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &m2).is_ok() {
                    applied.push("VBR");
                }
            }
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

/// Embrulha uma textura D3D11 num `IMFSample` sem copiar pixel algum.
///
/// É este ponto que elimina o desvio pela CPU: o encoder recebe uma
/// referência à superfície que já está na memória de vídeo.
unsafe fn make_texture_sample(
    tex: &ID3D11Texture2D,
    time: i64,
    duration: i64,
) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, tex, 0, false)?;

        // O buffer nasce com comprimento zero; sem declarar o tamanho útil o
        // encoder trata o frame como vazio.
        if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
            let len = two_d.GetContiguousLength()?;
            buffer.SetCurrentLength(len)?;
        }

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(duration)?;
        Ok(sample)
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

/// Enumera os MFTs candidatos (ainda **não ativados**) que convertem
/// `in_subtype`→`out_subtype`, na ordem de preferência do sistema. Ativar cada
/// um custa recurso (uma sessão de NVENC, por exemplo), então a ativação fica
/// para o chamador, que tenta um de cada vez e para no primeiro que configurar.
unsafe fn find_activates(
    category: GUID,
    in_subtype: GUID,
    out_subtype: GUID,
    backend: Backend,
) -> Result<Vec<IMFActivate>> {
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

        let list = std::slice::from_raw_parts(activates, count as usize);
        let candidatos: Vec<IMFActivate> = list.iter().flatten().cloned().collect();

        // Os ponteiros COM foram clonados acima; resta liberar o array que o
        // MFTEnumEx alocou.
        CoTaskMemFree(Some(activates as *const c_void));

        Ok(candidatos)
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
