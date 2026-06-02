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
///
/// O egui pinta cada widget a partir do `Visuals`. As cores ficam organizadas
/// em duas camadas: cores "globais" (fundos de painel/janela, texto, selecao) e
/// um conjunto de estilos por estado de interacao em `v.widgets.*`
/// (noninteractive, inactive, hovered, active, open). Cada estado define o
/// fundo (`bg_fill`/`weak_bg_fill`), o texto/icone (`fg_stroke`) e a borda
/// (`bg_stroke`). Ajustar aqui afeta TODOS os widgets padrao da aplicacao.
fn apply_dark_theme(ctx: &egui::Context) {
    use egui::Stroke;

    // --- Paleta base (todas as variantes derivam destas cores) ---
    let bg_extreme = hex("#0c0912"); // fundo mais escuro (campos de texto)
    let bg_panel = hex("#15111e"); // fundo dos paineis
    let bg_widget = hex("#201a2c"); // fundo de botoes/campos em repouso
    let bg_hover = hex("#322746"); // fundo ao passar o mouse
    let accent = hex("#8b5cf6"); // roxo de destaque (bordas/links/selecao)
    let accent_dim = hex("#5f2c91"); // roxo escuro (estado pressionado)
    let text = hex("#3d3a44"); // texto principal
    let text_weak = hex("#b3a8c6"); // texto secundario/desabilitado

    // Parte do tema escuro padrao do egui e sobrescreve o que interessa.
    let mut v = egui::Visuals::dark();

    // --- Fundos globais e texto ---
    v.panel_fill = bg_panel; // fundo dos CentralPanel/SidePanel
    // window_fill/window_stroke valem tambem para menus de contexto e tooltips
    // (Frame::menu/popup), entao usam uma cor escura do tema para combinar.
    v.window_fill = hex("#241d33"); // fundo de janelas/menus/tooltips (popup)
    v.extreme_bg_color = bg_extreme; // fundo de areas "afundadas": TextEdit, ScrollArea
    v.faint_bg_color = hex("#1b1526"); // listras alternadas (ex.: linhas de Grid)
    v.code_bg_color = bg_extreme; // fundo de trechos de codigo/monospace
    v.override_text_color = Some(text); // forca a cor de todo texto (ignora a cor por estado)
    v.hyperlink_color = accent; // cor de links
    v.window_stroke = Stroke::new(1.0, hex("#332847")); // borda de janelas/menus
    v.menu_corner_radius = egui::CornerRadius::same(8); // cantos arredondados dos menus

    // --- Selecao de texto / itens selecionados ---
    v.selection.bg_fill = accent_dim; // fundo do texto/linha selecionada
    v.selection.stroke = Stroke::new(1.0, accent); // contorno da selecao

    // --- noninteractive: elementos que nao respondem ao mouse (rotulos, separadores) ---
    v.widgets.noninteractive.bg_fill = bg_panel;
    v.widgets.noninteractive.weak_bg_fill = bg_panel;
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text_weak); // cor dos rotulos

    // --- inactive: widget interativo em repouso (mouse longe) ---
    v.widgets.inactive.bg_fill = bg_widget; // fundo do botao/campo parado
    v.widgets.inactive.weak_bg_fill = bg_widget; // variante "fraca" (ex.: fundo de checkbox)
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, text); // texto/icone parado

    // --- hovered: mouse em cima do widget ---
    v.widgets.hovered.bg_fill = bg_hover; // fundo realcado no hover
    v.widgets.hovered.weak_bg_fill = bg_hover;
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, hex("#eeed9c")); // texto fica branco no hover
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent); // borda roxa que aparece ao passar o mouse

    // --- active: widget sendo clicado/pressionado ou arrastado ---
    v.widgets.active.bg_fill = accent_dim; // fundo roxo escuro ao pressionar
    v.widgets.active.weak_bg_fill = accent_dim;
    v.widgets.active.fg_stroke = Stroke::new(1.0, hex("#eeed9c")); // texto branco ao pressionar
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent); // borda roxa ao pressionar

    // --- open: combos/menus abertos ---
    v.widgets.open.bg_fill = bg_widget;
    v.widgets.open.weak_bg_fill = bg_widget;

    // Aplica o tema montado ao contexto (vale para toda a UI a partir daqui).
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

#[derive(PartialEq, Clone, Copy)]
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
    /// Texto de busca do seletor de conexoes deste painel.
    filter: String,
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
            filter: String::new(),
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
    /// Abrir o editor para o host indicado (a partir do seletor de um painel).
    Edit { host: usize },
    /// Excluir o host indicado (a partir do seletor de um painel).
    Delete { host: usize },
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
const ICON_PLUG: egui::ImageSource = egui::include_image!("../assets/plug.svg");
const ICON_SETTINGS: egui::ImageSource = egui::include_image!("../assets/settings.svg");
const ICON_TRASH: egui::ImageSource = egui::include_image!("../assets/trash.svg");
const ICON_SEARCH: egui::ImageSource = egui::include_image!("../assets/search.svg");

