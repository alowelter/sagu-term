//! Aplicacao eframe/egui: portao do cofre, gestao de hosts e sessao de terminal.

use std::path::PathBuf;
use std::time::Instant;

use crate::pty;
use crate::ssh::{self, SshHandle, SshToUi};
use crate::terminal::Terminal;
use crate::vault::{self, AuthMethod, Host, Vault};

const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const SPLASH_SECS: f32 = 2.2;

/// Tema escuro com acentos roxos, em contraste com o logo da aplicacao.
fn apply_dark_theme(ctx: &egui::Context) {
    use egui::{Color32, Stroke};

    let bg_extreme = Color32::from_rgb(0x0c, 0x09, 0x12);
    let bg_panel = Color32::from_rgb(0x15, 0x11, 0x1e);
    let bg_widget = Color32::from_rgb(0x20, 0x1a, 0x2c);
    let bg_hover = Color32::from_rgb(0x32, 0x27, 0x46);
    let accent = Color32::from_rgb(0x8b, 0x5c, 0xf6);
    let accent_dim = Color32::from_rgb(0x5f, 0x2c, 0x91);
    let text = Color32::from_rgb(0xe7, 0xe1, 0xf2);
    let text_weak = Color32::from_rgb(0xb3, 0xa8, 0xc6);

    let mut v = egui::Visuals::dark();
    v.panel_fill = bg_panel;
    v.window_fill = bg_widget;
    v.extreme_bg_color = bg_extreme;
    v.faint_bg_color = Color32::from_rgb(0x1b, 0x15, 0x26);
    v.code_bg_color = bg_extreme;
    v.override_text_color = Some(text);
    v.hyperlink_color = accent;
    v.window_stroke = Stroke::new(1.0, Color32::from_rgb(0x2c, 0x22, 0x3c));

    v.selection.bg_fill = accent_dim;
    v.selection.stroke = Stroke::new(1.0, accent);

    v.widgets.noninteractive.bg_fill = bg_panel;
    v.widgets.noninteractive.weak_bg_fill = bg_panel;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text_weak);

    v.widgets.inactive.bg_fill = bg_widget;
    v.widgets.inactive.weak_bg_fill = bg_widget;
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, text);

    v.widgets.hovered.bg_fill = bg_hover;
    v.widgets.hovered.weak_bg_fill = bg_hover;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent);

    v.widgets.active.bg_fill = accent_dim;
    v.widgets.active.weak_bg_fill = accent_dim;
    v.widgets.active.fg_stroke = Stroke::new(1.0, Color32::WHITE);
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent);

    v.widgets.open.bg_fill = bg_widget;
    v.widgets.open.weak_bg_fill = bg_widget;

    ctx.set_visuals(v);
}

/// Resolve o caminho de um arquivo da pasta `assets/`, procurando ao lado do
/// executavel, no diretorio do projeto e no diretorio atual.
pub fn asset_path(name: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("assets").join(name));
        }
    }
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets").join(name));
    candidates.push(PathBuf::from("assets").join(name));
    candidates.into_iter().find(|p| p.exists())
}

#[derive(PartialEq)]
enum GateMode {
    Open,
    Create,
}

enum Screen {
    Splash,
    Gate,
    Hosts,
    Session,
}

enum SessionState {
    Connecting,
    Connected,
    Closed,
    Error(String),
}

/// Um painel da sessao: ou uma conexao SSH ativa, ou um seletor de host
/// (quando `picking` e verdadeiro) aguardando o usuario escolher uma conexao.
struct Pane {
    ssh: Option<SshHandle>,
    terminal: Option<Terminal>,
    state: SessionState,
    host_name: String,
    picking: bool,
    /// Marcado quando a sessao encerra normalmente (ex.: `exit`); leva ao
    /// fechamento automatico do painel no proximo quadro.
    should_close: bool,
}

impl Pane {
    /// Painel vazio que mostra a lista de hosts para escolher uma conexao.
    fn picker() -> Self {
        Pane {
            ssh: None,
            terminal: None,
            state: SessionState::Closed,
            host_name: String::new(),
            picking: true,
            should_close: false,
        }
    }
}

/// Direcao de uma divisao da tela.
#[derive(Clone, Copy, PartialEq)]
enum SplitDir {
    /// Paineis lado a lado (linha divisoria vertical).
    SideBySide,
    /// Paineis empilhados (linha divisoria horizontal).
    Stacked,
}

/// Arvore de layout: cada folha e um painel; cada divisao tem filhos de
/// tamanho igual numa direcao. Permite dividir qualquer painel recursivamente.
enum Node {
    Leaf(Pane),
    Split { dir: SplitDir, children: Vec<Node> },
}

