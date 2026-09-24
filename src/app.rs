//! Aplicacao eframe/egui: portao do cofre, gestao de hosts e sessao de terminal.

use std::path::PathBuf;
use std::time::Instant;

use crate::pty;
use crate::sftp::{self, SftpHandle, SftpToUi};
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

    // --- Paleta base grafite (todas as variantes derivam destas cores) ---
    // Escala neutra a partir de #1b1b1f (base) e #1f1f1f (um tom acima),
    // com um leve viés frio nos tons claros para dar profundidade.
    let bg_extreme = hex("#121216"); // fundo mais escuro (campos de texto)
    let bg_panel = hex("#1b1b1f"); // fundo dos paineis (base do tema)
    let bg_widget = hex("#26262b"); // fundo de botoes/campos em repouso
    let bg_hover = hex("#33333a"); // fundo ao passar o mouse
    let accent = hex("#9ba3b4"); // aco claro de destaque (bordas/links/icones)
    let accent_dim = hex("#454b57"); // grafite medio (estado pressionado)
    let text = hex("#ffffff"); // texto principal
    let text_weak = hex("#a0a0aa"); // texto secundario/desabilitado

    // Parte do tema escuro padrao do egui e sobrescreve o que interessa.
    let mut v = egui::Visuals::dark();

    // --- Fundos globais e texto ---
    v.panel_fill = bg_panel; // fundo dos CentralPanel/SidePanel
    // window_fill/window_stroke valem tambem para menus de contexto e tooltips
    // (Frame::menu/popup), entao usam uma cor escura do tema para combinar.
    v.window_fill = MENU_BG; // fundo de janelas/menus/tooltips (popup)
    v.extreme_bg_color = bg_extreme; // fundo de areas "afundadas": TextEdit, ScrollArea
    v.faint_bg_color = hex("#1e1e22"); // listras alternadas (ex.: linhas de Grid)
    v.code_bg_color = bg_extreme; // fundo de trechos de codigo/monospace
    v.override_text_color = Some(text); // forca a cor de todo texto (ignora a cor por estado)
    v.hyperlink_color = accent; // cor de links
    v.window_stroke = Stroke::new(1.0, hex("#35353d")); // borda de janelas/menus
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
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, hex("#eeed9c")); // texto ambar no hover
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, accent); // borda clara ao passar o mouse

    // --- active: widget sendo clicado/pressionado ou arrastado ---
    v.widgets.active.bg_fill = accent_dim; // grafite medio ao pressionar
    v.widgets.active.weak_bg_fill = accent_dim;
    v.widgets.active.fg_stroke = Stroke::new(1.0, hex("#eeed9c")); // texto ambar ao pressionar
    v.widgets.active.bg_stroke = Stroke::new(1.0, accent); // borda clara ao pressionar

    // --- open: combos/menus abertos ---
    v.widgets.open.bg_fill = bg_widget;
    v.widgets.open.weak_bg_fill = bg_widget;

    // Aplica o tema montado ao contexto (vale para toda a UI a partir daqui).
    ctx.set_visuals(v);
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
    /// Sessao SFTP e estado do navegador de arquivos (quando o painel e SFTP).
    sftp: Option<SftpHandle>,
    explorer: Option<FileExplorer>,
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
            sftp: None,
            explorer: None,
            state: SessionState::Closed,
            host_name: String::new(),
            picking: true,
            filter: String::new(),
            should_close: false,
        }
    }
}

/// Uma entrada (arquivo ou pasta) do diretorio atual no navegador SFTP.
struct FsNode {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    /// Bits de permissao (modo POSIX, ex.: 0o644).
    mode: u32,
    /// Nome (ou id) do proprietario e do grupo, ja resolvidos pelo backend.
    owner: String,
    group: String,
    /// Data da ultima alteracao ja formatada (DD/MM/AAAA; vazia se ausente).
    /// Pre-formatada na chegada da listagem para nao recalcular por quadro.
    date: String,
}

/// Operacao de gerenciamento de arquivo solicitada no menu de contexto SFTP.
enum FsOp {
    Rename { from: String, to: String },
    Chmod { path: String, mode: u32 },
    Chown { path: String, owner: String, group: String },
    Remove { path: String, is_dir: bool },
}

/// Dialogo flutuante para uma operacao sobre um arquivo/pasta remoto.
enum FsDialog {
    Rename {
        path: String,
        name: String,
    },
    Chmod {
        path: String,
        name: String,
        /// Modo POSIX atual (fonte de verdade para checkboxes e campo numerico).
        mode: u32,
        /// Campo numerico editavel (octal), sincronizado com `mode`.
        mode_text: String,
    },
    Chown {
        path: String,
        name: String,
        /// Campos editaveis: nome (ex.: "root") ou id numerico.
        owner_text: String,
        group_text: String,
    },
    Delete {
        path: String,
        name: String,
        is_dir: bool,
    },
}

/// Resultado de um quadro do navegador SFTP.
struct ExplorerOut {
    /// Diretorios a listar (apos navegar ou pedir refresh).
    to_list: Vec<String>,
    /// Verdadeiro se o botao de atualizar do cabecalho foi clicado.
    refresh: bool,
    /// Operacao de gerenciamento confirmada num dialogo (renomear/chmod/excluir).
    op: Option<FsOp>,
    /// Verdadeiro se alguma linha foi clicada (o painel deve tomar o foco).
    clicked_row: bool,
}

/// Estado do navegador de arquivos SFTP de um painel: mostra apenas o conteudo
/// do diretorio atual; navegar entra/sai de pastas (estilo file explorer).
struct FileExplorer {
    /// Diretorio sendo exibido; vazio ate o `Connected` chegar.
    cur_path: String,
    entries: Vec<FsNode>,
    error: Option<String>,
    /// Verdadeiro enquanto se aguarda a listagem do diretorio atual.
    loading: bool,
    /// Dialogo flutuante de gerenciamento (renomear/permissoes/excluir).
    dialog: Option<FsDialog>,
    /// Quando `Some`, o caminho esta sendo editado no cabecalho (texto digitado).
    editing_path: Option<String>,
    /// Pede foco para o campo de edicao do caminho no proximo quadro (uma vez).
    focus_path_edit: bool,
    /// Indice da entrada selecionada (teclado ou clique simples).
    sel: Option<usize>,
}

impl FileExplorer {
    fn new() -> Self {
        FileExplorer {
            cur_path: String::new(),
            entries: Vec::new(),
            error: None,
            loading: true,
            dialog: None,
            editing_path: None,
            focus_path_edit: false,
            sel: None,
        }
    }

    /// Aplica a listagem recebida, se for a do diretorio atualmente exibido.
    fn apply_listing(&mut self, path: &str, entries: Vec<sftp::RemoteEntry>) {
        if path != self.cur_path {
            return;
        }
        self.entries = entries
            .into_iter()
            .map(|e| FsNode {
                name: e.name,
                path: e.path,
                is_dir: e.is_dir,
                size: e.size,
                mode: e.mode,
                owner: e.owner,
                group: e.group,
                date: fmt_date(e.mtime),
            })
            .collect();
        self.loading = false;
    }

    /// Navega para `path`: limpa a lista e pede a nova listagem.
    fn navigate_to(&mut self, path: String, to_list: &mut Vec<String>) {
        self.cur_path = path.clone();
        self.entries.clear();
        self.error = None;
        self.loading = true;
        self.editing_path = None;
        self.sel = None;
        to_list.push(path);
    }

    /// Recarrega o diretorio atual (botao de refresh / tecla F5).
    fn refresh(&mut self, to_list: &mut Vec<String>) {
        if self.cur_path.is_empty() {
            return;
        }
        self.error = None;
        self.loading = true;
        to_list.push(self.cur_path.clone());
    }

