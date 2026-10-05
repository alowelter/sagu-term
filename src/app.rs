//! Aplicacao eframe/egui: portao do cofre, gestao de hosts e sessao de terminal.

use std::path::PathBuf;
use std::time::Instant;

use crate::download::{self, ConflictChoice, DownloadEvent};
use crate::hostkey::{self, HostKeyAnswer, HostKeyPrompt, KeyCheck};
use crate::osinfo::{self, OsReport};
use crate::paste;
use crate::pty;
use crate::remember;
use crate::sftp::{self, SftpHandle, SftpToUi};
use crate::ssh::{self, SshHandle, SshToUi};
use crate::terminal::Terminal;
use crate::upload::{self, UploadEvent};
use crate::vault::{self, AuthMethod, Host, Vault, VaultKey};
use crate::viewer::{self, ViewError, ViewEvent};

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

    // Aplica o tema montado aos dois estilos do egui e fixa o escuro: o egui
    // segue o tema do Windows e, no modo claro, trocaria para o estilo claro
    // padrao (dicas e menus com fundo claro sob textos de cor clara fixa).
    ctx.set_visuals_of(egui::Theme::Light, v.clone());
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.set_theme(egui::Theme::Dark);

    // A fonte proporcional (Ubuntu) nao tem setas (U+2190, U+2192) e outros
    // simbolos usados nos textos (alvo dos links, "← voltar"): sairiam como
    // um quadrado vazio. A Hack (monoespacada, ja embutida) entra por ultimo
    // como reserva e so desenha o que faltar.
    let mut fonts = egui::FontDefinitions::default();
    if let Some(prop) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        prop.push("Hack".to_owned());
    }
    ctx.set_fonts(fonts);
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
    /// Envio de arquivos soltos sobre este terminal (um lote por vez).
    upload: Option<UploadUi>,
    /// Pergunta sobre a chave do servidor aguardando o usuario (a sessao
    /// espera). Descartar o painel descarta a pergunta e aborta a conexao.
    host_key: Option<PendingHostKey>,
    /// Download do navegador SFTP (um por vez por painel).
    download: Option<DownloadUi>,
    /// Conexao do painel SFTP (colar so vale na mesma conexao).
    origin: Option<SftpOrigin>,
    /// Copiar/mover no servidor (um por vez por painel).
    paste: Option<PasteUi>,
}

/// Pergunta de chave cancelada porque o host saiu do cofre.
const HOST_KEY_DELETED: &str = "Conexão cancelada: esta conexão foi excluída do cofre.";
/// Pergunta de chave cancelada porque o endereco/porta do host mudou: a chave
/// deste servidor nunca e gravada num host que agora aponta para outro lugar.
const HOST_KEY_MOVED: &str =
    "Conexão cancelada: o endereço ou a porta desta conexão mudou; conecte de novo.";

/// Legenda do painel enquanto a sessao espera a confirmacao da chave.
const HOST_KEY_WAIT: &str = "Aguardando confirmação da chave do servidor...";

/// Clique ou Esc so valem apos este tempo com a pergunta visivel: um duplo
/// clique no cartao (que conecta) ou uma tecla em curso nao podem responder.
const HOST_KEY_ARM: std::time::Duration = std::time::Duration::from_millis(600);

/// Pergunta de chave do servidor pendente num painel (a sessao espera).
struct PendingHostKey {
    prompt: HostKeyPrompt,
    /// Ordem de chegada: a janela mostra sempre a mais antiga (fila).
    seq: u64,
    /// Primeiro quadro em que a janela desta pergunta apareceu.
    shown_at: Option<Instant>,
}

/// Envio de arquivos soltos sobre um terminal SSH, visto pela UI.
struct UploadUi {
    /// Identifica o lote nos eventos da sessao (0 = so uma mensagem local).
    id: u64,
    /// Pastas soltas junto, ignoradas (ainda nao sao enviadas).
    skipped_dirs: usize,
    stage: UploadStage,
}

enum UploadStage {
    /// Descobrindo a pasta do shell e conferindo o destino.
    Locating { since: Instant },
    /// Destino incerto: aguardando a escolha do usuario (barra no painel).
    Asking(upload::DropPlan),
    Sending {
        dir: String,
        index: usize,
        count: usize,
        name: String,
        sent: u64,
        size: u64,
    },
    /// Resultado final; quando `ok`, some sozinho apos 8 segundos.
    Done { text: String, ok: bool, at: Instant },
}

impl UploadUi {
    /// Mensagem avulsa (drop recusado), sem lote na sessao.
    fn notice(text: &str) -> Self {
        UploadUi {
            id: 0,
            skipped_dirs: 0,
            stage: UploadStage::Done {
                text: text.to_string(),
                ok: false,
                at: Instant::now(),
            },
        }
    }

    /// Lote em andamento (nao aceita outro drop ate terminar).
    fn busy(&self) -> bool {
        !matches!(self.stage, UploadStage::Done { .. })
    }
}

/// Download pelo navegador SFTP, visto pela UI.
struct DownloadUi {
    /// Identifica o lote nos eventos da sessao.
    id: u64,
    /// Pasta local escolhida pelo usuario.
    dest: PathBuf,
    /// Itens tirados antes de comecar (nome invalido, "pular existentes").
    pre_skipped: Vec<(String, String)>,
    stage: DownloadStage,
}

enum DownloadStage {
    /// Ha itens que ja existem no destino: dialogo aberto.
    Asking(download::Prepared),
    Running {
        cancel: download::Cancel,
        /// "Cancelar" clicado; aguardando a tarefa terminar.
        cancelling: bool,
        /// Ainda varrendo as pastas remotas (`found` entradas vistas).
        scanning: bool,
        found: usize,
        /// Arquivo atual (0-based) de `count`; bytes somados do lote.
        index: usize,
        count: usize,
        name: String,
        done: u64,
        total: u64,
    },
    /// Resultado; `tone` define a cor e se some sozinho (Ok/Neutral em 8 s).
    Done {
        text: String,
        /// Erros, ignorados e nomes ajustados (dica do rodape).
        detail: String,
        tone: Tone,
        at: Instant,
    },
}

/// Tom de uma mensagem de resultado.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tone {
    Ok,
    Neutral,
    Error,
}

impl DownloadUi {
    /// Perguntando ou em andamento (nao aceita outro pedido ate terminar).
    fn busy(&self) -> bool {
        !matches!(self.stage, DownloadStage::Done { .. })
    }
}

impl DownloadStage {
    /// Resultado so com texto (sem detalhes).
    fn done(text: &str, tone: Tone) -> Self {
        DownloadStage::Done {
            text: text.to_string(),
            detail: String::new(),
            tone,
            at: Instant::now(),
        }
    }

    /// Texto do rodape durante o download.
    fn running_text(&self) -> String {
        let DownloadStage::Running {
            cancelling,
            scanning,
            found,
            index,
            count,
            name,
            done,
            total,
            ..
        } = self
        else {
            return String::new();
        };
        if *cancelling {
            return "Cancelando\u{2026}".to_string();
        }
        if *scanning || name.is_empty() {
            return if *found == 0 {
                "Preparando o download\u{2026}".to_string()
            } else {
                format!("Preparando o download\u{2026} {found} itens encontrados")
            };
        }
        let pct = (download_fraction(*index, *count, *done, *total) * 100.0) as u32;
        let sizes = format!("{} de {}", human_size(*done), human_size(*total));
        if *count > 1 {
            format!(
                "Baixando {}/{count}: {} \u{00b7} {pct}% \u{00b7} {sizes}",
                index + 1,
                show_path(name)
            )
        } else {
            format!("Baixando {} \u{00b7} {pct}% \u{00b7} {sizes}", show_path(name))
        }
    }
}

/// Fracao concluida de um download: pelos bytes, ou pelos arquivos quando o
/// lote so tem arquivos vazios.
fn download_fraction(index: usize, count: usize, done: u64, total: u64) -> f32 {
    if total > 0 {
        (done as f32 / total as f32).min(1.0)
    } else if count > 0 {
        (index as f32 / count as f32).min(1.0)
    } else {
        0.0
    }
}

/// Se o navegador SFTP pode pedir um download agora (botao, menu e Ctrl+S).
#[derive(Clone, Copy, PartialEq)]
enum DlAvail {
    Ready,
    /// Outro download perguntando ou em andamento neste painel.
    Busy,
    /// Sessao ainda nao conectada (ou encerrada).
    Offline,
}

/// Download pedido no navegador, aguardando a escolha da pasta de destino.
struct PendingDownload {
    /// Painel que pediu.
    path: Vec<usize>,
    /// Pasta remota exibida quando o pedido foi feito.
    remote_dir: String,
    picks: Vec<download::Pick>,
}

/// Conexao de um painel SFTP, para colar: endereco (sem diferenciar
/// maiusculas), porta e usuario usados no connect (mesma conta, mesmas
/// permissoes). `label` so aparece nas mensagens.
#[derive(Clone, Debug)]
struct SftpOrigin {
    host: String,
    port: u16,
    user: String,
    label: String,
}

impl SftpOrigin {
    fn of(h: &Host) -> Self {
        SftpOrigin {
            host: h.host.trim().to_lowercase(),
            port: h.port,
            user: h.username.clone(),
            label: display_name(h),
        }
    }

    fn same(&self, o: &SftpOrigin) -> bool {
        self.host == o.host && self.port == o.port && self.user == o.user
    }
}

/// Copiar (Ctrl+C) ou recortar para mover (Ctrl+X).
#[derive(Clone, Copy, Debug, PartialEq)]
enum ClipMode {
    Copy,
    Cut,
}

/// Item copiado ou recortado no navegador SFTP.
#[derive(Clone, Debug)]
struct ClipItem {
    path: String,
    name: String,
    /// Pasta de verdade (um link para pasta nao conta).
    is_dir: bool,
}

/// Area de transferencia de arquivos do app (nunca a do Windows): o que
/// Ctrl+C/Ctrl+X guardaram, a colar com Ctrl+V num painel da mesma conexao.
#[derive(Clone, Debug)]
struct FsClip {
    mode: ClipMode,
    origin: SftpOrigin,
    /// Pasta de onde sairam os itens (todos sao entradas dela).
    src_dir: String,
    items: Vec<ClipItem>,
    /// Nomes, para esmaecer os recortados na pasta de origem.
    names: std::collections::BTreeSet<String>,
}

/// Pedido de copiar/recortar/colar vindo do navegador.
enum ClipCmd {
    Copy(Vec<ClipItem>),
    Cut(Vec<ClipItem>),
    Paste,
    Clear,
}

/// Ctrl+V repetido (tecla segurada, ou o evento de colar e a soltura do V
/// no mesmo aperto) so cola uma vez dentro deste tempo.
const PASTE_LATCH: std::time::Duration = std::time::Duration::from_millis(1500);

/// Colar (copiar/mover no servidor) num painel SFTP, visto pela UI.
struct PasteUi {
    /// Identifica o lote nos eventos (0 = so uma mensagem local). O lote de
    /// copiar e apagar da oferta reusa o id.
    id: u64,
    /// O que o usuario pediu (copiar ou mover; o "copiar e apagar" da oferta
    /// continua sendo um mover para as mensagens).
    op: paste::PasteOp,
    dest_dir: String,
    /// Conexao do painel (para atualizar os paineis dela no fim).
    origin: Option<SftpOrigin>,
    /// Recorte tirado do app ao colar; volta se nada sair da origem.
    restore: Option<FsClip>,
    /// Ja relatado antes deste lote: invalidos, "pular existentes" e o 1o
    /// lote de um mover que virou oferta.
    pre_failed: Vec<(String, String)>,
    pre_skipped: Vec<(String, String)>,
    pre_moved: Vec<(String, String)>,
    pre_done: usize,
    pre_count: usize,
    stage: PasteStage,
}

enum PasteStage {
    /// Ha itens que ja existem no destino: dialogo aberto.
    Asking(paste::Prepared),
    Running {
        cancel: download::Cancel,
        cancelling: bool,
        scanning: bool,
        found: usize,
        phase: paste::PastePhase,
        index: usize,
        count: usize,
        name: String,
        done: u64,
        total: u64,
    },
    /// Mover terminou com itens em outro disco: pergunta se copia e apaga.
    Offer(Box<paste::PasteReport>),
    Done {
        text: String,
        detail: String,
        tone: Tone,
        at: Instant,
    },
}

impl PasteStage {
    fn done(text: String, detail: String, tone: Tone) -> Self {
        PasteStage::Done {
            text,
            detail,
            tone,
            at: Instant::now(),
        }
    }
}

impl PasteUi {
    /// Mensagem avulsa (nada foi mandado a sessao).
    fn notice(text: &str, tone: Tone) -> Self {
        PasteUi {
            id: 0,
            op: paste::PasteOp::Copy,
            dest_dir: String::new(),
            origin: None,
            restore: None,
            pre_failed: Vec::new(),
            pre_skipped: Vec::new(),
            pre_moved: Vec::new(),
            pre_done: 0,
            pre_count: 0,
            stage: PasteStage::done(text.to_string(), String::new(), tone),
        }
    }

    /// Perguntando, rodando ou com a oferta aberta.
    fn busy(&self) -> bool {
        !matches!(self.stage, PasteStage::Done { .. })
    }

    /// Janela aberta (conflito ou oferta): o teclado do navegador para.
    fn asking(&self) -> bool {
        matches!(self.stage, PasteStage::Asking(_) | PasteStage::Offer(_))
    }

    /// Texto do rodape em andamento.
    fn running_text(&self) -> String {
        let PasteStage::Running {
            cancelling,
            scanning,
            found,
            phase,
            index,
            count,
            name,
            done,
            total,
            ..
        } = &self.stage
        else {
            return String::new();
        };
        if *cancelling {
            return "Cancelando\u{2026}".to_string();
        }
        let moving = self.op != paste::PasteOp::Copy;
        if *scanning || name.is_empty() {
            let base = if moving {
                "Preparando para mover\u{2026}"
            } else {
                "Preparando a cópia\u{2026}"
            };
            return if *found == 0 {
                base.to_string()
            } else {
                format!("{base} {found} itens encontrados")
            };
        }
        let nome = show_path(name);
        match phase {
            paste::PastePhase::Moving if *count > 1 => format!("Movendo {}/{count}: {nome}", index + 1),
            paste::PastePhase::Moving => format!("Movendo {nome}"),
            paste::PastePhase::Deleting => format!("Apagando os originais: {nome}"),
            paste::PastePhase::Copying => {
                let verbo = if moving { "Movendo" } else { "Copiando" };
                let pct = (download_fraction(*index, *count, *done, *total) * 100.0) as u32;
                let sizes = format!("{} de {}", human_size(*done), human_size(*total));
                if *count > 1 {
                    format!("{verbo} {}/{count}: {nome} \u{00b7} {pct}% \u{00b7} {sizes}", index + 1)
                } else {
                    format!("{verbo} {nome} \u{00b7} {pct}% \u{00b7} {sizes}")
                }
            }
        }
    }
}

/// O que fazer nos paineis da conexao depois de um colar.
struct PasteAfter {
    origin: Option<SftpOrigin>,
    refresh: Vec<String>,
    moved: Vec<(String, String)>,
    restore: Option<FsClip>,
}

/// Se da para colar no painel agora (e por que nao).
#[derive(Clone, Copy, PartialEq, Debug)]
enum PasteAvail {
    Ready,
    Empty,
    OtherServer,
    Offline,
    Busy,
    SameFolder,
    IntoItself,
}

fn paste_avail(clip: Option<&FsClip>, pane: &Pane) -> PasteAvail {
    let Some(c) = clip else {
        return PasteAvail::Empty;
    };
    if !pane.origin.as_ref().is_some_and(|o| o.same(&c.origin)) {
        return PasteAvail::OtherServer;
    }
    let Some(exp) = pane.explorer.as_ref() else {
        return PasteAvail::Offline;
    };
    if !matches!(pane.state, SessionState::Connected) || exp.cur_path.is_empty() || exp.loading {
        return PasteAvail::Offline;
    }
    if pane.paste.as_ref().is_some_and(PasteUi::busy) {
        return PasteAvail::Busy;
    }
    if c.mode == ClipMode::Cut && paste::same_dir(&c.src_dir, &exp.cur_path) {
        return PasteAvail::SameFolder;
    }
    if c.items.iter().all(|i| i.is_dir && paste::is_inside(&exp.cur_path, &i.path)) {
        return PasteAvail::IntoItself;
    }
    PasteAvail::Ready
}

/// Resposta das janelas do colar.
enum PasteAnswer {
    Conflict(ConflictChoice),
    Offer(bool),
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
            upload: None,
            host_key: None,
            download: None,
            origin: None,
            paste: None,
        }
    }
}

/// Uma entrada (arquivo ou pasta) do diretorio atual no navegador SFTP.
struct FsNode {
    name: String,
    /// Nome pronto para a tela (`download::safe_text`): sem controle nem
    /// bidi cru. Calculado uma vez, na chegada da listagem.
    label: String,
    path: String,
    /// Tipo efetivo (link resolvido vale pelo destino).
    kind: sftp::EntryKind,
    /// `Some` quando a entrada e um link simbolico.
    link: Option<sftp::LinkInfo>,
    size: u64,
    /// Bits de permissao (modo POSIX, ex.: 0o644); `None` se desconhecido.
    mode: Option<u32>,
    /// Nome (ou id) do proprietario e do grupo, ja resolvidos pelo backend.
    owner: String,
    group: String,
    /// Data da ultima alteracao ja formatada (DD/MM/AAAA; vazia se ausente).
    /// Pre-formatada na chegada da listagem para nao recalcular por quadro.
    date: String,
}

impl FsNode {
    /// Pasta, ou link para pasta (entra com Enter/duplo clique).
    fn is_dir(&self) -> bool {
        self.kind == sftp::EntryKind::Dir
    }

    /// Pasta de verdade (nao link): so ela e excluida com rmdir.
    fn is_real_dir(&self) -> bool {
        self.is_dir() && self.link.is_none()
    }
}

/// O que Enter/duplo clique fazem numa entrada.
#[derive(Debug, PartialEq)]
enum OpenAction {
    /// Entrar na pasta (caminho logico: um link fica no caminho).
    Navigate(String),
    /// Ver o arquivo (visualizador; o tipo final e conferido na leitura).
    View,
    /// Nao abre: mostra o aviso.
    Warn(String),
}

/// Acao de Enter/duplo clique conforme o tipo da entrada.
fn open_action(n: &FsNode) -> OpenAction {
    use sftp::{EntryKind, LinkState};
    if n.is_dir() {
        return OpenAction::Navigate(n.path.clone());
    }
    match (n.kind, n.link.as_ref().map(|l| l.state)) {
        (EntryKind::File, _) => OpenAction::View,
        (EntryKind::Special(_), _) => OpenAction::Warn(warn_special(&n.name)),
        (_, Some(LinkState::Broken)) => OpenAction::Warn(warn_broken_link(&n.name)),
        (_, Some(LinkState::Denied)) => OpenAction::Warn(warn_link_denied(&n.name)),
        // Nao verificado ou sem tipo: a leitura confere (stat) e decide.
        _ => OpenAction::View,
    }
}

/// Nome de uma entrada dentro de um aviso.
fn warn_name(name: &str) -> String {
    download::safe_text(name, 80)
}

fn warn_special(name: &str) -> String {
    format!(
        "\u{201C}{}\u{201D} é um arquivo especial (fifo, socket ou dispositivo) e não pode ser visualizado.",
        warn_name(name)
    )
}

fn warn_broken_link(name: &str) -> String {
    format!(
        "\u{201C}{}\u{201D} é um link simbólico quebrado: o destino não existe (ou os links formam um ciclo).",
        warn_name(name)
    )
}

fn warn_link_denied(name: &str) -> String {
    format!(
        "Sem permissão para acessar o destino do link \u{201C}{}\u{201D}.",
        warn_name(name)
    )
}

/// Permissoes e proprietario/grupo so com o modo conhecido: num link, so
/// com o destino resolvido (a alteracao vale para ele).
fn attrs_editable(n: &FsNode) -> bool {
    n.mode.is_some()
        && n.link
            .as_ref()
            .is_none_or(|l| l.state == sftp::LinkState::Ok)
}

/// Dica de Permissoes/Proprietario desativados num link sem destino.
const LINK_ATTRS_OFF: &str = "O destino do link não está disponível";

/// Aparencia de uma linha da listagem.
struct RowLook {
    icon: egui::ImageSource<'static>,
    color: egui::Color32,
    /// "/" no fim do nome (pastas e links para pasta).
    slash: bool,
}

/// Icone, cor e "/" por tipo: pastas (e links para pasta) em ambar com "/";
/// links com seta no icone; link quebrado em vermelho.
fn row_look(n: &FsNode) -> RowLook {
    use sftp::{EntryKind, LinkState};
    let link = n.link.as_ref().map(|l| l.state);
    let (icon, color) = match (n.kind, link) {
        (EntryKind::Dir, None) => (ICON_FOLDER, FOLDER_FG),
        (EntryKind::Dir, Some(_)) => (ICON_FOLDER_SYMLINK, FOLDER_FG),
        (EntryKind::File, None) => (ICON_FILE, CARD_TEXT),
        (EntryKind::File, Some(_)) => (ICON_FILE_SYMLINK, CARD_TEXT),
        (_, Some(LinkState::Broken)) => (ICON_FILE_SYMLINK, DANGER),
        (_, Some(_)) => (ICON_FILE_SYMLINK, TEXT_WEAK),
        (_, None) => (ICON_FILE, TEXT_WEAK),
    };
    RowLook {
        icon,
        color,
        slash: n.is_dir(),
    }
}

/// Nome de um arquivo especial nas dicas.
fn special_name(s: sftp::Special) -> &'static str {
    match s {
        sftp::Special::Fifo => "FIFO (pipe nomeado)",
        sftp::Special::Socket => "Socket",
        sftp::Special::CharDev => "Dispositivo de caracteres",
        sftp::Special::BlockDev => "Dispositivo de bloco",
    }
}

/// Dica da linha (so montada com o mouse sobre ela): tipo e alvo do link.
fn row_tip(n: &FsNode) -> Option<String> {
    use sftp::{EntryKind, LinkState};
    let Some(link) = &n.link else {
        return match n.kind {
            EntryKind::Special(s) => Some(special_name(s).to_string()),
            EntryKind::Unknown => Some("Tipo não informado pelo servidor".to_string()),
            _ => None,
        };
    };
    let alvo = if link.target.is_empty() {
        String::new()
    } else {
        format!("\n\u{2192} {}", download::safe_text(&link.target, 200))
    };
    const COLS: &str = "\nColunas: atributos do destino";
    Some(match (link.state, n.kind) {
        (LinkState::Ok, EntryKind::Dir) => format!("Link simbólico para pasta{alvo}{COLS}"),
        (LinkState::Ok, EntryKind::File) => format!("Link simbólico para arquivo{alvo}{COLS}"),
        (LinkState::Ok, EntryKind::Special(s)) => format!(
            "Link simbólico para {}{alvo}{COLS}",
            special_name(s).to_lowercase()
        ),
        (LinkState::Broken, _) => format!(
            "Link quebrado: o destino não existe (ou os links formam um ciclo){alvo}"
        ),
        (LinkState::Denied, _) => {
            format!("Link simbólico: sem permissão para acessar o destino{alvo}")
        }
        _ => format!("Link simbólico (destino não verificado){alvo}"),
    })
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
        /// Alvo, quando o item e um link (a alteracao vale para o destino).
        link_target: Option<String>,
    },
    Chown {
        path: String,
        name: String,
        /// Campos editaveis: nome (ex.: "root") ou id numerico.
        owner_text: String,
        group_text: String,
        /// Alvo, quando o item e um link (a alteracao vale para o destino).
        link_target: Option<String>,
    },
    Delete {
        path: String,
        name: String,
        /// Pasta de verdade (rmdir); um link para pasta e falso.
        is_dir: bool,
        /// Alvo, quando o item e um link (so o link e removido).
        link_target: Option<String>,
    },
}

impl FsDialog {
    fn delete(n: &FsNode) -> Self {
        FsDialog::Delete {
            path: n.path.clone(),
            name: n.name.clone(),
            is_dir: n.is_real_dir(),
            link_target: n.link.as_ref().map(|l| l.target.clone()),
        }
    }

    /// Parte do modo do destino num link resolvido (nunca do 0777 do link).
    fn chmod(n: &FsNode) -> Self {
        let mode = n.mode.unwrap_or(0);
        FsDialog::Chmod {
            path: n.path.clone(),
            name: n.name.clone(),
            mode,
            mode_text: format!("{mode:04o}"),
            link_target: n.link.as_ref().map(|l| l.target.clone()),
        }
    }

    fn chown(n: &FsNode) -> Self {
        FsDialog::Chown {
            path: n.path.clone(),
            name: n.name.clone(),
            owner_text: n.owner.clone(),
            group_text: n.group.clone(),
            link_target: n.link.as_ref().map(|l| l.target.clone()),
        }
    }
}

/// Nota dos dialogos de permissoes/proprietario num link.
fn link_attrs_note(target: &str) -> String {
    format!(
        "É um link simbólico: a alteração vale para o destino (\u{2192} {}).",
        download::safe_text(target, 200)
    )
}

/// Item copiado/recortado a partir de uma entrada da listagem.
fn clip_item(n: &FsNode) -> ClipItem {
    ClipItem {
        path: n.path.clone(),
        name: n.name.clone(),
        is_dir: n.is_real_dir(),
    }
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
    /// Itens a baixar (botao, menu de contexto ou Ctrl+S).
    download: Option<Vec<download::Pick>>,
    /// Caminho digitado na barra, a abrir no servidor: (pedido, caminho).
    goto: Option<(u64, String)>,
    /// Um campo dentro do painel (o do caminho ou a busca do visualizador)
    /// tem o foco do teclado: o painel conta como focado (borda, dicas e
    /// para onde o foco volta depois do Enter/Esc).
    inner_focus: bool,
    /// Arquivo a ler para o visualizador: (pedido, caminho).
    view: Option<(u64, String)>,
    /// Copiar/recortar/colar (Ctrl+C/Ctrl+X/Ctrl+V, menu ou Esc).
    clip: Option<ClipCmd>,
}

impl ExplorerOut {
    fn empty() -> Self {
        ExplorerOut {
            to_list: Vec::new(),
            refresh: false,
            op: None,
            clicked_row: false,
            download: None,
            goto: None,
            inner_focus: false,
            view: None,
            clip: None,
        }
    }
}

/// Recalcula a busca do visualizador depois deste tempo sem digitar.
const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);
/// Fundo (HIGHLIGHT com esta opacidade) da ocorrencia atual da busca: o
/// texto claro por cima continua acima de 4,5:1 de contraste.
const CUR_MATCH_FILL: f32 = 0.35;
/// Tempo do aviso "Copiado" no visualizador.
const COPIED_FOR: std::time::Duration = std::time::Duration::from_millis(1500);

/// Arquivo sendo lido para o visualizador (a leitura corre na sessao; a
/// listagem continua utilizavel ate chegar o resultado).
struct Opening {
    id: u64,
    name: String,
    path: String,
    got: u64,
    total: Option<u64>,
    /// Soltar cancela a leitura no servidor.
    cancel: Option<download::Cancel>,
    /// Recarga (F5) do arquivo ja aberto: o visualizador continua na tela.
    reload: bool,
}

/// Rolagem pedida para o proximo quadro do visualizador (teclado, busca).
#[derive(Default)]
struct ScrollReq {
    x: Option<f32>,
    y: Option<f32>,
}

/// Busca no arquivo aberto (Ctrl+F).
struct ViewSearch {
    query: String,
    /// Ocorrencias (bytes de `ViewDoc::text`).
    matches: Vec<(u32, u32)>,
    /// Havia mais que `viewer::MAX_MATCHES`.
    capped: bool,
    current: Option<usize>,
    /// A consulta mudou e as ocorrencias ainda nao foram recalculadas.
    dirty_at: Option<Instant>,
    /// Ao recalcular, rolar ate a ocorrencia atual (a consulta foi digitada).
    jump: bool,
    /// Pedir o foco (com o texto todo selecionado) no proximo quadro.
    focus: bool,
}

/// Visualizador somente leitura, aberto no lugar da listagem (que continua
/// intacta por baixo: entradas, cursor, marcados).
struct FileViewer {
    doc: Box<viewer::ViewDoc>,
    /// Nome remoto (download e avisos).
    name: String,
    /// Selecao em bytes de `doc.text`: (ancora, ponta), sempre em fronteira
    /// de caractere.
    sel: Option<(u32, u32)>,
    search: Option<ViewSearch>,
    scroll: ScrollReq,
    /// Rolagem, area visivel, altura da linha e largura de um caractere no
    /// ultimo quadro (teclado e busca).
    offset: egui::Vec2,
    view_size: egui::Vec2,
    row_h: f32,
    char_w: f32,
    copied_at: Option<Instant>,
    /// Falha ao recarregar (F5): o conteudo antigo continua na tela.
    reload_error: Option<String>,
}

/// Fonte e cores do texto do visualizador (explicitas: o tema claro do
/// Windows nao interfere).
fn view_row_style() -> viewer::RowStyle {
    viewer::RowStyle {
        font: egui::FontId::monospace(13.0),
        normal: CARD_TEXT,
        ctrl: (HIGHLIGHT, ACCENT_FILL),
        bidi: (DANGER, DANGER.gamma_multiply(0.2)),
    }
}

impl FileViewer {
    fn new(doc: Box<viewer::ViewDoc>, name: String) -> Self {
        FileViewer {
            doc,
            name,
            sel: None,
            search: None,
            // Arquivo novo comeca no topo: a area de texto tem o mesmo id em
            // todos os arquivos do painel, e o egui guardaria a rolagem do
            // anterior.
            scroll: ScrollReq {
                x: Some(0.0),
                y: Some(0.0),
            },
            offset: egui::Vec2::ZERO,
            view_size: egui::Vec2::ZERO,
            row_h: 16.0,
            char_w: 8.0,
            copied_at: None,
            reload_error: None,
        }
    }

    /// Primeira linha exibida no topo da area de texto.
    fn top_row(&self) -> usize {
        (self.offset.y / self.row_h).floor().max(0.0) as usize
    }

    fn max_y(&self) -> f32 {
        (self.doc.rows.len() as f32 * self.row_h - self.view_size.y).max(0.0)
    }

    /// Largura da coluna dos numeros de linha.
    fn gutter_w(&self) -> f32 {
        let digits = self.doc.lines.max(1).to_string().len().max(3);
        digits as f32 * self.char_w + 12.0
    }

    /// Conteudo novo (F5), mantendo a linha do topo e a consulta da busca.
    fn replace_doc(&mut self, doc: Box<viewer::ViewDoc>, now: Instant) {
        let top = self.top_row() as f32 * self.row_h;
        self.doc = doc;
        self.sel = None;
        self.reload_error = None;
        self.scroll.y = Some(top.min(self.max_y()));
        if let Some(s) = &mut self.search {
            s.dirty_at = Some(now.checked_sub(SEARCH_DEBOUNCE).unwrap_or(now));
            s.jump = false;
        }
    }

    /// Rolagem pelo teclado: linhas, paginas (linhas visiveis - 1), colunas
    /// (4 por seta) e as pontas.
    fn scroll_keys(&mut self, dy: i32, dpage: i32, dx: i32, home: bool, end: bool) {
        let rh = self.row_h;
        let page = ((self.view_size.y / rh).floor() - 1.0).max(1.0) * rh;
        let max_y = self.max_y();
        let mut y = self.scroll.y.unwrap_or(self.offset.y) + dy as f32 * rh + dpage as f32 * page;
        let mut x = self.scroll.x.unwrap_or(self.offset.x) + dx as f32 * 4.0 * self.char_w;
        if home {
            (x, y) = (0.0, 0.0);
        }
        if end {
            (x, y) = (0.0, max_y);
        }
        self.scroll.y = Some(y.clamp(0.0, max_y));
        self.scroll.x = Some(x.max(0.0));
    }

    /// Copia a selecao (texto seguro, ver `viewer::copy_text`).
    fn copy_selection(&mut self, ctx: &egui::Context, now: Instant) {
        if let Some((a, b)) = self.sel.filter(|(a, b)| a != b) {
            ctx.copy_text(viewer::copy_text(&self.doc, a, b));
            self.copied_at = Some(now);
        }
    }

    /// Abre a barra de busca (ou foca a que ja esta aberta).
    fn open_search(&mut self) {
        match &mut self.search {
            Some(s) => s.focus = true,
            None => {
                self.search = Some(ViewSearch {
                    query: String::new(),
                    matches: Vec::new(),
                    capped: false,
                    current: None,
                    dirty_at: None,
                    jump: false,
                    focus: true,
                })
            }
        }
    }

    /// Consulta mudada: recalcula depois de `SEARCH_DEBOUNCE` sem digitar, ou
    /// ja com `force` (Enter/F3, que tambem rolam ate a atual). Devolve
    /// verdadeiro se recalculou.
    fn refresh_search(&mut self, now: Instant, force: bool, ctx: &egui::Context) -> bool {
        let Some(s) = &mut self.search else {
            return false;
        };
        let Some(t) = s.dirty_at else {
            return false;
        };
        let passou = now.saturating_duration_since(t);
        if passou >= SEARCH_DEBOUNCE || force {
            s.jump |= force;
            self.recompute_search();
            true
        } else {
            ctx.request_repaint_after(SEARCH_DEBOUNCE - passou);
            false
        }
    }

    /// Recalcula as ocorrencias; a atual passa a ser a primeira a partir da
    /// linha do topo.
    fn recompute_search(&mut self) {
        let top = self.top_row();
        let Some(s) = &mut self.search else {
            return;
        };
        let (m, capped) = viewer::find_all(&self.doc.text, &s.query, viewer::MAX_MATCHES);
        s.matches = m;
        s.capped = capped;
        s.dirty_at = None;
        s.current = (!s.matches.is_empty()).then(|| {
            s.matches
                .iter()
                .position(|&(a, _)| viewer::row_of(&self.doc.rows, a) >= top)
                .unwrap_or(0)
        });
        let jump = std::mem::take(&mut s.jump);
        if let (true, Some(k)) = (jump, s.current) {
            self.show_match(k);
        }
    }

    /// Proxima (ou anterior) ocorrencia, dando a volta.
    fn step_match(&mut self, forward: bool) {
        let Some(s) = &mut self.search else {
            return;
        };
        let n = s.matches.len();
        if n == 0 {
            return;
        }
        let k = match s.current {
            None => 0,
            Some(c) if forward => (c + 1) % n,
            Some(c) => (c + n - 1) % n,
        };
        s.current = Some(k);
        self.show_match(k);
    }

    /// Rola ate a ocorrencia `k`: linha centralizada e, se preciso, a coluna.
    fn show_match(&mut self, k: usize) {
        let Some(&(a, _)) = self.search.as_ref().and_then(|s| s.matches.get(k)) else {
            return;
        };
        let r = viewer::row_of(&self.doc.rows, a);
        let rh = self.row_h;
        let vh = self.view_size.y.max(rh);
        self.scroll.y = Some((r as f32 * rh - (vh - rh) / 2.0).clamp(0.0, self.max_y()));
        let Some(&row) = self.doc.rows.get(r) else {
            return;
        };
        let map = viewer::RowView::build(&self.doc.text, row, &view_row_style()).map;
        let col = map.partition_point(|&b| b < a) as f32 * self.char_w;
        let text_w = (self.view_size.x - self.gutter_w() - 6.0).max(self.char_w * 8.0);
        let x = self.offset.x;
        if col < x || col > x + text_w - 4.0 * self.char_w {
            self.scroll.x = Some((col - text_w / 3.0).max(0.0));
        }
    }
}

/// Resultado da area de texto do visualizador num quadro.
#[derive(Default)]
struct AreaOut {
    /// Clique no texto: o painel toma o foco.
    clicked: bool,
    /// "Baixar..." no menu de contexto.
    download: bool,
    /// "Buscar..." no menu de contexto.
    search: bool,
}

impl FileViewer {
    /// Area de texto: so as linhas visiveis sao montadas e desenhadas
    /// (arquivos com muitas linhas nao travam), com os numeros de linha fixos
    /// a esquerda. Arrastar seleciona (e copia ao soltar, como no terminal);
    /// Shift+clique estende; duplo clique pega a palavra; triplo, a linha.
    fn text_area(&mut self, ui: &mut egui::Ui, salt: egui::Id, dl: DlAvail, now: Instant) -> AreaOut {
        let mut res = AreaOut::default();
        let style = view_row_style();
        let font = style.font.clone();
        let (row_h, char_w) =
            ui.fonts(|f| (f.row_height(&font).ceil() + 1.0, f.glyph_width(&font, '0')));
        self.row_h = row_h;
        self.char_w = char_w;
        let gutter = self.gutter_w();
        let text_x0 = gutter + 6.0;
        let ctx = ui.ctx().clone();
        let FileViewer {
            doc,
            sel,
            search,
            scroll,
            copied_at,
            offset,
            view_size,
            ..
        } = &mut *self;
        let doc: &viewer::ViewDoc = doc;
        let n_rows = doc.rows.len();
        let content = egui::vec2(
            text_x0 + doc.max_cols as f32 * char_w + 16.0,
            n_rows as f32 * row_h,
        );
        let mut area = egui::ScrollArea::both()
            .id_salt(salt.with("texto"))
            .auto_shrink([false, false])
            .drag_to_scroll(false);
        if let Some(y) = scroll.y.take() {
            area = area.vertical_scroll_offset(y);
        }
        if let Some(x) = scroll.x.take() {
            area = area.horizontal_scroll_offset(x);
        }
        let shown = egui::Frame::NONE.fill(FIELD_BG).show(ui, |ui| {
            area.show_viewport(ui, |ui, viewport| {
                ui.set_min_size(content);
                let origin = ui.max_rect().min;
                let clip = ui.clip_rect();
                let resp = ui.interact(clip, salt.with("area"), egui::Sense::click_and_drag());
                let hit = |p: egui::Pos2| view_hit(&ctx, doc, &style, origin, text_x0, row_h, p);
                let shift = ui.input(|i| i.modifiers.shift);

                // --- Mouse ---
                if resp.drag_started() {
                    let from = ui
                        .input(|i| i.pointer.press_origin())
                        .or(resp.interact_pointer_pos());
                    if let Some(b) = from.and_then(hit) {
                        *sel = match *sel {
                            Some((a, _)) if shift => Some((a, b)),
                            _ => Some((b, b)),
                        };
                    }
                    res.clicked = true;
                }
                if resp.dragged() {
                    if let Some(p) = resp.interact_pointer_pos() {
                        if let (Some(b), Some(s)) = (hit(p), sel.as_mut()) {
                            s.1 = b;
                        }
                        // Fora da area, a vista rola sozinha.
                        let dy = if p.y < clip.top() {
                            -row_h
                        } else if p.y > clip.bottom() {
                            row_h
                        } else {
                            0.0
                        };
                        let dx = if p.x < clip.left() + gutter {
                            -2.0 * char_w
                        } else if p.x > clip.right() {
                            2.0 * char_w
                        } else {
                            0.0
                        };
                        if dy != 0.0 {
                            scroll.y = Some((viewport.min.y + dy).max(0.0));
                        }
                        if dx != 0.0 {
                            scroll.x = Some((viewport.min.x + dx).max(0.0));
                        }
                        if dy != 0.0 || dx != 0.0 {
                            ctx.request_repaint();
                        }
                    }
                }
                if resp.drag_stopped() {
                    if let Some((a, b)) = (*sel).filter(|(a, b)| a != b) {
                        ctx.copy_text(viewer::copy_text(doc, a, b));
                        *copied_at = Some(now);
                    }
                }
                if resp.clicked() {
                    *sel = match (resp.interact_pointer_pos().and_then(hit), *sel) {
                        (Some(b), Some((a, _))) if shift => Some((a, b)),
                        _ => None,
                    };
                    res.clicked = true;
                }
                if resp.double_clicked() {
                    if let Some(b) = resp.interact_pointer_pos().and_then(hit) {
                        *sel = Some(viewer::word_at(&doc.text, b));
                    }
                }
                if resp.triple_clicked() {
                    if let Some(b) = resp.interact_pointer_pos().and_then(hit) {
                        *sel = viewer::line_at(&doc.rows, b);
                    }
                }
                if resp.secondary_clicked() {
                    res.clicked = true;
                }

                // --- Linhas visiveis ---
                let first = (viewport.min.y / row_h).floor().max(0.0) as usize;
                let last = ((viewport.max.y / row_h).ceil().max(0.0) as usize + 1).min(n_rows);
                let painter = ui.painter();
                let sel_n = (*sel).map(|(a, b)| (a.min(b), a.max(b))).filter(|(a, b)| a < b);
                let found = search.as_ref().map(|s| (&s.matches[..], s.current));
                for r in first..last.max(first) {
                    let row = doc.rows[r];
                    let top = origin.y + r as f32 * row_h;
                    let rv = viewer::RowView::build(&doc.text, row, &style);
                    let map = rv.map;
                    let galley = ctx.fonts(|f| f.layout_job(rv.job));
                    let tx = origin.x + text_x0;
                    let x_of = |byte: u32| {
                        let i = map.partition_point(|&b| b < byte);
                        tx + galley.pos_from_ccursor(egui::text::CCursor::new(i)).min.x
                    };
                    let band = |x0: f32, x1: f32| {
                        egui::Rect::from_min_max(egui::pos2(x0, top), egui::pos2(x1, top + row_h))
                    };
                    // Ocorrencias da busca (a atual mais forte e com contorno).
                    if let Some((ms, cur)) = found {
                        let mut k = ms.partition_point(|m| m.1 <= row.start);
                        while k < ms.len() && ms[k].0 < row.end {
                            let (a, b) = ms[k];
                            let rect = band(x_of(a.max(row.start)), x_of(b.min(row.end)));
                            if cur == Some(k) {
                                // A atual se destaca pelo contorno: um fundo
                                // mais forte tiraria o contraste do texto.
                                painter.rect_filled(rect, 2.0, HIGHLIGHT.gamma_multiply(CUR_MATCH_FILL));
                                painter.rect_stroke(
                                    rect,
                                    2.0,
                                    egui::Stroke::new(1.0, HIGHLIGHT),
                                    egui::StrokeKind::Inside,
                                );
                            } else {
                                painter.rect_filled(rect, 2.0, HIGHLIGHT.gamma_multiply(0.25));
                            }
                            k += 1;
                        }
                    }
                    // Selecao (o fim de linha selecionado ganha um caractere).
                    if let Some((a, b)) = sel_n {
                        if a <= row.end && b > row.start {
                            let x0 = x_of(a.max(row.start));
                            let mut x1 = x_of(b.min(row.end));
                            if b > row.end && doc.rows.get(r + 1).is_some_and(|n| n.line > 0) {
                                x1 += char_w;
                            }
                            if x1 > x0 {
                                painter.rect_filled(band(x0, x1), 0.0, ACCENT.gamma_multiply(0.35));
                            }
                        }
                    }
                    let ty = top + ((row_h - galley.size().y) / 2.0).max(0.0);
                    painter.galley(egui::pos2(tx, ty), galley, CARD_TEXT);
                }
                if n_rows == 0 {
                    painter.text(
                        egui::pos2(origin.x + text_x0, origin.y + 6.0),
                        egui::Align2::LEFT_TOP,
                        "(arquivo vazio)",
                        egui::FontId::proportional(13.0),
                        TEXT_WEAK,
                    );
                }
                // Numeros de linha, fixos a esquerda (o texto rola por baixo).
                let gx = clip.left();
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(gx, clip.top()),
                        egui::pos2(gx + gutter, clip.bottom()),
                    ),
                    0.0,
                    FIELD_BG,
                );
                let num_font = egui::FontId::monospace(12.0);
                for r in first..last.max(first) {
                    let line = doc.rows[r].line;
                    let t = if line > 0 {
                        line.to_string()
                    } else {
                        "\u{00b7}".to_string()
                    };
                    painter.text(
                        egui::pos2(gx + gutter - 6.0, origin.y + (r as f32 + 0.5) * row_h),
                        egui::Align2::RIGHT_CENTER,
                        t,
                        num_font.clone(),
                        TEXT_WEAK,
                    );
                }
                painter.line_segment(
                    [egui::pos2(gx + gutter, clip.top()), egui::pos2(gx + gutter, clip.bottom())],
                    egui::Stroke::new(1.0, CARD_BORDER),
                );

                // --- Menu de contexto ---
                resp.context_menu(|ui| {
                    let bg = style_context_menu(ui);
                    let has_sel = sel.is_some_and(|(a, b)| a != b);
                    if ui.add_enabled_ui(has_sel, |ui| menu_text_item(ui, "Copiar")).inner {
                        if let Some((a, b)) = *sel {
                            ctx.copy_text(viewer::copy_text(doc, a, b));
                            *copied_at = Some(now);
                        }
                        ui.close_menu();
                    }
                    if menu_text_item(ui, "Selecionar tudo") {
                        *sel = Some((0, doc.text.len() as u32));
                        ui.close_menu();
                    }
                    ui.add_space(2.0);
                    ui.separator();
                    ui.add_space(2.0);
                    if menu_item(ui, ICON_SEARCH, "Buscar\u{2026}", CARD_TEXT) {
                        res.search = true;
                        ui.close_menu();
                    }
                    let baixar = ui
                        .add_enabled_ui(dl == DlAvail::Ready, |ui| {
                            menu_item(ui, ICON_DOWNLOAD, "Baixar\u{2026}", CARD_TEXT)
                        })
                        .inner;
                    if baixar {
                        res.download = true;
                        ui.close_menu();
                    }
                    paint_menu_bg(ui, bg);
                });
            })
        });
        let so = shown.inner;
        *offset = so.state.offset;
        *view_size = so.inner_rect.size();

        // "Copiado" por um instante, no canto da area de texto.
        if let Some(t) = *copied_at {
            let passou = now.saturating_duration_since(t);
            if passou < COPIED_FOR {
                corner_pill(ui, so.inner_rect, "Copiado", CARD_TEXT, ACCENT);
                ui.ctx().request_repaint_after(COPIED_FOR - passou);
            } else {
                *copied_at = None;
            }
        }
        res
    }
}

/// Byte do texto sob o ponteiro: a linha pelo y (limitada ao documento) e a
/// coluna pelo galley da linha (mapa do exibido para o byte de origem).
fn view_hit(
    ctx: &egui::Context,
    doc: &viewer::ViewDoc,
    style: &viewer::RowStyle,
    origin: egui::Pos2,
    text_x0: f32,
    row_h: f32,
    pos: egui::Pos2,
) -> Option<u32> {
    let last = doc.rows.len().checked_sub(1)?;
    let r = (((pos.y - origin.y) / row_h).floor().max(0.0) as usize).min(last);
    let rv = viewer::RowView::build(&doc.text, doc.rows[r], style);
    let galley = ctx.fonts(|f| f.layout_job(rv.job));
    let x = pos.x - (origin.x + text_x0);
    let i = galley
        .cursor_from_pos(egui::vec2(x, galley.size().y / 2.0))
        .ccursor
        .index;
    rv.map.get(i).or(rv.map.last()).copied()
}

/// Pilula com texto no canto inferior direito de `area` (por cima, sem
/// ocupar espaco).
fn corner_pill(ui: &egui::Ui, area: egui::Rect, text: &str, fg: egui::Color32, edge: egui::Color32) {
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), egui::FontId::proportional(12.0), fg);
    let size = galley.size() + egui::vec2(20.0, 10.0);
    let rect = egui::Rect::from_min_size(
        egui::pos2(area.right() - size.x - 20.0, area.bottom() - size.y - 12.0),
        size,
    );
    let painter = ui.painter();
    painter.rect(rect, 6.0, MENU_BG, egui::Stroke::new(1.0, edge), egui::StrokeKind::Inside);
    painter.galley(
        egui::pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );
}

/// Numero com ponto de milhar ("200.000").
fn thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// Faixa fixa de aviso do visualizador (ambar, sem X; o texto quebra linha),
/// com um botao opcional (texto, ativo) embaixo. Devolve verdadeiro quando o
/// botao foi clicado.
fn info_band(ui: &mut egui::Ui, text: &str, button: Option<(&str, bool)>) -> bool {
    let mut clicked = false;
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(HIGHLIGHT.gamma_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, HIGHLIGHT))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(egui::RichText::new(text).color(HIGHLIGHT)).wrap());
            if let Some((b, enabled)) = button {
                ui.add_space(2.0);
                let btn = egui::Button::new(egui::RichText::new(b).color(HIGHLIGHT))
                    .fill(egui::Color32::TRANSPARENT)
                    .stroke(egui::Stroke::new(1.0, HIGHLIGHT));
                clicked = ui.add_enabled(enabled, btn).clicked();
            }
        });
    clicked
}

/// Faixa "Abrindo" (leitura para o visualizador em andamento), com o
/// andamento e um X. Devolve verdadeiro quando o X foi clicado (cancela).
fn opening_band(ui: &mut egui::Ui, op: &Opening) -> bool {
    let mut cancel = false;
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(ACCENT.gamma_multiply(0.12))
        .stroke(egui::Stroke::new(1.0, CARD_BORDER))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            // Na largura do painel: o X, "Esc cancela" e o andamento ficam a
            // direita e o nome ocupa o que sobrar (cortado com "…"); num
            // painel estreito o andamento, e depois o "Esc cancela", saem.
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                let texto = format!("Abrindo \u{201C}{}\u{201D}\u{2026}", warn_name(&op.name));
                let andamento = match op.total.filter(|t| *t > 0) {
                    Some(t) => format!("{} de {}", human_size(op.got), human_size(t)),
                    None if op.got > 0 => human_size(op.got),
                    None => String::new(),
                };
                const ESC: &str = "Esc cancela";
                let small = egui::FontId::proportional(11.0);
                let gap = ui.spacing().item_spacing.x;
                let width = |t: &str| {
                    ui.fonts(|f| f.layout_no_wrap(t.to_string(), small.clone(), TEXT_WEAK).size().x)
                };
                let (w_esc, w_and) = (width(ESC) + gap, width(&andamento) + gap);
                let free = ui.available_width() - 14.0 - gap;
                const MIN_NAME: f32 = 60.0;
                let show_and = !andamento.is_empty() && free - w_esc - w_and >= MIN_NAME;
                let show_esc = free - w_esc - if show_and { w_and } else { 0.0 } >= MIN_NAME;
                let name_w = (free
                    - if show_esc { w_esc } else { 0.0 }
                    - if show_and { w_and } else { 0.0 })
                .max(24.0);
                ui.allocate_ui(egui::vec2(name_w, 18.0), |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(texto).color(CARD_TEXT))
                            .truncate()
                            .selectable(false),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let x = egui::ImageButton::new(
                        egui::Image::new(ICON_CLOSE)
                            .fit_to_exact_size(egui::vec2(14.0, 14.0))
                            .tint(TEXT_WEAK),
                    )
                    .frame(false);
                    if ui.add(x).on_hover_text("Cancelar (Esc)").clicked() {
                        cancel = true;
                    }
                    if show_esc {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(ESC).font(small.clone()).color(TEXT_WEAK),
                            )
                            .selectable(false),
                        );
                    }
                    if show_and {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(andamento).font(small.clone()).color(TEXT_WEAK),
                            )
                            .selectable(false),
                        );
                    }
                });
            });
        });
    cancel
}

/// Item de menu de contexto so com texto, alinhado aos que tem icone.
fn menu_text_item(ui: &mut egui::Ui, text: &str) -> bool {
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(MENU_ITEM_W, 24.0), egui::Sense::click());
    if ui.is_enabled() && resp.hovered() {
        ui.painter()
            .rect_filled(rect, 6.0, ACCENT.gamma_multiply(0.30));
    }
    let left = ui.spacing().button_padding.x + 16.0 + ui.spacing().icon_spacing;
    ui.painter().text(
        egui::pos2(rect.left() + left, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::TextStyle::Button.resolve(ui.style()),
        CARD_TEXT,
    );
    resp.clicked()
}

/// Texto do aviso quando o arquivo nao abre no visualizador; `true` = aviso
/// (faixa ambar), `false` = falha (faixa vermelha).
fn view_error_text(e: &ViewError, name: &str) -> (String, bool) {
    let n = warn_name(name);
    match e {
        ViewError::Special => (warn_special(name), true),
        ViewError::BrokenLink => (warn_broken_link(name), true),
        ViewError::BadLink(_) => (
            format!(
                "Não foi possível seguir o link \u{201C}{n}\u{201D} (em ciclo ou com destino inacessível)."
            ),
            true,
        ),
        ViewError::UnknownType | ViewError::IsDir => (
            format!(
                "O servidor não informou o tipo de \u{201C}{n}\u{201D}; por segurança ele não foi aberto."
            ),
            true,
        ),
        ViewError::Binary => (
            format!(
                "\u{201C}{n}\u{201D} parece ser um arquivo binário e não foi aberto. Para usá-lo, baixe-o (Ctrl+S)."
            ),
            true,
        ),
        ViewError::Draining => (
            format!(
                "\u{201C}{n}\u{201D} não foi aberto: é uma interface do kernel cuja leitura bloqueia ou consome os dados."
            ),
            true,
        ),
        ViewError::Denied => (format!("Sem permissão para ler \u{201C}{n}\u{201D}."), false),
        ViewError::NotFound => (format!("\u{201C}{n}\u{201D} não existe mais no servidor."), false),
        ViewError::Timeout => (
            format!("O servidor demorou demais para enviar \u{201C}{n}\u{201D}."),
            false,
        ),
        ViewError::Remote(msg) => (
            format!(
                "Não foi possível abrir \u{201C}{n}\u{201D}: {}",
                download::safe_text(msg, 300)
            ),
            false,
        ),
        ViewError::Internal => (format!("Erro interno ao abrir \u{201C}{n}\u{201D}."), false),
    }
}

/// Mensagem quando a sessao acabou antes de pedir a leitura.
const VIEW_SESSION_GONE: &str = "A sessão SFTP foi encerrada.";

/// Faixa do arquivo com bytes invalidos (ver `viewer::INVALID_MARK`).
const LOSSY_BAND: &str =
    "Alguns bytes não são UTF-8 válidos e aparecem como \u{201C}?\u{201D} em destaque.";

/// Edicao do caminho na barra do navegador (clique na barra ou Ctrl+L).
struct PathEdit {
    /// Texto no campo.
    text: String,
    /// Primeiro quadro: pede o foco e seleciona o texto todo (uma vez).
    fresh: bool,
    /// Erro da ultima tentativa, mostrado sob a barra; some ao editar.
    error: Option<String>,
    /// Pedido em andamento no servidor: (numero, caminho absoluto).
    pending: Option<(u64, String)>,
    /// Caminho sem os espacos das pontas, tentado se o exato nao abrir.
    retry: Option<String>,
}

/// Tempo sem digitar que zera o prefixo da busca por letras.
const TYPEAHEAD_RESET: std::time::Duration = std::time::Duration::from_millis(1000);

/// Ids dos campos de texto de um navegador (`id_salt` dele): o do caminho
/// (barra) e o da busca do visualizador.
fn explorer_field_ids(id_salt: &impl std::hash::Hash) -> (egui::Id, egui::Id) {
    (
        egui::Id::new(("sftp_path_edit", id_salt)),
        egui::Id::new(("sftp_viewer", id_salt)).with("busca"),
    )
}

/// Estado do navegador de arquivos SFTP de um painel: mostra apenas o conteudo
/// do diretorio atual; navegar entra/sai de pastas (estilo file explorer).
///
/// Linhas da listagem: a ".." (pasta acima; so fora da raiz) seguida das
/// entradas. O cursor fica numa entrada (`sel`) ou na ".." (`on_up`); a ".."
/// nunca e marcada, baixada, renomeada nem excluida.
struct FileExplorer {
    /// Diretorio sendo exibido; vazio ate o `Connected` chegar.
    cur_path: String,
    /// Pasta inicial do usuario (do `Connected`), para o "~" da barra.
    home: String,
    entries: Vec<FsNode>,
    error: Option<String>,
    /// Verdadeiro enquanto se aguarda a listagem do diretorio atual.
    loading: bool,
    /// Dialogo flutuante de gerenciamento (renomear/permissoes/excluir).
    dialog: Option<FsDialog>,
    /// Aviso (faixa ambar, ex.: link quebrado); falhas vao em `error`.
    notice: Option<String>,
    /// Edicao do caminho na barra (`None`: a barra mostra o caminho atual).
    path_edit: Option<PathEdit>,
    /// Numero do ultimo pedido de abrir caminho (respostas antigas sao ignoradas).
    goto_seq: u64,
    /// Cursor do teclado (contorno) numa entrada; Enter/F2/Delete agem nele.
    /// `None` com o cursor na ".." ou sem cursor.
    sel: Option<usize>,
    /// Cursor na linha ".." (exclusivo com `sel`).
    on_up: bool,
    /// Nomes marcados na pasta atual (selecao multipla), por nome para
    /// sobreviver a uma nova listagem.
    marked: std::collections::BTreeSet<String>,
    /// Ponta fixa do intervalo do Shift (indice em `entries`).
    anchor: Option<usize>,
    /// Nome a por sob o cursor quando a listagem chegar (a pasta de onde se
    /// voltou, ou o arquivo digitado na barra).
    reselect: Option<String>,
    /// Rolar ate o cursor quando a linha dele for desenhada (uma vez).
    scroll_to_cursor: bool,
    /// Pasta nova na tela: a lista volta ao topo no proximo quadro (o egui
    /// guardaria a rolagem da pasta anterior, com o cursor na ".." fora da
    /// vista).
    scroll_top: bool,
    /// Linhas inteiras visiveis na listagem no ultimo quadro (PageUp/PageDown).
    page_rows: usize,
    /// Busca por letras: prefixo digitado, instante da ultima letra e se
    /// nenhum item comeca com ele.
    typeahead: String,
    typeahead_at: Option<Instant>,
    typeahead_miss: bool,
    /// Numero do ultimo pedido de leitura (respostas antigas sao ignoradas).
    view_seq: u64,
    /// Leitura em andamento para o visualizador (faixa "Abrindo").
    opening: Option<Opening>,
    /// Visualizador aberto no lugar da listagem.
    viewer: Option<FileViewer>,
    /// Tab (ou Shift+Tab) tirado da entrada antes do egui
    /// (`App::take_sftp_tab`), a tratar neste quadro.
    tab: bool,
    /// Widgets cujo foco quer dizer "teclado neste navegador" (a area do
    /// painel, o campo do caminho e a busca do visualizador), do ultimo
    /// quadro; vazio com um dialogo aberto (ali o Tab anda entre os campos).
    keys_home: Vec<egui::Id>,
    /// Trava do Ctrl+V (ver `PASTE_LATCH`).
    paste_latch: Option<Instant>,
    /// Dialogo recem-aberto: no primeiro quadro ele toma o foco do teclado
    /// (senao o foco fica no painel atras dele e so o mouse o alcanca).
    dialog_fresh: bool,
    /// Ha itens copiados/recortados desta conexao (o Esc os descarta) e os
    /// recortados desta pasta (esmaecidos); postos a cada quadro por
    /// `render_node`.
    clip_active: bool,
    cut_names: Option<std::collections::BTreeSet<String>>,
}

impl FileExplorer {
    fn new() -> Self {
        FileExplorer {
            cur_path: String::new(),
            home: String::new(),
            entries: Vec::new(),
            error: None,
            loading: true,
            dialog: None,
            notice: None,
            path_edit: None,
            goto_seq: 0,
            sel: None,
            on_up: false,
            marked: std::collections::BTreeSet::new(),
            anchor: None,
            reselect: None,
            scroll_to_cursor: false,
            scroll_top: false,
            page_rows: 10,
            typeahead: String::new(),
            typeahead_at: None,
            typeahead_miss: false,
            view_seq: 0,
            opening: None,
            viewer: None,
            tab: false,
            keys_home: Vec::new(),
            paste_latch: None,
            dialog_fresh: false,
            clip_active: false,
            cut_names: None,
        }
    }

    /// Sessao conectada: comeca na pasta inicial, com o cursor na "..".
    fn connected(&mut self, home: String) {
        self.home = home.clone();
        self.cur_path = home;
        self.loading = true;
        self.on_up = parent_path(&self.cur_path).is_some();
    }

    /// Aplica a listagem recebida, se for a do diretorio atualmente exibido.
    /// O cursor e reposicionado pelo nome e os marcados que sumiram saem.
    fn apply_listing(&mut self, path: &str, entries: Vec<sftp::RemoteEntry>) {
        if path != self.cur_path {
            return;
        }
        let reselect = self.reselect.take();
        let cursor = reselect.clone().or_else(|| {
            self.sel
                .and_then(|s| self.entries.get(s))
                .map(|n| n.name.clone())
        });
        self.entries = entries
            .into_iter()
            .map(|e| FsNode {
                label: download::safe_text(&e.name, 255),
                name: e.name,
                path: e.path,
                kind: e.kind,
                link: e.link,
                size: e.size,
                mode: e.mode,
                owner: e.owner,
                group: e.group,
                date: fmt_date(e.mtime),
            })
            .collect();
        self.loading = false;
        self.sel = cursor.and_then(|c| self.entries.iter().position(|n| n.name == c));
        if let Some(s) = self.sel {
            self.on_up = false;
            // Voltou de uma pasta (ou abriu um arquivo pela barra): o item
            // fica sob o cursor e selecionado, como numa seta, e a vista
            // rola ate ele.
            if reselect.is_some() {
                self.marked = [self.entries[s].name.clone()].into();
                self.scroll_to_cursor = true;
            }
        }
        let names: std::collections::HashSet<&str> =
            self.entries.iter().map(|n| n.name.as_str()).collect();
        self.marked.retain(|m| names.contains(m.as_str()));
        self.anchor = self.sel;
    }

    /// 1 se ha a linha ".." (pasta acima) no topo da listagem; 0 na raiz.
    fn up_rows(&self) -> usize {
        usize::from(parent_path(&self.cur_path).is_some())
    }

    /// Linhas navegaveis: a ".." (se houver) e as entradas.
    fn row_count(&self) -> usize {
        self.up_rows() + self.entries.len()
    }

    /// Linha do cursor (a 0 e a ".." quando ela existe).
    fn cursor_row(&self) -> Option<usize> {
        if self.on_up {
            return (self.up_rows() == 1).then_some(0);
        }
        self.sel
            .filter(|&s| s < self.entries.len())
            .map(|s| s + self.up_rows())
    }

    /// Entrada (indice em `entries`) de uma linha; `None` na "..".
    fn row_entry(&self, row: usize) -> Option<usize> {
        row.checked_sub(self.up_rows())
            .filter(|&i| i < self.entries.len())
    }

    /// Marca as entradas de `a` ate `b` (inclusive, em qualquer ordem).
    fn mark_range(&mut self, a: usize, b: usize) {
        let (lo, hi) = (a.min(b), a.max(b));
        self.marked = self
            .entries
            .iter()
            .skip(lo)
            .take(hi - lo + 1)
            .map(|n| n.name.clone())
            .collect();
    }

    /// Clique numa linha: sem modificador seleciona so ela; Ctrl marca ou
    /// desmarca; Shift seleciona o intervalo desde a ancora.
    fn click(&mut self, idx: usize, ctrl: bool, shift: bool) {
        let Some(name) = self.entries.get(idx).map(|n| n.name.clone()) else {
            return;
        };
        match self.anchor.filter(|&a| shift && a < self.entries.len()) {
            Some(a) => self.mark_range(a, idx),
            None if ctrl => {
                if !self.marked.remove(&name) {
                    self.marked.insert(name);
                }
                self.anchor = Some(idx);
            }
            None => {
                self.marked = [name].into();
                self.anchor = Some(idx);
            }
        }
        self.sel = Some(idx);
        self.on_up = false;
    }

    /// Clique na "..": o cursor vai para ela; sem Ctrl/Shift a selecao e
    /// desfeita (a ".." nunca e marcada).
    fn click_up(&mut self, keep_marks: bool) {
        if self.up_rows() == 0 {
            return;
        }
        self.on_up = true;
        self.sel = None;
        if !keep_marks {
            self.marked.clear();
            self.anchor = None;
        }
    }

    /// Poe o cursor na linha `row` (limitada a listagem). Sem `extend`
    /// seleciona so o item novo (na ".." nada fica marcado); com `extend`
    /// (Shift) marca o intervalo desde a ancora, sem nunca incluir a "..".
    fn set_cursor_row(&mut self, row: usize, extend: bool) {
        let Some(last) = self.row_count().checked_sub(1) else {
            return;
        };
        let row = row.min(last);
        // Entrada de onde o cursor sai; saindo da ".." conta a primeira.
        let before = self
            .cursor_row()
            .map(|r| self.row_entry(r).unwrap_or(0));
        let entry = self.row_entry(row);
        self.on_up = entry.is_none();
        self.sel = entry;
        if extend {
            if self.entries.is_empty() {
                return;
            }
            // Na ".." o intervalo vai ate a primeira entrada.
            let target = entry.unwrap_or(0);
            let a = self
                .anchor
                .filter(|&a| a < self.entries.len())
                .or(before)
                .unwrap_or(target);
            self.anchor = Some(a);
            self.mark_range(a, target);
        } else if let Some(i) = entry {
            self.marked = [self.entries[i].name.clone()].into();
            self.anchor = Some(i);
        } else {
            self.marked.clear();
            self.anchor = None;
        }
    }

    /// Move o cursor `delta` linhas (setas, PageUp/PageDown), parando nas
    /// pontas. Com `extend` (Shift) marca o intervalo desde a ancora; sem,
    /// seleciona so o item novo. Sem cursor, descer vai a primeira entrada e
    /// subir a ultima.
    fn move_cursor(&mut self, delta: i32, extend: bool) {
        let Some(last) = self.row_count().checked_sub(1) else {
            return;
        };
        let next = match self.cursor_row() {
            None if delta > 0 => self.up_rows().min(last),
            None => last,
            Some(r) => (r as i64 + delta as i64).clamp(0, last as i64) as usize,
        };
        self.set_cursor_row(next, extend);
    }

    /// Marca todas as entradas (Ctrl+A); a ".." nunca.
    fn select_all(&mut self) {
        self.marked = self.entries.iter().map(|n| n.name.clone()).collect();
        if self.cursor_row().is_none() && !self.entries.is_empty() {
            self.sel = Some(0);
            self.on_up = false;
        }
    }

    /// Itens a baixar: os marcados, na ordem da listagem. O cursor sozinho
    /// nao conta (ex.: Ctrl+clique desmarcou o ultimo): so vale o que aparece
    /// selecionado.
    fn picks(&self) -> Vec<download::Pick> {
        self.entries
            .iter()
            .filter(|n| self.marked.contains(&n.name))
            .map(|n| download::Pick {
                remote: n.path.clone(),
                name: n.name.clone(),
            })
            .collect()
    }

    /// Itens a copiar/recortar: os marcados, na ordem da listagem (a mesma
    /// regra do Ctrl+S; a ".." nunca).
    fn clip_items(&self) -> Vec<ClipItem> {
        self.entries
            .iter()
            .filter(|n| self.marked.contains(&n.name))
            .map(clip_item)
            .collect()
    }

    /// Alvo de F2/Delete: o cursor, so quando ele e o unico item marcado. Com
    /// varios, ou com o cursor fora da selecao (ou na ".."), nada acontece
    /// (nunca age num item sem o usuario perceber).
    fn single_target(&self) -> Option<usize> {
        let s = self.sel.filter(|&s| s < self.entries.len())?;
        (self.marked.len() == 1 && self.marked.contains(&self.entries[s].name)).then_some(s)
    }

    /// Enter/duplo clique no cursor: na ".." sobe; numa entrada, conforme o
    /// tipo (`open_action`): pasta entra, arquivo abre no visualizador (a
    /// leitura a pedir volta aqui), tipo que nao abre vira aviso.
    fn activate(&mut self, to_list: &mut Vec<String>) -> Option<(u64, String)> {
        if self.on_up {
            self.go_up(to_list);
            return None;
        }
        let idx = self.sel.filter(|&s| s < self.entries.len())?;
        match open_action(&self.entries[idx]) {
            OpenAction::Navigate(p) => self.navigate_to(p, to_list),
            OpenAction::Warn(text) => self.notice = Some(text),
            OpenAction::View => return self.open_view(idx),
        }
        None
    }

    /// Pede a leitura da entrada `idx` para o visualizador: (pedido,
    /// caminho). O mesmo arquivo ja abrindo e ignorado; outro pedido em
    /// andamento e cancelado (o `Cancel` dele e solto).
    fn open_view(&mut self, idx: usize) -> Option<(u64, String)> {
        let node = self.entries.get(idx)?;
        if self.opening.as_ref().is_some_and(|o| !o.reload && o.path == node.path) {
            return None;
        }
        self.view_seq += 1;
        let path = node.path.clone();
        self.opening = Some(Opening {
            id: self.view_seq,
            name: node.name.clone(),
            path: path.clone(),
            got: 0,
            total: None,
            cancel: None,
            reload: false,
        });
        self.notice = None;
        Some((self.view_seq, path))
    }

    /// Guarda o cancelamento da leitura `id` (soltar cancela). De um pedido
    /// que nao e mais o atual, e solto na hora.
    fn set_view_cancel(&mut self, id: u64, cancel: download::Cancel) {
        if let Some(o) = self.opening.as_mut().filter(|o| o.id == id) {
            o.cancel = Some(cancel);
        }
    }

    /// A sessao acabou antes de a leitura `id` ser pedida.
    fn view_unavailable(&mut self, id: u64) {
        if self.opening.as_ref().is_some_and(|o| o.id == id) {
            self.opening = None;
            self.error = Some(VIEW_SESSION_GONE.into());
        }
    }

    /// Esc precisa chegar ao navegador: fecha o visualizador ou cancela uma
    /// abertura (sem isso o egui soltaria o foco antes).
    fn wants_escape(&self) -> bool {
        self.viewer.is_some() || self.opening.is_some()
    }

    /// F5 (ou o botao de atualizar): com o visualizador aberto, recarrega o
    /// arquivo (devolve a leitura a pedir); na listagem, cancela uma abertura
    /// em curso e atualiza a pasta.
    fn on_f5(&mut self, to_list: &mut Vec<String>) -> Option<(u64, String)> {
        if let Some(v) = &self.viewer {
            if self.opening.as_ref().is_some_and(|o| o.reload) {
                return None;
            }
            self.view_seq += 1;
            let path = v.doc.path.clone();
            self.opening = Some(Opening {
                id: self.view_seq,
                name: v.name.clone(),
                path: path.clone(),
                got: 0,
                total: None,
                cancel: None,
                reload: true,
            });
            return Some((self.view_seq, path));
        }
        self.opening = None;
        self.refresh(to_list);
        None
    }

    /// Andamento ou resultado de uma leitura. So vale o pedido atual: os de
    /// ids antigos (cancelados ou trocados) sao descartados. Pasta: entra
    /// nela; binario, especial ou erro: aviso, sem abrir nada.
    fn apply_view_event(&mut self, ev: ViewEvent, to_list: &mut Vec<String>) {
        match ev {
            ViewEvent::Progress { id, got, total } => {
                if let Some(o) = self.opening.as_mut().filter(|o| o.id == id) {
                    o.got = got;
                    o.total = total;
                }
            }
            ViewEvent::Done { id, result } => {
                if self.opening.as_ref().is_none_or(|o| o.id != id) {
                    return;
                }
                let Some(op) = self.opening.take() else {
                    return;
                };
                match result {
                    Ok(doc) => {
                        if let (true, Some(v)) = (op.reload, &mut self.viewer) {
                            v.replace_doc(doc, Instant::now());
                            return;
                        }
                        self.viewer = Some(FileViewer::new(doc, op.name));
                        self.notice = None;
                        self.reset_typeahead();
                    }
                    // Pasta (ou link para pasta): entra pelo caminho logico.
                    Err(ViewError::IsDir) => self.navigate_to(op.path, to_list),
                    Err(e) => {
                        let (text, aviso) = view_error_text(&e, &op.name);
                        if let (true, Some(v)) = (op.reload, &mut self.viewer) {
                            v.reload_error = Some(text);
                            return;
                        }
                        if e == ViewError::NotFound {
                            self.refresh(to_list);
                        }
                        if aviso {
                            self.notice = Some(text);
                        } else {
                            self.error = Some(text);
                        }
                    }
                }
            }
        }
    }

    /// Passa a exibir `path` (vazia, carregando) sem pedir a listagem. Ao
    /// entrar numa pasta o cursor comeca na ".." (estilo Total Commander).
    /// Cancela uma abertura em curso e fecha o visualizador.
    fn show_dir(&mut self, path: String) {
        self.on_up = parent_path(&path).is_some();
        self.cur_path = path;
        self.entries.clear();
        self.error = None;
        self.notice = None;
        self.loading = true;
        self.path_edit = None;
        self.sel = None;
        self.marked.clear();
        self.anchor = None;
        self.reselect = None;
        self.scroll_top = true;
        self.reset_typeahead();
        self.opening = None;
        self.viewer = None;
    }

    /// Navega para `path`: limpa a lista e pede a nova listagem.
    fn navigate_to(&mut self, path: String, to_list: &mut Vec<String>) {
        self.show_dir(path.clone());
        to_list.push(path);
    }

    /// Sobe para a pasta acima (Backspace, Enter/duplo clique na ".."); a
    /// pasta de onde se saiu fica sob o cursor quando a listagem chegar.
    fn go_up(&mut self, to_list: &mut Vec<String>) {
        let Some(parent) = parent_path(&self.cur_path) else {
            return;
        };
        let from = self
            .cur_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_string();
        self.navigate_to(parent, to_list);
        self.reselect = (!from.is_empty()).then_some(from);
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

    /// Abre a edicao do caminho na barra, com o texto todo selecionado.
    /// Sem conexao (sem caminho) nao abre.
    fn start_path_edit(&mut self) {
        if self.cur_path.is_empty() {
            return;
        }
        self.path_edit = Some(PathEdit {
            text: self.cur_path.clone(),
            fresh: true,
            error: None,
            pending: None,
            retry: None,
        });
        self.reset_typeahead();
    }

    /// Resposta do servidor ao caminho digitado na barra. Pasta: passa a
    /// exibi-la (a listagem vem logo atras). Arquivo: abre a pasta dele com o
    /// cursor no arquivo. Erro: se o texto tinha espacos nas pontas, devolve
    /// o pedido do caminho sem eles (a mandar ao servidor); senao a edicao
    /// continua, com a mensagem. Resposta de um pedido cancelado (ou antigo)
    /// e ignorada.
    fn apply_goto(
        &mut self,
        seq: u64,
        result: Result<sftp::GotoKind, String>,
        to_list: &mut Vec<String>,
    ) -> Option<(u64, String)> {
        let edit = self.path_edit.as_mut()?;
        let (pedido, alvo) = edit.pending.clone()?;
        if pedido != seq {
            return None;
        }
        edit.pending = None;
        match result {
            Ok(sftp::GotoKind::Dir) => self.show_dir(alvo),
            Ok(sftp::GotoKind::File) => {
                let pai = parent_path(&alvo).unwrap_or_else(|| "/".into());
                let nome = alvo.rsplit('/').next().unwrap_or("").to_string();
                self.navigate_to(pai, to_list);
                self.reselect = (!nome.is_empty()).then_some(nome);
            }
            Err(msg) => match edit.retry.take() {
                Some(alt) => {
                    self.goto_seq += 1;
                    edit.pending = Some((self.goto_seq, alt.clone()));
                    return Some((self.goto_seq, alt));
                }
                None => edit.error = Some(msg),
            },
        }
        None
    }

    /// Busca por letras: acrescenta `typed` ao prefixo (zerado apos
    /// TYPEAHEAD_RESET sem digitar) e leva o cursor ao item cujo nome comeca
    /// com ele, sem diferenciar maiusculas nem acentos. Uma letra so (ou a
    /// mesma repetida) passa ao proximo item que comeca com ela, como no
    /// Explorador. Devolve verdadeiro se o cursor mudou.
    fn type_ahead(&mut self, typed: &str, now: Instant) -> bool {
        if self
            .typeahead_at
            .is_some_and(|t| now.saturating_duration_since(t) > TYPEAHEAD_RESET)
        {
            self.typeahead.clear();
        }
        let mut moved = false;
        for c in typed.chars().filter(|c| !c.is_control()) {
            // Espaco so conta no meio de um nome ("meu arq").
            if c == ' ' && self.typeahead.is_empty() {
                continue;
            }
            self.typeahead.push(c);
            self.typeahead_at = Some(now);
            if self.entries.is_empty() {
                self.typeahead_miss = true;
                continue;
            }
            let key = download::fold_name(&self.typeahead);
            let Some(first) = key.chars().next() else {
                continue;
            };
            let single = key.chars().count() == 1;
            let repeated = !single && key.chars().all(|k| k == first);
            // Cursor numa entrada; na ".." (ou sem cursor) conta do inicio.
            let cur = self.sel.filter(|&s| !self.on_up && s < self.entries.len());
            let next_with = |this: &Self, k: &str| match cur {
                Some(s) => this.find_prefix(k, s, false),
                None => this.find_prefix(k, 0, true),
            };
            let hit = if single {
                next_with(self, &key)
            } else {
                self.find_prefix(&key, cur.unwrap_or(0), true).or_else(|| {
                    if repeated {
                        next_with(self, &first.to_string())
                    } else {
                        None
                    }
                })
            };
            match hit {
                Some(i) => {
                    if cur != Some(i) {
                        moved = true;
                    }
                    self.set_cursor_row(i + self.up_rows(), false);
                    self.typeahead_miss = false;
                }
                None => self.typeahead_miss = true,
            }
        }
        moved
    }

    /// Primeira entrada, a partir de `start` (dando a volta), cujo nome
    /// comeca com `key` (ja normalizada por `download::fold_name`). Sem `inclusive`,
    /// `start` e o ultimo candidato.
    fn find_prefix(&self, key: &str, start: usize, inclusive: bool) -> Option<usize> {
        let n = self.entries.len();
        (0..n)
            .map(|k| (start + k + usize::from(!inclusive)) % n)
            .find(|&i| download::fold_name(&self.entries[i].name).starts_with(key))
    }

    /// Zera a busca por letras (outra tecla, clique ou navegacao).
    fn reset_typeahead(&mut self) {
        self.typeahead.clear();
        self.typeahead_at = None;
        self.typeahead_miss = false;
    }

    /// Desenha o navegador do diretorio atual e devolve os diretorios a carregar
    /// (apos navegar) e se o usuario pediu refresh pelo botao do cabecalho.
    /// Com `has_focus`, trata o teclado: setas, PageUp/PageDown e Home/End
    /// movem o cursor (Shift estende a selecao), letras buscam pelo nome,
    /// Ctrl+A marca tudo, Enter abre (na ".." sobe), Backspace volta, F2
    /// renomeia, Delete exclui, Ctrl+S baixa a selecao e Ctrl+L edita o
    /// caminho. `dl` diz se um download pode ser pedido agora.
    fn ui(
        &mut self,
        ui: &mut egui::Ui,
        id_salt: impl std::hash::Hash,
        has_focus: bool,
        dl: DlAvail,
    ) -> ExplorerOut {
        // Visualizador aberto: ele ocupa a area e fica com todo o teclado.
        if self.viewer.is_some() {
            return self.ui_viewer(ui, id_salt, has_focus, dl);
        }
        // Tab na lista ou no campo do caminho: nada (o teclado so nao sai do
        // painel; o texto digitado no campo fica).
        self.tab = false;
        let mut to_list: Vec<String> = Vec::new();
        // Enter/duplo clique (no cursor) e Backspace, aplicados no fim.
        let mut activate = false;
        let mut go_up = false;
        let mut refresh = false;
        let mut new_dialog: Option<FsDialog> = None;
        let mut clicked_row = false;
        let mut download: Option<Vec<download::Pick>> = None;
        let mut start_edit = false;
        // Arquivo a abrir no visualizador (Enter, duplo clique ou menu).
        let mut view_req: Option<(u64, String)> = None;
        let (edit_id, _) = explorer_field_ids(&id_salt);
        let mut clip: Option<ClipCmd> = None;
        // A trava do Ctrl+V solta quando o painel perde o foco.
        if !has_focus {
            self.paste_latch = None;
        }

        // O prefixo da busca por letras expira sozinho (o indicador some).
        let now = Instant::now();
        if let Some(t) = self.typeahead_at {
            let passou = now.saturating_duration_since(t);
            if passou > TYPEAHEAD_RESET {
                self.reset_typeahead();
            } else {
                ui.ctx().request_repaint_after(TYPEAHEAD_RESET - passou);
            }
        }

        // --- Teclado (somente com o painel em foco e sem dialogo/edicao) ---
        let mut sel_changed = false;
        if has_focus && self.dialog.is_none() && self.path_edit.is_none() {
            use egui::{Key, Modifiers};
            let page = self.page_rows.saturating_sub(1).max(1) as i32;
            let mut ext = 0i32;
            let mut mv = 0i32;
            // Home/End: `Some(false)` = primeira linha, `Some(true)` = ultima.
            let mut ext_to: Option<bool> = None;
            let mut mv_to: Option<bool> = None;
            let mut typed = String::new();
            // Esc cancela uma abertura em curso (sem ela, segue como antes).
            let mut esc = false;
            let opening = self.opening.is_some();
            // Copiar/recortar/colar (ver abaixo).
            let mut clip_key: Option<ClipMode> = None;
            let mut paste_ev = false;
            let mut v_up = false;
            let mut key_up = false;
            let mut esc_clip = false;
            let clip_active = self.clip_active;
            let (open, back, rename, del, all, save, edit) = ui.input_mut(|i| {
                if opening {
                    esc = i.consume_key(Modifiers::NONE, Key::Escape);
                }
                // Copiar/recortar/colar do navegador (nunca a area de
                // transferencia do Windows): sempre consumidos aqui, nada
                // vaza. Shift+Delete (recortar do Windows) e ignorado.
                let m = i.modifiers;
                i.events.retain(|ev| match ev {
                    egui::Event::Copy => {
                        clip_key = Some(ClipMode::Copy);
                        false
                    }
                    egui::Event::Cut => {
                        if !(m.shift && !m.ctrl && !m.command) {
                            clip_key = Some(ClipMode::Cut);
                        }
                        false
                    }
                    egui::Event::Paste(_) => {
                        paste_ev = true;
                        false
                    }
                    // Sem texto na area de transferencia do Windows, o Ctrl+V
                    // so chega como a soltura do V (com o Ctrl ainda seguro).
                    egui::Event::Key {
                        key: Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } => {
                        v_up |= modifiers.command || modifiers.ctrl;
                        key_up = true;
                        true
                    }
                    egui::Event::Key {
                        key: Key::Insert,
                        pressed: false,
                        ..
                    } => {
                        key_up = true;
                        true
                    }
                    _ => true,
                });
                // Esc desiste de copiar/mover (a menor prioridade do Esc).
                if clip_active && !opening {
                    esc_clip = i.consume_key(Modifiers::NONE, Key::Escape);
                }
                // Shift primeiro: o padrao sem modificador tambem casaria com
                // Shift (o egui ignora shift/alt a mais no padrao).
                for (k, d) in [
                    (Key::ArrowDown, 1),
                    (Key::ArrowUp, -1),
                    (Key::PageDown, page),
                    (Key::PageUp, -page),
                ] {
                    if i.consume_key(Modifiers::SHIFT, k) {
                        ext += d;
                    }
                }
                if i.consume_key(Modifiers::SHIFT, Key::Home) {
                    ext_to = Some(false);
                }
                if i.consume_key(Modifiers::SHIFT, Key::End) {
                    ext_to = Some(true);
                }
                for (k, d) in [
                    (Key::ArrowDown, 1),
                    (Key::ArrowUp, -1),
                    (Key::PageDown, page),
                    (Key::PageUp, -page),
                ] {
                    if i.consume_key(Modifiers::NONE, k) {
                        mv += d;
                    }
                }
                if i.consume_key(Modifiers::NONE, Key::Home) {
                    mv_to = Some(false);
                }
                if i.consume_key(Modifiers::NONE, Key::End) {
                    mv_to = Some(true);
                }
                // Letras (sem Ctrl/Alt) vao para a busca e nao seguem adiante.
                if !(i.modifiers.ctrl || i.modifiers.command || i.modifiers.alt) {
                    i.events.retain(|ev| match ev {
                        egui::Event::Text(t) => {
                            typed.push_str(t);
                            false
                        }
                        _ => true,
                    });
                }
                (
                    i.consume_key(Modifiers::NONE, Key::Enter),
                    i.consume_key(Modifiers::NONE, Key::Backspace),
                    i.consume_key(Modifiers::NONE, Key::F2),
                    i.consume_key(Modifiers::NONE, Key::Delete),
                    i.consume_key(Modifiers::CTRL, Key::A),
                    // Consumido sempre, mesmo sem efeito.
                    i.consume_key(Modifiers::CTRL, Key::S),
                    i.consume_key(Modifiers::CTRL, Key::L),
                )
            });
            let rows = self.row_count();
            if rows > 0 {
                if ext != 0 {
                    self.move_cursor(ext, true);
                }
                if let Some(end) = ext_to {
                    self.set_cursor_row(if end { rows - 1 } else { 0 }, true);
                }
                if mv != 0 {
                    self.move_cursor(mv, false);
                }
                if let Some(end) = mv_to {
                    self.set_cursor_row(if end { rows - 1 } else { 0 }, false);
                }
            }
            if esc {
                self.opening = None;
            }
            let latch_free = self
                .paste_latch
                .is_none_or(|t| now.saturating_duration_since(t) > PASTE_LATCH);
            if paste_ev {
                if latch_free {
                    clip = Some(ClipCmd::Paste);
                }
                self.paste_latch = Some(now);
            } else if v_up && latch_free {
                clip = Some(ClipCmd::Paste);
            }
            if key_up {
                self.paste_latch = None;
            }
            if let Some(mode) = clip_key {
                let items = self.clip_items();
                if !items.is_empty() {
                    clip = Some(match mode {
                        ClipMode::Copy => ClipCmd::Copy(items),
                        ClipMode::Cut => ClipCmd::Cut(items),
                    });
                }
            }
            if esc_clip {
                clip = Some(ClipCmd::Clear);
            }
            let nav = ext != 0 || mv != 0 || ext_to.is_some() || mv_to.is_some();
            sel_changed |= nav && rows > 0;
            if nav || open || back || all || edit {
                self.reset_typeahead();
            }
            if !typed.is_empty() && self.type_ahead(&typed, now) {
                sel_changed = true;
            }
            if all {
                self.select_all();
            }
            activate |= open;
            go_up |= back;
            if edit {
                start_edit = true;
            }
            // F2/Delete: so com um item selecionado (ver `single_target`).
            if let Some(s) = self.single_target() {
                let node = &self.entries[s];
                if rename {
                    new_dialog = Some(FsDialog::Rename {
                        path: node.path.clone(),
                        name: node.name.clone(),
                    });
                }
                if del {
                    new_dialog = Some(FsDialog::delete(node));
                }
            }
            if save && dl == DlAvail::Ready {
                let picks = self.picks();
                if !picks.is_empty() {
                    download = Some(picks);
                }
            }
        }

        // Cabecalho: barra do caminho (clique nela, no icone ou Ctrl+L para
        // digitar outro) e botoes de baixar e de atualizar (direita).
        let has_picks = !self.marked.is_empty();
        let mut want_download = false;
        let mut submit = false;
        let mut close_edit = false;
        let mut inner_focus = false;
        let can_edit = !self.cur_path.is_empty();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(3.0);
            let icon = ui.add(
                egui::Image::new(ICON_FOLDER_LOCK)
                    .fit_to_exact_size(egui::vec2(16.0, 16.0))
                    .tint(ACCENT)
                    .sense(egui::Sense::click()),
            );
            if can_edit && icon.clicked() {
                start_edit = true;
            }
            // Os botoes de refresh e de baixar ficam a direita; reservamos o
            // espaco deles antes para que o caminho/edicao ocupe o restante.
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
                ui.add_space(4.0);
                let dl_btn = egui::ImageButton::new(
                    egui::Image::new(ICON_DOWNLOAD)
                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                        .tint(ACCENT),
                )
                .frame(false);
                let why_not = match dl {
                    DlAvail::Ready => "Selecione arquivos ou pastas para baixar",
                    DlAvail::Busy => "Aguarde o download atual terminar",
                    DlAvail::Offline => "Aguarde a conexão",
                };
                if ui
                    .add_enabled(dl == DlAvail::Ready && has_picks, dl_btn)
                    .on_hover_text("Baixar a seleção para o computador (Ctrl+S)")
                    .on_disabled_hover_text(why_not)
                    .clicked()
                {
                    want_download = true;
                }
                ui.add_space(4.0);

                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let font = egui::TextStyle::Body.resolve(ui.style());
                    if let Some(edit) = &mut self.path_edit {
                        // Modo edicao: campo que ocupa a largura da barra. Cores
                        // explicitas para nao depender do tema do Windows.
                        let busy = edit.pending.is_some();
                        let w = (ui.available_width() - if busy { 24.0 } else { 0.0 }).max(40.0);
                        // Enter tratado aqui (return_key None): o campo segue
                        // focado se o caminho nao abrir, com o erro embaixo.
                        let out = egui::TextEdit::singleline(&mut edit.text)
                            .id(edit_id)
                            .font(font)
                            .hint_text("Caminho, ex.: /var/www ou ~/public_html")
                            .background_color(FIELD_BG)
                            .text_color(TEXT)
                            .return_key(None)
                            .vertical_align(egui::Align::Center)
                            .min_size(egui::vec2(w, 24.0))
                            .desired_width(w)
                            .show(ui);
                        if busy {
                            ui.add(egui::Spinner::new().size(14.0));
                        }
                        let resp = out.response;
                        // Pede foco apenas no primeiro quadro apos abrir. Pedir
                        // foco todo quadro impediria o `lost_focus()` (Esc).
                        if edit.fresh {
                            edit.fresh = false;
                            resp.request_focus();
                            // Texto todo selecionado: digitar ja substitui.
                            let mut state = out.state;
                            let n = edit.text.chars().count();
                            state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                                egui::text::CCursor::new(0),
                                egui::text::CCursor::new(n),
                            )));
                            state.store(ui.ctx(), edit_id);
                        }
                        if resp.changed() {
                            edit.error = None;
                        }
                        inner_focus = resp.has_focus();
                        if inner_focus
                            && !busy
                            && ui.input_mut(|i| {
                                i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                            })
                        {
                            submit = true;
                        }
                        // Esc, clique fora ou Alt+setas cancelam (o Tab nao
                        // chega ao campo: `App::take_sftp_tab`).
                        if resp.lost_focus() {
                            close_edit = true;
                        }
                    } else {
                        // Barra com o caminho atual: clicar em qualquer ponto
                        // dela (mesmo arrastando um pouco) abre a edicao.
                        let w = ui.available_width().max(40.0);
                        let (rect, resp) = ui
                            .allocate_exact_size(egui::vec2(w, 24.0), egui::Sense::click_and_drag());
                        let hovered = can_edit && resp.hovered();
                        ui.painter().rect(
                            rect,
                            4.0,
                            FIELD_BG,
                            egui::Stroke::new(1.0, if hovered { ACCENT } else { CARD_BORDER }),
                            egui::StrokeKind::Inside,
                        );
                        let atual = if self.cur_path.is_empty() {
                            "/".to_string()
                        } else {
                            download::safe_text(&self.cur_path, 4096)
                        };
                        let max_chars = ((w - 16.0) / 7.0).max(4.0) as usize;
                        let shown = elide_path(&atual, max_chars);
                        // Mesma posicao do texto no campo de edicao (sem pulo).
                        ui.painter().text(
                            egui::pos2(rect.left() + 4.0, rect.center().y),
                            egui::Align2::LEFT_CENTER,
                            &shown,
                            font,
                            TEXT,
                        );
                        if can_edit {
                            let dica = if shown == atual {
                                "Clique para digitar outro caminho (Ctrl+L)".to_string()
                            } else {
                                format!("{atual}\nClique para digitar outro caminho (Ctrl+L)")
                            };
                            let resp = resp
                                .on_hover_text(dica)
                                .on_hover_cursor(egui::CursorIcon::Text);
                            if resp.clicked() || resp.drag_stopped() {
                                start_edit = true;
                            }
                        }
                    }
                });
            });
        });
        if want_download {
            download = Some(self.picks());
        }
        let mut goto: Option<(u64, String)> = None;
        if submit {
            let text = self
                .path_edit
                .as_ref()
                .map(|e| e.text.clone())
                .unwrap_or_default();
            // O proprio caminho, sem mexer em nada (inclusive um nome com
            // espaco no fim): so atualiza.
            let resolved = if text == self.cur_path {
                Ok(Some((text, None)))
            } else {
                resolve_remote_input(&text, &self.cur_path, &self.home)
            };
            match resolved {
                Ok(None) => close_edit = true,
                Ok(Some((alvo, _))) if alvo == self.cur_path => {
                    close_edit = true;
                    refresh = true;
                }
                Ok(Some((alvo, retry))) => {
                    self.goto_seq += 1;
                    let seq = self.goto_seq;
                    if let Some(e) = &mut self.path_edit {
                        e.pending = Some((seq, alvo.clone()));
                        e.retry = retry;
                    }
                    goto = Some((seq, alvo));
                }
                Err(msg) => {
                    if let Some(e) = &mut self.path_edit {
                        e.error = Some(msg);
                    }
                }
            }
        }
        if close_edit {
            self.path_edit = None;
        }
        if start_edit {
            self.start_path_edit();
        }
        // Erro do caminho digitado, logo abaixo da barra (tamanho normal e
        // com quebra de linha: um caminho longo nao alarga o painel).
        if let Some(err) = self.path_edit.as_ref().and_then(|e| e.error.as_deref()) {
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 27,
                    right: 4,
                    top: 2,
                    bottom: 0,
                })
                .show(ui, |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(err).color(ERROR_FG)).wrap());
                });
        }
        // Erros de operacao (listar/renomear/excluir...) numa faixa visivel,
        // com botao para dispensar.
        if let Some(err) = &self.error {
            if dismissable_band(ui, err, DANGER, ERROR_FG) {
                self.error = None;
            }
        }
        // Avisos (ex.: link quebrado): mesma faixa, em ambar.
        if let Some(notice) = &self.notice {
            if dismissable_band(ui, notice, HIGHLIGHT, HIGHLIGHT) {
                self.notice = None;
            }
        }
        // Arquivo sendo lido para o visualizador (a lista segue utilizavel).
        if let Some(op) = self.opening.as_ref().filter(|o| !o.reload) {
            if opening_band(ui, op) {
                self.opening = None;
            }
        }
        ui.add_space(4.0);
        ui.separator();

        // Cabecalho das colunas, alinhado com as larguras usadas em file_row
        // (so as colunas que cabem na largura do painel).
        let cols_on = visible_cols(ui.available_width());
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
            for ((texto, w), on) in LIST_COLS.into_iter().zip(cols_on) {
                if !on {
                    continue;
                }
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

        let row_gap = ui.spacing().item_spacing.y;
        let mut area = egui::ScrollArea::vertical()
            .id_salt(id_salt)
            .auto_shrink([false, false]);
        // Pasta nova: do topo (a volta de uma pasta ainda rola ate ela).
        if std::mem::take(&mut self.scroll_top) {
            area = area.vertical_scroll_offset(0.0);
        }
        // Linha do cursor na tela (o indicador da busca por letras nao a cobre).
        let mut cursor_rect: Option<egui::Rect> = None;
        let scroll = area
            .show(ui, |ui| {
                ui.add_space(4.0);
                let mods = ui.input(|i| i.modifiers);
                // Rolar ate o cursor: ele mudou pelo teclado ou foi pedido
                // (volta de uma pasta).
                let mut scrolled = false;

                // Primeira linha: a pasta acima, navegavel como as demais.
                if self.up_rows() == 1 {
                    let look = RowLook {
                        icon: ICON_FOLDER_UP,
                        color: FOLDER_FG,
                        slash: false,
                    };
                    let up = file_row(ui, look, "..", None, false, self.on_up && has_focus);
                    if self.on_up {
                        cursor_rect = Some(up.rect);
                    }
                    if self.on_up && (sel_changed || self.scroll_to_cursor) {
                        up.scroll_to_me(None);
                        scrolled = true;
                    }
                    let up = up.on_hover_text("Pasta acima (Enter ou duplo clique)");
                    if up.clicked() {
                        self.click_up(mods.command || mods.ctrl || mods.shift);
                        self.reset_typeahead();
                        clicked_row = true;
                    }
                    if up.double_clicked() {
                        activate = true;
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

                // Clique: (linha, Ctrl, Shift), aplicado depois do laco.
                let mut click_sel: Option<(usize, bool, bool)> = None;
                // Duplo clique (Enter do mouse), tratado depois do laco.
                let mut open_idx: Option<usize> = None;
                // "Visualizar" no menu de contexto.
                let mut view_idx: Option<usize> = None;
                for (idx, node) in self.entries.iter().enumerate() {
                    let mut look = row_look(node);
                    // Recortados (mover) ficam esmaecidos na pasta de origem.
                    if self.cut_names.as_ref().is_some_and(|c| c.contains(&node.name)) {
                        look.color = look.color.gamma_multiply(0.45);
                    }
                    // Tamanho so de arquivo (num link, o do destino); pasta,
                    // especial e link sem destino ficam vazios.
                    let size = (node.kind == sftp::EntryKind::File).then_some(node.size);
                    let cols = RowCols {
                        mode: node.mode,
                        owner: &node.owner,
                        group: &node.group,
                        date: &node.date,
                        size,
                        show: cols_on,
                    };
                    let selected = self.marked.contains(&node.name);
                    let is_cursor = !self.on_up && self.sel == Some(idx);
                    let mut resp = file_row(
                        ui,
                        look,
                        &node.label,
                        Some(cols),
                        selected,
                        is_cursor && has_focus,
                    );
                    // Tipo e alvo do link: montada so com o mouse em cima.
                    if resp.hovered() {
                        if let Some(tip) = row_tip(node) {
                            resp = resp.on_hover_text(tip);
                        }
                    }
                    // Mantem o cursor visivel ao navegar pelo teclado (e ao
                    // voltar de uma pasta).
                    if is_cursor {
                        cursor_rect = Some(resp.rect);
                    }
                    if is_cursor && (sel_changed || self.scroll_to_cursor) {
                        resp.scroll_to_me(None);
                        scrolled = true;
                    }
                    // Clique seleciona (Ctrl marca/desmarca, Shift estende) e o
                    // painel toma o foco do teclado.
                    if resp.clicked() {
                        click_sel = Some((idx, mods.command || mods.ctrl, mods.shift));
                        clicked_row = true;
                    }
                    // Botao direito fora da selecao: a selecao passa a ser so
                    // essa linha (como no Explorer).
                    if resp.secondary_clicked() && !selected {
                        click_sel = Some((idx, false, false));
                    }
                    // Duplo clique: o mesmo que Enter (pasta entra; tipo que
                    // nao abre vira aviso).
                    if resp.double_clicked() {
                        open_idx = Some(idx);
                    }
                    // Menu de contexto: baixar / renomear / permissoes / excluir.
                    resp.context_menu(|ui| {
                        let bg = style_context_menu(ui);
                        // Baixar: a selecao inteira se a linha faz parte dela.
                        let in_sel = self.marked.contains(&node.name);
                        let n = if in_sel { self.marked.len() } else { 1 };
                        // Com varios marcados, o menu e da selecao: as acoes de
                        // um item so ficam desativadas (como F2/Delete).
                        let one = n == 1;
                        let title = if one {
                            elide(&node.label, 24)
                        } else {
                            format!("{n} itens selecionados")
                        };
                        ui.label(egui::RichText::new(title).small().color(TEXT_WEAK));
                        ui.add_space(2.0);
                        // Visualizar: so um arquivo (o que Enter abriria).
                        if one
                            && open_action(node) == OpenAction::View
                            && menu_item(ui, ICON_EYE, "Visualizar", CARD_TEXT)
                        {
                            view_idx = Some(idx);
                            ui.close_menu();
                        }
                        let single = |ui: &mut egui::Ui, icon, text: &str, color| {
                            ui.add_enabled_ui(one, |ui| menu_item(ui, icon, text, color)).inner
                        };
                        // Permissoes/proprietario: num link, so com o destino
                        // resolvido (a alteracao vale para ele).
                        let editable = attrs_editable(node);
                        let attrs = |ui: &mut egui::Ui, icon, text: &str| {
                            let r = ui.add_enabled_ui(one && editable, |ui| {
                                menu_item(ui, icon, text, CARD_TEXT)
                            });
                            if one && !editable && node.link.is_some() {
                                r.response.on_disabled_hover_text(LINK_ATTRS_OFF);
                            }
                            r.inner
                        };
                        let label = if n > 1 {
                            format!("Baixar {n} itens\u{2026}")
                        } else {
                            "Baixar\u{2026}".to_string()
                        };
                        let clicked = ui
                            .add_enabled_ui(dl == DlAvail::Ready, |ui| {
                                menu_item(ui, ICON_DOWNLOAD, &label, CARD_TEXT)
                            })
                            .inner;
                        if clicked {
                            download = Some(if in_sel {
                                self.picks()
                            } else {
                                vec![download::Pick {
                                    remote: node.path.clone(),
                                    name: node.name.clone(),
                                }]
                            });
                            ui.close_menu();
                        }
                        // Copiar/recortar: a selecao inteira se a linha faz
                        // parte dela (cola-se com Ctrl+V na pasta de destino).
                        let on = dl != DlAvail::Offline;
                        let (lc, lx) = if n > 1 {
                            (format!("Copiar {n} itens"), format!("Recortar {n} itens"))
                        } else {
                            ("Copiar".to_string(), "Recortar".to_string())
                        };
                        let items = || {
                            if in_sel {
                                self.clip_items()
                            } else {
                                vec![clip_item(node)]
                            }
                        };
                        if ui
                            .add_enabled_ui(on, |ui| menu_item(ui, ICON_COPY, &lc, CARD_TEXT))
                            .inner
                        {
                            clip = Some(ClipCmd::Copy(items()));
                            ui.close_menu();
                        }
                        if ui
                            .add_enabled_ui(on, |ui| menu_item(ui, ICON_CUT, &lx, CARD_TEXT))
                            .inner
                        {
                            clip = Some(ClipCmd::Cut(items()));
                            ui.close_menu();
                        }
                        ui.add_space(2.0);
                        ui.separator();
                        ui.add_space(2.0);
                        if single(ui, ICON_PEN, "Renomear", CARD_TEXT) {
                            new_dialog = Some(FsDialog::Rename {
                                path: node.path.clone(),
                                name: node.name.clone(),
                            });
                            ui.close_menu();
                        }
                        if attrs(ui, ICON_SETTINGS, "Permissões") {
                            new_dialog = Some(FsDialog::chmod(node));
                            ui.close_menu();
                        }
                        if attrs(ui, ICON_USERS, "Proprietário/Grupo") {
                            new_dialog = Some(FsDialog::chown(node));
                            ui.close_menu();
                        }
                        ui.add_space(2.0);
                        ui.separator();
                        ui.add_space(2.0);
                        if single(ui, ICON_TRASH, "Excluir", DANGER) {
                            new_dialog = Some(FsDialog::delete(node));
                            ui.close_menu();
                        }
                        paint_menu_bg(ui, bg);
                    });
                }
                if scrolled {
                    self.scroll_to_cursor = false;
                }
                if let Some((idx, ctrl, shift)) = click_sel {
                    self.click(idx, ctrl, shift);
                    self.reset_typeahead();
                }
                // Duplo clique numa entrada: cursor nela e o mesmo que Enter.
                if let Some(idx) = open_idx {
                    if self.on_up || self.sel != Some(idx) {
                        self.click(idx, false, false);
                    }
                    activate = true;
                }
                if let Some(idx) = view_idx {
                    view_req = self.open_view(idx);
                }
            });
        // Linhas inteiras visiveis: o passo do PageUp/PageDown.
        let list_rect = scroll.inner_rect;
        self.page_rows =
            ((list_rect.height() + row_gap) / (ROW_H + row_gap)).floor().max(1.0) as usize;

        // Indicador do prefixo da busca por letras, no canto da listagem
        // (por cima dela, sem ocupar espaco).
        if !self.typeahead.is_empty() {
            let prefixo = download::safe_text(&self.typeahead, 60);
            let (texto, cor) = if self.typeahead_miss {
                (format!("{prefixo}  (nenhum item)"), ERROR_FG)
            } else {
                (prefixo, CARD_TEXT)
            };
            let galley = ui
                .painter()
                .layout_no_wrap(texto, egui::FontId::proportional(12.0), cor);
            let size = galley.size() + egui::vec2(36.0, 10.0);
            let x = list_rect.right() - size.x - 12.0;
            let mut rect = egui::Rect::from_min_size(
                egui::pos2(x, list_rect.bottom() - size.y - 8.0),
                size,
            );
            // Embaixo, a menos que cubra o item achado (que fica na ultima
            // linha quando a lista rola ate ele): entao em cima.
            if cursor_rect.is_some_and(|c| c.intersects(rect.expand(2.0))) {
                rect = egui::Rect::from_min_size(egui::pos2(x, list_rect.top() + 8.0), size);
            }
            let painter = ui.painter();
            painter.rect(
                rect,
                6.0,
                MENU_BG,
                egui::Stroke::new(1.0, if self.typeahead_miss { ERROR_FG } else { ACCENT }),
                egui::StrokeKind::Inside,
            );
            let icon = egui::Rect::from_center_size(
                egui::pos2(rect.left() + 15.0, rect.center().y),
                egui::vec2(12.0, 12.0),
            );
            egui::Image::new(ICON_SEARCH).tint(cor).paint_at(ui, icon);
            painter.galley(
                egui::pos2(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
                galley,
                cor,
            );
        }

        if let Some(d) = new_dialog {
            self.dialog = Some(d);
            self.dialog_fresh = true;
        }
        if activate {
            if let Some(v) = self.activate(&mut to_list) {
                view_req = Some(v);
            }
        } else if go_up {
            self.go_up(&mut to_list);
        }

        // Dialogo de gerenciamento (se aberto) pode produzir uma operacao.
        let op = self.show_dialog(ui.ctx());

        ExplorerOut {
            to_list,
            refresh,
            op,
            clicked_row,
            download,
            goto,
            inner_focus,
            view: view_req,
            clip,
        }
    }

    /// Visualizador somente leitura, no lugar do cabecalho, da barra de
    /// caminho e da lista (o titulo do painel, as bordas e o rodape do
    /// download ficam). Com `has_focus` (e sem dialogo), todo o teclado e
    /// dele: rolar, selecionar e copiar, buscar, baixar e Esc (fecha a busca;
    /// sem ela, volta a listagem no mesmo item). As demais teclas sao
    /// consumidas sem efeito: nada chega a lista por baixo.
    fn ui_viewer(
        &mut self,
        ui: &mut egui::Ui,
        id_salt: impl std::hash::Hash,
        has_focus: bool,
        dl: DlAvail,
    ) -> ExplorerOut {
        let mut out = ExplorerOut::empty();
        let Some(mut v) = self.viewer.take() else {
            return out;
        };
        let salt = egui::Id::new(("sftp_viewer", &id_salt));
        let (_, search_id) = explorer_field_ids(&id_salt);
        let now = Instant::now();
        let reloading = self.opening.as_ref().is_some_and(|o| o.reload);
        // Tab/Shift+Tab (tirados da entrada antes do egui: `App::take_sftp_tab`):
        // com a busca aberta alternam o teclado entre o campo e o texto; sem
        // ela, nada.
        if std::mem::take(&mut self.tab) {
            if ui.memory(|m| m.has_focus(search_id)) {
                // Do campo para o texto: o painel toma o foco no fim do quadro
                // (a busca segue aberta; Enter/F3 andam pelas ocorrencias).
                out.clicked_row = true;
            } else if let Some(s) = v.search.as_mut() {
                s.focus = true;
            }
        }
        let mut close = false;
        let mut want_dl = false;
        let mut open_search = false;
        let mut select_all = false;
        let mut copy = false;
        // Some(true): proxima ocorrencia; Some(false): anterior.
        let mut step: Option<bool> = None;

        // --- Teclado (painel em foco, sem dialogo) ---
        if has_focus && self.dialog.is_none() {
            use egui::{Key, Modifiers};
            let (mut dy, mut dpage, mut dx) = (0i32, 0i32, 0i32);
            let (mut home, mut end, mut esc, mut save) = (false, false, false, false);
            // Com a busca aberta, Enter e Shift+Enter tambem andam pelas
            // ocorrencias (como no campo), mesmo com o foco no texto.
            let searching = v.search.is_some();
            ui.input_mut(|i| {
                // Shift/Ctrl primeiro: o padrao sem modificador tambem
                // aceita Shift a mais.
                if i.consume_key(Modifiers::SHIFT, Key::F3)
                    || (searching && i.consume_key(Modifiers::SHIFT, Key::Enter))
                {
                    step = Some(false);
                }
                if i.consume_key(Modifiers::NONE, Key::F3)
                    || (searching && i.consume_key(Modifiers::NONE, Key::Enter))
                {
                    step = Some(true);
                }
                home = i.consume_key(Modifiers::CTRL, Key::Home);
                end = i.consume_key(Modifiers::CTRL, Key::End);
                // Cada tecla conta quantas vezes veio no quadro (repeticao).
                let mut count = |k| i.count_and_consume_key(Modifiers::NONE, k) as i32;
                dy += count(Key::ArrowDown) - count(Key::ArrowUp);
                dpage += count(Key::PageDown) - count(Key::PageUp);
                dx += count(Key::ArrowRight) - count(Key::ArrowLeft);
                home |= i.consume_key(Modifiers::NONE, Key::Home);
                end |= i.consume_key(Modifiers::NONE, Key::End);
                select_all = i.consume_key(Modifiers::CTRL, Key::A);
                open_search = i.consume_key(Modifiers::CTRL, Key::F);
                save = i.consume_key(Modifiers::CTRL, Key::S);
                esc = i.consume_key(Modifiers::NONE, Key::Escape);
                // Sem efeito aqui, mas nunca chegam a lista por baixo.
                for k in [Key::Enter, Key::Backspace, Key::Delete, Key::F2, Key::Tab] {
                    i.consume_key(Modifiers::NONE, k);
                }
                i.events.retain(|ev| match ev {
                    egui::Event::Copy => {
                        copy = true;
                        false
                    }
                    egui::Event::Text(_) | egui::Event::Paste(_) | egui::Event::Cut => false,
                    _ => true,
                });
            });
            if dy != 0 || dpage != 0 || dx != 0 || home || end {
                v.scroll_keys(dy, dpage, dx, home, end);
            }
            want_dl |= save;
            if esc {
                if v.search.is_some() {
                    v.search = None;
                } else {
                    close = true;
                }
            }
        }
        // Campo de busca focado: Enter/Shift+Enter e F3 sao tratados antes do
        // TextEdit (senao ele perderia o foco).
        if ui.memory(|m| m.has_focus(search_id)) {
            use egui::{Key, Modifiers};
            ui.input_mut(|i| {
                if i.consume_key(Modifiers::SHIFT, Key::Enter)
                    || i.consume_key(Modifiers::SHIFT, Key::F3)
                {
                    step = Some(false);
                }
                if i.consume_key(Modifiers::NONE, Key::Enter)
                    || i.consume_key(Modifiers::NONE, Key::F3)
                {
                    step = Some(true);
                }
                if i.consume_key(Modifiers::CTRL, Key::F) {
                    open_search = true;
                }
            });
        }
        if select_all {
            v.sel = Some((0, v.doc.text.len() as u32));
        }
        if copy {
            v.copy_selection(ui.ctx(), now);
        }
        if open_search {
            v.open_search();
        }
        // Busca: recalcula depois de uma pausa na digitacao (ou ja, com
        // Enter/F3), antes de desenhar o contador.
        let mut recomputed = v.refresh_search(now, step.is_some(), ui.ctx());

        // --- Cabecalho: voltar, icone, nome e dados; botoes a direita ---
        let icon_btn = |ui: &mut egui::Ui, img: egui::ImageSource<'static>, tip: &str| {
            let b = egui::ImageButton::new(
                egui::Image::new(img)
                    .fit_to_exact_size(egui::vec2(16.0, 16.0))
                    .tint(ACCENT),
            )
            .frame(false);
            ui.add(b).on_hover_text(tip).clicked()
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(3.0);
            if icon_btn(ui, ICON_ARROW_LEFT, "Voltar à listagem (Esc)") {
                close = true;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(5.0);
                if icon_btn(ui, ICON_CLOSE, "Fechar (Esc)") {
                    close = true;
                }
                ui.add_space(4.0);
                let dl_btn = egui::ImageButton::new(
                    egui::Image::new(ICON_DOWNLOAD)
                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                        .tint(ACCENT),
                )
                .frame(false);
                let why_not = match dl {
                    DlAvail::Ready => "",
                    DlAvail::Busy => "Aguarde o download atual terminar",
                    DlAvail::Offline => "Aguarde a conexão",
                };
                if ui
                    .add_enabled(dl == DlAvail::Ready, dl_btn)
                    .on_hover_text("Baixar este arquivo (Ctrl+S)")
                    .on_disabled_hover_text(why_not)
                    .clicked()
                {
                    want_dl = true;
                }
                ui.add_space(4.0);
                if reloading {
                    ui.add(egui::Spinner::new().size(16.0).color(ACCENT));
                } else if icon_btn(ui, ICON_FOLDER_SYNC, "Recarregar (F5)") {
                    out.refresh = true;
                }
                ui.add_space(4.0);
                if icon_btn(ui, ICON_SEARCH, "Buscar (Ctrl+F)") {
                    v.open_search();
                }
                ui.add_space(6.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    let doc = &v.doc;
                    let icon = if doc.target.is_some() {
                        ICON_FILE_SYMLINK
                    } else {
                        ICON_FILE
                    };
                    ui.add(
                        egui::Image::new(icon)
                            .fit_to_exact_size(egui::vec2(16.0, 16.0))
                            .tint(CARD_TEXT),
                    );
                    let size = doc.size.filter(|s| *s > 0).unwrap_or(doc.text.len() as u64);
                    let mut meta =
                        format!("\u{00b7} {} \u{00b7} {}", human_size(size), doc.encoding.label());
                    if doc.lossy {
                        meta.push_str(" (bytes inválidos)");
                    }
                    if let Some(e) = doc.eol.label() {
                        meta.push_str(&format!(" \u{00b7} {e}"));
                    }
                    const SELO: &str = "somente leitura";
                    let meta_font = egui::FontId::proportional(12.0);
                    let (meta_w, selo_w) = ui.fonts(|f| {
                        (
                            f.layout_no_wrap(meta.clone(), meta_font.clone(), TEXT_WEAK).size().x,
                            f.layout_no_wrap(SELO.into(), egui::FontId::proportional(11.0), ACCENT)
                                .size()
                                .x
                                + 12.0,
                        )
                    });
                    // O que cabe entre o icone e os botoes: o selo so se sobrar
                    // espaco para o nome e os dados inteiros; os dados encolhem
                    // (cortados, com dica) antes do nome ficar menor que 60 px.
                    let gap = ui.spacing().item_spacing.x;
                    let avail = ui.available_width();
                    const MIN_NAME: f32 = 60.0;
                    let selo_need = selo_w + gap;
                    let show_selo = avail >= MIN_NAME + gap + meta_w + selo_need;
                    let rest = avail - if show_selo { selo_need } else { 0.0 };
                    let meta_box = (rest - MIN_NAME - gap).clamp(0.0, meta_w);
                    let show_meta = meta_box >= 24.0;
                    let name_w =
                        (rest - if show_meta { meta_box + gap } else { 0.0 }).max(24.0);
                    let label = download::safe_text(&v.name, 255);
                    let name = ui
                        .allocate_ui(egui::vec2(name_w, 20.0), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(label).strong().color(CARD_TEXT),
                                )
                                .truncate()
                                .selectable(false),
                            )
                        })
                        .inner;
                    let mut dica = if show_meta { String::new() } else { meta.clone() };
                    if let Some(t) = doc.mtime.filter(|t| *t > 0) {
                        if !dica.is_empty() {
                            dica.push('\n');
                        }
                        dica.push_str(&format!("Modificado em {}", fmt_date(t)));
                    }
                    if !dica.is_empty() {
                        let _ = name.on_hover_text(dica);
                    }
                    if show_meta {
                        ui.allocate_ui(egui::vec2(meta_box, 20.0), |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(meta).font(meta_font).color(TEXT_WEAK),
                                )
                                .truncate()
                                .selectable(false),
                            );
                        });
                    }
                    if show_selo {
                        egui::Frame::NONE
                            .stroke(egui::Stroke::new(1.0, ACCENT))
                            .corner_radius(4.0)
                            .inner_margin(egui::Margin::symmetric(5, 1))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(SELO).size(11.0).color(ACCENT),
                                    )
                                    .selectable(false),
                                );
                            });
                    }
                });
            });
        });
        // Linha 2: caminho (cortado no inicio) e o alvo do link.
        ui.horizontal(|ui| {
            ui.add_space(27.0);
            let mut linha = download::safe_text(&v.doc.path, 4096);
            if let Some(t) = &v.doc.target {
                linha.push_str(&format!("  \u{2192} {}", download::safe_text(t, 300)));
            }
            let max_chars = ((ui.available_width() - 8.0) / 6.0).max(8.0) as usize;
            ui.add(
                egui::Label::new(
                    egui::RichText::new(elide_path(&linha, max_chars))
                        .size(11.0)
                        .color(TEXT_WEAK),
                )
                .truncate()
                .selectable(false),
            );
        });

        // --- Faixas ---
        if let Some(err) = &self.error {
            if dismissable_band(ui, err, DANGER, ERROR_FG) {
                self.error = None;
            }
        }
        match v.doc.truncated {
            Some(viewer::Trunc::Bytes { shown, total }) => {
                let texto = match total.filter(|t| *t > shown) {
                    Some(t) => format!(
                        "Mostrando os primeiros {} de {}. Baixe o arquivo para ver tudo.",
                        human_size(shown),
                        human_size(t)
                    ),
                    None => format!(
                        "Mostrando os primeiros {}. Baixe o arquivo para ver tudo.",
                        human_size(shown)
                    ),
                };
                if info_band(ui, &texto, Some(("Baixar\u{2026}", dl == DlAvail::Ready))) {
                    want_dl = true;
                }
            }
            Some(viewer::Trunc::Rows { lines }) => {
                info_band(
                    ui,
                    &format!("Mostrando as primeiras {} linhas.", thousands(lines)),
                    None,
                );
            }
            Some(viewer::Trunc::Deadline { shown }) => {
                info_band(
                    ui,
                    &format!(
                        "O servidor demorou demais; mostrando só o que chegou ({}).",
                        human_size(shown)
                    ),
                    None,
                );
            }
            None => {}
        }
        if v.doc.lossy {
            info_band(ui, LOSSY_BAND, None);
        }
        if v.doc.has_bidi {
            info_band(
                ui,
                "Atenção: o texto tem caracteres invisíveis de direção (bidi), mostrados como <U+\u{2026}>.",
                None,
            );
        }
        if let Some(e) = &v.reload_error {
            let texto = format!("Não foi possível recarregar: {e}");
            if dismissable_band(ui, &texto, HIGHLIGHT, HIGHLIGHT) {
                v.reload_error = None;
            }
        }

        // --- Barra de busca ---
        let mut close_search = false;
        if let Some(s) = v.search.as_mut() {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(3.0);
                ui.add(
                    egui::Image::new(ICON_SEARCH)
                        .fit_to_exact_size(egui::vec2(14.0, 14.0))
                        .tint(ACCENT),
                );
                let te = egui::TextEdit::singleline(&mut s.query)
                    .id(search_id)
                    .hint_text("Buscar no arquivo")
                    .background_color(FIELD_BG)
                    .text_color(TEXT)
                    .return_key(None)
                    .desired_width(220.0)
                    .show(ui);
                let resp = te.response;
                if s.focus {
                    // Ctrl+F: foco com o texto todo selecionado (digitar troca).
                    s.focus = false;
                    resp.request_focus();
                    let mut state = te.state;
                    let n = s.query.chars().count();
                    state.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(n),
                    )));
                    state.store(ui.ctx(), search_id);
                }
                if resp.changed() {
                    s.dirty_at = Some(now);
                    s.jump = true;
                }
                if resp.has_focus() {
                    out.inner_focus = true;
                }
                // Esc no campo: o egui ja tirou o foco dele; a barra fecha e
                // o teclado volta ao painel.
                if resp.lost_focus()
                    && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
                {
                    close_search = true;
                }
                let (cont, cor) = if s.query.is_empty() || s.dirty_at.is_some() {
                    (String::new(), TEXT_WEAK)
                } else if s.capped {
                    (format!("{}+ resultados", viewer::MAX_MATCHES), TEXT_WEAK)
                } else if s.matches.is_empty() {
                    ("nenhum resultado".to_string(), ERROR_FG)
                } else {
                    (
                        format!("{} de {}", s.current.map_or(0, |c| c + 1), s.matches.len()),
                        TEXT_WEAK,
                    )
                };
                ui.add(
                    egui::Label::new(egui::RichText::new(cont).size(11.0).color(cor))
                        .selectable(false),
                );
                if icon_btn(ui, ICON_CHEVRON_UP, "Anterior (Shift+Enter)") {
                    step = Some(false);
                }
                if icon_btn(ui, ICON_CHEVRON_DOWN, "Próximo (Enter)") {
                    step = Some(true);
                }
                if icon_btn(ui, ICON_CLOSE, "Fechar a busca (Esc)") {
                    close_search = true;
                }
            });
        }
        if close_search {
            v.search = None;
        }
        // Setas da barra com a consulta ainda por recalcular.
        if step.is_some() && !recomputed {
            recomputed = v.refresh_search(now, true, ui.ctx());
        }
        // Logo depois de recalcular, a atual ja e a primeira a partir do topo.
        if let (Some(forward), false) = (step, recomputed) {
            v.step_match(forward);
        }

        // --- Texto ---
        let area = v.text_area(ui, salt, dl, now);
        out.clicked_row |= area.clicked;
        want_dl |= area.download;
        if area.search {
            v.open_search();
        }

        if want_dl && dl == DlAvail::Ready {
            out.download = Some(vec![download::Pick {
                remote: v.doc.path.clone(),
                name: v.name.clone(),
            }]);
        }
        if close {
            // A listagem volta no mesmo item (e rola ate ele); uma recarga
            // em andamento deixa de valer.
            if reloading {
                self.opening = None;
            }
            self.scroll_to_cursor = true;
        } else {
            self.viewer = Some(v);
        }
        out.op = self.show_dialog(ui.ctx());
        out
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
        // Primeiro quadro: o campo principal toma o foco, para usar o dialogo
        // so pelo teclado.
        let fresh = std::mem::take(&mut self.dialog_fresh);
        // Enter confirma (no Renomear quem trata e o proprio campo).
        let enter = !matches!(dialog, FsDialog::Rename { .. })
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));

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
                        // Foca ao abrir, ou quando nada tem o foco (pedir todo
                        // quadro impediria o lost_focus() do Enter de disparar).
                        if fresh || ui.memory(|m| m.focused().is_none()) {
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
                                // Na pasta do proprio item: navegar com o
                                // dialogo aberto nao pode move-lo para a
                                // pasta nova.
                                let dir = parent_path(path).unwrap_or_else(|| cur.clone());
                                op = Some(FsOp::Rename {
                                    from: path.clone(),
                                    to: join_remote(&dir, name.trim()),
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
                        link_target,
                    } => {
                        // Nome remoto neutralizado (um "\n" ou bidi no nome
                        // nao imita a nota do link nem disfarca a extensao).
                        ui.label(
                            egui::RichText::new(download::safe_text(name, 255))
                                .size(16.0)
                                .color(HIGHLIGHT),
                        );
                        if let Some(t) = link_target {
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(link_attrs_note(t)).color(TEXT_WEAK),
                                )
                                .wrap(),
                            );
                        }
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
                            if fresh {
                                resp.request_focus();
                            }
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
                            if accent_btn(ui, "Aplicar") || enter {
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
                        link_target,
                    } => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Proprietário/grupo de \u{201C}{}\u{201D}.",
                                download::safe_text(name, 255)
                            ))
                            .color(TEXT_WEAK),
                        );
                        if let Some(t) = link_target {
                            ui.add_space(4.0);
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(link_attrs_note(t)).color(TEXT_WEAK),
                                )
                                .wrap(),
                            );
                        }
                        ui.add_space(8.0);
                        egui::Grid::new("chown_grid")
                            .num_columns(2)
                            .spacing([10.0, 6.0])
                            .show(ui, |ui| {
                                ui.label(egui::RichText::new("Proprietário").color(TEXT_WEAK));
                                let r = ui.add(
                                    egui::TextEdit::singleline(owner_text).desired_width(160.0),
                                );
                                if fresh {
                                    r.request_focus();
                                }
                                ui.end_row();

                                ui.label(egui::RichText::new("Grupo").color(TEXT_WEAK));
                                ui.add(
                                    egui::TextEdit::singleline(group_text).desired_width(160.0),
                                );
                                ui.end_row();
                            });
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Aceita nome (ex.: root) ou id numérico.")
                                .small()
                                .weak(),
                        );

                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if (accent_btn(ui, "Aplicar") || enter)
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
                        link_target,
                    } => {
                        let tipo = if link_target.is_some() {
                            "o link"
                        } else if *is_dir {
                            "a pasta"
                        } else {
                            "o arquivo"
                        };
                        ui.label(
                            egui::RichText::new(format!("Excluir {tipo}:"))
                                .color(TEXT_WEAK),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(download::safe_text(name, 255))
                                .size(14.0)
                                .color(HIGHLIGHT),
                        );
                        // Link: mostra o alvo e deixa claro que o destino fica.
                        if let Some(t) = link_target {
                            if !t.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "\u{2192} {}",
                                        download::safe_text(t, 200)
                                    ))
                                    .color(TEXT_WEAK),
                                );
                            }
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(
                                    "Só o link é removido; o destino não é alterado.",
                                )
                                .color(TEXT_WEAK),
                            );
                        }
                        ui.add_space(12.0);
                        ui.label(
                            egui::RichText::new("Esta ação é permanente.")
                                .color(TEXT_WEAK),
                        );
                        ui.label(
                            egui::RichText::new("Enter exclui  \u{00b7}  Esc cancela")
                                .small()
                                .color(TEXT_WEAK),
                        );
                        ui.add_space(18.0);
                        ui.horizontal(|ui| {
                            if danger_btn(ui, "Excluir") || enter {
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

/// Faixa com texto e botao "Dispensar" (erros em vermelho, avisos em ambar):
/// `edge` e a borda (e o fundo, bem claro) e `fg` o texto e o X. Devolve
/// verdadeiro quando o X foi clicado.
fn dismissable_band(ui: &mut egui::Ui, text: &str, edge: egui::Color32, fg: egui::Color32) -> bool {
    let mut dismissed = false;
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(edge.gamma_multiply(0.15))
        .stroke(egui::Stroke::new(1.0, edge))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            // Na largura do painel: o texto quebra linha e o X fica sempre a
            // direita (um aviso longo num painel estreito nao pode alargar a
            // listagem para dentro do painel vizinho).
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let x_w = 14.0;
                let text_w = (ui.available_width() - x_w - ui.spacing().item_spacing.x).max(24.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(text_w, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(text_w);
                        ui.add(egui::Label::new(egui::RichText::new(text).color(fg)).wrap());
                    },
                );
                let x = egui::ImageButton::new(
                    egui::Image::new(ICON_CLOSE)
                        .fit_to_exact_size(egui::vec2(x_w, x_w))
                        .tint(fg),
                )
                .frame(false);
                if ui.add(x).on_hover_text("Dispensar").clicked() {
                    dismissed = true;
                }
            });
        });
    dismissed
}

/// Junta um diretorio e um nome em um caminho remoto POSIX.
fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Caminho absoluto para o texto digitado na barra do navegador: absoluto,
/// relativo a pasta atual (`cur`) ou com "~" (pasta inicial `home`). Resolve
/// "." e ".." pelo texto, sem consultar o servidor. Devolve o caminho do
/// texto exato e, se ele tem espacos nas pontas, a alternativa sem eles,
/// tentada se o exato nao abrir: uma colagem traz espacos, mas um nome com
/// espaco no fim tambem existe. Quebras de linha e TAB nas pontas (de
/// colagens) sempre saem. `Ok(None)` para texto vazio (ou so espacos).
fn resolve_remote_input(
    input: &str,
    cur: &str,
    home: &str,
) -> Result<Option<(String, Option<String>)>, String> {
    let exact = input.trim_matches(|c| matches!(c, '\r' | '\n' | '\t'));
    let trimmed = exact.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let one = |t: &str| -> Result<String, String> {
        let junto = if t == "~" || t.starts_with("~/") {
            if home.is_empty() {
                return Err("Pasta inicial desconhecida.".into());
            }
            format!("{home}/{}", &t[1..])
        } else if t.starts_with('/') {
            t.to_string()
        } else {
            format!("{cur}/{t}")
        };
        Ok(normalize_remote(&junto))
    };
    let sem_espacos = one(trimmed)?;
    if exact == trimmed {
        return Ok(Some((sem_espacos, None)));
    }
    match one(exact) {
        Ok(p) if p != sem_espacos => Ok(Some((p, Some(sem_espacos)))),
        _ => Ok(Some((sem_espacos, None))),
    }
}

/// Normaliza um caminho remoto absoluto: sem barras repetidas, "." e "..".
/// O ".." e resolvido pelo texto (como a linha ".."), nunca acima da raiz.
fn normalize_remote(p: &str) -> String {
    let mut partes: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                partes.pop();
            }
            s => partes.push(s),
        }
    }
    format!("/{}", partes.join("/"))
}

/// Linha de uma classe de permissao (proprietario/grupo/publico) com os tres
/// checkboxes Ler/Gravar/Executar, ligados aos bits indicados de `mode`.
fn perm_class(ui: &mut egui::Ui, mode: &mut u32, label: &str, r: u32, w: u32, x: u32) {
    ui.label(egui::RichText::new(label).color(TEXT_WEAK));
    ui.horizontal(|ui| {
        for (bit, texto) in [(r, "Ler"), (w, "Gravar"), (x, "Executar")] {
            let mut on = *mode & bit != 0;
            if painted_checkbox(ui, &mut on, texto).changed() {
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
/// A resposta vem com `changed()` se o estado mudou (e aceita dica no hover).
fn painted_checkbox(ui: &mut egui::Ui, on: &mut bool, text: &str) -> egui::Response {
    let font = egui::FontId::proportional(14.0);
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_owned(), font.clone(), TEXT);
    let box_sz = 16.0;
    let gap = 6.0;
    let w = box_sz + gap + galley.size().x;
    let h = box_sz.max(galley.size().y);
    let (rect, mut response) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
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

    response
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

/// Botao com largura ajustada ao texto (minimo 100 px, nunca maior que a
/// area disponivel), para rotulos variaveis como caminhos. `hover`: dica
/// opcional (ex.: o caminho completo).
fn fit_btn(ui: &mut egui::Ui, text: &str, s: &BtnStyle, hover: &str) -> bool {
    let text_w = ui.fonts(|f| {
        f.layout_no_wrap(
            text.to_string(),
            egui::FontId::proportional(14.0),
            egui::Color32::WHITE,
        )
        .size()
        .x
    });
    let width = (text_w + 24.0).max(100.0).min(ui.available_width().max(100.0));
    let before = ui.cursor().min;
    let clicked = painted_btn(ui, egui::vec2(width, 28.0), text, 14.0, s);
    if !hover.is_empty() {
        let rect = egui::Rect::from_min_size(before, egui::vec2(width, 28.0));
        ui.interact(rect, ui.id().with(("fit_btn_hover", text)), egui::Sense::hover())
            .on_hover_text(hover);
    }
    clicked
}

/// Corta um caminho longo pelo inicio ("...pasta/final"), mantendo o final.
fn elide_path(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(n - (max - 1)).collect();
        format!("\u{2026}{tail}")
    }
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
    /// Modo POSIX; `None` deixa a coluna vazia (ex.: link sem destino).
    mode: Option<u32>,
    owner: &'a str,
    group: &'a str,
    /// Data ja formatada (DD/MM/AAAA) ou vazia.
    date: &'a str,
    /// Tamanho (apenas arquivos); `None` para pastas.
    size: Option<u64>,
    /// Colunas que cabem no painel (ver `visible_cols`).
    show: [bool; 5],
}

/// Altura de uma linha da listagem SFTP.
const ROW_H: f32 = 24.0;

/// Colunas de metadados da listagem, da direita para a esquerda: (titulo,
/// largura). A ordem e a de `RowCols::show`.
const LIST_COLS: [(&str, f32); 5] = [
    ("Modificado", 78.0),
    ("Tamanho", 64.0),
    ("Grupo", 84.0),
    ("Dono", 84.0),
    ("Perm", 46.0),
];

/// Colunas que cabem numa linha de `width` px com espaco para o nome (as 5
/// somam 356 px): abaixo de 560 px saem Dono e Grupo; abaixo de 420, Perm e
/// Modificado; abaixo de 250, tambem o Tamanho (tela dividida em 3, janela
/// minima).
fn visible_cols(width: f32) -> [bool; 5] {
    let dono_grupo = width >= 560.0;
    let perm_data = width >= 420.0;
    let tamanho = width >= 250.0;
    [perm_data, tamanho, dono_grupo, dono_grupo, perm_data]
}

/// `text` cortado com "…" para caber em `max_w` px na fonte dada (medido,
/// nao estimado: letras largas como W e m nao invadem as colunas). Texto
/// curto que cabe mesmo so com letras largas nem e medido (a listagem nao e
/// virtualizada: roda para todas as linhas a cada quadro).
fn fit_text(ui: &egui::Ui, text: &str, font: &egui::FontId, max_w: f32) -> String {
    // Nenhum glifo das fontes do app (emoji inclusive) passa de 1,5 x o tamanho.
    if text.chars().count() as f32 * font.size * 1.5 <= max_w {
        return text.to_owned();
    }
    let mut job =
        egui::text::LayoutJob::simple_singleline(text.to_owned(), font.clone(), CARD_TEXT);
    job.wrap = egui::text::TextWrapping {
        max_width: max_w.max(1.0),
        max_rows: 1,
        break_anywhere: true,
        overflow_character: Some('\u{2026}'),
    };
    let g = ui.fonts(|f| f.layout_job(job));
    if !g.elided {
        return text.to_owned();
    }
    g.rows
        .first()
        .map(|r| r.glyphs.iter().map(|gl| gl.chr).collect())
        .unwrap_or_default()
}

fn file_row(
    ui: &mut egui::Ui,
    look: RowLook,
    name: &str,
    cols: Option<RowCols>,
    selected: bool,
    cursor: bool,
) -> egui::Response {
    let RowLook { icon, color, slash } = look;
    let width = ui.available_width();
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, ROW_H), egui::Sense::click());

    // Cursor do teclado (painel em foco) numa linha nao marcada (sempre o
    // caso da ".."): o mesmo fundo do mouse em cima, alem do contorno.
    if selected {
        ui.painter()
            .rect_filled(rect, 4.0, ACCENT.gamma_multiply(0.30));
    } else if response.hovered() || cursor {
        ui.painter()
            .rect_filled(rect, 4.0, ACCENT.gamma_multiply(0.18));
    }
    // Contorno do cursor em ACCENT pleno (bem visivel sobre o fundo escuro).
    if cursor {
        ui.painter().rect_stroke(
            rect.shrink(0.75),
            4.0,
            egui::Stroke::new(1.5, ACCENT),
            egui::StrokeKind::Inside,
        );
    }

    let pad = 6.0;
    let cy = rect.center().y;
    let icon_rect = egui::Rect::from_min_size(
        egui::pos2(rect.left() + pad, cy - 8.0),
        egui::vec2(16.0, 16.0),
    );
    egui::Image::new(icon).tint(color).paint_at(ui, icon_rect);

    // Colunas de metadados a direita (permissoes | proprietario | grupo |
    // tamanho | data), cada uma com largura fixa, alinhadas a direita; num
    // painel estreito so as que cabem.
    let mut x = rect.right() - pad;
    if let Some(c) = &cols {
        let size_str = c.size.map(human_size).unwrap_or_default();
        let columns: [(String, bool); 5] = [
            (c.date.to_string(), false),
            (size_str, false),
            (c.group.to_string(), false),
            (c.owner.to_string(), false),
            (c.mode.map(|m| format!("{m:04o}")).unwrap_or_default(), true),
        ];
        for (((text, mono), (_, w)), on) in columns.into_iter().zip(LIST_COLS).zip(c.show) {
            if !on {
                continue;
            }
            if !text.is_empty() {
                let font = if mono {
                    egui::FontId::monospace(11.0)
                } else {
                    egui::FontId::proportional(11.0)
                };
                ui.painter().text(
                    egui::pos2(x, cy),
                    egui::Align2::RIGHT_CENTER,
                    fit_text(ui, &text, &font, w - 6.0),
                    font,
                    TEXT_WEAK,
                );
            }
            x -= w;
        }
    }

    // Nome (a esquerda), cortado pela largura medida para nao invadir as
    // colunas da direita. Pasta: "/" no fim, sempre visivel (o corte fica
    // antes dela).
    let font = egui::FontId::proportional(13.0);
    let name_left = icon_rect.right() + 8.0;
    let avail = (x - 8.0 - name_left).max(24.0);
    let shown = if slash {
        let slash_w = ui.fonts(|f| f.glyph_width(&font, '/'));
        format!("{}/", fit_text(ui, name, &font, avail - slash_w))
    } else {
        fit_text(ui, name, &font, avail)
    };
    ui.painter().text(
        egui::pos2(name_left, cy),
        egui::Align2::LEFT_CENTER,
        shown,
        font,
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
    /// Baixar itens do navegador SFTP do painel (pede a pasta no fim do quadro).
    Download {
        path: Vec<usize>,
        remote_dir: String,
        picks: Vec<download::Pick>,
    },
    /// Copiar/recortar no navegador SFTP (substitui o que havia).
    ClipSet(FsClip),
    /// Desistir de copiar/mover (Esc ou o "x" da faixa).
    ClipClear,
    /// Devolver um recorte que nao saiu da origem (se nada novo foi copiado).
    ClipRestore(FsClip),
    /// Colar (Ctrl+V ou o botao da faixa) no painel indicado.
    Paste { path: Vec<usize> },
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
const ICON_FOLDER_SYMLINK: egui::ImageSource =
    egui::include_image!("../assets/folder-symlink.svg");
const ICON_FILE_SYMLINK: egui::ImageSource = egui::include_image!("../assets/file-symlink.svg");
const ICON_EYE: egui::ImageSource = egui::include_image!("../assets/eye.svg");
const ICON_ARROW_LEFT: egui::ImageSource = egui::include_image!("../assets/arrow-left.svg");
const ICON_CHEVRON_UP: egui::ImageSource = egui::include_image!("../assets/chevron-up.svg");
const ICON_CHEVRON_DOWN: egui::ImageSource = egui::include_image!("../assets/chevron-down.svg");
const ICON_FOLDER_SYNC: egui::ImageSource = egui::include_image!("../assets/folder-sync.svg");
const ICON_DOWNLOAD: egui::ImageSource = egui::include_image!("../assets/download.svg");
const ICON_PEN: egui::ImageSource = egui::include_image!("../assets/pen.svg");
const ICON_COPY: egui::ImageSource = egui::include_image!("../assets/copy.svg");
const ICON_CUT: egui::ImageSource = egui::include_image!("../assets/scissors.svg");
const ICON_USERS: egui::ImageSource = egui::include_image!("../assets/users.svg");
const ICON_PLUS: egui::ImageSource = egui::include_image!("../assets/plus.svg");
const ICON_KEY: egui::ImageSource = egui::include_image!("../assets/key-round.svg");
const ICON_PASSWORD: egui::ImageSource =
    egui::include_image!("../assets/rectangle-ellipsis.svg");

/// Icones dos sistemas operacionais (slug de `osinfo::icon_slug`, imagem e
/// cor), para o badge do cartao do host. Vem do Simple Icons 16.33.0 (creditos
/// e licencas no README); o SVG original so ganhou `fill="#ffffff"` para aceitar
/// o tint, como os Lucide. Cor: a da marca, clareada em OKLCH ate contraste de
/// pelo menos 4,5:1 sobre WIDGET_BG (7:1 no CentOS, de traco fino); marcas
/// pretas usam CARD_TEXT.
const OS_ICONS: &[(&str, egui::ImageSource<'static>, egui::Color32)] = &[
    ("almalinux", egui::include_image!("../assets/os/almalinux.svg"), CARD_TEXT),
    ("alpinelinux", egui::include_image!("../assets/os/alpinelinux.svg"), hex("#5194bd")),
    ("apple", egui::include_image!("../assets/os/apple.svg"), CARD_TEXT),
    ("archlinux", egui::include_image!("../assets/os/archlinux.svg"), hex("#1a95d3")),
    ("centos", egui::include_image!("../assets/os/centos.svg"), hex("#9facff")),
    ("debian", egui::include_image!("../assets/os/debian.svg"), hex("#ea5f68")),
    ("devuan", egui::include_image!("../assets/os/devuan.svg"), hex("#538fda")),
    ("elementary", egui::include_image!("../assets/os/elementary.svg"), hex("#64baff")),
    ("endeavouros", egui::include_image!("../assets/os/endeavouros.svg"), hex("#7f7fff")),
    ("fedora", egui::include_image!("../assets/os/fedora.svg"), hex("#51a2da")),
    ("freebsd", egui::include_image!("../assets/os/freebsd.svg"), hex("#e66359")),
    ("gentoo", egui::include_image!("../assets/os/gentoo.svg"), hex("#9185bc")),
    ("linuxmint", egui::include_image!("../assets/os/linuxmint.svg"), hex("#86be43")),
    ("manjaro", egui::include_image!("../assets/os/manjaro.svg"), hex("#35bfa4")),
    ("nixos", egui::include_image!("../assets/os/nixos.svg"), hex("#658cd9")),
    ("opensuse", egui::include_image!("../assets/os/opensuse.svg"), hex("#73ba25")),
    ("openwrt", egui::include_image!("../assets/os/openwrt.svg"), hex("#00b5e2")),
    ("popos", egui::include_image!("../assets/os/popos.svg"), hex("#48b9c7")),
    ("raspberrypi", egui::include_image!("../assets/os/raspberrypi.svg"), hex("#e2647a")),
    ("redhat", egui::include_image!("../assets/os/redhat.svg"), hex("#ff4b3b")),
    ("rockylinux", egui::include_image!("../assets/os/rockylinux.svg"), hex("#10b981")),
    ("slackware", egui::include_image!("../assets/os/slackware.svg"), CARD_TEXT),
    ("suse", egui::include_image!("../assets/os/suse.svg"), hex("#6e948c")),
    ("ubuntu", egui::include_image!("../assets/os/ubuntu.svg"), hex("#f15c29")),
    ("voidlinux", egui::include_image!("../assets/os/voidlinux.svg"), hex("#609979")),
    ("zorin", egui::include_image!("../assets/os/zorin.svg"), hex("#15a6f0")),
];

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
/// Ambar suave das pastas (e links para pasta) no navegador SFTP: separa
/// pasta de arquivo (CARD_TEXT, neutro) pela cor, alem do icone e da "/".
const FOLDER_FG: egui::Color32 = hex("#e5c07b");

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
    /// "Esquecer chave" do servidor: vale so ao salvar (Cancelar descarta).
    forget_key: bool,
    /// Detectar o sistema do servidor ao conectar (ver `Host::detect_os`).
    detect_os: bool,
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
            forget_key: false,
            detect_os: true,
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
            forget_key: false,
            detect_os: h.detect_os,
        }
    }

    /// Host com os dados do formulario. A chave do servidor e o SO detectado vem
    /// do cofre ATUAL (`current`), pois podem ter chegado com o editor aberto;
    /// so sao mantidos se o endereco e a porta nao mudaram (a chave, se o
    /// usuario nao pediu para esquece-la; o SO, se a deteccao segue ligada).
    fn to_host(&self, id: uuid::Uuid, current: Option<&Host>) -> Host {
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
        let mut host = Host {
            id,
            name: self.name.trim().to_string(),
            host: self.host.trim().to_string(),
            port: self.parsed_port().unwrap_or(22),
            username: self.username.trim().to_string(),
            auth,
            host_key: None,
            os: None,
            detect_os: self.detect_os,
        };
        host.host_key = current
            .filter(|c| !self.forget_key && same_endpoint(&host.host, host.port, c))
            .and_then(|c| c.host_key.clone());
        // SO detectado: vem do cofre ATUAL (pode ter chegado com o editor aberto) e
        // so vale para o mesmo endereco e porta; "Esquecer chave" nao o apaga.
        // Deteccao desligada: nada de sistema guardado.
        host.os = current
            .filter(|c| self.detect_os && same_endpoint(&host.host, host.port, c))
            .and_then(|c| c.os.clone());
        host
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
    // Chave do cofre aberto (sal + chave derivada da senha), para gravar; a
    // senha em si nao fica guardada.
    master_key: Option<VaultKey>,

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

    // Arquivo das chaves do "abrir sem senha neste computador" (ver remember);
    // `None` sem pasta de dados do app (e nos testes, salvo os do proprio recurso).
    remember_file: Option<PathBuf>,

    // O cofre aberto abre sozinho neste computador (caixa da tela de conexoes).
    remembered: bool,

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

    // Proximo id de lote de envio de arquivos soltos sobre um terminal.
    next_upload_id: u64,

    // Ordem de chegada da proxima pergunta de chave do servidor (fila).
    next_host_key_seq: u64,

    // Esc (sem modificadores) recebido neste quadro com uma pergunta de chave
    // aberta: cancela, se a janela ja estiver armada.
    host_key_esc: bool,

    // Download pedido no navegador SFTP aguardando a escolha da pasta (o
    // dialogo do Windows abre no fim do quadro).
    pending_download: Option<PendingDownload>,

    // Ultima pasta escolhida para downloads (so em memoria; o dialogo do
    // Windows tambem lembra a ultima usada).
    download_dir: Option<PathBuf>,

    // Proximo id de lote de download.
    next_download_id: u64,

    // Hosts cuja sonda do SO ja respondeu nesta abertura do cofre: a proxima
    // conexao com eles nao repete a sonda (ver osinfo).
    os_checked: std::collections::HashSet<uuid::Uuid>,

    // Itens copiados/recortados no navegador SFTP (Ctrl+C/Ctrl+X), a colar
    // com Ctrl+V num painel da mesma conexao.
    fs_clip: Option<FsClip>,

    // Proximo id de lote de colar.
    next_paste_id: u64,
}

/// `host`:`port` e o mesmo endereco do host do cofre (sem diferenciar
/// maiusculas, ignorando espacos nas pontas). A chave do servidor guardada so
/// vale para o mesmo endereco e porta.
fn same_endpoint(host: &str, port: u16, h: &Host) -> bool {
    port == h.port && host.trim().eq_ignore_ascii_case(h.host.trim())
}

/// Nome de exibicao de um host: o apelido, ou o endereco quando sem apelido.
fn display_name(host: &Host) -> String {
    if host.name.trim().is_empty() {
        host.host.clone()
    } else {
        host.name.clone()
    }
}

/// Endereco como aparece no cartao e na dica.
fn host_address(h: &Host) -> String {
    format!("{}@{}:{}", h.username, h.host, h.port)
}

const HOST_HINT_USE: &str = "Duplo clique conecta \u{00b7} botão direito para opções";
const LOCAL_HINT_USE: &str = "Duplo clique abre \u{00b7} botão direito para opções";
const NEW_HOST_HINT: &str = "Clique para cadastrar uma conexão SSH (Ctrl+N)";

/// Opcao do editor que liga a deteccao do SO (ver osinfo). Para que serve e o
/// motivo para desligar ficam na dica (uma linha so no editor, que precisa
/// caber na janela minima).
const DETECT_OS_LABEL: &str = "Detectar o sistema do servidor";
const DETECT_OS_HINT: &str = "Mostra no cartão o ícone da distribuição. Ao conectar, o \
     SaguTerm executa no servidor um comando que só lê a identificação do sistema (veja a \
     política de privacidade). Desmarque se o servidor força um comando próprio (ForceCommand \
     ou command= no authorized_keys): é ele que rodaria no lugar desse, uma vez a mais.";
const DETECT_OS_CLEARS: &str = "O sistema detectado será apagado ao salvar.";

/// Linhas da dica de um host: nome completo, endereco, autenticacao, sistema
/// detectado (quando houver) e como usar.
fn host_hint(host: &Host) -> Vec<String> {
    let auth = match host.auth {
        AuthMethod::Password { .. } => "Autenticação por senha",
        AuthMethod::Key { .. } => "Autenticação por chave",
    };
    let mut lines = vec![display_name(host), host_address(host), auth.to_string()];
    // O prefixo deixa claro que o texto veio do servidor (ja saneado e
    // limitado em osinfo).
    if let Some(os) = &host.os {
        lines.push(format!("Sistema: {}", os.label()));
    }
    lines.push(HOST_HINT_USE.to_string());
    lines
}

/// Titulo e subtitulo do cartao de terminal local.
fn local_tile_text(shell: pty::LocalShell) -> (&'static str, &'static str) {
    match shell {
        pty::LocalShell::Cmd => ("Terminal local", "Prompt de comando do Windows"),
        pty::LocalShell::Wsl => ("WSL", "Linux (Windows Subsystem for Linux)"),
    }
}

/// Dica do cartao de terminal local (o subtitulo aparece inteiro).
fn local_hint(shell: pty::LocalShell) -> Vec<String> {
    let (titulo, subtitulo) = local_tile_text(shell);
    vec![titulo.to_string(), subtitulo.to_string(), LOCAL_HINT_USE.to_string()]
}

/// Icone do badge de um host: a marca do sistema detectado quando ha icone
/// para ele; senao o servidor do tema (pedido: sem identificacao ou sem
/// icone, mantem o atual).
fn host_icon(host: &Host) -> TileIcon {
    host.os
        .as_ref()
        .and_then(osinfo::icon_slug)
        .and_then(|slug| OS_ICONS.iter().find(|(s, ..)| *s == slug))
        .map(|(_, image, tint)| TileIcon {
            image: image.clone(),
            tint: *tint,
        })
        .unwrap_or_else(|| TileIcon::theme(ICON_SERVER))
}

const STORAGE_LAST_PATH: &str = "last_vault_path";

/// Opcao da tela de conexoes que abre o cofre sem a senha (ver remember). O
/// que guarda e quando volta a pedir a senha ficam na dica.
const REMEMBER_LABEL: &str = "Abrir sem senha neste computador";
const REMEMBER_HINT: &str = "Ao iniciar o SaguTerm neste computador, com o seu usuário do \
     Windows, este cofre abre direto, sem pedir a senha. A chave do cofre (não a senha) fica \
     guardada protegida pela sua conta do Windows: em outro computador ou outra conta, o \
     arquivo continua pedindo a senha. Bloquear o cofre volta a pedir a senha na próxima \
     abertura. Proteja a conta do Windows com senha ou PIN e bloqueie a tela (Win+L) ao se \
     afastar.";
const AUTO_OPEN_FAILED: &str = "O cofre não abriu sozinho neste computador. Digite a senha \
     e, se quiser, marque de novo \u{201C}Abrir sem senha neste computador\u{201D}.";

/// Versao do app no topo da ajuda (vem do Cargo.toml, em tempo de compilacao).
const APP_VERSION_LABEL: &str = concat!("versão ", env!("CARGO_PKG_VERSION"));
/// Altura reservada na ajuda para barra de titulo, cabecalho, rodape e margens;
/// o resto da tela vai para a lista de atalhos, que rola quando nao cabe.
const HELP_CHROME_H: f32 = 230.0;
/// Folga minima entre o editor de host e as bordas de cima e de baixo da tela.
const EDITOR_SCREEN_GAP: f32 = 12.0;
/// Altura do editor de host fora do cabecalho e do formulario rolavel: folgas
/// ate as bordas da tela, margem e borda do quadro (25 + 25) e rodape com
/// Salvar/Cancelar (8 + 16 + 34).
const EDITOR_CHROME_H: f32 = 2.0 * EDITOR_SCREEN_GAP + 50.0 + 58.0;
/// Altura minima do formulario rolavel do editor de host.
const EDITOR_MIN_FORM_H: f32 = 120.0;
/// Legenda da listagem SFTP na ajuda (logo apos os atalhos do navegador).
const HELP_SFTP_LEGEND: &str = "Pastas em âmbar com \u{201C}/\u{201D}; links simbólicos têm \
     seta no ícone (link para pasta fica junto das pastas e abre com Enter; link quebrado em \
     vermelho). Passe o mouse para ver o destino.";
/// Creditos dos icones (interface e sistemas), no fim da ajuda.
const HELP_ICON_CREDITS: &str = "Ícones da interface: Lucide (ISC) e Feather (MIT). \
     Ícones de sistemas operacionais: Simple Icons \
     (simpleicons.org) e autores de cada logotipo, sob as licenças listadas no README do \
     projeto (github.com/alowelter/sagu-term). Marcas e logotipos pertencem aos seus donos.";

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
            master_key: None,
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
            remember_file: remember::default_file(),
            remembered: false,
            gate_focus_requested: true,
            gate_busy: 0,
            focused_path: None,
            chord_armed_at: None,
            pane_rects: Vec::new(),
            pending_focus: None,
            last_pane_focus: None,
            show_help: false,
            pending_delete: None,
            next_upload_id: 1,
            next_host_key_seq: 1,
            host_key_esc: false,
            pending_download: None,
            download_dir: None,
            next_download_id: 1,
            fs_clip: None,
            next_paste_id: 1,
            os_checked: std::collections::HashSet::new(),
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
        let key = self
            .master_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("nenhum cofre aberto"))?;
        let bytes = vault::encrypt_vault(&self.vault, key)?;
        // Escrita atomica: grava num arquivo temporario ao lado e renomeia por
        // cima. Uma falha no meio (queda de energia, disco cheio) nunca deixa
        // o cofre — unico arquivo com todas as credenciais — corrompido.
        let tmp = path.with_extension("sagu.tmp");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            // Dados no disco antes do rename: sem isso o NTFS pode gravar o
            // rename (metadado, com journal) antes do conteudo e, numa queda
            // de energia, o cofre voltaria zerado.
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn lock(&mut self) {
        // Abrindo sozinho neste computador: a proxima abertura do app pede a
        // senha (senao fechar e abrir o app desfaria o bloqueio).
        if let (true, Some(file), Some(key)) =
            (self.remembered, &self.remember_file, &self.master_key)
        {
            let _ = remember::suspend(file, key.salt());
        }
        if let Some(root) = &self.root {
            disconnect_tree(root);
        }
        self.root = None;
        self.last_pane_focus = None;
        self.pending_download = None;
        self.fs_clip = None;
        self.vault = Vault::default();
        self.master_key = None;
        self.remembered = false;
        self.clear_gate_passwords();
        self.editor = None;
        self.hosts_filter.clear();
        self.os_checked.clear();
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

        let opened = match self.gate_mode {
            GateMode::Open => std::fs::read(&path)
                .map_err(|e| format!("Não foi possível ler o arquivo: {e}"))
                .and_then(|bytes| {
                    vault::decrypt_vault(&bytes, &self.gate_password).map_err(|e| format!("{e}"))
                }),
            GateMode::Create => {
                if self.gate_password != self.gate_password_confirm {
                    self.gate_error = Some("As senhas não conferem.".into());
                    return;
                }
                let v = Vault::default();
                VaultKey::new(&self.gate_password)
                    .and_then(|key| Ok((vault::encrypt_vault(&v, &key)?, key)))
                    .map_err(|e| format!("{e}"))
                    .and_then(|(bytes, key)| {
                        std::fs::write(&path, bytes)
                            .map(|()| (v, key))
                            .map_err(|e| format!("Não foi possível gravar: {e}"))
                    })
            }
        };
        match opened {
            Ok((v, key)) => self.open_vault(path, v, key, false),
            Err(e) => self.gate_error = Some(e),
        }
    }

    /// Cofre aberto, pela senha ou pela chave guardada neste computador (`auto`):
    /// vai para a tela de conexoes.
    fn open_vault(&mut self, path: PathBuf, vault: Vault, key: VaultKey, auto: bool) {
        // Com a senha, um cofre que abre sozinho neste computador renova a
        // chave guardada e sai da suspensao do bloqueio.
        self.remembered = match &self.remember_file {
            Some(_) if auto => true,
            Some(file) if remember::state(file, key.salt()) != remember::State::Off => {
                remember::enable(file, &key).is_ok()
            }
            _ => false,
        };
        self.last_vault_path = path.to_string_lossy().to_string();
        self.vault = vault;
        self.vault_path = Some(path);
        self.master_key = Some(key);
        self.clear_gate_passwords();
        self.screen = Screen::Hosts;
    }

    /// Ao sair da splash: abre sozinho o ultimo cofre se a chave dele esta
    /// guardada neste computador (ver remember); senao, o portao pede a senha.
    fn leave_splash(&mut self) {
        if !self.try_auto_open() {
            self.screen = Screen::Gate;
            self.gate_focus_requested = true;
        }
    }

    fn try_auto_open(&mut self) -> bool {
        let Some(file) = self.remember_file.clone() else {
            return false;
        };
        if self.last_vault_path.trim().is_empty() {
            return false;
        }
        let path = PathBuf::from(self.last_vault_path.trim());
        let Ok(bytes) = std::fs::read(&path) else {
            return false;
        };
        let Some(salt) = vault::file_salt(&bytes) else {
            return false;
        };
        let opened = match remember::key_for(&file, salt) {
            Ok(None) => return false,
            Ok(Some(key)) => vault::decrypt_with_key(&bytes, &key).map(|v| (v, key)),
            Err(e) => Err(e),
        };
        match opened {
            Ok((v, key)) => {
                self.open_vault(path, v, key, true);
                true
            }
            Err(_) => {
                // A chave guardada nao serve mais (senha do Windows redefinida,
                // arquivo adulterado): apaga e pede a senha.
                let _ = remember::disable(&file, salt);
                self.gate_error = Some(AUTO_OPEN_FAILED.into());
                false
            }
        }
    }

    /// Liga/desliga o "abrir sem senha neste computador" do cofre aberto.
    fn set_remembered(&mut self, on: bool) {
        let (Some(file), Some(key)) = (&self.remember_file, &self.master_key) else {
            return;
        };
        let result = if on {
            remember::enable(file, key)
        } else {
            remember::disable(file, key.salt())
        };
        match result {
            Ok(()) => {
                self.remembered = on;
                self.hosts_error = None;
            }
            Err(e) => {
                let acao = if on { "ligar" } else { "desligar" };
                self.hosts_error = Some(format!("Não foi possível {acao} a abertura sem senha: {e}"));
            }
        }
    }

    /// Apaga as senhas digitadas no portao, sobrescrevendo a memoria delas.
    fn clear_gate_passwords(&mut self) {
        use zeroize::Zeroize;
        self.gate_password.zeroize();
        self.gate_password_confirm.zeroize();
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
            // Caminho do cofre a esquerda (cortado se nao couber) e, a direita,
            // a opcao de abrir sem a senha neste computador.
            let path_text = format!("\u{1f5c4}  {}", path.to_string_lossy());
            let mut remember_on = self.remembered;
            let mut toggled = false;
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.remember_file.is_some() {
                        toggled = painted_checkbox(ui, &mut remember_on, REMEMBER_LABEL)
                            .on_hover_text(REMEMBER_HINT)
                            .changed();
                        ui.add_space(12.0);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(path_text).color(TEXT_WEAK))
                                .truncate(),
                        );
                    });
                });
            });
            if toggled {
                self.set_remembered(remember_on);
            }
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
            "Altere os dados da conexão e salve."
        } else {
            "Preencha os dados para cadastrar uma nova conexão."
        };

        // Largura util dos campos (mesmo padrao do portao do cofre).
        const FIELD_W: f32 = 360.0;

        // Host em edicao como esta no cofre agora (a chave do servidor pode
        // ser aceita ou esquecida por outro caminho com o editor aberto).
        let stored = editor
            .id
            .and_then(|id| self.vault.hosts.iter().find(|h| h.id == id));

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

        // Um pouco acima do centro quando sobra espaco; numa tela baixa, so o
        // que deixa a folga EDITOR_SCREEN_GAP (pela altura do quadro anterior).
        let height_id = egui::Id::new("host_editor_window").with("altura");
        let screen_h = ctx.screen_rect().height();
        let lift = ctx
            .data(|d| d.get_temp::<f32>(height_id))
            .map_or(0.0, |h| ((screen_h - h) / 2.0 - EDITOR_SCREEN_GAP).clamp(0.0, 20.0));

        let shown = egui::Window::new("host_editor_window")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .movable(true)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, -lift])
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

                // Formulario rolavel entre o cabecalho e os botoes: a janela
                // inteira cabe na tela (inclusive na minima, 640x420) e Salvar
                // e Cancelar ficam sempre visiveis. O min_scrolled_height faz a
                // area ja nascer com a altura final (ver `ui_help`).
                let header_h = ui.cursor().min.y - ui.min_rect().min.y;
                let form_h = (screen_h - EDITOR_CHROME_H - header_h).max(EDITOR_MIN_FORM_H);
                // Barra fina visivel sempre que o formulario rola (a flutuante
                // padrao so aparece com o mouse em cima): mostra que ha mais
                // campos embaixo.
                ui.spacing_mut().scroll = egui::style::ScrollStyle::thin();
                egui::ScrollArea::vertical()
                    .id_salt("host_editor_scroll")
                    .max_height(form_h)
                    .min_scrolled_height(form_h)
                    .show(ui, |ui| {
                        // Largura de um campo de FIELD_W com a margem interna
                        // do TextEdit (4 + 4), a mesma da linha de botoes: os
                        // campos nao encolhem e a barra, quando aparece, fica
                        // ao lado deles.
                        ui.set_min_width(FIELD_W + 8.0);

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
                                        .hint_text("endereço ou IP"),
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
                        ui.label(egui::RichText::new("Usuário").small().color(TEXT_WEAK));
                        ui.add(
                            egui::TextEdit::singleline(&mut editor.username)
                                .desired_width(FIELD_W),
                        );

                        ui.add_space(10.0);

                        // Autenticacao: abas segmentadas (Senha/Chave).
                        ui.label(egui::RichText::new("Autenticação").small().color(TEXT_WEAK));
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

                        // Deteccao do SO (ver osinfo), desligavel por conexao. Embaixo,
                        // so o aviso de que desmarcar apaga o sistema ja detectado.
                        ui.add_space(10.0);
                        painted_checkbox(ui, &mut editor.detect_os, DETECT_OS_LABEL)
                            .on_hover_text(DETECT_OS_HINT);
                        if !editor.detect_os && stored.is_some_and(|c| c.os.is_some()) {
                            ui.label(
                                egui::RichText::new(DETECT_OS_CLEARS).small().color(HIGHLIGHT),
                            );
                        }

                        // Chave do servidor ja aceita (so ao editar um host existente).
                        if let Some(cur) = stored {
                            ui.add_space(10.0);
                            ui.label(
                                egui::RichText::new("Chave do servidor")
                                    .small()
                                    .color(TEXT_WEAK),
                            );
                            let same = editor
                                .parsed_port()
                                .is_some_and(|port| same_endpoint(&editor.host, port, cur));
                            match &cur.host_key {
                                Some(_) if editor.forget_key => {
                                    ui.label(
                                        egui::RichText::new(
                                            "A chave será esquecida ao salvar e confirmada \
                                             de novo na próxima conexão.",
                                        )
                                        .color(TEXT_WEAK),
                                    );
                                    if fit_btn(ui, "Desfazer", &BTN_GHOST, "") {
                                        editor.forget_key = false;
                                    }
                                }
                                Some(key) if same => {
                                    match hostkey::describe(key) {
                                        Some(info) => {
                                            ui.label(
                                                egui::RichText::new(info.fingerprint)
                                                    .monospace()
                                                    .size(11.0)
                                                    .color(CARD_TEXT),
                                            );
                                            ui.label(
                                                egui::RichText::new(info.algorithm)
                                                    .small()
                                                    .color(TEXT_WEAK),
                                            );
                                        }
                                        None => {
                                            ui.label(
                                                egui::RichText::new("(chave guardada ilegível)")
                                                    .small()
                                                    .color(ERROR_FG),
                                            );
                                        }
                                    }
                                    if fit_btn(
                                        ui,
                                        "Esquecer chave",
                                        &BTN_GHOST,
                                        "A chave será pedida de novo na próxima conexão.",
                                    ) {
                                        editor.forget_key = true;
                                    }
                                }
                                Some(_) => {
                                    ui.label(
                                        egui::RichText::new(
                                            "O endereço ou a porta mudou: a chave guardada será \
                                             apagada ao salvar.",
                                        )
                                        .small()
                                        .color(HIGHLIGHT),
                                    );
                                }
                                None => {
                                    ui.label(
                                        egui::RichText::new(
                                            "Nenhuma chave guardada; ela será confirmada na \
                                             primeira conexão.",
                                        )
                                        .small()
                                        .color(TEXT_WEAK),
                                    );
                                }
                            }
                        }
                    });

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
        if let Some(r) = shown {
            ctx.data_mut(|d| d.insert_temp(height_id, r.response.rect.height()));
        }

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
            self.commit_editor(editor);
        } else {
            self.editor = Some(editor);
        }
    }

    /// Valida e grava o formulario do editor no cofre (pelo id do host) e salva
    /// o cofre. Dado invalido ou host excluido: o editor continua aberto, com
    /// a mensagem.
    fn commit_editor(&mut self, editor: HostEditor) {
        if editor.host.trim().is_empty() || editor.username.trim().is_empty() {
            self.hosts_error = Some("Host e usuário são obrigatórios.".into());
            self.editor = Some(editor);
            return;
        }
        if editor.parsed_port().is_none() {
            self.hosts_error = Some("Informe uma porta válida (1-65535).".into());
            self.editor = Some(editor);
            return;
        }
        match editor.id {
            Some(id) => {
                // Resolve o host pelo id no momento do save: a lista pode
                // ter mudado (exclusoes) com o editor aberto.
                match self.vault.hosts.iter().position(|h| h.id == id) {
                    Some(i) => {
                        let old = &self.vault.hosts[i];
                        let host = editor.to_host(id, Some(old));
                        // Endereco ou porta mudaram, ou a deteccao foi ligada ou
                        // desligada: o SO guardado caiu (to_host) e a proxima
                        // conexao (com a deteccao ligada) detecta de novo.
                        if !same_endpoint(&host.host, host.port, old)
                            || host.detect_os != old.detect_os
                        {
                            self.os_checked.remove(&id);
                        }
                        self.vault.hosts[i] = host;
                    }
                    None => {
                        self.hosts_error =
                            Some("Este host foi removido enquanto era editado.".into());
                        self.editor = Some(editor);
                        return;
                    }
                }
            }
            None => {
                self.vault
                    .hosts
                    .push(editor.to_host(uuid::Uuid::new_v4(), None));
            }
        }
        self.editor = None;
        self.hosts_error = None;
        if let Err(e) = self.save_vault() {
            self.hosts_error = Some(format!("Erro ao salvar: {e}"));
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

    /// A proxima conexao com o host deve detectar o SO do servidor: so com a
    /// deteccao ligada no host e se a sonda ainda nao respondeu nesta abertura
    /// do cofre.
    fn wants_os_probe(&self, host: &Host) -> bool {
        host.detect_os && !self.os_checked.contains(&host.id)
    }

    /// Conecta o painel (folha) no caminho indicado ao host escolhido.
    fn connect_pane(&mut self, path: &[usize], host_index: usize) {
        let host = self.vault.hosts[host_index].clone();
        let name = display_name(&host);
        let detect_os = self.wants_os_probe(&host);

        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = ssh::connect(host, INITIAL_COLS, INITIAL_ROWS, detect_os, repaint);

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
        let detect_os = self.wants_os_probe(&host);
        let origin = SftpOrigin::of(&host);

        let ctx = self.ctx_for_repaint.clone();
        let repaint = move || {
            if let Some(ctx) = &ctx {
                ctx.request_repaint();
            }
        };
        let handle = sftp::connect(host, detect_os, repaint);

        if let Some(root) = &mut self.root {
            if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                pane.host_name = format!("{name}  (SFTP)");
                pane.origin = Some(origin);
                pane.paste = None;
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
        // Os caminhos mudam: retangulos do quadro anterior deixam de valer
        // (um arquivo solto neste intervalo e ignorado, nunca desviado).
        self.pane_rects.clear();
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
        // Os caminhos mudam: retangulos do quadro anterior deixam de valer.
        self.pane_rects.clear();
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
        // SO detectado pelas sessoes: gravado no cofre depois de percorrer os
        // paineis (ver `apply_os_reports`).
        let mut os_reports: Vec<OsReport> = Vec::new();
        // Colar terminado: re-listagens e recorte a devolver (depois de
        // percorrer os paineis, ver `apply_paste_after`).
        let mut paste_after: Vec<PasteAfter> = Vec::new();
        {
            let next_seq = &mut self.next_host_key_seq;
            let Some(root) = &mut self.root else {
                return;
            };
            // Pergunta de chave recebida: entra na fila (a janela mostra a
            // mais antiga). Uma anterior no mesmo painel (nao deveria haver)
            // e descartada, o que aborta a conexao dela.
            let mut ask = |pane: &mut Pane, prompt: HostKeyPrompt| {
                pane.host_key = Some(PendingHostKey {
                    prompt,
                    seq: *next_seq,
                    shown_at: None,
                });
                *next_seq += 1;
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
                        SshToUi::Error(msg) => {
                            pane.state = SessionState::Error(msg);
                            // A sessao desistiu (ex.: servidor caiu durante a
                            // pergunta): a janela da chave some.
                            pane.host_key = None;
                        }
                        SshToUi::Upload(ev) => apply_upload_event(pane, ev),
                        SshToUi::HostKey(p) => ask(pane, p),
                        // So vai para o cofre: nao toca no terminal nem no painel.
                        SshToUi::Os(r) => os_reports.push(r),
                        SshToUi::Closed => {
                            pane.host_key = None;
                            // Envio em andamento morre com a sessao: mantem o
                            // painel aberto com o aviso em vez de fecha-lo.
                            match pane.upload.as_ref().map(|u| &u.stage) {
                                Some(UploadStage::Locating { .. } | UploadStage::Sending { .. }) => {
                                    pane.upload = Some(UploadUi::notice(
                                        "Envio interrompido: a sessão foi encerrada.",
                                    ));
                                    // Preserva o erro real (ex.: conexao perdida).
                                    if !matches!(pane.state, SessionState::Error(_)) {
                                        pane.state = SessionState::Error(
                                            "sessão encerrada durante o envio de arquivos".into(),
                                        );
                                    }
                                }
                                // A pergunta de destino nao vale mais.
                                Some(UploadStage::Asking(_)) => {
                                    pane.upload = Some(UploadUi::notice(
                                        "Envio cancelado: a sessão foi encerrada.",
                                    ));
                                }
                                _ => {}
                            }
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
                                exp.connected(home.clone());
                                sftp.list_dir(home);
                            }
                        }
                        SftpToUi::Listing { path, entries } => {
                            if let Some(exp) = &mut pane.explorer {
                                exp.apply_listing(&path, entries);
                            }
                        }
                        SftpToUi::Goto { seq, result } => {
                            if let (Some(exp), Some(sftp)) = (&mut pane.explorer, &pane.sftp) {
                                let mut to_list = Vec::new();
                                // Texto com espacos nas pontas que nao abriu:
                                // tenta sem eles.
                                if let Some((s, p)) = exp.apply_goto(seq, result, &mut to_list) {
                                    sftp.goto(s, p);
                                }
                                for p in to_list {
                                    sftp.list_dir(p);
                                }
                            }
                        }
                        SftpToUi::View(ev) => {
                            if let (Some(exp), Some(sftp)) = (&mut pane.explorer, &pane.sftp) {
                                let mut to_list = Vec::new();
                                exp.apply_view_event(ev, &mut to_list);
                                for p in to_list {
                                    sftp.list_dir(p);
                                }
                            }
                        }
                        SftpToUi::HostKey(p) => ask(pane, p),
                        SftpToUi::Download(ev) => apply_download_event(pane, ev),
                        SftpToUi::Paste(ev) => {
                            if let Some(a) = apply_paste_event(pane, ev) {
                                paste_after.push(a);
                            }
                        }
                        SftpToUi::Os(r) => os_reports.push(r),
                        SftpToUi::Error(msg) => {
                            pane.host_key = None;
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
                            pane.host_key = None;
                            // Sessao encerrada: o visualizador e uma abertura em
                            // curso saem (a memoria do arquivo e liberada).
                            if let Some(exp) = &mut pane.explorer {
                                exp.opening = None;
                                exp.viewer = None;
                            }
                            // Download em andamento morre com a sessao: mantem o
                            // painel aberto com o aviso em vez de fecha-lo.
                            if let Some(d) = &mut pane.download {
                                match d.stage {
                                    DownloadStage::Running { .. } => {
                                        d.stage = DownloadStage::done(
                                            "Download interrompido: a sessão foi encerrada.",
                                            Tone::Error,
                                        );
                                        // Preserva o erro real, se houver.
                                        if !matches!(pane.state, SessionState::Error(_)) {
                                            pane.state = SessionState::Error(
                                                "sessão encerrada durante o download".into(),
                                            );
                                        }
                                    }
                                    // A pergunta de conflito nao vale mais.
                                    DownloadStage::Asking(_) => {
                                        d.stage = DownloadStage::done(
                                            "Download cancelado: a sessão foi encerrada.",
                                            Tone::Neutral,
                                        );
                                    }
                                    DownloadStage::Done { .. } => {}
                                }
                            }
                            // Colar em andamento morre com a sessao; a
                            // pergunta (conflito/oferta) nao vale mais.
                            if let Some(p) = &mut pane.paste {
                                let moving = p.op != paste::PasteOp::Copy;
                                let msg = match p.stage {
                                    PasteStage::Running { .. } => Some((
                                        if moving {
                                            "Movimentação interrompida: a sessão foi encerrada."
                                        } else {
                                            "Cópia interrompida: a sessão foi encerrada."
                                        },
                                        Tone::Error,
                                    )),
                                    PasteStage::Asking(_) | PasteStage::Offer(_) => Some((
                                        "Colagem cancelada: a sessão foi encerrada.",
                                        Tone::Neutral,
                                    )),
                                    PasteStage::Done { .. } => None,
                                };
                                if let Some((text, tone)) = msg {
                                    if tone == Tone::Error && !matches!(pane.state, SessionState::Error(_)) {
                                        pane.state = SessionState::Error(if moving {
                                            "sessão encerrada ao mover arquivos".into()
                                        } else {
                                            "sessão encerrada durante a cópia".into()
                                        });
                                    }
                                    p.stage = PasteStage::done(text.to_string(), String::new(), tone);
                                    paste_after.push(PasteAfter {
                                        origin: p.origin.clone(),
                                        refresh: Vec::new(),
                                        moved: Vec::new(),
                                        restore: p.restore.take(),
                                    });
                                }
                            }
                            if !matches!(pane.state, SessionState::Error(_)) {
                                pane.state = SessionState::Closed;
                                pane.should_close = true;
                            }
                        }
                    }
                }
            });
        }
        self.apply_os_reports(os_reports);
        self.apply_paste_after(paste_after);

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

    /// Grava o SO no host do cofre (pelo id, so com o mesmo endereco e porta da
    /// conexao) e salva o cofre na hora, uma vez, so se algo mudou. Resultado
    /// vazio nao apaga o que ja esta guardado. Falha ao gravar e silenciosa: o SO
    /// fica em memoria e vai na proxima gravacao.
    fn apply_os_reports(&mut self, reports: Vec<OsReport>) {
        let mut changed = false;
        for r in reports {
            let Some(h) = self.vault.hosts.iter_mut().find(|h| h.id == r.host_id) else {
                continue; // host excluido
            };
            if !same_endpoint(&r.host, r.port, h) {
                continue; // endereco/porta mudaram no editor com a sessao aberta
            }
            if !h.detect_os {
                continue; // deteccao desligada no editor com a sessao aberta
            }
            self.os_checked.insert(r.host_id);
            if let Some(os) = r.os.filter(|os| h.os.as_ref() != Some(os)) {
                h.os = Some(os);
                changed = true;
            }
        }
        if changed {
            let _ = self.save_vault();
        }
    }

    /// Tab e Shift+Tab com o teclado num navegador SFTP (a area do painel, o
    /// campo do caminho ou a busca do visualizador) saem da entrada antes de
    /// o egui ver o quadro: ele os usaria para passar o foco a um botao ou a
    /// uma linha do painel, que ficaria sem teclado (as letras seguintes nao
    /// fariam nada ate um clique). Filtro de foco do egui nao basta: so vale
    /// a partir do segundo quadro com o foco. O navegador trata o Tab no
    /// quadro (`FileExplorer::tab`); com um dialogo aberto, o Tab e dele.
    fn take_sftp_tab(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
        // Tab (com qualquer modificador): Some(pressionado).
        let tab = |ev: &egui::Event| match ev {
            egui::Event::Key {
                key: egui::Key::Tab,
                pressed,
                ..
            } => Some(*pressed),
            _ => None,
        };
        if !matches!(self.screen, Screen::Session) || !raw.events.iter().any(|e| tab(e).is_some()) {
            return;
        }
        let (Some(focused), Some(root)) = (ctx.memory(|m| m.focused()), &mut self.root) else {
            return;
        };
        let pressed = raw.events.iter().any(|e| tab(e) == Some(true));
        let mut ours = false;
        for_each_pane_mut(root, &mut |pane| {
            let Some(exp) = pane.explorer.as_mut() else {
                return;
            };
            if exp.keys_home.contains(&focused) {
                exp.tab |= pressed;
                ours = true;
            }
        });
        if ours {
            raw.events.retain(|e| tab(e).is_none());
            // Com o prefixo Ctrl+B armado, o Tab e a segunda tecla (invalida):
            // desarma, como o `handle_session_keys` faria.
            if pressed {
                self.chord_armed_at = None;
            }
        }
    }

    /// Atalhos de teclado da sessao.
    ///
    /// `Alt+setas` troca o painel focado diretamente (atalho principal).
    ///
    /// O restante usa o prefixo `Ctrl+B` em dois tempos (estilo tmux): `H`/`V`
    /// dividem o painel focado, setas tambem movem o foco (alias do Alt+setas),
    /// `O` cicla, `X` fecha o painel, `A` abre a ajuda, `Ctrl+B` de novo envia
    /// um Ctrl+B literal ao terminal (util para tmux remoto) e `F1` envia o F1
    /// que, sem o prefixo, abre a ajuda (ex.: ajuda do htop/mc). Teclas invalidas
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
        // Tecla que o app usaria, enviada tal qual ao terminal focado.
        let mut literal: Option<&[u8]> = None;
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
                    // Copiar/recortar/colar tambem sao a segunda tecla:
                    // engolidos (nao colam no terminal nem no navegador).
                    if matches!(ev, egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_)) {
                        acted = true;
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
                    egui::Key::B if modifiers.ctrl => literal = Some(&[0x02]),
                    egui::Key::F1 => literal = Some(b"\x1bOP"),
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
        } else if let Some(bytes) = literal {
            if let (Some(path), Some(root)) = (&self.focused_path, &mut self.root) {
                if let Some(Node::Leaf(pane)) = node_at_mut(root, path) {
                    if let Some(ssh) = &pane.ssh {
                        ssh.send_data(bytes.to_vec());
                    }
                    // Tecla enviada ao terminal: a visao volta ao fim, como ao digitar.
                    if let Some(term) = &mut pane.terminal {
                        term.scroll_to_bottom();
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

    /// Arquivos soltos na janela (vindos do Explorer): vao para o painel sob o
    /// cursor. Painel SFTP: pasta aberta nele. Terminal SSH: pasta atual do
    /// shell (a sessao descobre qual e). Outros paineis recusam com aviso.
    fn handle_file_drop(&mut self, ctx: &egui::Context) {
        let files: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        let modal_open = self.editor.is_some()
            || self.pending_delete.is_some()
            || self.show_help
            || self.host_key_pending();
        if files.is_empty() || modal_open {
            return;
        }
        let Some(path) = drop_target(ctx, &self.pane_rects) else {
            return;
        };
        let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, &path)) else {
            return;
        };
        if pane.picking {
            return;
        }
        if let (Some(sftp), Some(exp)) = (&pane.sftp, &pane.explorer) {
            if !exp.cur_path.is_empty() {
                for f in files {
                    sftp.upload(f, exp.cur_path.clone());
                }
            }
            return;
        }
        let Some(ssh) = &pane.ssh else {
            return;
        };
        if pane.upload.as_ref().is_some_and(UploadUi::busy) {
            // Um lote por vez (a dica sobre o painel ja avisou).
            return;
        }
        if !ssh.supports_upload() {
            pane.upload = Some(UploadUi::notice(
                "Terminais locais não recebem arquivos; solte sobre um terminal SSH.",
            ));
            return;
        }
        if !matches!(pane.state, SessionState::Connected) {
            pane.upload = Some(UploadUi::notice("Aguarde a conexão para enviar arquivos."));
            return;
        }
        let (dirs, files): (Vec<PathBuf>, Vec<PathBuf>) =
            files.into_iter().partition(|f| f.is_dir());
        if files.is_empty() {
            pane.upload = Some(UploadUi::notice(
                "Pastas ainda não são enviadas; arraste os arquivos.",
            ));
            return;
        }
        let id = self.next_upload_id;
        self.next_upload_id += 1;
        if !ssh.drop_files(id, files) {
            pane.upload = Some(UploadUi::notice("Sessão encerrada; nada foi enviado."));
            return;
        }
        pane.upload = Some(UploadUi {
            id,
            skipped_dirs: dirs.len(),
            stage: UploadStage::Locating {
                since: Instant::now(),
            },
        });
        // O painel que recebeu os arquivos passa a ser o focado.
        self.pending_focus = Some(path);
    }

    /// Realce do painel sob o cursor enquanto arquivos sao arrastados sobre a
    /// janela, com a dica de para onde iriam.
    fn ui_drop_overlay(&self, ctx: &egui::Context) {
        // Com a pergunta de chave aberta, arquivos soltos sao ignorados.
        if ctx.input(|i| i.raw.hovered_files.is_empty()) || self.host_key_pending() {
            return;
        }
        // O Windows nao avisa o movimento do mouse durante o arrasto:
        // redesenha em ciclo curto para o realce acompanhar o cursor.
        ctx.request_repaint_after(std::time::Duration::from_millis(50));
        let Some(path) = drop_target(ctx, &self.pane_rects) else {
            return;
        };
        let Some(rect) = self.pane_rects.iter().find(|(p, _)| *p == path).map(|(_, r)| *r) else {
            return;
        };
        let Some(Node::Leaf(pane)) = self.root.as_ref().and_then(|r| node_at(r, &path)) else {
            return;
        };
        let (text, accepts) = drop_hint(pane);
        if text.is_empty() {
            return;
        }
        let color = if accepts { ACCENT } else { TEXT_WEAK };
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("drop_overlay"),
        ));
        let r = rect.shrink(3.0);
        painter.rect_filled(r, 6.0, color.gamma_multiply(0.14));
        painter.rect_stroke(r, 6.0, egui::Stroke::new(2.0, color), egui::StrokeKind::Inside);
        painter.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(16.0),
            color,
        );
    }

    /// F1 abre/fecha a ajuda de atalhos em qualquer tela, inclusive com um
    /// terminal em foco (o F1 e consumido antes de chegar a ele; para envia-lo
    /// ao servidor ha o `Ctrl+B, F1`). Esc fecha a ajuda.
    fn handle_help_keys(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::F1)) {
            self.show_help = !self.show_help;
        }
        if self.show_help
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            self.show_help = false;
        }
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

            // Cabecalho: nome e versao instalada, bem visiveis (sairam do
            // titulo da janela na 1.1.0).
            let mut cabecalho = egui::text::LayoutJob::default();
            cabecalho.append(
                "SaguTerm",
                0.0,
                egui::TextFormat::simple(egui::FontId::proportional(18.0), CARD_TEXT),
            );
            cabecalho.append(
                APP_VERSION_LABEL,
                8.0,
                egui::TextFormat::simple(egui::FontId::proportional(14.0), ACCENT),
            );
            ui.label(cabecalho);
            ui.add_space(4.0);
            ui.separator();

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

            // Lista de atalhos rolavel: numa tela baixa o cabecalho e o botao
            // Fechar continuam visiveis. O min_scrolled_height faz a lista ja
            // nascer com a altura final (senao ela fica presa a altura inicial
            // da janela, 420, e a janela cresce um pouco a cada quadro, so
            // quando o mouse mexe); com poucos atalhos ela encolhe igual.
            let lista_h = (ctx.screen_rect().height() - HELP_CHROME_H).max(120.0);
            egui::ScrollArea::vertical()
                .id_salt("help_scroll")
                .max_height(lista_h)
                .min_scrolled_height(lista_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    secao(ui, "Geral");
                    atalho(ui, "F1", "abrir/fechar esta ajuda (em qualquer tela)");

                    secao(ui, "Painéis da sessão");
                    atalho(ui, "Alt+setas", "trocar de painel (na direção)");

                    secao(ui, "Painéis da sessão (prefixo Ctrl+B)");
                    atalho(ui, "Ctrl+B, H", "dividir lado a lado");
                    atalho(ui, "Ctrl+B, V", "dividir empilhado");
                    atalho(ui, "Ctrl+B, setas", "trocar de painel (alternativa)");
                    atalho(ui, "Ctrl+B, O", "ciclar o foco");
                    atalho(ui, "Ctrl+B, X", "fechar o painel focado");
                    atalho(ui, "Ctrl+B, Ctrl+B", "enviar Ctrl+B ao terminal");
                    atalho(ui, "Ctrl+B, F1", "enviar F1 ao terminal");
                    atalho(ui, "Ctrl+B, A", "abrir/fechar esta ajuda");
                    atalho(ui, "Ctrl+B, Esc", "cancelar o prefixo");

                    secao(ui, "Terminal (histórico)");
                    atalho(ui, "roda do mouse", "rolar o histórico (3 linhas por clique)");
                    atalho(ui, "Shift+PgUp/PgDn", "rolar uma página");
                    atalho(ui, "Shift+Home/End", "início / fim do histórico");
                    atalho(ui, "digitar", "voltar ao fim");
                    atalho(ui, "Shift+roda", "rolar mesmo com o programa usando o mouse");
                    atalho(ui, "Ctrl+B, Ctrl+B, [", "rolar dentro do tmux (q sai)");
                    atalho(ui, "set -g mouse on", "no ~/.tmux.conf: a roda rola o tmux");

                    secao(ui, "Seleção de conexões");
                    atalho(ui, "digitar", "filtrar pelo nome da conexão");
                    atalho(ui, "setas", "escolher na grade");
                    atalho(ui, "Enter", "conectar a seleção");
                    atalho(ui, "Ctrl+Enter", "abrir SFTP da seleção");
                    atalho(ui, "Esc", "limpar filtro / fechar seletor");
                    atalho(ui, "Ctrl+N", "novo host (em qualquer seletor)");
                    atalho(ui, "Ctrl+L", "bloquear o cofre (tela de conexões)");

                    secao(ui, "Navegador SFTP (painel em foco)");
                    atalho(ui, "setas", "mover na lista (\"..\": pasta acima)");
                    atalho(ui, "PageUp, PageDown", "uma página acima/abaixo");
                    atalho(ui, "Home, End", "primeiro/último item");
                    atalho(ui, "letras", "ir ao item que começa com elas");
                    atalho(ui, "Ctrl+clique", "marcar/desmarcar itens");
                    atalho(ui, "Shift+clique, Shift+setas", "selecionar um intervalo");
                    atalho(ui, "Ctrl+A", "selecionar tudo");
                    atalho(ui, "Ctrl+S", "baixar a seleção para o computador");
                    atalho(ui, "Ctrl+C", "copiar a seleção (cole com Ctrl+V)");
                    atalho(ui, "Ctrl+X", "recortar a seleção para mover");
                    atalho(ui, "Ctrl+V", "colar na pasta atual (copia ou move)");
                    atalho(ui, "Esc", "desistir de copiar/mover");
                    atalho(ui, "Enter, duplo clique", "abrir a pasta ou ver o arquivo (\"..\" sobe)");
                    atalho(ui, "Backspace", "voltar à pasta acima");
                    atalho(ui, "F2", "renomear o item selecionado");
                    atalho(ui, "Delete", "excluir o item selecionado");
                    atalho(ui, "F5", "atualizar a listagem");
                    atalho(ui, "Ctrl+L, clique no caminho", "digitar outro caminho (~ = pasta inicial)");
                    atalho(ui, "Esc", "cancelar a edição do caminho / a abertura");
                    ui.add(
                        egui::Label::new(egui::RichText::new(HELP_SFTP_LEGEND).color(TEXT_WEAK))
                            .wrap(),
                    );

                    secao(ui, "Visualizador de arquivos (somente leitura)");
                    atalho(ui, "setas, PageUp, PageDown", "rolar o texto");
                    atalho(ui, "Home, End", "início/fim do arquivo");
                    atalho(ui, "Ctrl+F", "buscar no arquivo");
                    atalho(ui, "Enter, F3", "próxima ocorrência");
                    atalho(ui, "Shift+Enter, Shift+F3", "ocorrência anterior");
                    atalho(ui, "Tab", "alternar entre a busca e o texto");
                    atalho(ui, "arrastar, Ctrl+A", "selecionar texto");
                    atalho(ui, "Ctrl+C", "copiar (arrastar também copia)");
                    atalho(ui, "Ctrl+S", "baixar o arquivo");
                    atalho(ui, "F5", "recarregar");
                    atalho(ui, "Esc", "fechar a busca / voltar à listagem");

                    secao(ui, "Diálogos e editor de host");
                    atalho(ui, "Esc", "cancelar/fechar");
                    atalho(ui, "Enter", "confirmar (renomear)");
                    atalho(ui, "Ctrl+Enter", "salvar host");

                    // Creditos dos icones dos sistemas (quem instala pela Store
                    // nao le o README).
                    ui.add_space(12.0);
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(HELP_ICON_CREDITS).small().color(TEXT_WEAK),
                        )
                        .wrap(),
                    );
                });

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

    // ---------------- Chave do servidor (TOFU) ----------------

    /// Dica da barra de baixo na sessao. Ctrl+S so aparece com um painel SFTP
    /// em foco: num terminal a tecla vai ao servidor (XOFF, que congela a
    /// saida ate um Ctrl+Q).
    fn session_hint(&self) -> &'static str {
        let pane = match (&self.root, &self.focused_path) {
            (Some(root), Some(path)) => match node_at(root, path) {
                Some(Node::Leaf(p)) => Some(p),
                _ => None,
            },
            _ => None,
        };
        let sftp = pane.is_some_and(|p| p.sftp.is_some());
        // Itens copiados/recortados desta conexao (Ctrl+V cola aqui).
        let clip = self.fs_clip.as_ref().filter(|c| {
            pane.and_then(|p| p.origin.as_ref()).is_some_and(|o| o.same(&c.origin))
        });
        let explorer = pane.and_then(|p| p.explorer.as_ref());
        // Visualizador aberto (tem prioridade sobre a edicao do caminho).
        let viewing = explorer.is_some_and(|e| e.viewer.is_some());
        // Editando o caminho na barra do navegador.
        let editing = explorer.is_some_and(|e| e.path_edit.is_some());
        if sftp && viewing {
            "Esc volta à listagem  \u{00b7}  Ctrl+F busca  \u{00b7}  Ctrl+C copia  \
             \u{00b7}  F5 recarrega  \u{00b7}  F1 ajuda"
        } else if sftp && editing {
            "Enter abre o caminho  \u{00b7}  Esc cancela  \u{00b7}  ~ é a pasta inicial"
        } else if sftp && clip.is_some_and(|c| c.mode == ClipMode::Cut) {
            "Ctrl+V move para cá  \u{00b7}  Esc desiste  \u{00b7}  Alt+setas troca de painel  \
             \u{00b7}  F1 ajuda"
        } else if sftp && clip.is_some() {
            "Ctrl+V cola aqui  \u{00b7}  Esc desiste  \u{00b7}  Alt+setas troca de painel  \
             \u{00b7}  F1 ajuda"
        } else if sftp {
            "Alt+setas troca de painel  \u{00b7}  F1 ajuda  \u{00b7}  F5 atualiza  \
             \u{00b7}  Ctrl+S baixa  \u{00b7}  Ctrl+C/Ctrl+X copia/move"
        } else {
            "Alt+setas troca de painel  \u{00b7}  F1 ajuda  \u{00b7}  Shift+PgUp rola o histórico  \
             \u{00b7}  F5 atualiza SFTP"
        }
    }

    /// Algum painel aguarda a confirmacao da chave do servidor?
    fn host_key_pending(&self) -> bool {
        let mut queue = Vec::new();
        if let Some(root) = &self.root {
            pending_host_keys(root, &mut Vec::new(), &mut queue);
        }
        !queue.is_empty()
    }

    /// Com uma pergunta de chave aberta, nenhuma tecla chega a terminais,
    /// seletores, SFTP ou atalhos (nem Enter/Espaco num botao focado). Esc e
    /// guardado para cancelar (o padrao seguro).
    fn guard_host_key_keys(&mut self, ctx: &egui::Context) {
        if !self.host_key_pending() {
            return;
        }
        let esc = ctx.input_mut(|i| {
            let esc = i.events.iter().any(|e| {
                matches!(
                    e,
                    egui::Event::Key {
                        key: egui::Key::Escape,
                        pressed: true,
                        modifiers,
                        ..
                    } if modifiers.is_none()
                )
            });
            i.events.retain(|e| {
                !matches!(
                    e,
                    egui::Event::Key { .. }
                        | egui::Event::Text(_)
                        | egui::Event::Paste(_)
                        | egui::Event::Copy
                        | egui::Event::Cut
                        | egui::Event::Ime(_)
                )
            });
            esc
        });
        self.host_key_esc |= esc;
    }

    /// Responde sem perguntar o que o cofre ja decide: chave ja aceita (inclusive
    /// por outro painel enquanto esta esperava), host excluido, endereco/porta
    /// mudados. Roda a cada quadro, antes de desenhar a janela.
    fn resolve_host_key_prompts(&mut self) {
        let hosts = &self.vault.hosts;
        let Some(root) = &mut self.root else {
            return;
        };
        for_each_pane_mut(root, &mut |pane| {
            let Some(pending) = &pane.host_key else {
                return;
            };
            let p = &pending.prompt;
            let answer = match hosts.iter().find(|h| h.id == p.host_id) {
                None => HostKeyAnswer::Cancel(HOST_KEY_DELETED.into()),
                Some(h) if !same_endpoint(&p.host, p.port, h) => {
                    HostKeyAnswer::Cancel(HOST_KEY_MOVED.into())
                }
                Some(h)
                    if hostkey::check(h.host_key.as_deref(), &p.presented) == KeyCheck::Match =>
                {
                    HostKeyAnswer::Accept
                }
                Some(_) => return,
            };
            if let Some(pending) = pane.host_key.take() {
                let _ = pending.prompt.reply.send(answer);
            }
        });
    }

    /// Janela modal da chave do servidor, sempre para a pergunta mais antiga da
    /// fila. "Servidor novo" na primeira conexao; alerta vermelho quando a
    /// chave difere da guardada (Cancelar e o padrao). A classificacao e
    /// refeita a cada quadro contra o cofre atual. Cliques e Esc so valem apos
    /// `HOST_KEY_ARM`; Enter/Espaco/Tab nunca chegam aqui (`guard_host_key_keys`)
    /// e clicar fora da janela nao faz nada.
    fn ui_host_key_prompt(&mut self, ctx: &egui::Context) {
        self.resolve_host_key_prompts();
        let esc = std::mem::take(&mut self.host_key_esc);
        let mut queue = Vec::new();
        if let Some(root) = &self.root {
            pending_host_keys(root, &mut Vec::new(), &mut queue);
        }
        let Some((seq, path)) = queue.iter().min_by_key(|(seq, _)| *seq).cloned() else {
            return;
        };
        let waiting = queue.len() - 1;

        let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, &path)) else {
            return;
        };
        let Some(pending) = &mut pane.host_key else {
            return;
        };
        let now = Instant::now();
        let shown_at = *pending.shown_at.get_or_insert(now);
        let (host_id, addr, presented) = (
            pending.prompt.host_id,
            format!("{}:{}", pending.prompt.host, pending.prompt.port),
            pending.prompt.presented.clone(),
        );
        // `resolve_host_key_prompts` garante que o host existe (mesmo endereco).
        let Some(h) = self.vault.hosts.iter().find(|h| h.id == host_id) else {
            return;
        };
        let kind = hostkey::check(h.host_key.as_deref(), &presented);
        let changed = kind == KeyCheck::Changed;
        let name = display_name(h);
        let target = format!("{}@{addr}", h.username);
        let new_info = hostkey::describe(&presented);
        let old_info = h.host_key.as_deref().and_then(hostkey::describe);

        let elapsed = now.saturating_duration_since(shown_at);
        let armed = elapsed >= HOST_KEY_ARM;
        if !armed {
            ctx.request_repaint_after(HOST_KEY_ARM - elapsed);
        }

        let stroke = if changed {
            egui::Stroke::new(1.5, ERROR_FG)
        } else {
            egui::Stroke::new(1.0, CARD_BORDER)
        };
        let frame = egui::Frame::window(&ctx.style())
            .fill(CARD_BG)
            .stroke(stroke)
            .corner_radius(12.0)
            .inner_margin(egui::Margin::same(20));
        let mut accept = false;
        let mut cancel = false;
        let mut copy: Option<String> = None;

        let resp = egui::Modal::new(egui::Id::new(("host_key_prompt", seq)))
            .frame(frame)
            .show(ctx, |ui| {
                ui.set_min_width(420.0);
                ui.set_max_width(480.0);
                ui.spacing_mut().item_spacing.y = 6.0;

                if changed {
                    ui.label(
                        egui::RichText::new("Atenção: a chave do servidor mudou")
                            .size(16.0)
                            .strong()
                            .color(ERROR_FG),
                    );
                } else {
                    ui.label(
                        egui::RichText::new("Servidor novo")
                            .size(16.0)
                            .strong()
                            .color(ACCENT),
                    );
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Primeira conexão com").color(TEXT_WEAK));
                }
                ui.label(egui::RichText::new(&name).size(14.0).color(HIGHLIGHT));
                ui.label(egui::RichText::new(&target).small().color(TEXT_WEAK));
                ui.add_space(8.0);

                if changed {
                    ui.label(
                        egui::RichText::new(
                            "A chave que este servidor apresentou agora é diferente da que \
                             está guardada no cofre.",
                        )
                        .color(CARD_TEXT),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Isso é esperado se o servidor foi reinstalado ou teve as chaves \
                             trocadas. Mas também pode ser um ataque: alguém no meio do \
                             caminho se passando pelo servidor para capturar sua senha e \
                             tudo o que você digitar.",
                        )
                        .color(TEXT_WEAK),
                    );
                    ui.label(
                        egui::RichText::new(
                            "Não aceite sem confirmar a troca com o responsável pelo servidor.",
                        )
                        .color(ERROR_FG),
                    );
                    ui.add_space(8.0);
                    egui::Grid::new(("host_key_grid", seq))
                        .num_columns(2)
                        .spacing([12.0, 8.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Guardada:").color(TEXT_WEAK));
                            ui.vertical(|ui| {
                                key_lines(ui, old_info.as_ref(), "(chave guardada ilegível)", false)
                            });
                            ui.end_row();
                            ui.label(egui::RichText::new("Nova:").color(TEXT_WEAK));
                            ui.vertical(|ui| {
                                if let Some(fp) =
                                    key_lines(ui, new_info.as_ref(), "(chave ilegível)", true)
                                {
                                    copy = Some(fp);
                                }
                            });
                            ui.end_row();
                        });
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        // Cancelar primeiro, com o destaque: e o padrao.
                        if fit_btn(ui, "Cancelar", &BTN_ACCENT, "") {
                            cancel = true;
                        }
                        ui.add_space(6.0);
                        if fit_btn(ui, "Aceitar a nova chave e conectar", &BTN_DANGER, "") {
                            accept = true;
                        }
                    });
                } else {
                    ui.label(
                        egui::RichText::new(
                            "O SaguTerm ainda não conhece a chave deste servidor. Confira se a \
                             impressão digital abaixo é a mesma do servidor antes de confiar.",
                        )
                        .color(CARD_TEXT),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Tipo da chave")
                            .small()
                            .color(TEXT_WEAK),
                    );
                    match &new_info {
                        Some(info) => {
                            ui.label(
                                egui::RichText::new(&info.algorithm).monospace().color(CARD_TEXT),
                            );
                            ui.label(
                                egui::RichText::new("Impressão digital (SHA256)")
                                    .small()
                                    .color(TEXT_WEAK),
                            );
                            ui.horizontal(|ui| {
                                ui.label(
                                    egui::RichText::new(&info.fingerprint)
                                        .monospace()
                                        .color(CARD_TEXT),
                                );
                                if painted_btn(ui, egui::vec2(64.0, 22.0), "Copiar", 12.0, &BTN_GHOST) {
                                    copy = Some(info.fingerprint.clone());
                                }
                            });
                            if let Some(file) = hostkey::server_pub_file(&info.algorithm) {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "No servidor, confira com: ssh-keygen -lf /etc/ssh/{file}"
                                    ))
                                    .small()
                                    .color(TEXT_WEAK),
                                );
                            }
                        }
                        None => {
                            ui.label(egui::RichText::new("(chave ilegível)").color(ERROR_FG));
                        }
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(
                            "Ao confiar, a chave fica guardada no cofre e as próximas conexões \
                             só pedem confirmação se ela mudar.",
                        )
                        .color(TEXT_WEAK),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if fit_btn(ui, "Confiar e conectar", &BTN_ACCENT, "") {
                            accept = true;
                        }
                        ui.add_space(6.0);
                        if fit_btn(ui, "Cancelar", &BTN_GHOST, "") {
                            cancel = true;
                        }
                    });
                }

                if waiting > 0 {
                    ui.add_space(8.0);
                    let text = if waiting == 1 {
                        "Outra conexão aguarda confirmação.".to_string()
                    } else {
                        format!("Outras {waiting} conexões aguardam confirmação.")
                    };
                    ui.label(egui::RichText::new(text).small().color(TEXT_WEAK));
                }
            });
        // Acima de tudo, inclusive da barra de destino do envio (tambem
        // Foreground). Clicar no fundo escurecido nao responde nada.
        ctx.move_to_top(resp.response.layer_id);

        if let Some(fp) = copy {
            ctx.copy_text(fp);
        }
        if !armed {
            return;
        }
        // Na duvida (Esc junto com um clique), cancelar vence.
        if cancel || esc {
            self.answer_host_key(&path, false, kind);
        } else if accept {
            self.answer_host_key(&path, true, kind);
        }
    }

    /// Responde a pergunta do painel em `path`. Aceitar grava a chave no host
    /// do cofre (pelo id, com o mesmo endereco e porta) e salva o cofre na
    /// hora; recusar nao grava nada e a sessao aborta com a mensagem.
    fn answer_host_key(&mut self, path: &[usize], accept: bool, kind: KeyCheck) {
        let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, path)) else {
            return;
        };
        let Some(pending) = pane.host_key.take() else {
            return;
        };
        let prompt = pending.prompt;
        if !accept {
            let msg = if kind == KeyCheck::Changed {
                "Conexão cancelada: a chave do servidor mudou e não foi aceita."
            } else {
                "Conexão cancelada: a chave do servidor não foi confirmada."
            };
            let _ = prompt.reply.send(HostKeyAnswer::Cancel(msg.into()));
            return;
        }
        let Some(h) = self.vault.hosts.iter_mut().find(|h| h.id == prompt.host_id) else {
            let _ = prompt
                .reply
                .send(HostKeyAnswer::Cancel(HOST_KEY_DELETED.into()));
            return;
        };
        if !same_endpoint(&prompt.host, prompt.port, h) {
            let _ = prompt
                .reply
                .send(HostKeyAnswer::Cancel(HOST_KEY_MOVED.into()));
            return;
        }
        h.host_key = Some(prompt.presented.clone());
        if kind == KeyCheck::Changed {
            // Chave diferente aceita: o servidor provavelmente foi reinstalado,
            // e o SO guardado deixa de valer. A sonda desta conexao (se roda
            // nela) ou a da proxima grava o sistema novo.
            h.os = None;
            let id = h.id;
            self.os_checked.remove(&id);
        }
        let saved = self.save_vault();
        // A decisao do usuario vale mesmo se o cofre nao pode ser gravado
        // (a chave fica so em memoria e sera pedida de novo depois).
        let _ = prompt.reply.send(HostKeyAnswer::Accept);
        if let Err(e) = saved {
            self.hosts_error = Some(format!(
                "A chave do servidor foi aceita, mas o cofre não pôde ser gravado: {e}"
            ));
            if let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, path)) {
                pane.upload = Some(UploadUi::notice(
                    "Cofre não gravado: a chave do servidor será pedida de novo.",
                ));
            }
        }
        self.pending_focus = Some(path.to_vec());
        // Outros paineis do mesmo host com a mesma chave seguem neste quadro.
        self.resolve_host_key_prompts();
    }

    /// Comeca o download pedido em `req` para a pasta local `dest` (ja
    /// escolhida). Confere se o painel ainda e o mesmo SFTP, na mesma pasta e
    /// sem outro download; conflitos com o que ja existe em `dest` abrem o
    /// dialogo de conflito antes de qualquer coisa ir para a sessao.
    fn begin_download(&mut self, req: PendingDownload, dest: PathBuf) {
        let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, &req.path)) else {
            return;
        };
        let same_dir = pane
            .explorer
            .as_ref()
            .is_some_and(|e| e.cur_path == req.remote_dir);
        if pane.sftp.is_none() || !same_dir || pane.download.as_ref().is_some_and(DownloadUi::busy) {
            return;
        }
        let id = self.next_download_id;
        self.next_download_id += 1;
        let prepared = download::prepare(&dest, &req.picks);
        if prepared.items.is_empty() {
            let text = match prepared.invalid.first() {
                Some((nome, motivo)) => format!("Nada a baixar: {}: {motivo}", show_path(nome)),
                None => "Nada a baixar.".to_string(),
            };
            pane.download = Some(DownloadUi {
                id,
                dest,
                pre_skipped: Vec::new(),
                stage: DownloadStage::done(&text, Tone::Error),
            });
            return;
        }
        let pre_skipped = prepared.invalid.clone();
        if !prepared.conflicts.is_empty() {
            pane.download = Some(DownloadUi {
                id,
                dest,
                pre_skipped,
                stage: DownloadStage::Asking(prepared),
            });
            return;
        }
        start_download(pane, id, dest, prepared.items, pre_skipped);
    }

    /// Ctrl+V (ou o botao da faixa) no painel `path`: confere se da para
    /// colar, planeja os conflitos com a listagem da pasta atual e manda o
    /// lote (ou abre o dialogo de conflito). Um recorte sai do app ja aqui e
    /// volta se nada sair da origem.
    fn begin_paste(&mut self, path: &[usize]) {
        let Some(Node::Leaf(pane)) = self.root.as_mut().and_then(|r| node_at_mut(r, path)) else {
            return;
        };
        let notice = |text: String, tone| Some(PasteUi::notice(&text, tone));
        match paste_avail(self.fs_clip.as_ref(), pane) {
            PasteAvail::Busy | PasteAvail::Offline => return,
            PasteAvail::Empty => {
                pane.paste = notice(
                    "Nada para colar: selecione arquivos ou pastas e tecle Ctrl+C (copiar) \
                     ou Ctrl+X (recortar)."
                        .into(),
                    Tone::Neutral,
                );
                return;
            }
            PasteAvail::OtherServer => {
                let label = self.fs_clip.as_ref().map(|c| c.origin.label.clone()).unwrap_or_default();
                pane.paste = notice(
                    format!(
                        "Os itens foram copiados em \u{201c}{}\u{201d}; colar só funciona num \
                         painel SFTP dessa mesma conexão. Para levar arquivos a outro servidor, \
                         baixe-os (Ctrl+S) e arraste-os para o outro painel.",
                        show_path(&label)
                    ),
                    Tone::Neutral,
                );
                return;
            }
            PasteAvail::SameFolder => {
                pane.paste = notice("Os itens já estão nesta pasta; nada foi movido.".into(), Tone::Neutral);
                return;
            }
            PasteAvail::IntoItself => {
                let text = match self.fs_clip.as_ref().map(|c| c.items.as_slice()) {
                    Some([one]) => format!(
                        "Não é possível colar \u{201c}{}\u{201d} dentro dela mesma.",
                        show_path(&one.name)
                    ),
                    _ => "Não é possível colar uma pasta dentro dela mesma.".to_string(),
                };
                pane.paste = notice(text, Tone::Error);
                return;
            }
            PasteAvail::Ready => {}
        }
        let (Some(clip), Some(exp)) = (self.fs_clip.as_ref(), pane.explorer.as_ref()) else {
            return;
        };
        let mode = clip.mode;
        let op = match mode {
            ClipMode::Copy => paste::PasteOp::Copy,
            ClipMode::Cut => paste::PasteOp::Move,
        };
        let dest: std::collections::HashMap<&str, bool> =
            exp.entries.iter().map(|n| (n.name.as_str(), n.is_real_dir())).collect();
        let sources: Vec<paste::Source> = clip
            .items
            .iter()
            .map(|i| paste::Source {
                path: &i.path,
                name: &i.name,
                is_dir: i.is_dir,
            })
            .collect();
        let prepared = paste::prepare(op, &clip.src_dir, &exp.cur_path, &sources, &dest);
        let dest_dir = exp.cur_path.clone();
        if prepared.items.is_empty() {
            let text = match prepared.invalid.first() {
                Some((nome, motivo)) => format!("Nada a colar: {}: {motivo}", show_path(nome)),
                None => "Nada a colar.".to_string(),
            };
            pane.paste = notice(text, Tone::Error);
            return;
        }
        let id = self.next_paste_id;
        self.next_paste_id += 1;
        // O que estava copiado/recortado sai do app ao colar (a faixa some) e
        // so volta se nada for colado (cancelado, erro, tudo pulado).
        let restore = self.fs_clip.take();
        let mut pui = PasteUi {
            id,
            op,
            dest_dir,
            origin: pane.origin.clone(),
            restore,
            pre_failed: Vec::new(),
            pre_skipped: prepared.invalid.clone(),
            pre_moved: Vec::new(),
            pre_done: 0,
            pre_count: 0,
            stage: PasteStage::done(String::new(), String::new(), Tone::Neutral),
        };
        if !prepared.conflicts.is_empty() {
            pui.stage = PasteStage::Asking(prepared);
            pane.paste = Some(pui);
            return;
        }
        if let Some(c) = start_paste(pane, pui, op, prepared.items) {
            self.fs_clip.get_or_insert(c);
        }
    }

    /// Depois de um colar: nos paineis da mesma conexao, segue uma pasta
    /// movida, re-lista o destino e as origens e devolve um recorte que nao
    /// saiu da origem (so se nada novo foi copiado nesse meio tempo).
    fn apply_paste_after(&mut self, afters: Vec<PasteAfter>) {
        for after in afters {
            if let (Some(root), Some(origin)) = (&mut self.root, &after.origin) {
                for_each_pane_mut(root, &mut |pane| {
                    if !pane.origin.as_ref().is_some_and(|o| o.same(origin)) {
                        return;
                    }
                    let (Some(exp), Some(sftp)) = (&mut pane.explorer, &pane.sftp) else {
                        return;
                    };
                    let mut to_list = Vec::new();
                    let moved_here = after
                        .moved
                        .iter()
                        .find(|(from, _)| paste::is_inside(&exp.cur_path, from));
                    if let Some((from, to)) = moved_here {
                        // A pasta aberta (ou uma de cima) foi movida: segue.
                        let cut = from.trim_end_matches('/').len().min(exp.cur_path.len());
                        let rest = exp.cur_path[cut..].to_string();
                        exp.navigate_to(format!("{to}{rest}"), &mut to_list);
                    } else if after.refresh.iter().any(|d| paste::same_dir(d, &exp.cur_path)) {
                        exp.refresh(&mut to_list);
                    }
                    for p in to_list {
                        sftp.list_dir(p);
                    }
                });
            }
            if let Some(c) = after.restore {
                if self.fs_clip.is_none() {
                    self.fs_clip = Some(c);
                }
            }
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
        let modal_open = self.editor.is_some()
            || self.pending_delete.is_some()
            || self.show_help
            || self.host_key_pending();
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
                self.fs_clip.as_ref(),
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
                PaneAction::Download {
                    path,
                    remote_dir,
                    picks,
                } => {
                    // Um por vez por painel; a pasta e pedida no fim do quadro.
                    let busy = match self.root.as_ref().and_then(|r| node_at(r, &path)) {
                        Some(Node::Leaf(p)) => p.download.as_ref().is_some_and(DownloadUi::busy),
                        _ => true,
                    };
                    if !busy && self.pending_download.is_none() {
                        self.pending_download = Some(PendingDownload {
                            path,
                            remote_dir,
                            picks,
                        });
                    }
                }
                PaneAction::ClipSet(c) => self.fs_clip = Some(c),
                PaneAction::ClipClear => self.fs_clip = None,
                PaneAction::ClipRestore(c) => {
                    if self.fs_clip.is_none() {
                        self.fs_clip = Some(c);
                    }
                }
                PaneAction::Paste { path } => self.begin_paste(&path),
            }
        }
        // Sem painel SFTP da conexao, os itens copiados nao tem onde colar.
        if let Some(c) = &self.fs_clip {
            let alive = self.root.as_ref().is_some_and(|r| {
                any_pane(r, &|p| {
                    p.sftp.is_some() && p.origin.as_ref().is_some_and(|o| o.same(&c.origin))
                })
            });
            if !alive {
                self.fs_clip = None;
            }
        }

        // Editor de host pode ser aberto a partir do seletor de um painel; e
        // renderizado sobre a sessao como janela flutuante.
        if self.editor.is_some() {
            self.ui_host_editor(ui.ctx());
        }
    }
}

/// Algum painel da arvore satisfaz `f`?
fn any_pane(node: &Node, f: &dyn Fn(&Pane) -> bool) -> bool {
    match node {
        Node::Leaf(p) => f(p),
        Node::Split { children, .. } => children.iter().any(|c| any_pane(c, f)),
    }
}

/// Manda o lote de colar a sessao e passa o painel a "colando". Com a sessao
/// ja encerrada fica so o aviso, e o recorte volta para quem chamou.
fn start_paste(
    pane: &mut Pane,
    mut pui: PasteUi,
    op: paste::PasteOp,
    items: Vec<paste::PasteItem>,
) -> Option<FsClip> {
    let req = paste::PasteRequest {
        op,
        dest_dir: pui.dest_dir.clone(),
        items,
    };
    match pane.sftp.as_ref().and_then(|s| s.paste(pui.id, req)) {
        Some(cancel) => {
            pui.stage = PasteStage::Running {
                cancel,
                cancelling: false,
                scanning: op != paste::PasteOp::Move,
                found: 0,
                phase: if op == paste::PasteOp::Move {
                    paste::PastePhase::Moving
                } else {
                    paste::PastePhase::Copying
                },
                index: 0,
                count: 0,
                name: String::new(),
                done: 0,
                total: 0,
            };
            pane.paste = Some(pui);
            None
        }
        None => {
            let restore = pui.restore.take();
            pane.paste = Some(PasteUi::notice("Sessão encerrada; nada foi colado.", Tone::Error));
            restore
        }
    }
}

/// Resposta do dialogo de conflito do colar. Devolve o recorte a restaurar
/// quando nada vai ser movido.
fn answer_paste_conflict(pane: &mut Pane, choice: ConflictChoice) -> Option<FsClip> {
    let mut pui = pane.paste.take()?;
    let stage = std::mem::replace(
        &mut pui.stage,
        PasteStage::done(String::new(), String::new(), Tone::Neutral),
    );
    let PasteStage::Asking(prepared) = stage else {
        pui.stage = stage;
        pane.paste = Some(pui);
        return None;
    };
    if choice == ConflictChoice::Cancel {
        return pui.restore.take();
    }
    let op = prepared.op;
    let (items, skipped) = paste::resolve(prepared, choice);
    pui.pre_skipped.extend(skipped);
    if items.is_empty() {
        let restore = pui.restore.take();
        pane.paste = Some(PasteUi::notice(
            "Nada a colar: todos os itens já existem no destino.",
            Tone::Neutral,
        ));
        return restore;
    }
    start_paste(pane, pui, op, items)
}

/// Resposta da oferta de copiar e apagar (mover entre discos do servidor).
fn answer_paste_offer(pane: &mut Pane, accept: bool) -> Option<FsClip> {
    let mut pui = pane.paste.take()?;
    let stage = std::mem::replace(
        &mut pui.stage,
        PasteStage::done(String::new(), String::new(), Tone::Neutral),
    );
    let PasteStage::Offer(report) = stage else {
        pui.stage = stage;
        pane.paste = Some(pui);
        return None;
    };
    if accept {
        // O 1o lote ja terminou: soma nos `pre_*` e manda so os que ficaram.
        pui.pre_failed.extend(report.failed.iter().cloned());
        pui.pre_skipped.extend(report.skipped.iter().cloned());
        pui.pre_moved.extend(report.moved.iter().cloned());
        pui.pre_done += report.done;
        pui.pre_count += report.count.saturating_sub(report.cross_device.len());
        let items = report.cross_device.clone();
        return start_paste(pane, pui, paste::PasteOp::CopyThenDelete, items);
    }
    let left = report.cross_device.len();
    let (text, detail, tone) = if report.done + pui.pre_done == 0 && report.failed.is_empty() {
        (
            format!("Nada foi movido: {left} item(ns) em outro disco do servidor."),
            String::new(),
            Tone::Neutral,
        )
    } else {
        let (t, d, tone) = paste_result(&pui, &report);
        let tone = if tone == Tone::Ok { Tone::Neutral } else { tone };
        (format!("{t} \u{00b7} {left} não movido(s): discos diferentes"), d, tone)
    };
    let restore = if nothing_pasted(&pui, &report) { pui.restore.take() } else { None };
    pui.stage = PasteStage::done(text, detail, tone);
    pane.paste = Some(pui);
    restore
}

/// Aplica um evento do colar ao painel (ignora lotes antigos e eventos fora
/// do andamento). No fim devolve o que fazer nos paineis da conexao.
fn apply_paste_event(pane: &mut Pane, ev: paste::PasteEvent) -> Option<PasteAfter> {
    let p = pane.paste.as_mut()?;
    let id = match &ev {
        paste::PasteEvent::Scanning { id, .. } | paste::PasteEvent::Progress { id, .. } => *id,
        paste::PasteEvent::Finished(r) => r.id,
    };
    if p.id != id || !matches!(p.stage, PasteStage::Running { .. }) {
        return None;
    }
    match ev {
        paste::PasteEvent::Scanning { found: n, .. } => {
            if let PasteStage::Running { scanning, found, .. } = &mut p.stage {
                *scanning = true;
                *found = n;
            }
            None
        }
        paste::PasteEvent::Progress {
            phase: ph,
            index: i,
            count: c,
            name: nm,
            done: dn,
            total: t,
            ..
        } => {
            if let PasteStage::Running {
                scanning,
                phase,
                index,
                count,
                name,
                done,
                total,
                ..
            } = &mut p.stage
            {
                *scanning = false;
                *phase = ph;
                *index = i;
                *count = c;
                *name = nm;
                *done = dn;
                *total = t;
            }
            None
        }
        paste::PasteEvent::Finished(report) => {
            let after = PasteAfter {
                origin: p.origin.clone(),
                refresh: report.refresh.clone(),
                moved: report.moved.clone(),
                restore: None,
            };
            // Mover com itens em outro disco: pergunta antes de copiar e apagar.
            if report.op == Some(paste::PasteOp::Move)
                && !report.cross_device.is_empty()
                && !report.cancelled
                && report.fatal.is_none()
            {
                p.stage = PasteStage::Offer(report);
                return Some(after);
            }
            let (text, detail, tone) = paste_result(p, &report);
            let restore = if nothing_pasted(p, &report) { p.restore.take() } else { None };
            p.stage = PasteStage::done(text, detail, tone);
            Some(PasteAfter { restore, ..after })
        }
    }
}

/// Nada foi colado: num mover, nada saiu da origem; numa copia, nada foi
/// colocado no destino. So entao o que estava copiado/recortado volta.
fn nothing_pasted(p: &PasteUi, r: &paste::PasteReport) -> bool {
    if p.op == paste::PasteOp::Copy {
        p.pre_done + r.done == 0
    } else {
        p.pre_moved.is_empty() && r.moved.is_empty()
    }
}

/// Resumo de um colar terminado: texto do rodape, detalhe (dica) e tom.
fn paste_result(p: &PasteUi, r: &paste::PasteReport) -> (String, String, Tone) {
    let moving = p.op != paste::PasteOp::Copy;
    let dest = elide_path(&show_path(&p.dest_dir), 48);
    let failed: Vec<&(String, String)> = p.pre_failed.iter().chain(&r.failed).collect();
    let skipped: Vec<&(String, String)> = p.pre_skipped.iter().chain(&r.skipped).collect();
    let done = p.pre_done + r.done;
    let n = p.pre_count + r.count;
    let suffix = if skipped.is_empty() {
        String::new()
    } else {
        format!(" \u{00b7} {} ignorado(s)", skipped.len())
    };
    let (text, tone) = if r.cancelled {
        let t = match (moving, done, r.saved) {
            (true, 0, _) => "Movimentação cancelada; nada foi movido.".to_string(),
            (true, d, _) => format!("Movimentação cancelada; {d} de {n} itens já tinham sido movidos."),
            (false, _, 0) => "Cópia cancelada; nada foi copiado.".to_string(),
            (false, _, s) => format!("Cópia cancelada; {s} de {} arquivos já estavam copiados.", r.files),
        };
        (t, Tone::Neutral)
    } else if let Some(motivo) = &r.fatal {
        let t = if moving {
            format!("Movimentação interrompida ({motivo}); {done} de {n} itens movidos")
        } else {
            format!("Cópia interrompida ({motivo}); {} de {} arquivos copiados", r.saved, r.files)
        };
        (t, Tone::Error)
    } else if let Some((nome, erro)) = failed.first() {
        let (nome, erro) = (show_path(nome), show_path(erro));
        let verbo = if moving { "movidos" } else { "copiados" };
        let mut t = if done == 0 {
            format!("Nada foi colado: {nome}: {erro}")
        } else {
            format!("{done} de {n} itens {verbo} para {dest}; {nome}: {erro}")
        };
        if failed.len() > 1 {
            t.push_str(&format!(" (+{} com erro)", failed.len() - 1));
        }
        (t, Tone::Error)
    } else if let Some((nome, motivo)) = r.src_kept.first() {
        let mut t = format!(
            "Copiado para {dest}, mas o original de \u{201c}{}\u{201d} ficou: {motivo}",
            show_path(nome)
        );
        if r.src_kept.len() > 1 {
            t.push_str(&format!(" (+{})", r.src_kept.len() - 1));
        }
        (t, Tone::Error)
    } else if done == 0 {
        match skipped.first() {
            Some((nome, motivo)) => (
                format!("Nada foi colado: {}: {motivo}", show_path(nome)),
                Tone::Neutral,
            ),
            None => ("Nada foi colado.".to_string(), Tone::Error),
        }
    } else {
        let t = if moving {
            if done == 1 {
                format!("\u{201c}{}\u{201d} movido para {dest}", show_path(&r.last))
            } else {
                format!("{done} itens movidos para {dest}")
            }
        } else if !r.renamed.is_empty() && r.renamed.len() == done {
            // Tudo virou copia na propria pasta ("(cópia)").
            if done == 1 {
                format!("Cópia criada: \u{201c}{}\u{201d}", show_path(&r.renamed[0].1))
            } else {
                format!("{done} cópias criadas nesta pasta")
            }
        } else if done == 1 {
            format!("\u{201c}{}\u{201d} copiado para {dest}", show_path(&r.last))
        } else {
            format!("{done} itens copiados para {dest}")
        };
        (t + &suffix, Tone::Ok)
    };
    let mut detail = String::new();
    let line = |(a, b): &(String, String)| format!("\u{2022} {}: {}", show_path(a), show_path(b));
    detail_section(&mut detail, "Com erro:", failed.iter().map(|&x| line(x)));
    detail_section(&mut detail, "Ignorados:", skipped.iter().map(|&x| line(x)));
    detail_section(
        &mut detail,
        "Cópias com outro nome:",
        r.renamed
            .iter()
            .map(|(a, b)| format!("\u{2022} {} \u{2192} {}", show_path(a), show_path(b))),
    );
    detail_section(&mut detail, "Originais mantidos:", r.src_kept.iter().map(line));
    (text, detail, tone)
}

/// Rodape do colar no painel SFTP (mesmo visual do download): andamento com
/// "Cancelar" ou o resultado com "x" para dispensar.
fn paste_footer(ui: &mut egui::Ui, rect: egui::Rect, paste: &mut Option<PasteUi>) {
    let Some(p) = paste.as_mut() else {
        return;
    };
    ui.painter().rect(
        rect,
        6.0,
        CARD_BG,
        egui::Stroke::new(1.0, CARD_BORDER),
        egui::StrokeKind::Inside,
    );
    let mut dismiss = false;
    let inner = rect.shrink2(egui::vec2(8.0, 2.0));
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    let text = p.running_text();
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(layout), |ui| {
        match &mut p.stage {
            PasteStage::Running {
                cancel,
                cancelling,
                scanning,
                index,
                count,
                done,
                total,
                ..
            } => {
                let frac = if *scanning {
                    0.0
                } else {
                    download_fraction(*index, *count, *done, *total)
                };
                let track = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + 6.0, rect.top() + 1.0),
                    egui::pos2(rect.right() - 6.0, rect.top() + 4.0),
                );
                let painter = ui.painter();
                painter.rect_filled(track, 1.0, HIGHLIGHT.gamma_multiply(0.25));
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        track.min,
                        egui::vec2(track.width() * frac, track.height()),
                    ),
                    1.0,
                    HIGHLIGHT,
                );
                if !*cancelling
                    && painted_btn(ui, egui::vec2(84.0, 22.0), "Cancelar", 13.0, &BTN_GHOST)
                {
                    cancel.cancel();
                    *cancelling = true;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).color(HIGHLIGHT)).truncate());
                });
            }
            PasteStage::Done {
                text, detail, tone, ..
            } => {
                let x = egui::ImageButton::new(
                    egui::Image::new(ICON_CLOSE)
                        .fit_to_exact_size(egui::vec2(14.0, 14.0))
                        .tint(TEXT_WEAK),
                )
                .frame(false);
                if ui.add(x).on_hover_text("Dispensar").clicked() {
                    dismiss = true;
                }
                let color = match tone {
                    Tone::Ok => AUTH_KEY,
                    Tone::Neutral => TEXT_WEAK,
                    Tone::Error => ERROR_FG,
                };
                let hover = if detail.is_empty() {
                    text.clone()
                } else {
                    format!("{text}\n\n{detail}")
                };
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text.as_str()).color(color))
                            .truncate(),
                    )
                    .on_hover_text(hover);
                });
            }
            PasteStage::Asking(_) | PasteStage::Offer(_) => {}
        }
    });
    if dismiss {
        *paste = None;
    }
}

/// Janela do colar (conflito ou oferta), no mesmo estilo das demais.
fn paste_window(
    ctx: &egui::Context,
    id: (&'static str, Vec<usize>),
    title: &str,
    add: impl FnOnce(&mut egui::Ui),
) {
    let frame = egui::Frame::window(&ctx.style())
        .fill(CARD_BG)
        .stroke(egui::Stroke::new(1.0, CARD_BORDER))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(18));
    egui::Window::new(egui::RichText::new(title).color(ACCENT).strong())
        .id(egui::Id::new(id))
        .collapsible(false)
        .resizable(false)
        .movable(true)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_min_width(340.0);
            ui.set_max_width(460.0);
            add(ui);
        });
}

/// Dialogo "Já existe no destino" do colar. Cancelar (ou Esc, com o painel
/// em foco) desiste; Enter nao faz nada.
fn paste_conflict_dialog(
    ctx: &egui::Context,
    path: &[usize],
    p: &paste::Prepared,
    esc: bool,
) -> Option<ConflictChoice> {
    if esc && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        return Some(ConflictChoice::Cancel);
    }
    let pasta = elide_path(&show_path(&p.dest_dir), 48);
    let nomes: Vec<&str> = p
        .conflicts
        .iter()
        .filter_map(|&i| p.items.get(i))
        .map(|it| it.name.as_str())
        .collect();
    let moving = p.op != paste::PasteOp::Copy;
    let mut choice = None;
    paste_window(ctx, ("paste_conflict", path.to_vec()), "Já existe no destino", |ui| {
        if let [nome] = nomes.as_slice() {
            ui.label(
                egui::RichText::new(format!(
                    "\u{201c}{}\u{201d} já existe em {pasta}.",
                    show_path(nome)
                ))
                .color(CARD_TEXT),
            );
        } else {
            ui.label(
                egui::RichText::new(format!("{} itens já existem em {pasta}:", nomes.len()))
                    .color(CARD_TEXT),
            );
            for n in nomes.iter().take(5) {
                ui.label(egui::RichText::new(show_path(n)).color(HIGHLIGHT));
            }
            if nomes.len() > 5 {
                ui.label(egui::RichText::new(format!("e mais {}.", nomes.len() - 5)).color(TEXT_WEAK));
            }
        }
        ui.add_space(6.0);
        let nota = if moving {
            "Substituir troca os arquivos de mesmo nome pelos que você está movendo. Uma pasta \
             que já existe recebe o conteúdo; nada é apagado nela."
        } else {
            "Substituir troca os arquivos de mesmo nome pela cópia. Uma pasta que já existe \
             recebe o conteúdo copiado; nada é apagado nela."
        };
        ui.label(egui::RichText::new(nota).small().color(TEXT_WEAK));
        if p.mismatched > 0 {
            ui.label(
                egui::RichText::new("Quando um é pasta e o outro é arquivo, o item é sempre pulado.")
                    .small()
                    .color(TEXT_WEAK),
            );
        }
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if fit_btn(ui, "Substituir", &BTN_DANGER, "") {
                choice = Some(ConflictChoice::Replace);
            }
            if p.items.len() > p.conflicts.len() && fit_btn(ui, "Pular existentes", &BTN_GHOST, "") {
                choice = Some(ConflictChoice::Skip);
            }
            if fit_btn(ui, "Cancelar", &BTN_GHOST, "") {
                choice = Some(ConflictChoice::Cancel);
            }
        });
    });
    choice
}

/// Oferta de copiar e apagar os originais, quando o servidor nao move direto
/// (discos ou particoes diferentes). Esc = Cancelar.
fn paste_offer_dialog(
    ctx: &egui::Context,
    path: &[usize],
    r: &paste::PasteReport,
    dest_dir: &str,
    esc: bool,
) -> Option<bool> {
    if esc && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        return Some(false);
    }
    let dest = elide_path(&show_path(dest_dir), 48);
    let texto = match r.cross_device.as_slice() {
        [one] => format!(
            "Não foi possível mover \u{201c}{}\u{201d} direto no servidor: a origem e o destino \
             provavelmente ficam em discos ou partições diferentes. Copiar para {dest} e apagar o \
             original depois?",
            show_path(&one.name)
        ),
        v => format!(
            "Não foi possível mover {} itens direto no servidor: a origem e o destino \
             provavelmente ficam em discos ou partições diferentes. Copiar para {dest} e apagar os \
             originais depois?",
            v.len()
        ),
    };
    let mut answer = None;
    paste_window(
        ctx,
        ("paste_offer", path.to_vec()),
        "Mover para outro disco do servidor",
        |ui| {
            ui.label(egui::RichText::new(texto).color(CARD_TEXT));
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Cada original só é apagado depois que a cópia dele termina sem nenhum erro. \
                     Pode demorar: os dados passam por este computador.",
                )
                .small()
                .color(TEXT_WEAK),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if fit_btn(ui, "Copiar e apagar os originais", &BTN_DANGER, "") {
                    answer = Some(true);
                }
                if fit_btn(ui, "Cancelar", &BTN_GHOST, "") {
                    answer = Some(false);
                }
            });
        },
    );
    answer
}

/// Cliques na faixa de copiar/mover.
#[derive(Default)]
struct BannerOut {
    paste: bool,
    clear: bool,
}

/// Faixa no topo do painel SFTP com o que esta copiado/recortado (em todo
/// painel da mesma conexao): o que, de onde, como colar e o "x" que desiste.
fn clip_banner(ui: &mut egui::Ui, c: &FsClip, avail: PasteAvail, wide: bool) -> BannerOut {
    let mut out = BannerOut::default();
    let cut = c.mode == ClipMode::Cut;
    let (fill, edge, icon, tint, fg) = if cut {
        (
            HIGHLIGHT.gamma_multiply(0.12),
            HIGHLIGHT.gamma_multiply(0.6),
            ICON_CUT,
            HIGHLIGHT,
            HIGHLIGHT,
        )
    } else {
        (
            ACCENT.gamma_multiply(0.14),
            ACCENT.gamma_multiply(0.6),
            ICON_COPY,
            ACCENT,
            CARD_TEXT,
        )
    };
    let verbo = if cut { "Movendo" } else { "Copiando" };
    let pasta = elide_path(&show_path(&c.src_dir), 48);
    let line1 = match c.items.as_slice() {
        [one] => format!(
            "{verbo} \u{201c}{}\u{201d} de {pasta}",
            elide(&show_path(&one.name), 40)
        ),
        items => format!("{verbo} {} itens de {pasta}", items.len()),
    };
    let mut tip = format!("{verbo} de {}:", show_path(&c.src_dir));
    for i in c.items.iter().take(10) {
        let barra = if i.is_dir { "/" } else { "" };
        tip.push_str(&format!("\n\u{2022} {}{barra}", show_path(&i.name)));
    }
    if c.items.len() > 10 {
        tip.push_str(&format!("\n\u{2026} e mais {}", c.items.len() - 10));
    }
    tip.push_str(if cut {
        "\n\nOs itens só saem da origem quando você colar no destino."
    } else {
        "\n\nNa mesma pasta, Ctrl+V cria uma cópia com \u{201c}(cópia)\u{201d} no nome. \
         Depois de colar, a faixa some (para colar de novo, copie outra vez)."
    });
    ui.add_space(4.0);
    egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, edge))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.add(
                    egui::Image::new(icon)
                        .fit_to_exact_size(egui::vec2(16.0, 16.0))
                        .tint(tint),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let x = egui::ImageButton::new(
                        egui::Image::new(ICON_CLOSE)
                            .fit_to_exact_size(egui::vec2(14.0, 14.0))
                            .tint(TEXT_WEAK),
                    )
                    .frame(false);
                    if ui
                        .add(x)
                        .on_hover_text("Cancelar (Esc): nada é copiado nem movido")
                        .clicked()
                    {
                        out.clear = true;
                    }
                    if wide && matches!(avail, PasteAvail::Ready | PasteAvail::Busy) {
                        let label = if cut { "Mover para cá" } else { "Colar aqui" };
                        let ready = avail == PasteAvail::Ready;
                        let r = ui.add_enabled_ui(ready, |ui| {
                            painted_btn(ui, egui::vec2(104.0, 22.0), label, 13.0, &BTN_ACCENT)
                        });
                        if !ready {
                            r.response.on_hover_text("Aguarde a colagem atual terminar");
                        } else if r.inner {
                            out.paste = true;
                        }
                    }
                    ui.with_layout(egui::Layout::top_down(egui::Align::Min), |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&line1).size(13.0).strong().color(fg),
                            )
                            .truncate()
                            .selectable(false),
                        )
                        .on_hover_text(tip);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    "Vá até a pasta de destino e tecle Ctrl+V  \u{00b7}  Esc cancela",
                                )
                                .size(11.5)
                                .color(TEXT_WEAK),
                            )
                            .truncate()
                            .selectable(false),
                        );
                    });
                });
            });
        });
    out
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

/// Envia o pedido de download a sessao e passa o painel a "baixando"; com a
/// sessao ja encerrada, fica so o aviso (nada foi pedido).
fn start_download(
    pane: &mut Pane,
    id: u64,
    dest: PathBuf,
    items: Vec<download::DownloadItem>,
    pre_skipped: Vec<(String, String)>,
) {
    let cancel = pane
        .sftp
        .as_ref()
        .and_then(|s| s.download(id, dest.clone(), items));
    let stage = match cancel {
        Some(cancel) => DownloadStage::Running {
            cancel,
            cancelling: false,
            scanning: true,
            found: 0,
            index: 0,
            count: 0,
            name: String::new(),
            done: 0,
            total: 0,
        },
        None => DownloadStage::done("Sessão encerrada; nada foi baixado.", Tone::Error),
    };
    pane.download = Some(DownloadUi {
        id,
        dest,
        pre_skipped,
        stage,
    });
}

/// Aplica a escolha do dialogo de conflito: comeca o download (Substituir /
/// Pular existentes) ou descarta o pedido (Cancelar).
fn answer_conflict(pane: &mut Pane, choice: ConflictChoice) {
    let Some(DownloadUi {
        id,
        dest,
        mut pre_skipped,
        stage: DownloadStage::Asking(prepared),
    }) = pane.download.take()
    else {
        return;
    };
    if choice == ConflictChoice::Cancel {
        return;
    }
    let (items, skipped) = download::resolve(prepared, choice);
    pre_skipped.extend(skipped);
    if items.is_empty() {
        pane.download = Some(DownloadUi {
            id,
            dest,
            pre_skipped,
            stage: DownloadStage::done(
                "Nada a baixar: todos os itens já existem no destino.",
                Tone::Neutral,
            ),
        });
        return;
    }
    start_download(pane, id, dest, items, pre_skipped);
}

/// Aplica um evento de download ao painel (ignora lotes antigos e eventos
/// que chegam depois de o painel ja ter um resultado).
fn apply_download_event(pane: &mut Pane, ev: DownloadEvent) {
    let Some(d) = &mut pane.download else {
        return;
    };
    let id = match &ev {
        DownloadEvent::Scanning { id, .. } | DownloadEvent::Progress { id, .. } => *id,
        DownloadEvent::Finished(r) => r.id,
    };
    if d.id != id || !matches!(d.stage, DownloadStage::Running { .. }) {
        return;
    }
    match ev {
        DownloadEvent::Scanning { found: n, .. } => {
            if let DownloadStage::Running { scanning, found, .. } = &mut d.stage {
                *scanning = true;
                *found = n;
            }
        }
        DownloadEvent::Progress {
            index: i,
            count: c,
            name: nm,
            done: dn,
            total: t,
            ..
        } => {
            if let DownloadStage::Running {
                scanning,
                index,
                count,
                name,
                done,
                total,
                ..
            } = &mut d.stage
            {
                *scanning = false;
                *index = i;
                *count = c;
                *name = nm;
                *done = dn;
                *total = t;
            }
        }
        DownloadEvent::Finished(report) => {
            let (text, detail, tone) = download_summary(&report, &d.pre_skipped);
            d.stage = DownloadStage::Done {
                text,
                detail,
                tone,
                at: Instant::now(),
            };
        }
    }
}

/// Acrescenta a `out` uma secao do detalhe (titulo e ate 10 linhas).
fn detail_section(out: &mut String, title: &str, lines: impl ExactSizeIterator<Item = String>) {
    let n = lines.len();
    if n == 0 {
        return;
    }
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(title);
    for l in lines.take(10) {
        out.push('\n');
        out.push_str(&l);
    }
    if n > 10 {
        out.push_str(&format!("\n\u{2026} e mais {}", n - 10));
    }
}

/// Resumo de um download concluido: texto do rodape, detalhe para a dica e
/// tom. `pre_skipped` sao os itens tirados antes de comecar (nome invalido,
/// "pular existentes"). Caminhos remotos passam por `show_path`.
fn download_summary(
    r: &download::DownloadReport,
    pre_skipped: &[(String, String)],
) -> (String, String, Tone) {
    let pasta = elide_path(&r.dest.display().to_string(), 48);
    let skipped: Vec<&(String, String)> = pre_skipped.iter().chain(&r.skipped).collect();
    let mut suffix = String::new();
    if !skipped.is_empty() {
        suffix.push_str(&format!(" \u{00b7} {} ignorado(s)", skipped.len()));
    }
    if !r.renamed.is_empty() {
        suffix.push_str(&format!(
            " \u{00b7} {} nome(s) ajustado(s) para o Windows",
            r.renamed.len()
        ));
    }
    let (text, tone) = if r.cancelled {
        if r.saved > 0 {
            (
                format!(
                    "Download cancelado; {} de {} arquivos já estavam salvos em {pasta}.",
                    r.saved, r.files
                ),
                Tone::Neutral,
            )
        } else {
            ("Download cancelado; nada foi salvo.".to_string(), Tone::Neutral)
        }
    } else if let Some(motivo) = &r.fatal {
        let text = if r.saved > 0 {
            format!(
                "Download interrompido ({motivo}); {} de {} arquivos salvos em {pasta}",
                r.saved, r.files
            )
        } else {
            format!("Download interrompido ({motivo}); nada foi salvo.")
        };
        (text, Tone::Error)
    } else if let Some((nome, erro)) = r.failed.first() {
        let (nome, erro) = (show_path(nome), show_path(erro));
        let mut text = if r.saved == 0 {
            format!("Nada foi baixado: {nome}: {erro}")
        } else {
            format!(
                "{} de {} arquivos baixados em {pasta}; {nome}: {erro}",
                r.saved, r.files
            )
        };
        if r.failed.len() > 1 {
            text.push_str(&format!(" (+{} com erro)", r.failed.len() - 1));
        }
        (text, Tone::Error)
    } else if r.saved == 0 && r.dirs == 0 {
        let text = match skipped.first() {
            Some((nome, motivo)) => format!("Nada foi baixado: {}: {motivo}", show_path(nome)),
            None => "Nada foi baixado.".to_string(),
        };
        (text, Tone::Error)
    } else {
        let text = match r.saved {
            0 if r.dirs == 1 => format!("Pasta baixada em {pasta} (sem arquivos)"),
            0 => format!("Pastas baixadas em {pasta} (sem arquivos)"),
            1 => format!("{} baixado em {pasta}", r.last_saved),
            n => format!("{n} arquivos baixados em {pasta}"),
        };
        (text + &suffix, Tone::Ok)
    };

    let mut detail = String::new();
    let line = |(p, m): &(String, String)| format!("\u{2022} {}: {}", show_path(p), show_path(m));
    detail_section(&mut detail, "Com erro:", r.failed.iter().map(line));
    detail_section(&mut detail, "Ignorados:", skipped.iter().map(|&x| line(x)));
    detail_section(
        &mut detail,
        "Nomes ajustados para o Windows:",
        r.renamed
            .iter()
            .map(|(a, b)| format!("\u{2022} {} \u{2192} {}", show_path(a), show_path(b))),
    );
    (text, detail, tone)
}

/// Pasta Downloads do usuario (inicio sugerido do dialogo), se existir.
fn default_download_dir() -> Option<PathBuf> {
    let d = PathBuf::from(std::env::var_os("USERPROFILE")?).join("Downloads");
    d.is_dir().then_some(d)
}

/// Abre a pasta no Explorer do Windows (caminho completo do explorer.exe:
/// nunca um executavel homonimo de outra pasta).
fn open_folder(dir: &std::path::Path) {
    let exe = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("explorer.exe"))
        .unwrap_or_else(|| PathBuf::from("explorer.exe"));
    let _ = std::process::Command::new(exe).arg(dir).spawn();
}

/// Rodape do download no painel SFTP: andamento (barra, texto e "Cancelar")
/// ou resultado (texto na cor do tom, "Abrir pasta" e "x" para dispensar).
fn download_footer(ui: &mut egui::Ui, rect: egui::Rect, download: &mut Option<DownloadUi>) {
    let Some(d) = download.as_mut() else {
        return;
    };
    ui.painter().rect(
        rect,
        6.0,
        CARD_BG,
        egui::Stroke::new(1.0, CARD_BORDER),
        egui::StrokeKind::Inside,
    );
    let mut dismiss = false;
    let inner = rect.shrink2(egui::vec2(8.0, 2.0));
    let layout = egui::Layout::right_to_left(egui::Align::Center);
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner).layout(layout), |ui| {
        let text = d.stage.running_text();
        match &mut d.stage {
            DownloadStage::Running {
                cancel,
                cancelling,
                scanning,
                index,
                count,
                done,
                total,
                ..
            } => {
                // Barra fina no topo do rodape.
                let frac = if *scanning {
                    0.0
                } else {
                    download_fraction(*index, *count, *done, *total)
                };
                let track = egui::Rect::from_min_max(
                    egui::pos2(rect.left() + 6.0, rect.top() + 1.0),
                    egui::pos2(rect.right() - 6.0, rect.top() + 4.0),
                );
                let painter = ui.painter();
                painter.rect_filled(track, 1.0, HIGHLIGHT.gamma_multiply(0.25));
                painter.rect_filled(
                    egui::Rect::from_min_size(
                        track.min,
                        egui::vec2(track.width() * frac, track.height()),
                    ),
                    1.0,
                    HIGHLIGHT,
                );
                if !*cancelling
                    && painted_btn(ui, egui::vec2(84.0, 22.0), "Cancelar", 13.0, &BTN_GHOST)
                {
                    cancel.cancel();
                    *cancelling = true;
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(egui::Label::new(egui::RichText::new(text).color(HIGHLIGHT)).truncate());
                });
            }
            DownloadStage::Done {
                text, detail, tone, ..
            } => {
                let x = egui::ImageButton::new(
                    egui::Image::new(ICON_CLOSE)
                        .fit_to_exact_size(egui::vec2(14.0, 14.0))
                        .tint(TEXT_WEAK),
                )
                .frame(false);
                if ui.add(x).on_hover_text("Dispensar").clicked() {
                    dismiss = true;
                }
                if painted_btn(ui, egui::vec2(96.0, 22.0), "Abrir pasta", 13.0, &BTN_GHOST) {
                    open_folder(&d.dest);
                }
                let color = match tone {
                    Tone::Ok => AUTH_KEY,
                    Tone::Neutral => TEXT_WEAK,
                    Tone::Error => ERROR_FG,
                };
                let hover = if detail.is_empty() {
                    text.clone()
                } else {
                    format!("{text}\n\n{detail}")
                };
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text.as_str()).color(color))
                            .truncate(),
                    )
                    .on_hover_text(hover);
                });
            }
            DownloadStage::Asking(_) => {}
        }
    });
    if dismiss {
        *download = None;
    }
}

/// Dialogo "Ja existe no destino" de um download. Cancelar (ou Esc, com o
/// painel em foco) descarta o pedido; Enter nao faz nada: nao ha acao padrao
/// destrutiva.
fn download_conflict_dialog(
    ctx: &egui::Context,
    path: &[usize],
    p: &download::Prepared,
    esc: bool,
) -> Option<ConflictChoice> {
    if esc && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
        return Some(ConflictChoice::Cancel);
    }
    let pasta = elide_path(&p.dest.display().to_string(), 48);
    let nomes: Vec<&str> = p
        .conflicts
        .iter()
        .filter_map(|&i| p.items.get(i))
        .map(|it| it.local.as_str())
        .collect();
    let frame = egui::Frame::window(&ctx.style())
        .fill(CARD_BG)
        .stroke(egui::Stroke::new(1.0, CARD_BORDER))
        .corner_radius(12.0)
        .inner_margin(egui::Margin::same(18));
    let mut choice = None;
    egui::Window::new(egui::RichText::new("Já existe no destino").color(ACCENT).strong())
        .id(egui::Id::new(("download_conflict", path.to_vec())))
        .collapsible(false)
        .resizable(false)
        .movable(true)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .frame(frame)
        .show(ctx, |ui| {
            ui.set_min_width(340.0);
            ui.set_max_width(460.0);
            if let [nome] = nomes.as_slice() {
                ui.label(
                    egui::RichText::new(format!(
                        "\u{201c}{}\u{201d} já existe em {pasta}.",
                        show_path(nome)
                    ))
                    .color(CARD_TEXT),
                );
            } else {
                ui.label(
                    egui::RichText::new(format!("{} itens já existem em {pasta}:", nomes.len()))
                        .color(CARD_TEXT),
                );
                for n in nomes.iter().take(5) {
                    ui.label(egui::RichText::new(show_path(n)).color(HIGHLIGHT));
                }
                if nomes.len() > 5 {
                    ui.label(
                        egui::RichText::new(format!("e mais {}.", nomes.len() - 5)).color(TEXT_WEAK),
                    );
                }
            }
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(
                    "Substituir troca os arquivos com o mesmo nome pela versão do servidor. \
                     Uma pasta que já existe recebe o conteúdo baixado; nada é apagado.",
                )
                .small()
                .color(TEXT_WEAK),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if fit_btn(ui, "Substituir", &BTN_DANGER, "") {
                    choice = Some(ConflictChoice::Replace);
                }
                if p.items.len() > p.conflicts.len()
                    && fit_btn(ui, "Pular existentes", &BTN_GHOST, "")
                {
                    choice = Some(ConflictChoice::Skip);
                }
                if fit_btn(ui, "Cancelar", &BTN_GHOST, "") {
                    choice = Some(ConflictChoice::Cancel);
                }
            });
        });
    choice
}

/// Fracao do andamento na base do titulo do painel, com envio ou download
/// em curso.
fn title_fraction(pane: &Pane) -> Option<f32> {
    if let Some(UploadUi {
        stage: UploadStage::Sending { sent, size, .. },
        ..
    }) = &pane.upload
    {
        return Some(if *size == 0 {
            0.0
        } else {
            (*sent as f32 / *size as f32).min(1.0)
        });
    }
    match &pane.download {
        Some(DownloadUi {
            stage:
                DownloadStage::Running {
                    scanning,
                    index,
                    count,
                    done,
                    total,
                    ..
                },
            ..
        }) => Some(if *scanning {
            0.0
        } else {
            download_fraction(*index, *count, *done, *total)
        }),
        _ => match &pane.paste {
            Some(PasteUi {
                stage:
                    PasteStage::Running {
                        scanning,
                        index,
                        count,
                        done,
                        total,
                        ..
                    },
                ..
            }) => Some(if *scanning {
                0.0
            } else {
                download_fraction(*index, *count, *done, *total)
            }),
            _ => None,
        },
    }
}

/// Linha fina de progresso (2 px) na base do retangulo `bar` (titulo).
fn title_progress(ui: &egui::Ui, bar: egui::Rect, frac: f32) {
    let track = egui::Rect::from_min_max(
        egui::pos2(bar.left(), bar.bottom() - 2.0),
        bar.right_bottom(),
    );
    let painter = ui.painter();
    painter.rect_filled(track, 0.0, HIGHLIGHT.gamma_multiply(0.25));
    painter.rect_filled(
        egui::Rect::from_min_size(track.min, egui::vec2(track.width() * frac, track.height())),
        0.0,
        HIGHLIGHT,
    );
}

/// Aplica um evento de envio ao estado do painel (ignora lotes antigos).
fn apply_upload_event(pane: &mut Pane, ev: UploadEvent) {
    let Some(u) = &mut pane.upload else {
        return;
    };
    let id = match &ev {
        UploadEvent::Plan(p) => p.id,
        UploadEvent::Progress { id, .. }
        | UploadEvent::Finished { id, .. }
        | UploadEvent::Failed { id, .. } => *id,
    };
    if u.id != id {
        return;
    }
    u.stage = match ev {
        UploadEvent::Plan(plan) => UploadStage::Asking(plan),
        UploadEvent::Progress {
            dir,
            index,
            count,
            name,
            sent,
            size,
            ..
        } => UploadStage::Sending {
            dir,
            index,
            count,
            name,
            sent,
            size,
        },
        UploadEvent::Finished {
            dir, sent, failed, ..
        } => UploadStage::Done {
            text: finished_text(&dir, &sent, &failed, u.skipped_dirs),
            ok: failed.is_empty(),
            at: Instant::now(),
        },
        UploadEvent::Failed { error, .. } => UploadStage::Done {
            text: format!("Falha no envio: {error}"),
            ok: false,
            at: Instant::now(),
        },
    };
}

/// Resumo de um lote concluido.
fn finished_text(dir: &str, sent: &[String], failed: &[(String, String)], skipped: usize) -> String {
    let dir = show_path(dir);
    let mut text = if failed.is_empty() {
        match sent.len() {
            1 => format!("{} enviado para {dir}", sent[0]),
            n => format!("{n} arquivos enviados para {dir}"),
        }
    } else {
        let (name, err) = &failed[0];
        let mut t = format!(
            "{} de {} enviados para {dir}; {name}: {err}",
            sent.len(),
            sent.len() + failed.len()
        );
        if failed.len() > 1 {
            t.push_str(&format!(" (+{} com erro)", failed.len() - 1));
        }
        t
    };
    if skipped > 0 {
        text.push_str(&format!(" \u{00b7} {skipped} pasta(s) ignorada(s)"));
    }
    text
}

/// Caminho remoto para exibir: troca caracteres de controle e de direcao de
/// texto (que poderiam disfarcar o destino) por '?'.
fn show_path(s: &str) -> String {
    s.chars()
        .map(|c| {
            let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
            if c.is_control() || bidi {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Posicao do cursor em pontos da janela, perguntada ao Windows. Durante um
/// arrasto vindo do Explorer o egui nao recebe movimento do mouse (a posicao
/// dele fica velha ou vazia), entao o painel alvo e achado por aqui.
#[cfg(windows)]
fn os_cursor_pos(ctx: &egui::Context) -> Option<egui::Pos2> {
    #[repr(C)]
    struct Point {
        x: i32,
        y: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetCursorPos(p: *mut Point) -> i32;
    }
    // Origem da area cliente em pontos (None em testes, sem janela real).
    let inner = ctx.input(|i| i.viewport().inner_rect)?;
    let mut p = Point { x: 0, y: 0 };
    // SAFETY: GetCursorPos apenas escreve no POINT fornecido.
    if unsafe { GetCursorPos(&mut p) } == 0 {
        return None;
    }
    let ppp = ctx.pixels_per_point();
    Some(egui::pos2(p.x as f32 / ppp, p.y as f32 / ppp) - inner.min.to_vec2())
}

#[cfg(not(windows))]
fn os_cursor_pos(_ctx: &egui::Context) -> Option<egui::Pos2> {
    None
}

/// Painel sob o cursor (retangulos do ultimo quadro). Sem posicao confiavel,
/// nenhum painel: nunca "chuta" o painel focado (poderia ir ao servidor errado).
fn drop_target(ctx: &egui::Context, rects: &[(Vec<usize>, egui::Rect)]) -> Option<Vec<usize>> {
    let pos = os_cursor_pos(ctx).or_else(|| ctx.input(|i| i.pointer.latest_pos()))?;
    rects
        .iter()
        .find(|(_, r)| r.contains(pos))
        .map(|(p, _)| p.clone())
}

/// Dica exibida sobre o painel enquanto arquivos sao arrastados por cima;
/// `true` quando o painel aceita o drop.
fn drop_hint(pane: &Pane) -> (String, bool) {
    if pane.picking {
        return ("Escolha uma conexão antes de soltar arquivos".into(), false);
    }
    if let Some(exp) = &pane.explorer {
        return if exp.cur_path.is_empty() {
            ("Aguarde a pasta carregar".into(), false)
        } else {
            (format!("Solte para enviar a {}", show_path(&exp.cur_path)), true)
        };
    }
    let Some(ssh) = &pane.ssh else {
        return (String::new(), false);
    };
    if !ssh.supports_upload() {
        ("Terminal local: não recebe arquivos".into(), false)
    } else if !matches!(pane.state, SessionState::Connected) {
        ("Aguarde a conexão para enviar arquivos".into(), false)
    } else if pane.upload.as_ref().is_some_and(UploadUi::busy) {
        ("Aguarde o envio atual terminar".into(), false)
    } else {
        (
            format!("Solte para enviar a {}\n(pasta atual do terminal)", pane.host_name),
            true,
        )
    }
}

/// Situacao do envio na barra de titulo do painel. Devolve `true` quando o
/// usuario clica para dispensar uma mensagem de erro.
fn upload_status(ui: &mut egui::Ui, upload: &Option<UploadUi>) -> bool {
    let Some(u) = upload else {
        return false;
    };
    let (text, color, full) = match &u.stage {
        UploadStage::Locating { since } => {
            // Evita piscar quando a descoberta e rapida.
            let left = std::time::Duration::from_millis(300).saturating_sub(since.elapsed());
            if !left.is_zero() {
                ui.ctx().request_repaint_after(left);
                return false;
            }
            ("\u{00b7} localizando pasta\u{2026}".to_string(), TEXT_WEAK, String::new())
        }
        UploadStage::Asking(_) => (
            "\u{00b7} escolha o destino do envio".to_string(),
            HIGHLIGHT,
            String::new(),
        ),
        UploadStage::Sending {
            dir,
            index,
            count,
            name,
            sent,
            size,
        } => {
            let pct = if *size == 0 { 100 } else { sent.saturating_mul(100) / size };
            let text = if name.is_empty() {
                // Escolha feita; o primeiro andamento ainda nao chegou.
                "\u{00b7} preparando o envio\u{2026}".to_string()
            } else if *count > 1 {
                format!(
                    "\u{00b7} enviando {}/{count}: {} \u{00b7} {pct}%",
                    index + 1,
                    elide(name, 24)
                )
            } else {
                format!("\u{00b7} enviando {} \u{00b7} {pct}%", elide(name, 28))
            };
            (text, HIGHLIGHT, format!("{name} \u{2192} {}", show_path(dir)))
        }
        UploadStage::Done { text, ok, .. } => {
            let color = if *ok { AUTH_KEY } else { ERROR_FG };
            let full = if *ok {
                text.clone()
            } else {
                format!("{text}\n(clique para dispensar)")
            };
            (format!("\u{00b7} {}", elide(text, 60)), color, full)
        }
    };
    // Deixa espaco para os botoes de dividir a direita: com o painel
    // estreito o texto e cortado ("...") em vez de ficar atras deles.
    let max_w = (ui.available_width() - 56.0).max(40.0);
    let resp = ui
        .scope(|ui| {
            ui.set_max_width(max_w);
            ui.add(
                egui::Label::new(egui::RichText::new(text).color(color))
                    .truncate()
                    .sense(egui::Sense::click()),
            )
        })
        .inner;
    let resp = if full.is_empty() { resp } else { resp.on_hover_text(full) };
    matches!(u.stage, UploadStage::Done { ok: false, .. }) && resp.clicked()
}

/// Escolha feita na barra de destino.
enum PlanChoice {
    Send { dir: String, replace: Vec<String> },
    Cancel,
}

/// Explica por que o destino nao foi usado direto.
fn plan_headline(plan: &upload::DropPlan) -> String {
    let p = &plan.probe;
    // O script troca espacos por '_' no nome do programa (ex.: "tmux: client").
    let comm = if p.fg_comm.is_empty() {
        "?".to_string()
    } else {
        p.fg_comm.replace('_', " ")
    };
    if upload::confident(p) {
        return "Já existe arquivo com o mesmo nome na pasta do terminal.".into();
    }
    if let (Some(dir), false) = (&p.dir, p.writable) {
        if p.method != "home" && p.method != "none" {
            return format!("Sem permissão de escrita em {}.", show_path(dir));
        }
    }
    match (p.method.as_str(), p.reason.as_str()) {
        ("tmux", _) => "O terminal está no tmux; a pasta abaixo é a do painel ativo dele.".into(),
        (_, "multiplexer") | (_, "tmux-failed") => format!(
            "O terminal está num multiplexador ({comm}); a pasta pode estar desatualizada."
        ),
        (_, "fg-unreadable") => format!(
            "O programa em uso no terminal ({comm}) roda como outro usuário; não vejo a pasta dele."
        ),
        ("fg", _) => format!("O terminal está executando '{comm}', numa pasta diferente da do shell."),
        (_, reason) => {
            let motivo = match reason {
                "no-shell" => "o shell deste terminal não foi encontrado no servidor",
                "cwd-unreadable" => "a pasta atual não pôde ser lida",
                "no-proc" => "o servidor não oferece /proc",
                "timeout" => "o servidor não respondeu a tempo",
                "exec-refused" => "o servidor não permite executar comandos",
                "no-marker" => "o shell do servidor não executou a verificação",
                other => other,
            };
            format!("Não consegui descobrir a pasta atual do terminal ({motivo}).")
        }
    }
}

/// Barra flutuante no rodape do painel com as opcoes de destino. So botoes:
/// nenhuma tecla e capturada (o que se digita continua indo ao terminal).
fn upload_plan_bar(
    ctx: &egui::Context,
    pane_rect: egui::Rect,
    path: &[usize],
    plan: &upload::DropPlan,
) -> Option<PlanChoice> {
    let p = &plan.probe;
    let mut choice = None;
    let width = (pane_rect.width() - 24.0).clamp(160.0, 640.0);
    egui::Area::new(egui::Id::new(("upload_plan", path.to_vec())))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(pane_rect.left_bottom() + egui::vec2(12.0, -12.0))
        .show(ctx, |ui| {
            egui::Frame::NONE
                .fill(CARD_BG)
                .stroke(egui::Stroke::new(1.0, HIGHLIGHT))
                .corner_radius(8.0)
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_max_width(width);
                    ui.label(egui::RichText::new(plan_headline(plan)).color(HIGHLIGHT).strong());
                    let nomes: Vec<String> = plan
                        .files
                        .iter()
                        .filter_map(|f| f.file_name().map(|n| n.to_string_lossy().into_owned()))
                        .collect();
                    ui.label(
                        egui::RichText::new(format!("Arquivos: {}", elide(&nomes.join(", "), 90)))
                            .small()
                            .color(TEXT_WEAK),
                    );
                    if !plan.conflicts.is_empty() {
                        let mais = plan.conflicts.len().saturating_sub(5);
                        let mut lista = plan.conflicts.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
                        if mais > 0 {
                            lista.push_str(&format!(" e mais {mais}"));
                        }
                        ui.label(
                            egui::RichText::new(format!("Já existe(m) no destino: {lista}"))
                                .small()
                                .color(ERROR_FG),
                        );
                    }
                    ui.add_space(8.0);
                    // Um destino por linha, com o caminho cortado no inicio (o
                    // final e o que distingue as pastas) e completo na dica.
                    ui.spacing_mut().item_spacing.y = 6.0;
                    let mut send = |dir: &String, replace: Vec<String>| {
                        choice = Some(PlanChoice::Send {
                            dir: dir.clone(),
                            replace,
                        })
                    };
                    let writable_dir = p.dir.as_ref().filter(|_| p.writable);
                    if let Some(dir) = writable_dir {
                        let label = format!("Enviar para {}", elide_path(&show_path(dir), 48));
                        if fit_btn(ui, &label, &BTN_ACCENT, &show_path(dir)) {
                            send(dir, Vec::new());
                        }
                    }
                    if let Some(shd) = p.shell_dir.as_ref().filter(|s| p.dir.as_ref() != Some(*s)) {
                        let label = format!("Pasta do shell: {}", elide_path(&show_path(shd), 44));
                        if fit_btn(ui, &label, &BTN_GHOST, &show_path(shd)) {
                            send(shd, Vec::new());
                        }
                    }
                    if !plan.home.is_empty() && p.dir.as_ref() != Some(&plan.home) {
                        let label =
                            format!("Pasta pessoal: {}", elide_path(&show_path(&plan.home), 44));
                        if fit_btn(ui, &label, &BTN_GHOST, &show_path(&plan.home)) {
                            send(&plan.home, Vec::new());
                        }
                    }
                    // Substituir fica separado dos demais (acao destrutiva).
                    if let (Some(dir), false) = (writable_dir, plan.conflicts.is_empty()) {
                        ui.add_space(4.0);
                        let hint = format!("Substitui em {}: {}", show_path(dir), plan.conflicts.join(", "));
                        if fit_btn(ui, "Substituir os existentes e enviar", &BTN_DANGER, &hint) {
                            send(dir, plan.conflicts.clone());
                        }
                    }
                    ui.add_space(2.0);
                    if fit_btn(ui, "Cancelar", &BTN_GHOST, "") {
                        choice = Some(PlanChoice::Cancel);
                    }
                });
        });
    choice
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

/// Perguntas de chave pendentes na arvore: (ordem de chegada, caminho).
fn pending_host_keys(node: &Node, path: &mut Vec<usize>, out: &mut Vec<(u64, Vec<usize>)>) {
    match node {
        Node::Leaf(pane) => {
            if let Some(p) = &pane.host_key {
                out.push((p.seq, path.clone()));
            }
        }
        Node::Split { children, .. } => {
            for (i, c) in children.iter().enumerate() {
                path.push(i);
                pending_host_keys(c, path, out);
                path.pop();
            }
        }
    }
}

/// Tipo e impressao digital (monoespacados) de uma chave na janela da chave do
/// servidor; `ilegivel` quando nao ha como ler a chave. Com `copiar`, mostra o
/// botao "Copiar" (Ctrl+C fica bloqueado com a janela aberta) e devolve a
/// impressao digital quando ele e clicado.
fn key_lines(
    ui: &mut egui::Ui,
    info: Option<&hostkey::KeyInfo>,
    ilegivel: &str,
    copiar: bool,
) -> Option<String> {
    let Some(info) = info else {
        ui.label(egui::RichText::new(ilegivel).color(ERROR_FG));
        return None;
    };
    ui.label(egui::RichText::new(&info.algorithm).monospace().color(CARD_TEXT));
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&info.fingerprint).monospace().color(CARD_TEXT));
        (copiar && painted_btn(ui, egui::vec2(64.0, 22.0), "Copiar", 12.0, &BTN_GHOST))
            .then(|| info.fingerprint.clone())
    })
    .inner
}

/// Spinner com legenda no corpo de um painel que ainda nao conectou.
fn pane_spinner(ui: &mut egui::Ui, text: &str) {
    ui.add_space((ui.available_height() * 0.4).max(12.0));
    ui.vertical_centered(|ui| {
        ui.add(egui::Spinner::new().size(20.0));
        ui.add_space(8.0);
        ui.label(egui::RichText::new(text).color(TEXT_WEAK));
    });
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

/// Largura/altura de cada cartao da listagem de hosts. Largura fixa: 178 da
/// 1.0.1 + 10% (arredondado).
const HOST_TILE_SIZE: egui::Vec2 = egui::vec2(196.0, 96.0);
/// Espaco entre cartoes (horizontal e vertical).
const TILE_SPACING: f32 = 10.0;
/// Margem interna do cartao.
const TILE_PAD: f32 = 12.0;
/// Lado do badge do icone principal.
const TILE_BADGE: f32 = 34.0;
/// Lado do icone dentro do badge (tema e sistemas).
const TILE_ICON: f32 = 19.0;
/// Icone de chave/senha: proporcional ao texto de 11 px do endereco.
const TILE_AUTH_ICON: f32 = 12.0;
/// Espaco entre o icone de chave/senha e o endereco.
const TILE_AUTH_GAP: f32 = 5.0;

/// Quantos cartoes cabem por linha em `avail` px (mesma regra de quebra do
/// horizontal_wrapped: so quebra quando sobra MENOS que um cartao).
fn tiles_per_row(avail: f32) -> usize {
    (((avail + TILE_SPACING) / (HOST_TILE_SIZE.x + TILE_SPACING)).floor() as usize).max(1)
}

/// Posicoes dentro de um cartao (pura, testavel sem desenhar).
struct TileLayout {
    badge: egui::Rect,
    title_pos: egui::Pos2,
    title_w: f32,
    /// Icone de chave/senha a esquerda do endereco (so hosts cadastrados).
    auth_icon: Option<egui::Rect>,
    subtitle_pos: egui::Pos2,
    subtitle_w: f32,
}

/// `sub_row_h` = altura da linha do texto de 11 px (centraliza o icone nela).
fn tile_layout(rect: egui::Rect, has_auth: bool, sub_row_h: f32) -> TileLayout {
    let badge = egui::Rect::from_min_size(
        rect.min + egui::vec2(TILE_PAD, TILE_PAD),
        egui::Vec2::splat(TILE_BADGE),
    );
    let title_pos = egui::pos2(badge.right() + 10.0, rect.top() + TILE_PAD + 2.0);
    let sub_y = rect.bottom() - TILE_PAD - 14.0;
    let auth_icon = has_auth.then(|| {
        egui::Rect::from_min_size(
            egui::pos2(
                rect.left() + TILE_PAD,
                sub_y + ((sub_row_h - TILE_AUTH_ICON) / 2.0).ceil(),
            ),
            egui::Vec2::splat(TILE_AUTH_ICON),
        )
    });
    let sub_x = auth_icon.map_or(rect.left() + TILE_PAD, |r| r.right() + TILE_AUTH_GAP);
    TileLayout {
        badge,
        title_pos,
        title_w: (rect.right() - TILE_PAD - title_pos.x).max(0.0),
        auth_icon,
        subtitle_pos: egui::pos2(sub_x, sub_y),
        subtitle_w: (rect.right() - TILE_PAD - sub_x).max(0.0),
    }
}

/// Icone do badge: imagem e cor (tint; SVG branco + tint, como os Lucide).
#[derive(Clone)]
struct TileIcon {
    image: egui::ImageSource<'static>,
    tint: egui::Color32,
}

impl TileIcon {
    /// Icone do tema (Lucide) no acento.
    fn theme(image: egui::ImageSource<'static>) -> Self {
        TileIcon { image, tint: ACCENT }
    }
}

/// O que um cartao do seletor mostra.
struct TileSpec<'a> {
    icon: TileIcon,
    title: &'a str,
    subtitle: &'a str,
    /// Chave/senha (cor + icone) a esquerda do subtitulo; so hosts cadastrados.
    auth: Option<(egui::Color32, egui::ImageSource<'static>)>,
    /// Linhas da dica (a 1a em destaque, a ultima = como usar).
    hint: Vec<String>,
}

/// Dica de um cartao: 1a linha em destaque, as do meio fracas e a ultima
/// (como usar) pequena, depois de um respiro.
fn tile_hint_ui(ui: &mut egui::Ui, lines: &[String]) {
    let n = lines.len();
    for (i, l) in lines.iter().enumerate() {
        let t = egui::RichText::new(l);
        if i == 0 {
            ui.label(t.color(CARD_TEXT));
        } else if i + 1 == n {
            ui.add_space(2.0);
            ui.label(t.small().color(TEXT_WEAK));
        } else {
            ui.label(t.color(TEXT_WEAK));
        }
    }
}

/// Desenha um cartao compacto (icone + nome + endereco, com o icone de
/// autenticacao a esquerda do endereco) com largura fixa, para caberem varios
/// por linha. A dica mostra o nome completo e o resto de `spec.hint`. Retorna a
/// resposta (sense de clique) para tratar duplo clique e menu de contexto.
fn host_tile(ui: &mut egui::Ui, spec: &TileSpec, selected: bool) -> egui::Response {
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

    let sub_row_h = ui.fonts(|f| f.row_height(&egui::FontId::proportional(11.0)));
    let lay = tile_layout(rect, spec.auth.is_some(), sub_row_h);

    // Badge do icone no canto superior esquerdo.
    painter.rect(
        lay.badge,
        8.0,
        WIDGET_BG,
        egui::Stroke::new(1.0, hex("#3d3d45")),
        egui::StrokeKind::Inside,
    );
    let icon_rect = egui::Rect::from_center_size(lay.badge.center(), egui::Vec2::splat(TILE_ICON));
    egui::Image::new(spec.icon.image.clone())
        .tint(spec.icon.tint)
        .paint_at(ui, icon_rect);

    // Icone do metodo de autenticacao (chave/senha) a esquerda do endereco, no
    // tamanho do texto; o nome do metodo aparece na dica ao passar o mouse.
    if let (Some((color, icon)), Some(r)) = (&spec.auth, lay.auth_icon) {
        egui::Image::new(icon.clone()).tint(*color).paint_at(ui, r);
    }

    // Titulo (nome) e subtitulo (endereco), truncados pela largura disponivel
    // para nunca invadir a borda do cartao (o nome inteiro fica na dica).
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
        lay.title_pos,
        truncated(spec.title, 14.0, egui::Color32::WHITE, lay.title_w),
        egui::Color32::WHITE,
    );
    painter.galley(
        lay.subtitle_pos,
        truncated(spec.subtitle, 11.0, TEXT_WEAK, lay.subtitle_w),
        TEXT_WEAK,
    );

    // Affordance: o cartao e clicavel; a dica traz o nome completo e como usar.
    response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_ui(|ui| tile_hint_ui(ui, &spec.hint))
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
    let per_row = tiles_per_row(ui.available_width());
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
                .add(
                    egui::ImageButton::new(
                        egui::Image::new(ICON_CLOSE)
                            .fit_to_exact_size(egui::vec2(14.0, 14.0))
                            .tint(TEXT_WEAK),
                    )
                    .frame(false),
                )
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
                ui.spacing_mut().item_spacing = egui::Vec2::splat(TILE_SPACING);

                for (idx, t) in tiles.iter().enumerate() {
                    let selected = has_kb && idx == sel;
                    match t {
                        // Terminal local (cmd.exe ou WSL).
                        PickerTile::Local(shell) => {
                            let shell = *shell;
                            let (titulo, subtitulo) = local_tile_text(shell);
                            let spec = TileSpec {
                                icon: TileIcon::theme(ICON_TERMINAL),
                                title: titulo,
                                subtitle: subtitulo,
                                auth: None,
                                hint: local_hint(shell),
                            };
                            let local = host_tile(ui, &spec, selected);
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
                            let spec = TileSpec {
                                icon: TileIcon::theme(ICON_PLUS),
                                title: "Novo host",
                                subtitle: "Cadastrar uma conexão SSH",
                                auth: None,
                                hint: vec![NEW_HOST_HINT.to_string()],
                            };
                            let novo = host_tile(ui, &spec, selected);
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
                            let subtitle = host_address(host);
                            let auth = match &host.auth {
                                AuthMethod::Password { .. } => (AUTH_PASS, ICON_PASSWORD),
                                AuthMethod::Key { .. } => (AUTH_KEY, ICON_KEY),
                            };
                            let spec = TileSpec {
                                icon: host_icon(host),
                                title: &title,
                                subtitle: &subtitle,
                                auth: Some(auth),
                                hint: host_hint(host),
                            };
                            let tile = host_tile(ui, &spec, selected);
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
                        "Nenhuma conexão corresponde a \"{}\".",
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
    clip: Option<&FsClip>,
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
                    // Nada de um painel e pintado no vizinho (a borda de 2 px
                    // do painel focado cabe na folga de 1 px).
                    ui.set_clip_rect(child_rect.expand(1.0).intersect(ui.clip_rect()));
                    render_node(
                        ui, child_rect, child, path, hosts, actions, focused, rects, pending, clip,
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
                SessionState::Connecting if pane.host_key.is_some() => {
                    "aguardando confirmação da chave"
                }
                SessionState::Connecting => "conectando...",
                SessionState::Connected => "conectado",
                SessionState::Closed => "sessão encerrada",
                SessionState::Error(_) => "erro",
            };

            // Mensagem de envio concluido some sozinha apos alguns segundos.
            if let Some(UploadUi {
                stage: UploadStage::Done { ok: true, at, .. },
                ..
            }) = &pane.upload
            {
                let left = std::time::Duration::from_secs(8).saturating_sub(at.elapsed());
                if left.is_zero() {
                    pane.upload = None;
                } else {
                    ui.ctx().request_repaint_after(left);
                }
            }
            // O resultado do download tambem (menos os de erro).
            if let Some(DownloadUi {
                stage: DownloadStage::Done { tone, at, .. },
                ..
            }) = &pane.download
            {
                if *tone != Tone::Error {
                    let left = std::time::Duration::from_secs(8).saturating_sub(at.elapsed());
                    if left.is_zero() {
                        pane.download = None;
                    } else {
                        ui.ctx().request_repaint_after(left);
                    }
                }
            }
            // E o do colar.
            if let Some(PasteUi {
                stage: PasteStage::Done { tone, at, .. },
                ..
            }) = &pane.paste
            {
                if *tone != Tone::Error {
                    let left = std::time::Duration::from_secs(8).saturating_sub(at.elapsed());
                    if left.is_zero() {
                        pane.paste = None;
                    } else {
                        ui.ctx().request_repaint_after(left);
                    }
                }
            }

            // Barra de titulo com fundo proprio, ocupando toda a largura.
            let title_resp = egui::Frame::NONE
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
                            "Selecione uma conexão".to_string()
                        } else {
                            pane.host_name.clone()
                        };
                        ui.label(egui::RichText::new(title).strong().color(egui::Color32::WHITE))
                            .on_hover_text(status);
                        // Andamento/resultado do envio de arquivos, ao lado do
                        // nome (a direita os botoes o encobririam).
                        if upload_status(ui, &pane.upload) {
                            pane.upload = None;
                        }

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

            // Barra de progresso fina na base do titulo durante um envio ou
            // um download.
            if let Some(frac) = title_fraction(pane) {
                title_progress(ui, title_resp.response.rect, frac);
            }

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
                } else if pane.sftp.is_some() && pane.host_key.is_some() {
                    // SFTP aguardando a confirmacao da chave: sem navegador ainda.
                    pane_spinner(ui, HOST_KEY_WAIT);
                } else if pane.sftp.is_some() {
                    // Painel SFTP: navegador de arquivos remoto (apenas a pasta atual).
                    if let SessionState::Error(msg) = &pane.state {
                        ui.colored_label(ERROR_FG, msg.clone());
                    }
                    // Itens copiados/recortados desta conexao: faixa com o que
                    // sera colado (em todo painel SFTP da mesma conexao).
                    let clip_here =
                        clip.filter(|c| pane.origin.as_ref().is_some_and(|o| o.same(&c.origin)));
                    let mut banner = BannerOut::default();
                    if let Some(c) = clip_here {
                        banner = clip_banner(ui, c, paste_avail(Some(c), pane), rect.width() >= 420.0);
                    }

                    // Area focavel cobrindo o conteudo: permite "selecionar" o painel
                    // (clicando) para que F5/setas atuem so no painel SFTP em foco. E
                    // adicionada antes das linhas, ficando atras delas nos cliques.
                    let content_rect = ui.available_rect_before_wrap();
                    let focus_id = ui.id().with(("sftp_focus", path.as_slice()));
                    let focus_resp = ui.interact(content_rect, focus_id, egui::Sense::click());
                    if focus_resp.clicked() || take || banner.paste {
                        focus_resp.request_focus();
                    }
                    if banner.paste {
                        actions.push(PaneAction::Paste { path: path.clone() });
                    }
                    if banner.clear {
                        actions.push(PaneAction::ClipClear);
                    }
                    let has_focus = focus_resp.has_focus();
                    // Trava as setas neste foco: sem isso o egui as usaria para
                    // mover o foco para outro widget na primeira tecla. Com o
                    // visualizador (ou uma abertura em curso), Tab e Esc tambem:
                    // senao o egui soltaria o foco antes de o navegador ver o Esc.
                    let lock_focus = |ui: &egui::Ui, exp: Option<&FileExplorer>| {
                        ui.memory_mut(|m| {
                            m.set_focus_lock_filter(
                                focus_id,
                                egui::EventFilter {
                                    tab: exp.is_some_and(|e| e.viewer.is_some()),
                                    horizontal_arrows: true,
                                    vertical_arrows: true,
                                    escape: exp.is_some_and(FileExplorer::wants_escape),
                                },
                            );
                        });
                    };
                    if has_focus {
                        *focused = Some(path.clone());
                        lock_focus(ui, pane.explorer.as_ref());
                    }
                    // F5 atualiza o painel em foco ou sob o cursor.
                    let active = has_focus || focus_resp.hovered();
                    let f5 = active && ui.input(|i| i.key_pressed(egui::Key::F5));

                    // Download: com o dialogo de conflito aberto nenhuma tecla
                    // age no navegador embaixo dele.
                    let asking = matches!(
                        pane.download.as_ref().map(|d| &d.stage),
                        Some(DownloadStage::Asking(_))
                    ) || pane.paste.as_ref().is_some_and(PasteUi::asking);
                    let dl = if !matches!(pane.state, SessionState::Connected) {
                        DlAvail::Offline
                    } else if pane.download.as_ref().is_some_and(DownloadUi::busy) {
                        DlAvail::Busy
                    } else {
                        DlAvail::Ready
                    };
                    // Rodape do download e do colar (andamento ou resultado),
                    // uma linha cada, no fim do painel.
                    let dl_line = pane
                        .download
                        .as_ref()
                        .is_some_and(|d| !matches!(d.stage, DownloadStage::Asking(_)));
                    let pa_line = pane.paste.as_ref().is_some_and(|p| !p.asking());
                    let footer_h = 30.0 * (u8::from(dl_line) + u8::from(pa_line)) as f32;
                    let list_rect = egui::Rect::from_min_max(
                        content_rect.min,
                        egui::pos2(content_rect.max.x, content_rect.max.y - footer_h),
                    );

                    let mut to_list: Vec<String> = Vec::new();
                    let mut fs_op: Option<FsOp> = None;
                    let mut cur_dir = String::new();
                    if let Some(exp) = &mut pane.explorer {
                        // O que esta copiado/recortado vale neste painel?
                        exp.clip_active = clip_here.is_some();
                        exp.cut_names = clip_here
                            .filter(|c| {
                                c.mode == ClipMode::Cut && paste::same_dir(&c.src_dir, &exp.cur_path)
                            })
                            .map(|c| c.names.clone());
                        // O ScrollArea ocupa toda a altura disponivel: o
                        // navegador fica restrito a area acima do rodape.
                        let out = ui
                            .scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
                                exp.ui(ui, ("sftp_explorer", path.as_slice()), has_focus && !asking, dl)
                            })
                            .inner;
                        // Onde o teclado conta como "neste navegador" para o
                        // Tab (`App::take_sftp_tab`); com um dialogo aberto o
                        // Tab e dele (anda entre os campos e botoes).
                        exp.keys_home = if asking || exp.dialog.is_some() {
                            Vec::new()
                        } else {
                            let (edit, busca) =
                                explorer_field_ids(&("sftp_explorer", path.as_slice()));
                            vec![focus_id, edit, busca]
                        };
                        to_list = out.to_list;
                        fs_op = out.op;
                        cur_dir = exp.cur_path.clone();
                        // F5/atualizar: recarrega o arquivo aberto ou a pasta.
                        let mut view = out.view;
                        if out.refresh || f5 {
                            if let Some(v) = exp.on_f5(&mut to_list) {
                                view = Some(v);
                            }
                        }
                        // Leitura para o visualizador (soltar o Cancel cancela).
                        if let Some((id, p)) = view {
                            match pane.sftp.as_ref().and_then(|s| s.read_file(id, p)) {
                                Some(c) => exp.set_view_cancel(id, c),
                                None => exp.view_unavailable(id),
                            }
                        }
                        // Estado de agora (ex.: abertura que acabou de comecar).
                        if has_focus {
                            lock_focus(ui, Some(exp));
                        }
                        // Clicar numa linha da listagem tambem seleciona o painel.
                        if out.clicked_row {
                            focus_resp.request_focus();
                        }
                        // Editando o caminho, o painel continua sendo o focado:
                        // depois do Enter/Esc o teclado volta para a listagem.
                        if out.inner_focus {
                            *focused = Some(path.clone());
                        }
                        if let (Some((seq, alvo)), Some(sftp)) = (out.goto, &pane.sftp) {
                            sftp.goto(seq, alvo);
                        }
                        if let Some(picks) = out.download {
                            actions.push(PaneAction::Download {
                                path: path.clone(),
                                remote_dir: exp.cur_path.clone(),
                                picks,
                            });
                        }
                        // Copiar/recortar guardam no app; colar comeca no
                        // fim do quadro (`App::begin_paste`).
                        let set = match out.clip {
                            Some(ClipCmd::Copy(items)) => Some((ClipMode::Copy, items)),
                            Some(ClipCmd::Cut(items)) => Some((ClipMode::Cut, items)),
                            Some(ClipCmd::Paste) => {
                                actions.push(PaneAction::Paste { path: path.clone() });
                                None
                            }
                            Some(ClipCmd::Clear) => {
                                actions.push(PaneAction::ClipClear);
                                None
                            }
                            None => None,
                        };
                        if let (Some((mode, items)), Some(origin)) = (set, pane.origin.clone()) {
                            let names = items.iter().map(|i| i.name.clone()).collect();
                            actions.push(PaneAction::ClipSet(FsClip {
                                mode,
                                origin,
                                src_dir: exp.cur_path.clone(),
                                items,
                                names,
                            }));
                        }
                    }
                    if footer_h > 0.0 {
                        let mut top = list_rect.max.y + 2.0;
                        // A linha do colar fica acima da do download.
                        if pa_line {
                            let r = egui::Rect::from_min_max(
                                egui::pos2(content_rect.min.x, top),
                                egui::pos2(content_rect.max.x, top + 28.0),
                            );
                            paste_footer(ui, r, &mut pane.paste);
                            top += 30.0;
                        }
                        if dl_line {
                            let footer = egui::Rect::from_min_max(
                                egui::pos2(content_rect.min.x, top),
                                content_rect.max,
                            );
                            download_footer(ui, footer, &mut pane.download);
                        }
                    }
                    if asking {
                        let choice = match &pane.download {
                            Some(DownloadUi {
                                stage: DownloadStage::Asking(p),
                                ..
                            }) => download_conflict_dialog(ui.ctx(), path, p, has_focus),
                            _ => None,
                        };
                        if let Some(c) = choice {
                            answer_conflict(pane, c);
                        }
                    }
                    // Colar: conflito no destino ou oferta de copiar e apagar.
                    let answer = match pane.paste.as_ref().map(|p| (&p.stage, p.dest_dir.as_str())) {
                        Some((PasteStage::Asking(prep), _)) => {
                            paste_conflict_dialog(ui.ctx(), path, prep, has_focus).map(PasteAnswer::Conflict)
                        }
                        Some((PasteStage::Offer(r), dest)) => {
                            paste_offer_dialog(ui.ctx(), path, r, dest, has_focus).map(PasteAnswer::Offer)
                        }
                        _ => None,
                    };
                    let restore = match answer {
                        Some(PasteAnswer::Conflict(c)) => answer_paste_conflict(pane, c),
                        Some(PasteAnswer::Offer(yes)) => answer_paste_offer(pane, yes),
                        None => None,
                    };
                    if let Some(c) = restore {
                        actions.push(PaneAction::ClipRestore(c));
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
                        if pane.host_key.is_some() {
                            pane_spinner(ui, HOST_KEY_WAIT);
                        } else {
                            pane_spinner(ui, &format!("Conectando a {}...", pane.host_name));
                        }
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

                        // Destino incerto para os arquivos soltos: barra com
                        // as opcoes sobre o rodape do painel.
                        // So com a sessao viva (uma escolha numa sessao morta
                        // nunca teria resposta).
                        if let (
                            Some(UploadUi {
                                stage: UploadStage::Asking(plan),
                                ..
                            }),
                            SessionState::Connected,
                        ) = (&pane.upload, &pane.state)
                        {
                            let plan = plan.clone();
                            match upload_plan_bar(ui.ctx(), rect, path, &plan) {
                                Some(PlanChoice::Send { dir, replace }) => {
                                    let sent = pane.ssh.as_ref().is_some_and(|ssh| {
                                        ssh.upload(plan.id, dir.clone(), plan.files.clone(), replace)
                                    });
                                    if !sent {
                                        pane.upload = Some(UploadUi::notice(
                                            "Sessão encerrada; nada foi enviado.",
                                        ));
                                    } else if let Some(u) = &mut pane.upload {
                                        u.stage = UploadStage::Sending {
                                            dir,
                                            index: 0,
                                            count: plan.files.len(),
                                            name: String::new(),
                                            sent: 0,
                                            size: 0,
                                        };
                                    }
                                }
                                Some(PlanChoice::Cancel) => pane.upload = None,
                                None => {}
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

    /// Antes de o egui ver a entrada do quadro: o Tab num navegador SFTP.
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.take_sftp_tab(ctx, raw_input);
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.ctx_for_repaint = Some(ctx.clone());

        match self.screen {
            Screen::Splash => {
                let elapsed = self.splash_start.elapsed().as_secs_f32();
                let skip = ctx.input(|i| i.pointer.any_pressed() || i.key_pressed(egui::Key::Escape));
                if elapsed >= SPLASH_SECS || skip {
                    self.leave_splash();
                } else {
                    ctx.request_repaint();
                }
            }
            Screen::Session => {
                // Arquivos soltos primeiro: o painel sob o cursor e achado
                // pelos retangulos do quadro anterior, antes que atalhos ou
                // sessoes encerradas mudem a arvore de paineis.
                // Pergunta de chave ja aberta: bloqueia o teclado antes dos
                // atalhos; de novo apos drenar, para uma que chegou agora.
                self.guard_host_key_keys(ctx);
                self.handle_file_drop(ctx);
                self.handle_session_keys(ctx);
                self.drain_ssh_events();
                self.guard_host_key_keys(ctx);
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

        self.handle_help_keys(ctx);

        // Barra de dicas de atalhos na base (Session e Hosts).
        match self.screen {
            Screen::Session | Screen::Hosts => {
                let armed = self.chord_armed_at.is_some();
                let in_session = matches!(self.screen, Screen::Session);
                let host_key = in_session && self.host_key_pending();
                egui::TopBottomPanel::bottom("hint_bar")
                    .frame(
                        egui::Frame::NONE
                            .fill(SCREEN_BG)
                            .inner_margin(egui::Margin::symmetric(10, 4)),
                    )
                    .show_separator_line(false)
                    .show(ctx, |ui| {
                        ui.horizontal(|ui| {
                            if host_key {
                                ui.label(
                                    egui::RichText::new(
                                        "Confirme a chave do servidor na janela aberta  \
                                         \u{00b7}  Esc cancela",
                                    )
                                    .small()
                                    .color(HIGHLIGHT),
                                );
                            } else if in_session && armed {
                                // Prefixo armado: mostra as opcoes do chord.
                                ui.label(
                                    egui::RichText::new("Ctrl+B \u{2026}")
                                        .strong()
                                        .color(ACCENT),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "H dividir  \u{00b7}  V empilhar  \u{00b7}  \
                                         setas trocar painel  \u{00b7}  \
                                         O ciclar  \u{00b7}  X fechar  \u{00b7}  \
                                         Ctrl+B / F1 literal  \u{00b7}  A ajuda  \u{00b7}  \
                                         Esc cancela",
                                    )
                                    .small()
                                    .color(CARD_TEXT),
                                );
                            } else if in_session {
                                ui.label(
                                    egui::RichText::new(self.session_hint())
                                        .small()
                                        .color(TEXT_WEAK),
                                );
                            } else {
                                ui.label(
                                    egui::RichText::new(
                                        "Digite para filtrar  \u{00b7}  \
                                         setas escolher  \u{00b7}  \
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

        // Realce do painel alvo enquanto arquivos sao arrastados sobre a janela.
        if matches!(self.screen, Screen::Session) {
            self.ui_drop_overlay(ctx);
        }

        // Download pedido neste quadro: escolhe a pasta de destino. O dialogo
        // do Windows e modal sobre a janela do app (bloqueia cliques nela).
        if let Some(req) = self.pending_download.take() {
            if matches!(self.screen, Screen::Session) {
                let path = req.path.clone();
                let mut dlg = rfd::FileDialog::new()
                    .set_title("Escolha a pasta onde salvar")
                    .set_parent(&*frame);
                if let Some(d) = self.download_dir.clone().or_else(default_download_dir) {
                    dlg = dlg.set_directory(d);
                }
                if let Some(dest) = dlg.pick_folder() {
                    self.download_dir = Some(dest.clone());
                    self.begin_download(req, dest);
                }
                // Devolve o foco ao painel que pediu.
                self.pending_focus = Some(path);
                ctx.request_repaint();
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

        // Pergunta sobre a chave do servidor, por cima de tudo.
        if matches!(self.screen, Screen::Session) {
            self.ui_host_key_prompt(ctx);
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
            master_key: None,
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
            remember_file: None,
            remembered: false,
            gate_focus_requested: false,
            gate_busy: 0,
            focused_path: None,
            chord_armed_at: None,
            pane_rects: Vec::new(),
            pending_focus: None,
            last_pane_focus: None,
            show_help: false,
            pending_delete: None,
            next_upload_id: 1,
            next_host_key_seq: 1,
            host_key_esc: false,
            pending_download: None,
            download_dir: None,
            next_download_id: 1,
            fs_clip: None,
            next_paste_id: 1,
            os_checked: std::collections::HashSet::new(),
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
            app.handle_help_keys(ctx);
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

    // --- Arquivos soltos sobre paineis -----------------------------------

    /// Quadro completo da sessao, como em `update()`, com arquivos soltos
    /// e/ou sendo arrastados sobre a janela.
    fn frame_drop(
        ctx: &egui::Context,
        app: &mut App,
        events: Vec<egui::Event>,
        dropped: Vec<PathBuf>,
    ) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 700.0),
            )),
            events,
            dropped_files: dropped
                .into_iter()
                .map(|p| egui::DroppedFile {
                    path: Some(p),
                    ..Default::default()
                })
                .collect(),
            focused: true,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            app.handle_session_keys(ctx);
            app.drain_ssh_events();
            app.handle_file_drop(ctx);
            egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
            app.ui_drop_overlay(ctx);
        });
    }

    /// App com um terminal SSH "conectado" a canais de teste.
    fn ssh_app(
        supports_upload: bool,
    ) -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<crate::ssh::UiToSsh>,
        std::sync::mpsc::Sender<crate::ssh::SshToUi>,
    ) {
        let mut app = app();
        let (handle, to_rx, from_tx) = crate::ssh::SshHandle::test_pair(supports_upload);
        if let Some(Node::Leaf(pane)) = &mut app.root {
            pane.ssh = Some(handle);
            pane.host_name = "servidor".into();
        }
        (app, to_rx, from_tx)
    }

    /// Pedidos de envio que a UI mandou a sessao (ignora teclado/resize).
    fn upload_requests(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::ssh::UiToSsh>,
    ) -> Vec<crate::ssh::UiToSsh> {
        use crate::ssh::UiToSsh;
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if matches!(m, UiToSsh::DropFiles { .. } | UiToSsh::Upload { .. }) {
                out.push(m);
            }
        }
        out
    }

    fn pane0(app: &App) -> &Pane {
        match &app.root {
            Some(Node::Leaf(p)) => p,
            _ => panic!("esperava um unico painel"),
        }
    }

    /// Pasta temporaria vazia e exclusiva do teste (apagada no `Drop`,
    /// inclusive quando o teste falha).
    struct TempDir(PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl TempDir {
        /// Arquivo pequeno `name` criado dentro da pasta.
        fn file(&self, name: &str) -> PathBuf {
            let f = self.0.join(name);
            std::fs::write(&f, b"conteudo").unwrap();
            f
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!("sagu-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TempDir(d)
    }

    /// Solta `files` com o mouse no centro do (unico) painel.
    fn drop_on_pane(ctx: &egui::Context, app: &mut App, files: Vec<PathBuf>) {
        for _ in 0..2 {
            frame_drop(ctx, app, vec![], vec![]);
        }
        let center = app.pane_rects[0].1.center();
        frame_drop(ctx, app, vec![egui::Event::PointerMoved(center)], vec![]);
        frame_drop(ctx, app, vec![], files);
    }

    #[test]
    fn drop_on_ssh_terminal_asks_session_then_follows_events() {
        use crate::ssh::{SshToUi, UiToSsh};
        use crate::upload::{DropPlan, Probe, UploadEvent};
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let (mut app, mut to_rx, from_tx) = ssh_app(true);
        let tmp = temp_dir("drop");
        let f = tmp.file("relatorio.txt");
        drop_on_pane(&ctx, &mut app, vec![f.clone()]);

        // A sessao recebeu o pedido com o arquivo; o painel esta localizando.
        let mut reqs = upload_requests(&mut to_rx);
        assert_eq!(reqs.len(), 1);
        let (id, files) = match reqs.remove(0) {
            UiToSsh::DropFiles { id, files } => (id, files),
            _ => panic!("DropFiles nao enviado"),
        };
        assert_eq!(files, vec![f.clone()]);
        assert!(matches!(
            pane0(&app).upload.as_ref().map(|u| &u.stage),
            Some(UploadStage::Locating { .. })
        ));
        // Outro drop durante o lote: ignorado (um lote por vez).
        frame_drop(&ctx, &mut app, vec![], vec![f.clone()]);
        assert!(upload_requests(&mut to_rx).is_empty());

        // Destino incerto: a sessao devolve um plano e o painel pergunta.
        let plan = DropPlan {
            id,
            files: files.clone(),
            probe: Probe {
                method: "home".into(),
                reason: "no-shell".into(),
                writable: true,
                fg_comm: String::new(),
                dir: Some("/home/user".into()),
                shell_dir: None,
            },
            home: "/home/user".into(),
            conflicts: vec![],
        };
        from_tx.send(SshToUi::Upload(UploadEvent::Plan(plan))).unwrap();
        frame_drop(&ctx, &mut app, vec![], vec![]);
        assert!(matches!(
            pane0(&app).upload.as_ref().map(|u| &u.stage),
            Some(UploadStage::Asking(_))
        ));

        // Conclusao: mensagem de sucesso na barra de titulo.
        from_tx
            .send(SshToUi::Upload(UploadEvent::Finished {
                id,
                dir: "/home/user".into(),
                sent: vec!["relatorio.txt".into()],
                failed: vec![],
            }))
            .unwrap();
        frame_drop(&ctx, &mut app, vec![], vec![]);
        match pane0(&app).upload.as_ref().map(|u| &u.stage) {
            Some(UploadStage::Done { text, ok: true, .. }) => {
                assert_eq!(text, "relatorio.txt enviado para /home/user")
            }
            _ => panic!("esperava envio concluido"),
        }
    }

    #[test]
    fn drop_on_local_terminal_or_folder_is_refused() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        // Terminal local: recusado com aviso, nada vai para a sessao.
        let tmp = temp_dir("drop");
        let (mut app, mut to_rx, _tx) = ssh_app(false);
        drop_on_pane(&ctx, &mut app, vec![tmp.file("a.txt")]);
        assert!(upload_requests(&mut to_rx).is_empty());
        assert!(matches!(
            pane0(&app).upload.as_ref().map(|u| &u.stage),
            Some(UploadStage::Done { ok: false, .. })
        ));

        // So uma pasta: recusada (pastas ainda nao sao enviadas).
        let (mut app, mut to_rx, _tx) = ssh_app(true);
        let pasta = tmp.file("b.txt").parent().unwrap().to_path_buf();
        drop_on_pane(&ctx, &mut app, vec![pasta]);
        assert!(upload_requests(&mut to_rx).is_empty());
        match pane0(&app).upload.as_ref().map(|u| &u.stage) {
            Some(UploadStage::Done { text, ok: false, .. }) => assert!(text.contains("Pastas")),
            _ => panic!("esperava aviso de pasta"),
        }
    }

    #[test]
    fn session_closed_during_upload_keeps_pane_open() {
        use crate::ssh::SshToUi;
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let (mut app, _to_rx, from_tx) = ssh_app(true);
        let tmp = temp_dir("drop");
        drop_on_pane(&ctx, &mut app, vec![tmp.file("c.txt")]);
        from_tx.send(SshToUi::Closed).unwrap();
        frame_drop(&ctx, &mut app, vec![], vec![]);
        let pane = pane0(&app);
        assert!(!pane.should_close, "painel nao pode fechar no meio do envio");
        assert!(matches!(pane.state, SessionState::Error(_)));
    }

    #[test]
    fn drop_without_pointer_position_goes_nowhere() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let (mut app, mut to_rx, _tx) = ssh_app(true);
        for _ in 0..2 {
            frame_drop(&ctx, &mut app, vec![], vec![]);
        }
        // Sem posicao conhecida do cursor: nao "chuta" o painel focado.
        let tmp = temp_dir("drop");
        frame_drop(&ctx, &mut app, vec![], vec![tmp.file("d.txt")]);
        assert!(upload_requests(&mut to_rx).is_empty());
        assert!(pane0(&app).upload.is_none());
    }

    /// Textos pintados no quadro, com a area que ocupam.
    fn painted_texts(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((
                    t.galley.text().to_string(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                _ => None,
            })
            .collect()
    }

    /// O andamento do envio aparece no titulo do painel, inteiro dentro do
    /// painel e antes dos botoes de dividir (a direita) — inclusive com a tela
    /// dividida e nome de arquivo longo.
    #[test]
    fn upload_status_is_visible_in_title_bar() {
        use crate::ssh::SshToUi;
        use crate::upload::UploadEvent;
        for split in [false, true] {
            let ctx = egui::Context::default();
            egui_extras::install_image_loaders(&ctx);
            let (mut app, _rx, tx) = ssh_app(true);
            if split {
                // Painel SSH a esquerda, seletor a direita (metade da tela).
                app.split_pane(&[], SplitDir::SideBySide);
            }
            let path: Vec<usize> = if split { vec![0] } else { vec![] };
            if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, &path)) {
                p.upload = Some(UploadUi {
                    id: 7,
                    skipped_dirs: 0,
                    stage: UploadStage::Locating { since: Instant::now() },
                });
            }
            let name = "relatorio-financeiro-consolidado-do-trimestre.xlsx";
            tx.send(SshToUi::Upload(UploadEvent::Progress {
                id: 7,
                dir: "/srv".into(),
                index: 0,
                count: 1,
                name: name.into(),
                sent: 50,
                size: 100,
            }))
            .unwrap();
            let mut out = None;
            for _ in 0..3 {
                let raw = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1200.0, 700.0),
                    )),
                    ..Default::default()
                };
                out = Some(ctx.run(raw, |ctx| {
                    app.drain_ssh_events();
                    egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
                }));
            }
            let pane = app.pane_rects.iter().find(|(p, _)| *p == path).unwrap().1;
            let texts = painted_texts(out.as_ref().unwrap());
            let (text, r) = texts
                .iter()
                .find(|(t, _)| t.contains("enviando"))
                .unwrap_or_else(|| panic!("status nao pintado: {texts:?}"));
            assert!(text.contains("50%"), "{text}");
            assert!(
                r.left() >= pane.left() && r.right() <= pane.right() - 40.0,
                "status fora da area visivel (split={split}): {r:?} em {pane:?}"
            );
        }
    }

    /// Bytes de teclado que a UI mandou a sessao (ignora resize e envios).
    fn sent_bytes(rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::ssh::UiToSsh>) -> Vec<u8> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let crate::ssh::UiToSsh::Data(d) = m {
                out.extend(d);
            }
        }
        out
    }

    #[test]
    fn f1_opens_help_even_with_terminal_focused() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let (mut app, mut to_rx, _tx) = ssh_app(false);
        app.pending_focus = Some(vec![]);
        for _ in 0..3 {
            frame(&ctx, &mut app, vec![]);
        }
        // O terminal tem o foco: uma tecla comum chega ao servidor.
        frame(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(sent_bytes(&mut to_rx), b"x");

        // F1 abre a ajuda sem vazar para o servidor; F1 de novo fecha.
        frame(&ctx, &mut app, vec![key(egui::Key::F1, egui::Modifiers::NONE)]);
        assert!(app.show_help);
        frame(&ctx, &mut app, vec![key(egui::Key::F1, egui::Modifiers::NONE)]);
        assert!(!app.show_help);
        assert!(sent_bytes(&mut to_rx).is_empty());

        // Ctrl+B, F1 envia o F1 ao servidor sem abrir a ajuda.
        frame(&ctx, &mut app, vec![key(egui::Key::B, egui::Modifiers::CTRL)]);
        frame(&ctx, &mut app, vec![key(egui::Key::F1, egui::Modifiers::NONE)]);
        assert!(!app.show_help);
        assert_eq!(sent_bytes(&mut to_rx), b"\x1bOP");
    }

    // --- Chave do servidor (TOFU) -----------------------------------------

    use crate::ssh::{SshToUi, UiToSsh};
    use tokio::sync::oneshot;

    /// Vetores reais (ver `hostkey::tests`), sem comentario.
    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIDx116/S6vbyAU3ZR1ebTYjMs187ZiPcltXd5Dg8Oapm";
    const KEY_A_FP: &str = "SHA256:JEpzgJ+qq0bLVo5Bj81AUoT0IRMtv5HFvtIy+xM6K74";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOaHfvKbIEav1XH7DfTNlEHNkxTAES3oEFcajJtkuuIU";
    const KEY_B_FP: &str = "SHA256:ZaMQkuNWz1gMHIFCAGlBWlXZuF4Wq8cp7cWUk/lN8vo";

    fn test_host_id() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x5a6_u128)
    }

    /// Host do cofre usado nas perguntas (srv:22).
    fn test_host() -> Host {
        let mut h = Host::new();
        h.id = test_host_id();
        h.name = "Produção".into();
        h.host = "srv".into();
        h.port = 22;
        h.username = "user".into();
        h
    }

    fn key_prompt(
        host_id: uuid::Uuid,
        host: &str,
        port: u16,
        presented: &str,
    ) -> (HostKeyPrompt, oneshot::Receiver<HostKeyAnswer>) {
        let (tx, rx) = oneshot::channel();
        let p = HostKeyPrompt {
            host_id,
            host: host.into(),
            port,
            presented: presented.into(),
            reply: tx,
        };
        (p, rx)
    }

    /// Quadro completo da sessao, na mesma ordem de `update()`.
    fn frame_session(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1200.0, 700.0),
            )),
            events,
            focused: true,
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            app.guard_host_key_keys(ctx);
            app.handle_file_drop(ctx);
            app.handle_session_keys(ctx);
            app.drain_ssh_events();
            app.guard_host_key_keys(ctx);
            app.handle_help_keys(ctx);
            egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
            app.ui_host_key_prompt(ctx);
        })
    }

    /// Textos pintados em dois quadros seguidos (a janela nova e medida no
    /// primeiro e so aparece no segundo).
    fn session_texts(ctx: &egui::Context, app: &mut App) -> Vec<String> {
        frame_session(ctx, app, vec![]);
        let out = frame_session(ctx, app, vec![]);
        painted_texts(&out).into_iter().map(|(t, _)| t).collect()
    }

    /// Todas as perguntas abertas como se estivessem na tela ha 1 s (armadas).
    fn arm_prompts(app: &mut App) {
        set_shown_at(app, Instant::now().checked_sub(std::time::Duration::from_secs(1)));
    }

    /// Todas as perguntas abertas como recem-mostradas (ainda nao armadas).
    fn disarm_prompts(app: &mut App) {
        set_shown_at(app, Some(Instant::now() + std::time::Duration::from_secs(60)));
    }

    fn set_shown_at(app: &mut App, at: Option<Instant>) {
        if let Some(root) = &mut app.root {
            for_each_pane_mut(root, &mut |p| {
                if let Some(k) = &mut p.host_key {
                    k.shown_at = at;
                }
            });
        }
    }

    /// Clica no centro do texto pintado `text` (botao da janela).
    fn click_text(ctx: &egui::Context, app: &mut App, text: &str) {
        let out = frame_session(ctx, app, vec![]);
        let r = painted_texts(&out)
            .into_iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| panic!("texto nao pintado: {text}"))
            .1;
        let pos = r.center();
        frame_session(ctx, app, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        frame_session(ctx, app, vec![click(pos, false)]);
    }

    /// Transforma o painel em `path` num terminal SSH ainda conectando.
    fn connecting_ssh_pane(
        app: &mut App,
        path: &[usize],
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<UiToSsh>,
        std::sync::mpsc::Sender<SshToUi>,
    ) {
        let (handle, to_rx, from_tx) = crate::ssh::SshHandle::test_pair(true);
        if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, path)) {
            p.picking = false;
            p.ssh = Some(handle);
            p.terminal = Some(Terminal::new(80, 24));
            p.state = SessionState::Connecting;
            p.host_name = "outro".into();
        }
        (to_rx, from_tx)
    }

    /// App com um unico terminal SSH conectando e o host de teste no cofre.
    fn key_app() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<UiToSsh>,
        std::sync::mpsc::Sender<SshToUi>,
    ) {
        let mut app = app();
        app.vault.hosts.push(test_host());
        let (to_rx, from_tx) = connecting_ssh_pane(&mut app, &[]);
        (app, to_rx, from_tx)
    }

    /// App com um unico painel SFTP conectando e o host de teste no cofre.
    fn sftp_key_app() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<crate::sftp::UiToSftp>,
        std::sync::mpsc::Sender<SftpToUi>,
    ) {
        let mut app = app();
        app.vault.hosts.push(test_host());
        let (handle, to_rx, from_tx) = crate::sftp::SftpHandle::test_pair();
        if let Some(Node::Leaf(p)) = &mut app.root {
            p.terminal = None;
            p.sftp = Some(handle);
            p.explorer = Some(FileExplorer::new());
            p.state = SessionState::Connecting;
            p.host_name = "srv  (SFTP)".into();
        }
        (app, to_rx, from_tx)
    }

    fn test_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        ctx
    }

    fn has(texts: &[String], needle: &str) -> bool {
        texts.iter().any(|t| t.contains(needle))
    }

    #[test]
    fn host_key_new_prompt_blocks_keyboard_and_waits_for_click() {
        let ctx = test_ctx();
        // Terminal conectado e focado a esquerda; o da direita pergunta.
        let (mut app, mut to_rx0, _tx0) = ssh_app(true);
        app.vault.hosts.push(test_host());
        app.split_pane(&[], SplitDir::SideBySide);
        let (_to_rx1, tx1) = connecting_ssh_pane(&mut app, &[1]);
        app.pending_focus = Some(vec![0]);
        for _ in 0..3 {
            frame_session(&ctx, &mut app, vec![]);
        }
        frame_session(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(sent_bytes(&mut to_rx0), b"x", "terminal deveria ter o foco");

        // A pergunta chega junto com uma tecla: nem essa vaza.
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx1.send(SshToUi::HostKey(p)).unwrap();
        frame_session(&ctx, &mut app, vec![egui::Event::Text("y".into())]);
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Servidor novo"), "{texts:?}");
        assert!(has(&texts, KEY_A_FP), "{texts:?}");
        assert!(has(&texts, "Aguardando confirmação da chave do servidor..."), "{texts:?}");

        // Teclado bloqueado: nada vai ao terminal, nada responde a pergunta.
        let keys = vec![
            egui::Event::Text("x".into()),
            key(egui::Key::Enter, egui::Modifiers::NONE),
            key(egui::Key::Space, egui::Modifiers::NONE),
            egui::Event::Text(" ".into()),
            key(egui::Key::Tab, egui::Modifiers::NONE),
            key(egui::Key::F1, egui::Modifiers::NONE),
            key(egui::Key::B, egui::Modifiers::CTRL),
        ];
        frame_session(&ctx, &mut app, keys);
        frame_session(&ctx, &mut app, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert!(sent_bytes(&mut to_rx0).is_empty());
        assert!(!app.show_help, "F1 nao pode abrir a ajuda por cima");
        assert!(app.chord_armed_at.is_none());
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        // Clique antes de a janela armar: ignorado.
        disarm_prompts(&mut app);
        click_text(&ctx, &mut app, "Confiar e conectar");
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Confiar e conectar");
        assert_eq!(rx.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(app.vault.hosts[0].host_key.as_deref(), Some(KEY_A));
        let texts = session_texts(&ctx, &mut app);
        assert!(!has(&texts, "Servidor novo"), "{texts:?}");
    }

    #[test]
    fn host_key_accept_saves_vault_and_frees_same_host_prompts() {
        let ctx = test_ctx();
        let (mut app, _to_rx0, tx0) = key_app();
        let dir = temp_dir("hostkey");
        let path = dir.0.join("cofre.sagu");
        app.vault_path = Some(path.clone());
        app.master_key = Some(VaultKey::new("t").unwrap());

        // Dois paineis do mesmo host, com a mesma chave: um clique libera os dois.
        app.split_pane(&[], SplitDir::SideBySide);
        let (_to_rx1, tx1) = connecting_ssh_pane(&mut app, &[1]);
        let (p0, mut rx0) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        let (p1, mut rx1) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx0.send(SshToUi::HostKey(p0)).unwrap();
        tx1.send(SshToUi::HostKey(p1)).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Outra conexão aguarda confirmação."), "{texts:?}");

        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Confiar e conectar");
        assert_eq!(rx0.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(rx1.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(app.vault.hosts[0].host_key.as_deref(), Some(KEY_A));
        assert!(app.hosts_error.is_none(), "{:?}", app.hosts_error);

        // Gravado na hora, cifrado, no arquivo do cofre.
        let bytes = std::fs::read(&path).unwrap();
        let back = vault::decrypt_vault(&bytes, "t").unwrap().0;
        assert_eq!(back.hosts[0].host_key.as_deref(), Some(KEY_A));
    }

    #[test]
    fn host_key_changed_alert_cancel_is_default() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        app.vault.hosts[0].host_key = Some(KEY_A.into());
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_B);
        tx.send(SshToUi::HostKey(p)).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Atenção: a chave do servidor mudou"), "{texts:?}");
        assert!(has(&texts, KEY_A_FP) && has(&texts, KEY_B_FP), "{texts:?}");

        // Esc antes de armar: ignorado.
        disarm_prompts(&mut app);
        frame_session(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        // Armada: Enter/Espaco nunca aceitam; Esc cancela.
        arm_prompts(&mut app);
        frame_session(
            &ctx,
            &mut app,
            vec![
                key(egui::Key::Enter, egui::Modifiers::NONE),
                key(egui::Key::Space, egui::Modifiers::NONE),
            ],
        );
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        frame_session(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        match rx.try_recv() {
            Ok(HostKeyAnswer::Cancel(msg)) => assert!(msg.contains("mudou"), "{msg}"),
            other => panic!("esperava cancelamento: {other:?}"),
        }
        assert_eq!(app.vault.hosts[0].host_key.as_deref(), Some(KEY_A));
    }

    #[test]
    fn host_key_changed_accepts_only_by_explicit_click() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        app.vault.hosts[0].host_key = Some(KEY_A.into());
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_B);
        tx.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Aceitar a nova chave e conectar");
        assert_eq!(rx.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(app.vault.hosts[0].host_key.as_deref(), Some(KEY_B));
    }

    #[test]
    fn second_pane_with_different_key_turns_into_alert() {
        let ctx = test_ctx();
        let (mut app, _to_rx0, tx0) = key_app();
        app.split_pane(&[], SplitDir::SideBySide);
        let (_to_rx1, tx1) = connecting_ssh_pane(&mut app, &[1]);
        let (p0, mut rx0) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        let (p1, mut rx1) = key_prompt(test_host_id(), "srv", 22, KEY_B);
        tx0.send(SshToUi::HostKey(p0)).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        tx1.send(SshToUi::HostKey(p1)).unwrap();

        // A mais antiga (A) aparece primeiro, como servidor novo.
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Servidor novo") && has(&texts, KEY_A_FP), "{texts:?}");
        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Confiar e conectar");
        assert_eq!(rx0.try_recv(), Ok(HostKeyAnswer::Accept));

        // A outra agora conflita com a chave guardada: alerta vermelho.
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Atenção: a chave do servidor mudou"), "{texts:?}");
        assert!(has(&texts, "Guardada:") && has(&texts, KEY_A_FP), "{texts:?}");
        assert!(has(&texts, "Nova:") && has(&texts, KEY_B_FP), "{texts:?}");
        assert_eq!(rx1.try_recv(), Err(oneshot::error::TryRecvError::Empty));
    }

    #[test]
    fn host_key_prompt_aborts_when_pane_closes_or_session_ends() {
        let ctx = test_ctx();

        // Painel fechado com a pergunta aberta: a sessao ve a pergunta cair.
        let (mut app, _to_rx0, _tx0) = key_app();
        app.split_pane(&[], SplitDir::SideBySide);
        let (_to_rx1, tx1) = connecting_ssh_pane(&mut app, &[1]);
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx1.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        app.close_pane(&[1]);
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed));
        assert_eq!(app.vault.hosts[0].host_key, None);

        // Sessao SSH desistiu (ex.: servidor caiu): janela some, erro fica.
        let (mut app, _to_rx, tx) = key_app();
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx.send(SshToUi::HostKey(p)).unwrap();
        assert!(has(&session_texts(&ctx, &mut app), "Servidor novo"));
        tx.send(SshToUi::Error("servidor caiu".into())).unwrap();
        tx.send(SshToUi::Closed).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(!has(&texts, "Servidor novo"), "{texts:?}");
        assert!(pane0(&app).host_key.is_none());
        assert!(matches!(pane0(&app).state, SessionState::Error(_)));
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed));

        // O mesmo num painel SFTP.
        let (mut app, _to_sftp, tx) = sftp_key_app();
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx.send(SftpToUi::HostKey(p)).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Servidor novo"), "{texts:?}");
        assert!(has(&texts, "Aguardando confirmação da chave do servidor..."), "{texts:?}");
        tx.send(SftpToUi::Error("servidor caiu".into())).unwrap();
        tx.send(SftpToUi::Closed).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(!has(&texts, "Servidor novo"), "{texts:?}");
        assert!(pane0(&app).host_key.is_none());
        assert!(matches!(pane0(&app).state, SessionState::Error(_)));
        assert_eq!(rx.try_recv(), Err(oneshot::error::TryRecvError::Closed));
    }

    #[test]
    fn host_key_prompt_cancelled_when_host_deleted_or_moved() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        app.vault.hosts.clear();
        frame_session(&ctx, &mut app, vec![]);
        match rx.try_recv() {
            Ok(HostKeyAnswer::Cancel(msg)) => assert!(msg.contains("excluída"), "{msg}"),
            other => panic!("esperava cancelamento: {other:?}"),
        }

        let (mut app, _to_rx, tx) = key_app();
        let (p, mut rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        app.vault.hosts[0].port = 2222;
        frame_session(&ctx, &mut app, vec![]);
        match rx.try_recv() {
            Ok(HostKeyAnswer::Cancel(msg)) => assert!(msg.contains("endereço"), "{msg}"),
            other => panic!("esperava cancelamento: {other:?}"),
        }
        assert_eq!(app.vault.hosts[0].host_key, None);
    }

    #[test]
    fn drop_ignored_while_host_key_prompt_open() {
        let ctx = test_ctx();
        let (mut app, mut to_rx, tx) = key_app();
        let (p, _rx) = key_prompt(test_host_id(), "srv", 22, KEY_A);
        tx.send(SshToUi::HostKey(p)).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert!(pane0(&app).host_key.is_some());
        let tmp = temp_dir("drop");
        drop_on_pane(&ctx, &mut app, vec![tmp.file("e.txt")]);
        assert!(upload_requests(&mut to_rx).is_empty());
        // Sem a pergunta, o drop num painel conectando daria um aviso.
        assert!(pane0(&app).upload.is_none());
    }

    #[test]
    fn editor_keeps_or_clears_host_key() {
        let mut stored = test_host();
        stored.host_key = Some(KEY_A.into());
        let editor = HostEditor::from_host(&stored);
        // A chave foi trocada (aceita) com o editor aberto: vale a do cofre.
        let mut current = stored.clone();
        current.host_key = Some(KEY_B.into());
        let id = stored.id;
        assert_eq!(editor.to_host(id, Some(&current)).host_key.as_deref(), Some(KEY_B));

        let mut moved = HostEditor::from_host(&stored);
        moved.port_text = "2222".into();
        assert_eq!(moved.to_host(id, Some(&current)).host_key, None);

        let mut case = HostEditor::from_host(&stored);
        case.host = " SRV ".into();
        assert_eq!(case.to_host(id, Some(&current)).host_key.as_deref(), Some(KEY_B));

        let mut forget = HostEditor::from_host(&stored);
        forget.forget_key = true;
        assert_eq!(forget.to_host(id, Some(&current)).host_key, None);

        let mut novo = HostEditor::new();
        novo.host = "srv".into();
        assert_eq!(novo.to_host(uuid::Uuid::new_v4(), None).host_key, None);
    }

    // --- Download pelo navegador SFTP --------------------------------------

    use crate::download::{DownloadItem, DownloadReport};
    use crate::sftp::UiToSftp;

    fn fs_node(name: &str) -> FsNode {
        FsNode {
            name: name.into(),
            label: download::safe_text(name, 255),
            path: format!("/srv/{name}"),
            kind: if name.starts_with("pasta") {
                sftp::EntryKind::Dir
            } else {
                sftp::EntryKind::File
            },
            link: None,
            size: 10,
            mode: Some(0o644),
            owner: "u".into(),
            group: "g".into(),
            date: String::new(),
        }
    }

    /// Navegador ja listando /srv com as entradas dadas.
    fn explorer_with(names: &[&str]) -> FileExplorer {
        let mut e = FileExplorer::new();
        e.cur_path = "/srv".into();
        e.loading = false;
        e.entries = names.iter().map(|n| fs_node(n)).collect();
        e
    }

    fn remote_entry(name: &str) -> sftp::RemoteEntry {
        sftp::RemoteEntry {
            name: name.into(),
            path: format!("/srv/{name}"),
            kind: sftp::EntryKind::File,
            link: None,
            size: 1,
            mode: Some(0o644),
            owner: "u".into(),
            group: "g".into(),
            mtime: 0,
        }
    }

    fn pick_names(e: &FileExplorer) -> Vec<String> {
        e.picks().into_iter().map(|p| p.name).collect()
    }

    /// Um quadro so com o navegador (em foco).
    fn explorer_frame(ctx: &egui::Context, e: &mut FileExplorer, events: Vec<egui::Event>) {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events,
            focused: true,
            ..Default::default()
        };
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                e.ui(ui, "exp", true, DlAvail::Ready);
            });
        });
    }

    /// App com um unico painel SFTP conectado (canais de teste) em /srv, com
    /// o foco do teclado.
    fn sftp_app(
        names: &[&str],
    ) -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<UiToSftp>,
        std::sync::mpsc::Sender<SftpToUi>,
    ) {
        let mut app = app();
        let (handle, to_rx, from_tx) = crate::sftp::SftpHandle::test_pair();
        if let Some(Node::Leaf(p)) = &mut app.root {
            p.terminal = None;
            p.sftp = Some(handle);
            p.explorer = Some(explorer_with(names));
            p.state = SessionState::Connected;
            p.host_name = "srv  (SFTP)".into();
        }
        app.pending_focus = Some(vec![]);
        (app, to_rx, from_tx)
    }

    fn pane0_mut(app: &mut App) -> &mut Pane {
        match &mut app.root {
            Some(Node::Leaf(p)) => p,
            _ => panic!("esperava um unico painel"),
        }
    }

    fn explorer0_mut(app: &mut App) -> &mut FileExplorer {
        pane0_mut(app).explorer.as_mut().unwrap()
    }

    /// Pedido com todas as entradas do painel (como se todas fossem marcadas).
    fn request_all(app: &App) -> PendingDownload {
        let exp = pane0(app).explorer.as_ref().unwrap();
        PendingDownload {
            path: vec![],
            remote_dir: exp.cur_path.clone(),
            picks: exp
                .entries
                .iter()
                .map(|n| download::Pick {
                    remote: n.path.clone(),
                    name: n.name.clone(),
                })
                .collect(),
        }
    }

    /// Pasta de destino vazia e exclusiva do teste.
    fn dl_dest() -> TempDir {
        temp_dir("dl-app")
    }

    type DlRequest = (u64, PathBuf, Vec<DownloadItem>, tokio::sync::watch::Receiver<bool>);

    /// Pedidos de download que a UI mandou a sessao (ignora o resto).
    fn download_requests(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiToSftp>) -> Vec<DlRequest> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let UiToSftp::Download {
                id,
                dest,
                items,
                cancel,
            } = m
            {
                out.push((id, dest, items, cancel));
            }
        }
        out
    }

    fn locals(items: &[DownloadItem]) -> Vec<(String, bool)> {
        items.iter().map(|i| (i.local.clone(), i.replace)).collect()
    }

    fn footer_stage(app: &App) -> &DownloadStage {
        &pane0(app).download.as_ref().expect("sem download no painel").stage
    }

    #[test]
    fn explorer_selection_click_ctrl_shift_and_ctrl_a() {
        let mut e = explorer_with(&["a", "b", "c", "d", "e"]);
        // Clique simples: so ela.
        e.click(1, false, false);
        assert_eq!(pick_names(&e), ["b"]);
        assert_eq!(e.sel, Some(1));
        // Ctrl marca mais uma; Ctrl de novo desmarca.
        e.click(3, true, false);
        assert_eq!(pick_names(&e), ["b", "d"]);
        e.click(1, true, false);
        assert_eq!(pick_names(&e), ["d"]);
        // Shift: intervalo desde a ancora, nas duas direcoes.
        e.click(1, false, false);
        e.click(3, false, true);
        assert_eq!(pick_names(&e), ["b", "c", "d"]);
        e.click(0, false, true);
        assert_eq!(pick_names(&e), ["a", "b"]);
        assert_eq!(e.sel, Some(0));
        // Setas: sem Shift seleciona so o novo; com Shift estende e recolhe.
        e.move_cursor(1, false);
        assert_eq!(pick_names(&e), ["b"]);
        e.move_cursor(1, true);
        e.move_cursor(1, true);
        assert_eq!(pick_names(&e), ["b", "c", "d"]);
        e.move_cursor(-1, true);
        assert_eq!(pick_names(&e), ["b", "c"]);
        // Ctrl+A: tudo, na ordem da listagem.
        e.select_all();
        assert_eq!(pick_names(&e), ["a", "b", "c", "d", "e"]);
        // So o cursor, sem nada marcado: nada a baixar, F2/Delete sem alvo.
        e.marked.clear();
        e.sel = Some(4);
        assert!(pick_names(&e).is_empty());
        assert_eq!(e.single_target(), None);
        e.click(4, false, false);
        assert_eq!(e.single_target(), Some(4));
        e.click(0, false, false);
        e.click(2, true, false);
        assert_eq!(e.single_target(), None);
        // Ctrl+clique desmarca "a": sobra "c" marcado, com o cursor em "a"
        // (desmarcado). F2/Delete nao agem em "a"; o download e so de "c".
        e.click(0, true, false);
        assert_eq!(pick_names(&e), ["c"]);
        assert_eq!(e.sel, Some(0));
        assert_eq!(e.single_target(), None);
        // Desmarcando o ultimo, nada fica selecionado para baixar.
        e.click(2, true, false);
        assert!(pick_names(&e).is_empty());
        assert_eq!(e.single_target(), None);
        // Navegar limpa a selecao.
        let mut to_list = Vec::new();
        e.navigate_to("/outra".into(), &mut to_list);
        assert!(e.marked.is_empty() && e.sel.is_none() && e.anchor.is_none());

        // Pelo teclado, num quadro real: Ctrl+A e Shift+setas.
        let ctx = test_ctx();
        let mut e = explorer_with(&["a", "b", "c"]);
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::A, egui::Modifiers::CTRL)]);
        assert_eq!(pick_names(&e), ["a", "b", "c"]);
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        assert_eq!(pick_names(&e), ["b"]);
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::ArrowDown, egui::Modifiers::SHIFT)]);
        assert_eq!(pick_names(&e), ["b", "c"]);
    }

    #[test]
    fn explorer_selection_survives_relisting() {
        let mut e = explorer_with(&["a", "b", "c", "d"]);
        e.click(0, false, false);
        e.click(2, true, false);
        e.click(3, true, false);
        e.sel = Some(2); // cursor em "c"
        // Nova listagem: "b" e "d" sumiram e entrou um item antes de todos.
        let novos = ["0novo", "a", "c", "x"].iter().map(|n| remote_entry(n)).collect();
        e.apply_listing("/srv", novos);
        assert_eq!(e.sel, Some(2), "cursor reposicionado pelo nome");
        assert_eq!(e.entries[2].name, "c");
        assert_eq!(pick_names(&e), ["a", "c"]);
        assert_eq!(e.anchor, Some(2));
        // O item do cursor sumiu (ex.: excluido): sem cursor, nunca outro item.
        e.apply_listing("/srv", vec![remote_entry("a"), remote_entry("x")]);
        assert_eq!(e.sel, None);
        assert_eq!(pick_names(&e), ["a"]);
        // Listagem de outra pasta nao mexe em nada.
        e.apply_listing("/outra", vec![remote_entry("z")]);
        assert_eq!(e.entries.len(), 2);
    }

    #[test]
    fn f2_and_delete_ignored_with_multiple_marked() {
        let ctx = test_ctx();
        let mut e = explorer_with(&["a", "b", "c"]);
        e.click(0, false, false);
        e.click(1, true, false);
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::F2, egui::Modifiers::NONE)]);
        assert!(e.dialog.is_none(), "F2 com 2 marcados abriu dialogo");
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        assert!(e.dialog.is_none(), "Delete com 2 marcados abriu dialogo");
        // Com um so, como antes.
        e.click(1, false, false);
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        assert!(matches!(&e.dialog, Some(FsDialog::Delete { name, .. }) if name == "b"));
        e.dialog = None;
        explorer_frame(&ctx, &mut e, vec![key(egui::Key::F2, egui::Modifiers::NONE)]);
        assert!(matches!(&e.dialog, Some(FsDialog::Rename { name, .. }) if name == "b"));
    }

    #[test]
    fn ctrl_s_on_focused_sftp_pane_requests_download() {
        let ctx = test_ctx();
        let (mut app, mut to_rx, _tx) = sftp_app(&["a.txt", "b.txt", "c.txt"]);
        for _ in 0..3 {
            frame_session(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![]));
        let e = explorer0_mut(&mut app);
        e.click(2, false, false);
        e.click(0, true, false);
        frame_session(&ctx, &mut app, vec![key(egui::Key::S, egui::Modifiers::CTRL)]);
        let req = app.pending_download.as_ref().expect("Ctrl+S nao pediu o download");
        assert!(req.path.is_empty());
        assert_eq!(req.remote_dir, "/srv");
        let names: Vec<&str> = req.picks.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "c.txt"], "ordem da listagem");
        assert_eq!(req.picks[0].remote, "/srv/a.txt");
        // Nada vai para a sessao antes de a pasta ser escolhida.
        assert!(download_requests(&mut to_rx).is_empty());
    }

    /// Um quadro do navegador (em foco), devolvendo a saida para achar textos.
    fn explorer_frame_out(ctx: &egui::Context, e: &mut FileExplorer, events: Vec<egui::Event>) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(900.0, 600.0),
            )),
            events,
            focused: true,
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                e.ui(ui, "exp", true, DlAvail::Ready);
            });
        })
    }

    /// Centro do texto `t` desenhado no quadro (o ultimo, se houver varios).
    fn text_pos(out: &egui::FullOutput, t: &str) -> Option<egui::Pos2> {
        painted_texts(out)
            .into_iter()
            .rev()
            .find(|(s, _)| s == t)
            .map(|(_, r)| r.center())
    }

    /// Botao direito numa linha e clique em `item` no menu que abre.
    fn context_menu_click(ctx: &egui::Context, e: &mut FileExplorer, row: &str, item: &str) -> Vec<String> {
        let out = explorer_frame_out(ctx, e, vec![]);
        let pos = text_pos(&out, row).expect("linha nao desenhada");
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Default::default(),
        };
        explorer_frame_out(ctx, e, vec![egui::Event::PointerMoved(pos), button(true)]);
        explorer_frame_out(ctx, e, vec![button(false)]);
        let out = explorer_frame_out(ctx, e, vec![]);
        let texts: Vec<String> = painted_texts(&out).into_iter().map(|(s, _)| s).collect();
        let at = text_pos(&out, item).expect("item do menu nao desenhado");
        explorer_frame_out(ctx, e, vec![egui::Event::PointerMoved(at), click(at, true)]);
        explorer_frame_out(ctx, e, vec![click(at, false)]);
        texts
    }

    #[test]
    fn context_menu_single_item_actions_off_with_multiple_marked() {
        let ctx = test_ctx();
        let mut e = explorer_with(&["a.txt", "b.txt", "c.txt"]);
        e.click(0, false, false);
        e.click(1, true, false);
        // Linha dentro da selecao de 2: o menu e da selecao, Excluir nao age.
        let texts = context_menu_click(&ctx, &mut e, "b.txt", "Excluir");
        assert!(texts.iter().any(|t| t == "2 itens selecionados"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Baixar 2 itens\u{2026}"), "{texts:?}");
        assert!(e.dialog.is_none(), "Excluir agiu com 2 itens marcados");
        let _ = context_menu_click(&ctx, &mut e, "b.txt", "Renomear");
        assert!(e.dialog.is_none(), "Renomear agiu com 2 itens marcados");
        assert_eq!(pick_names(&e), ["a.txt", "b.txt"]);
        // Linha fora da selecao: vira a selecao e o menu age nela.
        let texts = context_menu_click(&ctx, &mut e, "c.txt", "Excluir");
        assert!(texts.iter().any(|t| t == "c.txt"), "{texts:?}");
        assert!(matches!(&e.dialog, Some(FsDialog::Delete { name, .. }) if name == "c.txt"));
        assert_eq!(pick_names(&e), ["c.txt"]);
    }

    #[test]
    fn ctrl_s_hint_only_with_sftp_pane_focused() {
        let ctx = test_ctx();
        let (mut app, _to_rx, _tx) = sftp_app(&["a.txt"]);
        for _ in 0..3 {
            frame_session(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![]));
        assert!(app.session_hint().contains("Ctrl+S"), "{}", app.session_hint());
        // Terminal SSH em foco: Ctrl+S vai ao servidor, a dica nao o sugere.
        let ctx = test_ctx();
        let (mut app, _to_rx, _tx) = ssh_app(true);
        app.pending_focus = Some(vec![]);
        for _ in 0..3 {
            frame_session(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![]));
        assert!(!app.session_hint().contains("Ctrl+S"), "{}", app.session_hint());
    }

    #[test]
    fn ctrl_s_ignored_without_selection_or_while_busy() {
        let ctx = test_ctx();
        let (mut app, _to_rx, _tx) = sftp_app(&["a.txt", "b.txt", "c.txt"]);
        for _ in 0..3 {
            frame_session(&ctx, &mut app, vec![]);
        }
        frame_session(&ctx, &mut app, vec![key(egui::Key::S, egui::Modifiers::CTRL)]);
        assert!(app.pending_download.is_none(), "sem selecao nao pede");
        // Shift+seta estende a selecao (nao vira seta simples).
        frame_session(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        frame_session(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::SHIFT)]);
        assert_eq!(pick_names(pane0(&app).explorer.as_ref().unwrap()), ["a.txt", "b.txt"]);
        // Download em andamento no painel: Ctrl+S nao pede outro.
        let (cancel, _rx) = download::cancel_pair();
        pane0_mut(&mut app).download = Some(DownloadUi {
            id: 9,
            dest: PathBuf::from("C:\\x"),
            pre_skipped: Vec::new(),
            stage: DownloadStage::Running {
                cancel,
                cancelling: false,
                scanning: true,
                found: 0,
                index: 0,
                count: 0,
                name: String::new(),
                done: 0,
                total: 0,
            },
        });
        frame_session(&ctx, &mut app, vec![key(egui::Key::S, egui::Modifiers::CTRL)]);
        assert!(app.pending_download.is_none(), "pediu com download em andamento");
        // Terminado, o mesmo Ctrl+S pede.
        pane0_mut(&mut app).download = None;
        frame_session(&ctx, &mut app, vec![key(egui::Key::S, egui::Modifiers::CTRL)]);
        assert!(app.pending_download.is_some());
    }

    #[test]
    fn begin_download_without_conflicts_sends_request() {
        let (mut app, mut to_rx, _tx) = sftp_app(&["a:b.txt", "c.txt", "CON", ".."]);
        let dest_dir = dl_dest();
        let dest = dest_dir.0.clone();
        app.begin_download(request_all(&app), dest.clone());
        let mut reqs = download_requests(&mut to_rx);
        assert_eq!(reqs.len(), 1);
        let (id, d, items, _cancel) = reqs.remove(0);
        assert_eq!(d, dest);
        assert_eq!(
            locals(&items),
            [
                ("a_b.txt".to_string(), false),
                ("c.txt".to_string(), false),
                ("_CON".to_string(), false)
            ]
        );
        assert_eq!(items[0].remote, "/srv/a:b.txt");
        let dl = pane0(&app).download.as_ref().unwrap();
        assert_eq!(dl.id, id);
        assert!(matches!(dl.stage, DownloadStage::Running { scanning: true, .. }));
        assert_eq!(dl.pre_skipped.len(), 1, "\"..\" fica como ignorado");

        // Outro pedido com este em andamento, ou de outra pasta: descartado.
        app.begin_download(request_all(&app), dest.clone());
        let mut other = request_all(&app);
        other.remote_dir = "/outra".into();
        pane0_mut(&mut app).download = None;
        app.begin_download(other, dest.clone());
        assert!(download_requests(&mut to_rx).is_empty());
        assert!(pane0(&app).download.is_none());

        // Sessao ja encerrada: aviso, nada pedido.
        drop(to_rx);
        app.begin_download(request_all(&app), dest.clone());
        match footer_stage(&app) {
            DownloadStage::Done { text, tone, .. } => {
                assert_eq!(text, "Sessão encerrada; nada foi baixado.");
                assert_eq!(*tone, Tone::Error);
            }
            _ => panic!("esperava aviso"),
        }
    }

    #[test]
    fn begin_download_with_conflicts_asks_then_resolves() {
        let ctx = test_ctx();
        let substituir = vec![("a.txt".to_string(), true), ("b.txt".to_string(), false)];
        let pular = vec![("b.txt".to_string(), false)];
        for (acao, esperado) in [
            ("Substituir", Some(substituir)),
            ("Pular existentes", Some(pular)),
            ("Cancelar", None),
            ("Esc", None),
        ] {
            let (mut app, mut to_rx, _tx) = sftp_app(&["a.txt", "b.txt"]);
            let dest_dir = dl_dest();
            let dest = dest_dir.0.clone();
            std::fs::write(dest.join("a.txt"), "local").unwrap();
            for _ in 0..3 {
                frame_session(&ctx, &mut app, vec![]);
            }
            app.begin_download(request_all(&app), dest.clone());
            assert!(download_requests(&mut to_rx).is_empty(), "{acao}: pediu antes de perguntar");
            assert!(matches!(footer_stage(&app), DownloadStage::Asking(_)));
            let texts = session_texts(&ctx, &mut app);
            assert!(has(&texts, "Já existe no destino"), "{texts:?}");
            assert!(has(&texts, "\u{201c}a.txt\u{201d} já existe em"), "{texts:?}");
            // Enter e setas nao fazem nada com o dialogo aberto.
            frame_session(
                &ctx,
                &mut app,
                vec![
                    key(egui::Key::Enter, egui::Modifiers::NONE),
                    key(egui::Key::ArrowDown, egui::Modifiers::NONE),
                ],
            );
            assert!(matches!(footer_stage(&app), DownloadStage::Asking(_)));
            assert!(download_requests(&mut to_rx).is_empty());

            if acao == "Esc" {
                frame_session(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
            } else {
                click_text(&ctx, &mut app, acao);
            }
            let reqs = download_requests(&mut to_rx);
            match esperado {
                Some(v) => {
                    assert_eq!(reqs.len(), 1, "{acao}");
                    assert_eq!(locals(&reqs[0].2), v, "{acao}");
                    let dl = pane0(&app).download.as_ref().unwrap();
                    assert!(matches!(dl.stage, DownloadStage::Running { .. }));
                    if acao == "Pular existentes" {
                        assert_eq!(
                            dl.pre_skipped,
                            vec![("a.txt".to_string(), "já existia no destino (pulado)".to_string())]
                        );
                    }
                }
                None => {
                    assert!(reqs.is_empty(), "{acao}");
                    assert!(pane0(&app).download.is_none(), "{acao}");
                }
            }
            assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "local");
        }

        // Tudo ja existe: "Pular existentes" nem aparece.
        let (mut app, _to_rx, _tx) = sftp_app(&["a.txt"]);
        let dest_dir = dl_dest();
        let dest = dest_dir.0.clone();
        std::fs::write(dest.join("a.txt"), "local").unwrap();
        app.begin_download(request_all(&app), dest.clone());
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Substituir") && !has(&texts, "Pular existentes"), "{texts:?}");
    }

    #[test]
    fn download_events_update_footer_and_finish_text() {
        let ctx = test_ctx();
        let (mut app, mut to_rx, tx) = sftp_app(&["a.txt", "b.txt"]);
        let dest_dir = dl_dest();
        let dest = dest_dir.0.clone();
        app.begin_download(request_all(&app), dest.clone());
        let id = download_requests(&mut to_rx)[0].0;
        assert!(has(&session_texts(&ctx, &mut app), "Preparando o download\u{2026}"));

        tx.send(SftpToUi::Download(DownloadEvent::Scanning { id, found: 7 }))
            .unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "Preparando o download\u{2026} 7 itens encontrados"), "{texts:?}");

        tx.send(SftpToUi::Download(DownloadEvent::Progress {
            id,
            index: 0,
            count: 2,
            name: "a.txt".into(),
            done: 512,
            total: 1024,
        }))
        .unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(
            has(&texts, "Baixando 1/2: a.txt \u{00b7} 50% \u{00b7} 512 B de 1.0 KB"),
            "{texts:?}"
        );
        assert_eq!(title_fraction(pane0(&app)), Some(0.5));

        // Evento de outro lote: ignorado.
        tx.send(SftpToUi::Download(DownloadEvent::Progress {
            id: id + 100,
            index: 1,
            count: 2,
            name: "velho".into(),
            done: 1,
            total: 2,
        }))
        .unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(!has(&texts, "velho"), "{texts:?}");

        let report = DownloadReport {
            id,
            dest: dest.clone(),
            saved: 2,
            files: 2,
            last_saved: "b.txt".into(),
            ..Default::default()
        };
        tx.send(SftpToUi::Download(DownloadEvent::Finished(Box::new(report))))
            .unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert!(has(&texts, "2 arquivos baixados em"), "{texts:?}");
        assert!(matches!(footer_stage(&app), DownloadStage::Done { tone: Tone::Ok, .. }));
        assert_eq!(title_fraction(pane0(&app)), None);
        // Resultado dispensado pelo "x" (aqui, direto) libera o painel.
        assert!(!pane0(&app).download.as_ref().unwrap().busy());
    }

    #[test]
    fn cancel_button_signals_session() {
        let ctx = test_ctx();
        let (mut app, mut to_rx, tx) = sftp_app(&["a.txt"]);
        let dest_dir = dl_dest();
        let dest = dest_dir.0.clone();
        app.begin_download(request_all(&app), dest.clone());
        let (id, _, _, cancel_rx) = download_requests(&mut to_rx).remove(0);
        session_texts(&ctx, &mut app);
        assert!(!*cancel_rx.borrow());
        click_text(&ctx, &mut app, "Cancelar");
        assert!(*cancel_rx.borrow(), "a tarefa nao viu o cancelamento");
        assert!(has(&session_texts(&ctx, &mut app), "Cancelando\u{2026}"));
        // A sessao responde com o relatorio do cancelamento.
        let report = DownloadReport {
            id,
            dest: dest.clone(),
            files: 1,
            cancelled: true,
            ..Default::default()
        };
        tx.send(SftpToUi::Download(DownloadEvent::Finished(Box::new(report))))
            .unwrap();
        assert!(has(&session_texts(&ctx, &mut app), "Download cancelado; nada foi salvo."));

        // Fechar o painel com o download em andamento solta o `Cancel`.
        let (mut app, mut to_rx, _tx) = sftp_app(&["a.txt"]);
        app.begin_download(request_all(&app), dest.clone());
        let (_, _, _, cancel_rx) = download_requests(&mut to_rx).remove(0);
        assert!(cancel_rx.has_changed().is_ok());
        app.close_pane(&[]);
        assert!(cancel_rx.has_changed().is_err(), "a tarefa nao veria o fechamento");
        assert!(matches!(to_rx.try_recv(), Ok(UiToSftp::Disconnect)));
    }

    #[test]
    fn session_closed_during_download_keeps_pane_open() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = sftp_app(&["a.txt"]);
        let dest_dir = dl_dest();
        let dest = dest_dir.0.clone();
        app.begin_download(request_all(&app), dest.clone());
        tx.send(SftpToUi::Closed).unwrap();
        let texts = session_texts(&ctx, &mut app);
        let pane = pane0(&app);
        assert!(!pane.should_close, "painel nao pode fechar no meio do download");
        assert!(matches!(pane.state, SessionState::Error(_)));
        assert!(has(&texts, "Download interrompido: a sessão foi encerrada."), "{texts:?}");
        assert!(matches!(footer_stage(&app), DownloadStage::Done { tone: Tone::Error, .. }));

        // Com o dialogo de conflito aberto: o pedido cai (aviso neutro).
        let (mut app, _to_rx, tx) = sftp_app(&["a.txt"]);
        std::fs::write(dest.join("a.txt"), "x").unwrap();
        app.begin_download(request_all(&app), dest.clone());
        assert!(matches!(footer_stage(&app), DownloadStage::Asking(_)));
        tx.send(SftpToUi::Closed).unwrap();
        app.drain_ssh_events();
        assert!(app.root.is_none(), "sessao encerrada normalmente fecha o painel");
    }

    #[test]
    fn download_summary_texts() {
        let dest = PathBuf::from(r"C:\Users\x\Downloads");
        let base = DownloadReport {
            id: 1,
            dest: dest.clone(),
            ..Default::default()
        };
        let pasta = r"C:\Users\x\Downloads";

        // Um arquivo.
        let r = DownloadReport {
            saved: 1,
            files: 1,
            last_saved: "a.txt".into(),
            ..base.clone()
        };
        let (text, detail, tone) = download_summary(&r, &[]);
        assert_eq!(text, format!("a.txt baixado em {pasta}"));
        assert!(detail.is_empty());
        assert_eq!(tone, Tone::Ok);

        // Varios, com ignorados (antes e durante) e nomes ajustados.
        let r = DownloadReport {
            saved: 3,
            files: 3,
            dirs: 1,
            skipped: vec![("pasta/fifo".into(), "arquivo especial (dispositivo, fifo ou socket)".into())],
            renamed: vec![("pasta/a:b".into(), "pasta\\a_b".into())],
            ..base.clone()
        };
        let pre = vec![("x.txt".to_string(), "já existia no destino (pulado)".to_string())];
        let (text, detail, tone) = download_summary(&r, &pre);
        assert_eq!(
            text,
            format!(
                "3 arquivos baixados em {pasta} \u{00b7} 2 ignorado(s) \u{00b7} \
                 1 nome(s) ajustado(s) para o Windows"
            )
        );
        assert_eq!(tone, Tone::Ok);
        assert_eq!(
            detail,
            "Ignorados:\n\u{2022} x.txt: já existia no destino (pulado)\n\
             \u{2022} pasta/fifo: arquivo especial (dispositivo, fifo ou socket)\n\n\
             Nomes ajustados para o Windows:\n\u{2022} pasta/a:b \u{2192} pasta\\a_b"
        );

        // So pastas vazias.
        let r = DownloadReport { dirs: 1, ..base.clone() };
        assert_eq!(
            download_summary(&r, &[]).0,
            format!("Pasta baixada em {pasta} (sem arquivos)")
        );

        // Cancelado com e sem arquivos (tom neutro).
        let r = DownloadReport {
            saved: 2,
            files: 5,
            cancelled: true,
            ..base.clone()
        };
        assert_eq!(
            download_summary(&r, &[]),
            (
                format!("Download cancelado; 2 de 5 arquivos já estavam salvos em {pasta}."),
                String::new(),
                Tone::Neutral
            )
        );
        let r = DownloadReport { files: 5, cancelled: true, ..base.clone() };
        assert_eq!(download_summary(&r, &[]).0, "Download cancelado; nada foi salvo.");

        // Fatal.
        let r = DownloadReport {
            saved: 3,
            files: 9,
            fatal: Some("disco cheio".into()),
            ..base.clone()
        };
        assert_eq!(
            download_summary(&r, &[]),
            (
                format!("Download interrompido (disco cheio); 3 de 9 arquivos salvos em {pasta}"),
                String::new(),
                Tone::Error
            )
        );
        let r = DownloadReport {
            fatal: Some("conexão com o servidor perdida".into()),
            ..base.clone()
        };
        assert_eq!(
            download_summary(&r, &[]).0,
            "Download interrompido (conexão com o servidor perdida); nada foi salvo."
        );

        // Com falhas: a primeira no texto, a contagem das demais e todas no detalhe.
        let failed: Vec<(String, String)> = (0..12)
            .map(|i| (format!("p/f{i}"), "Permission denied".to_string()))
            .collect();
        let r = DownloadReport {
            saved: 1,
            files: 13,
            failed: failed.clone(),
            ..base.clone()
        };
        let (text, detail, tone) = download_summary(&r, &[]);
        assert_eq!(
            text,
            format!("1 de 13 arquivos baixados em {pasta}; p/f0: Permission denied (+11 com erro)")
        );
        assert_eq!(tone, Tone::Error);
        assert!(detail.starts_with("Com erro:\n\u{2022} p/f0: Permission denied\n"), "{detail}");
        assert!(detail.ends_with("\n\u{2026} e mais 2"), "{detail}");
        assert_eq!(detail.lines().count(), 12);

        // Nada baixado: por falha ou so ignorados.
        let r = DownloadReport {
            files: 1,
            failed: vec![("a.txt".into(), "Permission denied".into())],
            ..base.clone()
        };
        assert_eq!(download_summary(&r, &[]).0, "Nada foi baixado: a.txt: Permission denied");
        let r = DownloadReport {
            skipped: vec![("fifo".into(), "arquivo especial (dispositivo, fifo ou socket)".into())],
            ..base.clone()
        };
        assert_eq!(
            download_summary(&r, &[]),
            (
                "Nada foi baixado: fifo: arquivo especial (dispositivo, fifo ou socket)".to_string(),
                "Ignorados:\n\u{2022} fifo: arquivo especial (dispositivo, fifo ou socket)"
                    .to_string(),
                Tone::Error
            )
        );

        // Nome remoto com caractere de direcao: exibido com '?'.
        let r = DownloadReport {
            files: 1,
            failed: vec![("foto\u{202E}gpj.exe".into(), "erro".into())],
            ..base.clone()
        };
        assert_eq!(download_summary(&r, &[]).0, "Nada foi baixado: foto?gpj.exe: erro");
    }

    // --- Cartoes do seletor e ajuda (1.1.0) ---------------------------------

    /// Host cadastrado em root@srv:22, com chave ou senha.
    fn tile_host(name: &str, key: bool) -> Host {
        let mut h = Host::new();
        h.name = name.into();
        h.host = "srv".into();
        h.port = 22;
        h.username = "root".into();
        if key {
            h.auth = AuthMethod::Key { private_key: String::new(), passphrase: None };
        }
        h
    }

    const PICKER_SALT: &str = "picker_test";

    /// Um quadro so com o seletor (manage, filtro com autofoco), ocupando a
    /// tela `size` inteira: a largura util e a propria largura da tela.
    fn picker_frame(
        ctx: &egui::Context,
        hosts: &[Host],
        size: egui::Vec2,
        time: f64,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            time: Some(time),
            focused: true,
            ..Default::default()
        };
        let mut filter = String::new();
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                let opts = PickerOpts {
                    manage: true,
                    autofocus: true,
                    force_focus: false,
                    closable: false,
                };
                connection_picker(ui, hosts, &mut filter, PICKER_SALT, opts);
            });
        })
    }

    /// Galleys pintadas: texto visivel (com o "…" do corte), texto completo e
    /// area ocupada.
    fn painted_galleys(out: &egui::FullOutput) -> Vec<(String, String, egui::Rect)> {
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((
                    t.galley.rows.iter().map(|r| r.text()).collect(),
                    t.galley.text().to_string(),
                    egui::Rect::from_min_size(t.pos, t.galley.size()),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn tile_layout_with_auth() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, HOST_TILE_SIZE);
        assert_eq!(rect.width(), 196.0);
        let lay = tile_layout(rect, true, 13.0);
        let r = |x0, y0, x1, y1| egui::Rect::from_min_max(egui::pos2(x0, y0), egui::pos2(x1, y1));
        assert_eq!(lay.badge, r(12.0, 12.0, 46.0, 46.0));
        assert_eq!(lay.title_pos, egui::pos2(56.0, 14.0));
        assert_eq!(lay.title_w, 128.0);
        let auth = lay.auth_icon.expect("icone de autenticacao");
        assert_eq!(auth, r(12.0, 71.0, 24.0, 83.0));
        assert_eq!(lay.subtitle_pos, egui::pos2(29.0, 70.0));
        assert_eq!(lay.subtitle_w, 155.0);
        assert_eq!(auth.right() + TILE_AUTH_GAP, lay.subtitle_pos.x);
        // Tudo dentro do cartao, respeitando a margem interna.
        let inner = rect.shrink(TILE_PAD);
        assert!(inner.contains_rect(lay.badge) && inner.contains_rect(auth));
        assert!(lay.title_pos.x + lay.title_w <= inner.right());
        assert!(lay.subtitle_pos.x + lay.subtitle_w <= inner.right());
        assert!(lay.subtitle_pos.y + 13.0 <= inner.bottom() + 1.0);
    }

    #[test]
    fn tile_layout_without_auth() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, HOST_TILE_SIZE);
        let lay = tile_layout(rect, false, 13.0);
        assert!(lay.auth_icon.is_none());
        assert_eq!(lay.subtitle_pos, egui::pos2(12.0, 70.0));
        assert_eq!(lay.subtitle_w, 172.0);
        assert_eq!(lay.title_w, 128.0);
    }

    #[test]
    fn tiles_per_row_values() {
        for (avail, n) in [
            (608.0, 3),
            (616.0, 3),
            (968.0, 4),
            (1168.0, 5),
            (476.0, 2),
            (296.0, 1),
            (190.0, 1),
            (100.0, 1),
        ] {
            assert_eq!(tiles_per_row(avail), n, "largura {avail}");
        }
    }

    /// A conta das setas (tiles_per_row) bate com a quebra real da grade,
    /// inclusive no encaixe exato da janela minima (608 = 3 cartoes) e com a
    /// barra de rolagem do ScrollArea (flutuante: nao come largura).
    #[test]
    fn picker_rows_match_tiles_per_row() {
        let hosts: Vec<Host> =
            (0..10).map(|i| tile_host(&format!("h{i}"), i % 2 == 0)).collect();
        for w in [608.0, 616.0, 968.0, 476.0, 296.0, 190.0] {
            let ctx = egui::Context::default();
            egui_extras::install_image_loaders(&ctx);
            let size = egui::vec2(w, 700.0);
            picker_frame(&ctx, &hosts, size, 0.0, vec![]);
            let out = picker_frame(&ctx, &hosts, size, 0.1, vec![]);
            let texts = painted_galleys(&out);
            let top = texts
                .iter()
                .find(|(_, full, _)| full == "Terminal local")
                .map(|(_, _, r)| r.top())
                .expect("cartao Terminal local");
            let first_row = texts.iter().filter(|(_, _, r)| (r.top() - top).abs() < 0.5).count();
            assert_eq!(first_row, tiles_per_row(w), "largura {w}: {texts:?}");
            // Com a altura real da linha de 11 px (12,66 na fonte padrao) o
            // icone de autenticacao fica onde tile_layout_with_auth confere.
            let row_h = ctx.fonts(|f| f.row_height(&egui::FontId::proportional(11.0)));
            let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, HOST_TILE_SIZE);
            assert_eq!(
                tile_layout(rect, true, row_h).auth_icon,
                tile_layout(rect, true, 13.0).auth_icon,
                "altura da linha de 11 px: {row_h}"
            );
        }
    }

    #[test]
    fn arrow_down_moves_one_row() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let hosts: Vec<Host> = (0..10).map(|i| tile_host(&format!("h{i}"), true)).collect();
        let size = egui::vec2(968.0, 700.0);
        for i in 0..3 {
            picker_frame(&ctx, &hosts, size, i as f64 * 0.1, vec![]);
        }
        let base = egui::Id::new(PICKER_SALT);
        assert_eq!(ctx.memory(|m| m.focused()), Some(base.with("filter")));
        let sel = || ctx.memory(|m| m.data.get_temp::<usize>(base.with("sel")));
        let down = vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)];
        picker_frame(&ctx, &hosts, size, 0.3, down);
        assert_eq!(sel(), Some(4), "4 cartoes por linha em 968 px");
        let up = vec![key(egui::Key::ArrowUp, egui::Modifiers::NONE)];
        picker_frame(&ctx, &hosts, size, 0.4, up);
        assert_eq!(sel(), Some(0));
    }

    #[test]
    fn host_hint_lines() {
        let h = tile_host("Produção", true);
        assert_eq!(
            host_hint(&h),
            ["Produção", "root@srv:22", "Autenticação por chave", HOST_HINT_USE]
        );
        let h = tile_host("Produção", false);
        assert_eq!(host_hint(&h)[2], "Autenticação por senha");
        // Sem apelido: a 1a linha e o endereco do servidor (o nome exibido).
        let h = tile_host("  ", true);
        assert_eq!(host_hint(&h)[0], "srv");
        // Nome longo aparece inteiro na dica (no cartao ele e cortado).
        let nome = "x".repeat(200);
        assert_eq!(host_hint(&tile_host(&nome, true))[0], nome);
    }

    #[test]
    fn local_and_new_tile_hints() {
        assert_eq!(
            local_hint(pty::LocalShell::Wsl),
            ["WSL", "Linux (Windows Subsystem for Linux)", LOCAL_HINT_USE]
        );
        assert_eq!(
            local_hint(pty::LocalShell::Cmd),
            ["Terminal local", "Prompt de comando do Windows", LOCAL_HINT_USE]
        );
        // O cartao Novo abre com um clique e nao tem menu.
        assert!(NEW_HOST_HINT.contains("Ctrl+N") && !NEW_HOST_HINT.contains("Duplo"));
        let mut todas = vec![NEW_HOST_HINT.to_string()];
        todas.extend(local_hint(pty::LocalShell::Cmd));
        todas.extend(local_hint(pty::LocalShell::Wsl));
        todas.extend(host_hint(&tile_host("a", true)));
        todas.extend(host_hint(&tile_host("a", false)));
        for l in &todas {
            for sem_acento in ["Autenticacao", "botao", "opcoes", "conexao"] {
                assert!(!l.contains(sem_acento), "{l:?}");
            }
        }
    }

    /// Nome longo: cortado com "…" no cartao e inteiro na dica, junto com o
    /// metodo de autenticacao.
    #[test]
    fn hover_shows_full_name() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        ctx.style_mut(|s| s.interaction.tooltip_delay = 0.0);
        let nome = "Servidor de produção do ERP financeiro no datacenter principal";
        let hosts = vec![tile_host(nome, true)];
        let size = egui::vec2(1000.0, 700.0);
        let out = picker_frame(&ctx, &hosts, size, 0.0, vec![]);
        let (visivel, _, area) = painted_galleys(&out)
            .into_iter()
            .find(|(_, full, _)| full == nome)
            .expect("titulo do cartao");
        assert!(visivel.ends_with('\u{2026}'), "titulo nao foi cortado: {visivel:?}");

        let pos = area.center();
        picker_frame(&ctx, &hosts, size, 0.1, vec![egui::Event::PointerMoved(pos)]);
        let mut out = None;
        for i in 0..3 {
            out = Some(picker_frame(&ctx, &hosts, size, 0.6 + i as f64 * 0.5, vec![]));
        }
        let texts = painted_galleys(out.as_ref().unwrap());
        let sem_espaco = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert!(
            texts
                .iter()
                .any(|(vis, full, _)| full == nome && sem_espaco(vis) == sem_espaco(nome)),
            "dica sem o nome inteiro: {texts:?}"
        );
        assert!(
            texts.iter().any(|(_, full, _)| full == "Autenticação por chave"),
            "dica sem a autenticacao: {texts:?}"
        );
    }

    /// O tema escuro do app vale mesmo com o Windows no modo claro: o egui
    /// segue o tema do sistema e, sem isso, trocava para o estilo claro padrao
    /// (dica do cartao com fundo claro e titulo claro, ilegivel).
    #[test]
    fn dark_theme_survives_light_windows() {
        for sistema in [egui::Theme::Light, egui::Theme::Dark, egui::Theme::Light] {
            let ctx = egui::Context::default();
            apply_dark_theme(&ctx);
            for _ in 0..2 {
                let raw = egui::RawInput { system_theme: Some(sistema), ..Default::default() };
                let _ = ctx.run(raw, |_| {});
                let style = ctx.style();
                assert!(style.visuals.dark_mode, "sistema {sistema:?}: estilo claro ativo");
                assert_eq!(style.visuals.window_fill, MENU_BG, "sistema {sistema:?}: fundo da dica");
            }
        }
    }

    /// Versao no topo da ajuda, legivel, e a janela cabe na tela padrao e na
    /// minima (a lista de atalhos rola; "Fechar" sempre visivel).
    #[test]
    fn help_shows_version_on_top_and_fits() {
        for size in [egui::vec2(1000.0, 680.0), egui::vec2(640.0, 420.0)] {
            let ctx = egui::Context::default();
            egui_extras::install_image_loaders(&ctx);
            apply_dark_theme(&ctx);
            let mut app = app();
            app.show_help = true;
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            let mut out = None;
            for i in 0..4 {
                let raw = egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(i as f64 * 0.1),
                    focused: true,
                    ..Default::default()
                };
                out = Some(ctx.run(raw, |ctx| app.ui_help(ctx)));
            }
            assert!(app.show_help);
            let texts = painted_galleys(out.as_ref().unwrap());
            let find = |pred: &dyn Fn(&str) -> bool, what: &str| {
                texts
                    .iter()
                    .find(|(vis, _, _)| pred(vis))
                    .map(|(_, _, r)| *r)
                    .unwrap_or_else(|| panic!("{what} nao pintado em {size:?}: {texts:?}"))
            };
            let versao = find(&|t| t.contains(APP_VERSION_LABEL), "versao");
            let geral = find(&|t| t == "Geral", "Geral");
            let fechar = find(&|t| t == "Fechar", "Fechar");
            for (nome, r) in [("versao", versao), ("Geral", geral), ("Fechar", fechar)] {
                assert!(screen.contains_rect(r), "{nome} fora da tela {size:?}: {r:?}");
            }
            assert!(versao.bottom() <= geral.top(), "versao abaixo de Geral: {versao:?} {geral:?}");
            assert!(versao.height() >= 15.0, "versao pequena demais: {versao:?}");
            assert!(
                texts.iter().all(|(vis, _, _)| !vis.contains("SaguTerm v")),
                "versao antiga no rodape: {texts:?}"
            );
            // A janela inteira cabe na tela (os creditos quebram linha em vez
            // de alargar a janela).
            let win = ctx
                .memory(|m| m.area_rect(egui::Id::new("Atalhos de teclado")))
                .expect("janela da ajuda");
            assert!(screen.contains_rect(win), "ajuda fora da tela {size:?}: {win:?}");
        }
    }

    /// A ajuda chega ao tamanho final em poucos quadros, sem depender de o
    /// mouse mexer (o egui so repinta sozinho nos primeiros quadros).
    #[test]
    fn help_reaches_final_size_quickly() {
        for h in [680.0, 1000.0, 1600.0] {
            let ctx = egui::Context::default();
            egui_extras::install_image_loaders(&ctx);
            apply_dark_theme(&ctx);
            let mut app = app();
            app.show_help = true;
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, h));
            let mut rects = Vec::new();
            for i in 0..12 {
                let raw = egui::RawInput {
                    screen_rect: Some(screen),
                    time: Some(i as f64 * 0.1),
                    focused: true,
                    ..Default::default()
                };
                let _ = ctx.run(raw, |ctx| app.ui_help(ctx));
                rects.push(ctx.memory(|m| m.area_rect(egui::Id::new("Atalhos de teclado"))));
            }
            assert_eq!(rects[2], rects[11], "altura {h}: {rects:?}");
        }
    }

    /// Creditos dos icones no fim da ajuda: inteiros, com quebra de linha e
    /// sem alargar a janela (tela alta, sem rolagem).
    #[test]
    fn help_shows_icon_credits() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        apply_dark_theme(&ctx);
        let mut app = app();
        app.show_help = true;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 2000.0));
        let mut out = None;
        for i in 0..4 {
            let raw = egui::RawInput {
                screen_rect: Some(screen),
                time: Some(i as f64 * 0.1),
                focused: true,
                ..Default::default()
            };
            out = Some(ctx.run(raw, |ctx| app.ui_help(ctx)));
        }
        let texts = painted_galleys(out.as_ref().unwrap());
        let (vis, _, area) = texts
            .iter()
            .find(|(_, full, _)| full == HELP_ICON_CREDITS)
            .unwrap_or_else(|| panic!("creditos dos icones: {texts:?}"));
        let sem_espaco = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(sem_espaco(vis), sem_espaco(HELP_ICON_CREDITS), "creditos cortados");
        assert!(area.height() > 20.0, "creditos numa linha so: {area:?}");
        let win = ctx
            .memory(|m| m.area_rect(egui::Id::new("Atalhos de teclado")))
            .expect("janela da ajuda");
        assert!(win.contains_rect(*area), "{win:?} {area:?}");
        assert!(win.width() < 600.0, "janela larga demais: {win:?}");
        assert!(HELP_ICON_CREDITS.contains("Simple Icons") && HELP_ICON_CREDITS.contains("README"));
    }

    // --- SO do servidor detectado em segundo plano (1.1.0) -----------------

    use crate::osinfo::OsInfo;

    fn alma(version: &str) -> OsInfo {
        OsInfo {
            id: "almalinux".into(),
            name: "AlmaLinux".into(),
            version: Some(version.into()),
        }
    }

    /// Relatorio da sonda para o host de teste, na conexao `host`:`port`.
    fn os_report(host: &str, port: u16, os: Option<OsInfo>) -> OsReport {
        OsReport {
            host_id: test_host_id(),
            host: host.into(),
            port,
            os,
        }
    }

    /// Cofre em arquivo temporario (senha "t"); devolve a pasta (apagada no
    /// drop) e o arquivo.
    fn temp_vault(app: &mut App) -> (TempDir, PathBuf) {
        let dir = temp_dir("os");
        let path = dir.0.join("cofre.sagu");
        app.vault_path = Some(path.clone());
        app.master_key = Some(VaultKey::new("t").unwrap());
        (dir, path)
    }

    /// SO do primeiro host gravado no arquivo do cofre.
    fn saved_os(path: &std::path::Path) -> Option<OsInfo> {
        let back = vault::decrypt_vault(&std::fs::read(path).unwrap(), "t").unwrap().0;
        back.hosts[0].os.clone()
    }

    #[test]
    fn os_report_saves_vault_only_when_changed() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        pane0_mut(&mut app).state = SessionState::Connected;
        let (_dir, path) = temp_vault(&mut app);

        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        let texts = session_texts(&ctx, &mut app);
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.10")));
        assert_eq!(saved_os(&path), Some(alma("8.10")), "gravado na hora, cifrado");
        assert!(app.os_checked.contains(&test_host_id()));
        assert!(app.hosts_error.is_none(), "{:?}", app.hosts_error);
        // Nada vai para o terminal nem muda o painel.
        assert!(!has(&texts, "AlmaLinux") && !has(&texts, "almalinux"), "{texts:?}");
        assert!(matches!(pane0(&app).state, SessionState::Connected));
        assert!(pane0(&app).upload.is_none() && !pane0(&app).should_close);

        // Mesmo resultado: nao grava de novo (o arquivo apagado nao volta).
        std::fs::remove_file(&path).unwrap();
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert!(!path.exists(), "gravou sem mudanca");

        // Versao nova (upgrade do servidor): grava.
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.11"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(saved_os(&path), Some(alma("8.11")));

        // Sonda sem resultado nao apaga o que esta guardado.
        std::fs::remove_file(&path).unwrap();
        tx.send(SshToUi::Os(os_report("srv", 22, None))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.11")));
        assert!(!path.exists());
        let texts = session_texts(&ctx, &mut app);
        assert!(!has(&texts, "AlmaLinux"), "{texts:?}");
    }

    #[test]
    fn os_report_from_sftp_pane_is_applied() {
        let ctx = test_ctx();
        let (mut app, _to_sftp, tx) = sftp_key_app();
        let (_dir, path) = temp_vault(&mut app);
        tx.send(SftpToUi::Connected { home: "/home/u".into() }).unwrap();
        tx.send(SftpToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.10")));
        assert_eq!(saved_os(&path), Some(alma("8.10")));
        assert!(app.os_checked.contains(&test_host_id()));
        let exp = pane0(&app).explorer.as_ref().unwrap();
        assert!(exp.error.is_none(), "{:?}", exp.error);
        assert!(matches!(pane0(&app).state, SessionState::Connected));
    }

    #[test]
    fn os_report_ignored_when_host_deleted_or_moved() {
        let ctx = test_ctx();
        // Host excluido com a sessao aberta.
        let (mut app, _to_rx, tx) = key_app();
        let (_dir, path) = temp_vault(&mut app);
        app.vault.hosts.clear();
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert!(!path.exists());
        assert!(app.os_checked.is_empty());

        // Porta ou endereco trocados no editor com a sessao aberta.
        let (mut app, _to_rx, tx) = key_app();
        let (_dir2, path) = temp_vault(&mut app);
        app.vault.hosts[0].port = 2222;
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        tx.send(SshToUi::Os(os_report("outro", 2222, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(app.vault.hosts[0].os, None);
        assert!(!path.exists());
        assert!(app.os_checked.is_empty());

        // Mesmo endereco com outra grafia (maiusculas/espacos): vale.
        tx.send(SshToUi::Os(os_report(" SRV ", 2222, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.10")));
        assert!(path.exists());
    }

    #[test]
    fn editor_keeps_or_clears_os() {
        let mut stored = test_host();
        stored.os = Some(alma("8.10"));
        let id = stored.id;
        // O SO mudou (sonda) com o editor aberto: vale o do cofre.
        let editor = HostEditor::from_host(&stored);
        let mut current = stored.clone();
        current.os = Some(alma("8.11"));
        assert_eq!(editor.to_host(id, Some(&current)).os, Some(alma("8.11")));

        let mut moved = HostEditor::from_host(&stored);
        moved.port_text = "2222".into();
        assert_eq!(moved.to_host(id, Some(&current)).os, None);
        let mut other = HostEditor::from_host(&stored);
        other.host = "outro".into();
        assert_eq!(other.to_host(id, Some(&current)).os, None);

        let mut case = HostEditor::from_host(&stored);
        case.host = " SRV ".into();
        assert_eq!(case.to_host(id, Some(&current)).os, Some(alma("8.11")));

        // "Esquecer chave" so apaga a chave.
        let mut forget = HostEditor::from_host(&stored);
        forget.forget_key = true;
        assert_eq!(forget.to_host(id, Some(&current)).os, Some(alma("8.11")));

        let mut novo = HostEditor::new();
        novo.host = "srv".into();
        assert_eq!(novo.to_host(uuid::Uuid::new_v4(), None).os, None);

        // Salvar pelo editor: mesmo endereco mantem o SO e a marca da sonda;
        // porta trocada apaga o SO e libera a sonda na proxima conexao.
        let mut app = app();
        let (_dir, path) = temp_vault(&mut app);
        app.vault.hosts.push(stored.clone());
        app.os_checked.insert(id);
        let mut same = HostEditor::from_host(&stored);
        same.name = "Produção 2".into();
        app.commit_editor(same);
        assert!(app.editor.is_none() && app.hosts_error.is_none(), "{:?}", app.hosts_error);
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.10")));
        assert!(app.os_checked.contains(&id));
        assert_eq!(saved_os(&path), Some(alma("8.10")));
        let mut moved = HostEditor::from_host(&app.vault.hosts[0]);
        moved.port_text = "2222".into();
        app.commit_editor(moved);
        assert_eq!(app.vault.hosts[0].port, 2222);
        assert_eq!(app.vault.hosts[0].os, None);
        assert!(!app.os_checked.contains(&id));
        assert_eq!(saved_os(&path), None);
    }

    #[test]
    fn detect_os_off_skips_probe_and_keeps_no_os() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        let (_dir, path) = temp_vault(&mut app);
        let id = test_host_id();
        app.vault.hosts[0].os = Some(alma("8.10"));
        app.os_checked.insert(id);
        assert!(HostEditor::new().to_host(uuid::Uuid::new_v4(), None).detect_os);

        // Desligar no editor apaga o SO guardado e grava o cofre.
        let mut off = HostEditor::from_host(&app.vault.hosts[0]);
        assert!(off.detect_os);
        off.detect_os = false;
        app.commit_editor(off);
        assert!(app.editor.is_none() && app.hosts_error.is_none(), "{:?}", app.hosts_error);
        assert!(!app.vault.hosts[0].detect_os);
        assert_eq!(app.vault.hosts[0].os, None);
        assert!(!app.wants_os_probe(&app.vault.hosts[0]));
        let back = vault::decrypt_vault(&std::fs::read(&path).unwrap(), "t").unwrap().0;
        assert!(!back.hosts[0].detect_os);
        assert_eq!(back.hosts[0].os, None);

        // Sonda de uma sessao aberta antes de desligar: resultado ignorado.
        std::fs::remove_file(&path).unwrap();
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("8.10"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(app.vault.hosts[0].os, None);
        assert!(!path.exists());

        // Religar: a proxima conexao detecta de novo, mesmo que a sonda ja
        // tivesse respondido nesta abertura do cofre.
        app.os_checked.insert(id);
        let mut on = HostEditor::from_host(&app.vault.hosts[0]);
        assert!(!on.detect_os);
        on.detect_os = true;
        app.commit_editor(on);
        assert!(app.vault.hosts[0].detect_os);
        assert!(app.wants_os_probe(&app.vault.hosts[0]));
    }

    /// Quadro so com o editor de host aberto.
    fn editor_frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) -> egui::FullOutput {
        editor_frame_sized(ctx, app, egui::vec2(1200.0, 900.0), events)
    }

    /// Quadro so com o editor de host aberto, numa tela de tamanho `size`.
    fn editor_frame_sized(
        ctx: &egui::Context,
        app: &mut App,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            focused: true,
            ..Default::default()
        };
        ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |_| {});
            if app.editor.is_some() {
                app.ui_host_editor(ctx);
            }
        })
    }

    fn editor_texts(ctx: &egui::Context, app: &mut App) -> Vec<String> {
        let out = editor_frame(ctx, app, vec![]);
        painted_texts(&out).into_iter().map(|(t, _)| t).collect()
    }

    /// Clica no centro do texto pintado `text` do editor.
    fn click_editor_text(ctx: &egui::Context, app: &mut App, text: &str) {
        let out = editor_frame(ctx, app, vec![]);
        let r = painted_texts(&out)
            .into_iter()
            .find(|(t, _)| t == text)
            .unwrap_or_else(|| panic!("texto nao pintado: {text}"))
            .1;
        let pos = r.center();
        editor_frame(ctx, app, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
        editor_frame(ctx, app, vec![click(pos, false)]);
    }

    #[test]
    fn editor_detect_os_option_and_accents() {
        let ctx = test_ctx();
        let mut app = app();
        let (_dir, path) = temp_vault(&mut app);
        let mut stored = test_host();
        stored.os = Some(alma("8.10"));
        app.vault.hosts.push(stored.clone());
        app.editor = Some(HostEditor::from_host(&stored));

        editor_frame(&ctx, &mut app, vec![]);
        let texts = editor_texts(&ctx, &mut app);
        for t in [
            DETECT_OS_LABEL,
            "Usuário",
            "Autenticação",
            "Altere os dados da conexão e salve.",
        ] {
            assert!(has(&texts, t), "{t}: {texts:?}");
        }
        for sem_acento in ["Usuario", "Autenticacao", "conexao"] {
            assert!(!has(&texts, sem_acento), "{sem_acento}: {texts:?}");
        }
        // Para que serve fica so na dica (uma linha a menos no editor).
        assert!(!has(&texts, "ícone da distribuição"), "{texts:?}");
        assert!(!has(&texts, DETECT_OS_CLEARS));

        // Desmarcar avisa que o sistema sera apagado; salvar apaga e grava.
        click_editor_text(&ctx, &mut app, DETECT_OS_LABEL);
        assert!(!app.editor.as_ref().unwrap().detect_os);
        let texts = editor_texts(&ctx, &mut app);
        assert!(has(&texts, DETECT_OS_CLEARS), "{texts:?}");
        click_editor_text(&ctx, &mut app, "Salvar");
        assert!(app.editor.is_none(), "{:?}", app.hosts_error);
        assert!(!app.vault.hosts[0].detect_os);
        assert_eq!(app.vault.hosts[0].os, None);
        assert_eq!(saved_os(&path), None);
        assert!(DETECT_OS_HINT.starts_with("Mostra no cartão o ícone da distribuição."));
        assert!(DETECT_OS_HINT.contains("ForceCommand") && DETECT_OS_HINT.contains("command="));
    }

    /// O editor cabe na tela: o titulo e os botoes Salvar/Cancelar ficam
    /// visiveis na janela padrao (1000x680) e na minima (640x420), com o
    /// formulario mais longo (chave, chave do servidor guardada e o aviso da
    /// deteccao desligada); o que nao cabe rola. Numa tela alta nada rola.
    #[test]
    fn editor_fits_screen() {
        let mut long = test_host();
        long.auth = AuthMethod::Key {
            private_key: "-----BEGIN OPENSSH PRIVATE KEY-----".into(),
            passphrase: None,
        };
        long.host_key = Some(KEY_A.into());
        long.os = Some(alma("8.10"));
        let mut senha = test_host();
        senha.os = Some(alma("8.10"));

        for (host, size) in [
            (&long, egui::vec2(1000.0, 680.0)),
            (&long, egui::vec2(640.0, 420.0)),
            (&senha, egui::vec2(640.0, 420.0)),
            (&long, egui::vec2(1200.0, 900.0)),
        ] {
            let ctx = test_ctx();
            let mut app = app();
            let (_dir, path) = temp_vault(&mut app);
            app.vault.hosts.push(host.clone());
            let mut editor = HostEditor::from_host(host);
            editor.detect_os = false; // mostra o aviso: o formulario mais alto
            app.editor = Some(editor);
            // A janela se acomoda em alguns quadros (tamanho do anterior).
            let mut out = editor_frame_sized(&ctx, &mut app, size, vec![]);
            for _ in 0..4 {
                out = editor_frame_sized(&ctx, &mut app, size, vec![]);
            }
            let texts = painted_texts(&out);
            let rect = |t: &str| {
                texts
                    .iter()
                    .find(|(s, _)| s == t)
                    .unwrap_or_else(|| panic!("{size:?}: sem {t:?}: {texts:?}"))
                    .1
            };
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
            for t in ["Editar host", "Salvar", "Cancelar"] {
                assert!(screen.contains_rect(rect(t)), "{size:?}: {t} fora da tela: {:?}", rect(t));
            }
            // Folgas iguais em cima e embaixo quando o formulario rola.
            let salvar = rect("Salvar");
            let top_gap = rect("Editar host").top() - 25.0;
            let bottom_gap = size.y - (salvar.bottom() + 8.5 + 25.0);
            if size.y < 900.0 {
                assert!((top_gap - EDITOR_SCREEN_GAP).abs() < 1.0, "{size:?}: {top_gap}");
                assert!((bottom_gap - EDITOR_SCREEN_GAP).abs() < 1.0, "{size:?}: {bottom_gap}");
            }
            // Ultimo item do formulario: inteiro acima dos botoes numa tela
            // alta; na minima, fica para rolar (cortado ou nem pintado; o
            // formulario termina 24 px acima do botao).
            let last = if host.host_key.is_some() { "Esquecer chave" } else { DETECT_OS_CLEARS };
            let form_end = salvar.top() - 8.5 - 24.0;
            let last_rect = texts.iter().find(|(s, _)| s == last).map(|(_, r)| *r);
            if size.y >= 900.0 {
                let r = last_rect.unwrap_or_else(|| panic!("{size:?}: sem {last:?}"));
                assert!(r.bottom() <= form_end, "{size:?}: {last} cortado: {r:?}");
            } else if size.y <= 420.0 {
                assert!(last_rect.is_none_or(|r| r.bottom() > form_end), "{size:?}: nada rolou");
            }

            // Salvar alcancavel com o mouse.
            let pos = salvar.center();
            editor_frame_sized(
                &ctx,
                &mut app,
                size,
                vec![egui::Event::PointerMoved(pos), click(pos, true)],
            );
            editor_frame_sized(&ctx, &mut app, size, vec![click(pos, false)]);
            assert!(app.editor.is_none(), "{size:?}: {:?}", app.hosts_error);
            assert_eq!(saved_os(&path), None);
        }
    }

    #[test]
    fn changed_key_accepted_forgets_os() {
        let ctx = test_ctx();
        let id = test_host_id();

        // Primeira chave (servidor novo, ex.: depois de "Esquecer chave"): o SO fica.
        let (mut app, _to_rx, tx) = key_app();
        let (_dir, _path) = temp_vault(&mut app);
        app.vault.hosts[0].os = Some(alma("8.10"));
        app.os_checked.insert(id);
        let (p, mut rx) = key_prompt(id, "srv", 22, KEY_A);
        tx.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Confiar e conectar");
        assert_eq!(rx.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(app.vault.hosts[0].os, Some(alma("8.10")));
        assert!(app.os_checked.contains(&id));

        // Chave diferente aceita (servidor reinstalado): o SO antigo sai, e a
        // sonda desta conexao (ou da proxima) grava o novo.
        let (mut app, _to_rx, tx) = key_app();
        let (_dir2, path) = temp_vault(&mut app);
        app.vault.hosts[0].host_key = Some(KEY_A.into());
        app.vault.hosts[0].os = Some(alma("8.10"));
        app.os_checked.insert(id);
        let (p, mut rx) = key_prompt(id, "srv", 22, KEY_B);
        tx.send(SshToUi::HostKey(p)).unwrap();
        session_texts(&ctx, &mut app);
        arm_prompts(&mut app);
        click_text(&ctx, &mut app, "Aceitar a nova chave e conectar");
        assert_eq!(rx.try_recv(), Ok(HostKeyAnswer::Accept));
        assert_eq!(app.vault.hosts[0].host_key.as_deref(), Some(KEY_B));
        assert_eq!(app.vault.hosts[0].os, None);
        assert!(app.wants_os_probe(&app.vault.hosts[0]));
        assert_eq!(saved_os(&path), None);
        tx.send(SshToUi::Os(os_report("srv", 22, Some(alma("9.4"))))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert_eq!(saved_os(&path), Some(alma("9.4")));
    }

    #[test]
    fn os_detection_runs_once_per_host() {
        let ctx = test_ctx();
        let (mut app, _to_rx, tx) = key_app();
        let id = test_host_id();
        let host = test_host();
        assert!(app.wants_os_probe(&host));
        // Sonda que rodou sem resultado tambem conta.
        tx.send(SshToUi::Os(os_report("srv", 22, None))).unwrap();
        frame_session(&ctx, &mut app, vec![]);
        assert!(app.os_checked.contains(&id));
        assert!(!app.wants_os_probe(&host));
        assert!(app.wants_os_probe(&Host::new()), "outros hosts seguem");
        // Deteccao desligada no host: nunca.
        let mut off = Host::new();
        off.detect_os = false;
        assert!(!app.wants_os_probe(&off));
        // Bloquear o cofre recomeca: a proxima abertura detecta de novo.
        app.lock();
        assert!(app.os_checked.is_empty());
        assert!(app.wants_os_probe(&host));
    }

    // --- Icones dos sistemas no cartao (1.1.0) ------------------------------

    /// Bytes embutidos de um icone (`include_image!`).
    fn icon_bytes(img: &egui::ImageSource<'static>) -> (String, &'static [u8]) {
        match img {
            egui::ImageSource::Bytes {
                uri,
                bytes: egui::load::Bytes::Static(b),
            } => (uri.to_string(), b),
            _ => panic!("icone sem bytes embutidos"),
        }
    }

    #[test]
    fn os_icons_match_icon_slugs() {
        let mut slugs: Vec<&str> = OS_ICONS.iter().map(|(s, ..)| *s).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), OS_ICONS.len(), "slug repetido em OS_ICONS");
        let mut want = osinfo::ICON_SLUGS.to_vec();
        want.sort_unstable();
        assert_eq!(slugs, want);
        // Nenhum arquivo sobrando em assets/os (todo icone la tem credito no
        // README e uso no app).
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/os");
        let mut files: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort_unstable();
        let expected: Vec<String> = slugs.iter().map(|s| format!("{s}.svg")).collect();
        assert_eq!(files, expected);
    }

    /// Cada SVG de assets/os e so <svg><title/><path fill=branco d=.../></svg>:
    /// sem script, link externo nem estilo, e pequeno.
    #[test]
    fn os_icon_assets_are_safe_white_svgs() {
        for (slug, img, _) in OS_ICONS {
            let (uri, bytes) = icon_bytes(img);
            assert_eq!(uri, format!("bytes://../assets/os/{slug}.svg"));
            let text = std::str::from_utf8(bytes).expect("SVG em UTF-8");
            assert!(text.starts_with("<svg "), "{slug}");
            assert!(text.ends_with("</svg>"), "{slug}");
            assert_eq!(text.matches("<path").count(), 1, "{slug}");
            assert_eq!(text.matches("<path fill=\"#ffffff\" d=").count(), 1, "{slug}");
            for bad in ["script", "href", "style", "xlink", "<!", "\r", "\n"] {
                assert!(!text.contains(bad), "{slug}: {bad:?}");
            }
            assert!(bytes.len() <= 8 * 1024, "{slug}: {} bytes", bytes.len());
        }
    }

    /// Todos carregam pelo mesmo carregador do app (egui_extras/resvg) e saem
    /// brancos: um SVG preto ou colorido nao aceitaria o tint.
    #[test]
    fn os_icons_render_white() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        for (slug, img, _) in OS_ICONS {
            let (uri, bytes) = icon_bytes(img);
            assert!(egui_extras::image::load_svg_bytes(bytes).is_ok(), "{slug}");
            ctx.include_bytes(uri.clone(), bytes);
            // 19 px do badge numa tela a 200%.
            let hint = egui::SizeHint::Size(38, 38);
            let image = match ctx.try_load_image(&uri, hint) {
                Ok(egui::load::ImagePoll::Ready { image }) => image,
                other => panic!("{slug}: {:?}", other.map(|_| "pendente")),
            };
            assert_eq!(image.size, [38, 38], "{slug}");
            let opaque: Vec<_> = image.pixels.iter().filter(|p| p.a() == 255).collect();
            assert!(!opaque.is_empty(), "{slug}: nada opaco");
            assert!(
                opaque.iter().all(|p| p.r() >= 250 && p.g() >= 250 && p.b() >= 250),
                "{slug}: pixel opaco que nao e branco"
            );
        }
    }

    /// Contraste WCAG (luminancia relativa, limiar 0,04045) entre duas cores.
    fn wcag_contrast(a: egui::Color32, b: egui::Color32) -> f64 {
        let lum = |c: egui::Color32| {
            let lin = |u: u8| {
                let v = u as f64 / 255.0;
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
        };
        let (la, lb) = (lum(a), lum(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn os_icon_colors_readable_on_badge() {
        for (slug, _, tint) in OS_ICONS {
            let c = wcag_contrast(*tint, WIDGET_BG);
            assert!(c >= 4.5, "{slug}: {c:.2}:1");
            assert_eq!(tint.a(), 255, "{slug}");
        }
        // CentOS tem traco fino: 7:1.
        let centos = OS_ICONS.iter().find(|(s, ..)| *s == "centos").unwrap().2;
        assert!(wcag_contrast(centos, WIDGET_BG) >= 7.0);
    }

    #[test]
    fn host_icon_uses_os_or_server() {
        let with_os = |id: &str| {
            let mut h = tile_host("a", true);
            h.os = Some(OsInfo {
                id: id.into(),
                name: "X".into(),
                version: None,
            });
            host_icon(&h)
        };
        let icon = with_os("almalinux");
        assert_eq!(icon.image.uri(), Some("bytes://../assets/os/almalinux.svg"));
        assert_eq!(icon.tint, CARD_TEXT);
        let icon = with_os("ubuntu");
        assert_eq!(icon.image.uri(), Some("bytes://../assets/os/ubuntu.svg"));
        assert_eq!(icon.tint, hex("#f15c29"));
        let icon = with_os("opensuse-leap");
        assert_eq!(icon.image.uri(), Some("bytes://../assets/os/opensuse.svg"));
        // Sem icone proprio ou sem SO: mantem o servidor do tema.
        let server = ICON_SERVER.uri();
        assert_eq!(server, Some("bytes://../assets/server.svg"));
        for icon in [with_os("ol"), with_os("kali"), host_icon(&tile_host("a", true))] {
            assert_eq!(icon.image.uri(), server);
            assert_eq!(icon.tint, ACCENT);
        }
    }

    #[test]
    fn host_hint_with_os() {
        let mut h = tile_host("Produção", true);
        h.os = Some(alma("8.10"));
        assert_eq!(
            host_hint(&h),
            [
                "Produção",
                "root@srv:22",
                "Autenticação por chave",
                "Sistema: AlmaLinux 8.10",
                HOST_HINT_USE
            ]
        );
        // Sem versao (Arch): so o nome.
        h.os = Some(OsInfo {
            id: "arch".into(),
            name: "Arch Linux".into(),
            version: None,
        });
        assert_eq!(host_hint(&h)[3], "Sistema: Arch Linux");
        // Sem SO: as linhas de sempre.
        h.os = None;
        assert_eq!(
            host_hint(&h),
            ["Produção", "root@srv:22", "Autenticação por chave", HOST_HINT_USE]
        );
    }

    /// O cartao de um host com SO pinta o icone do sistema no badge (e o
    /// carregador do egui o rasteriza sem erro).
    #[test]
    fn picker_paints_os_icon() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut h = tile_host("alma", true);
        h.os = Some(alma("8.10"));
        let hosts = vec![h, tile_host("sem so", false)];
        let size = egui::vec2(1000.0, 700.0);
        for i in 0..3 {
            picker_frame(&ctx, &hosts, size, i as f64 * 0.1, vec![]);
        }
        // So o que foi pintado fica registrado no contexto (include_bytes).
        let hint = egui::SizeHint::Size(19, 19);
        let loads = |uri: &str| {
            matches!(ctx.try_load_image(uri, hint), Ok(egui::load::ImagePoll::Ready { .. }))
        };
        assert!(loads("bytes://../assets/os/almalinux.svg"), "icone do SO nao pintado");
        assert!(loads("bytes://../assets/server.svg"), "icone do servidor nao pintado");
        assert!(!loads("bytes://../assets/os/ubuntu.svg"), "icone que nao foi pintado");
    }

    // --- Tipos e links simbolicos na listagem SFTP (1.1.0) -----------------
    //
    // Quadros com o Windows no modo claro (system_theme Light), como o do
    // usuario: o app forca o tema escuro.

    fn light_ctx() -> egui::Context {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        apply_dark_theme(&ctx);
        ctx
    }

    fn light_raw(size: egui::Vec2, events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            events,
            focused: true,
            system_theme: Some(egui::Theme::Light),
            ..Default::default()
        }
    }

    /// Quadro do navegador sozinho, em foco (900x600).
    fn nav_frame(
        ctx: &egui::Context,
        e: &mut FileExplorer,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, ExplorerOut) {
        let mut out = None;
        let full = ctx.run(light_raw(egui::vec2(900.0, 600.0), events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                out = Some(e.ui(ui, "exp", true, DlAvail::Ready));
            });
        });
        (full, out.unwrap())
    }

    /// Quadro completo da sessao (mesma ordem de update(), com o
    /// `raw_input_hook` antes, como no eframe), no modo claro.
    fn nav_app_frame(ctx: &egui::Context, app: &mut App, events: Vec<egui::Event>) -> egui::FullOutput {
        let mut raw = light_raw(egui::vec2(1200.0, 700.0), events);
        eframe::App::raw_input_hook(app, ctx, &mut raw);
        ctx.run(raw, |ctx| {
            app.guard_host_key_keys(ctx);
            app.handle_file_drop(ctx);
            app.handle_session_keys(ctx);
            app.drain_ssh_events();
            app.guard_host_key_keys(ctx);
            app.handle_help_keys(ctx);
            egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
            app.ui_host_key_prompt(ctx);
        })
    }

    fn remote_dir(name: &str) -> sftp::RemoteEntry {
        sftp::RemoteEntry {
            kind: sftp::EntryKind::Dir,
            ..remote_entry(name)
        }
    }

    /// Link simbolico para `target`, com o destino no estado dado (modo so
    /// quando resolvido, como o backend manda).
    fn link_node(name: &str, kind: sftp::EntryKind, state: sftp::LinkState, target: &str) -> FsNode {
        FsNode {
            kind,
            link: Some(sftp::LinkInfo {
                target: target.into(),
                state,
            }),
            mode: (state == sftp::LinkState::Ok).then_some(0o640),
            ..fs_node(name)
        }
    }

    fn special_node(name: &str, s: sftp::Special) -> FsNode {
        FsNode {
            kind: sftp::EntryKind::Special(s),
            ..fs_node(name)
        }
    }

    fn unknown_node(name: &str) -> FsNode {
        FsNode {
            kind: sftp::EntryKind::Unknown,
            mode: None,
            ..fs_node(name)
        }
    }

    /// Cor com que o texto `t` foi pintado (a ultima vez no quadro).
    fn text_color(out: &egui::FullOutput, t: &str) -> Option<egui::Color32> {
        out.shapes.iter().rev().find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(g) if g.galley.text() == t => {
                g.galley.job.sections.first().map(|sec| sec.format.color)
            }
            _ => None,
        })
    }

    #[test]
    fn row_look_for_each_kind() {
        use sftp::{EntryKind, LinkState, Special};
        let folder = "bytes://../assets/folder.svg";
        let folder_link = "bytes://../assets/folder-symlink.svg";
        let file = "bytes://../assets/file.svg";
        let file_link = "bytes://../assets/file-symlink.svg";
        let cases = [
            (fs_node("pasta"), folder, FOLDER_FG, true),
            (link_node("www", EntryKind::Dir, LinkState::Ok, "public_html"), folder_link, FOLDER_FG, true),
            (fs_node("a.txt"), file, CARD_TEXT, false),
            (link_node("l", EntryKind::File, LinkState::Ok, "a.txt"), file_link, CARD_TEXT, false),
            (special_node("fifo", Special::Fifo), file, TEXT_WEAK, false),
            (link_node("null", EntryKind::Special(Special::CharDev), LinkState::Ok, "/dev/null"), file_link, TEXT_WEAK, false),
            (link_node("q", EntryKind::Unknown, LinkState::Broken, "x"), file_link, DANGER, false),
            (link_node("d", EntryKind::Unknown, LinkState::Denied, "x"), file_link, TEXT_WEAK, false),
            (link_node("n", EntryKind::Unknown, LinkState::Unchecked, ""), file_link, TEXT_WEAK, false),
            (unknown_node("u"), file, TEXT_WEAK, false),
        ];
        for (node, icon, color, slash) in cases {
            let look = row_look(&node);
            assert_eq!(look.icon.uri(), Some(icon), "{}", node.name);
            assert_eq!(look.color, color, "{}", node.name);
            assert_eq!(look.slash, slash, "{}", node.name);
        }
        // Dicas: pasta e arquivo comuns nao tem; os demais dizem o tipo.
        assert_eq!(row_tip(&fs_node("pasta")), None);
        assert_eq!(row_tip(&fs_node("a.txt")), None);
        assert_eq!(row_tip(&special_node("s", Special::Socket)).as_deref(), Some("Socket"));
        assert_eq!(
            row_tip(&unknown_node("u")).as_deref(),
            Some("Tipo não informado pelo servidor")
        );
        assert_eq!(
            row_tip(&link_node("www", EntryKind::Dir, LinkState::Ok, "public_html")).as_deref(),
            Some("Link simbólico para pasta\n\u{2192} public_html\nColunas: atributos do destino")
        );
        assert_eq!(
            row_tip(&link_node("l", EntryKind::File, LinkState::Ok, "a.txt")).as_deref(),
            Some("Link simbólico para arquivo\n\u{2192} a.txt\nColunas: atributos do destino")
        );
        assert_eq!(
            row_tip(&link_node("q", EntryKind::Unknown, LinkState::Broken, "/x")).as_deref(),
            Some("Link quebrado: o destino não existe (ou os links formam um ciclo)\n\u{2192} /x")
        );
        assert_eq!(
            row_tip(&link_node("d", EntryKind::Unknown, LinkState::Denied, "/x")).as_deref(),
            Some("Link simbólico: sem permissão para acessar o destino\n\u{2192} /x")
        );
        assert_eq!(
            row_tip(&link_node("n", EntryKind::Unknown, LinkState::Unchecked, "")).as_deref(),
            Some("Link simbólico (destino não verificado)")
        );
        let dev = link_node("null", EntryKind::Special(Special::CharDev), LinkState::Ok, "/dev/null");
        assert!(row_tip(&dev).unwrap().starts_with("Link simbólico para dispositivo de caracteres\n"));
    }

    #[test]
    fn link_tooltip_target_is_neutralized() {
        use sftp::{EntryKind, LinkState};
        let evil = "a\u{202E}gpj.exe\u{1b}[2J\nfalso";
        for state in [LinkState::Ok, LinkState::Broken, LinkState::Denied, LinkState::Unchecked] {
            let kind = if state == LinkState::Ok { EntryKind::File } else { EntryKind::Unknown };
            let tip = row_tip(&link_node("l", kind, state, evil)).unwrap();
            for bad in ['\u{202E}', '\u{1b}'] {
                assert!(!tip.contains(bad), "{state:?}: {tip:?}");
            }
            let alvo = tip.lines().find(|l| l.starts_with('\u{2192}')).expect("linha do alvo");
            assert_eq!(alvo, "\u{2192} a<U+202E>gpj.exe^[[2J^Jfalso", "{state:?}");
        }
        // Alvo longo: cortado em 200 caracteres (com "…").
        let long = "d/".repeat(600);
        let tip = row_tip(&link_node("l", EntryKind::Unknown, LinkState::Broken, &long)).unwrap();
        let alvo = tip.lines().find(|l| l.starts_with('\u{2192}')).unwrap();
        assert_eq!(alvo.chars().count(), 2 + 200 + 1);
        assert!(alvo.ends_with('\u{2026}'));
        // O nome exibido tambem sai neutralizado (calculado na listagem).
        let mut e = explorer_with(&[]);
        e.apply_listing("/srv", vec![remote_entry("foto\u{202E}gpj.exe")]);
        assert_eq!(e.entries[0].label, "foto<U+202E>gpj.exe");
        assert_eq!(e.entries[0].name, "foto\u{202E}gpj.exe");
    }

    #[test]
    fn open_action_by_kind() {
        use sftp::{EntryKind, LinkState, Special};
        assert_eq!(open_action(&fs_node("pasta")), OpenAction::Navigate("/srv/pasta".into()));
        // Link para pasta: caminho logico (o do link, nunca o destino).
        let www = link_node("www", EntryKind::Dir, LinkState::Ok, "/home/u/public_html");
        assert_eq!(open_action(&www), OpenAction::Navigate("/srv/www".into()));
        assert_eq!(open_action(&fs_node("a.txt")), OpenAction::View);
        assert_eq!(
            open_action(&link_node("l", EntryKind::File, LinkState::Ok, "a.txt")),
            OpenAction::View
        );
        assert_eq!(
            open_action(&special_node("fifo", Special::Fifo)),
            OpenAction::Warn(warn_special("fifo"))
        );
        assert_eq!(
            open_action(&link_node("null", EntryKind::Special(Special::CharDev), LinkState::Ok, "/dev/null")),
            OpenAction::Warn(warn_special("null"))
        );
        assert_eq!(
            open_action(&link_node("q", EntryKind::Unknown, LinkState::Broken, "x")),
            OpenAction::Warn(warn_broken_link("q"))
        );
        assert_eq!(
            open_action(&link_node("d", EntryKind::Unknown, LinkState::Denied, "x")),
            OpenAction::Warn(warn_link_denied("d"))
        );
        // Nao verificado ou sem tipo: a leitura decide.
        assert_eq!(
            open_action(&link_node("n", EntryKind::Unknown, LinkState::Unchecked, "")),
            OpenAction::View
        );
        assert_eq!(open_action(&unknown_node("u")), OpenAction::View);
        // Textos (secao 9.3), com o nome neutralizado.
        assert_eq!(
            warn_special("f"),
            "\u{201C}f\u{201D} é um arquivo especial (fifo, socket ou dispositivo) e não pode ser visualizado."
        );
        assert_eq!(
            warn_broken_link("q"),
            "\u{201C}q\u{201D} é um link simbólico quebrado: o destino não existe (ou os links formam um ciclo)."
        );
        assert_eq!(warn_link_denied("d"), "Sem permissão para acessar o destino do link \u{201C}d\u{201D}.");
        let w = warn_broken_link("x\u{202E}y");
        assert!(!w.contains('\u{202E}') && w.contains("x<U+202E>y"), "{w}");
        assert!(warn_special(&"n".repeat(500)).contains(&format!("{}\u{2026}\u{201D}", "n".repeat(80))));
    }

    /// Mensagens de exclusao que a UI mandou a sessao: (caminho, is_dir).
    fn remove_requests(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiToSftp>) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let UiToSftp::Remove { path, is_dir, .. } = m {
                out.push((path, is_dir));
            }
        }
        out
    }

    /// Delete no item `idx` do painel e confirma no dialogo; devolve os
    /// textos pintados no dialogo.
    fn delete_and_confirm(ctx: &egui::Context, app: &mut App, idx: usize) -> Vec<String> {
        explorer0_mut(app).click(idx, false, false);
        nav_app_frame(ctx, app, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        assert!(explorer0_mut(app).dialog.is_some(), "Delete nao abriu o dialogo");
        let mut out = nav_app_frame(ctx, app, vec![]);
        for _ in 0..2 {
            out = nav_app_frame(ctx, app, vec![]);
        }
        let texts: Vec<String> = painted_texts(&out).into_iter().map(|(s, _)| s).collect();
        // O titulo da janela tambem e "Excluir": o botao e o mais abaixo.
        let at = painted_texts(&out)
            .into_iter()
            .filter(|(s, _)| s == "Excluir")
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.y.total_cmp(&b.y))
            .expect("botao Excluir");
        nav_app_frame(ctx, app, vec![egui::Event::PointerMoved(at), click(at, true)]);
        nav_app_frame(ctx, app, vec![click(at, false)]);
        assert!(explorer0_mut(app).dialog.is_none(), "o dialogo continuou aberto");
        texts
    }

    #[test]
    fn delete_link_to_dir_removes_only_link() {
        use sftp::{EntryKind, LinkState};
        let ctx = light_ctx();
        let (mut app, mut to_rx, _tx) = sftp_app(&[]);
        explorer0_mut(&mut app).entries = vec![
            fs_node("pasta"),
            link_node("www", EntryKind::Dir, LinkState::Ok, "public_html"),
            link_node("q", EntryKind::Unknown, LinkState::Broken, "/nao/existe"),
        ];
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        // O dialogo do link diz o que acontece (so o link sai).
        explorer0_mut(&mut app).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Delete, egui::Modifiers::NONE)]);
        match &explorer0_mut(&mut app).dialog {
            Some(FsDialog::Delete {
                is_dir, link_target, ..
            }) => {
                assert!(!is_dir, "link para pasta iria para o rmdir");
                assert_eq!(link_target.as_deref(), Some("public_html"));
            }
            _ => panic!("Delete nao abriu o dialogo de exclusao"),
        }
        explorer0_mut(&mut app).dialog = None;
        let texts = delete_and_confirm(&ctx, &mut app, 1);
        for want in [
            "Excluir o link:",
            "www",
            "\u{2192} public_html",
            "Só o link é removido; o destino não é alterado.",
            "Esta ação é permanente.",
        ] {
            assert!(texts.iter().any(|t| t == want), "{want:?} em {texts:?}");
        }
        assert_eq!(remove_requests(&mut to_rx), [("/srv/www".to_string(), false)]);
        // FsOp tambem sem rmdir.
        let d = FsDialog::delete(&link_node("www", EntryKind::Dir, LinkState::Ok, "p"));
        assert!(matches!(d, FsDialog::Delete { is_dir: false, .. }));
        // Link quebrado: tambem remove (sem rmdir).
        let _ = delete_and_confirm(&ctx, &mut app, 2);
        assert_eq!(remove_requests(&mut to_rx), [("/srv/q".to_string(), false)]);
        // Regressao: pasta de verdade continua com rmdir.
        let texts = delete_and_confirm(&ctx, &mut app, 0);
        assert!(texts.iter().any(|t| t == "Excluir a pasta:"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Só o link")), "{texts:?}");
        assert_eq!(remove_requests(&mut to_rx), [("/srv/pasta".to_string(), true)]);
    }

    #[test]
    fn chmod_on_links() {
        use sftp::{EntryKind, LinkState};
        for state in [LinkState::Broken, LinkState::Denied, LinkState::Unchecked] {
            assert!(!attrs_editable(&link_node("l", EntryKind::Unknown, state, "x")), "{state:?}");
        }
        assert!(attrs_editable(&link_node("l", EntryKind::File, LinkState::Ok, "x")));
        assert!(attrs_editable(&fs_node("a.txt")));
        assert!(!attrs_editable(&unknown_node("u")), "sem modo conhecido");
        // Pelo menu: link resolvido parte do modo do destino (0640).
        let ctx = light_ctx();
        let mut e = explorer_with(&[]);
        e.entries = vec![
            link_node("l", EntryKind::File, LinkState::Ok, "alvo.txt"),
            link_node("q", EntryKind::Unknown, LinkState::Broken, "x"),
        ];
        let _ = context_menu_click(&ctx, &mut e, "l", "Permissões");
        match &e.dialog {
            Some(FsDialog::Chmod {
                mode,
                mode_text,
                link_target,
                ..
            }) => {
                assert_eq!((*mode, mode_text.as_str()), (0o640, "0640"));
                assert_eq!(link_target.as_deref(), Some("alvo.txt"));
            }
            _ => panic!("Permissoes nao abriu"),
        }
        // A nota aparece no dialogo.
        let mut out = explorer_frame_out(&ctx, &mut e, vec![]);
        for _ in 0..2 {
            out = explorer_frame_out(&ctx, &mut e, vec![]);
        }
        let note = "É um link simbólico: a alteração vale para o destino (\u{2192} alvo.txt).";
        assert!(painted_texts(&out).iter().any(|(t, _)| t == note));
        e.dialog = None;
        let _ = context_menu_click(&ctx, &mut e, "l", "Proprietário/Grupo");
        assert!(
            matches!(&e.dialog, Some(FsDialog::Chown { link_target: Some(t), .. }) if t == "alvo.txt")
        );
        e.dialog = None;
        // Link quebrado: Permissoes e Proprietario desativados.
        let _ = context_menu_click(&ctx, &mut e, "q", "Permissões");
        assert!(e.dialog.is_none(), "Permissoes agiu num link quebrado");
        let _ = context_menu_click(&ctx, &mut e, "q", "Proprietário/Grupo");
        assert!(e.dialog.is_none(), "Proprietario agiu num link quebrado");
    }

    #[test]
    fn apply_listing_keeps_kind_and_link() {
        use sftp::{EntryKind, LinkInfo, LinkState};
        let mut e = explorer_with(&[]);
        let link = LinkInfo {
            target: "public_html".into(),
            state: LinkState::Ok,
        };
        e.apply_listing(
            "/srv",
            vec![
                sftp::RemoteEntry {
                    link: Some(link.clone()),
                    mode: Some(0o750),
                    ..remote_dir("www")
                },
                sftp::RemoteEntry {
                    kind: EntryKind::Unknown,
                    link: Some(LinkInfo {
                        target: "x".into(),
                        state: LinkState::Broken,
                    }),
                    mode: None,
                    size: 0,
                    ..remote_entry("q")
                },
            ],
        );
        let www = &e.entries[0];
        assert_eq!((www.kind, www.link.as_ref(), www.mode), (EntryKind::Dir, Some(&link), Some(0o750)));
        assert!(www.is_dir() && !www.is_real_dir());
        let q = &e.entries[1];
        assert_eq!((q.kind, q.mode), (EntryKind::Unknown, None));
        assert_eq!(q.link.as_ref().map(|l| l.state), Some(LinkState::Broken));
        assert!(!q.is_dir());
    }

    /// Croma simples (max - min dos canais).
    fn chroma(c: egui::Color32) -> u8 {
        let (r, g, b) = (c.r(), c.g(), c.b());
        r.max(g).max(b) - r.min(g).min(b)
    }

    #[test]
    fn folder_color_contrast_and_distinct() {
        assert!(wcag_contrast(FOLDER_FG, SCREEN_BG) >= 7.0);
        // Linha selecionada: ACCENT a 30% sobre o fundo.
        let mix = |a: u8, b: u8| (a as f32 * 0.30 + b as f32 * 0.70).round() as u8;
        let sel = egui::Color32::from_rgb(
            mix(ACCENT.r(), SCREEN_BG.r()),
            mix(ACCENT.g(), SCREEN_BG.g()),
            mix(ACCENT.b(), SCREEN_BG.b()),
        );
        assert!(wcag_contrast(FOLDER_FG, sel) >= 4.5, "{:.2}", wcag_contrast(FOLDER_FG, sel));
        // Pasta tem cor; arquivo e neutro.
        assert!(chroma(FOLDER_FG) >= 60, "{}", chroma(FOLDER_FG));
        assert!(chroma(CARD_TEXT) <= 10, "{}", chroma(CARD_TEXT));
        assert_ne!(FOLDER_FG, CARD_TEXT);
    }

    const SYMLINK_ICONS: [(&str, egui::ImageSource<'static>); 2] = [
        ("folder-symlink", ICON_FOLDER_SYMLINK),
        ("file-symlink", ICON_FILE_SYMLINK),
    ];

    /// Mesmo formato dos outros Lucide: traco branco (aceita o tint), sem
    /// script, link externo nem estilo, e pequeno.
    #[test]
    fn symlink_icons_are_safe_white_svgs() {
        for (name, img) in SYMLINK_ICONS {
            let (uri, bytes) = icon_bytes(&img);
            assert_eq!(uri, format!("bytes://../assets/{name}.svg"));
            let text = std::str::from_utf8(bytes).expect("SVG em UTF-8");
            assert!(text.starts_with("<svg "), "{name}");
            assert!(text.ends_with("</svg>"), "{name}");
            assert!(text.contains("stroke=\"#ffffff\""), "{name}");
            for bad in ["currentColor", "script", "href", "style", "<!", "\r", "\n"] {
                assert!(!text.contains(bad), "{name}: {bad:?}");
            }
            assert!(bytes.len() < 2 * 1024, "{name}: {} bytes", bytes.len());
        }
    }

    #[test]
    fn symlink_icons_render_white() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        for (name, img) in SYMLINK_ICONS {
            let (uri, bytes) = icon_bytes(&img);
            assert!(egui_extras::image::load_svg_bytes(bytes).is_ok(), "{name}");
            ctx.include_bytes(uri.clone(), bytes);
            // 16 px da linha numa tela a 200%.
            let hint = egui::SizeHint::Size(32, 32);
            let image = match ctx.try_load_image(&uri, hint) {
                Ok(egui::load::ImagePoll::Ready { image }) => image,
                other => panic!("{name}: {:?}", other.map(|_| "pendente")),
            };
            assert_eq!(image.size, [32, 32], "{name}");
            let opaque: Vec<_> = image.pixels.iter().filter(|p| p.a() == 255).collect();
            assert!(!opaque.is_empty(), "{name}: nada opaco");
            assert!(
                opaque.iter().all(|p| p.r() >= 250 && p.g() >= 250 && p.b() >= 250),
                "{name}: pixel opaco que nao e branco"
            );
        }
    }

    /// Link para pasta junto das pastas (a ordem vem do backend), com o icone
    /// de link, a cor ambar e a "/"; arquivo neutro e sem "/".
    #[test]
    fn listing_shows_dir_links_with_folders() {
        use sftp::{LinkInfo, LinkState};
        let ctx = light_ctx();
        let mut e = explorer_with(&[]);
        e.apply_listing(
            "/srv",
            vec![
                remote_dir("pasta-a"),
                sftp::RemoteEntry {
                    link: Some(LinkInfo {
                        target: "public_html".into(),
                        state: LinkState::Ok,
                    }),
                    ..remote_dir("www")
                },
                remote_entry("z.txt"),
            ],
        );
        let mut out = nav_frame(&ctx, &mut e, vec![]).0;
        for _ in 0..2 {
            out = nav_frame(&ctx, &mut e, vec![]).0;
        }
        let pos = |t: &str| text_pos(&out, t).unwrap_or_else(|| panic!("{t:?} nao pintado"));
        assert!(pos("pasta-a/").y < pos("www/").y && pos("www/").y < pos("z.txt").y);
        assert_eq!(text_color(&out, "www/"), Some(FOLDER_FG));
        assert_eq!(text_color(&out, "pasta-a/"), Some(FOLDER_FG));
        assert_eq!(text_color(&out, "z.txt"), Some(CARD_TEXT));
        assert_eq!(text_color(&out, ".."), Some(FOLDER_FG));
        assert!(text_pos(&out, "www").is_none() && text_pos(&out, "z.txt/").is_none());
        // So o que foi pintado fica registrado no contexto (include_bytes).
        let hint = egui::SizeHint::Size(16, 16);
        let loads = |uri: &str| {
            matches!(ctx.try_load_image(uri, hint), Ok(egui::load::ImagePoll::Ready { .. }))
        };
        assert!(loads("bytes://../assets/folder-symlink.svg"), "icone de link para pasta");
        assert!(loads("bytes://../assets/folder.svg"));
        assert!(!loads("bytes://../assets/file-symlink.svg"), "nenhum link para arquivo");
        // Dica com o alvo ao passar o mouse sobre o link.
        let at = pos("www/");
        let mut out = nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at)]).0;
        for i in 0..40 {
            let raw_time = egui::Event::PointerMoved(at + egui::vec2(0.0, (i % 2) as f32 * 0.1));
            out = nav_frame(&ctx, &mut e, vec![raw_time]).0;
            if painted_texts(&out).iter().any(|(t, _)| t.contains("public_html")) {
                break;
            }
        }
        let tip = "Link simbólico para pasta\n\u{2192} public_html\nColunas: atributos do destino";
        assert!(
            painted_texts(&out).iter().any(|(t, _)| t == tip),
            "{:?}",
            painted_texts(&out)
        );
    }

    /// Enter (ou duplo clique) num link quebrado: aviso ambar, nada de
    /// listagem; numa pasta, entra e o aviso some.
    #[test]
    fn enter_on_broken_link_shows_notice() {
        use sftp::{EntryKind, LinkState};
        let ctx = light_ctx();
        let (mut app, mut to_rx, _tx) = sftp_app(&[]);
        explorer0_mut(&mut app).entries = vec![
            fs_node("pasta"),
            link_node("q", EntryKind::Unknown, LinkState::Broken, "/nao/existe"),
            fs_node("a.txt"),
        ];
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let enter = || key(egui::Key::Enter, egui::Modifiers::NONE);
        explorer0_mut(&mut app).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let want = warn_broken_link("q");
        assert_eq!(explorer0_mut(&mut app).notice.as_deref(), Some(want.as_str()));
        assert!(painted_texts(&out).iter().any(|(t, _)| *t == want), "aviso nao pintado");
        assert_eq!(text_color(&out, &want), Some(HIGHLIGHT));
        assert!(sftp_msgs_list(&mut to_rx).is_empty(), "link quebrado pediu listagem");
        // Arquivo: abre no visualizador (pede a leitura), sem listagem.
        explorer0_mut(&mut app).click(2, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert!(sftp_msgs_list(&mut to_rx).is_empty());
        // Pasta: entra, e o aviso some.
        explorer0_mut(&mut app).click(0, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs_list(&mut to_rx), ["/srv/pasta"]);
        assert!(explorer0_mut(&mut app).notice.is_none());

        // Duplo clique no link quebrado: o mesmo aviso.
        let mut e = explorer_with(&[]);
        e.entries = vec![link_node("q", EntryKind::Unknown, LinkState::Broken, "/x")];
        let out = nav_frame(&ctx, &mut e, vec![]).0;
        let at = text_pos(&out, "q").expect("linha do link");
        nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at), click(at, true)]);
        nav_frame(&ctx, &mut e, vec![click(at, false)]);
        nav_frame(&ctx, &mut e, vec![click(at, true)]);
        let (_, out) = nav_frame(&ctx, &mut e, vec![click(at, false)]);
        assert!(out.to_list.is_empty());
        assert_eq!(e.notice.as_deref(), Some(warn_broken_link("q").as_str()));
        // Duplo clique numa pasta (link para pasta): entra. Contexto novo:
        // no mesmo, os cliques de antes contariam como clique triplo.
        let ctx = light_ctx();
        let mut e = explorer_with(&[]);
        e.entries = vec![link_node("www", EntryKind::Dir, LinkState::Ok, "public_html")];
        let out = nav_frame(&ctx, &mut e, vec![]).0;
        let at = text_pos(&out, "www/").expect("linha do link");
        nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at), click(at, true)]);
        nav_frame(&ctx, &mut e, vec![click(at, false)]);
        nav_frame(&ctx, &mut e, vec![click(at, true)]);
        let (_, out) = nav_frame(&ctx, &mut e, vec![click(at, false)]);
        assert_eq!(out.to_list, ["/srv/www"]);
    }

    /// Caminhos das listagens pedidas a sessao (ignora o resto).
    fn sftp_msgs_list(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiToSftp>) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            if let UiToSftp::ListDir(p) = m {
                v.push(p);
            }
        }
        v
    }

    #[test]
    fn help_lists_sftp_legend_and_interface_credits() {
        assert!(HELP_ICON_CREDITS.contains("Lucide (ISC) e Feather (MIT)"));
        let ctx = light_ctx();
        let mut app = app();
        app.show_help = true;
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 1600.0));
        let mut out = None;
        for i in 0..4 {
            let raw = egui::RawInput {
                time: Some(i as f64 * 0.1),
                ..light_raw(screen.size(), vec![])
            };
            out = Some(ctx.run(raw, |ctx| app.ui_help(ctx)));
        }
        let texts = painted_galleys(out.as_ref().unwrap());
        let (vis, _, _) = texts
            .iter()
            .find(|(_, full, _)| full == HELP_SFTP_LEGEND)
            .unwrap_or_else(|| panic!("legenda do SFTP: {texts:?}"));
        let sem_espaco = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(sem_espaco(vis), sem_espaco(HELP_SFTP_LEGEND), "legenda cortada");
    }

    // --- Navegador SFTP: "..", PageUp/PageDown, busca por letras e barra ---
    //
    // Quadros com o Windows no modo claro (system_theme Light), como o do
    // usuario: o app forca o tema escuro.

    /// Ctrl como o egui-winit manda no Windows (ctrl e command juntos).
    fn ctrl_win() -> egui::Modifiers {
        egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        }
    }

    /// Nome sob o cursor ("..", a entrada, ou `None` sem cursor).
    fn cursor_name(e: &FileExplorer) -> Option<String> {
        if e.on_up {
            return Some("..".into());
        }
        e.sel.map(|s| e.entries[s].name.clone())
    }

    fn marked_names(e: &FileExplorer) -> Vec<String> {
        e.marked.iter().cloned().collect()
    }

    fn explorer_at<'a>(app: &'a mut App, path: &[usize]) -> &'a mut FileExplorer {
        match app.root.as_mut().and_then(|r| node_at_mut(r, path)) {
            Some(Node::Leaf(p)) => p.explorer.as_mut().expect("painel sem navegador"),
            _ => panic!("sem painel em {path:?}"),
        }
    }

    /// Quadros ociosos ate a rolagem animada do egui (ate 0,3 s) terminar.
    fn settle(ctx: &egui::Context, e: &mut FileExplorer) -> egui::FullOutput {
        for _ in 0..30 {
            nav_frame(ctx, e, vec![]);
        }
        nav_frame(ctx, e, vec![]).0
    }

    /// Textos pintados inteiros dentro da area visivel (recorte) de cada forma.
    fn visible_texts(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => {
                    let r = egui::Rect::from_min_size(t.pos, t.galley.size());
                    s.clip_rect
                        .contains_rect(r)
                        .then(|| (t.galley.text().to_string(), r))
                }
                _ => None,
            })
            .collect()
    }

    /// Listagens e caminhos digitados que a UI mandou a sessao.
    fn sftp_msgs(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiToSftp>) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            match m {
                UiToSftp::ListDir(p) => v.push(format!("list {p}")),
                UiToSftp::Goto { seq, path } => v.push(format!("goto {seq} {path}")),
                _ => v.push("outro".into()),
            }
        }
        v
    }

    /// A ".." e uma linha do cursor: setas chegam nela, nada fica marcado
    /// nela e ela nunca entra em Ctrl+A, download, F2 ou Delete.
    #[test]
    fn up_row_is_a_cursor_row_never_marked() {
        let mut e = explorer_with(&[]);
        let mut to_list = Vec::new();
        e.navigate_to("/srv".into(), &mut to_list);
        e.apply_listing("/srv", vec![remote_entry("a.txt"), remote_entry("b.txt")]);
        // Ao entrar numa pasta, o cursor comeca na "..".
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert_eq!(e.row_count(), 3);
        assert!(e.marked.is_empty() && e.picks().is_empty() && e.single_target().is_none());
        e.move_cursor(1, false);
        assert_eq!(cursor_name(&e).as_deref(), Some("a.txt"));
        assert_eq!(marked_names(&e), ["a.txt"]);
        e.move_cursor(-1, false);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert!(e.marked.is_empty(), "a \"..\" desmarca como uma seta");
        e.move_cursor(-1, false);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."), "para na primeira linha");
        e.move_cursor(5, false);
        assert_eq!(cursor_name(&e).as_deref(), Some("b.txt"), "para na ultima linha");
        e.move_cursor(-5, false);
        // Ctrl+A com o cursor na "..": marca so as entradas; F2/Delete sem alvo.
        e.select_all();
        assert_eq!(pick_names(&e), ["a.txt", "b.txt"]);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert_eq!(e.single_target(), None);
        // Pelo teclado: Ctrl+A, F2 e Delete na ".." nao abrem dialogo.
        let ctx = light_ctx();
        e.marked.clear();
        nav_frame(&ctx, &mut e, vec![key(egui::Key::A, ctrl_win())]);
        assert_eq!(pick_names(&e), ["a.txt", "b.txt"]);
        e.marked = ["a.txt".to_string()].into();
        for k in [egui::Key::F2, egui::Key::Delete] {
            nav_frame(&ctx, &mut e, vec![key(k, egui::Modifiers::NONE)]);
            assert!(e.dialog.is_none(), "{k:?} agiu na \"..\"");
        }
        // Shift a partir da "..": o intervalo comeca na primeira entrada.
        e.click_up(false);
        e.move_cursor(2, true);
        assert_eq!(marked_names(&e), ["a.txt", "b.txt"]);
        // Shift voltando ate a "..": a ".." fica fora do intervalo.
        e.set_cursor_row(2, false);
        e.move_cursor(-2, true);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert_eq!(marked_names(&e), ["a.txt", "b.txt"]);
        // Ctrl+clique na "..": so o cursor vai; clique simples desmarca.
        e.click_up(true);
        assert_eq!(marked_names(&e), ["a.txt", "b.txt"]);
        e.click_up(false);
        assert!(e.marked.is_empty());
        // Pelo mouse: Ctrl+clique na ".." (quadro real) mantem a selecao.
        let ctx = light_ctx();
        e.click(1, false, false);
        let full = nav_frame(&ctx, &mut e, vec![]).0;
        let at = text_pos(&full, "..").expect("\"..\" nao pintada");
        let ctrl_click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: ctrl_win(),
        };
        let mut raw = light_raw(egui::vec2(900.0, 600.0), vec![egui::Event::PointerMoved(at), ctrl_click(true)]);
        raw.modifiers = ctrl_win();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| e.ui(ui, "exp", true, DlAvail::Ready));
        });
        let mut raw = light_raw(egui::vec2(900.0, 600.0), vec![ctrl_click(false)]);
        raw.modifiers = ctrl_win();
        let _ = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| e.ui(ui, "exp", true, DlAvail::Ready));
        });
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert_eq!(marked_names(&e), ["b.txt"]);
        // Na raiz nao ha "..".
        e.navigate_to("/".into(), &mut to_list);
        e.apply_listing("/", vec![remote_dir("etc"), remote_dir("srv")]);
        assert_eq!(e.row_count(), 2);
        assert_eq!(e.cursor_row(), None);
        e.click_up(false);
        assert_eq!(e.cursor_row(), None, "clique na \"..\" inexistente");
        e.move_cursor(1, false);
        assert_eq!(cursor_name(&e).as_deref(), Some("etc"));
        let full = nav_frame(&ctx, &mut e, vec![]).0;
        assert!(text_pos(&full, "..").is_none(), "\"..\" na raiz");
    }

    /// Enter e duplo clique na "..", e Backspace, sobem; a pasta de onde se
    /// saiu volta sob o cursor (selecionada e visivel) quando a listagem
    /// chega. Enter num arquivo nao navega.
    #[test]
    fn enter_double_click_and_backspace_go_up() {
        let ctx = light_ctx();
        let mut e = explorer_with(&[]);
        let mut to_list = Vec::new();
        e.navigate_to("/var/www".into(), &mut to_list);
        e.apply_listing("/var/www", vec![remote_dir("html"), remote_entry("a.txt")]);
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert_eq!(out.to_list, ["/var"]);
        assert_eq!(e.cur_path, "/var");
        // Muitas pastas: "www" fica fora da vista inicial e a vista rola.
        let mut novos: Vec<sftp::RemoteEntry> =
            (0..60).map(|i| remote_dir(&format!("d{i:02}"))).collect();
        novos.push(remote_dir("www"));
        novos.push(remote_entry("x"));
        e.apply_listing("/var", novos);
        assert_eq!(cursor_name(&e).as_deref(), Some("www"));
        assert_eq!(marked_names(&e), ["www"]);
        let full = settle(&ctx, &mut e);
        assert!(visible_texts(&full).iter().any(|(t, _)| t == "www/"), "www fora da vista");
        assert!(!e.scroll_to_cursor, "pedido de rolagem consumido");
        // Backspace sobe do mesmo jeito.
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        assert_eq!(out.to_list, ["/"]);
        e.apply_listing("/", vec![remote_dir("etc"), remote_dir("var")]);
        assert_eq!(cursor_name(&e).as_deref(), Some("var"));
        // Na raiz, Backspace nao faz nada.
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        assert!(out.to_list.is_empty());
        // Duplo clique na "..".
        let mut e = explorer_with(&["pasta1", "a.txt"]);
        let (full, _) = nav_frame(&ctx, &mut e, vec![]);
        let at = text_pos(&full, "..").expect("\"..\" nao pintada");
        let mut subiu = Vec::new();
        for pressed in [true, false, true, false] {
            let (_, out) = nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at), click(at, pressed)]);
            subiu.extend(out.to_list);
        }
        assert_eq!(subiu, ["/"]);
        assert_eq!(e.reselect.as_deref(), Some("srv"));
        // Enter numa pasta entra (cursor na ".." da nova pasta).
        let mut e = explorer_with(&["pasta1", "a.txt"]);
        e.set_cursor_row(1, false);
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert_eq!(out.to_list, ["/srv/pasta1"]);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        // Enter num arquivo nao navega nem pede listagem (pede a leitura
        // para o visualizador).
        let mut e = explorer_with(&["pasta1", "a.txt"]);
        e.set_cursor_row(2, false);
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert!(out.to_list.is_empty());
        assert_eq!(e.cur_path, "/srv");
        assert_eq!(cursor_name(&e).as_deref(), Some("a.txt"));
    }

    /// PageUp/PageDown andam uma pagina de linhas visiveis; Home/End vao as
    /// pontas; com Shift estendem a selecao; o cursor fica sempre visivel.
    #[test]
    fn page_home_end_move_and_extend() {
        let ctx = light_ctx();
        let nomes: Vec<String> = (0..60).map(|i| format!("f{i:02}")).collect();
        let refs: Vec<&str> = nomes.iter().map(String::as_str).collect();
        let mut e = explorer_with(&refs);
        e.on_up = true;
        nav_frame(&ctx, &mut e, vec![]);
        let page = e.page_rows;
        assert!((10..=25).contains(&page), "linhas visiveis: {page}");
        let pd = || key(egui::Key::PageDown, egui::Modifiers::NONE);
        nav_frame(&ctx, &mut e, vec![pd()]);
        assert_eq!(e.sel, Some(page - 2), "da \"..\" desce page-1 linhas");
        nav_frame(&ctx, &mut e, vec![pd()]);
        assert_eq!(e.sel, Some(2 * page - 3));
        assert_eq!(marked_names(&e).len(), 1);
        // O cursor fica visivel (a rolagem acompanha).
        let full = settle(&ctx, &mut e);
        let alvo = format!("f{:02}", 2 * page - 3);
        assert!(visible_texts(&full).iter().any(|(t, _)| *t == alvo), "{alvo} fora da vista");
        nav_frame(&ctx, &mut e, vec![key(egui::Key::End, egui::Modifiers::NONE)]);
        assert_eq!(e.sel, Some(59));
        let full = settle(&ctx, &mut e);
        assert!(visible_texts(&full).iter().any(|(t, _)| t == "f59"));
        nav_frame(&ctx, &mut e, vec![pd()]);
        assert_eq!(e.sel, Some(59), "PageDown no fim fica no fim");
        // Ctrl+Home/Ctrl+End nao sao da lista.
        nav_frame(&ctx, &mut e, vec![key(egui::Key::Home, ctrl_win())]);
        assert_eq!(e.sel, Some(59));
        nav_frame(&ctx, &mut e, vec![key(egui::Key::PageUp, egui::Modifiers::NONE)]);
        assert_eq!(e.sel, Some(59 - (page - 1)));
        nav_frame(&ctx, &mut e, vec![key(egui::Key::Home, egui::Modifiers::NONE)]);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert!(e.marked.is_empty());
        let full = settle(&ctx, &mut e);
        assert!(visible_texts(&full).iter().any(|(t, _)| t == ".."));
        nav_frame(&ctx, &mut e, vec![key(egui::Key::PageUp, egui::Modifiers::NONE)]);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        // Shift+PageDown e Shift+End a partir da "..".
        nav_frame(&ctx, &mut e, vec![key(egui::Key::PageDown, egui::Modifiers::SHIFT)]);
        assert_eq!(marked_names(&e).len(), page - 1);
        nav_frame(&ctx, &mut e, vec![key(egui::Key::End, egui::Modifiers::SHIFT)]);
        assert_eq!(marked_names(&e).len(), 60);
        nav_frame(&ctx, &mut e, vec![key(egui::Key::Home, egui::Modifiers::SHIFT)]);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert_eq!(marked_names(&e), ["f00"], "ancora no f00: como no Explorador");
        nav_frame(&ctx, &mut e, vec![key(egui::Key::PageUp, egui::Modifiers::SHIFT)]);
        assert_eq!(marked_names(&e), ["f00"]);
        // Lista vazia: nada acontece (nem panico); na raiz, nem a "..".
        let mut vazio = explorer_with(&[]);
        nav_frame(&ctx, &mut vazio, vec![pd(), key(egui::Key::End, egui::Modifiers::SHIFT)]);
        assert_eq!(cursor_name(&vazio).as_deref(), Some(".."));
        vazio.cur_path = "/".into();
        vazio.on_up = false;
        for k in [egui::Key::PageDown, egui::Key::PageUp, egui::Key::Home, egui::Key::End] {
            nav_frame(&ctx, &mut vazio, vec![key(k, egui::Modifiers::NONE), key(k, egui::Modifiers::SHIFT)]);
        }
        assert_eq!(cursor_name(&vazio), None);
    }

    /// Busca por letras: prefixo sem maiusculas nem acentos, mesma letra
    /// alterna, prefixo zera apos 1 s, sem item nao mexe no cursor.
    #[test]
    fn type_ahead_prefix_cycle_accents_and_timeout() {
        let mut e = explorer_with(&[
            "Área", "Ábaco", "arquivo.txt", "backup", "banco.sql", "bin", "ssh", "sa", "sx",
        ]);
        e.on_up = true;
        let t0 = Instant::now();
        let ms = |n: u64| t0 + std::time::Duration::from_millis(n);
        assert!(e.type_ahead("b", ms(0)));
        assert_eq!(cursor_name(&e).as_deref(), Some("backup"));
        assert_eq!(marked_names(&e), ["backup"], "move como uma seta");
        e.type_ahead("b", ms(100));
        assert_eq!(cursor_name(&e).as_deref(), Some("banco.sql"));
        e.type_ahead("b", ms(200));
        assert_eq!(cursor_name(&e).as_deref(), Some("bin"));
        e.type_ahead("b", ms(300));
        assert_eq!(cursor_name(&e).as_deref(), Some("backup"), "da a volta");
        // Prefixo mais longo, a partir do cursor.
        e.reset_typeahead();
        e.type_ahead("ban", ms(400));
        assert_eq!(cursor_name(&e).as_deref(), Some("banco.sql"));
        assert_eq!(e.typeahead, "ban");
        // Sem acento e sem maiuscula: "ab" acha "Ábaco"; "ÁR" acha "Área".
        e.reset_typeahead();
        e.type_ahead("ab", ms(500));
        assert_eq!(cursor_name(&e).as_deref(), Some("Ábaco"));
        e.reset_typeahead();
        e.click_up(false);
        e.type_ahead("ÁR", ms(600));
        assert_eq!(cursor_name(&e).as_deref(), Some("Área"));
        // Nenhum item: cursor fica, indicador marca a falta.
        let antes = e.sel;
        assert!(!e.type_ahead("z", ms(700)));
        assert_eq!(e.sel, antes);
        assert!(e.typeahead_miss);
        assert_eq!(e.typeahead, "ÁRz", "o indicador mostra o que foi digitado");
        // Depois de 1 s sem digitar, recomeca.
        e.type_ahead("s", ms(1800));
        assert_eq!(e.typeahead, "s");
        assert_eq!(cursor_name(&e).as_deref(), Some("ssh"));
        assert!(!e.typeahead_miss);
        // "ss": ha item com "ss" (fica); "sss": nao ha, passa ao proximo "s".
        e.type_ahead("s", ms(1900));
        assert_eq!(cursor_name(&e).as_deref(), Some("ssh"));
        e.type_ahead("s", ms(2000));
        assert_eq!(cursor_name(&e).as_deref(), Some("sa"));
        // Espaco sozinho nao busca; a ".." nunca e alvo.
        e.reset_typeahead();
        e.click_up(false);
        assert!(!e.type_ahead(" ", ms(3000)));
        assert!(e.typeahead.is_empty());
        e.type_ahead(".", ms(3100));
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert!(e.typeahead_miss);
        // Compara o nome (nunca o texto exibido com "/" nem o `label`).
        let mut e = explorer_with(&["pasta1", "pasta2"]);
        assert!(e.type_ahead("pasta2", ms(0)));
        assert_eq!(cursor_name(&e).as_deref(), Some("pasta2"));
        e.reset_typeahead();
        e.type_ahead("pasta2/", ms(100));
        assert!(e.typeahead_miss);
        assert_eq!(download::fold_name("ÁÉÍÓÚ ãõç Ñ"), "aeiou aoc n");
    }

    /// Letras no painel SFTP focado vao para a busca (com indicador) e nunca
    /// para o terminal ao lado; com Ctrl/Alt ou num dialogo, nao buscam.
    #[test]
    fn typed_letters_search_list_not_terminal() {
        let ctx = light_ctx();
        let (mut app, _rx, _tx) = sftp_app(&["pasta1", "backup", "banco.sql"]);
        app.split_pane(&[], SplitDir::SideBySide);
        let (mut ssh_rx, _stx) = connecting_ssh_pane(&mut app, &[1]);
        if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, &[1])) {
            p.state = SessionState::Connected;
        }
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        app.pending_focus = Some(vec![0]);
        for _ in 0..2 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![0]));
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("ba".into())]);
        let out = nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("n".into())]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some("banco.sql"));
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "ban"), "indicador do prefixo");
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "letra vazou para o terminal");
        // Sem item: o indicador avisa (em vermelho).
        let out = nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("z".into())]);
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "banz  (nenhum item)"));
        assert_eq!(text_color(&out, "banz  (nenhum item)"), Some(ERROR_FG));
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some("banco.sql"));
        // Passado 1 s, o indicador some.
        explorer_at(&mut app, &[0]).typeahead_at =
            Instant::now().checked_sub(std::time::Duration::from_secs(2));
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(!painted_texts(&out).iter().any(|(t, _)| t.starts_with("banz")));
        assert!(explorer_at(&mut app, &[0]).typeahead.is_empty());
        // Com Ctrl (ou Alt) segurado nao busca.
        explorer_at(&mut app, &[0]).click_up(false);
        for mods in [ctrl_win(), egui::Modifiers::ALT] {
            let mut raw = light_raw(egui::vec2(1200.0, 700.0), vec![egui::Event::Text("b".into())]);
            raw.modifiers = mods;
            let _ = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
            });
            assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some(".."));
        }
        // Com um dialogo aberto, letras nao mexem na lista.
        explorer_at(&mut app, &[0]).dialog = Some(FsDialog::Delete {
            path: "/srv/backup".into(),
            name: "backup".into(),
            is_dir: false,
            link_target: None,
        });
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("b".into())]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some(".."));
        assert!(explorer_at(&mut app, &[0]).typeahead.is_empty());
        assert!(sent_bytes(&mut ssh_rx).is_empty());
    }

    /// Resolucao do texto da barra: absoluto, relativo, "~", "." e "..".
    #[test]
    fn resolve_remote_input_cases() {
        let r = |t: &str| resolve_remote_input(t, "/srv/www", "/home/u");
        let so = |p: &str| Ok(Some((p.to_string(), None)));
        assert_eq!(r("/etc"), so("/etc"));
        assert_eq!(r("/etc/"), so("/etc"));
        assert_eq!(r("html"), so("/srv/www/html"));
        assert_eq!(r("../logs"), so("/srv/logs"));
        assert_eq!(r("../../../.."), so("/"));
        assert_eq!(r("~"), so("/home/u"));
        assert_eq!(r("~/public_html"), so("/home/u/public_html"));
        assert_eq!(r("~outro"), so("/srv/www/~outro"), "so ~ e ~/ expandem");
        assert_eq!(r("/"), so("/"));
        assert_eq!(r(""), Ok(None));
        assert_eq!(r("   "), Ok(None));
        // Quebras de linha e TAB de uma colagem sempre saem.
        assert_eq!(r("/etc\r\n"), so("/etc"));
        assert_eq!(r("\t/var/www\n"), so("/var/www"));
        // Espacos nas pontas: primeiro o texto exato (um nome pode terminar
        // em espaco), depois sem eles.
        let com_alt = |p: &str, alt: &str| Ok(Some((p.to_string(), Some(alt.to_string()))));
        assert_eq!(r("/home/u/uploads "), com_alt("/home/u/uploads ", "/home/u/uploads"));
        assert_eq!(r("/etc \r\n"), com_alt("/etc ", "/etc"));
        assert_eq!(r("  /a//b/./c/  "), com_alt("/srv/www/  /a/b/c/  ", "/a/b/c"));
        assert_eq!(r(" ~"), com_alt("/srv/www/ ~", "/home/u"));
        assert_eq!(
            resolve_remote_input("~", "/srv", ""),
            Err("Pasta inicial desconhecida.".to_string())
        );
        assert!(resolve_remote_input("~/x", "/srv", "").is_err());
        assert_eq!(normalize_remote("/a/b/../../.."), "/");
        assert_eq!(normalize_remote("//"), "/");
    }

    /// Clicar em qualquer ponto da barra (no texto, a direita dele, nas
    /// bordas, no icone, mesmo arrastando um pouco) abre a edicao com o texto
    /// todo selecionado: digitar substitui.
    #[test]
    fn path_bar_click_anywhere_opens_edit_selected() {
        let base = |ctx: &egui::Context| {
            let (mut app, rx, tx) = sftp_app(&["pasta1", "a.txt"]);
            for _ in 0..3 {
                nav_app_frame(ctx, &mut app, vec![]);
            }
            (app, rx, tx)
        };
        let ctx = light_ctx();
        let (mut app, _rx, _tx) = base(&ctx);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let r = painted_texts(&out)
            .into_iter()
            .find(|(t, _)| t == "/srv")
            .expect("caminho nao pintado")
            .1;
        // O caminho e texto forte (nao mais o cinza fraco do rotulo antigo).
        assert_eq!(text_color(&out, "/srv"), Some(TEXT));
        let y = r.center().y;
        let pontos = [
            ("texto", r.center(), 0.0),
            ("direita +20", egui::pos2(r.right() + 20.0, y), 0.0),
            ("direita +300", egui::pos2(r.right() + 300.0, y), 0.0),
            ("borda de cima", egui::pos2(r.right() + 100.0, y - 10.5), 0.0),
            ("borda de baixo", egui::pos2(r.right() + 100.0, y + 10.5), 0.0),
            ("icone", egui::pos2(r.left() - 14.0, y), 0.0),
            ("arrastando 8 px", r.center(), 8.0),
            ("arrastando 40 px", egui::pos2(r.right() + 50.0, y), 40.0),
        ];
        for (nome, p, arrasto) in pontos {
            let ctx = light_ctx();
            let (mut app, _rx, _tx) = base(&ctx);
            let hover = nav_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(p)]);
            if nome != "icone" {
                assert_eq!(hover.platform_output.cursor_icon, egui::CursorIcon::Text, "{nome}");
            }
            let q = p + egui::vec2(arrasto, 0.0);
            nav_app_frame(&ctx, &mut app, vec![click(p, true)]);
            nav_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(q)]);
            nav_app_frame(&ctx, &mut app, vec![click(q, false)]);
            nav_app_frame(&ctx, &mut app, vec![]);
            nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("/etc".into())]);
            let e = explorer_at(&mut app, &[]);
            assert_eq!(
                e.path_edit.as_ref().map(|p| p.text.as_str()),
                Some("/etc"),
                "clique em {nome} ({p:?})"
            );
            assert_eq!(app.focused_path, Some(vec![]), "{nome}: painel continua o focado");
        }
    }

    /// Enter pergunta ao servidor sem sair da pasta; pasta abre, arquivo
    /// abre a pasta dele com o cursor nele, erro mantem a edicao.
    #[test]
    fn path_bar_enter_goto_dir_file_error() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["pasta1", "a.txt"]);
        explorer_at(&mut app, &[]).home = "/home/u".into();
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let enter = || key(egui::Key::Enter, egui::Modifiers::NONE);
        let digita = |ctx: &egui::Context, app: &mut App, t: &str| {
            nav_app_frame(ctx, app, vec![key(egui::Key::L, ctrl_win())]);
            nav_app_frame(ctx, app, vec![]);
            nav_app_frame(ctx, app, vec![egui::Event::Text(t.into())]);
            nav_app_frame(ctx, app, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        };
        // Pasta: nada muda ate a resposta.
        digita(&ctx, &mut app, "/etc");
        assert_eq!(sftp_msgs(&mut rx), ["goto 1 /etc"]);
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/srv");
        assert!(explorer_at(&mut app, &[]).path_edit.is_some());
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert!(sftp_msgs(&mut rx).is_empty(), "Enter repetido durante a espera");
        tx.send(SftpToUi::Goto {
            seq: 1,
            result: Ok(sftp::GotoKind::Dir),
        })
        .unwrap();
        tx.send(SftpToUi::Listing {
            path: "/etc".into(),
            entries: vec![remote_entry("hosts")],
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![]);
        {
            let e = explorer_at(&mut app, &[]);
            assert_eq!(e.cur_path, "/etc");
            assert!(e.path_edit.is_none());
            assert_eq!(cursor_name(e).as_deref(), Some(".."));
            assert_eq!(e.entries.len(), 1);
        }
        assert!(sftp_msgs(&mut rx).is_empty(), "a listagem veio junto com a resposta");
        // O teclado voltou para a lista.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some("hosts"));
        // Arquivo: abre a pasta dele, com o cursor (e a selecao) no arquivo.
        digita(&ctx, &mut app, "/var/log/messages");
        assert_eq!(sftp_msgs(&mut rx), ["goto 2 /var/log/messages"]);
        tx.send(SftpToUi::Goto {
            seq: 2,
            result: Ok(sftp::GotoKind::File),
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(sftp_msgs(&mut rx), ["list /var/log"]);
        tx.send(SftpToUi::Listing {
            path: "/var/log".into(),
            entries: vec![remote_entry("dnf.log"), remote_entry("messages")],
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some("messages"));
        assert_eq!(marked_names(explorer_at(&mut app, &[])), ["messages"]);
        assert!(explorer_at(&mut app, &[]).path_edit.is_none());
        // Erro: a edicao continua, com a mensagem, e da para corrigir.
        digita(&ctx, &mut app, "/nao/existe");
        assert_eq!(sftp_msgs(&mut rx), ["goto 3 /nao/existe"]);
        tx.send(SftpToUi::Goto {
            seq: 3,
            result: Err("Caminho não encontrado: /nao/existe".into()),
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "Caminho não encontrado: /nao/existe"));
        assert_eq!(text_color(&out, "Caminho não encontrado: /nao/existe"), Some(ERROR_FG));
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/var/log");
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("2".into())]);
        {
            let e = explorer_at(&mut app, &[]);
            let edit = e.path_edit.as_ref().expect("edicao fechou com o erro");
            assert_eq!(edit.text, "/nao/existe2", "campo segue focado, cursor no fim");
            assert!(edit.error.is_none(), "o erro some ao editar");
        }
        // "~" sem pasta inicial conhecida: erro local, nada vai ao servidor.
        explorer_at(&mut app, &[]).home.clear();
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        digita(&ctx, &mut app, "~");
        assert!(sftp_msgs(&mut rx).is_empty());
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "Pasta inicial desconhecida."));
        explorer_at(&mut app, &[]).home = "/home/u".into();
        // Relativo, "~" e o proprio caminho (so atualiza).
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[]).path_edit.is_none());
        digita(&ctx, &mut app, "../lib");
        assert_eq!(sftp_msgs(&mut rx), ["goto 4 /var/lib"]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        digita(&ctx, &mut app, "~/public_html");
        assert_eq!(sftp_msgs(&mut rx), ["goto 5 /home/u/public_html"]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        digita(&ctx, &mut app, "/var/log/");
        assert_eq!(sftp_msgs(&mut rx), ["list /var/log"]);
        assert!(explorer_at(&mut app, &[]).path_edit.is_none());
        // Resposta atrasada de um pedido cancelado (Esc) e ignorada.
        tx.send(SftpToUi::Goto {
            seq: 5,
            result: Ok(sftp::GotoKind::Dir),
        })
        .unwrap();
        tx.send(SftpToUi::Listing {
            path: "/home/u/public_html".into(),
            entries: vec![],
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/var/log");
    }

    /// Com um terminal ao lado: editar o caminho mantem o SFTP como painel
    /// focado; Enter/Esc devolvem o teclado a lista; clicar no terminal
    /// cancela a edicao e o terminal fica com o foco.
    #[test]
    fn path_edit_focus_with_terminal_beside() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["pasta1", "a.txt"]);
        app.split_pane(&[], SplitDir::SideBySide);
        let (mut ssh_rx, _stx) = connecting_ssh_pane(&mut app, &[1]);
        if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, &[1])) {
            p.state = SessionState::Connected;
        }
        app.pending_focus = Some(vec![1]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![1]));
        let click_path = |ctx: &egui::Context, app: &mut App, atual: &str| {
            let out = nav_app_frame(ctx, app, vec![]);
            let r = painted_texts(&out).into_iter().find(|(t, _)| t == atual).unwrap().1;
            let p = egui::pos2(r.right() + 60.0, r.center().y);
            nav_app_frame(ctx, app, vec![egui::Event::PointerMoved(p), click(p, true)]);
            nav_app_frame(ctx, app, vec![click(p, false)]);
            nav_app_frame(ctx, app, vec![]);
        };
        // Enter.
        click_path(&ctx, &mut app, "/srv");
        assert_eq!(app.focused_path, Some(vec![0]));
        assert!(app.session_hint().starts_with("Enter abre o caminho"));
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("/etc".into())]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Enter, egui::Modifiers::NONE)]);
        assert_eq!(sftp_msgs(&mut rx), ["goto 1 /etc"]);
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "Enter foi para o terminal");
        tx.send(SftpToUi::Goto {
            seq: 1,
            result: Ok(sftp::GotoKind::Dir),
        })
        .unwrap();
        tx.send(SftpToUi::Listing {
            path: "/etc".into(),
            entries: vec![remote_entry("hosts")],
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("h".into())]);
        assert_eq!(app.focused_path, Some(vec![0]));
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some("hosts"));
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "teclas foram para o terminal");
        // Esc.
        click_path(&ctx, &mut app, "/etc");
        assert!(explorer_at(&mut app, &[0]).path_edit.is_some());
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::ArrowUp, egui::Modifiers::NONE)]);
        assert!(explorer_at(&mut app, &[0]).path_edit.is_none());
        assert_eq!(app.focused_path, Some(vec![0]));
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some(".."));
        assert!(sent_bytes(&mut ssh_rx).is_empty());
        // Clique no terminal cancela e o terminal fica com o teclado.
        click_path(&ctx, &mut app, "/etc");
        assert!(explorer_at(&mut app, &[0]).path_edit.is_some());
        let term = egui::pos2(900.0, 400.0);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(term), click(term, true)]);
        nav_app_frame(&ctx, &mut app, vec![click(term, false)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[0]).path_edit.is_none());
        assert_eq!(app.focused_path, Some(vec![1]));
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(sent_bytes(&mut ssh_rx), b"x");
    }

    /// Ctrl+L abre a edicao com tudo selecionado; a barra de dicas mostra
    /// Enter/Esc; antes de conectar (sem caminho) a barra nao abre.
    #[test]
    fn ctrl_l_and_hint_while_editing() {
        let ctx = light_ctx();
        let (mut app, _rx, _tx) = sftp_app(&["a.txt"]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert!(!app.session_hint().starts_with("Enter abre o caminho"));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[]).path_edit.is_some());
        assert_eq!(
            app.session_hint(),
            "Enter abre o caminho  \u{00b7}  Esc cancela  \u{00b7}  ~ é a pasta inicial"
        );
        // Tudo selecionado: digitar substitui o caminho.
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(explorer_at(&mut app, &[]).path_edit.as_ref().unwrap().text, "x");
        // A dica da barra e o campo com o exemplo.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(painted_texts(&out)
            .iter()
            .any(|(t, _)| t == "Caminho, ex.: /var/www ou ~/public_html"));
        // Sem conexao ainda: nada abre (nem por Ctrl+L, nem pela barra).
        let mut e = FileExplorer::new();
        e.start_path_edit();
        assert!(e.path_edit.is_none());
        e.loading = false;
        let (_, out) = nav_frame(&ctx, &mut e, vec![key(egui::Key::L, ctrl_win())]);
        assert!(e.path_edit.is_none() && out.goto.is_none());
        let full = nav_frame(&ctx, &mut e, vec![]).0;
        let at = text_pos(&full, "/").expect("barra sem caminho");
        nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at), click(at, true)]);
        nav_frame(&ctx, &mut e, vec![click(at, false)]);
        assert!(e.path_edit.is_none());
    }

    // --- Visualizador somente leitura (1.1.0) --------------------------------
    //
    // Quadros com o Windows no modo claro (system_theme Light), como o do
    // usuario: o app forca o tema escuro.

    type CancelRx = tokio::sync::watch::Receiver<bool>;

    /// Mensagens que a UI mandou a sessao ("read <id> <caminho>", "list
    /// <caminho>", "goto ..."); os cancelamentos das leituras vao para
    /// `cancels`.
    fn view_msgs(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiToSftp>,
        cancels: &mut Vec<CancelRx>,
    ) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(m) = rx.try_recv() {
            match m {
                UiToSftp::ReadFile { id, path, cancel } => {
                    v.push(format!("read {id} {path}"));
                    cancels.push(cancel);
                }
                UiToSftp::ListDir(p) => v.push(format!("list {p}")),
                UiToSftp::Goto { seq, path } => v.push(format!("goto {seq} {path}")),
                _ => v.push("outro".into()),
            }
        }
        v
    }

    /// Leitura cancelada: a UI soltou o `Cancel` ou cancelou.
    fn cancelled(c: &CancelRx) -> bool {
        *c.borrow() || c.has_changed().is_err()
    }

    /// Documento como a tarefa da sessao entregaria.
    fn view_doc(path: &str, text: &str) -> Box<viewer::ViewDoc> {
        Box::new(viewer::build_doc(
            path.into(),
            None,
            Some(text.len() as u64),
            Some(1_700_000_000),
            text.as_bytes().to_vec(),
            None,
        ))
    }

    fn done(id: u64, result: Result<Box<viewer::ViewDoc>, ViewError>) -> SftpToUi {
        SftpToUi::View(ViewEvent::Done { id, result })
    }

    fn enter() -> egui::Event {
        key(egui::Key::Enter, egui::Modifiers::NONE)
    }

    fn viewer0(app: &mut App) -> &mut FileViewer {
        explorer_at(app, &[]).viewer.as_mut().expect("visualizador fechado")
    }

    /// Painel SFTP em /srv (focado) com a entrada `idx` aberta no
    /// visualizador com o texto dado.
    fn viewer_app(
        names: &[&str],
        idx: usize,
        text: &str,
    ) -> (
        egui::Context,
        App,
        tokio::sync::mpsc::UnboundedReceiver<UiToSftp>,
        std::sync::mpsc::Sender<SftpToUi>,
    ) {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(names);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        explorer_at(&mut app, &[]).click(idx, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let path = format!("/srv/{}", names[idx]);
        let msgs = view_msgs(&mut rx, &mut Vec::new());
        let id = explorer_at(&mut app, &[]).view_seq;
        assert_eq!(msgs, [format!("read {id} {path}")]);
        tx.send(done(id, Ok(view_doc(&path, text)))).unwrap();
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert!(explorer_at(&mut app, &[]).viewer.is_some(), "visualizador nao abriu");
        (ctx, app, rx, tx)
    }

    /// Botao direito numa linha e clique em `item` no menu (quadros do app).
    fn app_menu_click(ctx: &egui::Context, app: &mut App, row: &str, item: &str) -> Vec<String> {
        let out = nav_app_frame(ctx, app, vec![]);
        let pos = text_pos(&out, row).unwrap_or_else(|| panic!("{row} nao desenhado"));
        let button = |pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Default::default(),
        };
        nav_app_frame(ctx, app, vec![egui::Event::PointerMoved(pos), button(true)]);
        nav_app_frame(ctx, app, vec![button(false)]);
        let out = nav_app_frame(ctx, app, vec![]);
        let texts: Vec<String> = painted_texts(&out).into_iter().map(|(s, _)| s).collect();
        if let Some(at) = text_pos(&out, item) {
            nav_app_frame(ctx, app, vec![egui::Event::PointerMoved(at), click(at, true)]);
            nav_app_frame(ctx, app, vec![click(at, false)]);
        } else {
            // Item ausente: fecha o menu.
            nav_app_frame(ctx, app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        }
        texts
    }

    /// Enter e duplo clique num arquivo pedem a leitura (faixa "Abrindo");
    /// numa pasta listam; na ".." sobem. O menu do arquivo tem "Visualizar".
    #[test]
    fn enter_on_file_sends_read_file() {
        let ctx = light_ctx();
        let (mut app, mut rx, _tx) = sftp_app(&["pasta1", "a.txt", "b.txt"]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let mut cancels = Vec::new();
        explorer_at(&mut app, &[]).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 1 /srv/a.txt"]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let abrindo = "Abrindo \u{201C}a.txt\u{201D}\u{2026}";
        assert!(painted_texts(&out).iter().any(|(t, _)| t == abrindo), "faixa Abrindo");
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "Esc cancela"));
        // A lista continua utilizavel; Enter de novo no mesmo arquivo nao
        // repete o pedido.
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert!(view_msgs(&mut rx, &mut cancels).is_empty());
        // Duplo clique em outro arquivo: pedido novo, o anterior e cancelado.
        let at = text_pos(&out, "b.txt").expect("linha b.txt");
        for pressed in [true, false, true, false] {
            nav_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(at), click(at, pressed)]);
        }
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 2 /srv/b.txt"]);
        assert!(cancelled(&cancels[0]), "pedido anterior nao foi cancelado");
        assert!(!cancelled(&cancels[1]));
        assert_eq!(explorer_at(&mut app, &[]).opening.as_ref().map(|o| o.id), Some(2));
        // Pasta: lista; ".." sobe.
        explorer_at(&mut app, &[]).click(0, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["list /srv/pasta1"]);
        assert!(cancelled(&cancels[1]), "navegar cancela a abertura");
        assert!(explorer_at(&mut app, &[]).opening.is_none());
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some(".."));
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["list /srv"]);

        // Menu de contexto: "Visualizar" so para arquivo.
        let (mut app, mut rx, _tx) = sftp_app(&["pasta1", "a.txt"]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let texts = app_menu_click(&ctx, &mut app, "pasta1/", "Visualizar");
        assert!(!texts.iter().any(|t| t == "Visualizar"), "Visualizar numa pasta");
        let texts = app_menu_click(&ctx, &mut app, "a.txt", "Visualizar");
        assert!(texts.iter().any(|t| t == "Visualizar"), "{texts:?}");
        let msgs = view_msgs(&mut rx, &mut cancels);
        assert_eq!(msgs.last().map(String::as_str), Some("read 1 /srv/a.txt"), "{msgs:?}");
    }

    /// O resultado abre o visualizador (nome, codificacao, fim de linha e o
    /// selo); Esc volta a lista no mesmo item, com o painel focado.
    #[test]
    fn view_done_opens_viewer_and_esc_returns() {
        let (ctx, mut app, mut rx, _tx) =
            viewer_app(&["pasta1", "a.txt", "b.txt"], 1, "ação é fácil\nlinha 2\n");
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let texts: Vec<String> = painted_texts(&out).into_iter().map(|(s, _)| s).collect();
        let has = |p: &dyn Fn(&str) -> bool| texts.iter().any(|t| p(t));
        assert!(has(&|t| t == "a.txt"), "nome: {texts:?}");
        assert!(has(&|t| t.contains("UTF-8") && t.contains("LF")), "{texts:?}");
        assert!(has(&|t| t == "somente leitura"));
        assert!(has(&|t| t == "ação é fácil") && has(&|t| t == "linha 2"));
        assert!(has(&|t| t == "1") && has(&|t| t == "2"), "numeros de linha");
        assert!(!has(&|t| t == "pasta1/"), "a lista nao aparece por baixo");
        assert_eq!(text_color(&out, "ação é fácil"), Some(CARD_TEXT));
        assert!(app.session_hint().starts_with("Esc volta à listagem"));
        assert_eq!(app.focused_path, Some(vec![]));
        // Esc: volta a lista no mesmo item, painel ainda focado.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[]).viewer.is_none());
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "pasta1/"));
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some("a.txt"));
        assert_eq!(app.focused_path, Some(vec![]));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some("b.txt"));
        assert!(view_msgs(&mut rx, &mut Vec::new()).is_empty());
    }

    /// Respostas de pedidos antigos (cancelados) sao ignoradas.
    #[test]
    fn stale_ids_ignored() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["a.txt", "b.txt"]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let mut cancels = Vec::new();
        explorer_at(&mut app, &[]).click(0, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        explorer_at(&mut app, &[]).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(
            view_msgs(&mut rx, &mut cancels),
            ["read 1 /srv/a.txt", "read 2 /srv/b.txt"]
        );
        assert!(cancelled(&cancels[0]) && !cancelled(&cancels[1]));
        // O andamento e o resultado de A chegam atrasados: ignorados.
        tx.send(SftpToUi::View(ViewEvent::Progress {
            id: 1,
            got: 5,
            total: Some(9),
        }))
        .unwrap();
        tx.send(done(1, Ok(view_doc("/srv/a.txt", "de A\n")))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        {
            let e = explorer_at(&mut app, &[]);
            assert!(e.viewer.is_none(), "abriu o pedido cancelado");
            assert_eq!(e.opening.as_ref().map(|o| (o.id, o.got)), Some((2, 0)));
        }
        tx.send(SftpToUi::View(ViewEvent::Progress {
            id: 2,
            got: 3,
            total: Some(9),
        }))
        .unwrap();
        let out = {
            nav_app_frame(&ctx, &mut app, vec![]);
            nav_app_frame(&ctx, &mut app, vec![])
        };
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "3 B de 9 B"), "andamento");
        tx.send(done(2, Ok(view_doc("/srv/b.txt", "de B\n")))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        let v = viewer0(&mut app);
        assert_eq!((v.name.as_str(), v.doc.text.as_str()), ("b.txt", "de B\n"));
        assert!(explorer_at(&mut app, &[]).opening.is_none());
    }

    /// Binario, especial ou erro nunca abrem o visualizador: aviso ambar ou
    /// falha vermelha. Pasta (tipo so descoberto na leitura) entra nela.
    #[test]
    fn binary_or_special_shows_notice_not_viewer() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["a.bin"]);
        explorer_at(&mut app, &[]).entries.push(unknown_node("sem-tipo"));
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let mut cancels = Vec::new();
        let mut abre = |app: &mut App, idx: usize, err: ViewError| -> egui::FullOutput {
            explorer_at(app, &[]).click(idx, false, false);
            nav_app_frame(&ctx, app, vec![enter()]);
            let id = explorer_at(app, &[]).view_seq;
            assert_eq!(view_msgs(&mut rx, &mut cancels).len(), 1);
            tx.send(done(id, Err(err))).unwrap();
            nav_app_frame(&ctx, app, vec![]);
            nav_app_frame(&ctx, app, vec![])
        };
        for (err, parte) in [(ViewError::Binary, "binário"), (ViewError::Special, "especial")] {
            let out = abre(&mut app, 0, err);
            let e = explorer_at(&mut app, &[]);
            assert!(e.viewer.is_none());
            let aviso = e.notice.clone().expect("sem aviso");
            assert!(aviso.contains(parte), "{aviso}");
            assert_eq!(text_color(&out, &aviso), Some(HIGHLIGHT));
        }
        // Falhas: faixa vermelha; "nao existe mais" atualiza a pasta.
        let out = abre(&mut app, 0, ViewError::Denied);
        let want = "Sem permissão para ler \u{201C}a.bin\u{201D}.";
        assert_eq!(explorer_at(&mut app, &[]).error.as_deref(), Some(want));
        assert_eq!(text_color(&out, want), Some(ERROR_FG));
        explorer_at(&mut app, &[]).click(0, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let id = explorer_at(&mut app, &[]).view_seq;
        view_msgs(&mut rx, &mut cancels);
        tx.send(done(id, Err(ViewError::NotFound))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["list /srv"]);
        assert_eq!(
            explorer_at(&mut app, &[]).error.as_deref(),
            Some("\u{201C}a.bin\u{201D} não existe mais no servidor.")
        );
        assert!(explorer_at(&mut app, &[]).viewer.is_none());
        // Tipo desconhecido que e pasta: entra nela.
        explorer_at(&mut app, &[]).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let id = explorer_at(&mut app, &[]).view_seq;
        assert_eq!(view_msgs(&mut rx, &mut cancels), [format!("read {id} /srv/sem-tipo")]);
        tx.send(done(id, Err(ViewError::IsDir))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["list /srv/sem-tipo"]);
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/srv/sem-tipo");
        assert!(explorer_at(&mut app, &[]).viewer.is_none());
    }

    /// Esc (e F5, Backspace) cancelam uma abertura em curso; o teclado
    /// continua na lista e uma resposta atrasada e ignorada.
    #[test]
    fn esc_while_opening_cancels() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["a.txt", "b.txt"]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let mut cancels = Vec::new();
        explorer_at(&mut app, &[]).click(0, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 1 /srv/a.txt"]);
        assert!(cancelled(&cancels[0]), "Esc nao cancelou");
        assert!(explorer_at(&mut app, &[]).opening.is_none());
        assert!(!painted_texts(&out).iter().any(|(t, _)| t.starts_with("Abrindo")));
        assert_eq!(app.focused_path, Some(vec![]));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::ArrowDown, egui::Modifiers::NONE)]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[])).as_deref(), Some("b.txt"));
        tx.send(done(1, Ok(view_doc("/srv/a.txt", "x\n")))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[]).viewer.is_none(), "resposta cancelada abriu");
        // F5 na lista: cancela e atualiza a pasta.
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F5, egui::Modifiers::NONE)]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 2 /srv/b.txt", "list /srv"]);
        assert!(cancelled(&cancels[1]));
        // Backspace (sobe): cancela tambem.
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 3 /srv/b.txt", "list /"]);
        assert!(cancelled(&cancels[2]));
    }

    /// Com o visualizador aberto, as teclas da lista nao agem (nem vazam
    /// para o terminal ao lado); Ctrl+S baixa so este arquivo.
    #[test]
    fn viewer_swallows_explorer_keys() {
        let (ctx, mut app, mut rx, _tx) = viewer_app(&["a.txt", "b.txt"], 0, "texto\n");
        app.split_pane(&[], SplitDir::SideBySide);
        let (mut ssh_rx, _stx) = connecting_ssh_pane(&mut app, &[1]);
        if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, &[1])) {
            p.state = SessionState::Connected;
        }
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        app.pending_focus = Some(vec![0]);
        for _ in 0..2 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![0]));
        explorer_at(&mut app, &[0]).marked = ["a.txt".to_string(), "b.txt".to_string()].into();
        let teclas = [
            enter(),
            key(egui::Key::Delete, egui::Modifiers::NONE),
            key(egui::Key::F2, egui::Modifiers::NONE),
            key(egui::Key::Backspace, egui::Modifiers::NONE),
            key(egui::Key::Tab, egui::Modifiers::NONE),
            egui::Event::Text("b".into()),
            egui::Event::Paste("colado".into()),
        ];
        for t in teclas {
            nav_app_frame(&ctx, &mut app, vec![t]);
        }
        nav_app_frame(&ctx, &mut app, vec![]);
        {
            let e = explorer_at(&mut app, &[0]);
            assert!(e.viewer.is_some(), "visualizador fechou");
            assert!(e.dialog.is_none(), "dialogo aberto por baixo");
            assert!(e.typeahead.is_empty(), "letra foi para a busca da lista");
            assert_eq!(e.cur_path, "/srv");
        }
        assert_eq!(app.focused_path, Some(vec![0]), "Tab tirou o foco do painel");
        assert!(view_msgs(&mut rx, &mut Vec::new()).is_empty(), "navegou por baixo");
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "tecla vazou para o terminal");
        // Ctrl+S: so o arquivo aberto (nao os marcados na lista).
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::S, ctrl_win())]);
        let req = app.pending_download.take().expect("Ctrl+S nao pediu o download");
        let names: Vec<&str> = req.picks.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["a.txt"]);
        assert_eq!(req.picks[0].remote, "/srv/a.txt");
        assert!(sent_bytes(&mut ssh_rx).is_empty());
    }

    /// Divide a tela: o painel SFTP vira o [0], com o teclado, e um terminal
    /// SSH conectado fica ao lado [1] (para ver que nada vaza para ele).
    fn sftp_beside_terminal(
        ctx: &egui::Context,
        app: &mut App,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<UiToSsh>,
        std::sync::mpsc::Sender<SshToUi>,
    ) {
        app.split_pane(&[], SplitDir::SideBySide);
        let (ssh_rx, stx) = connecting_ssh_pane(app, &[1]);
        if let Some(Node::Leaf(p)) = app.root.as_mut().and_then(|r| node_at_mut(r, &[1])) {
            p.state = SessionState::Connected;
        }
        for _ in 0..3 {
            nav_app_frame(ctx, app, vec![]);
        }
        app.pending_focus = Some(vec![0]);
        for _ in 0..2 {
            nav_app_frame(ctx, app, vec![]);
        }
        assert_eq!(app.focused_path, Some(vec![0]));
        (ssh_rx, stx)
    }

    /// Tab e Shift+Tab nunca tiram o teclado do painel SFTP: o egui os usaria
    /// para passar o foco a um botao ou a linha "..", e o painel ficaria sem
    /// teclado (letras sem efeito) ate um clique. Na lista e no campo do
    /// caminho nao fazem nada (o texto digitado fica); no visualizador, com a
    /// busca aberta, alternam entre o campo e o texto. Vale tambem para o Tab
    /// logo no quadro seguinte ao que deu o foco (antes de qualquer filtro de
    /// foco do egui valer). Nada vaza para o terminal ao lado.
    #[test]
    fn tab_keeps_keyboard_in_sftp_panel() {
        let tab = || key(egui::Key::Tab, egui::Modifiers::NONE);
        let shift_tab = || key(egui::Key::Tab, egui::Modifiers::SHIFT);
        let text = |t: &str| egui::Event::Text(t.into());

        // Lista.
        let ctx = light_ctx();
        let (mut app, mut rx, _tx) = sftp_app(&["pasta1", "a.txt", "b.txt"]);
        let (mut ssh_rx, _stx) = sftp_beside_terminal(&ctx, &mut app);
        for t in [tab(), shift_tab(), tab()] {
            nav_app_frame(&ctx, &mut app, vec![t]);
            nav_app_frame(&ctx, &mut app, vec![]);
            assert_eq!(app.focused_path, Some(vec![0]), "Tab tirou o teclado da lista");
        }
        nav_app_frame(&ctx, &mut app, vec![text("b")]);
        assert_eq!(cursor_name(explorer_at(&mut app, &[0])).as_deref(), Some("b.txt"));
        // Ctrl+B, Tab: o Tab e a segunda tecla (invalida) e desarma o prefixo.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::B, ctrl_win())]);
        assert!(app.chord_armed_at.is_some());
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        assert!(app.chord_armed_at.is_none(), "Tab nao desarmou o Ctrl+B");
        assert_eq!(app.focused_path, Some(vec![0]));

        // Campo do caminho, com o Tab logo depois do Ctrl+L: segue editando
        // com o texto digitado, e o Enter abre o caminho.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        nav_app_frame(&ctx, &mut app, vec![text("/etc")]);
        nav_app_frame(&ctx, &mut app, vec![shift_tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(app.focused_path, Some(vec![0]));
        let edit = explorer_at(&mut app, &[0]).path_edit.as_ref().map(|e| e.text.clone());
        assert_eq!(edit.as_deref(), Some("/etc"), "Tab fechou a edicao do caminho");
        let _ = sftp_msgs(&mut rx);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let seq = explorer_at(&mut app, &[0]).goto_seq;
        assert_eq!(sftp_msgs(&mut rx), [format!("goto {seq} /etc")]);
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "tecla vazou para o terminal");

        // Visualizador: Tab alterna entre o campo da busca e o texto (a busca
        // continua aberta); sem a busca, nao faz nada.
        let (ctx, mut app, _rx, _tx) = viewer_app(&["a.txt", "b.txt"], 0, "texto\nlinha 2\ntexto 3\n");
        let (mut ssh_rx, _stx) = sftp_beside_terminal(&ctx, &mut app);
        let (_, search_id) = explorer_field_ids(&("sftp_explorer", [0usize].as_slice()));
        let in_search = |ctx: &egui::Context| ctx.memory(|m| m.has_focus(search_id));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(!in_search(&ctx), "Tab nao levou o teclado ao texto");
        assert_eq!(app.focused_path, Some(vec![0]));
        assert!(explorer_at(&mut app, &[0]).viewer.as_ref().is_some_and(|v| v.search.is_some()));
        nav_app_frame(&ctx, &mut app, vec![shift_tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(in_search(&ctx), "Shift+Tab nao voltou ao campo da busca");
        nav_app_frame(&ctx, &mut app, vec![text("linha")]);
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(!in_search(&ctx));
        // No texto as letras nao vao a lugar nenhum (nem ao campo).
        nav_app_frame(&ctx, &mut app, vec![text("q")]);
        let query = |app: &mut App| {
            let v = explorer_at(app, &[0]).viewer.as_ref().expect("visualizador fechou");
            v.search.as_ref().map(|s| s.query.clone())
        };
        assert_eq!(query(&mut app).as_deref(), Some("linha"));
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(in_search(&ctx));
        // Esc fecha a busca; Tab sem ela nao faz nada e Esc fecha o arquivo.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(query(&mut app), None);
        nav_app_frame(&ctx, &mut app, vec![tab()]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(app.focused_path, Some(vec![0]));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[0]).viewer.is_none());
        assert_eq!(app.focused_path, Some(vec![0]));
        assert!(sent_bytes(&mut ssh_rx).is_empty(), "tecla vazou para o terminal");
    }

    /// Setas, PageUp/PageDown e Home/End rolam o texto.
    #[test]
    fn viewer_keyboard_scroll() {
        let text: String = (1..=1000).map(|i| format!("linha {i}\n")).collect();
        let (ctx, mut app, _rx, _tx) = viewer_app(&["a.txt"], 0, &text);
        let page = {
            let v = viewer0(&mut app);
            assert!(v.view_size.y > 200.0, "{:?}", v.view_size);
            ((v.view_size.y / v.row_h).floor() as usize).saturating_sub(1).max(1)
        };
        let k = |k| key(k, egui::Modifiers::NONE);
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::PageDown)]);
        assert_eq!(viewer0(&mut app).top_row(), page, "PageDown anda uma pagina");
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::ArrowDown), k(egui::Key::ArrowDown)]);
        assert_eq!(viewer0(&mut app).top_row(), page + 2);
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::PageUp)]);
        assert_eq!(viewer0(&mut app).top_row(), 2);
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::End)]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let v = viewer0(&mut app);
        assert!((v.offset.y - v.max_y()).abs() < 1.0, "End: {} de {}", v.offset.y, v.max_y());
        assert!(visible_texts(&out).iter().any(|(t, _)| t == "linha 1000"), "ultima linha");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Home, ctrl_win())]);
        assert_eq!(viewer0(&mut app).offset.y, 0.0);
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::End)]);
        nav_app_frame(&ctx, &mut app, vec![k(egui::Key::Home)]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(viewer0(&mut app).top_row(), 0);
        assert!(visible_texts(&out).iter().any(|(t, _)| t == "linha 1"));
    }

    /// Ctrl+F abre a busca; Enter e Shift+Enter andam entre as ocorrencias
    /// com o campo focado; Esc fecha a barra (painel focado) e o segundo Esc
    /// fecha o visualizador.
    #[test]
    fn viewer_search_flow() {
        let text: String = (0..300)
            .map(|i| if i % 50 == 7 { format!("linha {i}\n") } else { format!("outra {i}\n") })
            .collect();
        let (ctx, mut app, _rx, _tx) = viewer_app(&["a.txt"], 0, &text);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("LINHA".into())]);
        assert_eq!(
            viewer0(&mut app).search.as_ref().map(|s| s.query.as_str()),
            Some("LINHA"),
            "o campo de busca nao recebeu o texto"
        );
        assert_eq!(app.focused_path, Some(vec![]), "busca focada: painel continua o focado");
        assert!(app.session_hint().starts_with("Esc volta"));
        // Pausa na digitacao: recalcula (6 ocorrencias, a atual e a 1a).
        std::thread::sleep(SEARCH_DEBOUNCE + std::time::Duration::from_millis(30));
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        {
            let s = viewer0(&mut app).search.as_ref().unwrap();
            assert_eq!((s.matches.len(), s.current), (6, Some(0)));
        }
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "1 de 6"));
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let v = viewer0(&mut app);
        assert_eq!(v.search.as_ref().unwrap().current, Some(2));
        // A ocorrencia atual (linha 107) ficou visivel.
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(visible_texts(&out).iter().any(|(t, _)| t == "linha 107"));
        // Campo continua focado: digitar vai para ele.
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text(" 1".into())]);
        assert_eq!(viewer0(&mut app).search.as_ref().unwrap().query, "LINHA 1");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        // Logo depois de editar, o Shift+Enter so recalcula (a atual passa a
        // ser a primeira a partir do topo).
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)]);
        let s = viewer0(&mut app).search.as_ref().unwrap();
        assert_eq!(s.query, "LINHA");
        assert_eq!(s.matches.len(), 6);
        // Consulta ja calculada: Shift+Enter e Shift+F3 voltam uma ocorrencia
        // cada (dando a volta); F3 avanca.
        let cur = |app: &mut App| viewer0(app).search.as_ref().unwrap().current.unwrap();
        let c0 = cur(&mut app);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)]);
        let c1 = cur(&mut app);
        assert_eq!(c1, (c0 + 5) % 6, "Shift+Enter nao voltou");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F3, egui::Modifiers::SHIFT)]);
        assert_eq!(cur(&mut app), (c1 + 5) % 6, "Shift+F3 nao voltou");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F3, egui::Modifiers::NONE)]);
        assert_eq!(cur(&mut app), c1, "F3 nao avancou");
        // Esc: fecha a barra; o painel segue focado e o visualizador aberto.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(viewer0(&mut app).search.is_none());
        assert_eq!(app.focused_path, Some(vec![]));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        assert!(explorer_at(&mut app, &[]).viewer.is_none(), "o segundo Esc fecha");
    }

    /// Ctrl+A e Ctrl+C copiam o texto seguro: ESC em notacao e CRLF em \n.
    #[test]
    fn viewer_copy() {
        let (ctx, mut app, _rx, _tx) =
            viewer_app(&["a.txt"], 0, "cor \u{1b}[31mvermelha\r\nlinha 2\r\n");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::A, ctrl_win())]);
        let out = nav_app_frame(&ctx, &mut app, vec![egui::Event::Copy]);
        let copied: Vec<&str> = out
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, ["cor ^[[31mvermelha\nlinha 2"]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(painted_texts(&out).iter().any(|(t, _)| t == "Copiado"));
        // ESC visivel na tela tambem (nunca cru).
        assert!(painted_texts(&out).iter().any(|(t, _)| t.contains("^[")));
        assert!(!painted_texts(&out).iter().any(|(t, _)| t.contains('\u{1b}')));
    }

    /// Arquivo com 200.000 linhas: so as visiveis sao desenhadas.
    #[test]
    fn viewer_virtualized() {
        let text: String = (0..viewer::MAX_ROWS).map(|i| format!("linha {i}\n")).collect();
        let (ctx, mut app, _rx, _tx) = viewer_app(&["grande.txt"], 0, &text);
        let conta = |out: &egui::FullOutput| {
            painted_texts(out).iter().filter(|(t, _)| t.starts_with("linha ")).count()
        };
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let visiveis = {
            let v = viewer0(&mut app);
            assert_eq!(v.doc.rows.len(), viewer::MAX_ROWS);
            (v.view_size.y / v.row_h).ceil() as usize
        };
        assert!(conta(&out) <= visiveis + 2, "{} desenhadas, {visiveis} visiveis", conta(&out));
        assert!(conta(&out) >= visiveis.saturating_sub(1));
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::End, egui::Modifiers::NONE)]);
        let t0 = Instant::now();
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "quadro lento");
        assert!(conta(&out) <= visiveis + 2);
        let ultima = format!("linha {}", viewer::MAX_ROWS - 1);
        assert!(visible_texts(&out).iter().any(|(t, _)| *t == ultima));
    }

    /// F5 com o visualizador recarrega o arquivo (pedido novo, sem listar a
    /// pasta) e mantem a linha do topo.
    #[test]
    fn viewer_f5_reloads() {
        let text: String = (1..=1000).map(|i| format!("linha {i}\n")).collect();
        let (ctx, mut app, mut rx, tx) = viewer_app(&["a.txt", "b.txt"], 0, &text);
        let pd = || key(egui::Key::PageDown, egui::Modifiers::NONE);
        nav_app_frame(&ctx, &mut app, vec![pd(), pd()]);
        let top = viewer0(&mut app).top_row();
        assert!(top > 10, "{top}");
        let mut cancels = Vec::new();
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F5, egui::Modifiers::NONE)]);
        let msgs = view_msgs(&mut rx, &mut cancels);
        assert_eq!(msgs, ["read 2 /srv/a.txt"], "recarga sem listar a pasta");
        // O conteudo antigo continua na tela durante a recarga; F5 de novo
        // nao repete o pedido.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F5, egui::Modifiers::NONE)]);
        assert!(view_msgs(&mut rx, &mut cancels).is_empty());
        assert!(explorer_at(&mut app, &[]).viewer.is_some());
        let novo: String = (1..=1000).map(|i| format!("nova {i}\n")).collect();
        tx.send(done(2, Ok(view_doc("/srv/a.txt", &novo)))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let v = viewer0(&mut app);
        assert!(v.doc.text.starts_with("nova 1\n"));
        assert_eq!(v.top_row(), top, "linha do topo mantida");
        assert!(visible_texts(&out).iter().any(|(t, _)| *t == format!("nova {}", top + 1)));
        // Falha na recarga: o conteudo continua, com o aviso.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F5, egui::Modifiers::NONE)]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["read 3 /srv/a.txt"]);
        tx.send(done(3, Err(ViewError::Timeout))).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        let want = "Não foi possível recarregar: O servidor demorou demais para enviar \u{201C}a.txt\u{201D}.";
        assert!(painted_texts(&out).iter().any(|(t, _)| t == want), "aviso da recarga");
        assert!(viewer0(&mut app).doc.text.starts_with("nova 1\n"));
        // Fechado o visualizador, F5 volta a atualizar a pasta.
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F5, egui::Modifiers::NONE)]);
        assert_eq!(view_msgs(&mut rx, &mut cancels), ["list /srv"]);
    }

    /// Icones novos do visualizador: mesmo formato seguro dos demais.
    #[test]
    fn viewer_icons_are_safe_white_svgs() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        for (name, img) in [
            ("eye", ICON_EYE),
            ("arrow-left", ICON_ARROW_LEFT),
            ("chevron-up", ICON_CHEVRON_UP),
            ("chevron-down", ICON_CHEVRON_DOWN),
        ] {
            let (uri, bytes) = icon_bytes(&img);
            assert_eq!(uri, format!("bytes://../assets/{name}.svg"));
            let text = std::str::from_utf8(bytes).expect("SVG em UTF-8");
            assert!(text.starts_with("<svg ") && text.ends_with("</svg>"), "{name}");
            assert!(text.contains("stroke=\"#ffffff\""), "{name}");
            for bad in ["currentColor", "script", "href", "style", "<!", "\r", "\n"] {
                assert!(!text.contains(bad), "{name}: {bad:?}");
            }
            assert!(bytes.len() < 2 * 1024, "{name}");
            assert!(egui_extras::image::load_svg_bytes(bytes).is_ok(), "{name}");
        }
    }

    // --- Correcoes da revisao da 1.1.0 --------------------------------------

    /// Quadro do navegador sozinho, em foco, numa janela de `w` px.
    fn nav_frame_w(
        ctx: &egui::Context,
        e: &mut FileExplorer,
        w: f32,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, ExplorerOut) {
        let mut out = None;
        let full = ctx.run(light_raw(egui::vec2(w, 600.0), events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                out = Some(e.ui(ui, "exp", true, DlAvail::Ready));
            });
        });
        (full, out.unwrap())
    }

    /// Quadro completo da sessao numa janela do tamanho dado.
    fn app_frame_sized(
        ctx: &egui::Context,
        app: &mut App,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(light_raw(size, events), |ctx| {
            app.guard_host_key_keys(ctx);
            app.handle_file_drop(ctx);
            app.handle_session_keys(ctx);
            app.drain_ssh_events();
            app.guard_host_key_keys(ctx);
            app.handle_help_keys(ctx);
            egui::CentralPanel::default().show(ctx, |ui| app.ui_session(ui));
            app.ui_host_key_prompt(ctx);
        })
    }

    /// Textos pintados com a area que ocupam de fato na tela (um texto num
    /// layout da direita para a esquerda e alinhado a direita da posicao).
    fn drawn_texts(out: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Text(t) => Some((
                    t.galley.text().to_string(),
                    t.galley.rect.translate(t.pos.to_vec2()),
                )),
                _ => None,
            })
            .collect()
    }

    /// Area das imagens (icones) pintadas no quadro.
    fn painted_images(out: &egui::FullOutput) -> Vec<egui::Rect> {
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Rect(r) if r.brush.is_some() => Some(r.rect),
                _ => None,
            })
            .collect()
    }

    /// Tamanho da fonte com que o texto `t` foi pintado.
    fn text_size(out: &egui::FullOutput, t: &str) -> Option<f32> {
        out.shapes.iter().rev().find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(g) if g.galley.text() == t => {
                g.galley.job.sections.first().map(|sec| sec.format.font_id.size)
            }
            _ => None,
        })
    }

    /// Abrir um segundo arquivo depois de rolar o primeiro: ele comeca no
    /// topo (a area de texto tem o mesmo id no painel inteiro).
    #[test]
    fn viewer_second_file_starts_at_top() {
        let text: String = (1..=1000).map(|i| format!("linha {i}\n")).collect();
        let (ctx, mut app, mut rx, tx) = viewer_app(&["a.txt", "b.txt"], 0, &text);
        let pd = || key(egui::Key::PageDown, egui::Modifiers::NONE);
        nav_app_frame(&ctx, &mut app, vec![pd(), pd()]);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        let top_a = viewer0(&mut app).top_row();
        assert!(top_a > 10, "{top_a}");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Escape, egui::Modifiers::NONE)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(explorer_at(&mut app, &[]).viewer.is_none());
        explorer_at(&mut app, &[]).click(1, false, false);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        let id = explorer_at(&mut app, &[]).view_seq;
        assert_eq!(view_msgs(&mut rx, &mut Vec::new()), [format!("read {id} /srv/b.txt")]);
        let text_b: String = (1..=1000).map(|i| format!("outro {i}\n")).collect();
        tx.send(done(id, Ok(view_doc("/srv/b.txt", &text_b)))).unwrap();
        let mut out = nav_app_frame(&ctx, &mut app, vec![]);
        for _ in 0..3 {
            out = nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert_eq!(viewer0(&mut app).top_row(), 0, "b.txt abriu no meio (a.txt estava em {top_a})");
        assert!(visible_texts(&out).iter().any(|(t, _)| t == "outro 1"));
        let x = viewer0(&mut app).offset.x;
        assert_eq!(x, 0.0, "rolagem horizontal herdada");
    }

    /// Entrar numa pasta comeca do topo, com a ".." (o cursor) visivel, mesmo
    /// quando a listagem chega antes do quadro seguinte (servidor rapido) ou
    /// junto do Goto da barra; voltar ainda rola ate a pasta de onde se saiu.
    #[test]
    fn entering_folder_starts_at_top() {
        let ctx = light_ctx();
        let nomes: Vec<String> = (0..80).map(|i| format!("pasta{i:02}")).collect();
        let refs: Vec<&str> = nomes.iter().map(String::as_str).collect();
        let mut e = explorer_with(&refs);
        nav_frame(&ctx, &mut e, vec![key(egui::Key::End, egui::Modifiers::NONE)]);
        settle(&ctx, &mut e);
        let (_, out) = nav_frame(&ctx, &mut e, vec![enter()]);
        assert_eq!(out.to_list, ["/srv/pasta79"]);
        let novos: Vec<sftp::RemoteEntry> = (0..80).map(|i| remote_entry(&format!("f{i:02}"))).collect();
        e.apply_listing("/srv/pasta79", novos);
        let full = settle(&ctx, &mut e);
        assert_eq!(cursor_name(&e).as_deref(), Some(".."));
        assert!(visible_texts(&full).iter().any(|(t, _)| t == ".."), "a \"..\" ficou fora da vista");
        // Volta: listagem rapida, a pasta de onde se saiu (a ultima) a vista.
        nav_frame(&ctx, &mut e, vec![key(egui::Key::Backspace, egui::Modifiers::NONE)]);
        let pastas: Vec<sftp::RemoteEntry> = nomes.iter().map(|n| remote_dir(n)).collect();
        e.apply_listing("/srv", pastas);
        let full = settle(&ctx, &mut e);
        assert_eq!(cursor_name(&e).as_deref(), Some("pasta79"));
        assert!(visible_texts(&full).iter().any(|(t, _)| t == "pasta79/"), "voltou sem mostrar a pasta");

        // Pela barra: Goto e listagem chegam juntos.
        let (mut app, mut rx, tx) = sftp_app(&refs);
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::End, egui::Modifiers::NONE)]);
        for _ in 0..31 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("/etc".into())]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs(&mut rx), ["goto 1 /etc"]);
        tx.send(SftpToUi::Goto { seq: 1, result: Ok(sftp::GotoKind::Dir) }).unwrap();
        let entries: Vec<sftp::RemoteEntry> = (0..80).map(|i| remote_entry(&format!("e{i:02}"))).collect();
        tx.send(SftpToUi::Listing { path: "/etc".into(), entries }).unwrap();
        let mut out = nav_app_frame(&ctx, &mut app, vec![]);
        for _ in 0..31 {
            out = nav_app_frame(&ctx, &mut app, vec![]);
        }
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/etc");
        assert!(visible_texts(&out).iter().any(|(t, _)| t == ".."), "Goto: a \"..\" fora da vista");
    }

    /// Avisos, erros, a faixa "Abrindo" e o erro do caminho quebram linha num
    /// painel estreito: nada passa da borda, o X continua ao alcance e as
    /// colunas nao sao empurradas para fora. O erro do caminho e legivel.
    #[test]
    fn long_texts_fit_narrow_pane() {
        let ctx = light_ctx();
        let nome = "relatorio-financeiro-consolidado-do-ano-de-2026-versao-final.bin";
        let w = 330.0;
        let mut e = explorer_with(&[nome, "a.txt"]);
        e.notice = Some(view_error_text(&ViewError::Binary, nome).0);
        e.error = Some(format!("Não foi possível abrir /srv/{nome}: o servidor recusou"));
        let mut out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        for _ in 0..3 {
            out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        }
        let texts = drawn_texts(&out);
        for (t, r) in &texts {
            assert!(r.right() <= w + 0.5, "{t:?} passa da borda: {r:?}");
        }
        for r in painted_images(&out) {
            assert!(r.right() <= w + 0.5, "icone fora do painel: {r:?}");
        }
        let notice = e.notice.clone().unwrap();
        let linhas = out.shapes.iter().find_map(|s| match &s.shape {
            egui::epaint::Shape::Text(t) if t.galley.text() == notice => Some(t.galley.rows.len()),
            _ => None,
        });
        assert!(linhas.is_some_and(|n| n > 1), "o aviso nao quebrou linha: {linhas:?}");
        // Dispensar pelo X (dentro do painel).
        let xs: Vec<egui::Rect> = painted_images(&out)
            .into_iter()
            .filter(|r| r.width() <= 15.0 && r.right() > w - 40.0)
            .collect();
        assert!(xs.len() >= 2, "X dos avisos: {xs:?}");
        // Faixa "Abrindo" com nome longo: X e "Esc cancela" (ou so o X) dentro.
        e.notice = None;
        e.error = None;
        e.set_cursor_row(1, false);
        nav_frame_w(&ctx, &mut e, w, vec![enter()]);
        assert!(e.opening.is_some());
        if let Some(o) = &mut e.opening {
            o.got = 1_500_000;
            o.total = Some(9_800_000);
        }
        let mut out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        for _ in 0..2 {
            out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        }
        for (t, r) in drawn_texts(&out) {
            assert!(r.right() <= w + 0.5, "{t:?} passa da borda: {r:?}");
        }
        let abrindo = drawn_texts(&out)
            .into_iter()
            .find(|(t, _)| t.starts_with("Abrindo"))
            .expect("faixa Abrindo");
        let x = painted_images(&out)
            .into_iter()
            .find(|r| (r.center().y - abrindo.1.center().y).abs() < 8.0 && r.width() <= 15.0)
            .expect("X da faixa Abrindo");
        assert!(x.right() <= w, "X fora do painel: {x:?}");
        for (t, r) in drawn_texts(&out) {
            if t == "Esc cancela" || t.contains(" de ") {
                assert!(r.left() >= abrindo.1.right() - 0.5, "{t:?} por cima do nome");
                assert!(r.right() <= x.left() + 0.5, "{t:?} por cima do X");
            }
        }
        // Erro do caminho digitado: quebra linha e tem o tamanho normal.
        e.opening = None;
        e.start_path_edit();
        let erro = format!("Caminho não encontrado: /srv/{}", "pasta-que-nao-existe/".repeat(6));
        if let Some(p) = &mut e.path_edit {
            p.error = Some(erro.clone());
        }
        let mut out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        for _ in 0..2 {
            out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
        }
        for (t, r) in drawn_texts(&out) {
            assert!(r.right() <= w + 0.5, "{t:?} passa da borda: {r:?}");
        }
        let (t, _) = painted_texts(&out)
            .into_iter()
            .find(|(t, _)| t.starts_with("Caminho não encontrado"))
            .expect("erro do caminho");
        assert!(text_size(&out, &t).unwrap() >= 12.0, "erro do caminho em letra miuda");
    }

    /// Tela dividida em 3 na janela padrao: o que o painel SFTP pinta (um
    /// aviso longo, as colunas) fica recortado dentro dele, nunca no vizinho.
    #[test]
    fn split_panes_clip_their_contents() {
        let ctx = light_ctx();
        let (mut app, _rx, _tx) = sftp_app(&["relatorio-com-nome-bem-comprido-de-verdade.bin", "a.txt"]);
        app.split_pane(&[], SplitDir::SideBySide);
        app.split_pane(&[1], SplitDir::SideBySide);
        let size = egui::vec2(1000.0, 680.0);
        for _ in 0..3 {
            app_frame_sized(&ctx, &mut app, size, vec![]);
        }
        let nome = "relatorio-com-nome-bem-comprido-de-verdade.bin";
        {
            let e = explorer_at(&mut app, &[0]);
            e.notice = Some(view_error_text(&ViewError::Binary, nome).0);
            e.entries[0].date = "13/09/2026".into();
        }
        let mut out = app_frame_sized(&ctx, &mut app, size, vec![]);
        for _ in 0..2 {
            out = app_frame_sized(&ctx, &mut app, size, vec![]);
        }
        let pane = app
            .pane_rects
            .iter()
            .find(|(p, _)| p == &vec![0])
            .map(|(_, r)| *r)
            .expect("painel SFTP");
        assert!(pane.width() < 340.0, "{pane:?}");
        let mut vistos = 0;
        for s in &out.shapes {
            if let egui::epaint::Shape::Text(t) = &s.shape {
                let txt = t.galley.text();
                let do_sftp = txt.contains("parece ser um arquivo")
                    || txt == "Tamanho"
                    || txt.starts_with("relatorio-com-nome")
                    || txt == "a.txt";
                if do_sftp {
                    vistos += 1;
                    assert!(
                        s.clip_rect.right() <= pane.right() + 1.5,
                        "{txt:?}: recorte {:?} alem do painel {pane:?}",
                        s.clip_rect
                    );
                    let r = t.galley.rect.translate(t.pos.to_vec2());
                    assert!(r.right() <= pane.right() + 1.5, "{txt:?} em {r:?}");
                }
            }
        }
        assert!(vistos >= 3, "textos do painel SFTP: {vistos}");
    }

    /// A fonte proporcional tem as setas dos textos novos (alvo do link,
    /// "→ public_html"): sem reserva, sairiam como um quadrado vazio.
    #[test]
    fn proportional_font_has_arrows() {
        let ctx = light_ctx();
        let _ = ctx.run(light_raw(egui::vec2(200.0, 100.0), vec![]), |_| {});
        for c in ['\u{2192}', '\u{2190}', '\u{2026}', '\u{201C}', '\u{00B7}'] {
            let ok = ctx.fonts(|f| f.has_glyph(&egui::FontId::proportional(13.0), c));
            assert!(ok, "U+{:04X} sem glifo na fonte proporcional", c as u32);
        }
    }

    /// Cabecalho do visualizador num painel estreito: nome, dados e selo nunca
    /// passam por cima dos botoes (o selo sai e os dados encolhem antes).
    #[test]
    fn viewer_header_fits_narrow_panes() {
        let ctx = light_ctx();
        let mut e = explorer_with(&["config-antiga-do-sistema.ini"]);
        let bytes: Vec<u8> = b"op\xe7\xe3o=sim\r\nlinha 2\r\n".repeat(400);
        let doc = viewer::build_doc(
            "/srv/config-antiga-do-sistema.ini".into(),
            None,
            Some(bytes.len() as u64),
            Some(1_700_000_000),
            bytes,
            None,
        );
        assert_eq!(doc.encoding, viewer::Encoding::Windows1252);
        e.viewer = Some(FileViewer::new(Box::new(doc), "config-antiga-do-sistema.ini".into()));
        for w in [330.0f32, 500.0, 900.0] {
            let mut out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
            for _ in 0..2 {
                out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
            }
            let icons: Vec<egui::Rect> = painted_images(&out).into_iter().filter(|r| r.top() < 40.0).collect();
            let mut xs: Vec<f32> = icons.iter().map(|r| r.left()).collect();
            xs.sort_by(f32::total_cmp);
            // Os 4 botoes da direita (buscar, recarregar, baixar, fechar).
            assert!(xs.len() >= 6, "w={w}: {icons:?}");
            let botoes = xs[xs.len() - 4];
            let head: Vec<(String, egui::Rect)> =
                drawn_texts(&out).into_iter().filter(|(_, r)| r.top() < 30.0).collect();
            assert!(!head.is_empty(), "w={w}");
            for (t, r) in &head {
                assert!(r.right() <= botoes + 0.5, "w={w}: {t:?} ({r:?}) sobre os botoes em {botoes}");
            }
            // O selo so quando sobra espaco (sai primeiro).
            let selo = head.iter().any(|(t, _)| t == "somente leitura");
            if w >= 900.0 {
                assert!(selo, "w={w}: sem selo com espaco sobrando");
            }
            if w <= 330.0 {
                assert!(!selo, "w={w}: selo num painel estreito");
            }
        }
    }

    /// O indicador da busca por letras nunca cobre o item achado (que a lista
    /// deixa na ultima linha ao rolar ate ele): sobe para o topo.
    #[test]
    fn typeahead_pill_never_covers_found_row() {
        let ctx = light_ctx();
        let mut nomes: Vec<String> = (0..60).map(|i| format!("a{i:02}.txt")).collect();
        nomes.push("zeta.txt".into());
        let refs: Vec<&str> = nomes.iter().map(String::as_str).collect();
        let mut e = explorer_with(&refs);
        nav_frame(&ctx, &mut e, vec![egui::Event::Text("z".into())]);
        settle(&ctx, &mut e);
        e.typeahead_at = Some(Instant::now());
        let (out, _) = nav_frame(&ctx, &mut e, vec![]);
        assert_eq!(cursor_name(&e).as_deref(), Some("zeta.txt"));
        let row = painted_texts(&out)
            .into_iter()
            .find(|(t, _)| t == "zeta.txt")
            .expect("item achado")
            .1;
        let pill = painted_texts(&out)
            .into_iter()
            .find(|(t, r)| t == "z" && r.left() > 400.0)
            .expect("indicador")
            .1;
        let faixa = |r: egui::Rect| (r.center().y - ROW_H / 2.0, r.center().y + ROW_H / 2.0);
        let ((a0, a1), (b0, b1)) = (faixa(row), (pill.top() - 5.0, pill.bottom() + 5.0));
        assert!(a1 <= b0 || b1 <= a0, "indicador ({b0}..{b1}) sobre a linha achada ({a0}..{a1})");
    }

    /// O atalho do `fit_text` (nem medir texto curto) vale: nenhum glifo das
    /// fontes do app passa de 1,5 x o tamanho da fonte.
    #[test]
    fn glyph_advance_bound_of_fit_text() {
        let ctx = light_ctx();
        let _ = ctx.run(light_raw(egui::vec2(200.0, 100.0), vec![]), |_| {});
        let ranges = [0x20u32..0x3000, 0xFE00..0xFFFE, 0x1F000..0x1FB00];
        for font in [egui::FontId::proportional(13.0), egui::FontId::proportional(11.0), egui::FontId::monospace(11.0)] {
            let mut pior = (0.0f32, ' ');
            for c in ranges.iter().cloned().flatten().filter_map(char::from_u32) {
                let w = ctx.fonts(|f| f.glyph_width(&font, c));
                if w > pior.0 {
                    pior = (w, c);
                }
            }
            assert!(
                pior.0 <= font.size * 1.5,
                "{font:?}: U+{:04X} com {} px",
                pior.1 as u32,
                pior.0
            );
        }
    }

    #[test]
    fn visible_cols_by_width() {
        assert_eq!(visible_cols(900.0), [true; 5]);
        assert_eq!(visible_cols(560.0), [true; 5]);
        assert_eq!(visible_cols(500.0), [true, true, false, false, true]);
        assert_eq!(visible_cols(330.0), [false, true, false, false, false]);
        assert_eq!(visible_cols(200.0), [false; 5]);
    }

    /// Nomes cortados pela largura medida: letras largas nao invadem as
    /// colunas; num painel estreito as colunas que nao cabem saem (cabecalho
    /// e linhas iguais) e o nome continua legivel.
    #[test]
    fn names_and_columns_fit_the_row() {
        let ctx = light_ctx();
        let largo = "W".repeat(90);
        let pasta = format!("pasta{}", "M".repeat(80));
        for w in [900.0f32, 480.0, 330.0] {
            let mut e = explorer_with(&[&pasta, &largo, "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii.txt"]);
            for n in &mut e.entries {
                n.date = "13/09/2026".into();
            }
            let mut out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
            for _ in 0..2 {
                out = nav_frame_w(&ctx, &mut e, w, vec![]).0;
            }
            let texts = drawn_texts(&out);
            let head = |t: &str| texts.iter().find(|(s, r)| s == t && r.top() < 80.0).map(|(_, r)| *r);
            let cols = visible_cols(w - 16.0);
            for ((titulo, _), on) in LIST_COLS.into_iter().zip(cols) {
                assert_eq!(head(titulo).is_some(), on, "w={w}: {titulo}");
            }
            // Borda esquerda da coluna mais a esquerda que aparece.
            let mut x = w - 8.0 - 6.0;
            for ((_, cw), on) in LIST_COLS.into_iter().zip(cols) {
                if on {
                    x -= cw;
                }
            }
            for (t, r) in &texts {
                if t.starts_with('W') || t.starts_with("pasta") || t.starts_with("iiii") {
                    assert!(r.right() <= x + 0.5, "w={w}: nome {t:?} ({r:?}) invade as colunas em {x}");
                    assert!(r.left() >= 0.0);
                }
            }
            let pasta_txt = texts.iter().find(|(t, _)| t.starts_with("pasta")).unwrap().0.clone();
            assert!(pasta_txt.ends_with("\u{2026}/"), "w={w}: {pasta_txt:?}");
            assert!(texts.iter().any(|(t, _)| t == "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii.txt") || w < 480.0,
                "w={w}: nome estreito cortado cedo demais");
            // Colunas das linhas dentro da linha (nada a esquerda do icone).
            for (t, r) in &texts {
                if t == "13/09/2026" || t == "0644" || t == "10 B" {
                    assert!(r.left() >= 8.0 + 6.0 + 16.0, "w={w}: {t:?} em {r:?}");
                    assert!(r.right() <= w, "w={w}: {t:?} em {r:?}");
                }
            }
        }
    }

    /// A ocorrencia atual da busca continua legivel (contraste do texto sobre
    /// o fundo dela de pelo menos 4,5:1).
    #[test]
    fn current_match_keeps_contrast() {
        let over = |dst: egui::Color32, src: egui::Color32| {
            let a = src.a() as f32 / 255.0;
            let c = |s: u8, d: u8| (s as f32 + d as f32 * (1.0 - a)).round().min(255.0) as u8;
            egui::Color32::from_rgb(c(src.r(), dst.r()), c(src.g(), dst.g()), c(src.b(), dst.b()))
        };
        let fundo = over(FIELD_BG, HIGHLIGHT.gamma_multiply(CUR_MATCH_FILL));
        let ratio = wcag_contrast(CARD_TEXT, fundo);
        assert!(ratio >= 4.5, "contraste {ratio:.2}");
        // As demais ocorrencias (0,25) continuam mais apagadas que a atual (o
        // contorno tambem a distingue).
        let outras = over(FIELD_BG, HIGHLIGHT.gamma_multiply(0.25));
        assert!(wcag_contrast(CARD_TEXT, outras) > ratio);
    }

    /// O cursor do teclado na ".." (linha nunca marcada) aparece com fundo e
    /// contorno em ACCENT pleno.
    #[test]
    fn cursor_on_up_row_is_visible() {
        let ctx = light_ctx();
        let mut e = explorer_with(&["a.txt", "b.txt"]);
        e.on_up = true;
        let (out, _) = nav_frame(&ctx, &mut e, vec![]);
        let up = text_pos(&out, "..").expect("linha ..");
        let rects: Vec<&egui::epaint::RectShape> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::epaint::Shape::Rect(r) if r.rect.contains(up) && r.brush.is_none() => Some(r),
                _ => None,
            })
            .collect();
        assert!(
            rects.iter().any(|r| r.stroke.width >= 1.5 && r.stroke.color == ACCENT),
            "contorno do cursor fraco"
        );
        assert!(
            rects.iter().any(|r| r.fill == ACCENT.gamma_multiply(0.18)),
            "cursor sem fundo"
        );
        assert!(wcag_contrast(ACCENT, SCREEN_BG) >= 3.0);
    }

    /// Nomes remotos com quebra de linha ou bidi aparecem neutralizados nos
    /// dialogos de permissoes e de proprietario (nao imitam a nota do link
    /// nem disfarcam a extensao). Menus e dialogos com acentos.
    #[test]
    fn attr_dialogs_neutralize_remote_names() {
        let ctx = light_ctx();
        let falso = "a.php\n\nÉ um link simbólico: a alteração vale para o destino (\u{2192} /x)";
        for nome in [falso, "foto\u{202E}gpj.exe"] {
            let node = FsNode {
                label: download::safe_text(nome, 255),
                name: nome.into(),
                ..fs_node("x")
            };
            for dialog in [FsDialog::chmod(&node), FsDialog::chown(&node)] {
                let mut e = explorer_with(&["a.txt"]);
                e.dialog = Some(dialog);
                let mut out = nav_frame(&ctx, &mut e, vec![]).0;
                for _ in 0..2 {
                    out = nav_frame(&ctx, &mut e, vec![]).0;
                }
                let texts: Vec<String> = painted_texts(&out).into_iter().map(|(t, _)| t).collect();
                for t in &texts {
                    assert!(!t.contains('\n') && !t.contains('\u{202E}'), "nome cru: {t:?}");
                }
                assert!(
                    texts.iter().any(|t| t.contains("a.php^J^J") || t.contains("foto<U+202E>gpj.exe")),
                    "{texts:?}"
                );
                assert!(!texts.iter().any(|t| t.contains("Proprietario") || t.contains("numerico")));
            }
        }
    }

    /// Enter e Shift+Enter andam pela busca aberta mesmo com o foco no texto
    /// (a ajuda diz "Enter, F3"); sem busca aberta, Enter nao faz nada.
    #[test]
    fn viewer_enter_steps_when_search_open() {
        let text: String = (0..300)
            .map(|i| if i % 50 == 7 { format!("linha {i}\n") } else { format!("outra {i}\n") })
            .collect();
        let (ctx, mut app, _rx, _tx) = viewer_app(&["a.txt"], 0, &text);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert!(viewer0(&mut app).search.is_none());
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("linha".into())]);
        std::thread::sleep(SEARCH_DEBOUNCE + std::time::Duration::from_millis(30));
        let out = nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(viewer0(&mut app).search.as_ref().unwrap().current, Some(0));
        // Clique no texto: o campo perde o foco, a barra continua aberta.
        let at = visible_texts(&out)
            .into_iter()
            .find(|(t, _)| t == "outra 1")
            .map(|(_, r)| r.center())
            .expect("texto na tela");
        nav_app_frame(&ctx, &mut app, vec![egui::Event::PointerMoved(at), click(at, true)]);
        nav_app_frame(&ctx, &mut app, vec![click(at, false)]);
        nav_app_frame(&ctx, &mut app, vec![]);
        // Digitar nao chega ao campo: ele esta sem foco.
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("x".into())]);
        assert_eq!(viewer0(&mut app).search.as_ref().unwrap().query, "linha");
        let cur = |app: &mut App| viewer0(app).search.as_ref().unwrap().current;
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(cur(&mut app), Some(1), "Enter com a busca aberta");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::Enter, egui::Modifiers::SHIFT)]);
        assert_eq!(cur(&mut app), Some(0), "Shift+Enter com a busca aberta");
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::F3, egui::Modifiers::SHIFT)]);
        assert_eq!(cur(&mut app), Some(5), "Shift+F3 da a volta");
        assert!(viewer0(&mut app).search.is_some());
    }

    /// A barra de caminho nunca troca o caminho em silencio: Enter no
    /// proprio caminho (mesmo com espaco no fim do nome) so atualiza; um
    /// texto com espacos nas pontas tenta primeiro o exato e depois sem eles.
    #[test]
    fn path_bar_keeps_exact_path_and_retries_trimmed() {
        let ctx = light_ctx();
        let (mut app, mut rx, tx) = sftp_app(&["a.txt"]);
        explorer_at(&mut app, &[]).cur_path = "/srv/uploads ".into();
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        sftp_msgs(&mut rx);
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs(&mut rx), ["list /srv/uploads "], "foi para outra pasta");
        assert!(explorer_at(&mut app, &[]).path_edit.is_none());
        // Nome terminado em TAB (valido em POSIX; uma colagem perderia o TAB):
        // o proprio caminho tambem so atualiza.
        explorer_at(&mut app, &[]).cur_path = "/srv/log\t".into();
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs(&mut rx), ["list /srv/log\t"], "foi para outra pasta");
        // Colado com espaco no fim: o exato, e sem o espaco se ele nao abrir.
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("/etc ".into())]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs(&mut rx), ["goto 1 /etc "]);
        tx.send(SftpToUi::Goto {
            seq: 1,
            result: Err("Caminho não encontrado: /etc ".into()),
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(sftp_msgs(&mut rx), ["goto 2 /etc"]);
        assert!(explorer_at(&mut app, &[]).path_edit.as_ref().is_some_and(|p| p.error.is_none()));
        tx.send(SftpToUi::Goto { seq: 2, result: Ok(sftp::GotoKind::Dir) }).unwrap();
        tx.send(SftpToUi::Listing { path: "/etc".into(), entries: vec![remote_entry("hosts")] }).unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert_eq!(explorer_at(&mut app, &[]).cur_path, "/etc");
        // Sem alternativa: o erro fica na barra.
        for _ in 0..3 {
            nav_app_frame(&ctx, &mut app, vec![]);
        }
        nav_app_frame(&ctx, &mut app, vec![key(egui::Key::L, ctrl_win())]);
        nav_app_frame(&ctx, &mut app, vec![]);
        nav_app_frame(&ctx, &mut app, vec![egui::Event::Text("/nada".into())]);
        nav_app_frame(&ctx, &mut app, vec![enter()]);
        assert_eq!(sftp_msgs(&mut rx), ["goto 3 /nada"]);
        tx.send(SftpToUi::Goto {
            seq: 3,
            result: Err("Caminho não encontrado: /nada".into()),
        })
        .unwrap();
        nav_app_frame(&ctx, &mut app, vec![]);
        assert!(sftp_msgs(&mut rx).is_empty());
        let erro = explorer_at(&mut app, &[]).path_edit.as_ref().and_then(|p| p.error.clone());
        assert_eq!(erro.as_deref(), Some("Caminho não encontrado: /nada"));
    }

    /// Renomear usa a pasta do proprio item: navegar com o dialogo aberto nao
    /// move o arquivo para a pasta nova.
    #[test]
    fn rename_stays_in_items_folder() {
        let ctx = light_ctx();
        let mut e = explorer_with(&["a.txt"]);
        e.dialog = Some(FsDialog::Rename {
            path: "/srv/a.txt".into(),
            name: "b.txt".into(),
        });
        e.cur_path = "/outra".into();
        let mut out = nav_frame(&ctx, &mut e, vec![]).0;
        for _ in 0..2 {
            out = nav_frame(&ctx, &mut e, vec![]).0;
        }
        let at = painted_texts(&out)
            .into_iter()
            .filter(|(s, _)| s == "Renomear")
            .map(|(_, r)| r.center())
            .max_by(|a, b| a.y.total_cmp(&b.y))
            .expect("botao Renomear");
        nav_frame(&ctx, &mut e, vec![egui::Event::PointerMoved(at), click(at, true)]);
        let (_, out) = nav_frame(&ctx, &mut e, vec![click(at, false)]);
        match out.op {
            Some(FsOp::Rename { from, to }) => {
                assert_eq!((from.as_str(), to.as_str()), ("/srv/a.txt", "/srv/b.txt"));
            }
            _ => panic!("Renomear nao confirmou"),
        }
    }

    /// Quadro da tela de conexoes (tema claro do Windows, como o do usuario).
    fn hosts_frame(
        ctx: &egui::Context,
        app: &mut App,
        size: egui::Vec2,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(light_raw(size, events), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| app.ui_hosts(ui));
        })
    }

    /// Cofre de senha "t" com um host, gravado em `dir`; devolve o caminho dele
    /// e o do arquivo de chaves do "abrir sem senha" (na mesma pasta).
    fn remember_setup(dir: &TempDir) -> (PathBuf, PathBuf) {
        let path = dir.0.join("cofre.sagu");
        let mut v = Vault::default();
        v.hosts.push(test_host());
        let bytes = vault::encrypt_vault(&v, &VaultKey::new("t").unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        (path, dir.0.join("data").join("remembered.json"))
    }

    /// App recem-iniciado (na splash), com o ultimo cofre e o arquivo de chaves.
    fn starting_app(path: &std::path::Path, file: &std::path::Path) -> App {
        let mut app = app();
        app.screen = Screen::Splash;
        app.root = None;
        app.last_vault_path = path.to_string_lossy().to_string();
        app.gate_path = app.last_vault_path.clone();
        app.remember_file = Some(file.to_path_buf());
        app
    }

    fn open_with_password(app: &mut App, password: &str) {
        app.gate_mode = GateMode::Open;
        app.gate_password = password.into();
        app.gate_submit();
    }

    /// Com a opcao ligada, o app abre o cofre sem a senha ate ser bloqueado; a
    /// senha religa; desligar volta a pedir a senha.
    #[cfg(windows)]
    #[test]
    fn remember_opens_without_password_until_locked() {
        let dir = temp_dir("lembrar");
        let (path, file) = remember_setup(&dir);

        // Sem a opcao: da splash para o portao, que pede a senha.
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error, None);
        open_with_password(&mut app, "t");
        assert!(matches!(app.screen, Screen::Hosts), "{:?}", app.gate_error);
        assert!(!app.remembered);
        assert!(app.gate_password.is_empty());

        // Liga; gravar mantem o sal, entao a chave guardada continua valendo.
        app.set_remembered(true);
        assert!(app.remembered);
        app.vault.hosts[0].name = "Renomeado".into();
        app.save_vault().unwrap();

        // Proxima abertura do app: entra direto, com o cofre gravado.
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Hosts), "{:?}", app.gate_error);
        assert!(app.remembered);
        assert_eq!(app.vault.hosts[0].name, "Renomeado");
        assert_eq!(app.vault_path.as_deref(), Some(path.as_path()));
        // A senha continua abrindo o arquivo.
        let (back, _) = vault::decrypt_vault(&std::fs::read(&path).unwrap(), "t").unwrap();
        assert_eq!(back.hosts[0].name, "Renomeado");

        // Bloquear: a proxima abertura pede a senha...
        app.lock();
        assert!(app.master_key.is_none() && !app.remembered);
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error, None);
        // ...e digita-la religa a abertura automatica.
        open_with_password(&mut app, "t");
        assert!(app.remembered);
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Hosts));

        // Desligar: volta a pedir a senha.
        app.set_remembered(false);
        assert!(!app.remembered);
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error, None);
    }

    /// Chave guardada que nao serve mais: cofre regravado com outro sal (versao
    /// anterior do app) pede a senha em silencio; chave adulterada avisa e e
    /// apagada; cofre que sumiu so cai no portao.
    #[cfg(windows)]
    #[test]
    fn remember_stale_or_broken_key_asks_password() {
        let dir = temp_dir("lembrar-velho");
        let (path, file) = remember_setup(&dir);
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        open_with_password(&mut app, "t");
        app.set_remembered(true);

        // Outro sal.
        let (v, _) = vault::decrypt_vault(&std::fs::read(&path).unwrap(), "t").unwrap();
        let bytes = vault::encrypt_vault(&v, &VaultKey::new("t").unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error, None);
        open_with_password(&mut app, "t");
        assert!(!app.remembered, "sal novo: a opcao fica desligada");

        // Blob da DPAPI adulterado (ultimo digito trocado).
        app.set_remembered(true);
        let salt = app.master_key.as_ref().unwrap().salt().to_vec();
        let salt_hex: String = salt.iter().map(|b| format!("{b:02x}")).collect();
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        for e in json["vaults"].as_array_mut().unwrap() {
            if e["salt"] == salt_hex.as_str() {
                let k = e["key"].as_str().unwrap().to_string();
                let last = if k.ends_with('0') { "1" } else { "0" };
                e["key"] = format!("{}{last}", &k[..k.len() - 1]).into();
            }
        }
        std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error.as_deref(), Some(AUTO_OPEN_FAILED));
        assert_eq!(remember::state(&file, &salt), remember::State::Off);
        open_with_password(&mut app, "t");
        assert!(matches!(app.screen, Screen::Hosts));
        assert_eq!(app.gate_error, None);

        // Ultimo cofre sumiu.
        app.set_remembered(true);
        std::fs::remove_file(&path).unwrap();
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        assert!(matches!(app.screen, Screen::Gate));
        assert_eq!(app.gate_error, None);
    }

    /// A caixa da tela de conexoes liga e desliga a opcao; cabe inteira na
    /// janela minima, a direita do caminho do cofre (que e cortado).
    #[cfg(windows)]
    #[test]
    fn remember_checkbox_on_hosts_screen() {
        let dir = temp_dir("lembrar-caixa");
        let (path, file) = remember_setup(&dir);
        let ctx = light_ctx();
        let mut app = starting_app(&path, &file);
        app.leave_splash();
        open_with_password(&mut app, "t");
        let salt = app.master_key.as_ref().unwrap().salt().to_vec();

        let min = egui::vec2(640.0, 420.0);
        let click_option = |ctx: &egui::Context, app: &mut App| {
            let out = hosts_frame(ctx, app, min, vec![]);
            let texts = painted_texts(&out);
            let opt = texts.iter().find(|(t, _)| t == REMEMBER_LABEL).expect("opcao pintada").1;
            assert!(opt.right() <= min.x, "{opt:?}");
            let caminho = texts
                .iter()
                .find(|(t, _)| t.starts_with('\u{1f5c4}'))
                .expect("caminho do cofre")
                .1;
            assert!(caminho.right() <= opt.left(), "caminho invade a opcao: {caminho:?} {opt:?}");
            assert!((caminho.center().y - opt.center().y).abs() < 4.0, "mesma linha");
            let pos = opt.center();
            hosts_frame(ctx, app, min, vec![egui::Event::PointerMoved(pos), click(pos, true)]);
            hosts_frame(ctx, app, min, vec![click(pos, false)]);
        };
        for _ in 0..2 {
            hosts_frame(&ctx, &mut app, min, vec![]);
        }
        click_option(&ctx, &mut app);
        assert!(app.remembered, "{:?}", app.hosts_error);
        assert_eq!(remember::state(&file, &salt), remember::State::On);
        click_option(&ctx, &mut app);
        assert!(!app.remembered);
        assert_eq!(remember::state(&file, &salt), remember::State::Off);

        // Sem pasta de dados do app: a opcao nao aparece.
        app.remember_file = None;
        let out = hosts_frame(&ctx, &mut app, min, vec![]);
        assert!(!painted_texts(&out).iter().any(|(t, _)| t == REMEMBER_LABEL));
    }
}