/// Acao estrutural coletada durante a renderizacao (aplicada depois para
/// evitar conflitos de emprestimo sobre a arvore).
enum PaneAction {
    Split { path: Vec<usize>, dir: SplitDir },
    Close { path: Vec<usize> },
    Connect { path: Vec<usize>, host: usize },
    OpenCmd { path: Vec<usize> },
}

/// Caminho ate uma folha: indices de filho da raiz ate o no.
fn node_at_mut<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut cur = root;
    for &i in path {
        match cur {
            Node::Split { children, .. } => cur = children.get_mut(i)?,
            Node::Leaf(_) => return None,
        }
    }
    Some(cur)
}

const ICON_SPLIT_SIDE: egui::ImageSource =
    egui::include_image!("../assets/square-split-horizontal.svg");
const ICON_SPLIT_STACK: egui::ImageSource =
    egui::include_image!("../assets/square-split-vertical.svg");
const ICON_CLOSE: egui::ImageSource = egui::include_image!("../assets/x.svg");
const ICON_SERVER: egui::ImageSource = egui::include_image!("../assets/server.svg");
const ICON_TERMINAL: egui::ImageSource = egui::include_image!("../assets/square-terminal.svg");

/// Cor dos icones do cabecalho (igual a do tema fraco).
const ICON_TINT: egui::Color32 = egui::Color32::from_rgb(0xb3, 0xa8, 0xc6);
/// Cor da borda dos paineis (tambem usada no apelido da sessao).
const PANE_BORDER: egui::Color32 = egui::Color32::from_rgb(0x3a, 0x2e, 0x52);
/// Fundo da barra de titulo de cada painel.
const TITLE_BG: egui::Color32 = egui::Color32::from_rgb(0x2a, 0x29, 0x25);
/// Fundo da tela de abertura do cofre.
const GATE_BG: egui::Color32 = egui::Color32::from_rgb(0x51, 0x4d, 0x34);

/// Formulario de edicao/criacao de host.
struct HostEditor {
    index: Option<usize>,
    name: String,
    host: String,
    port: u16,
    username: String,
    use_key: bool,
    password: String,
    private_key: String,
    passphrase: String,
}

impl HostEditor {
    fn new() -> Self {
        HostEditor {
            index: None,
            name: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            use_key: false,
            password: String::new(),
            private_key: String::new(),
            passphrase: String::new(),
        }
    }

    fn from_host(index: usize, h: &Host) -> Self {
        let (use_key, password, private_key, passphrase) = match &h.auth {
            AuthMethod::Password { password } => (false, password.clone(), String::new(), String::new()),
            AuthMethod::Key {
                private_key,
                passphrase,
            } => (
                true,
                String::new(),
                private_key.clone(),
                passphrase.clone().unwrap_or_default(),
            ),
        };
        HostEditor {
            index: Some(index),
            name: h.name.clone(),
            host: h.host.clone(),
            port: h.port,
            username: h.username.clone(),
            use_key,
            password,
            private_key,
            passphrase,
        }
    }

    fn to_host(&self, id: uuid::Uuid) -> Host {
        let auth = if self.use_key {
            AuthMethod::Key {
                private_key: self.private_key.clone(),
                passphrase: if self.passphrase.is_empty() {
                    None
                } else {
                    Some(self.passphrase.clone())
                },
            }
        } else {
            AuthMethod::Password {
                password: self.password.clone(),
            }
        };
        Host {
            id,
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port: self.port,
            username: self.username.trim().to_string(),
            auth,
        }
    }
}

pub struct App {
    screen: Screen,

    // Cofre
    vault: Vault,
    vault_path: Option<PathBuf>,
    master_password: String,

    // Portao
    gate_mode: GateMode,
    gate_path: String,
    gate_password: String,
    gate_password_confirm: String,
    gate_error: Option<String>,

    // Editor de host
    editor: Option<HostEditor>,
    hosts_error: Option<String>,

    // Sessao: arvore de paineis (divisoes recursivas).
    root: Option<Node>,

    // Guardado para o closure de repaint da sessao.
    ctx_for_repaint: Option<egui::Context>,

    // Logo (assets/logo.png) carregada sob demanda.
    logo_texture: Option<egui::TextureHandle>,
    logo_load_attempted: bool,

    // Splash screen.
    splash_start: Instant,

    // Caminho do ultimo cofre aberto (persistido entre execucoes).
    last_vault_path: String,

    // Pede foco no campo de senha ao abrir a janela do cofre.
    gate_focus_requested: bool,
}