    /// Desenha o navegador do diretorio atual e devolve os diretorios a carregar
    /// (apos navegar) e se o usuario pediu refresh pelo botao do cabecalho.
    /// Com `has_focus`, trata a navegacao por teclado: setas selecionam, Enter
    /// abre a pasta, Backspace volta, F2 renomeia e Delete exclui.
    fn ui(&mut self, ui: &mut egui::Ui, id_salt: impl std::hash::Hash, has_focus: bool) -> ExplorerOut {
        let mut to_list: Vec<String> = Vec::new();
        let mut navigate: Option<String> = None;
        let mut refresh = false;
        let mut new_dialog: Option<FsDialog> = None;
        let mut clicked_row = false;

        // --- Teclado (somente com o painel em foco e sem dialogo/edicao) ---
        let mut sel_changed = false;
        if has_focus && self.dialog.is_none() && self.editing_path.is_none() {
            use egui::{Key, Modifiers};
            let (mv, open, back, rename, del) = ui.input_mut(|i| {
                let mut mv = 0i32;
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    mv += 1;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    mv -= 1;
                }
                (
                    mv,
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Backspace),
                    i.consume_key(Modifiers::NONE, Key::F2),
                    i.consume_key(Modifiers::NONE, Key::Delete),
                )
            });
            if mv != 0 && !self.entries.is_empty() {
                let last = self.entries.len() - 1;
                let next = match (self.sel, mv) {
                    (None, m) if m > 0 => 0,
                    (None, _) => last,
                    (Some(s), m) if m > 0 => (s + 1).min(last),
                    (Some(s), _) => s.saturating_sub(1),
                };
                self.sel = Some(next);
                sel_changed = true;
            }
            if back {
                if let Some(parent) = parent_path(&self.cur_path) {
                    navigate = Some(parent);
                }
            }
            if let Some(s) = self.sel.filter(|&s| s < self.entries.len()) {
                let node = &self.entries[s];
                if open && node.is_dir {
                    navigate = Some(node.path.clone());
                }
                if rename {
                    new_dialog = Some(FsDialog::Rename {
                        path: node.path.clone(),
                        name: node.name.clone(),
                    });
                }
                if del {
                    new_dialog = Some(FsDialog::Delete {
                        path: node.path.clone(),
                        name: node.name.clone(),
                        is_dir: node.is_dir,
                    });
                }
            }
        }

        // Cabecalho: caminho do diretorio atual e botao de atualizar (direita).
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(3.0);
            ui.add(
                egui::Image::new(ICON_FOLDER_LOCK)
                    .fit_to_exact_size(egui::vec2(16.0, 16.0))
                    .tint(ACCENT),
            );
            // O botao de refresh fica a direita; reservamos o espaco dele antes
            // para que o caminho/edicao ocupe o restante da largura.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(5.0);
                let btn = egui::ImageButton::new(
                    egui::Image::new(ICON_FOLDER_SYNC)
                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                        .tint(ACCENT),
                )
                .frame(false);
                if ui.add(btn).on_hover_text("Atualizar (F5)").clicked() {
                    refresh = true;
                }

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let mut close_edit = false;
                    let mut commit: Option<String> = None;
                    if let Some(text) = &mut self.editing_path {
                        // Modo edicao: campo de texto que ocupa a largura restante.
                        // Cores explicitas para nao depender do estado do tema.
                        let resp = ui.add_sized(
                            [ui.available_width(), 22.0],
                            egui::TextEdit::singleline(text)
                                .hint_text("/caminho/da/pasta")
                                .background_color(FIELD_BG)
                                .text_color(TEXT),
                        );
                        // Pede foco apenas no primeiro quadro apos abrir. Pedir
                        // foco todo quadro re-captura o foco no mesmo frame do
                        // Enter e impede o `lost_focus()` de disparar.
                        if self.focus_path_edit {
                            resp.request_focus();
                            self.focus_path_edit = false;
                        }
                        if resp.lost_focus() {
                            // Enter confirma; qualquer outra perda de foco (Esc ou
                            // clique fora) cancela a edicao.
                            if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                                let alvo = text.trim().to_string();
                                if !alvo.is_empty() {
                                    commit = Some(alvo);
                                }
                            }
                            close_edit = true;
                        }
                    } else {
                        // Modo exibicao: rotulo clicavel que abre a edicao.
                        let atual = if self.cur_path.is_empty() {
                            "/"
                        } else {
                            self.cur_path.as_str()
                        };
                        let resp = ui
                            .add(
                                egui::Label::new(egui::RichText::new(atual).color(TEXT_WEAK))
                                    .sense(egui::Sense::click())
                                    .truncate(),
                            )
                            .on_hover_text("Clique para editar o caminho")
                            .on_hover_cursor(egui::CursorIcon::Text);
                        if resp.clicked() {
                            self.editing_path = Some(self.cur_path.clone());
                            self.focus_path_edit = true;
                        }
                    }
                    if let Some(alvo) = commit {
                        navigate = Some(alvo);
                    }
                    if close_edit {
                        self.editing_path = None;
                    }
                });
            });
        });
        // Erros de operacao (listar/renomear/excluir...) numa faixa visivel,
        // com botao para dispensar.
        if let Some(err) = self.error.clone() {
            ui.add_space(4.0);
            egui::Frame::NONE
                .fill(DANGER.gamma_multiply(0.15))
                .stroke(egui::Stroke::new(1.0, DANGER))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(err).color(ERROR_FG));
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui
                                    .add(egui::Button::new("\u{2715}").frame(false))
                                    .on_hover_text("Dispensar")
                                    .clicked()
                                {
                                    self.error = None;
                                }
                            },
                        );
                    });
                });
        }
        ui.add_space(4.0);
        ui.separator();

        // Cabecalho das colunas, alinhado com as larguras usadas em file_row.
        {
            let (hrect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), 16.0),
                egui::Sense::hover(),
            );
            let font = egui::FontId::proportional(10.5);
            let pad = 6.0;
            let cy = hrect.center().y;
            ui.painter().text(
                egui::pos2(hrect.left() + pad + 24.0, cy),
                egui::Align2::LEFT_CENTER,
                "Nome",
                font.clone(),
                TEXT_WEAK,
            );
            let mut x = hrect.right() - pad;
            for (texto, w) in [
                ("Modificado", 78.0),
                ("Tamanho", 64.0),
                ("Grupo", 84.0),
                ("Dono", 84.0),
                ("Perm", 46.0),
            ] {
                ui.painter().text(
                    egui::pos2(x, cy),
                    egui::Align2::RIGHT_CENTER,
                    texto,
                    font.clone(),
                    TEXT_WEAK,
                );
                x -= w;
            }
        }

        egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(4.0);

                // Primeira linha: voltar ao diretorio anterior (pasta acima).
                if let Some(parent) = parent_path(&self.cur_path) {
                    let up = file_row(ui, ICON_FOLDER_UP, "..", ACCENT, None, false);
                    if up.clicked() {
                        clicked_row = true;
                    }
                    if up.double_clicked() {
                        navigate = Some(parent);
                    }
                }

                if self.loading {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        ui.add(egui::Spinner::new().size(16.0));
                        ui.label(egui::RichText::new("carregando...").weak());
                    });
                } else if self.entries.is_empty() {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new("(pasta vazia)").small().weak());
                    });
                }

                let mut click_sel: Option<usize> = None;
                for (idx, node) in self.entries.iter().enumerate() {
                    let (icon, color, size) = if node.is_dir {
                        (ICON_FOLDER, ACCENT, None)
                    } else {
                        (ICON_FILE, CARD_TEXT, Some(node.size))
                    };
                    let cols = RowCols {
                        mode: node.mode,
                        owner: &node.owner,
                        group: &node.group,
                        date: &node.date,
                        size,
                    };
                    let selected = self.sel == Some(idx);
                    let resp = file_row(ui, icon, &node.name, color, Some(cols), selected);
                    // Mantem a linha selecionada visivel ao navegar com as setas.
                    if selected && sel_changed {
                        resp.scroll_to_me(None);
                    }
                    // Clique simples seleciona (e o painel toma o foco do teclado).
                    if resp.clicked() {
                        click_sel = Some(idx);
                        clicked_row = true;
                    }
                    // Duplo clique numa pasta entra nela.
                    if node.is_dir && resp.double_clicked() {
                        navigate = Some(node.path.clone());
                    }
                    // Menu de contexto: renomear / permissoes / excluir.
                    resp.context_menu(|ui| {
                        let bg = style_context_menu(ui);
                        ui.label(
                            egui::RichText::new(elide(&node.name, 24)).small().color(TEXT_WEAK),
                        );
                        ui.add_space(2.0);
                        if menu_item(ui, ICON_PEN, "Renomear", CARD_TEXT) {
                            new_dialog = Some(FsDialog::Rename {
                                path: node.path.clone(),
                                name: node.name.clone(),
                            });
                            ui.close_menu();
                        }
                        if menu_item(ui, ICON_SETTINGS, "Permissoes", CARD_TEXT) {
                            new_dialog = Some(FsDialog::Chmod {
                                path: node.path.clone(),
                                name: node.name.clone(),
                                mode: node.mode,
                                mode_text: format!("{:04o}", node.mode),
                            });
                            ui.close_menu();
                        }
                        if menu_item(ui, ICON_USERS, "Proprietario/Grupo", CARD_TEXT) {
                            new_dialog = Some(FsDialog::Chown {
                                path: node.path.clone(),
                                name: node.name.clone(),
                                owner_text: node.owner.clone(),
                                group_text: node.group.clone(),
                            });
                            ui.close_menu();
                        }
                        ui.add_space(2.0);
                        ui.separator();
                        ui.add_space(2.0);
                        if menu_item(ui, ICON_TRASH, "Excluir", DANGER) {
                            new_dialog = Some(FsDialog::Delete {
                                path: node.path.clone(),
                                name: node.name.clone(),
                                is_dir: node.is_dir,
                            });
                            ui.close_menu();
                        }
                        paint_menu_bg(ui, bg);
                    });
                }
                if let Some(s) = click_sel {
                    self.sel = Some(s);
                }
            });

        if let Some(d) = new_dialog {
            self.dialog = Some(d);
        }
        if let Some(p) = navigate {
            self.navigate_to(p, &mut to_list);
        }

        // Dialogo de gerenciamento (se aberto) pode produzir uma operacao.
        let op = self.show_dialog(ui.ctx());

        ExplorerOut { to_list, refresh, op, clicked_row }
    }

    /// Renderiza o dialogo de gerenciamento aberto (se houver) e devolve a
    /// operacao confirmada. Fecha o dialogo ao confirmar ou cancelar.
    fn show_dialog(&mut self, ctx: &egui::Context) -> Option<FsOp> {
        let Some(mut dialog) = self.dialog.take() else {
            return None;
        };
        let cur = self.cur_path.clone();
        let mut op: Option<FsOp> = None;
        let mut keep = true;

        // Esc cancela qualquer dialogo aberto (consumido para nao vazar).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            return None;
        }

        let titulo = match &dialog {
            FsDialog::Rename { .. } => "Renomear",
            FsDialog::Chmod { .. } => "Permissões",
            FsDialog::Chown { .. } => "Proprietário/Grupo",
            FsDialog::Delete { .. } => "Excluir",
        };
        let frame = egui::Frame::window(&ctx.style())
            .fill(CARD_BG)
            .stroke(egui::Stroke::new(1.0, CARD_BORDER))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(18));

        egui::Window::new(egui::RichText::new(titulo).color(ACCENT).strong())
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                match &mut dialog {
                    FsDialog::Rename { path, name } => {
                        ui.label(egui::RichText::new("Novo nome").small().color(TEXT_WEAK));
                        let r = ui.add(
                            egui::TextEdit::singleline(name).desired_width(300.0),
                        );
                        // Foca apenas quando nada tem o foco (pedir todo quadro
                        // impediria o lost_focus() do Enter de disparar).
                        if ui.memory(|m| m.focused().is_none()) {
                            r.request_focus();
                        }
                        // Enter confirma (mesmo efeito do botao Renomear).
                        let enter =
                            r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if (accent_btn(ui, "Renomear") || enter)
                                && !name.trim().is_empty()
                            {
                                op = Some(FsOp::Rename {
                                    from: path.clone(),
                                    to: join_remote(&cur, name.trim()),
                                });
                                keep = false;
                            }
                            if ghost_btn(ui, "Cancelar") {
                                keep = false;
                            }
                        });
                    }
                    FsDialog::Chmod {
                        path,
                        name,
                        mode,
                        mode_text,
                    } => {
                        ui.label(
                            egui::RichText::new(name.as_str())
                                .size(16.0)
                                .color(HIGHLIGHT),
                        );
                        ui.add_space(8.0);

                        // Tres classes de permissao, cada uma com Ler/Gravar/Executar.
                        perm_class(ui, mode, "Proprietário", 0o400, 0o200, 0o100);
                        ui.add_space(4.0);
                        perm_class(ui, mode, "Grupo", 0o040, 0o020, 0o010);
                        ui.add_space(4.0);
                        perm_class(ui, mode, "Público", 0o004, 0o002, 0o001);

                        ui.add_space(10.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new("Numérico:").color(TEXT_WEAK));
                            let resp = ui.add(
                                egui::TextEdit::singleline(mode_text).desired_width(80.0),
                            );
                            // Campo numerico edita o modo; checkboxes refletem nele.
                            if resp.changed() {
                                if let Ok(v) = u32::from_str_radix(mode_text.trim(), 8) {
                                    *mode = v & 0o7777;
                                }
                            }
                            // Fora de edicao, o texto acompanha os checkboxes.
                            if !resp.has_focus() {
                                *mode_text = format!("{:04o}", *mode);
                            }
                        });

                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if accent_btn(ui, "Aplicar") {
                                op = Some(FsOp::Chmod {
                                    path: path.clone(),
                                    mode: *mode,
                                });
                                keep = false;
                            }
                            if ghost_btn(ui, "Cancelar") {
                                keep = false;
                            }
                        });
                    }
                    FsDialog::Chown {
                        path,
                        name,
                        owner_text,
                        group_text,
                    } => {
                        ui.label(
                            egui::RichText::new(format!("Proprietario/grupo de \"{name}\"."))
                                .color(TEXT_WEAK),
                        );
                        ui.add_space(8.0);
                        egui::Grid::new("chown_grid")
                            .num_columns(2)
                            .spacing([10.0, 6.0])
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Proprietario").color(TEXT_WEAK));
                                ui.add(
                                    egui::TextEdit::singleline(owner_text).desired_width(160.0),
                                );
                                ui.end_row();

                                ui.label(egui::RichText::new("Grupo").color(TEXT_WEAK));
                                ui.add(
                                    egui::TextEdit::singleline(group_text).desired_width(160.0),
                                );
                                ui.end_row();
                            });
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Aceita nome (ex.: root) ou id numerico.")
                                .small()
                                .weak(),
                        );

                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if accent_btn(ui, "Aplicar")
                                && !owner_text.trim().is_empty()
                                && !group_text.trim().is_empty()
                            {
                                op = Some(FsOp::Chown {
                                    path: path.clone(),
                                    owner: owner_text.trim().to_string(),
                                    group: group_text.trim().to_string(),
                                });
                                keep = false;
                            }
                            if ghost_btn(ui, "Cancelar") {
                                keep = false;
                            }
                        });
                    }
                    FsDialog::Delete {
                        path,
                        name,
                        is_dir,
                    } => {
                        let tipo = if *is_dir { "a pasta" } else { "o arquivo" };
                        ui.label(
                            egui::RichText::new(format!("Excluir {tipo}:"))
                                .color(TEXT_WEAK),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(format!("{name}"))
                                .size(14.0)
                                .color(HIGHLIGHT),
                        );
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("Esta ação é permanente.")
                                .color(TEXT_WEAK),
                        );
                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            if danger_btn(ui, "Excluir") {
                                op = Some(FsOp::Remove {
                                    path: path.clone(),
                                    is_dir: *is_dir,
                                });
                                keep = false;
                            }
                            if ghost_btn(ui, "Cancelar") {
                                keep = false;
                            }
                        });
                    }
                }
            });

        if keep && op.is_none() {
            self.dialog = Some(dialog);
        }
        op
    }
}

/// Junta um diretorio e um nome em um caminho remoto POSIX.
fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Linha de uma classe de permissao (proprietario/grupo/publico) com os tres
/// checkboxes Ler/Gravar/Executar, ligados aos bits indicados de `mode`.
fn perm_class(ui: &mut egui::Ui, mode: &mut u32, label: &str, r: u32, w: u32, x: u32) {
    ui.label(egui::RichText::new(label).color(TEXT_WEAK));
    ui.horizontal(|ui| {
        for (bit, texto) in [(r, "Ler"), (w, "Gravar"), (x, "Executar")] {
            let mut on = *mode & bit != 0;
            if perm_checkbox(ui, &mut on, texto) {
                if on {
                    *mode |= bit;
                } else {
                    *mode &= !bit;
                }
            }
        }
    });
}

/// Checkbox desenhado manualmente para permitir cores distintas entre o "✓"
/// (`CHECK_COLOR`) e o texto do label (`TEXT`, ou `TEXT_HOVER` sob o mouse).
/// Retorna `true` se o estado mudou.
fn perm_checkbox(ui: &mut egui::Ui, on: &mut bool, text: &str) -> bool {
    let font = egui::FontId::proportional(14.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), TEXT);
    let box_sz = 16.0;
    let gap = 6.0;
    let w = box_sz + gap + galley.size().x;
    let h = box_sz.max(galley.size().y);
    let (rect, response) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());

    let mut changed = false;
    if response.clicked() {
        *on = !*on;
        changed = true;
    }
    let hovered = response.hovered();

    // Quadradinho.
    let box_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left(), rect.center().y - box_sz / 2.0),
        egui::vec2(box_sz, box_sz),
    );
    let (fill, border) = if hovered {
        (WIDGET_BG_HOVER, ACCENT)
    } else {
        (WIDGET_BG, CARD_BORDER)
    };
    ui.painter().rect(
        box_rect,
        3.0,
        fill,
        egui::Stroke::new(1.0, border),
        egui::StrokeKind::Inside,
    );

    // "✓" na cor propria (CHECK_COLOR), independente do label.
    if *on {
        let s = egui::Stroke::new(2.0, CHECK_COLOR);
        ui.painter().line_segment(
            [
                egui::pos2(box_rect.left() + 3.5, box_rect.center().y),
                egui::pos2(box_rect.center().x - 1.0, box_rect.bottom() - 4.0),
            ],
            s,
        );
        ui.painter().line_segment(
            [
                egui::pos2(box_rect.center().x - 1.0, box_rect.bottom() - 4.0),
                egui::pos2(box_rect.right() - 3.0, box_rect.top() + 4.0),
            ],
            s,
        );
    }

    // Label com cor propria (muda no hover).
    let label_color = if hovered { TEXT_HOVER } else { TEXT };
    ui.painter().text(
        egui::pos2(box_rect.right() + gap, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        font,
        label_color,
    );

    changed
}

