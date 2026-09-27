//! Gravação de sessão: salva em **MP4** o vídeo H.264 que o viewer recebe, e
//! grava ao lado uma **ficha de auditoria** (quem acessou, qual máquina, quando,
//! por quanto tempo). É feita do lado do **operador** — é ele que precisa da
//! trilha, e o fluxo já chega pronto (não há recodificação).
//!
//! O fluxo que chega é H.264 **Annex B** com SPS/PPS embutidos nos keyframes.
//! Para o MP4, os parâmetros (SPS/PPS) vão no cabeçalho `avcC` e cada quadro
//! entra como amostra **AVCC** (NALs prefixados pelo tamanho). A gravação começa
//! sempre no **primeiro keyframe**, para o vídeo não nascer sem referência.

use std::fs::File;
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Instant;

use mp4::{AvcConfig, FourCC, MediaConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig, TrackType};

/// Timescale do filme e da faixa: milissegundos. Os timestamps saem do
/// `time_us` de cada quadro, então o tempo da gravação reflete o real (uma tela
/// parada não "acelera" na reprodução).
const TIMESCALE: u32 = 1000;

/// Dados de identidade da sessão, para a ficha de auditoria.
#[derive(Clone)]
pub struct MetaSessao {
    pub host_id: String,
    pub host_impressao: String,
    pub operador_id: String,
    pub operador_impressao: String,
    pub largura: u16,
    pub altura: u16,
}

pub struct Gravador {
    writer: Option<Mp4Writer<BufWriter<File>>>,
    caminho: PathBuf,
    meta: MetaSessao,
    faixa_criada: bool,
    inicio: Instant,
    inicio_local: String,
    base_us: Option<i64>,
    /// Amostra adiada em uma posição: só sei a duração dela quando chega a
    /// próxima (a diferença dos timestamps). A última sai no `finalizar`.
    pendente: Option<Amostra>,
    quadros: u64,
}

struct Amostra {
    bytes: Vec<u8>,
    inicio_ms: u64,
    keyframe: bool,
}

impl Gravador {
    /// Abre o arquivo MP4 e prepara a gravação. Ainda não escreve quadros — isso
    /// só começa quando o primeiro keyframe chega em [`Self::quadro`].
    pub fn iniciar(dir: &Path, meta: MetaSessao) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let nome = format!(
            "PCShare_{}_{}",
            so_digitos(&meta.host_id),
            carimbo_arquivo()
        );
        let caminho = dir.join(format!("{nome}.mp4"));
        let arquivo = File::create(&caminho)?;

        let cfg = Mp4Config {
            major_brand: FourCC::from(*b"isom"),
            minor_version: 512,
            compatible_brands: vec![
                FourCC::from(*b"isom"),
                FourCC::from(*b"iso2"),
                FourCC::from(*b"avc1"),
                FourCC::from(*b"mp41"),
            ],
            timescale: TIMESCALE,
        };
        let writer = Mp4Writer::write_start(BufWriter::new(arquivo), &cfg).map_err(erro)?;