const STORAGE_LAST_PATH: &str = "last_vault_path";

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        apply_dark_theme(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let last_vault_path = cc
            .storage
            .and_then(|s| s.get_string(STORAGE_LAST_PATH))
            .unwrap_or_default();
        App {
            screen: Screen::Splash,
            vault: Vault::default(),
            vault_path: None,
            master_password: String::new(),
            gate_mode: GateMode::Open,
            gate_path: last_vault_path.clone(),
            gate_password: String::new(),
            gate_password_confirm: String::new(),
            gate_error: None,
            editor: None,
            hosts_error: None,
            root: None,
            ctx_for_repaint: None,
            logo_texture: None,
            logo_load_attempted: false,
            splash_start: Instant::now(),
            last_vault_path,
            gate_focus_requested: true,
        }
    }

    fn ui_splash(&mut self, ui: &mut egui::Ui) {
        self.ensure_logo(ui.ctx());
        let elapsed = self.splash_start.elapsed().as_secs_f32();
        let fade = (elapsed / 0.7).clamp(0.0, 1.0);
        let alpha = (fade * 255.0) as u8;

        let avail = ui.available_size();
        ui.vertical_centered(|ui| {
            ui.add_space(avail.y * 0.26);
            if let Some(tex) = &self.logo_texture {
                let size = tex.size_vec2();
                let scale = (340.0 / size.x).min(1.0);
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                        .fit_to_exact_size(size * scale)
                        .tint(egui::Color32::from_white_alpha(alpha)),
                );
            } else {
                ui.heading(egui::RichText::new("SaguTerm").size(40.0));
            }
            ui.add_space(28.0);
            ui.add(egui::Spinner::new().size(22.0));
        });
    }

    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo_load_attempted {
            return;
        }
        self.logo_load_attempted = true;
        let Some(path) = asset_path("logo.png") else {
            return;
        };
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        if let Ok(img) = image::load_from_memory(&bytes) {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            let color = egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                rgba.as_raw(),
            );
            self.logo_texture =
                Some(ctx.load_texture("sagu_logo", color, egui::TextureOptions::LINEAR));
        }
    }

    fn save_vault(&mut self) -> anyhow::Result<()> {
        let path = self
            .vault_path
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nenhum cofre aberto"))?;
        let bytes = vault::encrypt_vault(&self.vault, &self.master_password)?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    fn lock(&mut self) {
        if let Some(root) = &self.root {
            disconnect_tree(root);
        }
        self.root = None;
        self.vault = Vault::default();
        self.master_password.clear();
        self.gate_password.clear();
        self.gate_password_confirm.clear();
        self.editor = None;
        self.gate_focus_requested = true;
        self.screen = Screen::Gate;
    }

    // ---------------- Portao do cofre ----------------

    fn ui_gate(&mut self, ui: &mut egui::Ui) {
        self.ensure_logo(ui.ctx());

        // Fundo: logo discreto centralizado atras da janela flutuante.
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.16);
            if let Some(tex) = &self.logo_texture {
                let size = tex.size_vec2();
                let scale = (200.0 / size.x).min(1.0);
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                        .fit_to_exact_size(size * scale)
                        .tint(egui::Color32::from_white_alpha(48)),
                );
            }
        });

        let mut submit = false;
        egui::Window::new("Cofre")
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(520.0)
            .show(ui.ctx(), |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.gate_mode, GateMode::Open, "Abrir cofre");
                    ui.selectable_value(&mut self.gate_mode, GateMode::Create, "Criar cofre");
                });
                ui.separator();

                ui.horizontal(|ui| {
                    ui.label("Arquivo:");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.gate_path)
                            .desired_width(330.0)
                            .hint_text("caminho do arquivo .sagu"),
                    );
                    let btn = if self.gate_mode == GateMode::Open {
                        "Procurar..."
                    } else {
                        "Salvar como..."
                    };
                    if ui.button(btn).clicked() {
                        self.pick_vault_path();
                    }
                });

                let mut pwd_enter = false;
                ui.horizontal(|ui| {
                    ui.label("Senha mestra:");
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.gate_password)
                            .password(true)
                            .desired_width(300.0),
                    );
                    if self.gate_focus_requested {
                        resp.request_focus();
                        self.gate_focus_requested = false;
                    }
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        pwd_enter = true;
                    }
                });

                if self.gate_mode == GateMode::Create {
                    ui.horizontal(|ui| {
                        ui.label("Confirmar:");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.gate_password_confirm)
                                .password(true)
                                .desired_width(300.0),
                        );
                        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            pwd_enter = true;
                        }
                    });
                }

                ui.add_space(8.0);
                let action_label = if self.gate_mode == GateMode::Open {
                    "Abrir"
                } else {
                    "Criar"
                };
                if ui.button(action_label).clicked() || pwd_enter {
                    submit = true;
                }

                if let Some(err) = &self.gate_error {
                    ui.add_space(8.0);
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), err);
                }
            });

        if submit {
            self.gate_submit();
        }
    }

    fn pick_vault_path(&mut self) {
        let dialog = rfd::FileDialog::new()
            .add_filter("Cofre SaguTerm", &["sagu"])
            .add_filter("Todos os arquivos", &["*"]);
        let chosen = if self.gate_mode == GateMode::Open {
            dialog.pick_file()
        } else {
            dialog.set_file_name("cofre.sagu").save_file()
        };
        if let Some(path) = chosen {
            self.gate_path = path.to_string_lossy().to_string();
        }
    }

    fn gate_submit(&mut self) {
        self.gate_error = None;
        if self.gate_path.trim().is_empty() {
            self.gate_error = Some("Informe o caminho do arquivo.".into());
            return;
        }
        if self.gate_password.is_empty() {
            self.gate_error = Some("Informe a senha mestra.".into());
            return;
        }
        let path = PathBuf::from(self.gate_path.trim());
        let path_str = self.gate_path.trim().to_string();

        match self.gate_mode {
            GateMode::Open => match std::fs::read(&path) {
                Ok(bytes) => match vault::decrypt_vault(&bytes, &self.gate_password) {
                    Ok(v) => {
                        self.vault = v;
                        self.vault_path = Some(path);
                        self.master_password = std::mem::take(&mut self.gate_password);
                        self.gate_password_confirm.clear();
                        self.last_vault_path = path_str.clone();
                        self.screen = Screen::Hosts;
                    }
                    Err(e) => self.gate_error = Some(format!("{e}")),
                },
                Err(e) => self.gate_error = Some(format!("Nao foi possivel ler o arquivo: {e}")),
            },
            GateMode::Create => {
                if self.gate_password != self.gate_password_confirm {
                    self.gate_error = Some("As senhas nao conferem.".into());
                    return;
                }
                let v = Vault::default();
                match vault::encrypt_vault(&v, &self.gate_password) {
                    Ok(bytes) => match std::fs::write(&path, bytes) {
                        Ok(()) => {
                            self.vault = v;
                            self.vault_path = Some(path);
                            self.master_password = std::mem::take(&mut self.gate_password);
                            self.gate_password_confirm.clear();
                            self.last_vault_path = path_str.clone();
                            self.screen = Screen::Hosts;
                        }
                        Err(e) => {
                            self.gate_error = Some(format!("Nao foi possivel gravar: {e}"))
                        }
                    },
                    Err(e) => self.gate_error = Some(format!("{e}")),
                }
            }
        }
    }

    // ---------------- Lista de hosts ----------------

    fn ui_hosts(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Hosts");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Bloquear cofre").clicked() {
                    self.lock();
                }
                if ui.button("+ Novo host").clicked() {
                    self.editor = Some(HostEditor::new());
                }
            });
        });
        if let Some(path) = &self.vault_path {
            ui.label(
                egui::RichText::new(path.to_string_lossy())
                    .small()
                    .weak(),
            );
        }
        ui.separator();

        if let Some(err) = &self.hosts_error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), err);
        }

        if self.editor.is_some() {
            self.ui_host_editor(ui);
            return;
        }

        let mut connect_index: Option<usize> = None;
        let mut edit_index: Option<usize> = None;
        let mut delete_index: Option<usize> = None;
        let mut open_cmd = false;

        egui::ScrollArea::vertical().show(ui, |ui| {
            // Terminal local do Windows no inicio da listagem.
            egui::Frame::group(ui.style())
                .inner_margin(10.0)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Image::new(ICON_TERMINAL)
                                .fit_to_exact_size(egui::vec2(22.0, 22.0))
                                .tint(ICON_TINT),
                        );
                        ui.vertical(|ui| {
                            ui.strong("Terminal local");
                            ui.label(
                                egui::RichText::new("Prompt de comando do Windows")
                                    .small()
                                    .weak(),
                            );
                        });
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.button("Abrir").clicked() {
                                    open_cmd = true;
                                }
                            },
                        );
                    });
                });

            if self.vault.hosts.is_empty() {
                ui.add_space(8.0);
                ui.weak("Nenhum host cadastrado. Clique em \"+ Novo host\".");
            }

            for (i, host) in self.vault.hosts.iter().enumerate() {
                egui::Frame::group(ui.style())
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::Image::new(ICON_SERVER)
                                    .fit_to_exact_size(egui::vec2(22.0, 22.0))
                                    .tint(ICON_TINT),
                            );
                            ui.vertical(|ui| {
                                let title = if host.name.trim().is_empty() {
                                    host.host.clone()
                                } else {
                                    host.name.clone()
                                };
                                ui.strong(title);
                                let auth = match &host.auth {
                                    AuthMethod::Password { .. } => "senha",
                                    AuthMethod::Key { .. } => "chave",
                                };
                                ui.label(
                                    egui::RichText::new(format!(
                                        "{}@{}:{}  ({})",
                                        host.username, host.host, host.port, auth
                                    ))
                                    .small()
                                    .weak(),
                                );
                            });
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.button("Excluir").clicked() {
                                        delete_index = Some(i);
                                    }
                                    if ui.button("Editar").clicked() {
                                        edit_index = Some(i);
                                    }
                                    if ui.button("Conectar").clicked() {
                                        connect_index = Some(i);
                                    }
                                },
                            );
                        });
                    });
            }
        });

        if open_cmd {
            self.start_cmd_session();
        }
        if let Some(i) = edit_index {
            self.editor = Some(HostEditor::from_host(i, &self.vault.hosts[i]));
        }
        if let Some(i) = delete_index {
            self.vault.hosts.remove(i);
            if let Err(e) = self.save_vault() {
                self.hosts_error = Some(format!("Erro ao salvar: {e}"));
            }
        }
        if let Some(i) = connect_index {
            self.start_session(i);
        }
    }

    fn ui_host_editor(&mut self, ui: &mut egui::Ui) {
        // Trabalha sobre uma copia para nao colidir com o emprestimo de self.
        let mut editor = self.editor.take().unwrap();
        let mut save = false;
        let mut cancel = false;
        let mut load_key = false;

        egui::Frame::group(ui.style())
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.heading(if editor.index.is_some() {
                    "Editar host"
                } else {
                    "Novo host"
                });
                ui.add_space(8.0);

                egui::Grid::new("host_editor_grid")
                    .num_columns(2)
                    .spacing([12.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Nome:");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.name)
                                .desired_width(320.0)
                                .hint_text("apelido (opcional)"),
                        );
                        ui.end_row();

                        ui.label("Host:");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.host)
                                .desired_width(320.0)
                                .hint_text("endereco ou IP"),
                        );
                        ui.end_row();

                        ui.label("Porta:");
                        ui.add(egui::DragValue::new(&mut editor.port).range(1..=65535));
                        ui.end_row();

                        ui.label("Usuario:");
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.username).desired_width(320.0),
                        );
                        ui.end_row();

                        ui.label("Autenticacao:");
                        ui.horizontal(|ui| {
                            ui.selectable_value(&mut editor.use_key, false, "Senha");
                            ui.selectable_value(&mut editor.use_key, true, "Chave");
                        });
                        ui.end_row();

                        if editor.use_key {
                            ui.label("Chave privada:");
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut editor.private_key)
                                        .desired_width(320.0)
                                        .desired_rows(5)
                                        .hint_text("-----BEGIN OPENSSH PRIVATE KEY-----"),
                                );
                                if ui.button("Carregar de arquivo...").clicked() {
                                    load_key = true;
                                }
                            });
                            ui.end_row();

                            ui.label("Passphrase:");
                            ui.add(
                                egui::TextEdit::singleline(&mut editor.passphrase)
                                    .password(true)
                                    .desired_width(320.0)
                                    .hint_text("opcional"),
                            );
                            ui.end_row();
                        } else {
                            ui.label("Senha:");
                            ui.add(
                                egui::TextEdit::singleline(&mut editor.password)
                                    .password(true)
                                    .desired_width(320.0),
                            );
                            ui.end_row();
                        }
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Salvar").clicked() {
                        save = true;
                    }
                    if ui.button("Cancelar").clicked() {
                        cancel = true;
                    }
                });
            });

        if load_key {
            if let Some(path) = rfd::FileDialog::new().pick_file() {
                match std::fs::read_to_string(&path) {
                    Ok(content) => editor.private_key = content,
                    Err(e) => self.hosts_error = Some(format!("Erro ao ler chave: {e}")),
                }
            }
        }

        if cancel {
            self.editor = None;
            return;
        }

        if save {
            if editor.host.trim().is_empty() || editor.username.trim().is_empty() {
                self.hosts_error = Some("Host e usuario sao obrigatorios.".into());
                self.editor = Some(editor);
                return;
            }
            match editor.index {
                Some(i) => {
                    let id = self.vault.hosts[i].id;
                    self.vault.hosts[i] = editor.to_host(id);
                }
                None => {
                    self.vault.hosts.push(editor.to_host(uuid::Uuid::new_v4()));
                }
            }
            self.editor = None;
            self.hosts_error = None;
            if let Err(e) = self.save_vault() {
                self.hosts_error = Some(format!("Erro ao salvar: {e}"));
            }
        } else {
            self.editor = Some(editor);
        }
    }

    // ---------------- Sessao SSH ----------------

    fn start_session(&mut self, index: usize) {
        self.root = Some(Node::Leaf(Pane::picker()));
        self.screen = Screen::Session;
        self.connect_pane(&[], index);
    }

    /// Abre uma nova sessao com um terminal local (cmd.exe).
    fn start_cmd_session(&mut self) {
        self.root = Some(Node::Leaf(Pane::picker()));
        self.screen = Screen::Session;
        self.connect_cmd_pane(&[]);
    }

    /// Conecta o painel (folha) no caminho indicado a um terminal local cmd.exe.
    fn connect_cmd_pane(&mut self, path: &[usize]) {
        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = pty::connect_cmd(INITIAL_COLS, INITIAL_ROWS, repaint);

        if let Some(root) = &mut self.root {
            if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                pane.host_name = "Terminal Local".to_string();
                pane.terminal = Some(Terminal::new(INITIAL_COLS, INITIAL_ROWS));
                pane.state = SessionState::Connecting;
                pane.ssh = Some(handle);
                pane.picking = false;
            }
        }
    }

    /// Conecta o painel (folha) no caminho indicado ao host escolhido.
    fn connect_pane(&mut self, path: &[usize], host_index: usize) {
        let host = self.vault.hosts[host_index].clone();
        let name = if host.name.trim().is_empty() {
            host.host.clone()
        } else {
            host.name.clone()
        };

        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = ssh::connect(host, INITIAL_COLS, INITIAL_ROWS, repaint);

        if let Some(root) = &mut self.root {
            if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                pane.host_name = name;
                pane.terminal = Some(Terminal::new(INITIAL_COLS, INITIAL_ROWS));
                pane.state = SessionState::Connecting;
                pane.ssh = Some(handle);
                pane.picking = false;
            }
        }
    }

    /// Divide o painel no caminho: adiciona um irmao se o pai ja tem a mesma
    /// direcao, caso contrario transforma a folha numa nova divisao.
    fn split_pane(&mut self, path: &[usize], dir: SplitDir) {
        let Some(root) = &mut self.root else {
            return;
        };
        if let Some((&idx, parent_path)) = path.split_last() {
            if let Some(Node::Split {
                dir: pdir,
                children,
            }) = node_at_mut(root, parent_path)
            {
                if *pdir == dir {
                    children.insert(idx + 1, Node::Leaf(Pane::picker()));
                    return;
                }
            }
        }
        if let Some(node) = node_at_mut(root, path) {
            if matches!(node, Node::Leaf(_)) {
                let old = std::mem::replace(
                    node,
                    Node::Split {
                        dir,
                        children: Vec::new(),
                    },
                );
                if let Node::Split { children, .. } = node {
                    children.push(old);
                    children.push(Node::Leaf(Pane::picker()));
                }
            }
        }
    }

    /// Fecha o painel no caminho; colapsa a divisao se sobrar um filho, ou
    /// retorna a lista de hosts se nao restar nenhum painel.
    fn close_pane(&mut self, path: &[usize]) {
        if let Some(root) = &mut self.root {
            if let Some(node) = node_at_mut(root, path) {
                disconnect_tree(node);
            }
        }
        if path.is_empty() {
            self.root = None;
            self.screen = Screen::Hosts;
            return;
        }
        let Some(root) = &mut self.root else {
            return;
        };
        let (&idx, parent_path) = path.split_last().unwrap();
        let mut collapse: Option<Node> = None;
        if let Some(Node::Split { children, .. }) = node_at_mut(root, parent_path) {
            if idx < children.len() {
                children.remove(idx);
            }
            if children.len() == 1 {
                collapse = Some(children.pop().unwrap());
            }
        }
        if let Some(only) = collapse {
            if let Some(parent) = node_at_mut(root, parent_path) {
                *parent = only;
            }
        }
    }

    fn drain_ssh_events(&mut self) {
        {
            let Some(root) = &mut self.root else {
                return;
            };
            for_each_pane_mut(root, &mut |pane| {
                let mut events = Vec::new();
                if let Some(ssh) = &pane.ssh {
                    while let Ok(ev) = ssh.from_ssh.try_recv() {
                        events.push(ev);
                    }
                }
                for ev in events {
                    match ev {
                        SshToUi::Connected => pane.state = SessionState::Connected,
                        SshToUi::Data(bytes) => {
                            if let Some(term) = &mut pane.terminal {
                                term.process(&bytes);
                            }
                        }
                        SshToUi::Error(msg) => pane.state = SessionState::Error(msg),
                        SshToUi::Closed => {
                            // Encerramento normal (ex.: `exit`): fecha o painel.
                            if matches!(pane.state, SessionState::Error(_)) {
                                // Mantem o painel para o usuario ler o erro.
                            } else {
                                pane.state = SessionState::Closed;
                                pane.should_close = true;
                            }
                        }
                    }
                }
            });
        }

        // Fecha os paineis cuja sessao encerrou (um por vez, recalculando o
        // caminho para acompanhar o colapso da arvore).
        loop {
            let target = self.root.as_ref().and_then(|r| {
                let mut p = Vec::new();
                first_closeable(r, &mut p)
            });
            match target {
                Some(path) => self.close_pane(&path),
                None => break,
            }
            if self.root.is_none() {
                break;
            }
        }
    }

    /// Renderiza a arvore de paineis na area disponivel e aplica as acoes
    /// estruturais coletadas (dividir/fechar/conectar).
    fn ui_session(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let mut actions: Vec<PaneAction> = Vec::new();
        let hosts = &self.vault.hosts;
        if let Some(root) = &mut self.root {
            let mut path = Vec::new();
            render_node(ui, rect, root, &mut path, hosts, &mut actions);
        }
        for action in actions {
            match action {
                PaneAction::Split { path, dir } => self.split_pane(&path, dir),
                PaneAction::Close { path } => self.close_pane(&path),
                PaneAction::Connect { path, host } => self.connect_pane(&path, host),
                PaneAction::OpenCmd { path } => self.connect_cmd_pane(&path),
            }
        }
    }
}