/// Estilo de um botao pintado manualmente (cores por estado de interacao).
struct BtnStyle {
    fill: egui::Color32,
    fill_hover: egui::Color32,
    text: egui::Color32,
    text_hover: egui::Color32,
    stroke: Option<egui::Color32>,
    stroke_hover: Option<egui::Color32>,
}

/// Botao primario (grafite medio): clareia no hover, com borda de aco para
/// destacar da superficie do cartao.
const BTN_ACCENT: BtnStyle = BtnStyle {
    fill: ACCENT_FILL,
    fill_hover: ACCENT_FILL_HOVER,
    text: egui::Color32::WHITE,
    text_hover: egui::Color32::WHITE,
    stroke: Some(hex("#5b6270")),
    stroke_hover: Some(ACCENT),
};

/// Botao secundario discreto: fundo sutil e borda que acende no hover.
const BTN_GHOST: BtnStyle = BtnStyle {
    fill: CARD_BG,
    fill_hover: hex("#2c2c33"),
    text: TEXT_WEAK,
    text_hover: CARD_TEXT,
    stroke: Some(CARD_BORDER),
    stroke_hover: Some(ACCENT),
};

/// Botao destrutivo (vermelho).
const BTN_DANGER: BtnStyle = BtnStyle {
    fill: DANGER,
    fill_hover: hex("#f2a3a3"),
    text: egui::Color32::WHITE,
    text_hover: egui::Color32::WHITE,
    stroke: None,
    stroke_hover: None,
};

/// Botao pintado manualmente: fundo/borda/texto reagem ao hover e ao clique,
/// com cursor de mao — os botoes com `Button::fill` fixo do egui nao mudam de
/// estado visual. Retorna `true` quando clicado.
fn painted_btn(
    ui: &mut egui::Ui,
    size: egui::Vec2,
    text: &str,
    font_size: f32,
    s: &BtnStyle,
) -> bool {
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let pressed = response.is_pointer_button_down_on();
    let hovered = response.hovered() || pressed;
    let fill = if pressed {
        s.fill_hover.gamma_multiply(0.85)
    } else if hovered {
        s.fill_hover
    } else {
        s.fill
    };
    let stroke = match if hovered { s.stroke_hover } else { s.stroke } {
        Some(c) => egui::Stroke::new(1.0, c),
        None => egui::Stroke::NONE,
    };
    let painter = ui.painter();
    painter.rect(rect, 6.0, fill, stroke, egui::StrokeKind::Inside);
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        egui::FontId::proportional(font_size),
        if hovered { s.text_hover } else { s.text },
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// Botao primario (acento) compacto para os dialogos.
fn accent_btn(ui: &mut egui::Ui, text: &str) -> bool {
    painted_btn(ui, egui::vec2(100.0, 28.0), text, 14.0, &BTN_ACCENT)
}

/// Botao secundario discreto para os dialogos.
fn ghost_btn(ui: &mut egui::Ui, text: &str) -> bool {
    painted_btn(ui, egui::vec2(100.0, 28.0), text, 14.0, &BTN_GHOST)
}

/// Botao destrutivo (vermelho) para os dialogos.
fn danger_btn(ui: &mut egui::Ui, text: &str) -> bool {
    painted_btn(ui, egui::vec2(100.0, 28.0), text, 14.0, &BTN_DANGER)
}

/// Desenha uma linha clicavel do navegador (icone + nome + tamanho opcional a
/// direita), realcando o fundo ao passar o mouse. Retorna a resposta para que o
/// chamador trate o duplo clique.
/// Colunas de metadados exibidas a direita de uma entrada na listagem SFTP.
struct RowCols<'a> {
    mode: u32,
    owner: &'a str,
    group: &'a str,
    /// Data ja formatada (DD/MM/AAAA) ou vazia.
    date: &'a str,
    /// Tamanho (apenas arquivos); `None` para pastas.
    size: Option<u64>,
}

fn file_row(
    ui: &mut egui::Ui,
    icon: egui::ImageSource,
    name: &str,
    color: egui::Color32,
    cols: Option<RowCols>,
    selected: bool,
) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 24.0), egui::Sense::click());

    if selected {
        ui.painter()
            .rect_filled(rect, 4.0, ACCENT.gamma_multiply(0.30));
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 4.0, ACCENT.gamma_multiply(0.18));
    }

    let pad = 6.0;
    let cy = rect.center().y;
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + pad, cy - 8.0),
        egui::vec2(16.0, 16.0),
    );
    egui::Image::new(icon).tint(color).paint_at(ui, icon_rect);

    // Colunas de metadados a direita (permissoes | proprietario | grupo |
    // tamanho | data), cada uma com largura fixa, alinhadas a direita.
    let mut x = rect.right() - pad;
    if let Some(c) = &cols {
        let size_str = c.size.map(human_size).unwrap_or_default();
        let columns: [(String, f32, bool); 5] = [
            (c.date.to_string(), 78.0, false),
            (size_str, 64.0, false),
            (c.group.to_string(), 84.0, false),
            (c.owner.to_string(), 84.0, false),
            (format!("{:04o}", c.mode), 46.0, true),
        ];
        for (text, w, mono) in columns {
            if !text.is_empty() {
                let font = if mono {
                    egui::FontId::monospace(11.0)
                } else {
                    egui::FontId::proportional(11.0)
                };
                ui.painter().text(
                    egui::pos2(x, cy),
                    egui::Align2::RIGHT_CENTER,
                    elide(&text, 12),
                    font,
                    TEXT_WEAK,
                );
            }
            x -= w;
        }
    }

    // Nome (a esquerda), truncado para nao invadir as colunas da direita.
    let name_left = icon_rect.right() + 8.0;
    let avail = (x - 8.0 - name_left).max(24.0);
    let max_chars = (avail / 7.0) as usize;
    ui.painter().text(
        egui::pos2(name_left, cy),
        egui::Align2::LEFT_CENTER,
        elide(name, max_chars.max(3)),
        egui::FontId::proportional(13.0),
        color,
    );

    response
}

/// Formata um timestamp Unix (segundos, UTC) como "DD/MM/AAAA". Vazio se 0.
fn fmt_date(secs: u32) -> String {
    if secs == 0 {
        return String::new();
    }
    // Dias desde a epoca -> data civil (algoritmo de Howard Hinnant).
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:02}/{:02}/{:04}", d, m, y)
}

/// Caminho do diretorio pai (estilo POSIX), ou `None` se ja for a raiz.
fn parent_path(p: &str) -> Option<String> {
    if p.is_empty() || p == "/" {
        return None;
    }
    let trimmed = p.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(idx) => Some(trimmed[..idx].to_string()),
        None => None,
    }
}

/// Formata um tamanho em bytes de forma legivel (B, KB, MB, GB).
fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
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
    /// Abrir um terminal local (cmd.exe ou WSL) no painel indicado.
    OpenLocal { path: Vec<usize>, shell: pty::LocalShell },
    /// Abrir um navegador SFTP no painel indicado para o host dado.
    Sftp { path: Vec<usize>, host: usize },
    /// Abrir o editor para o host indicado (a partir do seletor de um painel).
    Edit { host: usize },
    /// Abrir o editor para cadastrar um host novo (a partir do seletor).
    NewHost,
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

