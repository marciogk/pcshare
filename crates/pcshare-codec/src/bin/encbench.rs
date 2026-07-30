//! Sprint 0, etapa 2: o encoder aguenta 1080p30 nesta máquina?
//!
//! Alimenta o encoder com frames sintéticos de 1920x1080 em NV12. Como
//! codificar não depende do que está no monitor, esta é uma medição legítima
//! de 1080p mesmo numa tela de 1360x768 — ver a seção de restrição em
//! `docs/SPRINT-0.md`.
//!
//!     cargo run --release -p pcshare-codec --bin encbench
//!     cargo run --release -p pcshare-codec --bin encbench -- 1920 1080 30 10
//!
//! O bitstream de cada cena vai para `spike-output/`. Abra no VLC: se não
//! reproduzir, o número de fps não significa nada.

use pcshare_codec::{Backend, H264Encoder};
use std::time::Instant;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let width: u32 = a.first().and_then(|v| v.parse().ok()).unwrap_or(1920);
    let height: u32 = a.get(1).and_then(|v| v.parse().ok()).unwrap_or(1080);
    let fps: u32 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(30);
    let secs: u32 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(10);
    let frames = fps * secs;

    if cfg!(debug_assertions) {
        eprintln!("AVISO: build de debug. Use --release ou ignore os números.\n");
    }

    println!("PCShare — Sprint 0, etapa 2: encoder H.264");
    println!("{width}x{height} @ {fps} fps, {frames} frames por cena\n");

    let _ = std::fs::create_dir_all("spike-output");

    // Filtros para varredura rápida de parâmetros:
    //   PCSHARE_SCENE=parado|desktop|video   PCSHARE_BACKEND=hw|sw
    let só_cena = std::env::var("PCSHARE_SCENE").ok();
    let só_backend = std::env::var("PCSHARE_BACKEND").ok();

    for backend in [Backend::Hardware, Backend::Software] {
        if let Some(f) = só_backend.as_deref() {
            let tag = if backend == Backend::Hardware { "hw" } else { "sw" };
            if f != tag {
                continue;
            }
        }
        let label = match backend {
            Backend::Hardware => "HARDWARE (Quick Sync / NVENC / AMF)",
            Backend::Software => "SOFTWARE (referência de comparação)",
        };
        println!("{:=<70}", "");
        println!("{label}");
        println!("{:=<70}", "");

        for scene in Scene::ALL {
            if let Some(f) = só_cena.as_deref()
                && !scene.name().starts_with(f)
            {
                continue;
            }
            match run(backend, scene, width, height, fps, frames) {
                Ok(r) => r.print(fps),
                Err(e) => {
                    println!("  {:<10} indisponível: {e}", scene.name());
                    // Se o hardware não existe, a cena seguinte também falha.
                    break;
                }
            }
        }
        println!();
    }

    println!("Metas do Sprint 0:");
    println!("  · encode < {:.1} ms/frame para sustentar {fps} fps", 1000.0 / fps as f64);
    println!("  · cena parada  < 100 kbps");
    println!("  · cena de vídeo < 8 Mbps");
    println!("\nValide os arquivos em spike-output/ num player antes de");
    println!("acreditar em qualquer um destes números.");
}

#[derive(Clone, Copy, PartialEq)]
enum Scene {
    /// Tela absolutamente imóvel. Mede o piso de banda — corresponde à linha
    /// "banda, tela parada" do critério do Sprint 0.
    Parado,
    /// Janela deslizando e texto rolando sobre fundo claro. Uso ativo de
    /// escritório: o caso dominante do produto.
    Desktop,
    /// Panorâmica suave sobre uma textura coerente, mais um objeto em
    /// movimento. Corresponde à linha "vídeo em tela cheia".
    Video,
}

impl Scene {
    const ALL: [Scene; 3] = [Scene::Parado, Scene::Desktop, Scene::Video];

    fn name(&self) -> &'static str {
        match self {
            Scene::Parado => "parado",
            Scene::Desktop => "desktop",
            Scene::Video => "vídeo",
        }
    }

    /// Teto de bitrate passado ao encoder, não piso: a cena parada deve ficar
    /// ordens de grandeza abaixo do seu.
    fn bitrate(&self) -> u32 {
        match self {
            Scene::Parado | Scene::Desktop => 2_000_000,
            Scene::Video => 8_000_000,
        }
    }

    fn target(&self) -> &'static str {
        match self {
            Scene::Parado => "< 100 kbps",
            Scene::Desktop => "—",
            Scene::Video => "< 8 Mbps",
        }
    }
}

