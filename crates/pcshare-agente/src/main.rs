// Sem console no release; a janela preta atrás seria feia num produto.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! **PCShare Agente** — o app GRÁTIS de quem só *permite* acesso à sua máquina.
//!
//! Ele apenas compartilha a tela (papel de host) e mostra o ID + a senha de
//! sessão para a pessoa dizer a quem vai ajudá-la. **Não conecta em ninguém** e
//! **não pede licença** — a cobrança é toda do lado do operador (o app
//! `pcshare`). Reusa o motor `pcshare-sessao`; aqui só mora a telinha.

use eframe::egui;
use egui::Color32;
use pcshare_core::secure;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// Mesmo servidor de encontro do operador (o VPS). Editável em Configurações.
const ENCONTRO_PADRAO: &str = "198.50.117.107:7100";
const ACCENT: Color32 = Color32::from_rgb(0x2E, 0x6F, 0xD6);

fn pasta_pcshare() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("PCShare")
}

/// Lê o endereço do encontro do `ui.txt` compartilhado (o mesmo que o operador
/// usa), ou cai no padrão.
fn carregar_encontro() -> String {
    if let Ok(txt) = std::fs::read_to_string(pasta_pcshare().join("ui.txt")) {
        for linha in txt.lines() {
            if let Some(v) = linha.strip_prefix("encontro=") {
                let v = v.trim();
                if !v.is_empty() {
                    return v.to_string();
                }
            }
        }
    }
    ENCONTRO_PADRAO.to_string()
}

fn salvar_encontro(s: &str) {
    let dir = pasta_pcshare();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("ui.txt"), format!("encontro={}\n", s.trim()));
}

#[derive(Default)]
struct EstadoSessao {
    mensagem: String,
    ativa: bool,
}

struct App {
    id_local: String,
    fingerprint: String,
    encontro: String,
    senha: String,
    estado: Arc<Mutex<EstadoSessao>>,
    status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        aplicar_tema(&cc.egui_ctx);
        let (id_local, fingerprint) = pcshare_sessao::identidade_local()
            .unwrap_or_else(|| ("— erro ao carregar a identidade —".into(), String::new()));
        Self {
            id_local,
            fingerprint,
            encontro: carregar_encontro(),
            senha: secure::random_password(),
            estado: Arc::new(Mutex::new(EstadoSessao::default())),
            status: String::new(),
        }
    }

    /// Começa a compartilhar a tela desta máquina, numa thread.
    fn compartilhar(&mut self, ctx: &egui::Context) {
        if self.senha.trim().is_empty() {
            self.status = "Defina uma senha.".into();
            return;
        }
        let enc: SocketAddr = match self.encontro.trim().parse() {
            Ok(a) => a,
            Err(_) => {
                self.status = "Endereço de encontro inválido (use ip:porta).".into();
                return;
            }
        };
        salvar_encontro(&self.encontro);
        self.status = format!(
            "Passe estes dados a quem vai te ajudar — ID {} · senha {}",
            self.id_local,
            self.senha.trim()
        );
        let senha = self.senha.trim().to_string();
        let estado = self.estado.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let relatar = |p: pcshare_sessao::Progresso| {
                let (mensagem, ativa) = match p {
                    pcshare_sessao::Progresso::Falhou(e) => (format!("Falha: {e}"), false),
                    _ => ("Compartilhando esta tela (aguardando conexão)".into(), true),
                };
                if let Ok(mut g) = estado.lock() {
                    *g = EstadoSessao { mensagem, ativa };
                }
                ctx.request_repaint();
            };
            let _ = pcshare_sessao::hospedar_por_id(enc, &senha, relatar);
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(8.0);
            ui.heading("PCShare Agente");
            ui.label(egui::RichText::new("Permitir acesso à minha máquina").weak());
            ui.add_space(12.0);

            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("ESTA MÁQUINA").small().weak());
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.heading(&self.id_local);
                    if ui.button("Copiar").clicked() {
                        ctx.copy_text(self.id_local.chars().filter(char::is_ascii_digit).collect());
                        self.status = "ID copiado.".into();
                    }
                });
                if !self.fingerprint.is_empty() {
                    ui.label(
                        egui::RichText::new(format!("Impressão digital: {}", self.fingerprint))
                            .small()
                            .weak(),
                    );
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label("Senha:");
                    ui.add(egui::TextEdit::singleline(&mut self.senha).desired_width(100.0));
                });
            });

            ui.add_space(12.0);
            let botao = egui::Button::new(
                egui::RichText::new("Compartilhar minha tela").color(Color32::WHITE),
            )
            .fill(ACCENT);
            if ui.add_sized([ui.available_width(), 32.0], botao).clicked() {
                self.compartilhar(ctx);
            }

            ui.add_space(12.0);
            ui.collapsing("Configurações", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Servidor de encontro:");
                    ui.text_edit_singleline(&mut self.encontro);
                });
            });

            let (sess_msg, sess_ativa) = match self.estado.lock() {
                Ok(g) => (g.mensagem.clone(), g.ativa),
                Err(_) => (String::new(), false),
            };
            if !sess_msg.is_empty() || !self.status.is_empty() {
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
                if !sess_msg.is_empty() {
                    ui.horizontal(|ui| {
                        let cor = if sess_ativa {
                            Color32::from_rgb(0x3F, 0xB9, 0x50)
                        } else {
                            Color32::GRAY
                        };
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 5.0, cor);
                        ui.label(&sess_msg);
                    });
                }
                if !self.status.is_empty() {
                    ui.label(egui::RichText::new(&self.status).small().weak());
                }
            }
        });
    }
}

fn aplicar_tema(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Light);
    let mut v = egui::Visuals::light();
    v.hyperlink_color = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(v);
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style(style);
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([440.0, 460.0])
            .with_min_inner_size([380.0, 400.0])
            .with_title("PCShare Agente"),
        ..Default::default()
    };
    eframe::run_native(
        "PCShare Agente",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