        Ok(Self {
            writer: Some(writer),
            caminho,
            meta,
            faixa_criada: false,
            inicio: Instant::now(),
            inicio_local: agora_local(),
            base_us: None,
            pendente: None,
            quadros: 0,
        })
    }

    /// Alimenta um pacote H.264 (Annex B). Ignora tudo até o primeiro keyframe;
    /// dele extrai SPS/PPS e cria a faixa de vídeo, e daí em diante grava.
    pub fn quadro(&mut self, dados: &[u8], keyframe: bool, time_us: i64) {
        let Some(writer) = self.writer.as_mut() else {
            return;
        };

        if !self.faixa_criada {
            if !keyframe {
                return;
            }
            let nals = separar_nals(dados);
            let sps = nals.iter().find(|n| tipo_nal(n) == 7).map(|n| n.to_vec());
            let pps = nals.iter().find(|n| tipo_nal(n) == 8).map(|n| n.to_vec());
            let (Some(sps), Some(pps)) = (sps, pps) else {
                return; // espera um keyframe que traga os parâmetros
            };
            let faixa = TrackConfig {
                track_type: TrackType::Video,
                timescale: TIMESCALE,
                language: "und".to_string(),
                media_conf: MediaConfig::AvcConfig(AvcConfig {
                    width: self.meta.largura,
                    height: self.meta.altura,
                    seq_param_set: sps,
                    pic_param_set: pps,
                }),
            };
            if writer.add_track(&faixa).is_err() {
                return;
            }
            self.faixa_criada = true;
            self.base_us = Some(time_us);
        }

        let base = self.base_us.unwrap_or(time_us);
        let inicio_ms = ((time_us - base).max(0) / 1000) as u64;
        let avcc = para_avcc(dados);
        if avcc.is_empty() {
            return;
        }

        // Fecha a amostra anterior agora que sei quando esta começa (a duração é
        // a diferença). Sem isso, o MP4 não teria os tempos certos.
        if let Some(a) = self.pendente.take() {
            let dur = inicio_ms.saturating_sub(a.inicio_ms).max(1) as u32;
            self.escrever(a, dur);
        }
        self.pendente = Some(Amostra {
            bytes: avcc,
            inicio_ms,
            keyframe,
        });
    }

    fn escrever(&mut self, a: Amostra, dur_ms: u32) {
        if let Some(writer) = self.writer.as_mut() {
            let amostra = Mp4Sample {
                start_time: a.inicio_ms,
                duration: dur_ms,
                rendering_offset: 0,
                is_sync: a.keyframe,
                bytes: mp4::Bytes::from(a.bytes),
            };
            if writer.write_sample(1, &amostra).is_ok() {
                self.quadros += 1;
            }
        }
    }

    /// Fecha o MP4 (grava o índice) e escreve a ficha de auditoria ao lado.
    pub fn finalizar(mut self) -> io::Result<PathBuf> {
        self.finalizar_interno();
        Ok(self.caminho.clone())
    }

    /// O trabalho de fechar, idempotente (o `writer` vira `None` na primeira
    /// vez). Serve tanto ao `finalizar` explícito quanto ao `Drop` — assim uma
    /// gravação em curso é fechada mesmo se a sessão cair sem passar por "Parar".
    fn finalizar_interno(&mut self) {
        if self.writer.is_none() {
            return;
        }
        // A última amostra pendente sai com uma duração nominal.
        if let Some(a) = self.pendente.take() {
            self.escrever(a, 33);
        }
        if let Some(mut writer) = self.writer.take() {
            let _ = writer.write_end(); // grava o `moov`
            // `writer` (BufWriter<File>) é derrubado aqui → descarrega o buffer.
        }
        if let Err(e) = self.escrever_ficha() {
            eprintln!("gravação: falha ao escrever a ficha ({e})");
        }
    }

    fn escrever_ficha(&self) -> io::Result<()> {
        let ficha = self.caminho.with_extension("json");
        let nome = self
            .caminho
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let json = format!(
            "{{\n  \"produto\": \"PCShare\",\n  \"tipo\": \"gravacao_de_sessao\",\n  \
             \"arquivo\": \"{}\",\n  \"inicio\": \"{}\",\n  \"fim\": \"{}\",\n  \
             \"duracao_segundos\": {},\n  \"quadros\": {},\n  \"resolucao\": \"{}x{}\",\n  \
             \"host\": {{ \"id\": \"{}\", \"impressao\": \"{}\" }},\n  \
             \"operador\": {{ \"id\": \"{}\", \"impressao\": \"{}\" }}\n}}\n",
            nome,
            self.inicio_local,
            agora_local(),
            self.inicio.elapsed().as_secs(),
            self.quadros,
            self.meta.largura,
            self.meta.altura,
            self.meta.host_id,
            self.meta.host_impressao,
            self.meta.operador_id,
            self.meta.operador_impressao,
        );
        std::fs::write(&ficha, json)
    }
}

impl Drop for Gravador {
    fn drop(&mut self) {
        self.finalizar_interno();
    }
}

fn erro(e: mp4::Error) -> io::Error {
    io::Error::other(e.to_string())
}

/// Tipo da NAL (5 bits baixos do primeiro byte): 7=SPS, 8=PPS, 9=AUD, 5=IDR.
fn tipo_nal(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(0)
}

/// Fatia um fluxo Annex B nas NAL units (sem os start codes 00 00 01 / 00 00 00 01).
fn separar_nals(d: &[u8]) -> Vec<&[u8]> {
    let mut inicios = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            inicios.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(inicios.len());
    for k in 0..inicios.len() {
        let s = inicios[k];
        let mut e = if k + 1 < inicios.len() {
            inicios[k + 1] - 3
        } else {
            d.len()
        };
        // Recua sobre os zeros de prefixo do próximo start code (caso 4 bytes).
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            nals.push(&d[s..e]);
        }
    }
    nals
}

/// Converte Annex B → AVCC (cada NAL prefixado por 4 bytes de tamanho). SPS/PPS
/// vão no `avcC`, não nas amostras; AUD é descartado.
fn para_avcc(annexb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(annexb.len());
    for nal in separar_nals(annexb) {
        match tipo_nal(nal) {
            7 | 8 | 9 => continue,
            _ => {
                out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                out.extend_from_slice(nal);
            }
        }
    }
    out
}