/// Versao somente-leitura de `node_at_mut`.
fn node_at<'a>(root: &'a Node, path: &[usize]) -> Option<&'a Node> {
    let mut cur = root;
    for &i in path {
        match cur {
            Node::Split { children, .. } => cur = children.get(i)?,
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
const ICON_FOLDER_LOCK: egui::ImageSource = egui::include_image!("../assets/folder-lock.svg");
const ICON_FOLDER: egui::ImageSource = egui::include_image!("../assets/folder.svg");
const ICON_FILE: egui::ImageSource = egui::include_image!("../assets/file.svg");
const ICON_FOLDER_UP: egui::ImageSource = egui::include_image!("../assets/folder-up.svg");
const ICON_FOLDER_SYNC: egui::ImageSource = egui::include_image!("../assets/folder-sync.svg");
const ICON_PEN: egui::ImageSource = egui::include_image!("../assets/pen.svg");
const ICON_USERS: egui::ImageSource = egui::include_image!("../assets/users.svg");
const ICON_PLUS: egui::ImageSource = egui::include_image!("../assets/plus.svg");
const ICON_KEY: egui::ImageSource = egui::include_image!("../assets/key-round.svg");
const ICON_PASSWORD: egui::ImageSource =
    egui::include_image!("../assets/rectangle-ellipsis.svg");

/// Converte uma string hexadecimal (`"#332847"` ou `"332847"`) em
/// `egui::Color32`. E uma `const fn`, entao pode ser usada tanto em constantes
/// quanto em tempo de execucao. Em caso de string invalida, retorna magenta
/// (`#ff00ff`) para destacar o erro visualmente.
///
/// # Exemplo
/// ```ignore
/// const BORDA: egui::Color32 = hex("#34343b");
/// let cor = hex("9ba3b4");
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

// ---------------------------------------------------------------------------
// Paleta grafite
//
// Escala neutra construida a partir de #1b1b1f (base dos paineis) e #1f1f1f
// (um degrau acima, usado nas superficies elevadas: cartoes, dialogos e barra
// de titulo). Os tons claros levam um leve viés frio (aco) para separar da
// escala de cinza puro; as cores semanticas (verde/ambar/vermelho) permanecem
// como unicos pontos de cor, para status continuar legivel de relance.
// ---------------------------------------------------------------------------

/// Fundo geral das telas: o degrau mais escuro, atras dos paineis.
const SCREEN_BG: egui::Color32 = hex("#0f0f12");
/// Fundo de campos de texto e areas "afundadas".
const FIELD_BG: egui::Color32 = hex("#121216");
/// Fundo de menus de contexto, tooltips e popups.
const MENU_BG: egui::Color32 = hex("#232328");
/// Fundo de superficies elevadas: cartoes, dialogos e janelas flutuantes.
const CARD_BG: egui::Color32 = hex("#1f1f1f");
/// Fundo da barra de titulo de cada painel (mesmo degrau dos cartoes).
const TITLE_BG: egui::Color32 = hex("#1f1f1f");
/// Borda dos cartoes, dialogos e campos.
const CARD_BORDER: egui::Color32 = hex("#34343b");
/// Fundo de widgets pintados a mao em repouso (ex.: caixa do checkbox).
const WIDGET_BG: egui::Color32 = hex("#26262b");
/// Fundo de widgets pintados a mao sob o mouse.
const WIDGET_BG_HOVER: egui::Color32 = hex("#33333a");

/// Cor da borda dos paineis.
const PANE_BORDER: egui::Color32 = hex("#2e2e34");
/// Borda do painel com foco do teclado: mesma familia grafite, bem mais clara
/// para deixar obvio onde a digitacao vai cair.
const PANE_BORDER_FOCUS: egui::Color32 = hex("#6e7684");

/// Acento do tema (aco claro): icones, titulos, bordas de realce e links.
/// Claro o bastante para ler sobre os fundos grafite.
const ACCENT: egui::Color32 = hex("#9ba3b4");
/// Preenchimento dos botoes/abas primarios — escuro o suficiente para texto
/// branco por cima (o `ACCENT` claro serviria de borda, nao de fundo).
const ACCENT_FILL: egui::Color32 = hex("#454b57");
/// Preenchimento primario sob o mouse.
const ACCENT_FILL_HOVER: egui::Color32 = hex("#565e6c");

/// Cor dos icones do cabecalho (igual a do texto secundario).
const ICON_TINT: egui::Color32 = hex("#a0a0aa");
/// Texto secundario (cinza neutro): da hierarquia entre titulo e apoio.
const TEXT_WEAK: egui::Color32 = hex("#a0a0aa");
/// Texto principal (claro), para itens de menu/destaques sobre fundo escuro.
const CARD_TEXT: egui::Color32 = hex("#e3e3e8");
/// Cor normal do texto (label) dos checkboxes de permissao.
const TEXT: egui::Color32 = hex("#ffffff");
/// Cor do texto (label) dos checkboxes de permissao ao passar o mouse.
const TEXT_HOVER: egui::Color32 = hex("#e2c34f");
/// Cor do "✓" desenhado dentro do quadradinho (independente da cor do label).
const CHECK_COLOR: egui::Color32 = hex("#bfc7d3");

// --- Cores semanticas (unicos pontos de cor da interface) ---
/// Vermelho de mensagens de erro (mais vivo que DANGER, usado em botoes).
const ERROR_FG: egui::Color32 = hex("#ff6b6b");
/// Vermelho suave para acoes destrutivas (ex.: Excluir).
const DANGER: egui::Color32 = hex("#e88a8a");
/// Verde para indicar autenticacao por chave.
const AUTH_KEY: egui::Color32 = hex("#6ed69a");
/// Ambar para indicar autenticacao por senha.
const AUTH_PASS: egui::Color32 = hex("#e2b34f");
/// Ambar de destaque para o nome do item nos dialogos.
const HIGHLIGHT: egui::Color32 = hex("#e2c34f");

/// Formulario de edicao/criacao de host.
struct HostEditor {
    /// Id do host em edicao (`None` = criacao). Usa o id estavel em vez do
    /// indice na lista: excluir/reordenar hosts com o editor aberto nao pode
    /// gravar os dados sobre o host errado.
    id: Option<uuid::Uuid>,
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
            id: None,
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

    fn from_host(h: &Host) -> Self {
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
            id: Some(h.id),
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

    // Texto de busca para filtrar a listagem de hosts (pelo nome da conexao).
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

    // Abertura/criacao do cofre em andamento (derivar a chave Argon2 e lento e
    // roda na thread da UI): 1 = mostrar "abrindo..." neste quadro; 2 = fazer
    // o trabalho no inicio do proximo quadro (com o spinner ja na tela).
    gate_busy: u8,

    // Caminho do painel cujo terminal tem o foco do teclado (atualizado a cada
    // quadro durante a renderizacao da sessao).
    focused_path: Option<Vec<usize>>,

    // Instante em que Ctrl+B foi pressionado, aguardando a segunda tecla do
    // atalho (H/V divide, setas trocam o foco, O cicla, X fecha, A = ajuda).
    // `None` = prefixo desarmado; expira sozinho apos alguns segundos.
    chord_armed_at: Option<Instant>,

    // Retangulo de cada painel (folha) no ultimo quadro, na ordem de
    // renderizacao; usado pelos atalhos de troca de foco direcional/ciclica.
    pane_rects: Vec<(Vec<usize>, egui::Rect)>,

    // Painel que deve receber o foco do teclado no proximo quadro (definido
    // pelos atalhos Ctrl+B ou ao criar um painel por divisao).
    pending_focus: Option<Vec<usize>>,

    // Ultimo painel que teve o foco do teclado; recebe o foco de volta quando
    // nenhum widget o detem (ex.: apos clicar num cartao do seletor).
    last_pane_focus: Option<Vec<usize>>,

    // Janela flutuante com a lista de atalhos de teclado (F1 / Ctrl+B, A).
    show_help: bool,

    // Host aguardando confirmacao de exclusao (dialogo flutuante).
    pending_delete: Option<uuid::Uuid>,
}

/// Nome de exibicao de um host: o apelido, ou o endereco quando sem apelido.
fn display_name(host: &Host) -> String {
    if host.name.trim().is_empty() {
        host.host.clone()
    } else {
        host.name.clone()
    }
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
            gate_busy: 0,
            focused_path: None,
            chord_armed_at: None,
            pane_rects: Vec::new(),
            pending_focus: None,
            last_pane_focus: None,
            show_help: false,
            pending_delete: None,
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
            // Dica de que a splash pode ser pulada (aparece apos um instante).
            if elapsed > 0.8 {
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new("clique para continuar")
                        .small()
                        .color(TEXT_WEAK),
                );
            }
        });
    }

    fn ensure_logo(&mut self, ctx: &egui::Context) {
        if self.logo_load_attempted {
            return;
        }
        self.logo_load_attempted = true;
        // Logo embutido no binario: o .exe roda sozinho, sem a pasta assets/.
        let bytes = include_bytes!("../assets/logo.png");
        if let Ok(img) = image::load_from_memory(bytes) {
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
        // Escrita atomica: grava num arquivo temporario ao lado e renomeia por
        // cima. Uma falha no meio (queda de energia, disco cheio) nunca deixa
        // o cofre — unico arquivo com todas as credenciais — corrompido.
        let tmp = path.with_extension("sagu.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn lock(&mut self) {
        if let Some(root) = &self.root {
            disconnect_tree(root);
        }
        self.root = None;
        self.last_pane_focus = None;
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
                        "Abra um cofre existente ou crie um para guardar suas conexões.",
                    )
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
                ui.label(egui::RichText::new("Arquivo do cofre").color(TEXT_WEAK));
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
                ui.label(egui::RichText::new("Senha mestra").color(TEXT_WEAK));
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
                        egui::RichText::new("Confirmar senha").color(TEXT_WEAK),
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
                    ui.colored_label(ERROR_FG, err);
                }

                ui.add_space(16.0);

                // Botao de acao principal, ocupando toda a largura. Enquanto
                // abre/cria (Argon2 e lento), vira um indicador de progresso.
                if self.gate_busy > 0 {
                    let (r, _) = ui.allocate_exact_size(
                        egui::vec2(FIELD_W, 34.0),
                        egui::Sense::hover(),
                    );
                    let texto = if self.gate_mode == GateMode::Open {
                        "Abrindo cofre..."
                    } else {
                        "Criando cofre..."
                    };
                    ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| {
                        ui.horizontal_centered(|ui| {
                            ui.add_space(FIELD_W / 2.0 - 70.0);
                            ui.add(egui::Spinner::new().size(18.0));
                            ui.label(egui::RichText::new(texto).color(TEXT_WEAK));
                        });
                    });
                } else {
                    let action_label = if self.gate_mode == GateMode::Open {
                        "Abrir cofre"
                    } else {
                        "Criar cofre"
                    };
                    if painted_btn(
                        ui,
                        egui::vec2(FIELD_W, 34.0),
                        action_label,
                        15.0,
                        &BTN_ACCENT,
                    ) || pwd_enter
                    {
                        submit = true;
                    }
                }
            });

        if submit && self.gate_busy == 0 {
            // Nao processa ja: mostra o spinner por um quadro antes de derivar
            // a chave (o trabalho pesado roda no inicio do 2º quadro adiante,
            // em `update`, com o indicador ja visivel na tela).
            self.gate_busy = 1;
            ui.ctx().request_repaint();
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
                            self.gate_error = Some(format!("Não foi possível gravar: {e}"))
                        }
                    },
                    Err(e) => self.gate_error = Some(format!("{e}")),
                }
            }
        }
    }

    // ---------------- Lista de hosts ----------------

    fn ui_hosts(&mut self, ui: &mut egui::Ui) {
        // Atalhos da tela (desativados com o editor aberto): Ctrl+N cria um
        // host novo e Ctrl+L bloqueia o cofre.
        if self.editor.is_none() {
            let (novo, bloquear) = ui.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::CTRL, egui::Key::N),
                    i.consume_key(egui::Modifiers::CTRL, egui::Key::L),
                )
            });
            if novo {
                self.hosts_error = None;
                self.editor = Some(HostEditor::new());
            }
            if bloquear {
                self.lock();
                return;
            }
        }

        // Cabecalho com titulo e acoes principais.
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.heading(egui::RichText::new("Conexões").color(ACCENT).size(26.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Bloquear: acao secundaria, estilo discreto do tema.
                if painted_btn(
                    ui,
                    egui::vec2(150.0, 30.0),
                    "\u{1f512}  Bloquear cofre",
                    14.0,
                    &BTN_GHOST,
                ) {
                    self.lock();
                }
                ui.add_space(6.0);
                // Novo host: acao primaria, preenchida com o acento.
                if painted_btn(
                    ui,
                    egui::vec2(120.0, 30.0),
                    "+  Novo host",
                    14.0,
                    &BTN_ACCENT,
                ) {
                    self.hosts_error = None;
                    self.editor = Some(HostEditor::new());
                }
            });
        });
        if let Some(path) = &self.vault_path {
            ui.label(
                egui::RichText::new(format!("\u{1f5c4}  {}", path.to_string_lossy()))
                    .color(TEXT_WEAK),
            );
        }
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(8.0);

        if let Some(err) = &self.hosts_error {
            ui.colored_label(ERROR_FG, err);
            ui.add_space(4.0);
        }

        let mut connect_index: Option<usize> = None;
        let mut sftp_index: Option<usize> = None;
        let mut edit_index: Option<usize> = None;
        let mut delete_index: Option<usize> = None;
        let mut open_local: Option<pty::LocalShell> = None;

        // Seletor de conexoes compartilhado (com busca e gerenciamento). O foco
        // automatico no campo so vale quando nao ha um host sendo editado.
        let autofocus = self.editor.is_none();
        match connection_picker(
            ui,
            &self.vault.hosts,
            &mut self.hosts_filter,
            "hosts_picker",
            PickerOpts {
                manage: true,
                autofocus,
                force_focus: false,
                closable: false,
            },
        ) {
            Some(PickerAction::OpenLocal(s)) => open_local = Some(s),
            Some(PickerAction::Connect(i)) => connect_index = Some(i),
            Some(PickerAction::Sftp(i)) => sftp_index = Some(i),
            Some(PickerAction::Edit(i)) => edit_index = Some(i),
            Some(PickerAction::NewHost) => {
                self.hosts_error = None;
                self.editor = Some(HostEditor::new());
            }
            Some(PickerAction::Delete(i)) => delete_index = Some(i),
            // Nao se aplica na tela principal (closable = false).
            Some(PickerAction::ClosePane) | None => {}
        }

        if let Some(shell) = open_local {
            self.start_local_session(shell);
        }
        if let Some(i) = edit_index {
            self.hosts_error = None;
            self.editor = Some(HostEditor::from_host(&self.vault.hosts[i]));
        }
        if let Some(i) = delete_index {
            // A exclusao pede confirmacao num dialogo (acao permanente).
            if i < self.vault.hosts.len() {
                self.pending_delete = Some(self.vault.hosts[i].id);
            }
        }
        if let Some(i) = connect_index {
            self.start_session(i);
        }
        if let Some(i) = sftp_index {
            self.start_sftp_session(i);
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

        // Esc cancela; Ctrl+Enter salva (Enter puro insere nova linha no campo
        // multiline da chave privada, por isso nao serve como confirmacao).
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                cancel = true;
            }
            if i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter) {
                save = true;
            }
        });

        let editing = editor.id.is_some();
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
                    ui.colored_label(ERROR_FG, err);
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
                    if painted_btn(ui, egui::vec2(half, 34.0), "Salvar", 15.0, &BTN_ACCENT) {
                        save = true;
                    }
                    ui.add_space(8.0);
                    if painted_btn(ui, egui::vec2(half, 34.0), "Cancelar", 15.0, &BTN_GHOST) {
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
            match editor.id {
                Some(id) => {
                    // Resolve o host pelo id no momento do save: a lista pode
                    // ter mudado (exclusoes) com o editor aberto.
                    match self.vault.hosts.iter().position(|h| h.id == id) {
                        Some(i) => self.vault.hosts[i] = editor.to_host(id),
                        None => {
                            self.hosts_error =
                                Some("Este host foi removido enquanto era editado.".into());
                            self.editor = Some(editor);
                            return;
                        }
                    }
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

    /// Abre uma nova sessao com um terminal local (cmd.exe ou WSL).
    fn start_local_session(&mut self, shell: pty::LocalShell) {
        self.root = Some(Node::Leaf(Pane::picker()));
        self.screen = Screen::Session;
        self.connect_local_pane(&[], shell);
    }

    /// Conecta o painel (folha) no caminho indicado a um terminal local
    /// (cmd.exe ou WSL, conforme `shell`).
    fn connect_local_pane(&mut self, path: &[usize], shell: pty::LocalShell) {
        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = pty::connect_local(shell, INITIAL_COLS, INITIAL_ROWS, repaint);

        if let Some(root) = &mut self.root {
            if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                pane.host_name = shell.label().to_string();
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
        let name = display_name(&host);

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

    /// Abre uma nova sessao iniciando ja com um navegador SFTP.
    fn start_sftp_session(&mut self, index: usize) {
        self.root = Some(Node::Leaf(Pane::picker()));
        self.screen = Screen::Session;
        self.connect_sftp_pane(&[], index);
    }

    /// Conecta o painel (folha) no caminho indicado a uma sessao SFTP, exibindo
    /// um navegador de arquivos em vez de um terminal.
    fn connect_sftp_pane(&mut self, path: &[usize], host_index: usize) {
        let host = self.vault.hosts[host_index].clone();
        let name = display_name(&host);

        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = sftp::connect(host, repaint);

        if let Some(root) = &mut self.root {
            if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                pane.host_name = format!("{name}  (SFTP)");
                pane.explorer = Some(FileExplorer::new());
                pane.state = SessionState::Connecting;
                pane.sftp = Some(handle);
                pane.picking = false;
            }
        }
    }

    /// Divide o painel no caminho: adiciona um irmao se o pai ja tem a mesma
    /// direcao, caso contrario transforma a folha numa nova divisao. Retorna o
    /// caminho do painel recem-criado (para receber o foco do teclado).
    fn split_pane(&mut self, path: &[usize], dir: SplitDir) -> Option<Vec<usize>> {
        let Some(root) = &mut self.root else {
            return None;
        };
        if let Some((&idx, parent_path)) = path.split_last() {
            if let Some(Node::Split {
                dir: pdir,
                children,
            }) = node_at_mut(root, parent_path)
            {
                if *pdir == dir {
                    children.insert(idx + 1, Node::Leaf(Pane::picker()));
                    let mut new_path = parent_path.to_vec();
                    new_path.push(idx + 1);
                    return Some(new_path);
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
                let mut new_path = path.to_vec();
                new_path.push(1);
                return Some(new_path);
            }
        }
        None
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
            self.last_pane_focus = None;
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
        let collapsed = collapse.is_some();
        if let Some(only) = collapse {
            if let Some(parent) = node_at_mut(root, parent_path) {
                *parent = only;
            }
        }

        // Reposiciona o foco: a arvore mudou, entao o caminho do painel focado
        // e ajustado (irmaos deslocados / divisao colapsada) e ele retoma o
        // foco; se o painel fechado era o focado, o foco vai ao primeiro.
        let target = self
            .last_pane_focus
            .as_deref()
            .and_then(|f| remap_after_close(f, parent_path, idx, collapsed))
            .or_else(|| self.first_pane_path());
        self.focused_path = None;
        self.last_pane_focus = target.clone();
        self.pending_focus = target;
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

                // Eventos da sessao SFTP (navegador de arquivos).
                let mut sftp_events = Vec::new();
                if let Some(sftp) = &pane.sftp {
                    while let Ok(ev) = sftp.from_sftp.try_recv() {
                        sftp_events.push(ev);
                    }
                }
                for ev in sftp_events {
                    match ev {
                        SftpToUi::Connected { home } => {
                            pane.state = SessionState::Connected;
                            if let (Some(exp), Some(sftp)) = (&mut pane.explorer, &pane.sftp) {
                                exp.cur_path = home.clone();
                                exp.loading = true;
                                sftp.list_dir(home);
                            }
                        }
                        SftpToUi::Listing { path, entries } => {
                            if let Some(exp) = &mut pane.explorer {
                                exp.apply_listing(&path, entries);
                            }
                        }
                        SftpToUi::Error(msg) => {
                            // Erro antes de conectar e fatal: marca o painel
                            // como erro para que o `Closed` seguinte nao o
                            // feche silenciosamente (o usuario precisa ler).
                            if matches!(pane.state, SessionState::Connecting) {
                                pane.state = SessionState::Error(msg.clone());
                            }
                            if let Some(exp) = &mut pane.explorer {
                                exp.error = Some(msg);
                                // Sem listagem a caminho: para o "carregando...".
                                exp.loading = false;
                            } else {
                                pane.state = SessionState::Error(msg);
                            }
                        }
                        SftpToUi::Closed => {
                            if !matches!(pane.state, SessionState::Error(_)) {
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

    /// Atalhos de teclado da sessao.
    ///
    /// `Alt+setas` troca o painel focado diretamente (atalho principal).
    ///
    /// O restante usa o prefixo `Ctrl+B` em dois tempos (estilo tmux): `H`/`V`
    /// dividem o painel focado, setas tambem movem o foco (alias do Alt+setas),
    /// `O` cicla, `X` fecha o painel, `A` abre a ajuda e `Ctrl+B` de novo envia
    /// um Ctrl+B literal ao terminal (util para tmux remoto). Teclas invalidas
    /// apos o prefixo sao engolidas (nao vazam para o shell) e o prefixo expira
    /// sozinho. Todos os eventos usados sao consumidos.
    fn handle_session_keys(&mut self, ctx: &egui::Context) {
        // Alt+setas trocam o painel focado direto, sem prefixo (atalho
        // principal de navegacao). Consumido aqui, antes de qualquer widget,
        // para nao chegar ao terminal como sequencia de escape.
        let mut alt_nav: Option<(i32, i32)> = None;
        ctx.input_mut(|i| {
            use egui::{Key, Modifiers};
            for (key, dir) in [
                (Key::ArrowLeft, (-1, 0)),
                (Key::ArrowRight, (1, 0)),
                (Key::ArrowUp, (0, -1)),
                (Key::ArrowDown, (0, 1)),
            ] {
                if i.consume_key(Modifiers::ALT, key) {
                    alt_nav = Some(dir);
                    break;
                }
            }
        });
        if let Some((dx, dy)) = alt_nav {
            // Cancela um prefixo pendente: a acao ja foi dada por outro caminho.
            self.chord_armed_at = None;
            self.focus_dir(dx, dy);
            return;
        }

        // Expira o prefixo se a segunda tecla demorar demais.
        if let Some(t0) = self.chord_armed_at {
            if t0.elapsed().as_secs_f32() > 3.0 {
                self.chord_armed_at = None;
            }
        }

        let mut armed = self.chord_armed_at.is_some();
        let mut split: Option<SplitDir> = None;
        let mut nav: Option<(i32, i32)> = None;
        let mut cycle = false;
        let mut close = false;
        let mut help = false;
        let mut literal_b = false;
        // Segunda tecla recebida (valida ou nao): o prefixo desarma.
        let mut acted = false;

        ctx.input_mut(|i| {
            i.events.retain(|ev| {
                // Com o prefixo armado, o texto digitado pertence ao atalho
                // (ex.: o "h" de Ctrl+B,H) e nunca vai para o terminal.
                if armed {
                    if let egui::Event::Text(_) = ev {
                        return false;
                    }
                }
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
                // Ja armado: a proxima tecla decide a acao (sempre consumida).
                acted = true;
                match key {
                    egui::Key::H => split = Some(SplitDir::SideBySide),
                    egui::Key::V => split = Some(SplitDir::Stacked),
                    egui::Key::ArrowLeft => nav = Some((-1, 0)),
                    egui::Key::ArrowRight => nav = Some((1, 0)),
                    egui::Key::ArrowUp => nav = Some((0, -1)),
                    egui::Key::ArrowDown => nav = Some((0, 1)),
                    egui::Key::O => cycle = true,
                    egui::Key::X => close = true,
                    egui::Key::A => help = true,
                    egui::Key::B if modifiers.ctrl => literal_b = true,
                    egui::Key::Escape => {} // apenas cancela o prefixo
                    _ => {} // tecla invalida: engolida, sem acao (estilo tmux)
                }
                false
            });
        });

        if let Some(dir) = split {
            // Sem foco definido (ex.: sessao recem-aberta), usa o 1º painel.
            if let Some(path) = self.focused_path.clone().or_else(|| self.first_pane_path()) {
                // O novo painel (seletor) ja nasce com o filtro focado.
                self.pending_focus = self.split_pane(&path, dir);
            }
        } else if let Some((dx, dy)) = nav {
            self.focus_dir(dx, dy);
        } else if cycle {
            self.focus_cycle();
        } else if close {
            if let Some(path) = self.focused_path.clone() {
                self.close_pane(&path);
            }
        } else if help {
            self.show_help = !self.show_help;
        } else if literal_b {
            if let (Some(path), Some(root)) = (&self.focused_path, &mut self.root) {
                if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                    if let Some(ssh) = &pane.ssh {
                        ssh.send_data(vec![0x02]);
                    }
                }
            }
        }

        self.chord_armed_at = if armed && !acted {
            // Mantem o instante original; um arme novo comeca a contar agora.
            self.chord_armed_at.or_else(|| Some(Instant::now()))
        } else {
            None
        };
        // Redesenha em breve para o indicador do prefixo aparecer e expirar.
        if self.chord_armed_at.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    /// Caminho do primeiro painel (folha) da arvore, se houver.
    fn first_pane_path(&self) -> Option<Vec<usize>> {
        fn first_leaf(node: &Node, path: &mut Vec<usize>) -> Option<Vec<usize>> {
            match node {
                Node::Leaf(_) => Some(path.clone()),
                Node::Split { children, .. } => {
                    for (i, c) in children.iter().enumerate() {
                        path.push(i);
                        if let Some(p) = first_leaf(c, path) {
                            return Some(p);
                        }
                        path.pop();
                    }
                    None
                }
            }
        }
        self.root.as_ref().and_then(|r| first_leaf(r, &mut Vec::new()))
    }

    /// Move o foco para o painel vizinho na direcao (dx, dy), usando os
    /// retangulos renderizados no ultimo quadro.
    fn focus_dir(&mut self, dx: i32, dy: i32) {
        let Some(cur) = self.focused_path.clone().or_else(|| self.first_pane_path()) else {
            return;
        };
        let Some((_, cur_rect)) = self.pane_rects.iter().find(|(p, _)| *p == cur) else {
            self.pending_focus = Some(cur);
            return;
        };
        let c = cur_rect.center();
        let mut best: Option<(f32, Vec<usize>)> = None;
        for (p, r) in &self.pane_rects {
            if *p == cur {
                continue;
            }
            let d = r.center() - c;
            // Componente na direcao pedida; precisa apontar "para la".
            let along = d.x * dx as f32 + d.y * dy as f32;
            if along <= 1.0 {
                continue;
            }
            // Penaliza desvio lateral: prefere paineis alinhados com a seta.
            let ortho = (d.x * dy as f32).abs() + (d.y * dx as f32).abs();
            let score = along + ortho * 2.0;
            if best.as_ref().map_or(true, |(s, _)| score < *s) {
                best = Some((score, p.clone()));
            }
        }
        if let Some((_, p)) = best {
            self.pending_focus = Some(p);
        }
    }

    /// Cicla o foco para o proximo painel na ordem de renderizacao.
    fn focus_cycle(&mut self) {
        if self.pane_rects.is_empty() {
            return;
        }
        let next = self
            .focused_path
            .as_ref()
            .and_then(|f| self.pane_rects.iter().position(|(p, _)| p == f))
            .map(|i| (i + 1) % self.pane_rects.len())
            .unwrap_or(0);
        self.pending_focus = Some(self.pane_rects[next].0.clone());
    }

    /// Dialogo flutuante de confirmacao de exclusao de host (mesmo padrao do
    /// dialogo de exclusao de arquivos do SFTP).
    fn ui_confirm_delete(&mut self, ctx: &egui::Context) {
        let Some(id) = self.pending_delete else {
            return;
        };
        // O host pode ter sido removido por outro caminho nesse meio-tempo.
        let Some(pos) = self.vault.hosts.iter().position(|h| h.id == id) else {
            self.pending_delete = None;
            return;
        };
        let name = display_name(&self.vault.hosts[pos]);

        // Esc cancela (consumido para nao vazar para a tela de tras).
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            self.pending_delete = None;
            return;
        }

        let mut confirm = false;
        let mut cancel = false;
        let frame = egui::Frame::window(&ctx.style())
            .fill(CARD_BG)
            .stroke(egui::Stroke::new(1.0, CARD_BORDER))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(18));
        egui::Window::new(egui::RichText::new("Excluir conexão").color(ACCENT).strong())
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_min_width(320.0);
                ui.label(egui::RichText::new("Excluir a conexão:").color(TEXT_WEAK));
                ui.add_space(4.0);
                ui.label(egui::RichText::new(name).size(14.0).color(HIGHLIGHT));
                ui.add_space(12.0);
                ui.label(egui::RichText::new("Esta ação é permanente.").color(TEXT_WEAK));
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if danger_btn(ui, "Excluir") {
                        confirm = true;
                    }
                    if ghost_btn(ui, "Cancelar") {
                        cancel = true;
                    }
                });
            });

        if confirm {
            self.vault.hosts.remove(pos);
            self.pending_delete = None;
            if let Err(e) = self.save_vault() {
                self.hosts_error = Some(format!("Erro ao salvar: {e}"));
            }
        } else if cancel {
            self.pending_delete = None;
        }
    }

    /// Verdadeiro quando o painel focado e um terminal (as teclas de funcao
    /// pertencem ao shell remoto nesse caso).
    fn focused_pane_is_terminal(&self) -> bool {
        let (Some(path), Some(root)) = (&self.focused_path, &self.root) else {
            return false;
        };
        matches!(
            node_at(root, path),
            Some(Node::Leaf(pane)) if pane.terminal.is_some() && !pane.picking
        )
    }

    /// Janela flutuante com todos os atalhos de teclado, agrupados por area.
    fn ui_help(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::window(&ctx.style())
            .fill(CARD_BG)
            .stroke(egui::Stroke::new(1.0, CARD_BORDER))
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(20));

        let mut open = self.show_help;
        egui::Window::new(
            egui::RichText::new("Atalhos de teclado").color(ACCENT).strong(),
        )
        .collapsible(false)
        .resizable(false)
        .movable(true)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(frame)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.set_min_width(430.0);

            let secao = |ui: &mut egui::Ui, titulo: &str| {
                ui.add_space(8.0);
                ui.label(egui::RichText::new(titulo).color(ACCENT).strong());
                ui.add_space(2.0);
            };
            let atalho = |ui: &mut egui::Ui, teclas: &str, descricao: &str| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [170.0, 16.0],
                        egui::Label::new(
                            egui::RichText::new(teclas).monospace().color(CARD_TEXT),
                        ),
                    );
                    ui.label(egui::RichText::new(descricao).color(TEXT_WEAK));
                });
            };

            secao(ui, "Paineis da sessao");
            atalho(ui, "Alt+(setas)", "trocar de painel (na direcao)");

            secao(ui, "Paineis da sessao (prefixo Ctrl+B)");
            atalho(ui, "Ctrl+B, H", "dividir lado a lado");
            atalho(ui, "Ctrl+B, V", "dividir empilhado");
            atalho(ui, "Ctrl+B, \u{2190}\u{2191}\u{2193}\u{2192}", "trocar de painel (alternativa)");
            atalho(ui, "Ctrl+B, O", "ciclar o foco");
            atalho(ui, "Ctrl+B, X", "fechar o painel focado");
            atalho(ui, "Ctrl+B, Ctrl+B", "enviar Ctrl+B ao terminal");
            atalho(ui, "Ctrl+B, A", "abrir/fechar esta ajuda");
            atalho(ui, "Ctrl+B, Esc", "cancelar o prefixo");

            secao(ui, "Selecao de conexoes");
            atalho(ui, "digitar", "filtrar pelo nome da conexao");
            atalho(ui, "\u{2190}\u{2191}\u{2193}\u{2192}", "escolher na grade");
            atalho(ui, "Enter", "conectar a selecao");
            atalho(ui, "Ctrl+Enter", "abrir SFTP da selecao");
            atalho(ui, "Esc", "limpar filtro / fechar seletor");
            atalho(ui, "Ctrl+N", "novo host (em qualquer seletor)");
            atalho(ui, "Ctrl+L", "bloquear o cofre (tela de conexoes)");

            secao(ui, "Navegador SFTP (painel em foco)");
            atalho(ui, "\u{2191}\u{2193}", "selecionar arquivo/pasta");
            atalho(ui, "Enter", "abrir a pasta selecionada");
            atalho(ui, "Backspace", "voltar a pasta anterior");
            atalho(ui, "F2", "renomear a selecao");
            atalho(ui, "Delete", "excluir a selecao");
            atalho(ui, "F5", "atualizar a listagem");
            atalho(ui, "clique no caminho", "editar/navegar direto");

            secao(ui, "Dialogos e editor de host");
            atalho(ui, "Esc", "cancelar/fechar");
            atalho(ui, "Enter", "confirmar (renomear)");
            atalho(ui, "Ctrl+Enter", "salvar host");

            ui.add_space(12.0);
            ui.vertical_centered(|ui| {
                if accent_btn(ui, "Fechar") {
                    self.show_help = false;
                }
            });
        });
        // Fechou pelo "x" da janela.
        if !open {
            self.show_help = false;
        }
    }

    /// Renderiza a arvore de paineis na area disponivel e aplica as acoes
    /// estruturais coletadas (dividir/fechar/conectar).
    fn ui_session(&mut self, ui: &mut egui::Ui) {
        let rect = ui.max_rect();
        let mut actions: Vec<PaneAction> = Vec::new();
        let hosts = &self.vault.hosts;
        let mut focused: Option<Vec<usize>> = None;
        // O pedido de foco vale por um quadro; os retangulos dos paineis sao
        // recalculados a cada renderizacao.
        let mut pending = self.pending_focus.take();
        // Foco "grudento": se nenhum widget tem o foco (clique num cartao ou
        // espaco vazio, dialogo fechado...), devolve-o ao ultimo painel focado,
        // para que digitar continue filtrando/indo ao terminal sem o mouse.
        let modal_open =
            self.editor.is_some() || self.pending_delete.is_some() || self.show_help;
        if pending.is_none() && !modal_open && ui.memory(|m| m.focused().is_none()) {
            pending = self.last_pane_focus.clone();
        }
        self.pane_rects.clear();
        if let Some(root) = &mut self.root {
            let mut path = Vec::new();
            render_node(
                ui,
                rect,
                root,
                &mut path,
                hosts,
                &mut actions,
                &mut focused,
                &mut self.pane_rects,
                &pending,
            );
        }
        if focused.is_some() {
            self.last_pane_focus = focused.clone();
        }
        self.focused_path = focused;
        for action in actions {
            match action {
                PaneAction::Split { path, dir } => {
                    // O novo painel (seletor) ja nasce com o filtro focado.
                    self.pending_focus = self.split_pane(&path, dir);
                }
                PaneAction::Close { path } => self.close_pane(&path),
                PaneAction::Connect { path, host } => self.connect_pane(&path, host),
                PaneAction::OpenLocal { path, shell } => {
                    self.connect_local_pane(&path, shell)
                }
                PaneAction::Sftp { path, host } => self.connect_sftp_pane(&path, host),
                PaneAction::Edit { host } => {
                    if host < self.vault.hosts.len() {
                        self.hosts_error = None;
                        self.editor =
                            Some(HostEditor::from_host(&self.vault.hosts[host]));
                    }
                }
                PaneAction::NewHost => {
                    self.hosts_error = None;
                    self.editor = Some(HostEditor::new());
                }
                PaneAction::Delete { host } => {
                    // A exclusao pede confirmacao num dialogo (acao permanente).
                    if host < self.vault.hosts.len() {
                        self.pending_delete = Some(self.vault.hosts[host].id);
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
            if let Some(sftp) = &pane.sftp {
                sftp.disconnect();
            }
        }
        Node::Split { children, .. } => {
            for c in children {
                disconnect_tree(c);
            }
        }
    }
}

/// Novo caminho de um painel `f` depois de fechar o filho `idx` da divisao em
/// `parent` (`collapsed` = a divisao ficou com um so filho e foi substituida
/// por ele). `None` quando `f` era o proprio painel fechado (ou estava dentro).
fn remap_after_close(
    f: &[usize],
    parent: &[usize],
    idx: usize,
    collapsed: bool,
) -> Option<Vec<usize>> {
    let depth = parent.len();
    if f.len() <= depth || !f.starts_with(parent) {
        return Some(f.to_vec());
    }
    let k = f[depth];
    if k == idx {
        return None;
    }
    let mut out = f.to_vec();
    if k > idx {
        out[depth] -= 1;
    }
    if collapsed {
        out.remove(depth);
    }
    Some(out)
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

/// Aba segmentada generica: preenchida com o acento quando selecionada,
/// discreta (com hover) caso contrario. Retorna `true` quando clicada.
fn seg_tab(ui: &mut egui::Ui, selected: bool, size: egui::Vec2, text: &str) -> bool {
    let style = if selected {
        BtnStyle {
            fill: ACCENT_FILL,
            fill_hover: ACCENT_FILL_HOVER,
            text: egui::Color32::WHITE,
            text_hover: egui::Color32::WHITE,
            stroke: Some(ACCENT),
            stroke_hover: Some(ACCENT),
        }
    } else {
        BtnStyle {
            fill: CARD_BG,
            fill_hover: hex("#2c2c33"),
            text: TEXT_WEAK,
            text_hover: CARD_TEXT,
            stroke: Some(CARD_BORDER),
            stroke_hover: Some(ACCENT),
        }
    };
    painted_btn(ui, size, text, 14.0, &style)
}

/// Aba segmentada do portao do cofre (Abrir/Criar).
fn gate_tab(ui: &mut egui::Ui, mode: &mut GateMode, value: GateMode, text: &str) {
    if seg_tab(ui, *mode == value, egui::vec2(130.0, 28.0), text) {
        *mode = value;
    }
}

/// Aba segmentada do metodo de autenticacao (Senha/Chave) na janela de host.
fn auth_tab(ui: &mut egui::Ui, use_key: &mut bool, value: bool, text: &str) {
    if seg_tab(ui, *use_key == value, egui::vec2(110.0, 26.0), text) {
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
            MENU_BG,
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
    auth: Option<(egui::Color32, egui::ImageSource<'static>, &str)>,
    selected: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(HOST_TILE_SIZE, egui::Sense::click());
    let painter = ui.painter();

    // Fundo do cartao; a borda realca no hover e, mais forte, quando o cartao
    // esta selecionado pela navegacao por teclado (setas + Enter).
    let border = if selected {
        egui::Stroke::new(2.0, ACCENT)
    } else if response.hovered() {
        egui::Stroke::new(1.0, ACCENT)
    } else {
        egui::Stroke::new(1.0, CARD_BORDER)
    };
    painter.rect(rect, 8.0, CARD_BG, border, egui::StrokeKind::Inside);
    if selected {
        painter.rect_filled(rect, 8.0, ACCENT.gamma_multiply(0.10));
    }

    let pad = 12.0;
    // Badge do icone no canto superior esquerdo.
    let badge = egui::Rect::from_min_size(
        egui::pos2(rect.left() + pad, rect.top() + pad),
        egui::vec2(34.0, 34.0),
    );
    painter.rect(
        badge,
        8.0,
        WIDGET_BG,
        egui::Stroke::new(1.0, hex("#3d3d45")),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(badge.center(), egui::vec2(19.0, 19.0));
    egui::Image::new(icon).tint(ACCENT).paint_at(ui, icon_rect);

    // Icone do metodo de autenticacao (chave/senha) no canto superior direito;
    // o nome do metodo aparece na dica ao passar o mouse.
    let auth_size = 16.0;
    let mut title_right = rect.right() - pad;
    let mut hover = String::new();
    if let Some((color, icon, label)) = auth {
        let icon_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - pad - auth_size, rect.top() + pad + 2.0),
            egui::vec2(auth_size, auth_size),
        );
        egui::Image::new(icon).tint(color).paint_at(ui, icon_rect);
        title_right = icon_rect.left() - 6.0;
        hover = format!("Autenticacao por {label} \u{00b7} ");
    }

    // Titulo (nome) e subtitulo (endereco), truncados pela largura disponivel
    // para nunca invadir o icone de autenticacao nem a borda do cartao.
    let text_x = badge.right() + 10.0;
    let truncated = |text: &str, size: f32, color: egui::Color32, width: f32| {
        let mut job = egui::text::LayoutJob::simple_singleline(
            text.to_string(),
            egui::FontId::proportional(size),
            color,
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width(width.max(0.0));
        ui.fonts(|f| f.layout_job(job))
    };
    painter.galley(
        egui::pos2(text_x, rect.top() + pad + 2.0),
        truncated(title, 14.0, egui::Color32::WHITE, title_right - text_x),
        egui::Color32::WHITE,
    );
    painter.galley(
        egui::pos2(rect.left() + pad, rect.bottom() - pad - 14.0),
        truncated(subtitle, 11.0, TEXT_WEAK, rect.width() - 2.0 * pad),
        TEXT_WEAK,
    );

    // Affordance: o cartao e clicavel (duplo clique conecta).
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(format!(
            "{hover}Duplo clique conecta \u{00b7} botao direito para opcoes"
        ))
}

/// Acao escolhida pelo usuario no seletor de conexoes compartilhado.
enum PickerAction {
    /// Abrir um terminal local (cmd.exe ou WSL).
    OpenLocal(pty::LocalShell),
    /// Conectar ao host no indice indicado.
    Connect(usize),
    /// Abrir um navegador de arquivos SFTP no host indicado.
    Sftp(usize),
    /// Editar o host no indice indicado (so disponivel quando `manage`).
    Edit(usize),
    /// Cadastrar um host novo (cartao "Novo host" ou Ctrl+N).
    NewHost,
    /// Excluir o host no indice indicado (so disponivel quando `manage`).
    Delete(usize),
    /// Fechar o painel deste seletor (Esc com filtro vazio; so com `closable`).
    ClosePane,
}

/// Um cartao do seletor de conexoes: terminal local (cmd/WSL), um host
/// cadastrado ou o cartao de cadastro de host novo.
#[derive(Clone, Copy)]
enum PickerTile {
    Local(pty::LocalShell),
    Host(usize),
    New,
}

/// Opcoes do seletor de conexoes compartilhado.
struct PickerOpts {
    /// Mostra Editar/Excluir no menu de contexto.
    manage: bool,
    /// Foca o filtro quando nada mais tem o foco (tela principal de hosts).
    autofocus: bool,
    /// Forca o foco no filtro neste quadro (painel recem-criado por divisao).
    force_focus: bool,
    /// Esc com filtro vazio devolve `ClosePane` (seletor dentro de um painel).
    closable: bool,
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
    opts: PickerOpts,
) -> Option<PickerAction> {
    let mut action: Option<PickerAction> = None;

    // Ids estaveis deste seletor (filtro, selecao e rolagem).
    let base = egui::Id::new(id_salt);
    let filter_id = base.with("filter");
    let sel_id = base.with("sel");

    // Cartoes visiveis com o filtro atual (None = terminal local; Some(i) =
    // host `i`), calculados antes de desenhar para a navegacao por teclado.
    let termo = filter.trim().to_lowercase();
    let mut tiles: Vec<PickerTile> = Vec::new();
    if termo.is_empty() || "terminal local".contains(&termo) || "cmd".contains(&termo) {
        tiles.push(PickerTile::Local(pty::LocalShell::Cmd));
    }
    // O cartao WSL so aparece quando o WSL esta instalado na maquina.
    if pty::wsl_available()
        && (termo.is_empty() || "wsl".contains(&termo) || "linux".contains(&termo))
    {
        tiles.push(PickerTile::Local(pty::LocalShell::Wsl));
    }
    for (i, host) in hosts.iter().enumerate() {
        // Filtra so pelo nome exibido no cartao (o endereco/IP nao conta).
        if !termo.is_empty() && !display_name(host).to_lowercase().contains(&termo) {
            continue;
        }
        tiles.push(PickerTile::Host(i));
    }
    // Cartao "Novo host" sempre por ultimo: permite cadastrar uma conexao de
    // qualquer seletor (inclusive num painel dividido), sem voltar ao inicio.
    if opts.manage {
        tiles.push(PickerTile::New);
    }

    // --- Teclado: setas navegam a grade, Enter conecta, Ctrl+Enter abre SFTP,
    // Esc limpa o filtro (ou fecha o seletor). Ativo com o filtro em foco. ---
    let has_kb = ui.memory(|m| m.focused() == Some(filter_id));
    let mut sel: usize = ui.ctx().memory(|m| m.data.get_temp(sel_id).unwrap_or(0));
    let mut sel_changed = false;
    let mut activate = false;
    let mut activate_sftp = false;
    let mut new_host = false;
    let mut esc = false;
    // Quantos cartoes cabem por linha (mesma conta do layout horizontal_wrapped).
    let spacing = 10.0;
    let per_row = (((ui.available_width() + spacing) / (HOST_TILE_SIZE.x + spacing)).floor()
        as usize)
        .max(1);
    if has_kb {
        use egui::{Key, Modifiers};
        ui.input_mut(|i| {
            if !tiles.is_empty() {
                let last = tiles.len() - 1;
                if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                    sel = (sel + 1).min(last);
                    sel_changed = true;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                    sel = sel.saturating_sub(1);
                    sel_changed = true;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    sel = (sel + per_row).min(last);
                    sel_changed = true;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    sel = sel.saturating_sub(per_row);
                    sel_changed = true;
                }
                if i.consume_key(Modifiers::CTRL, Key::Enter) {
                    activate_sftp = true;
                } else if i.consume_key(Modifiers::NONE, Key::Enter) {
                    activate = true;
                }
            }
            // Ctrl+N abre o cadastro de host novo (tambem no painel dividido).
            if opts.manage && i.consume_key(Modifiers::CTRL, Key::N) {
                new_host = true;
            }
            // So consome Esc quando ele tem efeito aqui (limpar ou fechar).
            if (!filter.is_empty() || opts.closable)
                && i.consume_key(Modifiers::NONE, Key::Escape)
            {
                esc = true;
            }
        });
    }
    sel = sel.min(tiles.len().saturating_sub(1));

    // Campo de busca com lupa e botao de limpar.
    ui.horizontal(|ui| {
        ui.add(
            egui::Image::new(ICON_SEARCH)
                .fit_to_exact_size(egui::vec2(16.0, 16.0))
                .tint(TEXT_WEAK),
        );
        // Foco forcado e pedido antes de criar o campo: assim ele ja recebe o
        // texto digitado neste mesmo quadro (senao a 1ª tecla se perderia).
        if opts.force_focus {
            ui.memory_mut(|m| m.request_focus(filter_id));
        }
        let resp = ui.add(
            egui::TextEdit::singleline(filter)
                .id(filter_id)
                .desired_width(260.0)
                .hint_text("Filtrar conexões..."),
        );
        // Ao mudar o filtro, a selecao volta ao primeiro resultado.
        if resp.changed() {
            sel = 0;
        }
        if resp.has_focus() {
            // A trava abaixo so vale a partir do 2º quadro com foco. Quando o
            // campo acaba de ganhar o foco, pede uma 2ª passada imediata (sem
            // eventos novos) para a trava ja valer na proxima tecla — senao um
            // Esc/seta logo apos dividir a tela escaparia para o egui.
            if !ui.memory(|m| m.had_focus_last_frame(filter_id)) {
                ui.ctx().request_discard("filtro do seletor recebeu o foco");
            }
            // Trava setas e Esc no campo: sem isso o egui usaria as setas para
            // mover o foco a outro widget e o Esc para soltar o foco (entao o
            // seletor nem veria o Esc e a digitacao deixaria de filtrar).
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    filter_id,
                    egui::EventFilter {
                        tab: false,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }
        // Foca o campo para que comecar a digitar ja filtre.
        if opts.autofocus && !resp.has_focus() && ui.memory(|m| m.focused().is_none()) {
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

    egui::ScrollArea::vertical()
        .id_salt(base.with("scroll"))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(spacing, spacing);

                for (idx, t) in tiles.iter().enumerate() {
                    let selected = has_kb && idx == sel;
                    match t {
                        // Terminal local (cmd.exe ou WSL).
                        PickerTile::Local(shell) => {
                            let shell = *shell;
                            let (titulo, subtitulo) = match shell {
                                pty::LocalShell::Cmd => {
                                    ("Terminal local", "Prompt de comando do Windows")
                                }
                                pty::LocalShell::Wsl => {
                                    ("WSL", "Linux (Windows Subsystem for Linux)")
                                }
                            };
                            let local = host_tile(
                                ui,
                                ICON_TERMINAL,
                                titulo,
                                subtitulo,
                                None,
                                selected,
                            );
                            if selected && sel_changed {
                                local.scroll_to_me(None);
                            }
                            if local.double_clicked() {
                                action = Some(PickerAction::OpenLocal(shell));
                            }
                            local.context_menu(|ui| {
                                let bg = style_context_menu(ui);
                                if menu_item(ui, ICON_PLUG, "Abrir", ACCENT) {
                                    action = Some(PickerAction::OpenLocal(shell));
                                    ui.close_menu();
                                }
                                paint_menu_bg(ui, bg);
                            });
                        }
                        // Cartao de cadastro: um clique abre o editor de host.
                        PickerTile::New => {
                            let novo = host_tile(
                                ui,
                                ICON_PLUS,
                                "Novo host",
                                "Cadastrar uma conexao SSH",
                                None,
                                selected,
                            );
                            if selected && sel_changed {
                                novo.scroll_to_me(None);
                            }
                            if novo.clicked() {
                                action = Some(PickerAction::NewHost);
                            }
                        }
                        // Host cadastrado.
                        PickerTile::Host(i) => {
                            let i = *i;
                            let host = &hosts[i];
                            let title = display_name(host);
                            let subtitle =
                                format!("{}@{}:{}", host.username, host.host, host.port);
                            let auth = match &host.auth {
                                AuthMethod::Password { .. } => {
                                    (AUTH_PASS, ICON_PASSWORD, "senha")
                                }
                                AuthMethod::Key { .. } => (AUTH_KEY, ICON_KEY, "chave"),
                            };
                            let tile = host_tile(
                                ui,
                                ICON_SERVER,
                                &title,
                                &subtitle,
                                Some(auth),
                                selected,
                            );
                            if selected && sel_changed {
                                tile.scroll_to_me(None);
                            }
                            if tile.double_clicked() {
                                action = Some(PickerAction::Connect(i));
                            }
                            tile.context_menu(|ui| {
                                let bg = style_context_menu(ui);
                                ui.label(
                                    egui::RichText::new(elide(&title, 22))
                                        .small()
                                        .color(TEXT_WEAK),
                                );
                                ui.add_space(2.0);
                                if menu_item(ui, ICON_PLUG, "Conectar", ACCENT) {
                                    action = Some(PickerAction::Connect(i));
                                    ui.close_menu();
                                }
                                if menu_item(ui, ICON_FOLDER_LOCK, "SFTP", CARD_TEXT) {
                                    action = Some(PickerAction::Sftp(i));
                                    ui.close_menu();
                                }
                                if opts.manage {
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
                    }
                }
            });

            // Mensagens de lista vazia / sem correspondencia (o cartao "Novo
            // host" nao conta como resultado).
            let resultados = tiles
                .iter()
                .filter(|t| !matches!(t, PickerTile::New))
                .count();
            if hosts.is_empty() && termo.is_empty() {
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Nenhum host cadastrado.").color(TEXT_WEAK),
                );
            } else if resultados == 0 && !termo.is_empty() {
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

    // Aplica as acoes de teclado sobre o cartao selecionado.
    if action.is_none() {
        if new_host {
            action = Some(PickerAction::NewHost);
        } else if activate {
            match tiles.get(sel) {
                Some(PickerTile::Local(s)) => action = Some(PickerAction::OpenLocal(*s)),
                Some(PickerTile::Host(i)) => action = Some(PickerAction::Connect(*i)),
                Some(PickerTile::New) => action = Some(PickerAction::NewHost),
                None => {}
            }
        } else if activate_sftp {
            if let Some(PickerTile::Host(i)) = tiles.get(sel) {
                action = Some(PickerAction::Sftp(*i));
            }
        } else if esc {
            if !filter.is_empty() {
                filter.clear();
                sel = 0;
            } else if opts.closable {
                action = Some(PickerAction::ClosePane);
            }
        }
    }

    ui.ctx().memory_mut(|m| m.data.insert_temp(sel_id, sel));
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
    rects: &mut Vec<(Vec<usize>, egui::Rect)>,
    pending: &Option<Vec<usize>>,
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
                    render_node(
                        ui, child_rect, child, path, hosts, actions, focused, rects, pending,
                    );
                });
                path.pop();
            }
        }
        Node::Leaf(pane) => {
            // Registra o retangulo deste painel para os atalhos de troca de
            // foco (Ctrl+B + setas / O) do proximo quadro.
            rects.push((path.clone(), rect));
            // Este painel deve tomar o foco do teclado neste quadro?
            let take = pending.as_deref() == Some(path.as_slice());

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
                        // Ponto colorido com o estado da sessao (ambar =
                        // conectando, verde = conectado, vermelho = erro).
                        if !pane.picking {
                            let dot = match &pane.state {
                                SessionState::Connecting => AUTH_PASS,
                                SessionState::Connected => AUTH_KEY,
                                SessionState::Closed => TEXT_WEAK,
                                SessionState::Error(_) => ERROR_FG,
                            };
                            let (drect, _) = ui.allocate_exact_size(
                                egui::vec2(10.0, 10.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().circle_filled(drect.center(), 3.5, dot);
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

            // Conteudo recuado das bordas: a borda do painel (mais grossa quando
            // focado) nao encobre os cartoes do seletor nem a 1ª coluna do terminal.
            let inset = if pane.picking { 12.0 } else { 4.0 };
            let content_rect = egui::Rect::from_min_max(
                egui::pos2(rect.min.x + inset, ui.cursor().min.y),
                egui::pos2(rect.max.x - inset, rect.max.y - 3.0),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(content_rect), |ui| {
                if pane.picking {
                    ui.add_space(8.0);
                    // O painel-seletor participa do modelo de foco: quando o campo
                    // de filtro dele detem o foco, ele e o painel focado (borda
                    // destacada e alvo dos atalhos Ctrl+B).
                    let salt = ("pane_picker", path.clone());
                    let filter_focused = ui.memory(|m| {
                        m.focused() == Some(egui::Id::new(salt.clone()).with("filter"))
                    });
                    if filter_focused || take {
                        *focused = Some(path.clone());
                    }
                    // Mesmo seletor de conexoes da tela principal (com filtro), porem
                    // com gerenciamento (Conectar/SFTP/Editar/Excluir).
                    let outcome = connection_picker(
                        ui,
                        hosts,
                        &mut pane.filter,
                        salt,
                        PickerOpts {
                            manage: true,
                            autofocus: false,
                            force_focus: take,
                            closable: true,
                        },
                    );
                    match outcome {
                        Some(PickerAction::OpenLocal(s)) => actions.push(PaneAction::OpenLocal {
                            path: path.clone(),
                            shell: s,
                        }),
                        Some(PickerAction::Connect(h)) => actions.push(PaneAction::Connect {
                            path: path.clone(),
                            host: h,
                        }),
                        Some(PickerAction::Sftp(h)) => actions.push(PaneAction::Sftp {
                            path: path.clone(),
                            host: h,
                        }),
                        Some(PickerAction::Edit(h)) => {
                            actions.push(PaneAction::Edit { host: h })
                        }
                        Some(PickerAction::NewHost) => actions.push(PaneAction::NewHost),
                        Some(PickerAction::Delete(h)) => {
                            actions.push(PaneAction::Delete { host: h })
                        }
                        // Esc com filtro vazio fecha este painel de selecao.
                        Some(PickerAction::ClosePane) => {
                            actions.push(PaneAction::Close { path: path.clone() })
                        }
                        None => {}
                    }
                } else if pane.sftp.is_some() {
                    // Painel SFTP: navegador de arquivos remoto (apenas a pasta atual).
                    if let SessionState::Error(msg) = &pane.state {
                        ui.colored_label(ERROR_FG, msg.clone());
                    }

                    // Area focavel cobrindo o conteudo: permite "selecionar" o painel
                    // (clicando) para que F5/setas atuem so no painel SFTP em foco. E
                    // adicionada antes das linhas, ficando atras delas nos cliques.
                    let content_rect = ui.available_rect_before_wrap();
                    let focus_id = ui.id().with(("sftp_focus", path.as_slice()));
                    let focus_resp = ui.interact(content_rect, focus_id, egui::Sense::click());
                    if focus_resp.clicked() || take {
                        focus_resp.request_focus();
                    }
                    let has_focus = focus_resp.has_focus();
                    if has_focus {
                        *focused = Some(path.clone());
                        // Trava as setas neste foco: sem isso o egui as usaria para
                        // mover o foco para outro widget na primeira tecla.
                        ui.memory_mut(|m| {
                            m.set_focus_lock_filter(
                                focus_id,
                                egui::EventFilter {
                                    tab: false,
                                    horizontal_arrows: true,
                                    vertical_arrows: true,
                                    escape: false,
                                },
                            );
                        });
                    }
                    // F5 atualiza o painel em foco ou sob o cursor.
                    let active = has_focus || focus_resp.hovered();
                    let f5 = active && ui.input(|i| i.key_pressed(egui::Key::F5));

                    let pointer_in = ui.rect_contains_pointer(content_rect);

                    let mut to_list: Vec<String> = Vec::new();
                    let mut fs_op: Option<FsOp> = None;
                    let mut cur_dir = String::new();
                    if let Some(exp) = &mut pane.explorer {
                        let out = exp.ui(ui, ("sftp_explorer", path.as_slice()), has_focus);
                        to_list = out.to_list;
                        fs_op = out.op;
                        cur_dir = exp.cur_path.clone();
                        if out.refresh || f5 {
                            exp.refresh(&mut to_list);
                        }
                        // Clicar numa linha da listagem tambem seleciona o painel.
                        if out.clicked_row {
                            focus_resp.request_focus();
                        }
                    }

                    // Operacao de gerenciamento (renomear/permissoes/excluir).
                    if let (Some(op), Some(sftp)) = (fs_op, &pane.sftp) {
                        match op {
                            FsOp::Rename { from, to } => sftp.rename(from, to, cur_dir.clone()),
                            FsOp::Chmod { path, mode } => sftp.chmod(path, mode, cur_dir.clone()),
                            FsOp::Chown { path, owner, group } => {
                                sftp.chown(path, owner, group, cur_dir.clone())
                            }
                            FsOp::Remove { path, is_dir } => {
                                sftp.remove(path, is_dir, cur_dir.clone())
                            }
                        }
                    }

                    // Arrastar-e-soltar arquivos do SO: envia para o diretorio atual.
                    let dropped: Vec<PathBuf> = if pointer_in {
                        ui.input(|i| {
                            i.raw
                                .dropped_files
                                .iter()
                                .filter_map(|f| f.path.clone())
                                .collect()
                        })
                    } else {
                        Vec::new()
                    };

                    // Realce + dica enquanto se arrasta um arquivo sobre o painel.
                    let hovering_files = pointer_in && ui.input(|i| !i.raw.hovered_files.is_empty());
                    if hovering_files {
                        let painter = ui.painter();
                        painter.rect_filled(content_rect, 6.0, ACCENT.gamma_multiply(0.12));
                        painter.text(
                            content_rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "Solte para enviar ao servidor",
                            egui::FontId::proportional(16.0),
                            ACCENT,
                        );
                    }

                    if let Some(exp) = &pane.explorer {
                        if let Some(sftp) = &pane.sftp {
                            let dir = exp.cur_path.clone();
                            if !dir.is_empty() {
                                for local in dropped {
                                    sftp.upload(local, dir.clone());
                                }
                            }
                        }
                    }

                    if let Some(sftp) = &pane.sftp {
                        for p in to_list {
                            sftp.list_dir(p);
                        }
                    }
                } else {
                    if let SessionState::Error(msg) = &pane.state {
                        ui.colored_label(ERROR_FG, msg.clone());
                    }

                    // Enquanto conecta, mostra um spinner no corpo do painel em vez
                    // de uma area preta indistinguivel de um terminal ocioso.
                    if matches!(pane.state, SessionState::Connecting) {
                        ui.add_space((ui.available_height() * 0.4).max(12.0));
                        ui.vertical_centered(|ui| {
                            ui.add(egui::Spinner::new().size(20.0));
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "Conectando a {}...",
                                    pane.host_name
                                ))
                                .color(TEXT_WEAK),
                            );
                        });
                    } else {
                        let mut output = None;
                        if let Some(term) = &mut pane.terminal {
                            if take {
                                term.take_focus();
                            }
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
                }
            });

            // Borda que delimita a janela do painel (desenhada por ultimo).
            // O painel com o foco do teclado recebe borda roxa mais clara e
            // mais espessa (e o alvo dos atalhos Ctrl+B e da digitacao).
            let is_focused = focused.as_deref() == Some(path.as_slice());
            let border = if is_focused {
                egui::Stroke::new(2.0, PANE_BORDER_FOCUS)
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
                self.handle_session_keys(ctx);
                self.drain_ssh_events();
            }
            Screen::Gate => {
                // Abertura/criacao do cofre em dois tempos: o quadro 1 desenha
                // o spinner; o quadro 2 (ja com ele na tela) roda o Argon2.
                match self.gate_busy {
                    1 => {
                        self.gate_busy = 2;
                        ctx.request_repaint();
                    }
                    2 => {
                        self.gate_busy = 0;
                        self.gate_submit();
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        // F1 abre/fecha a ajuda de atalhos — exceto com um terminal em foco
        // (la o F1 pertence ao shell remoto; use Ctrl+B, A). Esc fecha.
        if !self.focused_pane_is_terminal()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1))
        {
            self.show_help = !self.show_help;
        }
        if self.show_help
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.show_help = false;
        }

        // Barra de dicas de atalhos na base (Session e Hosts).
        match self.screen {
            Screen::Session | Screen::Hosts => {
                let armed = self.chord_armed_at.is_some();
                let in_session = matches!(self.screen, Screen::Session);
                egui::TopBottomPanel::bottom("hint_bar")
                    .frame(
                        egui::Frame::NONE
                            .fill(SCREEN_BG)
                            .inner_margin(egui::Margin::symmetric(10, 4)),
                    )
                    .show_separator_line(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            if in_session && armed {
                                // Prefixo armado: mostra as opcoes do chord.
                                ui.label(
                                    egui::RichText::new("Ctrl+B \u{2026}")
                                        .strong()
                                        .color(ACCENT),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "H dividir  \u{00b7}  V empilhar  \u{00b7}  \
                                         \u{2190}\u{2191}\u{2193}\u{2192} trocar painel  \u{00b7}  \
                                         O ciclar  \u{00b7}  X fechar  \u{00b7}  \
                                         Ctrl+B literal  \u{00b7}  A ajuda  \u{00b7}  Esc cancela",
                                    )
                                    .small()
                                    .color(CARD_TEXT),
                                );
                            } else if in_session {
                                ui.label(
                                    egui::RichText::new(
                                        "Alt+\u{2190}\u{2191}\u{2193}\u{2192} troca de painel  \
                                         \u{00b7}  Ctrl+B + A: Ajuda  \u{00b7}  F5 atualiza SFTP",
                                    )
                                    .small()
                                    .color(TEXT_WEAK),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new(
                                        "Digite para filtrar  \u{00b7}  \
                                         \u{2190}\u{2191}\u{2193}\u{2192} escolher  \u{00b7}  \
                                         Enter conectar  \u{00b7}  Ctrl+Enter SFTP  \u{00b7}  \
                                         Ctrl+N novo host  \u{00b7}  Ctrl+L bloquear  \u{00b7}  \
                                         F1 ajuda",
                                    )
                                    .small()
                                    .color(TEXT_WEAK),
                                );
                            }
                        });
                    });
            }
            _ => {}
        }

        match self.screen {
            Screen::Session => {
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::NONE
                            .fill(SCREEN_BG)
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

        // Janela flutuante com a lista de atalhos (F1 / Ctrl+B, A).
        if self.show_help {
            self.ui_help(ctx);
        }

        // Confirmacao de exclusao de host (vale em Hosts e Session).
        if self.pending_delete.is_some() {
            self.ui_confirm_delete(ctx);
        }
    }
}

#[cfg(test)]
mod focus_tests {
    //! Navegacao so por teclado entre paineis: o seletor criado ao dividir a
    //! tela deve receber (e manter) o foco para que digitar ja filtre.
    use super::*;

    /// App ja na sessao, com um unico painel de terminal (sem conexao real).
    fn app() -> App {
        let mut pane = Pane::picker();
        pane.picking = false;
        pane.state = SessionState::Connected;
        pane.terminal = Some(Terminal::new(80, 24));
        App {
            screen: Screen::Session,
            vault: Vault::default(),
            vault_path: None,
            master_password: String::new(),
            gate_mode: GateMode::Open,
            gate_path: String::new(),
            gate_password: String::new(),
            gate_password_confirm: String::new(),
            gate_error: None,
            editor: None,
            hosts_error: None,
            hosts_filter: String::new(),
            root: Some(Node::Leaf(pane)),
            ctx_for_repaint: None,
            logo_texture: None,
            logo_load_attempted: false,
            splash_start: Instant::now(),
            last_vault_path: String::new(),
            gate_focus_requested: false,
            gate_busy: 0,
            focused_path: None,
            chord_armed_at: None,
            pane_rects: Vec::new(),
            pending_focus: None,
            last_pane_focus: None,
            show_help: false,
            pending_delete: None,
        }
    }

    fn key(k: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key { key: k, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    fn click(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        }
    }

    fn frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 700.0),
            )),
            events,
            focused: true,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            app.handle_session_keys(ctx);
            egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
        });
    }

    /// Texto do filtro do seletor criado pela divisao (painel [1]).
    fn new_pane_filter(app: &App) -> String {
        match &app.root {
            Some(Node::Split { children, .. }) => match &children[1] {
                Node::Leaf(p) => p.filter.clone(),
                _ => panic!("painel [1] nao e folha"),
            },
            _ => panic!("tela nao foi dividida"),
        }
    }

    /// Divide pelo atalho Ctrl+B, H e devolve o contexto ja com o seletor.
    fn split_by_keyboard() -> (egui::Context, App) {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut app = app();
        for _ in 0..3 {
            frame(&ctx, &mut app, vec![]);
        }
        frame(&ctx, &mut app, vec![key(egui::Key::B, egui::Modifiers::CTRL)]);
        frame(
            &ctx,
            &mut app,
            vec![key(egui::Key::H, egui::Modifiers::NONE), egui::Event::Text("h".into())],
        );
        (ctx, app)
    }

    #[test]
    fn split_focuses_filter_and_typing_filters() {
        let (ctx, mut app) = split_by_keyboard();
        assert_eq!(app.focused_path, Some(vec![1]));
        frame(&ctx, &mut app, vec![egui::Event::Text("pg".into())]);
        assert_eq!(new_pane_filter(&app), "pg");
    }

    #[test]
    fn arrows_and_esc_keep_focus_on_filter() {
        let (ctx, mut app) = split_by_keyboard();
        frame(&ctx, &mut app, vec![egui::Event::Text("pg".into())]);
        for k in [egui::Key::ArrowDown, egui::Key::ArrowUp, egui::Key::ArrowRight] {
            frame(&ctx, &mut app, vec![key(k, egui::Modifiers::NONE)]);
        }
        // Esc limpa o filtro sem soltar o foco do campo.
        frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert_eq!(new_pane_filter(&app), "");
        frame(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(new_pane_filter(&app), "x");
    }

    #[test]
    fn click_inside_picker_returns_focus_to_filter() {
        let (ctx, mut app) = split_by_keyboard();
        let r = app.pane_rects.iter().find(|(p, _)| *p == vec![1]).unwrap().1;
        // Clique no corpo do seletor (fora do campo): o egui solta o foco.
        let pos = egui::pos2(r.center().x, r.max.y - 20.0);
        frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        frame(&ctx, &mut app, vec![click(pos, false)]);
        frame(&ctx, &mut app, vec![egui::Event::Text("ns".into())]);
        assert_eq!(new_pane_filter(&app), "ns");
    }

    #[test]
    fn remap_after_close_paths() {
        // [0,1,2] lado a lado; fecha [1]: [2] vira [1], [0] fica igual.
        assert_eq!(remap_after_close(&[2], &[], 1, false), Some(vec![2 - 1]));
        assert_eq!(remap_after_close(&[0], &[], 1, false), Some(vec![0]));
        assert_eq!(remap_after_close(&[1], &[], 1, false), None);
        // Divisao [1] com 2 filhos colapsa: [1,0,3] -> [1,3] ao fechar [1,1].
        assert_eq!(remap_after_close(&[1, 0, 3], &[1], 1, true), Some(vec![1, 3]));
        // Fora da divisao afetada: inalterado.
        assert_eq!(remap_after_close(&[0, 2], &[1], 0, true), Some(vec![0, 2]));
        // Painel dentro da subarvore fechada.
        assert_eq!(remap_after_close(&[1, 1, 0], &[1], 1, true), None);
    }

    #[test]
    fn esc_on_empty_filter_closes_pane_and_refocuses_terminal() {
        let (ctx, mut app) = split_by_keyboard();
        frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert!(matches!(app.root, Some(Node::Leaf(_))), "seletor nao fechou");
        frame(&ctx, &mut app, vec![]);
        assert_eq!(app.focused_path, Some(vec![]));
    }

    #[test]
    fn closing_other_pane_keeps_focus_where_it_was() {
        // Tres paineis lado a lado; o foco no terceiro. Fechar o primeiro
        // (ex.: sessao encerrada) mantem o foco no mesmo painel, agora [1].
        let (ctx, mut app) = split_by_keyboard();
        frame(&ctx, &mut app, vec![key(egui::Key::B, egui::Modifiers::CTRL)]);
        frame(&ctx, &mut app, vec![key(egui::Key::H, egui::Modifiers::NONE)]);
        frame(&ctx, &mut app, vec![]);
        assert_eq!(app.focused_path, Some(vec![2]));
        frame(&ctx, &mut app, vec![egui::Event::Text("ab".into())]);
        app.close_pane(&[0]);
        frame(&ctx, &mut app, vec![]);
        frame(&ctx, &mut app, vec![]);
        assert_eq!(app.focused_path, Some(vec![1]));
        match &app.root {
            Some(Node::Split { children, .. }) => match &children[1] {
                Node::Leaf(p) => assert_eq!(p.filter, "ab"),
                _ => panic!(),
            },
            _ => panic!(),
        }
    }

    #[test]
    fn mouse_split_focuses_filter() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut app = app();
        for _ in 0..3 {
            frame(&ctx, &mut app, vec![]);
        }
        let r = app.pane_rects[0].1;
        // Os botoes de divisao ficam no canto direito da barra de titulo.
        let mut x = r.max.x - 2.0;
        while !matches!(app.root, Some(Node::Split { .. })) && x > r.max.x - 80.0 {
            let pos = egui::pos2(x, r.min.y + 10.0);
            frame(&ctx, &mut app, vec![egui::Event::PointerMoved(pos)]);
            frame(&ctx, &mut app, vec![click(pos, true)]);
            frame(&ctx, &mut app, vec![click(pos, false)]);
            x -= 2.0;
        }
        frame(&ctx, &mut app, vec![egui::Event::Text("pg".into())]);
        assert_eq!(new_pane_filter(&app), "pg");
    }
}