/// Desconecta recursivamente todas as sessoes SSH de uma subarvore.
fn disconnect_tree(node: &Node) {
    match node {
        Node::Leaf(pane) => {
            if let Some(ssh) = &pane.ssh {
                ssh.disconnect();
            }
        }
        Node::Split { children, .. } => {
            for c in children {
                disconnect_tree(c);
            }
        }
    }
}

/// Retorna o caminho da primeira folha marcada para fechamento, se houver.
fn first_closeable(node: &Node, path: &mut Vec<usize>) -> Option<Vec<usize>> {
    match node {
        Node::Leaf(pane) => {
            if pane.should_close {
                Some(path.clone())
            } else {
                None
            }
        }
        Node::Split { children, .. } => {
            for (i, c) in children.iter().enumerate() {
                path.push(i);
                if let Some(found) = first_closeable(c, path) {
                    return Some(found);
                }
                path.pop();
            }
            None
        }
    }
}

/// Aplica `f` a cada folha (painel) da arvore.
fn for_each_pane_mut(node: &mut Node, f: &mut impl FnMut(&mut Pane)) {
    match node {
        Node::Leaf(pane) => f(pane),
        Node::Split { children, .. } => {
            for c in children {
                for_each_pane_mut(c, f);
            }
        }
    }
}

