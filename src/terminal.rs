//! Emulador de terminal: parser VT100 + renderizacao em egui.
//!
//! Caracteristicas:
//! - desenha a grade de celulas com fonte monoespacada;
//! - encaminha teclado (incluindo setas, teclas de funcao e Ctrl+letra);
//! - **copia automaticamente** o texto selecionado para a area de transferencia
//!   ao soltar o botao do mouse;
//! - redimensiona o PTY conforme a area disponivel;
//! - traduz antes do parser as sequencias de cursor que o vt100 ignora (ver
//!   `vtfix`) e desenha os caracteres braille, que a fonte nao tem;
//! - guarda um historico da tela principal (`SCROLLBACK_LINES` linhas), lido
//!   com a roda do mouse e Shift+PgUp/PgDn/Home/End. Na tela alternativa
//!   (tmux, less, htop...) nao ha historico: a roda vira evento de mouse se o
//!   programa pediu mouse; senao, aparece uma dica.

use std::time::Duration;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Vec2};

use crate::emoji;
use crate::vtfix::{Piece, VtFix};

/// Cor de fundo padrao do terminal (compartilhada por render e resolucao de
/// cores; celulas com este fundo nao precisam pintar retangulo). Grafite bem
/// escuro, alinhado com a paleta da aplicacao.
const TERM_BG: Color32 = Color32::from_rgb(0x13, 0x13, 0x16);

/// Linhas guardadas no historico da tela principal, por painel. O vt100 gasta
/// 32 bytes por celula: 5.000 linhas custam ~19 MB num painel de 120 colunas
/// e ~31 MB num de 200, e so quando a saida enche o historico.
pub const SCROLLBACK_LINES: usize = 5_000;

/// Linhas por "clique" da roda (o padrao do Windows). Na tela alternativa com
/// mouse pedido, cada clique vira um evento de roda para o programa.
const WHEEL_LINES: f32 = 3.0;

/// Por quanto tempo (s) a dica da tela alternativa fica visivel.
const HINT_SECS: f64 = 4.0;

/// Velocidade da rolagem ao arrastar a selecao alem da borda: linhas por
/// segundo por celula de distancia (ate 10 celulas).
const AUTOSCROLL_RATE: f32 = 20.0;

/// Dica da tela alternativa sem historico, da mais larga a mais estreita
/// (vale a primeira que couber, ver `pill`).
const HINTS: [&str; 3] = [
    "Esta tela não tem histórico (programa em tela cheia).\n\
     tmux: Ctrl+B, Ctrl+B, [ para rolar (q sai)  ·  less/man: PgUp/PgDn",
    "Sem histórico nesta tela.\ntmux: Ctrl+B, Ctrl+B, [",
    "Sem histórico",
];

/// Fundo dos avisos: opaco (translucido, o texto de baixo aparecia) e
/// escuro fixo, como o terminal em qualquer tema.
const PILL_BG: Color32 = Color32::from_rgb(0x1f, 0x1f, 0x24);

/// Resultado de um quadro do terminal: o que precisa ser enviado ao servidor.
#[derive(Default)]
pub struct TerminalOutput {
    /// Bytes digitados/colados para enviar ao canal SSH.
    pub input: Vec<u8>,
    /// Novo tamanho (cols, rows) caso a area tenha mudado.
    pub resize: Option<(u16, u16)>,
    /// Verdadeiro quando este terminal detem o foco do teclado neste quadro.
    pub focused: bool,
}

/// Tamanho do historico do vt100 (ele so expoe o deslocamento, que satura
/// no tamanho).
fn hist_len(s: &mut vt100::Screen) -> usize {
    let v = s.scrollback();
    s.set_scrollback(usize::MAX);
    let n = s.scrollback();
    s.set_scrollback(v);
    n
}

/// Linhas que subiram para o historico num trecho, pelo tamanho antes
/// (`len0`) e depois (`len`) e pela sentinela (deslocamento posto em 1 antes
/// do trecho: o vt100 soma 1 a cada linha empurrada, saturando no tamanho).
/// `None` quando nao da para saber: o historico transbordou no trecho ou a
/// visao zerou (ESC c, troca de tela).
fn pushed_since(len0: usize, sentinel: usize, len: usize, cap: usize) -> Option<usize> {
    if len0 > 0 && sentinel == 0 {
        return None;
    }
    if len < cap {
        // Nada saiu pelo topo: a diferenca e exata.
        return len.checked_sub(len0);
    }
    if len0 == 0 {
        return None;
    }
    (sentinel < len).then(|| sentinel - 1)
}

/// Ganchos do vt100: o `CSI 3 J` (ED 3, "apagar as linhas guardadas", que o
/// `clear` do ncurses manda antes do `CSI H CSI 2 J`) cai em `unhandled_csi`.
/// A contagem do trecho recomeca nele (nova base e sentinela rearmada): o
/// que subir depois e o que fica visivel.
#[derive(Default)]
struct Hooks {
    cap: usize,
    /// Base da contagem em andamento: tamanho do historico no inicio do
    /// trecho ou no ultimo ED 3 dele. `None` fora de `feed`. Um ED 3 na
    /// tela alternativa nao conta (ela nao tem historico).
    base: Option<usize>,
    /// Linhas empurradas no trecho antes da base atual (`None`: incerto).
    before: Option<usize>,
    /// Houve ED 3 no trecho.
    ed3: bool,
    /// A visao zerou no trecho antes da base atual (ESC c, troca de tela).
    reset: bool,
}

impl Hooks {
    fn start(&mut self, len0: usize) {
        self.base = Some(len0);
        self.before = Some(0);
        self.ed3 = false;
        self.reset = false;
    }

    /// Trecho que comeca na tela alternativa: o historico da principal nao
    /// e legivel daqui. So um ED 3 depois de voltar a ela no trecho conta
    /// (e so o que sobe depois dele importa).
    fn start_unknown(&mut self) {
        self.start(0);
        self.before = None;
    }
}

impl vt100::Callbacks for Hooks {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        if c != 'J' || i1.is_some() || i2.is_some() || params != [&[3u16][..]] {
            return;
        }
        let Some(base) = self.base else {
            return;
        };
        if screen.alternate_screen() {
            return;
        }
        let sentinel = screen.scrollback();
        let len = hist_len(screen);
        if base > 0 && sentinel == 0 {
            self.reset = true;
        }
        self.before = self
            .before
            .zip(pushed_since(base, sentinel, len, self.cap))
            .map(|(a, b)| a + b);
        self.ed3 = true;
        self.base = Some(len);
        screen.set_scrollback(1);
    }
}

/// Acao de uma tecla de rolagem local.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollKey {
    PageUp,
    PageDown,
    Top,
    Bottom,
}

/// Shift+PgUp/PgDn/Home/End (com ou sem Ctrl) rolam o historico.
fn scroll_key(key: egui::Key, mods: &egui::Modifiers) -> Option<ScrollKey> {
    if !mods.shift || mods.alt {
        return None;
    }
    Some(match key {
        egui::Key::PageUp => ScrollKey::PageUp,
        egui::Key::PageDown => ScrollKey::PageDown,
        egui::Key::Home => ScrollKey::Top,
        egui::Key::End => ScrollKey::Bottom,
        _ => return None,
    })
}

/// Evento de roda (botao 64 = para cima, 65 = para baixo, +8 com Alt) na
/// celula `col`,`row` (a partir de 1), na codificacao pedida pelo programa.
fn wheel_event(button: u8, col: u16, row: u16, enc: vt100::MouseProtocolEncoding) -> Vec<u8> {
    match enc {
        vt100::MouseProtocolEncoding::Sgr => format!("\x1b[<{button};{col};{row}M").into_bytes(),
        vt100::MouseProtocolEncoding::Utf8 => {
            let mut v = b"\x1b[M".to_vec();
            v.push(32 + button);
            for n in [col, row] {
                let c = char::from_u32(32 + u32::from(n.min(2015))).unwrap_or(' ');
                let mut buf = [0u8; 4];
                v.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            v
        }
        vt100::MouseProtocolEncoding::Default => {
            // X10: um byte por coordenada; alem de 223 nao cabe.
            vec![
                0x1b,
                b'[',
                b'M',
                32 + button,
                (32 + col.min(223)) as u8,
                (32 + row.min(223)) as u8,
            ]
        }
    }
}

/// Numero com separador de milhar (5.000).
fn thousands(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(ch);
    }
    out
}

