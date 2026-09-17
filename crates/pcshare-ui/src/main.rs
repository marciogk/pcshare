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
use std::sync::{Arc, Mutex};

/// Servidor de encontro padrão (o VPS do usuário). Fica editável em
/// Configurações e é gravado no primeiro uso.
const ENCONTRO_PADRAO: &str = "198.50.117.107:7100";

/// Quantas máquinas recentes lembrar.
const MAX_RECENTES: usize = 8;

/// Uma máquina no catálogo de recentes: o ID e um apelido opcional.
#[derive(Clone)]
struct Maquina {
    id: String,
    alias: String,
}

/// Status de sessão compartilhado entre a thread do motor e a interface.
#[derive(Default)]
struct EstadoSessao {
    mensagem: String,
    ativa: bool,
}

fn pasta_pcshare() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("PCShare")
}

/// Lê `ui.txt`: o endereço do encontro e o catálogo de máquinas recentes.
/// Cada máquina é uma linha `maquina=<id>|<apelido>` (apelido opcional).
fn carregar_config() -> (String, Vec<Maquina>) {
    let mut encontro = ENCONTRO_PADRAO.to_string();
    let mut recentes = Vec::new();
    if let Ok(txt) = std::fs::read_to_string(pasta_pcshare().join("ui.txt")) {
        for linha in txt.lines() {
            if let Some(v) = linha.strip_prefix("encontro=") {
                let v = v.trim();
                if !v.is_empty() {
                    encontro = v.to_string();
                }
            } else if let Some(v) = linha.strip_prefix("maquina=") {
                let (id, alias) = match v.split_once('|') {
                    Some((i, a)) => (so_digitos(i), a.to_string()),
                    None => (so_digitos(v), String::new()),
                };
                if id.len() == 9 {
                    recentes.push(Maquina { id, alias });
                }
            }
        }
    }
    (encontro, recentes)
}