/// Renderiza um no da arvore dentro de `rect`. Para divisoes, subdivide o
/// retangulo em partes iguais e recorre; para folhas, desenha cabecalho,
/// conteudo (terminal ou seletor) e a borda do painel.
fn render_node(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &mut Node,
    path: &mut Vec<usize>,
    hosts: &[Host],
    actions: &mut Vec<PaneAction>,
) {
    match node {
        Node::Split { dir, children } => {
            let n = children.len();
            if n == 0 {
                return;
            }
            let dir = *dir;
            let gap = 2.0;
            for i in 0..n {
                let child_rect = match dir {
                    SplitDir::SideBySide => {
                        let each = (rect.width() - gap * (n as f32 - 1.0)) / n as f32;
                        let x0 = rect.min.x + i as f32 * (each + gap);
                        egui::Rect::from_min_size(
                            egui::pos2(x0, rect.min.y),
                            egui::vec2(each, rect.height()),
                        )
                    }
                    SplitDir::Stacked => {
                        let each = (rect.height() - gap * (n as f32 - 1.0)) / n as f32;
                        let y0 = rect.min.y + i as f32 * (each + gap);
                        egui::Rect::from_min_size(
                            egui::pos2(rect.min.x, y0),
                            egui::vec2(rect.width(), each),
                        )
                    }
                };
                path.push(i);
                let child = &mut children[i];
                ui.scope_builder(egui::UiBuilder::new().max_rect(child_rect), |ui| {
                    render_node(ui, child_rect, child, path, hosts, actions);
                });
                path.pop();
            }
        }
        Node::Leaf(pane) => {
            let status = match &pane.state {
                SessionState::Connecting => "conectando...",
                SessionState::Connected => "conectado",
                SessionState::Closed => "sessao encerrada",
                SessionState::Error(_) => "erro",
            };

            // Barra de titulo com fundo proprio, ocupando toda a largura.
            egui::Frame::NONE
                .fill(TITLE_BG)
                .inner_margin(egui::Margin::symmetric(4, 2))
                .show(ui, |ui| {
                    ui.set_min_width((rect.width() - 8.0).max(0.0));
                    ui.horizontal(|ui| {
                        let x = egui::ImageButton::new(
                            egui::Image::new(ICON_CLOSE)
                                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                .tint(ICON_TINT),
                        )
                        .frame(false);
                        if ui.add(x).on_hover_text("Fechar painel").clicked() {
                            actions.push(PaneAction::Close { path: path.clone() });
                        }
                        let title = if pane.picking {
                            "Selecione uma conexao".to_string()
                        } else {
                            pane.host_name.clone()
                        };
                        ui.label(egui::RichText::new(title).strong().color(egui::Color32::WHITE))
                            .on_hover_text(status);

                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let stack = egui::ImageButton::new(
                                    egui::Image::new(ICON_SPLIT_STACK)
                                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                        .tint(ICON_TINT),
                                )
                                .frame(false);
                                if ui
                                    .add(stack)
                                    .on_hover_text("Dividir (empilhado)")
                                    .clicked()
                                {
                                    actions.push(PaneAction::Split {
                                        path: path.clone(),
                                        dir: SplitDir::Stacked,
                                    });
                                }
                                let side = egui::ImageButton::new(
                                    egui::Image::new(ICON_SPLIT_SIDE)
                                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                        .tint(ICON_TINT),
                                )
                                .frame(false);
                                if ui
                                    .add(side)
                                    .on_hover_text("Dividir (lado a lado)")
                                    .clicked()
                                {
                                    actions.push(PaneAction::Split {
                                        path: path.clone(),
                                        dir: SplitDir::SideBySide,
                                    });
                                }
                            },
                        );
                    });
                });

            if pane.picking {
                egui::ScrollArea::vertical()
                    .id_salt(("pane_picker", path.as_slice()))
                    .show(ui, |ui| {
                        let term_img = egui::Image::new(ICON_TERMINAL)
                            .fit_to_exact_size(egui::vec2(16.0, 16.0))
                            .tint(ICON_TINT);
                        if ui
                            .add(egui::Button::image_and_text(term_img, "cmd.exe"))
                            .on_hover_text("Abrir terminal local do Windows")
                            .clicked()
                        {
                            actions.push(PaneAction::OpenCmd { path: path.clone() });
                        }
                        ui.separator();
                        if hosts.is_empty() {
                            ui.weak("Nenhum host cadastrado.");
                        }
                        for (h, host) in hosts.iter().enumerate() {
                            let label = if host.name.trim().is_empty() {
                                format!("{}@{}", host.username, host.host)
                            } else {
                                host.name.clone()
                            };
                            let img = egui::Image::new(ICON_SERVER)
                                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                                .tint(ICON_TINT);
                            if ui.add(egui::Button::image_and_text(img, label)).clicked() {
                                actions.push(PaneAction::Connect {
                                    path: path.clone(),
                                    host: h,
                                });
                            }
                        }
                    });
            } else {
                if let SessionState::Error(msg) = &pane.state {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), msg.clone());
                }

                let mut output = None;
                if let Some(term) = &mut pane.terminal {
                    output = Some(term.ui(ui));
                }
                if let Some(out) = output {
                    if let Some(ssh) = &pane.ssh {
                        if let Some((c, r)) = out.resize {
                            ssh.resize(c, r);
                        }
                        if !out.input.is_empty() {
                            ssh.send_data(out.input);
                        }
                    }
                }
            }

            // Borda que delimita a janela do painel (desenhada por ultimo).
            let border = egui::Stroke::new(1.0, PANE_BORDER);
            let r = rect.shrink(0.5);
            let painter = ui.painter();
            painter.line_segment([r.left_top(), r.right_top()], border);
            painter.line_segment([r.right_top(), r.right_bottom()], border);
            painter.line_segment([r.right_bottom(), r.left_bottom()], border);
            painter.line_segment([r.left_bottom(), r.left_top()], border);
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if !self.last_vault_path.is_empty() {
            storage.set_string(STORAGE_LAST_PATH, self.last_vault_path.clone());
        }
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ctx_for_repaint = Some(ctx.clone());

        match self.screen {
            Screen::Splash => {
                let elapsed = self.splash_start.elapsed().as_secs_f32();
                let skip = ctx.input(|i| i.pointer.any_pressed() || i.key_pressed(egui::Key::Escape));
                if elapsed >= SPLASH_SECS || skip {
                    self.screen = Screen::Gate;
                    self.gate_focus_requested = true;
                } else {
                    ctx.request_repaint();
                }
            }
            Screen::Session => self.drain_ssh_events(),
            _ => {}
        }

        match self.screen {
            Screen::Session => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(egui::Color32::from_rgb(0x0c, 0x0c, 0x0c))
                            .inner_margin(egui::Margin::ZERO),
                    )
                    .show(ctx, |ui| self.ui_session(ui));
            }
            Screen::Gate => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(GATE_BG)
                            .inner_margin(egui::Margin::same(8)),
                    )
                    .show(ctx, |ui| self.ui_gate(ui));
            }
            _ => {
                egui::CentralPanel::default().show(ctx, |ui| match self.screen {
                    Screen::Splash => self.ui_splash(ui),
                    Screen::Gate => self.ui_gate(ui),
                    Screen::Hosts => self.ui_hosts(ui),
                    Screen::Session => {}
                });
            }
        }
    }
}
