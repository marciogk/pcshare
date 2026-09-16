//! Interface do PCShare — a janela de conexão.
//!
//! Primeira fatia do frontend: mostra o ID desta máquina, deixa conectar a
//! outra por ID+senha, e compartilhar a própria tela. Não substitui ainda a
//! janela de sessão (o vídeo continua na janela Win32 do `pcshare-spike`).
//!
//! # Ponte, por ora
//!
//! Conectar e compartilhar **lançam o `pcshare-spike.exe`** ao lado, que é o
//! motor de sessão já provado ponta a ponta. É uma ponte deliberada: troca a
//! linha de comando por uma tela sem reescrever o que funciona. O passo
//! seguinte é extrair a sessão para uma biblioteca e chamá-la no mesmo
//! processo — a lógica de cada botão fica no mesmo lugar, só muda o que ela
//! invoca.

use eframe::egui;
use pcshare_core::secure::{self, Identity};
use std::path::PathBuf;

/// Servidor de encontro padrão (o VPS do usuário). Fica editável em
/// Configurações e é gravado no primeiro uso.
const ENCONTRO_PADRAO: &str = "198.50.117.107:7100";

fn pasta_pcshare() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into())).join("PCShare")
}

/// Carrega a identidade desta máquina, criando-a no primeiro uso. **Mesmo
/// caminho do `pcshare-spike`** (`%LOCALAPPDATA%\PCShare\identity.key`), para o
/// ID exibido aqui ser o mesmo que o host anuncia.
fn carregar_identidade() -> Option<Identity> {
    let dir = pasta_pcshare();
    let path = dir.join("identity.key");
    if let Ok(bytes) = std::fs::read(&path)
        && let Ok(id) = Identity::from_private(bytes)
    {
        return Some(id);
    }
    let id = Identity::generate().ok()?;
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, id.private_key());
    Some(id)
}

/// O `pcshare-spike.exe` ao lado deste executável.
fn caminho_spike() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("pcshare-spike.exe"))
}

fn carregar_encontro() -> String {
    let path = pasta_pcshare().join("ui.txt");
    if let Ok(txt) = std::fs::read_to_string(&path) {
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

/// Reduz o ID digitado a nove dígitos, ignorando espaços e pontuação. O usuário
/// pode colar "737 071 101" ou "737071101" — os dois viram o mesmo.
fn so_digitos(s: &str) -> String {
    s.chars().filter(char::is_ascii_digit).collect()
}

struct App {
    id_local: String,
    fingerprint: String,
    encontro: String,
    remoto_id: String,
    senha: String,
    senha_host: String,
    status: String,
}

impl App {
    fn new() -> Self {
        let (id_local, fingerprint) = match carregar_identidade() {
            Some(id) => (id.display_id(), id.fingerprint()),
            None => ("— erro ao carregar a identidade —".into(), String::new()),
        };
        Self {
            id_local,
            fingerprint,
            encontro: carregar_encontro(),
            remoto_id: String::new(),
            senha: String::new(),
            // Sugere uma senha forte para o modo de compartilhar; o usuário pode
            // trocar. O host aceita `--password` explícito.
            senha_host: secure::random_password(),
            status: String::new(),
        }
    }

    /// Lança o viewer contra o ID digitado.
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
        let Some(spike) = caminho_spike() else {
            self.status = "Não encontrei o pcshare-spike.exe ao lado deste programa.".into();
            return;
        };
        salvar_encontro(&self.encontro);
        match std::process::Command::new(&spike)
            .args([
                "viewer",
                "--encontro",
                self.encontro.trim(),
                "--id",
                &id,
                "--password",
                self.senha.trim(),
            ])
            .spawn()
        {
            Ok(_) => self.status = format!("Abrindo a sessão com {}...", self.remoto_id.trim()),
            Err(e) => self.status = format!("Falha ao iniciar a sessão: {e}"),
        }
    }

    /// Lança o host, compartilhando a tela desta máquina.
    fn compartilhar(&mut self) {
        if self.senha_host.trim().is_empty() {
            self.status = "Defina uma senha para compartilhar.".into();
            return;
        }
        let Some(spike) = caminho_spike() else {
            self.status = "Não encontrei o pcshare-spike.exe ao lado deste programa.".into();
            return;
        };
        salvar_encontro(&self.encontro);
        match std::process::Command::new(&spike)
            .args([
                "host",
                "--encontro",
                self.encontro.trim(),
                "--password",
                self.senha_host.trim(),
            ])
            .spawn()
        {
            Ok(_) => {
                self.status = format!(
                    "Compartilhando esta tela. Passe ao outro lado — ID {} · senha {}",
                    self.id_local,
                    self.senha_host.trim()
                );
            }
            Err(e) => self.status = format!("Falha ao compartilhar: {e}"),
        }
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
                if ui
                    .add_sized([ui.available_width(), 28.0], egui::Button::new("Conectar"))
                    .clicked()
                {
                    self.conectar();
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
        Box::new(|_cc| Ok(Box::new(App::new()))),
    )
}