fn salvar_config(encontro: &str, recentes: &[Maquina]) {
    let dir = pasta_pcshare();
    let _ = std::fs::create_dir_all(&dir);
    let mut s = format!("encontro={}\n", encontro.trim());
    for m in recentes {
        // `|` e quebras de linha separariam os campos: troca por espaço.
        let alias = m.alias.replace(['|', '\n', '\r'], " ");
        s.push_str(&format!("maquina={}|{}\n", m.id, alias.trim()));
    }
    let _ = std::fs::write(dir.join("ui.txt"), s);
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
    recentes: Vec<Maquina>,
    /// Índice do recente em edição de apelido, se houver.
    editando: Option<usize>,
    alias_buffer: String,
    /// Status da sessão em curso, atualizado pela thread do motor.
    estado: Arc<Mutex<EstadoSessao>>,
    /// Logo carregado, se o arquivo existir.
    logo: Option<egui::TextureHandle>,
    status: String,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        aplicar_tema(&cc.egui_ctx);
        let logo = carregar_logo(&cc.egui_ctx);
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
            editando: None,
            alias_buffer: String::new(),
            estado: Arc::new(Mutex::new(EstadoSessao::default())),
            logo,
            status: String::new(),
        }
    }

    /// Põe um ID no topo dos recentes (sem duplicar), preservando o apelido que
    /// já houvesse, e limitando o tamanho da lista.
    fn registrar_recente(&mut self, id: &str) {
        let id = so_digitos(id);
        if id.len() != 9 {
            return;
        }
        let alias = self
            .recentes
            .iter()
            .find(|m| m.id == id)
            .map(|m| m.alias.clone())
            .unwrap_or_default();
        self.recentes.retain(|m| m.id != id);
        self.recentes.insert(0, Maquina { id, alias });
        self.recentes.truncate(MAX_RECENTES);
    }

    /// Nome de exibição de um ID: o apelido, se houver; senão o ID formatado.
    fn nome_de(&self, id: &str) -> String {
        let d = so_digitos(id);
        self.recentes
            .iter()
            .find(|m| m.id == d)
            .map(|m| m.alias.trim().to_string())
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| formatar_id(&d))
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
    /// thread roda o motor, que cria a própria janela e bloqueia até fechar. O
    /// motor relata o progresso, que a interface mostra ao vivo.
    fn conectar(&mut self, ctx: &egui::Context) {
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
        self.status.clear();
        let senha = self.senha.trim().to_string();
        let nome = self.nome_de(&id);
        let titulo = format!("PCShare — {nome}");
        let estado = self.estado.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let relatar = |p: pcshare_sessao::Progresso| {
                let (mensagem, ativa) = match p {
                    pcshare_sessao::Progresso::Procurando => (format!("Procurando {nome}…"), true),
                    pcshare_sessao::Progresso::EmSessao => {
                        // A janela de vídeo vai abrir e ficar na frente; some com
                        // a janela de conexão para não competir por espaço/foco.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        (format!("Em sessão com {nome}"), true)
                    }
                    pcshare_sessao::Progresso::Encerrou => {
                        // Sessão fechou: traz a janela de conexão de volta.
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                        (format!("Sessão com {nome} encerrada"), false)
                    }
                    pcshare_sessao::Progresso::Falhou(e) => {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                        (format!("Falha: {e}"), false)
                    }
                };
                if let Ok(mut g) = estado.lock() {
                    *g = EstadoSessao { mensagem, ativa };
                }
                ctx.request_repaint();
            };
            let _ = pcshare_sessao::assistir_por_id(enc, &id, &senha, &titulo, relatar);
        });
    }

    /// Compartilha a tela desta máquina, **no mesmo processo**: uma thread
    /// registra no encontro e atende sessões até o programa fechar.
    fn compartilhar(&mut self, ctx: &egui::Context) {
        if self.senha_host.trim().is_empty() {
            self.status = "Defina uma senha para compartilhar.".into();
            return;
        }
        let Some(enc) = self.encontro_addr() else {
            return;
        };
        salvar_config(&self.encontro, &self.recentes);
        self.status = format!(
            "Passe ao outro lado — ID {} · senha {}",
            self.id_local,
            self.senha_host.trim()
        );
        let senha = self.senha_host.trim().to_string();
        let estado = self.estado.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let relatar = |p: pcshare_sessao::Progresso| {
                let (mensagem, ativa) = match p {
                    pcshare_sessao::Progresso::Falhou(e) => (format!("Falha: {e}"), false),
                    _ => ("Compartilhando esta tela (aguardando conexões)".into(), true),
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
            // Logo, se o arquivo existir; senão, o título em texto.
            if let Some(tex) = &self.logo {
                let largura = ui.available_width().min(260.0);
                let size = tex.size_vec2();
                let escala = (largura / size.x).min(1.0);
                ui.vertical_centered(|ui| {
                    ui.image(egui::load::SizedTexture::new(tex.id(), size * escala));
                });
            } else {
                ui.heading("PCShare");
                ui.label(egui::RichText::new("Acesso remoto").weak());
            }
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
                        self.compartilhar(ctx);
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
                    self.conectar(ctx);
                }

                // ---- máquinas recentes ----
                if !self.recentes.is_empty() {
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("RECENTES").small().weak());
                    ui.add_space(2.0);
                    // Itera sobre uma cópia para poder mutar self.* dentro do laço
                    // (preencher o ID, abrir/fechar a edição de apelido).
                    let recentes = self.recentes.clone();
                    let mut preencher: Option<String> = None;
                    for (i, m) in recentes.iter().enumerate() {
                        ui.horizontal(|ui| {
                            let rotulo = if m.alias.trim().is_empty() {
                                formatar_id(&m.id)
                            } else {
                                m.alias.clone()
                            };
                            if ui.button(rotulo).clicked() {
                                preencher = Some(m.id.clone());
                            }
                            if !m.alias.trim().is_empty() {
                                ui.label(egui::RichText::new(formatar_id(&m.id)).small().weak());
                            }
                            let editando = self.editando == Some(i);
                            if ui.selectable_label(editando, "renomear").clicked() {
                                if editando {
                                    self.editando = None;
                                } else {
                                    self.editando = Some(i);
                                    self.alias_buffer = m.alias.clone();
                                }
                            }
                        });
                        if self.editando == Some(i) {
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.alias_buffer)
                                        .hint_text("apelido")
                                        .desired_width(150.0),
                                );
                                if ui.button("Salvar").clicked() {
                                    let alias = self.alias_buffer.trim().to_string();
                                    if let Some(r) = self.recentes.iter_mut().find(|r| r.id == m.id) {
                                        r.alias = alias;
                                    }
                                    self.editando = None;
                                    salvar_config(&self.encontro, &self.recentes);
                                }
                            });
                        }
                    }
                    if let Some(id) = preencher {
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

            // ---- status da sessão (ao vivo) e mensagens do formulário ----
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
                            egui::Color32::from_rgb(0x3F, 0xB9, 0x50) // verde: em sessão
                        } else {
                            egui::Color32::GRAY // encerrada/falhou
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

use egui::Color32;

/// Azul do botão principal e da seleção.
const ACCENT: Color32 = Color32::from_rgb(0x2E, 0x6F, 0xD6);

/// Tema claro (como no início), fixo. O logo do PCShare tem letras escuras, que
/// ficam legíveis sobre o fundo claro — o fundo azul as apagava.
fn aplicar_tema(ctx: &egui::Context) {
    // Fixa o tema claro, independente do tema do sistema.
    ctx.set_theme(egui::ThemePreference::Light);
    let mut v = egui::Visuals::light();
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT.gamma_multiply(0.3);
    v.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    ctx.set_visuals(v);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    ctx.set_style(style);
}

/// Carrega o logo de um arquivo, se existir. Procura ao lado do executável e em
/// `%LOCALAPPDATA%\PCShare\`, aceitando `logo.png` ou `logo.jpg`. Devolve `None`
/// sem erro se não achar — a interface cai no título em texto.
fn carregar_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let mut candidatos = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        candidatos.push(dir.join("logo.png"));
        candidatos.push(dir.join("logo.jpg"));
    }
    candidatos.push(pasta_pcshare().join("logo.png"));
    candidatos.push(pasta_pcshare().join("logo.jpg"));

    for caminho in candidatos {
        let Ok(bytes) = std::fs::read(&caminho) else {
            continue;
        };
        let Ok(img) = image::load_from_memory(&bytes) else {
            continue;
        };
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        let color = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
        return Some(ctx.load_texture("logo", color, egui::TextureOptions::LINEAR));
    }
    None
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