pub struct Terminal {
    parser: vt100::Parser<Hooks>,
    /// Traduz HVP e SCOSC/SCORC (usados pelo btop) antes do parser.
    vtfix: VtFix,
    /// Atributos (SGR) da tela principal ao entrar na alternativa por
    /// `CSI ? 1049 h`, devolvidos na saida (ver `process`).
    primary_attrs: Option<Vec<u8>>,
    cols: u16,
    rows: u16,
    font_size: f32,
    /// Selecao em (linha absoluta, coluna): ver `abs_row`.
    sel_anchor: Option<(i64, u16)>,
    sel_head: Option<(i64, u16)>,
    want_focus: bool,
    /// Tamanho maximo do historico (linhas).
    cap: usize,
    /// Linhas que ja subiram para o historico da principal desde o inicio:
    /// origem das linhas absolutas da selecao.
    pushed: i64,
    /// Linhas hoje no historico do vt100.
    hist: usize,
    /// As mais antigas delas escondidas por um ED 3 (o vt100 nao as apaga).
    hidden: usize,
    /// Deslocamento da visao, em linhas acima do fim (0 = acompanhando).
    view: usize,
    /// Chegou saida com a visao no historico.
    unseen: bool,
    /// Resto da roda ainda nao usado (touchpad), em linhas.
    wheel_acc: f32,
    /// Ate quando (tempo do egui) mostrar a dica da tela alternativa.
    hint_until: f64,
    /// Saiu da alternativa num trecho que nao foi so o final do 1049 (ESC c,
    /// CSI ? 47 l): as escondidas deixam de valer (ver `process`).
    exit_unsure: bool,
    /// Rolagem ao arrastar alem da borda: tempo do ultimo passo e resto
    /// (fracao de linha) ainda nao usado.
    autoscroll_at: Option<f64>,
    autoscroll_acc: f32,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self::with_scrollback(cols, rows, SCROLLBACK_LINES)
    }

    fn with_scrollback(cols: u16, rows: u16, cap: usize) -> Self {
        let hooks = Hooks {
            cap,
            ..Hooks::default()
        };
        Terminal {
            parser: vt100::Parser::new_with_callbacks(rows, cols, cap, hooks),
            vtfix: VtFix::default(),
            primary_attrs: None,
            cols,
            rows,
            font_size: 14.0,
            sel_anchor: None,
            sel_head: None,
            want_focus: true,
            cap,
            pushed: 0,
            hist: 0,
            hidden: 0,
            view: 0,
            unseen: false,
            wheel_acc: 0.0,
            hint_until: f64::NEG_INFINITY,
            exit_unsure: false,
            autoscroll_at: None,
            autoscroll_acc: 0.0,
        }
    }

    /// Saida da alternativa sem o aviso do 1049 logo depois: pode ter sido
    /// um ESC c (historico novo), entao nada fica escondido.
    fn settle_exit(&mut self) {
        if self.exit_unsure {
            self.exit_unsure = false;
            self.hidden = 0;
        }
    }

    /// Bytes do servidor (ou do PTY local), em pedacos de qualquer tamanho.
    pub fn process(&mut self, bytes: &[u8]) {
        let mut vtfix = std::mem::take(&mut self.vtfix);
        // Tela alternativa antes do ultimo trecho (o aviso vem logo depois
        // do trecho que so tem o byte final do 1049).
        let mut was_alt = false;
        vtfix.feed(bytes, |piece| match piece {
            Piece::Bytes(chunk) => {
                self.settle_exit();
                was_alt = self.parser.screen().alternate_screen();
                self.feed(chunk);
            }
            // O vt100 guarda um so conjunto de atributos salvos para as duas
            // telas: um ESC 7 (ou CSI s, como nas caixas de mensagem do btop)
            // na alternativa apaga o que o 1049h salvou da principal, e o
            // prompt voltaria com as cores do programa. Como no xterm (um
            // cursor salvo por tela), a copia daqui volta na saida.
            Piece::AltScreen(true) => {
                if !was_alt && self.parser.screen().alternate_screen() {
                    self.primary_attrs = Some(self.parser.screen().attributes_formatted());
                }
            }
            Piece::AltScreen(false) => {
                // O trecho anterior foi so o final do 1049: saida normal.
                self.exit_unsure = false;
                if was_alt && !self.parser.screen().alternate_screen() {
                    if let Some(attrs) = self.primary_attrs.take() {
                        self.parser.process(&attrs);
                        // E no salvo da principal: o cursor acabou de voltar
                        // a posicao salva, que o ESC 7 so repete.
                        self.parser.process(b"\x1b7");
                    }
                }
            }
        });
        self.vtfix = vtfix;
        self.settle_exit();
    }

    /// Um trecho para o parser, contando as linhas que sobem para o
    /// historico da principal (ver `pushed_since`).
    fn feed(&mut self, chunk: &[u8]) {
        let s = self.parser.screen_mut();
        if s.alternate_screen() {
            self.parser.callbacks_mut().start_unknown();
            self.parser.process(chunk);
            let hooks = self.parser.callbacks_mut();
            let ed3 = hooks.ed3;
            let ed3_base = hooks.base.take().filter(|_| ed3);
            if !self.parser.screen().alternate_screen() {
                // Voltou a principal: pelo final do 1049 (o aviso vem logo
                // depois e confirma) ou por ESC c / CSI ? 47 l.
                let s = self.parser.screen_mut();
                let sentinel = s.scrollback();
                self.hist = hist_len(s);
                self.hidden = self.hidden.min(self.hist);
                self.exit_unsure = true;
                if let Some(base) = ed3_base {
                    // ED 3 depois da volta, no mesmo trecho: visiveis so as
                    // que subiram depois dele, qualquer que tenha sido a saida.
                    let last = pushed_since(base, sentinel, self.hist, self.cap);
                    self.hidden = last.map_or(0, |n| self.hist - n.min(self.hist));
                    self.exit_unsure = false;
                }
                self.switched_screen();
                self.parser.screen_mut().set_scrollback(0);
            }
            return;
        }
        let len0 = hist_len(s);
        // Sentinela: com o deslocamento em 1, o vt100 soma 1 a cada linha
        // empurrada (e o que ele faz para a visao nao andar enquanto se le).
        s.set_scrollback(1);
        self.parser.callbacks_mut().start(len0);
        self.parser.process(chunk);
        let hooks = self.parser.callbacks_mut();
        let base = hooks.base.take().unwrap_or(len0);
        let before = hooks.before;
        let ed3 = hooks.ed3;
        let mut reset = hooks.reset;
        let s = self.parser.screen_mut();
        if s.alternate_screen() {
            // Entrou na alternativa (o vt100 ja voltou a visao da principal
            // ao fim, e o historico dela nao e mais legivel daqui). No 1049 o
            // trecho e so o byte final (o VtFix o isola): nada subiu e as
            // escondidas continuam valendo. Noutro trecho (?47h, ?1047h) o
            // que subiu antes nele fica sem contagem.
            if ed3 {
                // Houve um ED 3 no trecho, antes da troca: escondidas sao as
                // que havia nele. Se depois dele saiu algo pelo topo, seriam
                // menos: fica o lado do clear (esconde a mais, nunca mostra o
                // que ele apagou). O historico tinha ao menos essas linhas.
                self.hist = self.hist.max(base);
                self.hidden = base;
            } else {
                // Se pode ter saido algo pelo topo, nada fica escondido
                // (mostra tudo).
                let most = chunk.len().saturating_mul(usize::from(self.rows));
                if chunk.len() > 1 && len0.saturating_add(most) >= self.cap {
                    self.hidden = 0;
                }
            }
            self.switched_screen();
            return;
        }
        let len1 = hist_len(s);
        let sentinel = s.scrollback();
        if base > 0 && sentinel == 0 {
            reset = true;
        }
        // Empurradas desde a base (inicio do trecho ou ultimo ED 3) e no
        // trecho todo.
        let last = pushed_since(base, sentinel, len1, self.cap);
        let k = before.zip(last).map(|(a, b)| a + b);
        self.hidden = if ed3 {
            // Visiveis: so as que subiram depois do ultimo ED 3.
            last.map_or(0, |n| len1 - n.min(len1))
        } else {
            // As que sairam pelo topo saem primeiro das escondidas.
            k.map_or(0, |k| self.hidden.saturating_sub(len0 + k - len1))
        }
        .min(len1);
        self.hist = len1;
        if ed3 {
            // O que se lia sumiu com o historico: volta ao fim, como no xterm.
            self.view = 0;
            self.unseen = false;
        }
        if reset {
            // ESC c (historico apagado pelo vt100) ou ida e volta a
            // alternativa por CSI ? 47 h/l: recomeca do fim.
            self.pushed += (len1 + usize::from(self.rows)) as i64;
            self.switched_screen();
            self.parser.screen_mut().set_scrollback(0);
            return;
        }
        match k {
            // A visao fica no mesmo texto enquanto o usuario le.
            Some(k) => {
                self.pushed += k as i64;
                if self.view > 0 {
                    self.view += k;
                }
            }
            // Transbordou no trecho: a contagem absoluta se perde (a
            // selecao some) e quem lia fica no topo.
            None => {
                self.pushed += (len1 + usize::from(self.rows)) as i64;
                self.sel_anchor = None;
                self.sel_head = None;
                if self.view > 0 {
                    self.view = usize::MAX;
                }
            }
        }
        if self.view > 0 {
            self.unseen = true;
        }
        self.view = self.view.min(self.avail());
        self.parser.screen_mut().set_scrollback(self.view);
    }

    /// Trocou de tela: a visao volta ao fim e a selecao (de outra tela) some,
    /// assim como a dica da tela alternativa (que nao vale para a outra).
    fn switched_screen(&mut self) {
        self.view = 0;
        self.unseen = false;
        self.sel_anchor = None;
        self.sel_head = None;
        self.hint_until = f64::NEG_INFINITY;
    }

    /// Linhas do historico que a visao alcanca.
    fn avail(&self) -> usize {
        self.hist.saturating_sub(self.hidden)
    }

    /// Linha absoluta da fileira `row` da visao.
    fn abs_row(&self, row: u16) -> i64 {
        self.pushed - self.view as i64 + i64::from(row)
    }

    fn set_view(&mut self, view: usize) {
        self.view = view.min(self.avail());
        if self.view == 0 {
            self.unseen = false;
        }
        self.parser.screen_mut().set_scrollback(self.view);
    }

    fn scroll_by(&mut self, lines: isize) {
        let v = (self.view as isize).saturating_add(lines).max(0) as usize;
        self.set_view(v);
    }

    /// Volta a visao ao fim (acompanhando a saida).
    pub fn scroll_to_bottom(&mut self) {
        self.set_view(0);
    }

    /// Pede o foco do teclado para este terminal no proximo quadro (usado
    /// pelos atalhos de troca de painel).
    pub fn take_focus(&mut self) {
        self.want_focus = true;
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        // Encolhendo com o cursor abaixo da nova ultima linha: as de cima
        // sobem para o historico (como no xterm), em vez de o vt100 cortar as
        // de baixo, onde estao o prompt e a saida mais recente. So com o
        // tradutor sem nada pendente (o CSI injetado nao pode cair no meio de
        // uma sequencia do servidor).
        let s = self.parser.screen();
        if rows < self.rows && !s.alternate_screen() && self.vtfix.idle() {
            let (cr, _) = s.cursor_position();
            if cr >= rows {
                let k = cr + 1 - rows;
                let unseen = self.unseen;
                self.feed(format!("\x1b[{k}S\x1b[{k}A").as_bytes());
                self.unseen = unseen;
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
        self.sel_anchor = None;
        self.sel_head = None;
        let v = self.view;
        self.set_view(v);
    }

    /// Arrastando a selecao `cells` celulas acima (positivo) ou abaixo do
    /// terminal: `AUTOSCROLL_RATE` linhas por segundo por celula. Conta o
    /// tempo, nao os quadros: eles vem mais depressa com o mouse mexendo ou
    /// com saida chegando, e a velocidade nao pode depender disso.
    fn autoscroll(&mut self, cells: f32, now: f64) {
        let lines = match self.autoscroll_at {
            // Acabou de sair da borda: uma linha na hora.
            None => cells.signum(),
            Some(t0) => {
                let dt = (now - t0).clamp(0.0, 0.25) as f32;
                self.autoscroll_acc + AUTOSCROLL_RATE * cells.clamp(-10.0, 10.0) * dt
            }
        };
        self.autoscroll_at = Some(now);
        let whole = lines.trunc();
        self.autoscroll_acc = lines - whole;
        self.scroll_by(whole as isize);
    }

    /// Roda do mouse sobre a celula (`row`, `col`) da visao. Bytes para o
    /// programa (se ele pediu mouse) vao em `out`.
    fn wheel(
        &mut self,
        (unit, dy, mods): (egui::MouseWheelUnit, f32, egui::Modifiers),
        (row, col): (u16, u16),
        cell_h: f32,
        now: f64,
        out: &mut Vec<u8>,
    ) {
        // Ctrl+roda e zoom no egui: fica livre.
        if mods.ctrl || mods.command || dy == 0.0 || !dy.is_finite() {
            return;
        }
        let lines = match unit {
            egui::MouseWheelUnit::Line => dy * WHEEL_LINES,
            egui::MouseWheelUnit::Point => dy / cell_h,
            egui::MouseWheelUnit::Page => dy * f32::from(self.rows),
        };
        if self.wheel_acc != 0.0 && self.wheel_acc.signum() != lines.signum() {
            self.wheel_acc = 0.0;
        }
        self.wheel_acc += lines;
        let screen = self.parser.screen();
        let alt = screen.alternate_screen();
        let mouse = screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None;
        if mouse && !mods.shift && self.view == 0 {
            // O programa pediu mouse: um evento por clique, como o xterm.
            let clicks = (self.wheel_acc / WHEEL_LINES).trunc();
            if clicks != 0.0 {
                self.wheel_acc -= clicks * WHEEL_LINES;
                let button = if clicks > 0.0 { 64 } else { 65 } + if mods.alt { 8 } else { 0 };
                let enc = screen.mouse_protocol_encoding();
                for _ in 0..clicks.abs() as usize {
                    out.extend_from_slice(&wheel_event(button, col + 1, row + 1, enc));
                }
            }
            return;
        }
        if alt {
            // Sem historico e sem mouse pedido: setas iriam ao shell dentro
            // do tmux (historico de comandos). So a dica.
            self.wheel_acc = 0.0;
            self.hint_until = now + HINT_SECS;
            return;
        }
        let whole = self.wheel_acc.trunc();
        self.wheel_acc -= whole;
        self.scroll_by(whole as isize);
    }

    /// Desenha o terminal e processa entrada. Retorna o que enviar ao servidor.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> TerminalOutput {
        let mut out = TerminalOutput::default();

        let font_id = FontId::monospace(self.font_size);
        // A celula e arredondada para um numero inteiro de pixels fisicos.
        //
        // Isso nao e cosmetico: ao compor um texto, o egui arredonda a posicao
        // de cada glifo para o pixel (epaint text_layout.rs), ou seja, avanca
        // sempre `round(advance)`. Com uma celula fracionaria (ex.: 8.275) o
        // texto anda 8 px por caractere enquanto a grade anda 8.275, e a
        // diferenca acumula: em 16 colunas o texto ja fica meio caractere a
        // esquerda do cursor, que e desenhado sobre a grade. Casando a celula
        // com o passo real do egui, glifos, fundos e cursor ficam alinhados.
        let ppp = ui.ctx().pixels_per_point();
        let (cell_w, cell_h) = ui.fonts(|f| cell_size(f, &font_id, ppp));

        let avail = ui.available_size();
        let new_cols = ((avail.x / cell_w).floor() as i32).clamp(1, 1000) as u16;
        let new_rows = ((avail.y / cell_h).floor() as i32).clamp(1, 1000) as u16;
        if new_cols != self.cols || new_rows != self.rows {
            self.resize(new_cols, new_rows);
            out.resize = Some((new_cols, new_rows));
        }

        let size = Vec2::new(self.cols as f32 * cell_w, self.rows as f32 * cell_h);
        let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
        let painter = ui.painter_at(rect);
        let now = ui.input(|i| i.time);

        // Fundo geral do terminal.
        painter.rect_filled(rect, CornerRadius::ZERO, TERM_BG);

        // Foco do teclado.
        if self.want_focus {
            response.request_focus();
            self.want_focus = false;
        }
        if response.clicked() {
            response.request_focus();
        }
        out.focused = response.has_focus();
        if response.has_focus() {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                );
            });
        }

        let (cols, rows) = (self.cols, self.rows);
        let pos_to_cell = move |p: Pos2| -> (u16, u16) {
            let c = (((p.x - rect.min.x) / cell_w).floor() as i32).clamp(0, cols as i32 - 1);
            let r = (((p.y - rect.min.y) / cell_h).floor() as i32).clamp(0, rows as i32 - 1);
            (r as u16, c as u16)
        };

        // --- Roda do mouse: vale sobre o terminal, com ou sem foco ---
        let mut wheel_bytes = Vec::new();
        if response.contains_pointer() {
            let (events, pointer) = ui.input(|i| {
                let ev: Vec<_> = i
                    .events
                    .iter()
                    .filter_map(|e| match e {
                        egui::Event::MouseWheel {
                            unit,
                            delta,
                            modifiers,
                        } => Some((*unit, delta.y, *modifiers)),
                        _ => None,
                    })
                    .collect();
                (ev, i.pointer.hover_pos())
            });
            let cell = pointer.map(pos_to_cell).unwrap_or((0, 0));
            for ev in events {
                self.wheel(ev, cell, cell_h, now, &mut wheel_bytes);
            }
        }

        // --- Selecao com auto-copia (em linhas absolutas) ---
        if response.drag_started() {
            // Ancora onde o botao desceu (o arrasto so e reconhecido alguns
            // pixels depois).
            let origin = ui.input(|i| i.pointer.press_origin());
            if let Some(p) = origin.or(response.interact_pointer_pos()) {
                let (r, c) = pos_to_cell(p);
                self.sel_anchor = Some((self.abs_row(r), c));
                self.sel_head = self.sel_anchor;
            }
        }
        let mut beyond = 0.0;
        if response.dragged() {
            if let Some(p) = response.interact_pointer_pos() {
                // Arrastando acima/abaixo do terminal: a visao rola junto.
                if !self.parser.screen().alternate_screen() {
                    beyond = if p.y < rect.min.y {
                        ((rect.min.y - p.y) / cell_h).ceil()
                    } else if p.y > rect.max.y {
                        -((p.y - rect.max.y) / cell_h).ceil()
                    } else {
                        0.0
                    };
                    if beyond != 0.0 {
                        self.autoscroll(beyond, now);
                        ui.ctx().request_repaint_after(Duration::from_millis(50));
                    }
                }
                let (r, c) = pos_to_cell(p);
                self.sel_head = Some((self.abs_row(r), c));
            }
        }
        if beyond == 0.0 {
            self.autoscroll_at = None;
        }
        if response.drag_stopped() {
            if let Some(text) = self.selection_text() {
                if !text.is_empty() {
                    ui.ctx().copy_text(text);
                }
            }
        }
        // Um clique simples (sem arrastar) limpa a selecao.
        if response.clicked() {
            self.sel_anchor = None;
            self.sel_head = None;
        }

        // Digitado/colado: vai ao servidor e volta a visao ao fim.
        let mut typed = Vec::new();

        // Botao direito cola o conteudo da area de transferencia (somente texto).
        if response.secondary_clicked() {
            if let Some(text) = clipboard_text() {
                typed.extend_from_slice(text.as_bytes());
            }
        }

        // --- Entrada de teclado (somente com foco) ---
        // Processa dentro do proprio closure de input: evita clonar o Vec de
        // eventos (que pode conter Strings grandes de colagem) a cada quadro.
        if response.has_focus() {
            let alt = self.parser.screen().alternate_screen();
            // Modo de cursor de aplicacao (DECCKM, ?1h): less, man, vim e os
            // programas ncurses esperam as setas como ESC O A, nao ESC [ A.
            let app_cursor = self.parser.screen().application_cursor();
            let page = usize::from(self.rows.saturating_sub(1).max(1));
            let mut keys: Vec<ScrollKey> = Vec::new();
            let mut hint = false;
            ui.input(|i| {
                for ev in &i.events {
                    match ev {
                        egui::Event::Text(t) => {
                            for ch in t.chars().filter(|c| !c.is_control()) {
                                let mut buf = [0u8; 4];
                                typed.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                            }
                        }
                        egui::Event::Paste(t) => typed.extend_from_slice(t.as_bytes()),
                        egui::Event::Copy => typed.push(0x03), // Ctrl+C = interrupcao
                        egui::Event::Cut => typed.push(0x18),  // Ctrl+X
                        egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers,
                            ..
                        } => {
                            match (scroll_key(*key, modifiers), alt) {
                                (Some(k), false) => keys.push(k),
                                // Na alternativa nao ha o que rolar; Shift+PgUp
                                // iria ao shell do tmux (history-search).
                                (Some(ScrollKey::PageUp | ScrollKey::PageDown), true) => {
                                    hint = true
                                }
                                _ => {
                                    if let Some(bytes) = map_key(*key, modifiers, app_cursor) {
                                        typed.extend_from_slice(&bytes);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            });
            if hint {
                self.hint_until = now + HINT_SECS;
            }
            for k in keys {
                match k {
                    ScrollKey::PageUp => self.scroll_by(page as isize),
                    ScrollKey::PageDown => self.scroll_by(-(page as isize)),
                    ScrollKey::Top => self.set_view(usize::MAX),
                    ScrollKey::Bottom => self.scroll_to_bottom(),
                }
            }
        }
        if !typed.is_empty() {
            self.scroll_to_bottom();
            out.input.extend_from_slice(&typed);
        }
        out.input.extend_from_slice(&wheel_bytes);

        // --- Renderizacao da grade ---
        self.parser.screen_mut().set_scrollback(self.view);
        self.paint_grid(&painter, rect, &font_id, cell_w, cell_h);

        // --- Posicao no historico: barra fina e aviso clicavel ---
        if self.view > 0 {
            let avail_lines = self.avail();
            let total = (avail_lines + usize::from(self.rows)) as f32;
            let h = rect.height();
            let thumb_h = (h * f32::from(self.rows) / total).max(12.0).min(h);
            let top_line = (avail_lines - self.view) as f32;
            let y = rect.min.y + (h - thumb_h) * (top_line / avail_lines.max(1) as f32);
            // Na sobra a direita da grade (menos de uma celula), para nao
            // cobrir a ultima coluna; sem sobra, 2 px colados na borda.
            let spare = ui.max_rect().max.x.min(ui.clip_rect().max.x) - rect.max.x;
            let bar = if spare >= 3.0 {
                Rect::from_min_size(
                    Pos2::new(rect.max.x + (spare - 3.0).min(1.0), y),
                    Vec2::new(3.0, thumb_h),
                )
            } else {
                Rect::from_min_size(Pos2::new(rect.max.x - 2.0, y), Vec2::new(2.0, thumb_h))
            };
            ui.painter().rect_filled(
                bar,
                CornerRadius::same(2),
                Color32::from_rgba_unmultiplied(0x9b, 0xa3, 0xb4, 0x99),
            );
            let n = thousands(self.view);
            // Do mais largo ao mais estreito (vale o primeiro que couber).
            let texts: Vec<String> = if self.unseen {
                vec![
                    format!("↑ {n} linhas  ·  saída nova  ·  Shift+End volta ao fim"),
                    format!("↑ {n} · novo"),
                    format!("↑ {n}"),
                ]
            } else {
                vec![
                    format!("↑ {n} linhas  ·  Shift+End volta ao fim"),
                    format!("↑ {n}"),
                ]
            };
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            // No topo do historico o aviso vai para baixo: a primeira linha
            // (em geral o comando) nao tem mais como descer para aparecer.
            let bottom = self.view >= avail_lines;
            let grid = (rect, Vec2::new(cell_w, cell_h));
            let id = response.id.with("fim");
            if pill(ui, &painter, grid, bottom, &texts, Some(id)) {
                self.scroll_to_bottom();
                ui.ctx().request_repaint();
            }
        } else if self.hint_until > now {
            let grid = (rect, Vec2::new(cell_w, cell_h));
            pill(ui, &painter, grid, false, &HINTS, None);
            ui.ctx()
                .request_repaint_after(Duration::from_secs_f64(self.hint_until - now));
        }

        out
    }

    /// Retorna o intervalo de selecao normalizado (inicio <= fim).
    fn selection_range(&self) -> Option<((i64, u16), (i64, u16))> {
        let a = self.sel_anchor?;
        let b = self.sel_head?;
        if a <= b {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    /// Texto selecionado, mesmo fora da visao: cada linha absoluta e lida
    /// pondo-a na visao (deslocamento temporario). Linhas quebradas pela
    /// largura (wrap) juntam-se sem "\n".
    fn selection_text(&mut self) -> Option<String> {
        let ((a0, c0), (a1, c1)) = self.selection_range()?;
        let oldest = self.pushed - self.avail() as i64;
        let mut text = String::new();
        for abs in a0.max(oldest)..=a1 {
            let (off, row) = if abs >= self.pushed {
                (0, u16::try_from(abs - self.pushed).unwrap_or(u16::MAX))
            } else {
                ((self.pushed - abs) as usize, 0)
            };
            if row >= self.rows {
                break;
            }
            let (start, end) = if a0 == a1 {
                (c0, c1)
            } else if abs == a0 {
                (c0, self.cols - 1)
            } else if abs == a1 {
                (0, c1)
            } else {
                (0, self.cols - 1)
            };
            let s = self.parser.screen_mut();
            s.set_scrollback(off);
            let s = self.parser.screen();
            // Ate a borda direita: a linha inteira, mesmo que ela tenha subido
            // ao historico com o painel mais largo (o vt100 a guarda com a
            // largura de entao; a grade so mostra a atual).
            let to_edge = end == self.cols - 1;
            let mut line = String::new();
            for c in start..=u16::MAX {
                if c > end && !to_edge {
                    break;
                }
                match s.cell(row, c) {
                    Some(cell) if cell.is_wide_continuation() => {}
                    Some(cell) => {
                        let t = cell.contents();
                        line.push_str(if t.is_empty() { " " } else { t });
                    }
                    None if c >= self.cols => break,
                    None => {}
                }
            }
            let soft_wrap = to_edge && abs != a1 && s.row_wrapped(row);
            if soft_wrap {
                text.push_str(&line);
            } else {
                text.push_str(line.trim_end());
                if abs != a1 {
                    text.push('\n');
                }
            }
        }
        let v = self.view;
        self.parser.screen_mut().set_scrollback(v);
        Some(text)
    }

    fn paint_grid(
        &self,
        painter: &egui::Painter,
        rect: Rect,
        font_id: &FontId,
        cell_w: f32,
        cell_h: f32,
    ) {
        let screen = self.parser.screen();
        let sel = self.selection_range();
        let ppp = painter.pixels_per_point();
        // Pontos do braille de toda a grade numa malha so, pintada depois dos
        // fundos: ja alinhados aos pixels, dispensam o anti-serrilhado de um
        // retangulo por ponto, que custa muito mais para tesselar.
        let mut dots = egui::Mesh::default();
        // Emojis e caracteres desenhados pelo sistema: (textura, retangulo).
        let mut images: Vec<(egui::TextureId, Rect)> = Vec::new();

        for row in 0..self.rows {
            let y = rect.min.y + row as f32 * cell_h;

            // Agrupa celulas ASCII contiguas com o mesmo estilo em "trechos":
            // um unico texto (galley) e um unico retangulo de fundo por trecho,
            // em vez de um por celula — reduz muito o custo por quadro em
            // grades grandes. Glifos largos (CJK) ou fora do ASCII sao pintados
            // individualmente, pois a fonte pode nao lhes dar a largura exata
            // da celula monoespacada.
            let mut run = String::new();
            let mut run_start = 0u16;
            let mut run_fg = Color32::WHITE;
            let mut run_bg = TERM_BG;
            let mut run_only_spaces = true;

            macro_rules! flush_run {
                () => {
                    if !run.is_empty() {
                        let x = rect.min.x + run_start as f32 * cell_w;
                        if run_bg != TERM_BG {
                            painter.rect_filled(
                                Rect::from_min_size(
                                    Pos2::new(x, y),
                                    // Trechos sao 100% ASCII: len == n. de colunas.
                                    Vec2::new(run.len() as f32 * cell_w, cell_h),
                                ),
                                CornerRadius::ZERO,
                                run_bg,
                            );
                        }
                        // Trechos so de espacos nao precisam desenhar texto.
                        if !run_only_spaces {
                            painter.text(
                                Pos2::new(x, y),
                                Align2::LEFT_TOP,
                                run.clone(),
                                font_id.clone(),
                                run_fg,
                            );
                        }
                        run.clear();
                    }
                };
            }

            let mut col = 0u16;
            while col < self.cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => {
                        flush_run!();
                        col += 1;
                        continue;
                    }
                };
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }

                let (fg, bg) = resolve_colors(cell);
                let s = cell.contents();
                let ascii = !cell.is_wide()
                    && (s.is_empty()
                        || (s.len() == 1 && (0x20..0x7f).contains(&s.as_bytes()[0])));

                if ascii {
                    let ch = if s.is_empty() { ' ' } else { s.as_bytes()[0] as char };
                    let is_space = ch == ' ';
                    // O trecho continua se o estilo casa (espacos so exigem o
                    // mesmo fundo; a cor do texto deles nao aparece).
                    let compat = !run.is_empty()
                        && bg == run_bg
                        && (is_space || run_only_spaces || fg == run_fg);
                    if !compat {
                        flush_run!();
                        run_start = col;
                        run_fg = fg;
                        run_bg = bg;
                        run_only_spaces = true;
                    }
                    if !is_space && run_only_spaces {
                        // O primeiro glifo visivel define a cor do trecho.
                        run_fg = fg;
                        run_only_spaces = false;
                    }
                    run.push(ch);
                    col += 1;
                } else {
                    flush_run!();
                    let span = if cell.is_wide() { 2 } else { 1 };
                    let cell_rect = Rect::from_min_size(
                        Pos2::new(rect.min.x + col as f32 * cell_w, y),
                        Vec2::new(cell_w * span as f32, cell_h),
                    );
                    if bg != TERM_BG {
                        painter.rect_filled(cell_rect, CornerRadius::ZERO, bg);
                    }
                    if let Some(bits) = braille_bits(s) {
                        // A fonte nao tem braille (sairia um quadradinho) e o
                        // btop desenha os graficos com ele: pontos desenhados.
                        for dot in braille_dots(bits, cell_rect, ppp) {
                            dots.add_colored_rect(dot, fg);
                        }
                    } else if !s.is_empty() {
                        // Emoji, ou caractere que a fonte nao tem: imagem do
                        // sistema (pintada no fim, por cima dos fundos, pois
                        // pode ocupar a celula vazia seguinte). Senao, fonte.
                        let next_blank = !cell.is_wide()
                            && col + 1 < self.cols
                            && screen
                                .cell(row, col + 1)
                                .is_some_and(|n| n.contents().trim().is_empty());
                        let sistema = system_glyph(
                            painter,
                            s,
                            cell.is_wide(),
                            cell_rect,
                            next_blank,
                            font_id,
                            fg,
                        );
                        match sistema {
                            Some(img) => images.push(img),
                            None => {
                                painter.text(cell_rect.min, Align2::LEFT_TOP, s, font_id.clone(), fg);
                            }
                        }
                    }
                    col += span;
                }
            }
            flush_run!();
        }
        if !dots.is_empty() {
            painter.add(egui::Shape::mesh(dots));
        }
        let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
        for (tex, r) in images {
            painter.image(tex, r, uv, Color32::WHITE);
        }

        // Selecao: overlay translucido no aco claro do tema, nas fileiras da
        // visao cujas linhas absolutas estao no intervalo.
        if let Some(((a0, c0), (a1, c1))) = sel {
            let overlay = Color32::from_rgba_unmultiplied(0x9b, 0xa3, 0xb4, 0x55);
            for r in 0..self.rows {
                let abs = self.abs_row(r);
                if abs < a0 || abs > a1 {
                    continue;
                }
                let (start, end) = if a0 == a1 {
                    (c0, c1)
                } else if abs == a0 {
                    (c0, self.cols - 1)
                } else if abs == a1 {
                    (0, c1)
                } else {
                    (0, self.cols - 1)
                };
                let sel_rect = Rect::from_min_size(
                    Pos2::new(
                        rect.min.x + start as f32 * cell_w,
                        rect.min.y + r as f32 * cell_h,
                    ),
                    Vec2::new((end - start + 1) as f32 * cell_w, cell_h),
                );
                painter.rect_filled(sel_rect, CornerRadius::ZERO, overlay);
            }
        }

        // Cursor: a posicao do vt100 e na tela; com a visao no historico ele
        // desce `view` fileiras (ou sai da visao).
        if !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            let cr = usize::from(cr) + self.view;
            if cr < usize::from(self.rows) && cc < self.cols {
                let cur_rect = Rect::from_min_size(
                    Pos2::new(rect.min.x + cc as f32 * cell_w, rect.min.y + cr as f32 * cell_h),
                    Vec2::new(cell_w, cell_h),
                );
                painter.rect_filled(
                    cur_rect,
                    CornerRadius::ZERO,
                    Color32::from_rgba_unmultiplied(0xcc, 0xcc, 0xcc, 0x88),
                );
            }
        }
    }
}

/// Aviso no canto direito da grade (`rect`, celulas de tamanho `cell`), em
/// cima ou embaixo (`bottom`). Ocupa celulas inteiras (nenhum glifo fica
/// cortado ao meio em volta dele) e deixa a ultima coluna livre. Usa o
/// primeiro de `texts` que cabe, pela largura medida (a da fonte muda com a
/// escala); se nenhum couber, o ultimo. Com `click`, e clicavel: devolve se
/// foi clicado.
fn pill(
    ui: &egui::Ui,
    painter: &egui::Painter,
    (rect, cell): (Rect, Vec2),
    bottom: bool,
    texts: &[&str],
    click: Option<egui::Id>,
) -> bool {
    // A fonte proporcional do egui nao tem as setas.
    let font = FontId::monospace(12.0);
    let color = Color32::from_rgb(0xdd, 0xdd, 0xe2);
    let pad_x = 8.0;
    let cols = (rect.width() / cell.x).round();
    let mut chosen = None;
    for t in texts {
        let g = ui.fonts(|f| f.layout_no_wrap((*t).to_owned(), font.clone(), color));
        let n = ((g.size().x + 2.0 * pad_x) / cell.x).ceil();
        chosen = Some((g, n));
        if n < cols {
            break;
        }
    }
    let Some((galley, n)) = chosen else {
        return false;
    };
    let lines = (galley.size().y / cell.y).ceil().max(1.0);
    let size = Vec2::new(n * cell.x, lines * cell.y);
    let x = rect.min.x + (cols - 1.0 - n).max(0.0) * cell.x;
    let y = if bottom {
        (rect.max.y - size.y).max(rect.min.y)
    } else {
        rect.min.y
    };
    let pill = Rect::from_min_size(Pos2::new(x, y), size);
    let clicked = click.is_some_and(|id| {
        ui.interact(pill, id, Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
    });
    painter.rect_filled(pill, CornerRadius::same(4), PILL_BG);
    painter.rect_stroke(
        pill,
        CornerRadius::same(4),
        egui::Stroke::new(1.0, Color32::from_rgb(0x6e, 0x76, 0x84)),
        egui::StrokeKind::Inside,
    );
    let text_pos = pill.center() - galley.size() / 2.0;
    painter.galley(text_pos, galley, color);
    clicked
}

/// Texto da area de transferencia do sistema, se houver (colar com o botao
/// direito). Nos testes vem de `tests::CLIPBOARD`: eles nao tocam na do
/// usuario.
fn clipboard_text() -> Option<String> {
    #[cfg(test)]
    let text = tests::CLIPBOARD.with(|c| c.borrow().clone());
    #[cfg(not(test))]
    let text = arboard::Clipboard::new().ok()?.get_text().ok();
    text.filter(|t| !t.is_empty())
}

/// Tamanho da celula em pontos, arredondado para um numero inteiro de pixels
/// fisicos (o porque esta em `Terminal::ui`).
fn cell_size(fonts: &egui::text::Fonts, font_id: &FontId, ppp: f32) -> (f32, f32) {
    let snap = |v: f32| ((v * ppp).round() / ppp).max(1.0);
    (
        snap(fonts.glyph_width(font_id, 'M')),
        snap(fonts.row_height(font_id)),
    )
}

/// Imagem do sistema para a celula `s`, se ela precisar: textura e retangulo
/// (alinhado aos pixels) onde pinta-la. `None`: a fonte do terminal desenha.
/// - emoji (🟢, ❤️): colorido, na caixa da celula;
/// - pictograma que falta na fonte (🛢, 🗂): colorido tambem; numa coluna so
///   ficaria minusculo, entao usa a celula seguinte se ela estiver vazia;
/// - outro caractere que falta na fonte (✓, ✗, CJK): da fonte do sistema que
///   o tiver, na cor do texto e no tamanho da fonte do terminal.
///
/// O retangulo e o da caixa aumentado pela folga transparente da imagem, para
/// a sombra e as bordas do emoji nao serem cortadas.
fn system_glyph(
    painter: &egui::Painter,
    s: &str,
    wide: bool,
    cell: Rect,
    next_blank: bool,
    font_id: &FontId,
    fg: Color32,
) -> Option<(egui::TextureId, Rect)> {
    let first = s.chars().next()?;
    let emoji = emoji::is_emoji(s, wide);
    if !emoji && painter.fonts(|f| f.has_glyph(font_id, first)) {
        return None;
    }
    let pictograma = emoji || emoji::is_pictographic(first);
    let mut caixa = cell;
    if pictograma && !wide && next_blank {
        caixa.max.x += cell.width();
    }
    let ppp = painter.pixels_per_point();
    let px = |v: f32| (v * ppp).round().max(1.0);
    let (w, h) = (px(caixa.width()) as u32, px(caixa.height()) as u32);
    let font_px = if pictograma {
        emoji::emoji_font_px(w, h)
    } else {
        font_id.size * ppp
    };
    let tex = emoji::texture(painter.ctx(), s, w, h, font_px, fg)?;
    let pad = emoji::pad_px(w, h) as f32;
    let snap = |v: f32| (v * ppp).round() / ppp;
    let min = Pos2::new(snap(caixa.min.x) - pad / ppp, snap(caixa.min.y) - pad / ppp);
    let size = Vec2::new((w as f32 + 2.0 * pad) / ppp, (h as f32 + 2.0 * pad) / ppp);
    Some((tex, Rect::from_min_size(min, size)))
}

/// Bits dos pontos, se a celula for um unico caractere braille
/// (U+2800..=U+28FF). Com marca combinante fica com a fonte.
fn braille_bits(s: &str) -> Option<u8> {
    let mut chars = s.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    u8::try_from(u32::from(c).checked_sub(0x2800)?).ok()
}

/// (coluna, linha) de cada bit na grade de 2x4 pontos, como no Unicode: bit 0
/// = ponto 1 (esquerda, 1a linha), 1 = ponto 2, 2 = ponto 3 (esquerda, 3a),
/// 3 = ponto 4 (direita, 1a), 4 = ponto 5, 5 = ponto 6 (direita, 3a),
/// 6 = ponto 7 (esquerda, 4a) e 7 = ponto 8 (direita, 4a).
const BRAILLE_DOTS: [(u8, u8); 8] = [
    (0, 0),
    (0, 1),
    (0, 2),
    (1, 0),
    (1, 1),
    (1, 2),
    (0, 3),
    (1, 3),
];

/// Retangulos dos pontos ligados em `bits` dentro de `cell`. As contas sao
/// em pixels fisicos inteiros (`ppp`), para os pontos sairem nitidos e iguais:
/// o passo entre pontos e a subcelula (1/2 da largura, 1/4 da altura)
/// arredondada para baixo, cada ponto e um quadrado de ~65% do passo (sempre
/// com 1 px de folga) e o bloco 2x4 fica centrado na celula. Assim os
/// graficos em celulas e linhas seguidas mantem (quase) o mesmo passo.
fn braille_dots(bits: u8, cell: Rect, ppp: f32) -> impl Iterator<Item = Rect> {
    let px = |v: f32| (v * ppp).round();
    let (x0, y0) = (px(cell.min.x), px(cell.min.y));
    let (w, h) = (px(cell.width()), px(cell.height()));
    let (step_x, step_y) = ((w / 2.0).floor().max(1.0), (h / 4.0).floor().max(1.0));
    let step = step_x.min(step_y);
    let side = (step * 0.65).round().clamp(1.0, (step - 1.0).max(1.0));
    let margin_x = ((w - step_x - side) / 2.0).floor().max(0.0);
    let margin_y = ((h - 3.0 * step_y - side) / 2.0).floor().max(0.0);
    BRAILLE_DOTS
        .into_iter()
        .enumerate()
        .filter(move |&(bit, _)| bits & (1 << bit) != 0)
        .map(move |(_, (c, r))| {
            let x = x0 + margin_x + f32::from(c) * step_x;
            let y = y0 + margin_y + f32::from(r) * step_y;
            Rect::from_min_size(Pos2::new(x / ppp, y / ppp), Vec2::splat(side / ppp))
        })
}

fn resolve_colors(cell: &vt100::Cell) -> (Color32, Color32) {
    let default_fg = Color32::from_rgb(0xcc, 0xcc, 0xcc);
    let default_bg = TERM_BG;

    let mut fg_color = cell.fgcolor();
    let mut bg_color = cell.bgcolor();
    if cell.inverse() {
        std::mem::swap(&mut fg_color, &mut bg_color);
    }

    // Negrito intensifica as 8 cores base.
    let fg = match fg_color {
        vt100::Color::Idx(i) if cell.bold() && i < 8 => ansi_to_rgb(i + 8),
        vt100::Color::Idx(i) => ansi_to_rgb(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Default => default_fg,
    };
    let bg = match bg_color {
        vt100::Color::Idx(i) => ansi_to_rgb(i),
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
        vt100::Color::Default => default_bg,
    };
    (fg, bg)
}

fn ansi_to_rgb(idx: u8) -> Color32 {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    if idx < 16 {
        let (r, g, b) = BASE[idx as usize];
        Color32::from_rgb(r, g, b)
    } else if idx < 232 {
        let i = idx - 16;
        let levels = [0u8, 95, 135, 175, 215, 255];
        let r = levels[(i / 36) as usize];
        let g = levels[((i / 6) % 6) as usize];
        let b = levels[(i % 6) as usize];
        Color32::from_rgb(r, g, b)
    } else {
        let v = 8 + (idx - 232) * 10;
        Color32::from_rgb(v, v, v)
    }
}

/// Traduz teclas especiais e combinacoes Ctrl+letra em bytes para o PTY.
/// Caracteres imprimiveis comuns chegam via `Event::Text` e retornam `None` aqui.
/// Setas, Home e End: "ESC [ x" no modo normal e "ESC O x" no modo de cursor
/// de aplicacao (DECCKM), que less, man, vim e os programas ncurses ligam.
fn cursor_key(final_byte: u8, app_cursor: bool) -> Vec<u8> {
    vec![0x1b, if app_cursor { b'O' } else { b'[' }, final_byte]
}

fn map_key(key: egui::Key, mods: &egui::Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    use egui::Key;
    let bytes: Vec<u8> = match key {
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Tab => vec![b'\t'],
        Key::Escape => vec![0x1b],
        Key::ArrowUp => cursor_key(b'A', app_cursor),
        Key::ArrowDown => cursor_key(b'B', app_cursor),
        Key::ArrowRight => cursor_key(b'C', app_cursor),
        Key::ArrowLeft => cursor_key(b'D', app_cursor),
        Key::Home => cursor_key(b'H', app_cursor),
        Key::End => cursor_key(b'F', app_cursor),
        Key::PageUp => b"\x1b[5~".to_vec(),
        Key::PageDown => b"\x1b[6~".to_vec(),
        Key::Insert => b"\x1b[2~".to_vec(),
        Key::Delete => b"\x1b[3~".to_vec(),
        Key::F1 => b"\x1bOP".to_vec(),
        Key::F2 => b"\x1bOQ".to_vec(),
        Key::F3 => b"\x1bOR".to_vec(),
        Key::F4 => b"\x1bOS".to_vec(),
        Key::F5 => b"\x1b[15~".to_vec(),
        Key::F6 => b"\x1b[17~".to_vec(),
        Key::F7 => b"\x1b[18~".to_vec(),
        Key::F8 => b"\x1b[19~".to_vec(),
        Key::F9 => b"\x1b[20~".to_vec(),
        Key::F10 => b"\x1b[21~".to_vec(),
        Key::F11 => b"\x1b[23~".to_vec(),
        Key::F12 => b"\x1b[24~".to_vec(),
        other => {
            if mods.ctrl {
                if let Some(c) = key_letter(other) {
                    return Some(vec![c - b'a' + 1]);
                }
                // Ctrl+Espaco => NUL
                if matches!(other, Key::Space) {
                    return Some(vec![0]);
                }
            }
            return None;
        }
    };
    Some(bytes)
}

fn key_letter(key: egui::Key) -> Option<u8> {
    use egui::Key::*;
    Some(match key {
        A => b'a',
        B => b'b',
        C => b'c',
        D => b'd',
        E => b'e',
        F => b'f',
        G => b'g',
        H => b'h',
        I => b'i',
        J => b'j',
        K => b'k',
        L => b'l',
        M => b'm',
        N => b'n',
        O => b'o',
        P => b'p',
        Q => b'q',
        R => b'r',
        S => b's',
        T => b't',
        U => b'u',
        V => b'v',
        W => b'w',
        X => b'x',
        Y => b'y',
        Z => b'z',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// Area de transferencia dos testes (ver `clipboard_text`), uma por
        /// thread: os testes rodam em paralelo.
        pub(super) static CLIPBOARD: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Texto das celulas `c0..c1` da linha `r` (celula vazia = espaco).
    fn row_text(t: &Terminal, r: u16, c0: u16, c1: u16) -> String {
        let screen = t.parser.screen();
        (c0..c1)
            .map(|c| match screen.cell(r, c).map(|x| x.contents()) {
                Some("") => " ",
                Some(s) => s,
                None => "?",
            })
            .collect()
    }

    fn cell_fg(t: &Terminal, r: u16, c: u16) -> vt100::Color {
        t.parser.screen().cell(r, c).unwrap().fgcolor()
    }

    /// Um quadro no estilo do btop: tudo posicionado por HVP ("CSI l;c f",
    /// o Mv::to do btop), salvar/restaurar por "CSI s"/"CSI u", grafico em
    /// braille e cores RGB.
    fn btop_frame() -> Vec<u8> {
        let mut f = String::new();
        f += "\x1b[?1049h\x1b[?25l"; // tela alternativa, sem cursor
        f += "\x1b[2J\x1b[0;0f"; // Term::clear do btop
        f += "\x1b[1;1f\x1b[38;2;128;128;128m╭─┤\x1b[38;2;255;255;255m cpu ";
        f += "\x1b[38;2;128;128;128m├──────────╮";
        for r in 2..=5 {
            f += &format!("\x1b[{r};1f│\x1b[{r};20f│");
        }
        f += "\x1b[6;1f╰──────────────────╯";
        f += "\x1b[3;3f\x1b[38;2;0;200;0m⣀⣤⣶⣿⡇⢸"; // grafico
        f += "\x1b[s\x1b[8;5f\x1b[0mMem 42%\x1b[u"; // salva, escreve longe, volta
        f += "▲"; // continua de onde estava, com a cor salva
        f += "\x1b[4;16f\x1b[1m99%";
        f += "\x1b]0;btop [f[s\x07"; // titulo com "[f": nada muda na tela
        f.into_bytes()
    }

    fn assert_btop_frame(t: &Terminal) {
        assert_eq!(row_text(t, 0, 0, 20), "╭─┤ cpu ├──────────╮");
        for r in 1..=4 {
            assert_eq!(row_text(t, r, 0, 1), "│", "linha {r}");
            assert_eq!(row_text(t, r, 19, 20), "│", "linha {r}");
        }
        assert_eq!(row_text(t, 5, 0, 20), "╰──────────────────╯");
        assert_eq!(row_text(t, 2, 2, 9), "⣀⣤⣶⣿⡇⢸▲");
        assert_eq!(cell_fg(t, 2, 2), vt100::Color::Rgb(0, 200, 0));
        // O restaurar devolve tambem a cor (como o DECRC e o xterm).
        assert_eq!(cell_fg(t, 2, 8), vt100::Color::Rgb(0, 200, 0));
        assert_eq!(row_text(t, 7, 4, 11), "Mem 42%");
        assert_eq!(cell_fg(t, 7, 4), vt100::Color::Default);
        assert_eq!(row_text(t, 3, 15, 18), "99%");
        assert_eq!(t.parser.screen().cursor_position(), (3, 18));
        let text = t.parser.screen().contents();
        assert!(!text.contains("btop") && !text.contains("[f"), "{text}");
    }

    #[test]
    fn btop_frame_lands_in_place() {
        let frame = btop_frame();
        let mut t = Terminal::new(40, 12);
        t.process(&frame);
        assert_btop_frame(&t);

        // Sem a traducao o vt100 ignora o HVP: a caixa sai corrida.
        let mut raw = vt100::Parser::new(12, 40, 0);
        raw.process(&frame);
        assert_ne!(raw.screen().cell(1, 0).unwrap().contents(), "│");
        assert_ne!(raw.screen().cell(7, 4).unwrap().contents(), "M");

        // Mesmo resultado com o quadro em pedacos: todos os cortes e byte a byte.
        let want = (t.parser.screen().contents_formatted(), (3, 18));
        for cut in 0..=frame.len() {
            let mut p = Terminal::new(40, 12);
            p.process(&frame[..cut]);
            p.process(&frame[cut..]);
            let got = (
                p.parser.screen().contents_formatted(),
                p.parser.screen().cursor_position(),
            );
            assert_eq!(got, want, "corte em {cut}");
        }
        let mut p = Terminal::new(40, 12);
        for b in &frame {
            p.process(std::slice::from_ref(b));
        }
        assert_btop_frame(&p);
    }

    #[test]
    fn save_restore_and_hvp_details() {
        let mut t = Terminal::new(20, 6);
        // HVP sem parametros e com zeros vai ao canto; valores enormes param na borda.
        t.process(b"\x1b[3;3fA\x1b[fB\x1b[3;3f\x1b[0;0fC\x1b[999;999fD");
        assert_eq!(row_text(&t, 0, 0, 1), "C");
        assert_eq!(row_text(&t, 2, 2, 3), "A");
        assert_eq!(row_text(&t, 5, 19, 20), "D");
        // Controle no meio do "CSI s": executado, e o cursor e salvo depois dele.
        t.process(b"\x1b[2;5fxy\x1b[\rs\x1b[6;1fz\x1b[uw");
        assert_eq!(row_text(&t, 1, 0, 6), "w   xy");
        // "CSI 1;10 s" (DECSLRM) nao salva: o "CSI u" volta ao (1,1) salvo antes.
        t.process(b"\x1b[1;1f\x1b[s\x1b[4;4f\x1b[1;10sQ\x1b[uR");
        assert_eq!(row_text(&t, 3, 3, 4), "Q");
        assert_eq!(row_text(&t, 0, 0, 1), "R");
        // "CSI ? u", "CSI > 1 u" e "CSI = 1;1 u" (teclado do kitty) nao restauram.
        t.process(b"\x1b[4;6f\x1b[?u\x1b[>1u\x1b[=1;1uS");
        assert_eq!(row_text(&t, 3, 5, 6), "S");
        // CSI 'f' com marcador privado continua ignorado.
        t.process(b"\x1b[5;5f\x1b[?1;1fT");
        assert_eq!(row_text(&t, 4, 4, 5), "T");
    }

    /// Estado visivel do terminal: tela, cursor, atributos correntes e qual
    /// tela esta ativa.
    fn state(t: &Terminal) -> (Vec<u8>, (u16, u16), Vec<u8>, bool) {
        let s = t.parser.screen();
        (
            s.contents_formatted(),
            s.cursor_position(),
            s.attributes_formatted(),
            s.alternate_screen(),
        )
    }

    /// Processa `bytes` de uma vez e cortado em cada ponto (um corte, dois
    /// pedacos): o estado tem de ser sempre o mesmo. Devolve o de uma vez.
    fn same_state_for_every_cut(bytes: &[u8], cols: u16, rows: u16) -> Terminal {
        let mut whole = Terminal::new(cols, rows);
        whole.process(bytes);
        for cut in 0..=bytes.len() {
            let mut t = Terminal::new(cols, rows);
            t.process(&bytes[..cut]);
            t.process(&bytes[cut..]);
            assert_eq!(
                state(&t),
                state(&whole),
                "corte em {cut}: {:?}",
                String::from_utf8_lossy(bytes)
            );
        }
        whole
    }

    #[test]
    fn utf8_cut_between_chunks_loses_nothing() {
        // O vte 0.15 perdia um byte quando um caractere de 2 bytes chegava
        // partido e, logo depois dele, vinham ASCII e um byte nao ASCII.
        let t = same_state_for_every_cut("45°C│ você é".as_bytes(), 20, 3);
        assert_eq!(row_text(&t, 0, 0, 12), "45°C│ você é");
        // Com um ESC seguido de byte alto, o ESC sumia e o tradutor perdia o
        // passo ("[s" virava "7" na tela; "[\r s" salvava o cursor).
        let t = same_state_for_every_cut(b"\xc3\xa7\x1b\x80[5;5fX", 20, 6);
        assert_eq!(row_text(&t, 0, 0, 3), "ç  ");
        assert_eq!(row_text(&t, 4, 4, 5), "X");
        let t = same_state_for_every_cut(b"\xc3\xa7\x1b\x80[s\x1b[3;3fB\x1b[uC", 20, 6);
        assert_eq!(row_text(&t, 0, 0, 3), "çC ");
        assert_eq!(row_text(&t, 2, 2, 3), "B");
        // O \r no meio do CSI volta a coluna 0 e o cursor e salvo ali: o A
        // cobre o ç, e o C (depois do ESC 8) cobre o A.
        let t = same_state_for_every_cut(b"\xc3\xa7\x1b\x80[\rsA\x1b[3;3HB\x1b8C", 20, 6);
        assert_eq!(row_text(&t, 0, 0, 3), "C  ");
        assert_eq!(row_text(&t, 2, 2, 3), "B");
        // Caracteres de 3 e 4 bytes e UTF-8 invalido, cortados em qualquer ponto.
        same_state_for_every_cut("⣿é\u{1f600}x\x1b[1;2f─".as_bytes(), 20, 3);
        same_state_for_every_cut(
            b"a\xc3b\xe2\x94c\xf0\x9f\x98d\xe0\x80e\xc3\x1b[2;2fz\xed\xa0\x80",
            20,
            3,
        );
    }

    #[test]
    fn leaving_alt_screen_restores_primary_attrs() {
        // O vt100 guarda um so conjunto de atributos salvos para as duas
        // telas: um salvar na tela alternativa (CSI s, como o btop faz nas
        // caixas de mensagem, ou ESC 7) apagava o que o "CSI ? 1049 h" salvou,
        // e o prompt voltava com as cores do programa.
        let enter_and_leave =
            b"\x1b[31m$\x1b[?1049h\x1b[42;32m\x1b[s\x1b[u\x1b7\x1b8\x1b[0m\x1b[?1049lX";
        let t = same_state_for_every_cut(enter_and_leave, 20, 4);
        assert_eq!(row_text(&t, 0, 0, 2), "$X");
        assert_eq!(cell_fg(&t, 0, 1), vt100::Color::Idx(1));
        assert_eq!(
            t.parser.screen().cell(0, 1).unwrap().bgcolor(),
            vt100::Color::Default
        );
        // O ESC 8 na tela principal tambem volta ao que ela salvou.
        let mut again = enter_and_leave.to_vec();
        again.extend_from_slice(b"\x1b[0m\x1b[3;1f\x1b8Y");
        let t = same_state_for_every_cut(&again, 20, 4);
        assert_eq!(row_text(&t, 0, 0, 2), "$Y");
        assert_eq!(cell_fg(&t, 0, 1), vt100::Color::Idx(1));
        // Dentro da tela alternativa o salvar/restaurar continua valendo.
        let t =
            same_state_for_every_cut(b"\x1b[?1049h\x1b[32m\x1b[s\x1b[0m\x1b[2;2fA\x1b[uB", 20, 4);
        assert_eq!(row_text(&t, 0, 0, 1), "B");
        assert_eq!(cell_fg(&t, 0, 0), vt100::Color::Idx(2));
        assert_eq!(cell_fg(&t, 1, 1), vt100::Color::Default);
        // Sair sem ter entrado (ou entrar duas vezes) nao inventa atributos.
        let t = same_state_for_every_cut(b"\x1b[33m\x1b7\x1b[0m\x1b[?1049lZ", 20, 4);
        assert_eq!(cell_fg(&t, 0, 0), vt100::Color::Idx(3));
        let t = same_state_for_every_cut(
            b"\x1b[31m\x1b[?1049h\x1b[32m\x1b[?1049h\x1b[0m\x1b[?1049lW",
            20,
            4,
        );
        assert_eq!(cell_fg(&t, 0, 0), vt100::Color::Idx(1));
    }

    #[test]
    fn random_streams_same_state_for_any_cut() {
        // UTF-8, sequencias traduzidas, 1049/47 e strings misturados e
        // cortados ao acaso: o parser de verdade chega sempre ao mesmo estado.
        const TOKENS: &[&[u8]] = &[
            b"a",
            b"C",
            b" ",
            b"\xc2\xb0",
            b"\xc3\xa7",
            b"\xc3\xaa",
            b"\xe2\x94\x82",
            b"\xe2\xa3\xbf",
            b"\xf0\x9f\x98\x80",
            b"\xc3",
            b"\xe2\x94",
            b"\x80",
            b"\x1b",
            b"\x1b[",
            b"\x1b[s",
            b"\x1b[u",
            b"\x1b7",
            b"\x1b8",
            b"s",
            b"u",
            b"f",
            b"5;5",
            b"3;12f",
            b"\x1b[2;3f",
            b"?1049h",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[?47h",
            b"\x1b[?47l",
            b"\x1b[31m",
            b"\x1b[42;33m",
            b"\x1b[0m",
            b"\r",
            b"\n",
            b"\x18",
            b"\x1b]0;[f",
            b"\x07",
            b"\x1bP",
            b"\x1b\\",
            b"\x9c",
        ];
        let mut rng = crate::vtfix::tests::Rng(0x7e57_ab1e_5eed_0001);
        for _ in 0..2_000 {
            let s = rng.tokens(TOKENS, 24);
            let mut whole = Terminal::new(12, 5);
            whole.process(&s);
            let mut cuts: Vec<usize> = (0..1 + rng.below(4))
                .map(|_| rng.below(s.len() + 1))
                .collect();
            cuts.sort_unstable();
            let mut t = Terminal::new(12, 5);
            let mut from = 0;
            for &c in cuts.iter().chain(std::iter::once(&s.len())) {
                t.process(&s[from..c]);
                from = c;
            }
            assert_eq!(state(&t), state(&whole), "cortes {cuts:?}: {s:?}");
        }
    }

    #[test]
    fn braille_bits_only_for_single_braille_chars() {
        assert_eq!(braille_bits("\u{2800}"), Some(0));
        assert_eq!(braille_bits("⣿"), Some(0xff));
        assert_eq!(braille_bits("⢸"), Some(0xb8));
        for s in ["", " ", "a", "\u{27ff}", "\u{2900}", "█", "─", "⣿\u{301}"] {
            assert_eq!(braille_bits(s), None, "{s:?}");
        }
    }

    /// (coluna, linha) de cada ponto desenhado, pelo centro do retangulo.
    fn dot_places(ch: char, cell: Rect, ppp: f32) -> Vec<(u8, u8)> {
        let bits = braille_bits(&ch.to_string()).unwrap();
        let mut v: Vec<(u8, u8)> = braille_dots(bits, cell, ppp)
            .map(|d| {
                let c = ((d.center().x - cell.min.x) / (cell.width() / 2.0)).floor() as u8;
                let r = ((d.center().y - cell.min.y) / (cell.height() / 4.0)).floor() as u8;
                (c, r)
            })
            .collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn braille_dot_map_matches_unicode() {
        let cell = Rect::from_min_size(Pos2::new(16.0, 32.0), Vec2::new(8.0, 16.0));
        // Pontos 1-8 isolados: 1,2,3 e 7 descem pela esquerda; 4,5,6 e 8 pela direita.
        let single = [
            ('⠁', (0, 0)),
            ('⠂', (0, 1)),
            ('⠄', (0, 2)),
            ('⠈', (1, 0)),
            ('⠐', (1, 1)),
            ('⠠', (1, 2)),
            ('⡀', (0, 3)),
            ('⢀', (1, 3)),
        ];
        for (ch, place) in single {
            assert_eq!(dot_places(ch, cell, 1.0), vec![place], "{ch}");
        }
        // Formas conhecidas dos graficos do btop.
        assert_eq!(dot_places('⣀', cell, 1.0), vec![(0, 3), (1, 3)]);
        assert_eq!(dot_places('⠉', cell, 1.0), vec![(0, 0), (1, 0)]);
        assert_eq!(
            dot_places('⡇', cell, 1.0),
            vec![(0, 0), (0, 1), (0, 2), (0, 3)]
        );
        assert_eq!(
            dot_places('⢸', cell, 1.0),
            vec![(1, 0), (1, 1), (1, 2), (1, 3)]
        );
        assert_eq!(dot_places('⣿', cell, 1.0).len(), 8);
        assert!(dot_places('\u{2800}', cell, 1.0).is_empty());
        for bits in 0..=255u8 {
            assert_eq!(
                braille_dots(bits, cell, 1.0).count(),
                bits.count_ones() as usize
            );
        }
    }

    /// Pontos dentro da celula, do mesmo tamanho, alinhados aos pixels e
    /// separados por ao menos 1 px.
    fn assert_dots_ok(cell: Rect, ppp: f32) {
        let dots: Vec<Rect> = braille_dots(0xff, cell, ppp).collect();
        let px = |v: f32| v * ppp;
        let side = px(dots[0].width());
        for d in &dots {
            assert!((px(d.width()) - side).abs() < 1e-3 && (px(d.height()) - side).abs() < 1e-3);
            for v in [d.min.x, d.min.y, d.max.x, d.max.y] {
                assert!(
                    (px(v) - px(v).round()).abs() < 1e-3,
                    "fora do pixel: {d:?} ppp={ppp}"
                );
            }
            assert!(
                px(d.min.x) >= px(cell.min.x) - 1e-3
                    && px(d.max.x) <= px(cell.max.x) + 1e-3
                    && px(d.min.y) >= px(cell.min.y) - 1e-3
                    && px(d.max.y) <= px(cell.max.y) + 1e-3,
                "ponto {d:?} fora de {cell:?} (ppp={ppp})"
            );
        }
        // Ordem de BRAILLE_DOTS: 0,1,2,6 na esquerda (de cima para baixo) e 3,4,5,7 na direita.
        for col in [[0, 1, 2, 6], [3, 4, 5, 7]] {
            for w in col.windows(2) {
                let gap = px(dots[w[1]].min.y) - px(dots[w[0]].max.y);
                assert!(
                    gap >= 1.0 - 1e-3,
                    "folga vertical {gap} (ppp={ppp}, {cell:?})"
                );
            }
        }
        for (l, r) in [(0, 3), (1, 4), (2, 5), (6, 7)] {
            let gap = px(dots[r].min.x) - px(dots[l].max.x);
            assert!(
                gap >= 1.0 - 1e-3,
                "folga horizontal {gap} (ppp={ppp}, {cell:?})"
            );
        }
    }

    #[test]
    fn braille_dots_stay_crisp_and_separate() {
        for ppp in [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0] {
            // Celulas de 4x8 a 30x60 px, alinhadas ao pixel como as do terminal.
            for w_px in 4..=30 {
                for h_px in [8, 12, 16, 17, 20, 24, 29, 33, 41, 49, 60] {
                    for (col, row) in [(0.0, 0.0), (7.0, 3.0), (79.0, 23.0)] {
                        let (w, h) = (w_px as f32 / ppp, h_px as f32 / ppp);
                        let min = Pos2::new(col * w, row * h);
                        assert_dots_ok(Rect::from_min_size(min, Vec2::new(w, h)), ppp);
                    }
                }
            }
        }
    }

    #[test]
    fn braille_dots_legible_at_app_font() {
        for ppp in [1.0, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0] {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(ppp);
            let mut cell = (0.0, 0.0);
            for _ in 0..2 {
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    cell = ctx.fonts(|f| cell_size(f, &FontId::monospace(14.0), ppp));
                });
            }
            let cell = Rect::from_min_size(Pos2::ZERO, Vec2::new(cell.0, cell.1));
            assert_dots_ok(cell, ppp);
            // Ponto de 3 px ou mais: nitido mesmo a 100%.
            let side_px = braille_dots(1, cell, ppp).next().unwrap().width() * ppp;
            assert!(
                side_px >= 3.0 - 1e-3,
                "ponto de {side_px} px em {cell:?} (ppp={ppp})"
            );
        }
    }

    /// Formas pintadas por um quadro do terminal num painel de 320x160.
    fn painted(t: &mut Terminal, ppp: f32) -> Vec<egui::Shape> {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(ppp);
        let mut shapes = Vec::new();
        // O primeiro quadro so ajusta o tamanho do terminal ao painel.
        for _ in 0..2 {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(320.0, 160.0))),
                ..Default::default()
            };
            let out = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    t.ui(ui);
                });
            });
            shapes = out.shapes.into_iter().map(|c| c.shape).collect();
        }
        shapes
    }

    /// Emojis saem como imagem colorida do tamanho da celula (duas colunas), e
    /// o que falta na fonte (🛢, ✓) sai do sistema em vez de um quadradinho; o
    /// texto comum, o braille e os simbolos do htop/btop seguem como antes.
    #[cfg(windows)]
    #[test]
    fn paints_emoji_as_color_images() {
        for ppp in [1.0, 1.5] {
            let mut t = Terminal::new(12, 3);
            t.process("a🟢b🟠 ●⣿\r\n\x1b[31m❤\u{fe0f}\x1b[0mz\r\n🛢 x✓y".as_bytes());
            let shapes = painted(&mut t, ppp);

            let texts: Vec<String> = shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            assert!(
                texts.iter().all(|s| !s.contains(['🟢', '🟠', '❤', '🛢', '✓'])),
                "ppp={ppp}: {texts:?}"
            );
            for s in ["●", "x", "y"] {
                assert!(texts.iter().any(|t| t.trim() == s), "ppp={ppp}: {s} em {texts:?}");
            }

            let term = shapes
                .iter()
                .find_map(|s| match s {
                    egui::Shape::Rect(r) if r.fill == TERM_BG => Some(r.rect),
                    _ => None,
                })
                .expect("fundo do terminal");
            let (cw, ch) = (term.width() / t.cols as f32, term.height() / t.rows as f32);
            // Imagens: malhas com textura propria (o atlas de fontes e a padrao).
            let imagens: Vec<(egui::TextureId, Rect)> = shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Mesh(m) if m.texture_id != egui::TextureId::default() => {
                        Some((m.texture_id, m.calc_bounds()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(imagens.len(), 5, "ppp={ppp}: {imagens:?}");
            // A imagem tem folga em volta da celula (a sombra do emoji nao e
            // cortada): o retangulo pintado e o da celula aumentado por ela.
            let col = |c: f32, r: f32, span: f32| {
                let (w, h) = ((span * cw * ppp).round() as u32, (ch * ppp).round() as u32);
                let pad = emoji::pad_px(w, h) as f32 / ppp;
                Rect::from_min_size(
                    term.min + Vec2::new(c * cw, r * ch),
                    Vec2::new(span * cw, ch),
                )
                .expand(pad)
            };
            // 🟢 na coluna 1, 🟠 na 4 (duas colunas cada); ❤ + FE0F na linha 2
            // (uma coluna: a seguinte tem o "z"); na 3, o 🛢 de uma coluna usa
            // tambem o espaco seguinte e o ✓ fica na sua.
            let esperado = [
                col(1.0, 0.0, 2.0),
                col(4.0, 0.0, 2.0),
                col(0.0, 1.0, 1.0),
                col(0.0, 2.0, 2.0),
                col(3.0, 2.0, 1.0),
            ];
            for (got, want) in imagens.iter().zip(esperado) {
                assert!(
                    (got.1.min - want.min).abs().max_elem() < 1.0 / ppp + 1e-3
                        && (got.1.size() - want.size()).abs().max_elem() < 1.0 / ppp + 1e-3,
                    "ppp={ppp}: imagem {:?} fora da celula {want:?}",
                    got.1
                );
            }
            // Cada emoji tem a sua textura.
            assert_ne!(imagens[0].0, imagens[1].0);
        }
    }

    #[test]
    fn paints_braille_as_dots() {
        for ppp in [1.0, 1.5] {
            let mut t = Terminal::new(10, 3);
            t.process("\x1b[38;2;10;200;30m⣿\x1b[48;2;1;2;3m⠁x\x1b[0m ─".as_bytes());
            let shapes = painted(&mut t, ppp);
            let rects: Vec<_> = shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Rect(r) => Some(r),
                    _ => None,
                })
                .collect();
            let texts: Vec<String> = shapes
                .iter()
                .filter_map(|s| match s {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            // Braille nunca vai para a fonte; o resto continua como texto.
            assert!(
                texts.iter().all(|s| !s.contains('⣿') && !s.contains('⠁')),
                "{texts:?}"
            );
            assert!(
                texts.iter().any(|s| s == "x") && texts.iter().any(|s| s == "─"),
                "{texts:?}"
            );

            let term = rects
                .iter()
                .find(|r| r.fill == TERM_BG)
                .expect("fundo do terminal");
            let (cw, ch) = (
                term.rect.width() / t.cols as f32,
                term.rect.height() / t.rows as f32,
            );
            let cell = |c: f32| {
                Rect::from_min_size(term.rect.min + Vec2::new(c * cw, 0.0), Vec2::new(cw, ch))
            };
            // Os pontos saem numa malha so, de quadrados sem anti-serrilhado
            // (4 vertices, 2 triangulos), nunca como retangulos soltos.
            let green = Color32::from_rgb(10, 200, 30);
            assert!(rects.iter().all(|r| r.fill != green), "ppp={ppp}");
            let meshes: Vec<(usize, &egui::Mesh)> = shapes
                .iter()
                .enumerate()
                .filter_map(|(i, s)| match s {
                    egui::Shape::Mesh(m) => Some((i, &**m)),
                    _ => None,
                })
                .collect();
            assert_eq!(meshes.len(), 1, "ppp={ppp}");
            let (mesh_at, mesh) = meshes[0];
            assert_eq!(mesh.texture_id, egui::TextureId::default());
            assert!(mesh.vertices.iter().all(|v| v.color == green));
            assert_eq!(mesh.indices.len(), mesh.vertices.len() / 4 * 6);
            let dots: Vec<Rect> = mesh
                .vertices
                .chunks(4)
                .map(|q| Rect::from_min_max(q[0].pos, q[3].pos))
                .collect();
            let mut want: Vec<Rect> = braille_dots(0xff, cell(0.0), ppp).collect();
            want.extend(braille_dots(0x01, cell(1.0), ppp));
            assert_eq!(dots, want, "ppp={ppp}");
            // O fundo da celula vem antes dos pontos; o cursor, depois.
            let rect_at = |fill: Color32| {
                shapes
                    .iter()
                    .position(|s| matches!(s, egui::Shape::Rect(r) if r.fill == fill))
                    .unwrap()
            };
            let bg = rect_at(Color32::from_rgb(1, 2, 3));
            assert!(matches!(&shapes[bg], egui::Shape::Rect(r) if r.rect == cell(1.0)));
            assert!(bg < mesh_at);
            assert!(mesh_at < rect_at(Color32::from_rgba_unmultiplied(0xcc, 0xcc, 0xcc, 0x88)));
        }
    }

    // --- Rolagem (historico, roda, teclas, selecao) ------------------------

    /// "L1\r\n" .. "L{to}\r\n".
    fn numbered(from: usize, to: usize) -> Vec<u8> {
        (from..=to)
            .flat_map(|i| format!("L{i}\r\n").into_bytes())
            .collect()
    }

    /// Texto da fileira `r` da visao atual, sem espacos no fim.
    fn view_row(t: &Terminal, r: u16) -> String {
        row_text(t, r, 0, t.cols).trim_end().to_string()
    }

    /// Estado da contagem do historico.
    fn scroll_state(t: &Terminal) -> (i64, usize, usize, usize) {
        (t.pushed, t.hist, t.hidden, t.view)
    }

    #[test]
    fn history_keeps_what_scrolls_off() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        // Na tela: L27..L30 e a linha vazia do cursor; no historico, L1..L26.
        assert_eq!((t.hist, t.avail(), t.pushed), (26, 26, 26));
        assert_eq!(view_row(&t, 0), "L27");
        t.set_view(usize::MAX);
        assert_eq!(t.view, 26);
        assert_eq!(view_row(&t, 0), "L1");
        assert_eq!(view_row(&t, 4), "L5");
        t.scroll_by(-24);
        assert_eq!(view_row(&t, 0), "L25");
    }

    #[test]
    fn output_does_not_pull_the_view_back() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.set_view(10);
        let before = view_row(&t, 0);
        assert!(!t.unseen);
        t.process(&numbered(31, 33));
        assert_eq!(t.view, 13);
        assert_eq!(view_row(&t, 0), before);
        assert!(t.unseen);
        // No fim a visao acompanha e o aviso some.
        t.scroll_to_bottom();
        assert!(!t.unseen);
        t.process(&numbered(34, 35));
        assert_eq!((t.view, view_row(&t, 3)), (0, "L35".to_string()));
        // Com o historico cheio a visao continua no mesmo texto ate o topo.
        let mut t = Terminal::with_scrollback(20, 5, 10);
        t.process(&numbered(1, 30));
        t.set_view(4);
        let before = view_row(&t, 0);
        t.process(&numbered(31, 33));
        assert_eq!((t.view, view_row(&t, 0)), (7, before));
        t.process(&numbered(34, 40));
        assert_eq!(t.view, 10, "satura no topo");
    }

    /// O mesmo fluxo cortado em qualquer ponto conta o mesmo (linhas
    /// empurradas, tamanho, escondidas, visao) e mostra o mesmo texto.
    #[test]
    fn counting_is_the_same_for_every_cut() {
        let mut s = numbered(1, 14);
        s.extend_from_slice(b"\x1b[31mcor\x1b[0m\r\n");
        s.extend_from_slice(&numbered(15, 18));
        s.extend_from_slice(b"\x1b[3J\x1b[H\x1b[2J$ "); // clear
        s.extend_from_slice(&numbered(19, 26));
        s.extend_from_slice(b"\x1b[?1049h\x1b[2Jalt\r\n\x1b[?1049l");
        s.extend_from_slice(&numbered(27, 29));
        for cap in [4usize, 8, 100] {
            // Com o historico transbordando num trecho so, a contagem
            // absoluta (so usada pela selecao, que entao some) pode diferir.
            let key = |t: &Terminal| {
                let (pushed, hist, hidden, view) = scroll_state(t);
                ((cap > 20).then_some(pushed), hist, hidden, view)
            };
            let mut whole = Terminal::with_scrollback(12, 4, cap);
            whole.process(&s);
            let want = (key(&whole), whole.parser.screen().contents());
            for cut in 0..=s.len() {
                let mut t = Terminal::with_scrollback(12, 4, cap);
                t.process(&s[..cut]);
                t.process(&s[cut..]);
                assert_eq!(
                    (key(&t), t.parser.screen().contents()),
                    want,
                    "cap {cap}, corte em {cut}"
                );
            }
            // Byte a byte.
            let mut t = Terminal::with_scrollback(12, 4, cap);
            for b in &s {
                t.process(std::slice::from_ref(b));
            }
            assert_eq!(key(&t), want.0, "cap {cap}, byte a byte");
        }
    }

    /// Fluxos aleatorios (linhas, ED2/ED3, ESC c, 1049, SU, DECSTBM, CSI
    /// partidos) cortados ao acaso, com a visao no historico ou no fim: a
    /// contagem e a tela sao as mesmas de uma vez so, e nada estoura.
    #[test]
    fn random_streams_count_the_same_for_any_cut() {
        const TOKENS: &[&[u8]] = &[
            b"x\r\n",
            b"yy\r\n",
            b"\n",
            b"\x1b[3J",
            b"\x1b[H\x1b[2J",
            b"\x1bc",
            b"\x1b[?1049h",
            b"\x1b[?1049l",
            b"\x1b[2S",
            b"\x1b[1;3r",
            b"\x1b[r",
            b"abc",
            b"\x1b[3",
            b"J",
            b"\x1b",
            b"\xc3\xa7",
        ];
        let mut rng = crate::vtfix::tests::Rng(0x5c01_1bac_0000_0001);
        for round in 0..6_000 {
            // Metade sem ESC c: tudo exato. Com ESC c (que tambem tira da
            // alternativa sem aviso), as escondidas podem zerar (conservador:
            // mostra tudo) conforme o corte; o resto continua igual.
            let with_reset = round % 2 == 1;
            let tokens: Vec<&[u8]> = TOKENS
                .iter()
                .copied()
                .filter(|t| with_reset || *t != b"\x1bc")
                .collect();
            let s = rng.tokens(&tokens, 30);
            let start_view = rng.below(3);
            let prefix = numbered(1, 12);
            let run = |cuts: &[usize]| {
                let mut t = Terminal::with_scrollback(8, 4, 60);
                t.process(&prefix);
                t.set_view(start_view);
                let mut from = 0;
                for &c in cuts.iter().chain(std::iter::once(&s.len())) {
                    t.process(&s[from..c]);
                    from = c;
                    assert!(t.hidden <= t.hist && t.view <= t.avail());
                }
                let (_, hist, hidden, view) = scroll_state(&t);
                let hidden = (!with_reset).then_some(hidden);
                (hist, hidden, view, t.parser.screen().contents())
            };
            let whole = run(&[]);
            let mut cuts: Vec<usize> = (0..1 + rng.below(4))
                .map(|_| rng.below(s.len() + 1))
                .collect();
            cuts.sort_unstable();
            assert_eq!(
                run(&cuts),
                whole,
                "cortes {cuts:?}: {:?}",
                String::from_utf8_lossy(&s)
            );
            let every: Vec<usize> = (1..s.len()).collect();
            assert_eq!(
                run(&every),
                whole,
                "byte a byte: {:?}",
                String::from_utf8_lossy(&s)
            );
        }
    }

    #[test]
    fn clear_hides_history_and_ctrl_l_keeps_it() {
        // Bytes reais do bash 4.4 / ncurses 6.1 (AlmaLinux 8, xterm-256color):
        // `clear` = ED3 + CUP + ED2; Ctrl+L = CUP + ED2.
        const CLEAR: &[u8] = b"\x1b[3J\x1b[H\x1b[2J";
        const CTRL_L: &[u8] = b"\x1b[H\x1b[2J";
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 60));
        assert_eq!(t.avail(), 56);
        t.process(CLEAR);
        t.process(b"$ ");
        assert_eq!(t.avail(), 0, "clear apaga o historico");
        t.process(&numbered(61, 70));
        assert_eq!(t.avail(), 6);
        t.set_view(usize::MAX);
        assert_eq!(view_row(&t, 0), "$ L61");
        t.scroll_to_bottom();
        t.process(CTRL_L);
        assert_eq!(t.avail(), 6, "Ctrl+L so limpa a tela");
        // ED3 no meio de um trecho, com o historico cheio: so as linhas
        // empurradas depois dele ficam visiveis.
        let mut t = Terminal::with_scrollback(20, 5, 10);
        t.process(&numbered(1, 40));
        t.process(b"a\r\nb\r\n\x1b[3Jc\r\nd\r\ne\r\n");
        assert_eq!(t.avail(), 3);
        t.set_view(usize::MAX);
        // L37 e L38 subiram antes do ED3; L39, L40 e "a", depois.
        assert_eq!(view_row(&t, 0), "L39");
        // "CSI ? 3 J" nao e o ED3.
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 20));
        t.process(b"\x1b[?3J");
        assert_eq!(t.avail(), 16);
        // Evictions depois do ED3 reduzem as escondidas.
        let mut t = Terminal::with_scrollback(20, 5, 10);
        t.process(&numbered(1, 12));
        t.process(b"\x1b[3J");
        assert_eq!((t.hist, t.hidden, t.avail()), (8, 8, 0));
        t.process(&numbered(13, 17));
        assert_eq!((t.hist, t.hidden, t.avail()), (10, 5, 5));
        t.process(&numbered(18, 40));
        assert_eq!((t.hidden, t.avail()), (0, 10));
    }

    #[test]
    fn reset_drops_history_and_view() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.set_view(7);
        t.process(b"\x1bc");
        assert_eq!((t.hist, t.avail(), t.view), (0, 0, 0));
        t.process(&numbered(1, 8));
        assert_eq!(t.avail(), 4);
        // Programa que morreu na tela alternativa e o usuario digita `reset`
        // (ESC c tira da alternativa sem o 1049): historico novo, nada escondido.
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.process(b"\x1b[3J");
        t.process(b"\x1b[?1049h\x1b[2Jtela presa\r\n");
        t.process(b"\x1bc\x1b]104\x07\x1b[!p\x1b[?3;4l\x1b[4l\x1b>");
        t.process(&numbered(1, 8));
        assert!(!t.parser.screen().alternate_screen());
        assert_eq!((t.hist, t.hidden, t.view), (4, 0, 0));
        // `clear` e, mais tarde, `reset` na tela principal chegando junto com
        // a saida seguinte: o historico novo nao herda as escondidas.
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.process(b"\x1b[3J");
        let mut s = b"\x1bc".to_vec();
        s.extend(numbered(1, 30));
        t.process(&s);
        assert_eq!((t.hist, t.hidden, t.avail()), (26, 0, 26));
    }

    #[test]
    fn alt_screen_has_no_history_and_resets_view() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.set_view(7);
        t.sel_anchor = Some((3, 0));
        t.sel_head = Some((4, 2));
        t.process(b"\x1b[?1049h");
        assert_eq!(t.view, 0);
        assert!(t.sel_anchor.is_none());
        t.process(&numbered(1, 50));
        assert_eq!(t.hist, 26, "nada da alternativa vai ao historico");
        t.process(b"\x1b[?1049l");
        assert_eq!((t.view, t.hist), (0, 26));
        // CSI ? 47 h no meio de um trecho (sem o aviso do 1049).
        t.set_view(3);
        t.process(b"x\r\n\x1b[?47hy\r\n\x1b[?47lz\r\n");
        assert_eq!(t.view, 0);
    }

    #[test]
    fn clear_survives_a_full_screen_program() {
        // Historico cheio, `clear` e logo depois um programa de tela cheia
        // (vim, less, tmux: 1049): na volta, o que o clear apagou continua
        // fora do alcance, com o fluxo cortado em qualquer ponto.
        let mut s = b"\x1b[3J\x1b[H\x1b[2J$ vim\r\n".to_vec();
        s.extend_from_slice(b"\x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[H\x1b[2J~\r\n~");
        s.extend_from_slice(b"\x1b[?1l\x1b>\x1b[?1049l\x1b[23;0;0t$ ");
        for cap in [10, 100] {
            for cut in 0..=s.len() {
                let mut t = Terminal::with_scrollback(20, 5, cap);
                t.process(&numbered(1, 200));
                assert_eq!(t.hist, cap);
                t.process(&s[..cut]);
                t.process(&s[cut..]);
                assert!(!t.parser.screen().alternate_screen());
                assert_eq!(
                    (t.hist, t.hidden, t.avail()),
                    (cap, cap, 0),
                    "cap {cap}, corte {cut}"
                );
                assert_eq!(view_row(&t, 0), "$ vim");
                assert_eq!(view_row(&t, 1), "$");
            }
        }
    }

    #[test]
    fn new_keeps_scrollback_lines() {
        // O construtor do app (os outros testes usam historicos pequenos).
        let mut t = Terminal::new(80, 24);
        t.process(&numbered(1, SCROLLBACK_LINES + 100));
        assert_eq!((t.hist, t.avail()), (SCROLLBACK_LINES, SCROLLBACK_LINES));
        // Na tela, L5078..L5100 e a linha do cursor; acima, as 5.000 anteriores.
        t.set_view(usize::MAX);
        assert_eq!(view_row(&t, 0), "L78");
    }

    #[test]
    fn overflow_in_one_chunk_drops_selection_and_keeps_reader_on_top() {
        // Mais linhas num pacote do que cabem no historico (seq rapido): a
        // contagem absoluta se perde, a selecao some e quem lia fica no topo.
        let mut t = Terminal::with_scrollback(20, 5, 50);
        t.process(&numbered(1, 60));
        t.set_view(10);
        t.sel_anchor = Some((t.abs_row(0), 0));
        t.sel_head = Some((t.abs_row(1), 2));
        t.process(&numbered(61, 200));
        assert!(t.selection_range().is_none());
        assert_eq!((t.view, t.avail(), t.unseen), (50, 50, true));
        assert_eq!(view_row(&t, 0), "L147");
        // Quem estava no fim continua no fim.
        let mut t = Terminal::with_scrollback(20, 5, 50);
        t.process(&numbered(1, 60));
        t.process(&numbered(61, 200));
        assert_eq!((t.view, view_row(&t, 3)), (0, "L200".to_string()));
        // Transbordou antes de um `clear` no mesmo pacote: a contagem antes
        // dele se perde (a selecao some); visiveis, so as linhas depois dele.
        let mut t = Terminal::with_scrollback(20, 5, 10);
        t.process(&numbered(1, 30));
        t.sel_anchor = Some((t.abs_row(1), 0));
        t.sel_head = Some((t.abs_row(2), 2));
        let mut s = numbered(31, 60);
        s.extend_from_slice(b"\x1b[3Jx\r\ny\r\n");
        t.process(&s);
        assert!(t.selection_range().is_none());
        assert_eq!((t.hist, t.hidden, t.view), (10, 8, 0));
    }

    #[test]
    fn reset_out_of_alt_screen_in_one_packet() {
        // Programa preso na tela alternativa, `clear` antes dele, e o `reset`
        // (ESC c) chega junto com as linhas seguintes: historico novo, nada
        // escondido, mesmo com mais trocas de tela no mesmo pacote.
        let stuck = || {
            let mut t = Terminal::with_scrollback(20, 5, 100);
            t.process(&numbered(1, 30));
            t.process(b"\x1b[3J");
            t.process(b"\x1b[?1049h\x1b[2Jpreso\r\n");
            t
        };
        let mut s = b"\x1bc".to_vec();
        s.extend(numbered(1, 30));
        let mut t = stuck();
        t.process(&s);
        assert_eq!((t.hist, t.hidden, t.avail()), (26, 0, 26));
        s.extend_from_slice(b"\x1b[?1049hvim\x1b[?1049l$ ");
        let mut t = stuck();
        t.process(&s);
        assert_eq!((t.hist, t.hidden, t.avail()), (26, 0, 26));
    }

    #[test]
    fn clear_and_screen_switches_in_one_packet() {
        // Saida da tela alternativa (ESC c ou CSI ? 47 l) e `clear` no mesmo
        // pacote: o clear vale; so as linhas depois dele ficam visiveis.
        for exit in [&b"\x1bc"[..], b"\x1b[?47l"] {
            let mut t = Terminal::with_scrollback(20, 5, 100);
            t.process(&numbered(1, 30));
            t.process(b"\x1b[?47h");
            let mut s = exit.to_vec();
            s.extend(numbered(31, 40));
            s.extend_from_slice(b"\x1b[3J\x1b[H\x1b[2J$ ");
            s.extend(numbered(41, 47));
            t.process(&s);
            assert_eq!(t.avail(), 3, "{exit:?}");
            t.set_view(usize::MAX);
            assert_eq!(view_row(&t, 0), "$ L41", "{exit:?}");
        }
        // `clear` e CSI ? 47 h no mesmo pacote: o que o clear escondeu
        // continua escondido (e depois da volta, pelo 1049 exato).
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.process(b"\x1b[3J\x1b[H\x1b[2J$ \x1b[?47h");
        assert_eq!((t.hist, t.hidden), (26, 26));
        t.process(b"alt\x1b[?1049l");
        assert_eq!((t.hist, t.avail()), (26, 0));
        // Um ED 3 ja na tela alternativa nao mexe no historico da principal.
        let mut t = Terminal::with_scrollback(20, 5, 1000);
        t.process(&numbered(1, 30));
        t.process(b"\x1b[3J");
        t.process(b"\x1b[?47h\x1b[3J\x1b[H\x1b[2Jalt");
        t.process(b"\x1b[?1049l");
        assert_eq!((t.hist, t.hidden), (26, 26));
        // Entrada que pode ter empurrado linhas para fora do historico cheio
        // num trecho sem contagem: as escondidas deixam de valer (mostra
        // tudo; nunca esconde o que o clear nao apagou).
        let mut t = Terminal::with_scrollback(20, 5, 10);
        t.process(&numbered(1, 30));
        t.process(b"\x1b[3J\x1b[H\x1b[2J$ ");
        assert_eq!((t.hist, t.hidden), (10, 10));
        t.process(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n\x1b[?47h");
        assert_eq!(t.hidden, 0);
    }

    #[test]
    fn shrinking_mid_sequence_injects_nothing() {
        // O pacote acabou no meio de um escape ou de uma string do servidor
        // ("ESC (" do sgr0 do ncurses, o titulo do PROMPT_COMMAND, um DCS) e
        // o painel encolhe nesse instante: o ESC do empurrao abortaria a
        // sequencia e o resto dela iria para a tela. Nada e injetado: fica o
        // corte do vt100 (o shell redesenha o prompt no SIGWINCH).
        for (head, tail) in [
            (&b"$ \x1b("[..], &b"B\x1b[mok"[..]),
            (b"$ \x1b)", b"0ok"),
            (b"$ \x1b]0;root@srv01:", b"~\x07ok"),
            (b"$ \x1bP1$r", b"0m\x1b\\ok"),
        ] {
            let mut t = Terminal::with_scrollback(20, 10, 100);
            t.process(&numbered(1, 9));
            t.process(head);
            t.resize(20, 5);
            t.process(tail);
            assert_eq!(
                (t.hist, view_row(&t, 4)),
                (0, "L5ok".to_string()),
                "{head:?}"
            );
            // Com a sequencia completa, encolher volta a empurrar.
            t.resize(20, 3);
            assert_eq!((t.hist, view_row(&t, 2)), (2, "L5ok".to_string()));
        }
    }

    #[test]
    fn shrinking_while_scrolled_keeps_the_text_in_view() {
        let mut t = Terminal::with_scrollback(20, 10, 100);
        t.process(&numbered(1, 40));
        t.process(b"$ ");
        t.set_view(5);
        let top = view_row(&t, 0);
        t.resize(20, 6);
        // As linhas empurradas ao encolher nao sao saida nova.
        assert_eq!((view_row(&t, 0), t.unseen), (top.clone(), false));
        t.resize(20, 12);
        assert_eq!(view_row(&t, 0), top);
    }

    #[test]
    fn copy_keeps_history_lines_wider_than_the_pane() {
        // Linhas que subiram com o painel mais largo (depois dividido com
        // Ctrl+B, V): a grade mostra o comeco, e selecionar ate a borda
        // copia a linha inteira; quebrada na largura antiga, junta com a
        // seguinte.
        let digits = "0123456789".repeat(4);
        let long: String = (0..50u8).map(|i| char::from(b'a' + i % 26)).collect();
        let mut t = Terminal::with_scrollback(40, 5, 100);
        t.process(format!("{digits}\r\n{long}\r\n").as_bytes());
        t.process(&numbered(1, 10));
        t.resize(20, 5);
        t.set_view(usize::MAX);
        assert_eq!(view_row(&t, 0), &digits[..20]);
        t.sel_anchor = Some((t.abs_row(0), 0));
        t.sel_head = Some((t.abs_row(0), 19));
        assert_eq!(t.selection_text().unwrap(), digits);
        t.sel_head = Some((t.abs_row(2), 19));
        assert_eq!(t.selection_text().unwrap(), format!("{digits}\n{long}"));
        // Ate o meio da linha: so o que a grade mostra.
        t.sel_head = Some((t.abs_row(0), 4));
        assert_eq!(t.selection_text().unwrap(), "01234");
    }

    /// Janela de egui com o terminal: `Harness::frame` roda um quadro com os
    /// eventos dados (tema claro do Windows simulado).
    struct Harness {
        ctx: egui::Context,
        size: Vec2,
        cell: (f32, f32),
        time: f64,
        /// Tempo (s) que passa a cada quadro.
        dt: f64,
    }

    impl Harness {
        fn new(t: &mut Terminal) -> Self {
            Self::with(t, 1.0, 0.5)
        }

        /// Escala `ppp` e `spare` pontos de sobra a direita e embaixo da
        /// grade (como num painel real, que quase nunca e multiplo da celula).
        fn with(t: &mut Terminal, ppp: f32, spare: f32) -> Self {
            let ctx = egui::Context::default();
            ctx.set_pixels_per_point(ppp);
            let mut cell = (0.0, 0.0);
            for _ in 0..2 {
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    cell = ctx.fonts(|f| cell_size(f, &FontId::monospace(14.0), ppp));
                });
            }
            let size = Vec2::new(
                f32::from(t.cols) * cell.0 + 16.0 + spare,
                f32::from(t.rows) * cell.1 + 16.0 + spare,
            );
            let mut h = Harness {
                ctx,
                size,
                cell,
                time: 0.0,
                dt: 0.1,
            };
            h.frame(t, vec![]);
            h
        }

        /// Area da grade do terminal (a margem do painel e de 8 pontos).
        fn grid(&self, t: &Terminal) -> Rect {
            Rect::from_min_size(
                Pos2::new(8.0, 8.0),
                Vec2::new(
                    f32::from(t.cols) * self.cell.0,
                    f32::from(t.rows) * self.cell.1,
                ),
            )
        }

        /// Centro da celula (linha, coluna) da visao.
        fn at(&self, r: u16, c: u16) -> Pos2 {
            Pos2::new(
                8.0 + (f32::from(c) + 0.5) * self.cell.0,
                8.0 + (f32::from(r) + 0.5) * self.cell.1,
            )
        }

        fn frame(
            &mut self,
            t: &mut Terminal,
            events: Vec<egui::Event>,
        ) -> (Vec<u8>, egui::FullOutput) {
            self.time += self.dt;
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
                time: Some(self.time),
                system_theme: Some(egui::Theme::Light),
                events,
                ..Default::default()
            };
            let mut bytes = Vec::new();
            let full = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    bytes = t.ui(ui).input;
                });
            });
            (bytes, full)
        }

        fn wheel(
            &mut self,
            t: &mut Terminal,
            at: Pos2,
            unit: egui::MouseWheelUnit,
            dy: f32,
            modifiers: egui::Modifiers,
        ) -> Vec<u8> {
            self.frame(
                t,
                vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::MouseWheel {
                        unit,
                        delta: Vec2::new(0.0, dy),
                        modifiers,
                    },
                ],
            )
            .0
        }

        fn key(&mut self, t: &mut Terminal, key: egui::Key, modifiers: egui::Modifiers) -> Vec<u8> {
            self.frame(
                t,
                vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
            )
            .0
        }
    }

    fn texts(full: &egui::FullOutput) -> Vec<String> {
        full.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    /// Textos copiados para a area de transferencia no quadro.
    fn copied(full: &egui::FullOutput) -> Vec<String> {
        full.platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }

    const NO_MODS: egui::Modifiers = egui::Modifiers::NONE;
    const LINE: egui::MouseWheelUnit = egui::MouseWheelUnit::Line;

    #[test]
    fn wheel_scrolls_history_three_lines_per_click() {
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        let p = h.at(2, 3);
        assert!(h.wheel(&mut t, p, LINE, 1.0, NO_MODS).is_empty());
        assert_eq!(t.view, 3);
        h.wheel(&mut t, p, LINE, -1.0, NO_MODS);
        assert_eq!(t.view, 0);
        // Touchpad: fracoes acumulam.
        for _ in 0..4 {
            h.wheel(&mut t, p, LINE, 0.25, NO_MODS);
        }
        assert_eq!(t.view, 3);
        // Em pontos: uma linha por altura de celula.
        let ch = h.cell.1;
        h.wheel(&mut t, p, egui::MouseWheelUnit::Point, 2.0 * ch, NO_MODS);
        assert_eq!(t.view, 5);
        // Ctrl+roda e do zoom do egui: nada.
        h.wheel(&mut t, p, LINE, 1.0, egui::Modifiers::CTRL);
        assert_eq!(t.view, 5);
        // Fora do terminal: nada.
        h.wheel(&mut t, Pos2::new(1.0, 1.0), LINE, 1.0, NO_MODS);
        assert_eq!(t.view, 5);
        // Nao passa do topo nem do fim.
        h.wheel(&mut t, p, LINE, 100.0, NO_MODS);
        assert_eq!(t.view, t.avail());
        h.wheel(&mut t, p, LINE, -100.0, NO_MODS);
        assert_eq!(t.view, 0);
        // Touchpad: o resto de um sentido nao vale no outro (0,9 linha para
        // cima e depois 1,2 para baixo: desce uma, nao fica parado).
        t.set_view(5);
        h.wheel(&mut t, p, LINE, 0.3, NO_MODS);
        assert_eq!(t.view, 5);
        h.wheel(&mut t, p, LINE, -0.4, NO_MODS);
        assert_eq!(t.view, 4);
    }

    #[test]
    fn wheel_scrolls_a_terminal_without_focus() {
        // Outro painel com o foco: a roda vale onde esta o ponteiro.
        let mut t = Terminal::with_scrollback(30, 6, 100);
        t.want_focus = false;
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        let (bytes, _) = h.frame(&mut t, vec![egui::Event::Text("a".into())]);
        assert!(bytes.is_empty(), "sem foco o teclado nao vem para ca");
        let p = h.at(2, 3);
        h.wheel(&mut t, p, LINE, 1.0, NO_MODS);
        assert_eq!(t.view, 3);
    }

    // Capturas reais por pty (python3 pty, sem sshd) no AlmaLinux 8, com
    // TERM=xterm-256color e 80x24: bytes exatos que os programas mandaram.

    /// Attach do tmux 2.7 com "set -g mouse on" (bash dentro, PS1 padrao):
    /// tela alternativa, teclado de aplicacao e mouse SGR (?1006h ?1002h).
    const TMUX_MOUSE_ATTACH: &[u8] = b"\
        \x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[H\x1b[2J\x1b[?12l\x1b[?25h\
        \x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1005l\x1b[c\x1b(B\x1b[m\x1b[?12;25h\
        \x1b[?12l\x1b[?25h\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[1;1H\x1b[1;24r\
        \x1b]112\x07\x1b[1;1H\x1b[?1006h\x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\x1b[H\x1b[?12l\x1b[?25h\x1b(B\x1b[m\x1b[?12;25h\x1b[?12l\x1b[?25h\
        \x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[1;1H\x1b[1;24r\x1b[1;1H\x1b[?1006h\
        \x1b[?1002h\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[H\x1b[?12l\x1b[?25hbash-4.4$ ";

    /// O mesmo attach sem o mouse ligado: nenhum modo de mouse (so os ?100xl).
    const TMUX_PLAIN_ATTACH: &[u8] = b"\
        \x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\x1b[H\x1b[2J\x1b[?12l\x1b[?25h\
        \x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1005l\x1b[c\x1b(B\x1b[m\x1b[?12;25h\
        \x1b[?12l\x1b[?25h\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[1;1H\x1b[1;24r\
        \x1b]112\x07\x1b[1;1H\x1b[?25l\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\x1b[H\x1b[?12l\
        \x1b[?25h\x1b(B\x1b[m\x1b[?12;25h\x1b[?12l\x1b[?25h\x1b[?1003l\x1b[?1006l\
        \x1b[?2004l\x1b[1;1H\x1b[1;24r\x1b[1;1H\x1b[?25l\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\x1b[K\r\n\
        \x1b[K\x1b[H\x1b[?12l\x1b[?25hbash-4.4$ ";

    /// Inicio do htop 3.2.1: mouse pedido junto com o SGR num CSI so.
    const HTOP_START: &[u8] = b"\
        \x1b[?1049h\x1b[22;0;0t\x1b[1;24r\x1b(B\x1b[m\x1b[4l\x1b[?7h\x1b[?1h\
        \x1b=\x1b[?25l\x1b[39;49m\x1b[?1006;1000h";

    /// Inicio do less 530 (`less /etc/services`): sem mouse.
    const LESS_START: &[u8] = b"\x1b[?1049h\x1b[22;0;0t\x1b[?1h\x1b=\r# /etc/services:\r\n";

    #[test]
    fn wheel_goes_to_the_program_that_asked_for_mouse() {
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 20));
        t.process(TMUX_MOUSE_ATTACH);
        let p = h.at(4, 9);
        assert_eq!(h.wheel(&mut t, p, LINE, 1.0, NO_MODS), b"\x1b[<64;10;5M");
        assert_eq!(
            h.wheel(&mut t, p, LINE, -2.0, NO_MODS),
            b"\x1b[<65;10;5M\x1b[<65;10;5M"
        );
        // Touchpad: um evento a cada clique inteiro.
        assert!(h.wheel(&mut t, p, LINE, 0.5, NO_MODS).is_empty());
        assert_eq!(h.wheel(&mut t, p, LINE, 0.5, NO_MODS), b"\x1b[<64;10;5M");
        // Alt soma 8 no botao.
        assert_eq!(
            h.wheel(&mut t, p, LINE, 1.0, egui::Modifiers::ALT),
            b"\x1b[<72;10;5M"
        );
        // Shift forca a rolagem local; na alternativa nao ha: so a dica.
        let (bytes, _) = h.frame(
            &mut t,
            vec![egui::Event::MouseWheel {
                unit: LINE,
                delta: Vec2::new(0.0, 1.0),
                modifiers: egui::Modifiers::SHIFT,
            }],
        );
        assert!(bytes.is_empty());
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(texts(&full).iter().any(|s| s.contains("Ctrl+B, Ctrl+B, [")));
        // Codificacao padrao (X10) e UTF-8 (1005).
        t.process(b"\x1b[?1006l\x1b[?1000h");
        assert_eq!(
            h.wheel(&mut t, p, LINE, 1.0, NO_MODS),
            [0x1b, b'[', b'M', 96, 42, 37]
        );
        t.process(b"\x1b[?1005h");
        assert_eq!(
            h.wheel(&mut t, p, LINE, -1.0, NO_MODS),
            [0x1b, b'[', b'M', 97, 42, 37]
        );
        assert_eq!(
            wheel_event(64, 200, 5, vt100::MouseProtocolEncoding::Default)[4],
            232
        );
        assert_eq!(
            wheel_event(64, 300, 5, vt100::MouseProtocolEncoding::Default)[4],
            255
        );
        assert_eq!(
            wheel_event(64, 300, 5, vt100::MouseProtocolEncoding::Utf8),
            "\x1b[M`\u{14c}%".as_bytes()
        );
        // Tela principal com mouse pedido (ex.: fzf --height): a roda vai ao
        // programa; Shift+roda rola o historico.
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 20));
        t.process(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(h.wheel(&mut t, p, LINE, 1.0, NO_MODS), b"\x1b[<64;10;5M");
        assert_eq!(t.view, 0);
        assert!(h
            .wheel(&mut t, p, LINE, 1.0, egui::Modifiers::SHIFT)
            .is_empty());
        assert_eq!(t.view, 3);
        // Ja no historico, a roda continua local (a tela do programa nem aparece).
        assert!(h.wheel(&mut t, p, LINE, -1.0, NO_MODS).is_empty());
        assert_eq!(t.view, 0);
    }

    #[test]
    fn alt_screen_without_mouse_sends_nothing_and_hints() {
        // tmux sem mouse, less, man, vim: nada vai ao programa (setas iriam ao
        // historico de comandos do shell dentro do tmux).
        let mut t = Terminal::with_scrollback(70, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(TMUX_PLAIN_ATTACH);
        let p = h.at(2, 2);
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(!texts(&full).iter().any(|s| s.contains("tmux")));
        assert!(h.wheel(&mut t, p, LINE, 1.0, NO_MODS).is_empty());
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(
            texts(&full).iter().any(|s| s.contains("Ctrl+B, Ctrl+B, [")),
            "{:?}",
            texts(&full)
        );
        // Some sozinha depois de alguns segundos.
        h.time += HINT_SECS;
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(!texts(&full).iter().any(|s| s.contains("tmux")));
        // Shift+PgUp tambem so mostra a dica; Shift+Home vai como Home (o
        // tmux liga o modo de cursor de aplicacao: ESC O H).
        assert!(h
            .key(&mut t, egui::Key::PageUp, egui::Modifiers::SHIFT)
            .is_empty());
        assert_eq!(
            h.key(&mut t, egui::Key::Home, egui::Modifiers::SHIFT),
            b"\x1bOH"
        );
        assert_eq!(h.key(&mut t, egui::Key::PageUp, NO_MODS), b"\x1b[5~");
    }

    #[test]
    fn scroll_keys_and_typing() {
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        let shift = egui::Modifiers::SHIFT;
        assert!(h.key(&mut t, egui::Key::PageUp, shift).is_empty());
        assert_eq!(t.view, 5);
        h.key(&mut t, egui::Key::PageUp, shift);
        h.key(&mut t, egui::Key::PageDown, shift);
        assert_eq!(t.view, 5);
        h.key(&mut t, egui::Key::Home, shift);
        assert_eq!(t.view, t.avail());
        h.key(&mut t, egui::Key::End, egui::Modifiers::CTRL | shift);
        assert_eq!(t.view, 0);
        h.key(&mut t, egui::Key::Home, egui::Modifiers::CTRL | shift);
        assert_eq!(t.view, t.avail());
        // Qualquer coisa digitada vai ao servidor e volta ao fim.
        let (bytes, _) = h.frame(&mut t, vec![egui::Event::Text("a".into())]);
        assert_eq!((bytes.as_slice(), t.view), (&b"a"[..], 0));
        t.set_view(9);
        assert_eq!(h.key(&mut t, egui::Key::ArrowUp, NO_MODS), b"\x1b[A");
        assert_eq!(t.view, 0);
        t.set_view(9);
        let (bytes, _) = h.frame(&mut t, vec![egui::Event::Paste("ls".into())]);
        assert_eq!((bytes.as_slice(), t.view), (&b"ls"[..], 0));
        // Sem historico, Shift+PgUp nao manda nada.
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        assert!(h.key(&mut t, egui::Key::PageUp, shift).is_empty());
    }

    #[test]
    fn selection_is_anchored_to_text_and_copies_history() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        t.process(&numbered(1, 30));
        t.set_view(20);
        assert_eq!(view_row(&t, 1), "L8");
        let (a, b) = (t.abs_row(1), t.abs_row(3));
        t.sel_anchor = Some((a, 0));
        t.sel_head = Some((b, 1));
        // Rolar ou chegar saida nao muda o texto selecionado.
        t.scroll_by(-10);
        t.process(&numbered(31, 34));
        assert_eq!(t.selection_text().unwrap(), "L8\nL9\nL1");
        // Selecao maior que a tela, lida fora da visao; a visao nao muda.
        let view = t.view;
        t.sel_anchor = Some((t.pushed - t.avail() as i64, 0));
        t.sel_head = Some((t.pushed + 3, 2));
        let text = t.selection_text().unwrap();
        assert_eq!(text.lines().count(), 34);
        assert!(
            text.starts_with("L1\nL2\n") && text.ends_with("L33\nL34"),
            "{text}"
        );
        assert_eq!(t.view, view);
        // Linhas escondidas pelo clear ficam de fora.
        t.process(b"\x1b[3J");
        t.sel_anchor = Some((0, 0));
        t.sel_head = Some((t.pushed + 1, 5));
        assert_eq!(t.selection_text().unwrap(), "L31\nL32");
    }

    #[test]
    fn selection_overlay_follows_the_text() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 30));
        t.set_view(10);
        let abs = t.abs_row(2);
        t.sel_anchor = Some((abs, 0));
        t.sel_head = Some((abs, 3));
        let overlay = Color32::from_rgba_unmultiplied(0x9b, 0xa3, 0xb4, 0x55);
        let sel_y = |full: &egui::FullOutput| -> Vec<f32> {
            full.shapes
                .iter()
                .filter_map(|c| match &c.shape {
                    egui::Shape::Rect(r) if r.fill == overlay => Some(r.rect.min.y),
                    _ => None,
                })
                .collect()
        };
        let (_, full) = h.frame(&mut t, vec![]);
        assert_eq!(sel_y(&full), vec![8.0 + 2.0 * h.cell.1]);
        t.scroll_by(1);
        let (_, full) = h.frame(&mut t, vec![]);
        assert_eq!(sel_y(&full), vec![8.0 + 3.0 * h.cell.1]);
        t.scroll_by(-11);
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(sel_y(&full).is_empty(), "fora da visao");
    }

    #[test]
    fn drag_select_copies_across_scroll() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 30));
        t.set_view(10);
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: NO_MODS,
        };
        let a = h.at(1, 0);
        h.frame(&mut t, vec![egui::Event::PointerMoved(a), press(a, true)]);
        let m = h.at(3, 2);
        h.frame(&mut t, vec![egui::Event::PointerMoved(m)]);
        // Roda com o botao preso: a visao rola 3 linhas e a selecao estende.
        h.frame(
            &mut t,
            vec![egui::Event::MouseWheel {
                unit: LINE,
                delta: Vec2::new(0.0, -1.0),
                modifiers: NO_MODS,
            }],
        );
        let e = h.at(3, 3);
        h.frame(&mut t, vec![egui::Event::PointerMoved(e)]);
        let (_, full) = h.frame(&mut t, vec![press(e, false)]);
        let copied: Vec<String> = full
            .platform_output
            .commands
            .iter()
            .filter_map(|c| match c {
                egui::OutputCommand::CopyText(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(copied, vec!["L18\nL19\nL20\nL21\nL22\nL23"]);
    }

    #[test]
    fn soft_wrapped_lines_copy_as_one() {
        let mut t = Terminal::with_scrollback(10, 4, 100);
        t.process(b"0123456789abcdef\r\nx\r\n");
        t.sel_anchor = Some((t.abs_row(0), 0));
        t.sel_head = Some((t.abs_row(2), 0));
        assert_eq!(t.selection_text().unwrap(), "0123456789abcdef\nx");
    }

    #[test]
    fn shrinking_keeps_the_bottom_and_pushes_the_top() {
        let mut t = Terminal::with_scrollback(20, 10, 100);
        t.process(&numbered(1, 9));
        t.process(b"$ ");
        t.resize(20, 5);
        assert_eq!(view_row(&t, 4), "$");
        assert_eq!(view_row(&t, 0), "L6");
        assert_eq!((t.hist, t.parser.screen().cursor_position()), (5, (4, 2)));
        t.set_view(usize::MAX);
        assert_eq!(view_row(&t, 0), "L1");
        // Crescer so acrescenta linhas vazias embaixo.
        t.scroll_to_bottom();
        t.resize(20, 8);
        assert_eq!((view_row(&t, 4), t.hist), ("$".to_string(), 5));
        // Na alternativa, ou com uma sequencia pela metade, fica o corte do vt100.
        let mut t = Terminal::with_scrollback(20, 10, 100);
        t.process(&numbered(1, 9));
        t.process(b"$ \x1b[");
        t.resize(20, 5);
        assert_eq!((t.hist, view_row(&t, 0)), (0, "L1".to_string()));
        let mut t = Terminal::with_scrollback(20, 10, 100);
        t.process(b"\x1b[?1049h");
        t.process(&numbered(1, 9));
        t.resize(20, 5);
        assert_eq!(t.hist, 0);
    }

    #[test]
    fn position_pill_and_bar_only_when_scrolled() {
        let mut t = Terminal::with_scrollback(70, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(!texts(&full).iter().any(|s| s.contains('↑')));
        t.set_view(3);
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(
            texts(&full)
                .iter()
                .any(|s| s == "↑ 3 linhas  ·  Shift+End volta ao fim"),
            "{:?}",
            texts(&full)
        );
        t.process(&numbered(61, 62));
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(texts(&full)
            .iter()
            .any(|s| s.contains("↑ 5 linhas") && s.contains("saída nova")));
        // Clicar no aviso volta ao fim.
        let pill = full
            .shapes
            .iter()
            .find_map(|c| match &c.shape {
                egui::Shape::Text(t) if t.galley.text().contains('↑') => {
                    Some(t.pos + t.galley.size() / 2.0)
                }
                _ => None,
            })
            .unwrap();
        h.frame(&mut t, vec![egui::Event::PointerMoved(pill)]);
        let click = |pressed| egui::Event::PointerButton {
            pos: pill,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: NO_MODS,
        };
        h.frame(&mut t, vec![click(true)]);
        h.frame(&mut t, vec![click(false)]);
        assert_eq!(t.view, 0);
        // Estreito: so o numero.
        let mut t = Terminal::with_scrollback(30, 6, 5000);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 3000));
        t.set_view(1234);
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(
            texts(&full).iter().any(|s| s == "↑ 1.234"),
            "{:?}",
            texts(&full)
        );
    }

    /// Aviso pintado no quadro: o retangulo (achado pela borda), o fundo e
    /// o retangulo e o texto do rotulo (fonte de 12 pontos; a grade usa 14).
    fn pill_parts(full: &egui::FullOutput) -> Option<(Rect, Color32, Rect, String)> {
        let border = Color32::from_rgb(0x6e, 0x76, 0x84);
        let rect = full.shapes.iter().find_map(|c| match &c.shape {
            egui::Shape::Rect(r) if r.stroke.color == border => Some(r.rect),
            _ => None,
        })?;
        let fill = full.shapes.iter().find_map(|c| match &c.shape {
            egui::Shape::Rect(r) if r.rect == rect && r.fill != Color32::TRANSPARENT => {
                Some(r.fill)
            }
            _ => None,
        })?;
        let (label, text) = full.shapes.iter().find_map(|c| match &c.shape {
            egui::Shape::Text(t)
                if t.galley.job.sections.first().map(|s| s.format.font_id.size) == Some(12.0) =>
            {
                let r = Rect::from_min_size(t.pos, t.galley.size());
                Some((r, t.galley.text().to_string()))
            }
            _ => None,
        })?;
        Some((rect, fill, label, text))
    }

    /// O aviso cabe na grade, em celulas inteiras, com fundo opaco e sem
    /// cobrir a ultima coluna; o rotulo cabe nele. Devolve o retangulo e o
    /// texto.
    fn assert_pill_on_grid(
        h: &Harness,
        t: &Terminal,
        full: &egui::FullOutput,
        what: &str,
    ) -> (Rect, String) {
        let grid = h.grid(t);
        let (cw, ch) = h.cell;
        let (pill, fill, label, text) =
            pill_parts(full).unwrap_or_else(|| panic!("{what}: sem aviso"));
        let eps = 0.01;
        assert!(
            grid.expand(eps).contains_rect(pill),
            "{what}: aviso {pill:?} fora da grade {grid:?} ({text:?})"
        );
        assert!(
            pill.expand(eps).contains_rect(label),
            "{what}: rotulo {label:?} fora do aviso {pill:?} ({text:?})"
        );
        for (v, step) in [
            (pill.min.x - grid.min.x, cw),
            (pill.width(), cw),
            (pill.min.y - grid.min.y, ch),
            (pill.height(), ch),
        ] {
            let k = v / step;
            assert!(
                (k - k.round()).abs() < 1e-3,
                "{what}: {v} nao e multiplo da celula {step} ({pill:?})"
            );
        }
        assert!(
            pill.max.x <= grid.max.x - cw + eps,
            "{what}: aviso {pill:?} cobre a ultima coluna ({grid:?})"
        );
        assert_eq!(
            fill,
            Color32::from_rgb(0x1f, 0x1f, 0x24),
            "{what}: fundo translucido"
        );
        (pill, text)
    }

    #[test]
    fn pills_fit_and_sit_on_the_grid() {
        // A largura do texto muda com a escala (glifos arredondados ao pixel):
        // vale o texto mais largo que cabe, medido. Em celulas inteiras,
        // nenhum glifo fica cortado ao meio em volta do aviso.
        let hints = [
            "Esta tela não tem histórico (programa em tela cheia).\n\
             tmux: Ctrl+B, Ctrl+B, [ para rolar (q sai)  ·  less/man: PgUp/PgDn",
            "Sem histórico nesta tela.\ntmux: Ctrl+B, Ctrl+B, [",
            "Sem histórico",
        ];
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            for cols in [12u16, 16, 20, 24, 30, 40, 52, 60, 62, 64, 80, 100] {
                let what = format!("{cols} colunas, escala {ppp}");
                // Dica da tela alternativa (tmux sem mouse, less...); a mais
                // curta pede umas 14 colunas.
                if cols >= 16 {
                    let mut t = Terminal::with_scrollback(cols, 6, 5000);
                    let mut h = Harness::with(&mut t, ppp, 0.5);
                    t.process(b"\x1b[?1049h");
                    let p = h.at(2, 2);
                    h.wheel(&mut t, p, LINE, 1.0, NO_MODS);
                    let (_, full) = h.frame(&mut t, vec![]);
                    let (pill, text) = assert_pill_on_grid(&h, &t, &full, &what);
                    assert_eq!(pill.min.y, h.grid(&t).min.y, "{what}");
                    let i = hints
                        .iter()
                        .position(|s| *s == text)
                        .unwrap_or_else(|| panic!("{what}: dica {text:?}"));
                    match cols {
                        80.. => assert_eq!(i, 0, "{what}"),
                        30 | 40 => assert_eq!(i, 1, "{what}"),
                        ..=20 => assert_eq!(i, 2, "{what}"),
                        _ => {}
                    }
                }

                // Posicao no historico, com saida nova.
                let mut t = Terminal::with_scrollback(cols, 6, 5000);
                let mut h = Harness::with(&mut t, ppp, 0.5);
                t.process(&b"\r\n".repeat(4400));
                t.set_view(4321);
                t.process(b"x\r\n");
                let (_, full) = h.frame(&mut t, vec![]);
                let (pill, text) = assert_pill_on_grid(&h, &t, &full, &what);
                assert_eq!(pill.min.y, h.grid(&t).min.y, "{what}");
                let wide = "↑ 4.322 linhas  ·  saída nova  ·  Shift+End volta ao fim";
                match cols {
                    60.. => assert_eq!(text, wide, "{what}"),
                    20..=40 => assert_eq!(text, "↑ 4.322 · novo", "{what}"),
                    ..=12 => assert_eq!(text, "↑ 4.322", "{what}"),
                    _ => {}
                }
                // No topo ele vai para baixo: a primeira linha (em geral o
                // comando) nao tem como descer para aparecer.
                t.set_view(usize::MAX);
                let (_, full) = h.frame(&mut t, vec![]);
                let (pill, _) = assert_pill_on_grid(&h, &t, &full, &what);
                assert_eq!(pill.max.y, h.grid(&t).max.y, "{what}");
            }
        }
    }

    #[test]
    fn position_bar_beside_the_grid() {
        // A barra vai na sobra a direita da grade (menos de uma celula) e nao
        // cobre a ultima coluna; sem sobra, 2 pontos colados na borda.
        let bar_color = Color32::from_rgba_unmultiplied(0x9b, 0xa3, 0xb4, 0x99);
        for (spare, inside) in [(6.0, false), (3.0, false), (0.5, true)] {
            let mut t = Terminal::with_scrollback(30, 6, 100);
            let mut h = Harness::with(&mut t, 1.0, spare);
            t.process(&numbered(1, 60));
            t.set_view(10);
            let (_, full) = h.frame(&mut t, vec![]);
            let bar = full
                .shapes
                .iter()
                .find_map(|c| match &c.shape {
                    egui::Shape::Rect(r) if r.fill == bar_color => Some(r.rect),
                    _ => None,
                })
                .expect("barra");
            let grid = h.grid(&t);
            if inside {
                assert_eq!((bar.min.x, bar.max.x), (grid.max.x - 2.0, grid.max.x));
            } else {
                assert!(
                    bar.min.x >= grid.max.x && bar.max.x <= grid.max.x + spare,
                    "sobra {spare}: barra {bar:?}, grade {grid:?}"
                );
            }
        }
    }

    #[test]
    fn cursor_moves_with_the_view() {
        let mut t = Terminal::with_scrollback(20, 5, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 30));
        t.process(b"$ ");
        let cursor = Color32::from_rgba_unmultiplied(0xcc, 0xcc, 0xcc, 0x88);
        let cursor_y = |full: &egui::FullOutput| -> Vec<f32> {
            full.shapes
                .iter()
                .filter_map(|c| match &c.shape {
                    egui::Shape::Rect(r) if r.fill == cursor => Some(r.rect.min.y),
                    _ => None,
                })
                .collect()
        };
        let (_, full) = h.frame(&mut t, vec![]);
        assert_eq!(cursor_y(&full), vec![8.0 + 4.0 * h.cell.1]);
        // Duas linhas acima o cursor (na ultima fileira da tela) sai da visao.
        t.set_view(2);
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(cursor_y(&full).is_empty());
        // Na segunda fileira da tela, ele desce duas junto com o texto.
        t.set_view(0);
        t.process(b"\x1b[2;1H");
        t.set_view(2);
        let (_, full) = h.frame(&mut t, vec![]);
        assert_eq!(cursor_y(&full), vec![8.0 + 3.0 * h.cell.1]);
    }

    #[test]
    fn dragging_past_the_edge_scrolls_by_time() {
        let press = |pos, pressed| egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: NO_MODS,
        };
        let mut t = Terminal::with_scrollback(20, 5, 1000);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 600));
        let a = h.at(2, 3);
        h.frame(&mut t, vec![egui::Event::PointerMoved(a), press(a, true)]);
        let m = h.at(1, 0);
        h.frame(&mut t, vec![egui::Event::PointerMoved(m)]);
        assert_eq!(t.view, 0);
        // Uma celula acima do terminal: uma linha assim que sai da borda...
        let above = Pos2::new(20.0, 1.0);
        h.frame(&mut t, vec![egui::Event::PointerMoved(above)]);
        assert_eq!(t.view, 1);
        // ... e depois 20 linhas por segundo, em 16 quadros ou num so (com o
        // mouse mexendo ou saida chegando os quadros vem mais depressa).
        h.dt = 1.0 / 64.0;
        for _ in 0..16 {
            h.frame(&mut t, vec![]);
        }
        assert_eq!(t.view, 6);
        h.dt = 0.25;
        h.frame(&mut t, vec![]);
        assert_eq!(t.view, 11);
        // Uma pausa longa nao vira um salto.
        h.dt = 5.0;
        h.frame(&mut t, vec![]);
        assert_eq!(t.view, 16);
        // Soltar copia da fileira de cima da visao ate a ancora (L599).
        h.dt = 0.1;
        let (_, full) = h.frame(&mut t, vec![press(above, false)]);
        let mut want = String::from("581");
        for i in 582..=598 {
            want += &format!("\nL{i}");
        }
        want += "\nL599";
        assert_eq!(copied(&full), vec![want]);
        // Abaixo do terminal a visao desce.
        let b = h.at(1, 3);
        h.frame(&mut t, vec![egui::Event::PointerMoved(b), press(b, true)]);
        h.frame(&mut t, vec![egui::Event::PointerMoved(h.at(3, 3))]);
        let below = Pos2::new(20.0, h.size.y - 1.0);
        h.frame(&mut t, vec![egui::Event::PointerMoved(below)]);
        assert_eq!(t.view, 15);
        h.frame(&mut t, vec![press(below, false)]);

        // Na tela alternativa nao ha historico: nada rola.
        let mut t = Terminal::with_scrollback(20, 5, 1000);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        t.process(b"\x1b[?1049h");
        h.frame(&mut t, vec![egui::Event::PointerMoved(a), press(a, true)]);
        h.frame(&mut t, vec![egui::Event::PointerMoved(m)]);
        for _ in 0..5 {
            h.frame(&mut t, vec![egui::Event::PointerMoved(above)]);
        }
        assert_eq!(t.view, 0);
    }

    #[test]
    fn right_click_pastes_and_returns_to_the_bottom() {
        let mut t = Terminal::with_scrollback(30, 6, 100);
        let mut h = Harness::new(&mut t);
        t.process(&numbered(1, 60));
        let p = h.at(2, 2);
        let click = |pressed| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: NO_MODS,
        };
        let mut right_click = |t: &mut Terminal| {
            h.frame(t, vec![egui::Event::PointerMoved(p), click(true)]);
            h.frame(t, vec![click(false)]).0
        };
        CLIPBOARD.with(|c| *c.borrow_mut() = Some("ls -la".into()));
        t.set_view(9);
        assert_eq!(right_click(&mut t), b"ls -la");
        assert_eq!(t.view, 0);
        // Area de transferencia vazia: nada vai e a visao fica.
        CLIPBOARD.with(|c| *c.borrow_mut() = Some(String::new()));
        t.set_view(9);
        assert!(right_click(&mut t).is_empty());
        assert_eq!(t.view, 9);
    }

    /// Saida do tmux 2.7 ao sair do shell (ainda na tela alternativa: o eco
    /// do "exit") e voltar a principal.
    const TMUX_EXIT: &[u8] = b"exit\r\nexit\r\n\x1b[1;24r\x1b(B\x1b[m\x1b[?1l\x1b>\x1b[H\x1b[2J\
        \x1b]112\x07\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1005l\
        \x1b[?1049l\x1b[23;0;0t[exited]\r\n";

    /// Sessao real do bash 4.4 (PS1="$ ", 80x24) capturada por pty: o boot,
    /// "seq 1 3000", "clear", "seq 1 40" e Ctrl+L, cada um com o eco do que
    /// foi digitado e o prompt seguinte. As saidas dos `seq` sao remontadas
    /// aqui (identicas byte a byte as capturadas, 16.907 bytes a primeira).
    fn bash_session() -> [Vec<u8>; 5] {
        let seq = |n: usize| {
            let mut v = format!("seq 1 {n}\r\n").into_bytes();
            v.extend((1..=n).flat_map(|i| format!("{i}\r\n").into_bytes()));
            v.extend_from_slice(b"$ ");
            v
        };
        [
            b"$ ".to_vec(),
            seq(3000),
            b"clear\r\n\x1b[3J\x1b[H\x1b[2J$ ".to_vec(),
            seq(40),
            b"\x1b[H\x1b[2J$ ".to_vec(),
        ]
    }

    #[test]
    fn real_bash_session_replay() {
        let [boot, seq3000, clear, seq40, ctrl_l] = bash_session();
        assert_eq!(seq3000.len(), 16_907);
        // Pedacos de qualquer tamanho (um pacote SSH tem ate 32 KiB): a
        // mesma contagem.
        for size in [usize::MAX, 4096, 1000, 37, 1] {
            let feed = |t: &mut Terminal, bytes: &[u8]| {
                for chunk in bytes.chunks(size.min(bytes.len()).max(1)) {
                    t.process(chunk);
                }
            };
            let mut t = Terminal::with_scrollback(80, 24, SCROLLBACK_LINES);
            feed(&mut t, &boot);
            feed(&mut t, &seq3000);
            // "$ seq 1 3000", 3000 numeros e o prompt: 3002 linhas, 24 na tela.
            assert_eq!((t.avail(), t.pushed), (2978, 2978), "pedacos de {size}");
            t.set_view(usize::MAX);
            assert_eq!(view_row(&t, 0), "$ seq 1 3000");
            assert_eq!(view_row(&t, 1), "1");
            // Lendo no meio: o `clear` apaga o historico e volta ao fim.
            t.set_view(100);
            feed(&mut t, &clear);
            assert_eq!(
                (t.avail(), t.view, t.unseen),
                (0, 0, false),
                "pedacos de {size}"
            );
            assert_eq!(view_row(&t, 0), "$");
            feed(&mut t, &seq40);
            assert_eq!(t.avail(), 42 - 24, "pedacos de {size}");
            t.set_view(usize::MAX);
            assert_eq!(view_row(&t, 0), "$ seq 1 40");
            // Ctrl+L so limpa a tela: o historico e a visao ficam.
            feed(&mut t, &ctrl_l);
            assert_eq!((t.avail(), t.view), (18, 18), "pedacos de {size}");
            assert_eq!(view_row(&t, 0), "$ seq 1 40");
            assert!(t.unseen);
            t.scroll_to_bottom();
            assert_eq!(view_row(&t, 0), "$");
        }
    }

    #[test]
    fn real_full_screen_programs() {
        let modes = |t: &Terminal| {
            let s = t.parser.screen();
            (
                s.alternate_screen(),
                s.application_cursor(),
                s.mouse_protocol_mode(),
                s.mouse_protocol_encoding(),
            )
        };
        use vt100::{MouseProtocolEncoding as Enc, MouseProtocolMode as Mode};
        let [boot, seq3000, ..] = bash_session();
        // tmux (com e sem mouse) por cima de um historico: a tela alternativa
        // nao mexe nele, e na saida o historico volta intacto.
        for (attach, mode, enc) in [
            (TMUX_MOUSE_ATTACH, Mode::ButtonMotion, Enc::Sgr),
            (TMUX_PLAIN_ATTACH, Mode::None, Enc::Default),
        ] {
            let mut t = Terminal::with_scrollback(80, 24, SCROLLBACK_LINES);
            t.process(&boot);
            t.process(&seq3000);
            t.set_view(50);
            t.process(b"tmux\r\n");
            assert_eq!(t.view, 51);
            t.process(attach);
            assert_eq!(modes(&t), (true, true, mode, enc));
            assert_eq!((t.view, t.hist), (0, 2979));
            assert_eq!(view_row(&t, 0), "bash-4.4$");
            t.process(TMUX_EXIT);
            assert_eq!(modes(&t), (false, false, Mode::None, Enc::Default));
            // "[exited]" e o prompt do bash de fora continuam na principal.
            assert_eq!((t.hist, t.hidden), (2980, 0));
            t.set_view(usize::MAX);
            assert_eq!(view_row(&t, 0), "$ seq 1 3000");
        }
        let mut t = Terminal::with_scrollback(80, 24, SCROLLBACK_LINES);
        t.process(HTOP_START);
        assert_eq!(modes(&t), (true, true, Mode::PressRelease, Enc::Sgr));
        let mut t = Terminal::with_scrollback(80, 24, SCROLLBACK_LINES);
        t.process(LESS_START);
        assert_eq!(modes(&t), (true, true, Mode::None, Enc::Default));
    }

    #[test]
    fn wheel_in_real_htop_and_less() {
        // htop pediu mouse (SGR): a roda vai para ele, um evento por clique.
        let mut t = Terminal::with_scrollback(80, 24, 100);
        let mut h = Harness::new(&mut t);
        t.process(HTOP_START);
        let p = h.at(9, 19);
        assert_eq!(h.wheel(&mut t, p, LINE, -1.0, NO_MODS), b"\x1b[<65;20;10M");
        // less sem mouse: nada vai ao programa (ele leria o evento como
        // comandos); aparece a dica.
        let mut t = Terminal::with_scrollback(80, 24, 100);
        let mut h = Harness::new(&mut t);
        t.process(LESS_START);
        assert!(h.wheel(&mut t, p, LINE, -1.0, NO_MODS).is_empty());
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(texts(&full)
            .iter()
            .any(|s| s.contains("less/man: PgUp/PgDn")));
        // Saindo da tela alternativa, a dica (que so vale para ela) some.
        t.process(b"\x1b[?1049l");
        let (_, full) = h.frame(&mut t, vec![]);
        assert!(!texts(&full).iter().any(|s| s.contains("less/man")));
    }

    // --- Ponta a ponta contra o sshd descartavel (ignorado) ----------------
    //
    // Mesmas variaveis dos outros e2e: SAGU_E2E_PORT (2222), SAGU_E2E_USER e
    // SAGU_E2E_KEY. A chave do sshd descartavel e aceita (TOFU).
    // Rodar com: cargo test e2e_btop -- --ignored

    /// O servidor manda as sequencias do btop (HVP, CSI s/u, braille) pelo
    /// shell de verdade; a grade do terminal tem de sair no lugar.
    #[test]
    #[ignore]
    fn e2e_btop_sequences_over_ssh() {
        use crate::hostkey::HostKeyAnswer;
        use crate::ssh::{self, SshToUi};
        use crate::vault::{AuthMethod, Host};
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};

        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"));
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = env("SAGU_E2E_PORT")
            .parse()
            .expect("SAGU_E2E_PORT invalida");
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        // Termina com o cursor na linha 8, longe das marcas, para o prompt.
        const CMD: &[u8] = b"printf '\\033[2J\\033[5;10fX\\033[s\\033[1;1fY\\033[uZ\\033[3;3f\\342\\243\\277\\033[8;1f'\n";

        let h = ssh::connect(host, 80, 24, false, || {});
        let mut term = Terminal::new(80, 24);
        let mut raw = Vec::new();
        let marks = |t: &Terminal| {
            (
                row_text(t, 0, 0, 1),
                row_text(t, 4, 9, 11),
                row_text(t, 2, 2, 3),
            )
        };
        let want = ("Y".to_string(), "XZ".to_string(), "⣿".to_string());
        let t0 = Instant::now();
        while marks(&term) != want {
            assert!(
                t0.elapsed() < Duration::from_secs(30),
                "tempo esgotado; tela:\n{}\nbytes: {:?}",
                term.parser.screen().contents(),
                String::from_utf8_lossy(&raw)
            );
            match h.from_ssh.recv_timeout(Duration::from_millis(100)) {
                Ok(SshToUi::HostKey(p)) => {
                    let _ = p.reply.send(HostKeyAnswer::Accept);
                }
                Ok(SshToUi::Connected) => h.send_data(CMD.to_vec()),
                Ok(SshToUi::Data(d)) => {
                    raw.extend_from_slice(&d);
                    term.process(&d);
                }
                Ok(SshToUi::Error(e)) => panic!("erro na sessao: {e}"),
                Ok(SshToUi::Closed) => panic!("a sessao fechou"),
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(e) => panic!("{e}"),
            }
        }
        // O servidor mandou mesmo as sequencias (nao so o eco do comando).
        let has = |needle: &[u8]| raw.windows(needle.len()).any(|w| w == needle);
        assert!(has(b"\x1b[5;10fX\x1b[s\x1b[1;1fY\x1b[uZ"), "{raw:?}");
        assert!(has("\x1b[3;3f⣿".as_bytes()), "{raw:?}");

        // Sem a traducao o vt100 nao poe nada no lugar.
        let mut plain = vt100::Parser::new(24, 80, 0);
        plain.process(&raw);
        assert_ne!(plain.screen().cell(4, 9).unwrap().contents(), "X");

        // Os mesmos bytes, um a um, dao a mesma tela.
        let mut replay = Terminal::new(80, 24);
        for b in &raw {
            replay.process(std::slice::from_ref(b));
        }
        assert_eq!(
            replay.parser.screen().contents_formatted(),
            term.parser.screen().contents_formatted()
        );

        h.disconnect();
        let t0 = Instant::now();
        loop {
            assert!(t0.elapsed() < Duration::from_secs(10), "sem Closed");
            match h.from_ssh.recv_timeout(Duration::from_millis(100)) {
                Ok(SshToUi::Closed) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(SshToUi::Error(e)) => panic!("erro ao encerrar: {e}"),
                _ => {}
            }
        }
    }
}
