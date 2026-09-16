// No modo release, sem console: uma janela preta atrás da interface seria feia
// num produto. Em debug o console fica, para ver os prints do motor.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Interface do PCShare — a janela de conexão.
//!
//! Mostra o ID desta máquina, conecta a outra por ID+senha, e compartilha a
//! própria tela. Conectar/Compartilhar rodam o motor (`pcshare-sessao`) **numa
//! thread do mesmo processo**, que abre a janela de sessão. O vídeo continua na
//! janela Win32 do motor; esta é a janela de controle.

use eframe::egui;
use pcshare_core::secure;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Servidor de encontro padrão (o VPS do usuário). Fica editável em
/// Configurações e é gravado no primeiro uso.
const ENCONTRO_PADRAO: &str = "198.50.117.107:7100";

/// Quantas máquinas recentes lembrar.
const MAX_RECENTES: usize = 8;

fn pasta_pcshare() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("PCShare")
}

/// Lê `ui.txt`: o endereço do encontro e a lista de IDs recentes.
fn carregar_config() -> (String, Vec<String>) {
    let mut encontro = ENCONTRO_PADRAO.to_string();
    let mut recentes = Vec::new();
    if let Ok(txt) = std::fs::read_to_string(pasta_pcshare().join("ui.txt")) {
        for linha in txt.lines() {
            if let Some(v) = linha.strip_prefix("encontro=") {
                let v = v.trim();
                if !v.is_empty() {
                    encontro = v.to_string();
                }
            } else if let Some(v) = linha.strip_prefix("recentes=") {
                recentes = v
                    .split(',')
                    .map(so_digitos)
                    .filter(|s| s.len() == 9)
                    .collect();
            }
        }
    }
    (encontro, recentes)
}

fn salvar_config(encontro: &str, recentes: &[String]) {
    let dir = pasta_pcshare();
    let _ = std::fs::create_dir_all(&dir);
    let conteudo = format!("encontro={}\nrecentes={}\n", encontro.trim(), recentes.join(","));
    let _ = std::fs::write(dir.join("ui.txt"), conteudo);
}