fn so_digitos(s: &str) -> String {
    let d: String = s.chars().filter(char::is_ascii_digit).collect();
    if d.is_empty() { "sessao".into() } else { d }
}

fn tempo_local() -> windows::Win32::Foundation::SYSTEMTIME {
    unsafe { windows::Win32::System::SystemInformation::GetLocalTime() }
}

/// `AAAA-MM-DD HH:MM:SS` para a ficha.
fn agora_local() -> String {
    let t = tempo_local();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// `AAAAMMDD-HHMMSS` para o nome do arquivo.
fn carimbo_arquivo() -> String {
    let t = tempo_local();
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::{para_avcc, separar_nals, tipo_nal};

    #[test]
    fn separa_nals_com_start_codes_de_3_e_4_bytes() {
        // 4-byte start + NAL(tipo 7), 3-byte start + NAL(tipo 5).
        let fluxo = [0, 0, 0, 1, 0x67, 0xAA, 0, 0, 1, 0x65, 0xBB, 0xCC];
        let nals = separar_nals(&fluxo);
        assert_eq!(nals.len(), 2);
        assert_eq!(tipo_nal(nals[0]), 7);
        assert_eq!(nals[0], &[0x67, 0xAA]);
        assert_eq!(tipo_nal(nals[1]), 5);
        assert_eq!(nals[1], &[0x65, 0xBB, 0xCC]);
    }

    #[test]
    fn avcc_prefixa_tamanho_e_descarta_sps_pps() {
        // SPS(7) + IDR(5): o SPS sai (vai no avcC); só o IDR vira amostra AVCC.
        let fluxo = [0, 0, 0, 1, 0x67, 0x11, 0, 0, 0, 1, 0x65, 0x22, 0x33];
        let avcc = para_avcc(&fluxo);
        // 4 bytes de tamanho (=3) + 3 bytes do IDR.
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 0x22, 0x33]);
    }

    // Prova ponta a ponta: encoda quadros de verdade, grava o MP4, e **reabre**
    // o arquivo para confirmar que saiu um MP4 válido (contêiner reproduzível).
    #[test]
    fn grava_mp4_valido_de_pacotes_reais() {
        use super::{Gravador, MetaSessao};
        use pcshare_codec::H264Encoder;

        let (w, h) = (320u32, 240u32);
        let mut enc = match H264Encoder::new(w, h, 30, 4_000_000) {
            Ok(e) => e,
            // Sem Media Foundation/encoder (ex.: CI): o teste não se aplica.
            Err(_) => return,
        };

        let dir = std::env::temp_dir().join("pcshare_grav_teste");
        let meta = MetaSessao {
            host_id: "578637276".into(),
            host_impressao: "AB:CD".into(),
            operador_id: "111222333".into(),
            operador_impressao: "EF:01".into(),
            largura: w as u16,
            altura: h as u16,
        };
        let mut g = Gravador::iniciar(&dir, meta).expect("iniciar");

        let mut t_us = 0i64;
        let mut nv12 = vec![128u8; (w * h + w * h / 2) as usize];
        for i in 0..30u8 {
            // Varia o brilho para forçar o encoder a gerar quadros.
            for p in nv12.iter_mut().take((w * h) as usize) {
                *p = 40u8.wrapping_add(i.wrapping_mul(7));
            }
            let _ = enc.submit(&nv12);
            for p in enc.take_packets() {
                g.quadro(&p.data, p.is_keyframe, t_us);
                t_us += 33_000;
            }
        }
        let _ = enc.finish();
        for p in enc.take_packets() {
            g.quadro(&p.data, p.is_keyframe, t_us);
            t_us += 33_000;
        }

        let caminho = g.finalizar().expect("finalizar");
        let arquivo = std::fs::File::open(&caminho).expect("abrir mp4");
        let tam = arquivo.metadata().unwrap().len();
        assert!(tam > 1000, "mp4 pequeno demais ({tam} bytes)");

        // Reabrir com o leitor prova que o contêiner está bem-formado.
        let reader = mp4::Mp4Reader::read_header(std::io::BufReader::new(arquivo), tam);
        assert!(reader.is_ok(), "mp4 inválido: {:?}", reader.err());
        assert!(caminho.with_extension("json").exists(), "faltou a ficha");

        let _ = std::fs::remove_file(&caminho);
        let _ = std::fs::remove_file(caminho.with_extension("json"));
    }
}