struct Report {
    scene: &'static str,
    target: &'static str,
    encoder: String,
    tuning: String,
    frames: u32,
    bytes: u64,
    keyframes: u32,
    wall_s: f64,
    per_frame_ms: Vec<f64>,
}

impl Report {
    fn print(mut self, target_fps: u32) {
        self.per_frame_ms
            .sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));

        let achieved = self.frames as f64 / self.wall_s;
        let mbps = (self.bytes as f64 * 8.0) / (self.frames as f64 / target_fps as f64) / 1e6;
        let budget = 1000.0 / target_fps as f64;
        let verdict = if achieved >= target_fps as f64 { "OK" } else { "ABAIXO" };

        println!("  cena {:<8} [{}]", self.scene, self.encoder);
        println!("    ajustes    : {}", self.tuning);
        println!(
            "    throughput : {achieved:>7.1} fps   (alvo {target_fps}, {verdict})",
        );
        println!(
            "    por frame  : p50 {:>6.2} ms   p99 {:>6.2} ms   máx {:>6.2} ms   (orçamento {budget:.1} ms)",
            pct(&self.per_frame_ms, 0.50),
            pct(&self.per_frame_ms, 0.99),
            self.per_frame_ms.last().copied().unwrap_or(0.0),
        );
        let rate = if mbps < 1.0 {
            format!("{:>7.0} kbps", mbps * 1000.0)
        } else {
            format!("{mbps:>7.2} Mbps")
        };
        println!(
            "    bitstream  : {rate}   (meta {})   {} KB, {} keyframes",
            self.target,
            self.bytes / 1024,
            self.keyframes,
        );
    }
}

fn run(
    backend: Backend,
    scene: Scene,
    width: u32,
    height: u32,
    fps: u32,
    frames: u32,
) -> Result<Report, windows::core::Error> {
    let mut enc = H264Encoder::with_backend(width, height, fps, scene.bitrate(), backend)?;
    let mut buf = vec![0u8; enc.nv12_len()];
    let renderer = Renderer::new(scene, width, height);

    let mut bytes = 0u64;
    let mut keyframes = 0u32;
    let mut per_frame_ms = Vec::with_capacity(frames as usize);
    let mut out = std::fs::File::create(format!(
        "spike-output/{}-{:?}.h264",
        scene.name(),
        backend
    ))
    .ok();

    // Aquece: a primeira chamada carrega o driver e distorceria o p50.
    renderer.render(0, &mut buf);
    let _ = enc.submit(&buf);
    let _ = enc.take_packets();

    let wall = Instant::now();
    for i in 0..frames {
        // A geração fica fora da região cronometrada: queremos o custo do
        // encoder, não o do gerador de cena.
        renderer.render(i + 1, &mut buf);

        let t0 = Instant::now();
        enc.submit(&buf)?;
        per_frame_ms.push(t0.elapsed().as_secs_f64() * 1000.0);

        for p in enc.take_packets() {
            bytes += p.data.len() as u64;
            keyframes += u32::from(p.is_keyframe);
            if let Some(f) = out.as_mut() {
                use std::io::Write;
                let _ = f.write_all(&p.data);
            }
        }
    }
    enc.finish()?;
    let wall_s = wall.elapsed().as_secs_f64();

    for p in enc.take_packets() {
        bytes += p.data.len() as u64;
        keyframes += u32::from(p.is_keyframe);
        if let Some(f) = out.as_mut() {
            use std::io::Write;
            let _ = f.write_all(&p.data);
        }
    }

    Ok(Report {
        scene: scene.name(),
        target: scene.target(),
        tuning: enc.tuning().join(", "),
        encoder: enc.name().to_string(),
        frames,
        bytes,
        keyframes,
        wall_s,
        per_frame_ms,
    })
}

/// Gera as cenas em NV12: plano Y de `w*h`, seguido do plano UV entrelaçado
/// com metade das linhas.
///
/// A cena de vídeo usa uma textura suave pré-calculada, percorrida em
/// panorâmica. Isso importa: uma textura de ruído seria o pior caso absoluto
/// para o encoder e não se parece com vídeo nenhum. Conteúdo real tem
/// coerência espacial e temporal, e é ela que a estimação de movimento
/// explora — medir com ruído produziria um veredito injustamente pessimista.
struct Renderer {
    scene: Scene,
    w: usize,
    h: usize,
    /// Só para `Scene::Video`: textura de fundo maior que o quadro, com
    /// margem para a panorâmica.
    bg: Vec<u8>,
    bg_w: usize,
}

const PAN_MARGIN: usize = 256;