/// Converte uma string hexadecimal (`"#332847"` ou `"332847"`) em
/// `egui::Color32`. E uma `const fn`, entao pode ser usada tanto em constantes
/// quanto em tempo de execucao. Em caso de string invalida, retorna magenta
/// (`#ff00ff`) para destacar o erro visualmente.
///
/// # Exemplo
/// ```ignore
/// const BORDA: egui::Color32 = hex("#332847");
/// let cor = hex("8b5cf6");
/// ```
const fn hex(s: &str) -> egui::Color32 {
    let b = s.as_bytes();
    // Aceita com ou sem o '#' inicial.
    let start = if !b.is_empty() && b[0] == b'#' { 1 } else { 0 };
    if b.len() != start + 6 {
        return egui::Color32::from_rgb(0xff, 0x00, 0xff);
    }
    let r = match hex_byte(b[start], b[start + 1]) {
        Some(v) => v,
        None => return egui::Color32::from_rgb(0xff, 0x00, 0xff),
    };
    let g = match hex_byte(b[start + 2], b[start + 3]) {
        Some(v) => v,
        None => return egui::Color32::from_rgb(0xff, 0x00, 0xff),
    };
    let bl = match hex_byte(b[start + 4], b[start + 5]) {
        Some(v) => v,
        None => return egui::Color32::from_rgb(0xff, 0x00, 0xff),
    };
    egui::Color32::from_rgb(r, g, bl)
}

/// Converte dois digitos hexadecimais (alto e baixo) em um byte. `None` se
/// algum caractere nao for hexadecimal valido.
const fn hex_byte(hi: u8, lo: u8) -> Option<u8> {
    match (hex_digit(hi), hex_digit(lo)) {
        (Some(h), Some(l)) => Some(h * 16 + l),
        _ => None,
    }
}

/// Converte um unico caractere hexadecimal (0-9, a-f, A-F) em seu valor 0..=15.
const fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Cor dos icones do cabecalho (igual a do tema fraco).
const ICON_TINT: egui::Color32 = hex("#b3a8c6");
/// Cor da borda dos paineis (tambem usada no apelido da sessao).
const PANE_BORDER: egui::Color32 = hex("#3a2e52");
/// Cor da borda do painel com foco do teclado: mesma familia roxa de
/// `PANE_BORDER`, porem mais clara/saturada para destacar sem destoar.
const PANE_BORDER_FOCUS: egui::Color32 = hex("#6d559c");
/// Fundo da barra de titulo de cada painel.
const TITLE_BG: egui::Color32 = hex("#2a2925");
/// Fundo geral das telas (escuro, base do tema).
const SCREEN_BG: egui::Color32 = hex("#100c18");
/// Acento roxo do tema (usado em titulos, botoes e realces).
const ACCENT: egui::Color32 = hex("#8b5cf6");
/// Texto secundario (cinza-arroxeado).
const TEXT_WEAK: egui::Color32 = hex("#b3a8c6");
/// Texto principal (claro), para itens de menu/destaques sobre fundo escuro.
const CARD_TEXT: egui::Color32 = hex("#e7e1f2");
/// Vermelho suave para acoes destrutivas (ex.: Excluir).
const DANGER: egui::Color32 = hex("#e88a8a");
/// Fundo de cada cartao de host na listagem.
const CARD_BG: egui::Color32 = hex("#1b1526");
/// Borda dos cartoes da listagem de hosts.
const CARD_BORDER: egui::Color32 = hex("#332847");
/// Verde para indicar autenticacao por chave.
const AUTH_KEY: egui::Color32 = hex("#6ed69a");
/// Ambar para indicar autenticacao por senha.
const AUTH_PASS: egui::Color32 = hex("#e2b34f");