/// Reduz o ID a nove dígitos, ignorando espaços e pontuação. O usuário
/// pode colar "737 071 101" ou "737071101" — os dois viram o mesmo.
fn so_digitos(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

/// Formata nove dígitos como "XXX XXX XXX"; deixa como está se não forem nove.
fn formatar_id(id: &str) -> String {
    let d = so_digitos(id);
    if d.len() == 9 {
        format!("{} {} {}", &d[0..3], &d[3..6], &d[6..9])
    } else {
        d
    }
}

struct App {
    id_local: String,
    fingerprint: String,
    encontro: String,
    remoto_id: String,
    senha: String,
    senha_host: String,
    recentes: Vec<String>,
    status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        aplicar_tema(&cc.egui_ctx);
        let (id_local, fingerprint) = pcshare_sessao::identidade_local()
            .unwrap_or_else(|| ("— erro ao carregar a identidade —".into(), String::new()));
        let (encontro, recentes) = carregar_config();
        Self {
            id_local,
            fingerprint,
            encontro,
            remoto_id: String::new(),
            senha: String::new(),
            // Sugere uma senha forte para o modo de compartilhar; o usuário pode
            // trocar. O host aceita `--password` explícito.
            senha_host: secure::random_password(),
            recentes,
            status: String::new(),
        }
    }

    /// Põe um ID no topo dos recentes (sem duplicar), limitando o tamanho.
    fn registrar_recente(&mut self, id: &str) {
        let id = so_digitos(id);
        if id.len() != 9 {
            return;
        }
        self.recentes.retain(|x| x != &id);
        self.recentes.insert(0, id);
        self.recentes.truncate(MAX_RECENTES);
    }

    /// Interpreta o endereço do encontro, avisando na barra de status se estiver
    /// malformado.
    fn encontro_addr(&mut self) -> Option<SocketAddr> {
        match self.encontro.trim().parse::<SocketAddr>() {
            Ok(a) => Some(a),
            Err(_) => {
                self.status =
                    "Endereço de encontro inválido (use ip:porta, ex. 198.50.117.107:7100).".into();
                None
            }
        }
    }

    /// Abre a sessão de viewer contra o ID digitado, **no mesmo processo**: uma
    /// thread roda o motor, que cria a própria janela e bloqueia até fechar.
    fn conectar(&mut self) {
        let id = so_digitos(&self.remoto_id);
        if id.len() != 9 {
            self.status = "Informe um ID de 9 dígitos.".into();
            return;
        }
        if self.senha.trim().is_empty() {
            self.status = "Informe a senha da máquina remota.".into();
            return;
        }
        let Some(enc) = self.encontro_addr() else {
            return;
        };
        self.registrar_recente(&id);
        salvar_config(&self.encontro, &self.recentes);
        let senha = self.senha.trim().to_string();
        std::thread::spawn(move || {
            if let Err(e) = pcshare_sessao::assistir_por_id(enc, &id, &senha) {
                eprintln!("sessão de viewer encerrada: {e}");
            }
        });
        self.status = format!("Abrindo a sessão com {}...", formatar_id(&self.remoto_id));
    }

    /// Compartilha a tela desta máquina, **no mesmo processo**: uma thread
    /// registra no encontro e atende sessões até o programa fechar.
    fn compartilhar(&mut self) {
        if self.senha_host.trim().is_empty() {
            self.status = "Defina uma senha para compartilhar.".into();
            return;
        }
        let Some(enc) = self.encontro_addr() else {
            return;
        };
        salvar_config(&self.encontro, &self.recentes);
        let senha = self.senha_host.trim().to_string();
        std::thread::spawn(move || {
            if let Err(e) = pcshare_sessao::hospedar_por_id(enc, &senha) {
                eprintln!("sessão de host encerrada: {e}");
            }
        });
        self.status = format!(
            "Compartilhando esta tela. Passe ao outro lado — ID {} · senha {}",
            self.id_local,
            self.senha_host.trim()
        );
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(6.0);
            ui.heading("PCShare");
            ui.label(egui::RichText::new("Acesso remoto").weak());
            ui.add_space(12.0);

            // ---- esta máquina ----
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("ESTA MÁQUINA").small().weak());
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.heading(&self.id_local);
                    if ui.button("Copiar").clicked() {
                        ctx.copy_text(so_digitos(&self.id_local));
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
                    ui.add(
                        egui::TextEdit::singleline(&mut self.senha_host)
                            .desired_width(96.0),
                    );
                    if ui.button("Compartilhar minha tela").clicked() {
                        self.compartilhar();
                    }
                });
            });

            ui.add_space(14.0);

            // ---- conectar a outra máquina ----
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("CONECTAR A OUTRA MÁQUINA").small().weak());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("ID:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.remoto_id)
                            .hint_text("737 071 101")
                            .desired_width(160.0),
                    );
                });
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label("Senha:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.senha)
                            .password(true)
                            .desired_width(160.0),
                    );
                });
                ui.add_space(8.0);
                let conectar = egui::Button::new(egui::RichText::new("Conectar").color(egui::Color32::WHITE))
                    .fill(ACCENT);
                if ui.add_sized([ui.available_width(), 30.0], conectar).clicked() {
                    self.conectar();
                }

                // ---- máquinas recentes ----
                if !self.recentes.is_empty() {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("RECENTES").small().weak());
                    ui.add_space(2.0);
                    let mut escolhido = None;
                    ui.horizontal_wrapped(|ui| {
                        for id in &self.recentes {
                            if ui.button(formatar_id(id)).clicked() {
                                escolhido = Some(id.clone());
                            }
                        }
                    });
                    if let Some(id) = escolhido {
                        self.remoto_id = formatar_id(&id);
                        self.status = "ID preenchido — informe a senha e conecte.".into();
                    }
                }
            });

            ui.add_space(12.0);
            ui.collapsing("Configurações", |ui| {
                ui.horizontal(|ui| {
                    ui.label("Servidor de encontro:");
                    ui.text_edit_singleline(&mut self.encontro);
                });
                ui.label(
                    egui::RichText::new(
                        "O servidor que casa os IDs. Só troque se você tiver o seu próprio.",
                    )
                    .small()
                    .weak(),
                );
            });

            if !self.status.is_empty() {
                ui.add_space(10.0);
                ui.separator();
                ui.label(&self.status);
            }
        });
    }
}

/// Azul de destaque, usado no botão principal e na seleção.
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x3B, 0x82, 0xF6);

/// Aplica um tema escuro consistente, independente do tema do sistema, com um
/// leve toque de destaque e um espaçamento mais folgado que o padrão.
fn aplicar_tema(ctx: &egui::Context) {
    // Fixa o tema escuro. Sem isto, o eframe segue o tema do sistema e
    // re-deriva as cores a cada quadro, desfazendo o `set_visuals` abaixo.
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut v = egui::Visuals::dark();
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style(style);
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 600.0])
            .with_min_inner_size([420.0, 520.0])
            .with_title("PCShare"),
        ..Default::default()
    };
    eframe::run_native(
        "PCShare",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