impl Renderer {
    fn new(scene: Scene, width: u32, height: u32) -> Self {
        let (w, h) = (width as usize, height as usize);
        let (bg, bg_w) = if scene == Scene::Video {
            let bw = w + PAN_MARGIN;
            let bh = h + PAN_MARGIN;
            let mut bg = vec![0u8; bw * bh];
            for y in 0..bh {
                let fy = y as f32;
                for x in 0..bw {
                    let fx = x as f32;
                    // Três escalas de variação suave: aproxima o espectro de
                    // uma cena natural muito melhor do que um XOR de bits.
                    let v = 128.0
                        + 55.0 * (fx * 0.004).sin()
                        + 45.0 * (fy * 0.006).sin()
                        + 30.0 * ((fx + fy) * 0.017).sin();
                    bg[y * bw + x] = v.clamp(0.0, 255.0) as u8;
                }
            }
            (bg, bw)
        } else {
            (Vec::new(), 0)
        };
        Self { scene, w, h, bg, bg_w }
    }

    fn render(&self, n: u32, buf: &mut [u8]) {
        let (w, h) = (self.w, self.h);
        let (y_plane, uv_plane) = buf.split_at_mut(w * h);

        match self.scene {
            Scene::Parado => {
                // Desenhado igual em todo frame. Depois do keyframe inicial o
                // encoder não deveria gastar quase nada.
                Self::desktop_base(y_plane, uv_plane, w, h);
                Self::window(y_plane, w, h, w / 8, h / 8, 0);
            }
            Scene::Desktop => {
                Self::desktop_base(y_plane, uv_plane, w, h);
                let win_w = w * 2 / 3;
                let x0 = (n as usize / 2) % (w - win_w).max(1);
                Self::window(y_plane, w, h, x0, h / 8, n);
            }
            Scene::Video => {
                // Panorâmica diagonal sobre a textura: movimento global que a
                // estimação de movimento consegue acompanhar, como vídeo real.
                let t = n as usize;
                let ox = (t * 3) % PAN_MARGIN;
                let oy = (t * 2) % PAN_MARGIN;
                for y in 0..h {
                    let src = (y + oy) * self.bg_w + ox;
                    y_plane[y * w..y * w + w].copy_from_slice(&self.bg[src..src + w]);
                }

                // Objeto brilhante com trajetória própria: movimento local
                // que não segue a panorâmica.
                let cx = w / 2 + ((t as f32 * 0.08).sin() * (w as f32 * 0.3)) as usize;
                let cy = h / 2 + ((t as f32 * 0.11).cos() * (h as f32 * 0.25)) as usize;
                let r = (h / 12) as isize;
                for dy in -r..r {
                    let y = cy.wrapping_add_signed(dy);
                    if y >= h {
                        continue;
                    }
                    let half = ((r * r - dy * dy) as f64).sqrt() as isize;
                    for dx in -half..half {
                        let x = cx.wrapping_add_signed(dx);
                        if x < w {
                            y_plane[y * w + x] = 245;
                        }
                    }
                }

                // Croma variando devagar: cor constante subestimaria o custo.
                for y in 0..h / 2 {
                    let row = &mut uv_plane[y * w..y * w + w];
                    for (x, px) in row.iter_mut().enumerate() {
                        *px = (128.0 + 40.0 * (((x + t * 2) as f32) * 0.003).sin()) as u8;
                    }
                }
            }
        }
    }

    fn desktop_base(y_plane: &mut [u8], uv_plane: &mut [u8], _w: usize, _h: usize) {
        y_plane.fill(225);
        uv_plane.fill(128);
    }

    /// Janela com moldura dura e linhas de texto. Bordas nítidas são o que o
    /// H.264 codifica pior em conteúdo de tela.
    fn window(y_plane: &mut [u8], w: usize, h: usize, x0: usize, y0: usize, n: u32) {
        let win_w = w * 2 / 3;
        let win_h = h * 2 / 3;
        let x1 = (x0 + win_w).min(w);
        let y1 = (y0 + win_h).min(h);

        for y in y0..y1 {
            let row = &mut y_plane[y * w..y * w + w];
            let edge_row = y == y0 || y + 1 == y1;
            for x in x0..x1 {
                row[x] = if edge_row || x == x0 || x + 1 == x1 {
                    40
                } else {
                    250
                };
            }
        }

        let scroll = (n as usize / 3) % 24;
        let mut y = y0 + 24 + scroll;
        while y < y1 {
            let row = &mut y_plane[y * w..y * w + w];
            let text_end = (x0 + win_w * 3 / 4).min(w);
            for x in (x0 + 20).min(w)..text_end {
                if (x / 7 + y / 12) % 5 != 0 {
                    row[x] = 30;
                }
            }
            y += 24;
        }
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}