/// Formulario de edicao/criacao de host.
struct HostEditor {
    index: Option<usize>,
    name: String,
    host: String,
    port_text: String,
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
            port_text: "22".to_string(),
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
            port_text: h.port.to_string(),
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
            port: self.parsed_port().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth,
        }
    }

    /// Porta valida (1..=65535) a partir do texto digitado, se houver.
    fn parsed_port(&self) -> Option<u16> {
        self.port_text.trim().parse::<u16>().ok().filter(|&p| p >= 1)
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

    // Texto de busca para filtrar a listagem de hosts (por nome/apelido/host).
    hosts_filter: String,

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

    // Caminho do painel cujo terminal tem o foco do teclado (atualizado a cada
    // quadro durante a renderizacao da sessao).
    focused_path: Option<Vec<usize>>,

    // Verdadeiro apos pressionar Ctrl+B, aguardando a segunda tecla do atalho
    // de divisao (H = lado a lado, V = empilhado).
    split_chord_armed: bool,
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
            hosts_filter: String::new(),
            root: None,
            ctx_for_repaint: None,
            logo_texture: None,
            logo_load_attempted: false,
            splash_start: Instant::now(),
            last_vault_path,
            gate_focus_requested: true,
            focused_path: None,
            split_chord_armed: false,
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
        self.hosts_filter.clear();
        self.gate_focus_requested = true;
        self.screen = Screen::Gate;
    }

    // ---------------- Portao do cofre ----------------

    fn ui_gate(&mut self, ui: &mut egui::Ui) {
        self.ensure_logo(ui.ctx());

        // Fundo: logo discreto centralizado, bem fraco, atras da janela.
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.10);
            if let Some(tex) = &self.logo_texture {
                let size = tex.size_vec2();
                let scale = (260.0 / size.x).min(1.0);
                ui.add(
                    egui::Image::new(egui::load::SizedTexture::from_handle(tex))
                        .fit_to_exact_size(size * scale)
                        .tint(egui::Color32::from_white_alpha(22)),
                );
            }
        });

        // Largura util dos campos dentro da janela.
        const FIELD_W: f32 = 360.0;

        let frame = egui::Frame::window(&ui.ctx().style())
            .fill(CARD_BG)
            .stroke(egui::Stroke::new(1.0, CARD_BORDER))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(24))
            .shadow(egui::epaint::Shadow {
                offset: [0, 8],
                blur: 28,
                spread: 0,
                color: egui::Color32::from_black_alpha(140),
            });

        let mut submit = false;
        egui::Window::new("gate_window")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            // Levemente acima do centro: melhora o foco visual do operador.
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .frame(frame)
            .show(ui.ctx(), |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;

                // Cabecalho com icone e titulo.
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Image::new(ICON_SERVER)
                            .fit_to_exact_size(egui::vec2(22.0, 22.0))
                            .tint(ACCENT),
                    );
                    ui.add_space(2.0);
                    ui.heading(
                        egui::RichText::new("Cofre SaguTerm").color(ACCENT).size(22.0),
                    );
                });
                ui.label(
                    egui::RichText::new(
                        "Abra um cofre existente ou crie um novo para guardar suas conexoes.",
                    )
                    .small()
                    .color(TEXT_WEAK),
                );

                ui.add_space(12.0);

                // Abas Abrir / Criar como botoes segmentados.
                ui.horizontal(|ui| {
                    gate_tab(ui, &mut self.gate_mode, GateMode::Open, "Abrir cofre");
                    ui.add_space(6.0);
                    gate_tab(ui, &mut self.gate_mode, GateMode::Create, "Criar cofre");
                });

                ui.add_space(10.0);

                let mut pwd_enter = false;

                // Arquivo do cofre.
                ui.label(egui::RichText::new("Arquivo do cofre").small().color(TEXT_WEAK));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.gate_path)
                            .desired_width(FIELD_W - 110.0)
                            .hint_text("caminho do arquivo .sagu"),
                    );
                    let btn = if self.gate_mode == GateMode::Open {
                        "Procurar..."
                    } else {
                        "Salvar como..."
                    };
                    if ui
                        .add_sized([100.0, 24.0], egui::Button::new(btn))
                        .clicked()
                    {
                        self.pick_vault_path();
                    }
                });

                ui.add_space(6.0);

                // Senha mestra.
                ui.label(egui::RichText::new("Senha mestra").small().color(TEXT_WEAK));
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.gate_password)
                        .password(true)
                        .desired_width(FIELD_W)
                        .hint_text("senha do cofre"),
                );
                if self.gate_focus_requested {
                    resp.request_focus();
                    self.gate_focus_requested = false;
                }
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    pwd_enter = true;
                }

                if self.gate_mode == GateMode::Create {
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Confirmar senha").small().color(TEXT_WEAK),
                    );
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.gate_password_confirm)
                            .password(true)
                            .desired_width(FIELD_W)
                            .hint_text("repita a senha"),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        pwd_enter = true;
                    }
                }

                if let Some(err) = &self.gate_error {
                    ui.add_space(8.0);
                    ui.colored_label(hex("#ff6b6b"), err);
                }

                ui.add_space(16.0);

                // Botao de acao principal, ocupando toda a largura.
                let action_label = if self.gate_mode == GateMode::Open {
                    "Abrir cofre"
                } else {
                    "Criar cofre"
                };
                let action_btn = egui::Button::new(
                    egui::RichText::new(action_label).color(egui::Color32::WHITE).size(15.0),
                )
                .fill(ACCENT)
                .corner_radius(8.0)
                .wrap_mode(egui::TextWrapMode::Extend);
                if ui.add_sized([FIELD_W, 34.0], action_btn).clicked() || pwd_enter {
                    submit = true;
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
        // Cabecalho com titulo e acoes principais.
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("Conexões").color(ACCENT).size(26.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Bloquear: acao secundaria, estilo discreto do tema.
                let bloquear = egui::Button::new(
                    egui::RichText::new("\u{1f512}  Bloquear cofre")
                        .color(TEXT_WEAK)
                        .size(14.0),
                )
                .fill(CARD_BG)
                .stroke(egui::Stroke::new(1.0, CARD_BORDER))
                .corner_radius(6.0);
                if ui
                    .add_sized([130.0, 30.0], bloquear)
                    .on_hover_text("Bloquear o cofre e voltar a tela de senha")
                    .clicked()
                {
                    self.lock();
                }
                ui.add_space(6.0);
                // Novo host: acao primaria, preenchida com o acento.
                let novo = egui::Button::new(
                    egui::RichText::new("+  Novo host")
                        .color(egui::Color32::WHITE)
                        .size(14.0),
                )
                .fill(ACCENT)
                .corner_radius(6.0);
                if ui.add_sized([120.0, 30.0], novo).clicked() {
                    self.hosts_error = None;
                    self.editor = Some(HostEditor::new());
                }
            });
        });
        if let Some(path) = &self.vault_path {
            ui.label(
                egui::RichText::new(format!("\u{1f5c4}  {}", path.to_string_lossy()))
                    .small()
                    .color(TEXT_WEAK),
            );
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(8.0);

        if let Some(err) = &self.hosts_error {
            ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), err);
            ui.add_space(4.0);
        }

        let mut connect_index: Option<usize> = None;
        let mut edit_index: Option<usize> = None;
        let mut delete_index: Option<usize> = None;
        let mut open_cmd = false;

        // Seletor de conexoes compartilhado (com busca e gerenciamento). O foco
        // automatico no campo so vale quando nao ha um host sendo editado.
        let autofocus = self.editor.is_none();
        match connection_picker(
            ui,
            &self.vault.hosts,
            &mut self.hosts_filter,
            "hosts_picker",
            true,
            autofocus,
        ) {
            Some(PickerAction::OpenCmd) => open_cmd = true,
            Some(PickerAction::Connect(i)) => connect_index = Some(i),
            Some(PickerAction::Edit(i)) => edit_index = Some(i),
            Some(PickerAction::Delete(i)) => delete_index = Some(i),
            None => {}
        }

        if open_cmd {
            self.start_cmd_session();
        }
        if let Some(i) = edit_index {
            self.hosts_error = None;
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

        // Janela flutuante de edicao/criacao, centralizada sobre a listagem.
        if self.editor.is_some() {
            self.ui_host_editor(ui.ctx());
        }
    }

    fn ui_host_editor(&mut self, ctx: &egui::Context) {
        // Trabalha sobre uma copia para nao colidir com o emprestimo de self.
        let mut editor = self.editor.take().unwrap();
        let mut save = false;
        let mut cancel = false;
        let mut load_key = false;

        let editing = editor.index.is_some();
        let titulo = if editing { "Editar host" } else { "Novo host" };
        let subtitulo = if editing {
            "Altere os dados da conexao e salve."
        } else {
            "Preencha os dados para cadastrar uma nova conexao."
        };

        // Largura util dos campos (mesmo padrao do portao do cofre).
        const FIELD_W: f32 = 360.0;

        let frame = egui::Frame::window(&ctx.style())
            .fill(CARD_BG)
            .stroke(egui::Stroke::new(1.0, CARD_BORDER))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(24))
            .shadow(egui::epaint::Shadow {
                offset: [0, 8],
                blur: 28,
                spread: 0,
                color: egui::Color32::from_black_alpha(140),
            });

        egui::Window::new("host_editor_window")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -20.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;

                // Cabecalho com icone e titulo (igual ao portao do cofre).
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Image::new(ICON_SERVER)
                            .fit_to_exact_size(egui::vec2(22.0, 22.0))
                            .tint(ACCENT),
                    );
                    ui.add_space(2.0);
                    ui.heading(egui::RichText::new(titulo).color(ACCENT).size(22.0));
                });
                ui.label(egui::RichText::new(subtitulo).small().color(TEXT_WEAK));

                ui.add_space(12.0);

                if let Some(err) = &self.hosts_error {
                    ui.colored_label(hex("#ff6b6b"), err);
                    ui.add_space(6.0);
                }

                // Nome
                ui.label(egui::RichText::new("Nome").small().color(TEXT_WEAK));
                ui.add(
                    egui::TextEdit::singleline(&mut editor.name)
                        .desired_width(FIELD_W)
                        .hint_text("apelido (opcional)"),
                );

                ui.add_space(6.0);

                // Host + Porta lado a lado.
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Host").small().color(TEXT_WEAK));
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.host)
                                .desired_width(FIELD_W - 110.0)
                                .hint_text("endereco ou IP"),
                        );
                    });
                    ui.add_space(8.0);
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new("Porta").small().color(TEXT_WEAK));
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut editor.port_text)
                                .desired_width(90.0)
                                .hint_text("1-65535"),
                        );
                        if resp.changed() {
                            // Mantem apenas digitos e limita ao range de portas TCP.
                            let digits: String = editor
                                .port_text
                                .chars()
                                .filter(|c| c.is_ascii_digit())
                                .collect();
                            editor.port_text = match digits.parse::<u32>() {
                                Ok(n) if n > 65535 => "65535".to_string(),
                                _ => digits,
                            };
                        }
                    });
                });

                ui.add_space(6.0);

                // Usuario
                ui.label(egui::RichText::new("Usuario").small().color(TEXT_WEAK));
                ui.add(egui::TextEdit::singleline(&mut editor.username).desired_width(FIELD_W));

                ui.add_space(10.0);

                // Autenticacao: abas segmentadas (Senha/Chave).
                ui.label(egui::RichText::new("Autenticacao").small().color(TEXT_WEAK));
                ui.horizontal(|ui| {
                    auth_tab(ui, &mut editor.use_key, false, "Senha");
                    ui.add_space(6.0);
                    auth_tab(ui, &mut editor.use_key, true, "Chave");
                });

                ui.add_space(8.0);

                if editor.use_key {
                    ui.label(egui::RichText::new("Chave privada").small().color(TEXT_WEAK));
                    // Altura fixa com rolagem interna: uma chave grande nao
                    // expande a janela alem da tela (overflow rolavel).
                    egui::ScrollArea::vertical()
                        .id_salt("private_key_scroll")
                        .max_height(110.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::TextEdit::multiline(&mut editor.private_key)
                                    .desired_width(FIELD_W)
                                    .desired_rows(4)
                                    .hint_text("-----BEGIN OPENSSH PRIVATE KEY-----"),
                            );
                        });
                    if ui
                        .add_sized(
                            [FIELD_W, 26.0],
                            egui::Button::new("Carregar de arquivo...").corner_radius(6.0),
                        )
                        .clicked()
                    {
                        load_key = true;
                    }

                    ui.add_space(6.0);
                    ui.label(egui::RichText::new("Passphrase").small().color(TEXT_WEAK));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.passphrase)
                            .password(true)
                            .desired_width(FIELD_W)
                            .hint_text("opcional"),
                    );
                } else {
                    ui.label(egui::RichText::new("Senha").small().color(TEXT_WEAK));
                    ui.add(
                        egui::TextEdit::singleline(&mut editor.password)
                            .password(true)
                            .desired_width(FIELD_W),
                    );
                }

                ui.add_space(16.0);

                // Acoes: Salvar (acento, principal) e Cancelar lado a lado.
                ui.horizontal(|ui| {
                    let half = (FIELD_W - 8.0) / 2.0;
                    let salvar = egui::Button::new(
                        egui::RichText::new("Salvar").color(egui::Color32::WHITE).size(15.0),
                    )
                    .fill(ACCENT)
                    .corner_radius(8.0)
                    .wrap_mode(egui::TextWrapMode::Extend);
                    if ui.add_sized([half, 34.0], salvar).clicked() {
                        save = true;
                    }
                    ui.add_space(8.0);
                    let cancelar = egui::Button::new(
                        egui::RichText::new("Cancelar").color(TEXT_WEAK).size(15.0),
                    )
                    .fill(CARD_BG)
                    .stroke(egui::Stroke::new(1.0, CARD_BORDER))
                    .corner_radius(8.0)
                    .wrap_mode(egui::TextWrapMode::Extend);
                    if ui.add_sized([half, 34.0], cancelar).clicked() {
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
            self.hosts_error = None;
            return;
        }

        if save {
            if editor.host.trim().is_empty() || editor.username.trim().is_empty() {
                self.hosts_error = Some("Host e usuario sao obrigatorios.".into());
                self.editor = Some(editor);
                return;
            }
            if editor.parsed_port().is_none() {
                self.hosts_error = Some("Informe uma porta valida (1-65535).".into());
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
                pane.host_name = "Local".to_string();
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

    /// Trata o atalho de divisao em dois tempos: `Ctrl+B` seguido de `H`
    /// (lado a lado) ou `V` (empilhado). Consome os eventos correspondentes do
    /// teclado para que o terminal em foco nao os receba como bytes.
    fn handle_split_chord(&mut self, ctx: &egui::Context) {
        // Coleta as teclas pressionadas neste quadro, removendo da fila as que
        // fazem parte do atalho para nao vazarem para o terminal.
        let mut armed = self.split_chord_armed;
        let mut split: Option<SplitDir> = None;
        let mut disarm_other = false;

        ctx.input_mut(|i| {
            i.events.retain(|ev| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = ev
                else {
                    return true;
                };
                if !armed {
                    // Arma o atalho ao pressionar Ctrl+B (consome o evento).
                    if *key == egui::Key::B && modifiers.ctrl {
                        armed = true;
                        return false;
                    }
                    return true;
                }
                // Ja armado: a proxima tecla decide a divisao.
                match key {
                    egui::Key::H => {
                        split = Some(SplitDir::SideBySide);
                        false
                    }
                    egui::Key::V => {
                        split = Some(SplitDir::Stacked);
                        false
                    }
                    // Permite encadear (Ctrl+B seguido de Ctrl+B) sem cancelar.
                    egui::Key::B if modifiers.ctrl => false,
                    _ => {
                        disarm_other = true;
                        true
                    }
                }
            });
        });

        if let Some(dir) = split {
            if let Some(path) = self.focused_path.clone() {
                self.split_pane(&path, dir);
            }
            self.split_chord_armed = false;
        } else if disarm_other {
            self.split_chord_armed = false;
        } else {
            self.split_chord_armed = armed;
        }
    }

    /// Renderiza a arvore de paineis na area disponivel e aplica as acoes
    /// estruturais coletadas (dividir/fechar/conectar).
    fn ui_session(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let mut actions: Vec<PaneAction> = Vec::new();
        let hosts = &self.vault.hosts;
        let mut focused: Option<Vec<usize>> = None;
        if let Some(root) = &mut self.root {
            let mut path = Vec::new();
            render_node(ui, rect, root, &mut path, hosts, &mut actions, &mut focused);
        }
        self.focused_path = focused;
        for action in actions {
            match action {
                PaneAction::Split { path, dir } => self.split_pane(&path, dir),
                PaneAction::Close { path } => self.close_pane(&path),
                PaneAction::Connect { path, host } => self.connect_pane(&path, host),
                PaneAction::OpenCmd { path } => self.connect_cmd_pane(&path),
                PaneAction::Edit { host } => {
                    self.hosts_error = None;
                    self.editor = Some(HostEditor::from_host(host, &self.vault.hosts[host]));
                }
                PaneAction::Delete { host } => {
                    if host < self.vault.hosts.len() {
                        self.vault.hosts.remove(host);
                        if let Err(e) = self.save_vault() {
                            self.hosts_error = Some(format!("Erro ao salvar: {e}"));
                        }
                    }
                }
            }
        }

        // Editor de host pode ser aberto a partir do seletor de um painel; e
        // renderizado sobre a sessao como janela flutuante.
        if self.editor.is_some() {
            self.ui_host_editor(ui.ctx());
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

/// Trunca um texto longo com reticencias para caber na legenda do bloco.
fn elide(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('\u{2026}');
        out
    }
}

/// Aba segmentada do portao do cofre (Abrir/Criar): preenchida com acento
/// quando selecionada, discreta caso contrario.
fn gate_tab(ui: &mut egui::Ui, mode: &mut GateMode, value: GateMode, text: &str) {
    let selected = *mode == value;
    let (fill, fg, stroke) = if selected {
        (ACCENT, egui::Color32::WHITE, ACCENT)
    } else {
        (CARD_BG, TEXT_WEAK, CARD_BORDER)
    };
    let btn = egui::Button::new(egui::RichText::new(text).color(fg))
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(6.0);
    if ui.add_sized([130.0, 28.0], btn).clicked() {
        *mode = value;
    }
}

/// Aba segmentada do metodo de autenticacao (Senha/Chave) na janela de host.
/// Mesmo visual das abas do portao do cofre.
fn auth_tab(ui: &mut egui::Ui, use_key: &mut bool, value: bool, text: &str) {
    let selected = *use_key == value;
    let (fill, fg, stroke) = if selected {
        (ACCENT, egui::Color32::WHITE, ACCENT)
    } else {
        (CARD_BG, TEXT_WEAK, CARD_BORDER)
    };
    let btn = egui::Button::new(egui::RichText::new(text).color(fg))
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke))
        .corner_radius(6.0);
    if ui.add_sized([110.0, 26.0], btn).clicked() {
        *use_key = value;
    }
}

/// Largura util dos itens do menu de contexto das conexoes.
const MENU_ITEM_W: f32 = 168.0;

/// Ajusta os visuais do menu de contexto: itens sem fundo em repouso, realce
/// roxo no hover, cantos arredondados e espacamento confortavel.
///
/// Tambem pinta um fundo escuro do tema cobrindo o frame claro padrao do egui.
/// Usa a tecnica de "shape placeholder": reserva um indice no painter agora e
/// preenche o retangulo correto depois que o conteudo definir o tamanho.
fn style_context_menu(ui: &mut egui::Ui) -> egui::layers::ShapeIdx {
    ui.set_min_width(MENU_ITEM_W);
    ui.spacing_mut().item_spacing.y = 2.0;
    ui.spacing_mut().button_padding = egui::vec2(8.0, 6.0);

    // Reserva um espaco no painter (desenhado ANTES do conteudo, ou seja, atras
    // dele). O retangulo real e definido por `paint_menu_bg` no fim do menu.
    let bg_idx = ui.painter().add(egui::Shape::Noop);

    let v = ui.visuals_mut();
    // Em repouso o item se funde ao fundo do menu (sem "caixa").
    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.inactive.bg_fill = egui::Color32::TRANSPARENT;
    v.widgets.inactive.bg_stroke = egui::Stroke::NONE;
    // No hover, fundo roxo translucido sem borda.
    v.widgets.hovered.weak_bg_fill = ACCENT.gamma_multiply(0.30);
    v.widgets.hovered.bg_fill = ACCENT.gamma_multiply(0.30);
    v.widgets.hovered.bg_stroke = egui::Stroke::NONE;
    v.widgets.active.weak_bg_fill = ACCENT.gamma_multiply(0.45);
    v.widgets.active.bg_fill = ACCENT.gamma_multiply(0.45);
    v.widgets.active.bg_stroke = egui::Stroke::NONE;
    v.widgets.inactive.corner_radius = egui::CornerRadius::same(6);
    v.widgets.hovered.corner_radius = egui::CornerRadius::same(6);
    v.widgets.active.corner_radius = egui::CornerRadius::same(6);

    bg_idx
}

/// Preenche o fundo escuro do menu de contexto no indice reservado por
/// `style_context_menu`, agora que o conteudo ja definiu o tamanho real.
fn paint_menu_bg(ui: &egui::Ui, bg_idx: egui::layers::ShapeIdx) {
    let rect = ui.min_rect().expand(6.0);
    ui.painter().set(
        bg_idx,
        egui::epaint::RectShape::new(
            rect,
            8.0,
            hex("#241d33"),
            egui::Stroke::new(1.0, CARD_BORDER),
            egui::StrokeKind::Inside,
        ),
    );
}

/// Item de menu de contexto: icone SVG + rotulo, ocupando toda a largura.
/// O icone e o texto usam a mesma cor (`color`). Retorna `true` quando clicado.
fn menu_item(
    ui: &mut egui::Ui,
    icon: egui::ImageSource,
    text: &str,
    color: egui::Color32,
) -> bool {
    let img = egui::Image::new(icon)
        .fit_to_exact_size(egui::vec2(16.0, 16.0))
        .tint(color);
    ui.add_sized(
        [MENU_ITEM_W, 24.0],
        egui::Button::image_and_text(img, egui::RichText::new(text).color(color))
            .wrap_mode(egui::TextWrapMode::Extend)
            .min_size(egui::vec2(MENU_ITEM_W, 24.0)),
    )
    .clicked()
}

/// Largura/altura de cada cartao compacto da listagem de hosts.
const HOST_TILE_SIZE: egui::Vec2 = egui::vec2(178.0, 96.0);

/// Desenha um cartao compacto de host (icone + nome + endereco + etiqueta de
/// autenticacao) com largura fixa, para caberem varios por linha. Retorna a
/// resposta (sense de clique) para tratar duplo clique e menu de contexto.
fn host_tile(
    ui: &mut egui::Ui,
    icon: egui::ImageSource,
    title: &str,
    subtitle: &str,
    auth: Option<(egui::Color32, &str)>,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(HOST_TILE_SIZE, egui::Sense::click());
    let painter = ui.painter();

    // Fundo do cartao, realcando borda ao passar o mouse.
    let border = if response.hovered() {
        egui::Stroke::new(1.0, ACCENT)
    } else {
        egui::Stroke::new(1.0, CARD_BORDER)
    };
    painter.rect(rect, 8.0, CARD_BG, border, egui::StrokeKind::Inside);

    let pad = 12.0;
    // Badge do icone no canto superior esquerdo.
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.left() + pad, rect.top() + pad),
        egui::vec2(34.0, 34.0),
    );
    painter.rect(
        badge,
        8.0,
        egui::Color32::from_rgb(0x2a, 0x1f, 0x40),
        egui::Stroke::new(1.0, egui::Color32::from_rgb(0x46, 0x33, 0x6b)),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(badge.center(), egui::vec2(19.0, 19.0));
    egui::Image::new(icon).tint(ACCENT).paint_at(ui, icon_rect);

    // Etiqueta de autenticacao no canto superior direito.
    if let Some((color, label)) = auth {
        let galley = painter.layout_no_wrap(
            label.to_string(),
            egui::FontId::proportional(11.0),
            color,
        );
        let chip = egui::Rect::from_min_size(
            egui::pos2(rect.right() - pad - galley.size().x - 12.0, rect.top() + pad + 4.0),
            egui::vec2(galley.size().x + 12.0, galley.size().y + 4.0),
        );
        painter.rect(
            chip,
            8.0,
            color.gamma_multiply(0.18),
            egui::Stroke::new(1.0, color.gamma_multiply(0.6)),
            egui::StrokeKind::Inside,
        );
        painter.galley(
            egui::pos2(chip.left() + 6.0, chip.top() + 2.0),
            galley,
            color,
        );
    }

    // Titulo (nome) e subtitulo (endereco), truncados para caber.
    let text_x = badge.right() + 10.0;
    let max_chars = 18;
    painter.text(
        egui::pos2(text_x, rect.top() + pad + 2.0),
        egui::Align2::LEFT_TOP,
        elide(title, max_chars),
        egui::FontId::proportional(14.0),
        egui::Color32::WHITE,
    );
    painter.text(
        egui::pos2(rect.left() + pad, rect.bottom() - pad - 14.0),
        egui::Align2::LEFT_TOP,
        elide(subtitle, 26),
        egui::FontId::proportional(11.0),
        TEXT_WEAK,
    );

    response
}

/// Acao escolhida pelo usuario no seletor de conexoes compartilhado.
enum PickerAction {
    /// Abrir terminal local (cmd.exe).
    OpenCmd,
    /// Conectar ao host no indice indicado.
    Connect(usize),
    /// Editar o host no indice indicado (so disponivel quando `manage`).
    Edit(usize),
    /// Excluir o host no indice indicado (so disponivel quando `manage`).
    Delete(usize),
}

/// Seletor de conexoes reutilizavel: campo de busca + grade de cartoes (Terminal
/// local + hosts) com duplo clique para conectar e menu de contexto.
///
/// Usado tanto na tela principal (`ui_hosts`) quanto no seletor que aparece ao
/// dividir um painel. Com `manage = true` o menu de contexto mostra tambem
/// Editar/Excluir, permitindo gerenciar hosts em qualquer um dos dois locais.
/// `filter` e o texto de busca (mantido pelo chamador) e `id_salt` isola o
/// estado do `ScrollArea` quando ha varios seletores.
fn connection_picker(
    ui: &mut egui::Ui,
    hosts: &[Host],
    filter: &mut String,
    id_salt: impl std::hash::Hash,
    manage: bool,
    autofocus: bool,
) -> Option<PickerAction> {
    let mut action: Option<PickerAction> = None;

    // Campo de busca com lupa e botao de limpar.
    ui.horizontal(|ui| {
        ui.add(
            egui::Image::new(ICON_SEARCH)
                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                .tint(TEXT_WEAK),
        );
        let resp = ui.add(
            egui::TextEdit::singleline(filter)
                .desired_width(260.0)
                .hint_text("Filtrar conexoes..."),
        );
        // Foca o campo para que comecar a digitar ja filtre.
        if autofocus && !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
            resp.request_focus();
        }
        if !filter.is_empty()
            && ui
                .add(egui::Button::new("\u{2715}").frame(false))
                .on_hover_text("Limpar filtro")
                .clicked()
        {
            filter.clear();
        }
    });
    ui.add_space(8.0);

    let termo = filter.trim().to_lowercase();
    let mut visiveis = 0usize;
    let mut ultimo_match: Option<usize> = None;

    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);

                // Terminal local (so com filtro vazio ou que case com o nome).
                if termo.is_empty()
                    || "terminal local".contains(&termo)
                    || "cmd".contains(&termo)
                {
                    visiveis += 1;
                    let local = host_tile(
                        ui,
                        ICON_TERMINAL,
                        "Terminal local",
                        "Prompt de comando do Windows",
                        None,
                    );
                    if local.double_clicked() {
                        action = Some(PickerAction::OpenCmd);
                    }
                    local.context_menu(|ui| {
                        let bg = style_context_menu(ui);
                        if menu_item(ui, ICON_PLUG, "Abrir", ACCENT) {
                            action = Some(PickerAction::OpenCmd);
                            ui.close_menu();
                        }
                        paint_menu_bg(ui, bg);
                    });
                }

                for (i, host) in hosts.iter().enumerate() {
                    let title = if host.name.trim().is_empty() {
                        host.host.clone()
                    } else {
                        host.name.clone()
                    };
                    // Filtra por nome/apelido e endereco do host.
                    if !termo.is_empty()
                        && !title.to_lowercase().contains(&termo)
                        && !host.host.to_lowercase().contains(&termo)
                    {
                        continue;
                    }
                    visiveis += 1;
                    ultimo_match = Some(i);
                    let subtitle = format!("{}@{}:{}", host.username, host.host, host.port);
                    let auth = match &host.auth {
                        AuthMethod::Password { .. } => (AUTH_PASS, "senha"),
                        AuthMethod::Key { .. } => (AUTH_KEY, "chave"),
                    };
                    let tile = host_tile(ui, ICON_SERVER, &title, &subtitle, Some(auth));
                    if tile.double_clicked() {
                        action = Some(PickerAction::Connect(i));
                    }
                    tile.context_menu(|ui| {
                        let bg = style_context_menu(ui);
                        ui.label(
                            egui::RichText::new(elide(&title, 22)).small().color(TEXT_WEAK),
                        );
                        ui.add_space(2.0);
                        if menu_item(ui, ICON_PLUG, "Conectar", ACCENT) {
                            action = Some(PickerAction::Connect(i));
                            ui.close_menu();
                        }
                        if manage {
                            if menu_item(ui, ICON_SETTINGS, "Editar", CARD_TEXT) {
                                action = Some(PickerAction::Edit(i));
                                ui.close_menu();
                            }
                            ui.add_space(2.0);
                            ui.separator();
                            ui.add_space(2.0);
                            if menu_item(ui, ICON_TRASH, "Excluir", DANGER) {
                                action = Some(PickerAction::Delete(i));
                                ui.close_menu();
                            }
                        }
                        paint_menu_bg(ui, bg);
                    });
                }
            });

            // Mensagens de lista vazia / sem correspondencia.
            if hosts.is_empty() && termo.is_empty() {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Nenhum host cadastrado.").color(TEXT_WEAK),
                );
            } else if visiveis == 0 {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(format!(
                        "Nenhuma conexao corresponde a \"{}\".",
                        filter.trim()
                    ))
                    .color(TEXT_WEAK),
                );
            }
        });

    // Enter com filtro ativo e um unico resultado conecta direto.
    if action.is_none()
        && !termo.is_empty()
        && visiveis == 1
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
    {
        if let Some(i) = ultimo_match {
            action = Some(PickerAction::Connect(i));
        }
    }

    action
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
    focused: &mut Option<Vec<usize>>,
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
                    render_node(ui, child_rect, child, path, hosts, actions, focused);
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
                                    .on_hover_text("Dividir empilhado (Ctrl+B, V)")
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
                                    .on_hover_text("Dividir lado a lado (Ctrl+B, H)")
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
                ui.add_space(8.0);
                // Mesmo seletor de conexoes da tela principal (com filtro), porem
                // sem as acoes de gerenciamento (Editar/Excluir).
                let outcome = connection_picker(
                    ui,
                    hosts,
                    &mut pane.filter,
                    ("pane_picker", path.as_slice()),
                    true,
                    false,
                );
                match outcome {
                    Some(PickerAction::OpenCmd) => {
                        actions.push(PaneAction::OpenCmd { path: path.clone() })
                    }
                    Some(PickerAction::Connect(h)) => actions.push(PaneAction::Connect {
                        path: path.clone(),
                        host: h,
                    }),
                    Some(PickerAction::Edit(h)) => {
                        actions.push(PaneAction::Edit { host: h })
                    }
                    Some(PickerAction::Delete(h)) => {
                        actions.push(PaneAction::Delete { host: h })
                    }
                    None => {}
                }
            } else {
                if let SessionState::Error(msg) = &pane.state {
                    ui.colored_label(egui::Color32::from_rgb(0xff, 0x6b, 0x6b), msg.clone());
                }

                let mut output = None;
                if let Some(term) = &mut pane.terminal {
                    output = Some(term.ui(ui));
                }
                if let Some(out) = output {
                    if out.focused {
                        *focused = Some(path.clone());
                    }
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
            // O painel com foco do teclado recebe borda verde mais espessa.
            let is_focused = focused.as_deref() == Some(path.as_slice());
            let border = if is_focused {
                egui::Stroke::new(1.0, PANE_BORDER_FOCUS)
            } else {
                egui::Stroke::new(1.0, PANE_BORDER)
            };
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
            Screen::Session => {
                self.handle_split_chord(ctx);
                self.drain_ssh_events();
            }
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
                            .fill(SCREEN_BG)
                            .inner_margin(egui::Margin::same(8)),
                    )
                    .show(ctx, |ui| self.ui_gate(ui));
            }
            Screen::Hosts => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(SCREEN_BG)
                            .inner_margin(egui::Margin::same(16)),
                    )
                    .show(ctx, |ui| self.ui_hosts(ui));
            }
            Screen::Splash => {
                egui::CentralPanel::default()
                    .frame(egui::Frame::NONE.fill(SCREEN_BG))
                    .show(ctx, |ui| self.ui_splash(ui));
            }
        }
    }
}
