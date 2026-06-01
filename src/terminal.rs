//! Emulador de terminal: parser VT100 + renderizacao em egui.
//!
//! Caracteristicas:
//! - desenha a grade de celulas com fonte monoespacada;
//! - encaminha teclado (incluindo setas, teclas de funcao e Ctrl+letra);
//! - **copia automaticamente** o texto selecionado para a area de transferencia
//!   ao soltar o botao do mouse;
//! - redimensiona o PTY conforme a area disponivel.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Vec2};

/// Resultado de um quadro do terminal: o que precisa ser enviado ao servidor.
#[derive(Default)]
pub struct TerminalOutput {
    /// Bytes digitados/colados para enviar ao canal SSH.
    pub input: Vec<u8>,
    /// Novo tamanho (cols, rows) caso a area tenha mudado.
    pub resize: Option<(u16, u16)>,
}

pub struct Terminal {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    font_size: f32,
    sel_anchor: Option<(u16, u16)>,
    sel_head: Option<(u16, u16)>,
    want_focus: bool,
}

impl Terminal {
    pub fn new(cols: u16, rows: u16) -> Self {
        Terminal {
            parser: vt100::Parser::new(rows, cols, 0),
            cols,
            rows,
            font_size: 14.0,
            sel_anchor: None,
            sel_head: None,
            want_focus: true,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Desenha o terminal e processa entrada. Retorna o que enviar ao servidor.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> TerminalOutput {
        let mut out = TerminalOutput::default();

        let font_id = FontId::monospace(self.font_size);
        let (cell_w, cell_h) = ui.fonts(|f| {
            (
                f.glyph_width(&font_id, 'M').max(1.0),
                f.row_height(&font_id).max(1.0),
            )
        });

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

        // Fundo geral do terminal.
        painter.rect_filled(rect, CornerRadius::ZERO, Color32::from_rgb(0x0c, 0x0c, 0x0c));

        // Foco do teclado.
        if self.want_focus {
            response.request_focus();
            self.want_focus = false;
        }
        if response.clicked() {
            response.request_focus();
        }
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

        // --- Selecao com auto-copia ---
        let pos_to_cell = |p: Pos2| -> (u16, u16) {
            let c = (((p.x - rect.min.x) / cell_w).floor() as i32).clamp(0, self.cols as i32 - 1);
            let r = (((p.y - rect.min.y) / cell_h).floor() as i32).clamp(0, self.rows as i32 - 1);
            (r as u16, c as u16)
        };

        if response.drag_started() {
            if let Some(p) = response.interact_pointer_pos() {
                self.sel_anchor = Some(pos_to_cell(p));
                self.sel_head = self.sel_anchor;
            }
        }
        if response.dragged() {
            if let Some(p) = response.interact_pointer_pos() {
                self.sel_head = Some(pos_to_cell(p));
            }
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

        // Botao direito cola o conteudo da area de transferencia (somente texto).
        if response.secondary_clicked() {
            if let Ok(mut clipboard) = arboard::Clipboard::new() {
                if let Ok(text) = clipboard.get_text() {
                    if !text.is_empty() {
                        out.input.extend_from_slice(text.as_bytes());
                    }
                }
            }
        }

        // --- Renderizacao da grade ---
        self.paint_grid(&painter, rect, &font_id, cell_w, cell_h);

        // --- Entrada de teclado (somente com foco) ---
        if response.has_focus() {
            let events = ui.input(|i| i.events.clone());
            for ev in events {
                match ev {
                    egui::Event::Text(t) => {
                        for ch in t.chars().filter(|c| !c.is_control()) {
                            let mut buf = [0u8; 4];
                            out.input.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                        }
                    }
                    egui::Event::Paste(t) => out.input.extend_from_slice(t.as_bytes()),
                    egui::Event::Copy => out.input.push(0x03), // Ctrl+C = interrupcao
                    egui::Event::Cut => out.input.push(0x18), // Ctrl+X
                    egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if let Some(bytes) = map_key(key, &modifiers) {
                            out.input.extend_from_slice(&bytes);
                        }
                    }
                    _ => {}
                }
            }
        }

        out
    }

    /// Retorna o intervalo de selecao normalizado (inicio <= fim).
    fn selection_range(&self) -> Option<((u16, u16), (u16, u16))> {
        let a = self.sel_anchor?;
        let b = self.sel_head?;
        if (a.0, a.1) <= (b.0, b.1) {
            Some((a, b))
        } else {
            Some((b, a))
        }
    }

    fn selection_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = self.selection_range()?;
        let screen = self.parser.screen();
        let mut lines: Vec<String> = Vec::new();
        for r in r0..=r1 {
            let (start, end) = if r0 == r1 {
                (c0, c1)
            } else if r == r0 {
                (c0, self.cols - 1)
            } else if r == r1 {
                (0, c1)
            } else {
                (0, self.cols - 1)
            };
            let mut line = String::new();
            for c in start..=end {
                if let Some(cell) = screen.cell(r, c) {
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let s = cell.contents();
                    if s.is_empty() {
                        line.push(' ');
                    } else {
                        line.push_str(s);
                    }
                }
            }
            lines.push(line.trim_end().to_string());
        }
        Some(lines.join("\n"))
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

        for row in 0..self.rows {
            let y = rect.min.y + row as f32 * cell_h;

            // Agrupa celulas contiguas com mesmo estilo para reduzir draw calls.
            let mut col = 0u16;
            while col < self.cols {
                let cell = match screen.cell(row, col) {
                    Some(c) => c,
                    None => {
                        col += 1;
                        continue;
                    }
                };
                if cell.is_wide_continuation() {
                    col += 1;
                    continue;
                }

                let (fg, bg) = resolve_colors(cell);
                let span = if cell.is_wide() { 2 } else { 1 };

                // Fundo da celula.
                if bg != Color32::from_rgb(0x0c, 0x0c, 0x0c) {
                    let bg_rect = Rect::from_min_size(
                        Pos2::new(rect.min.x + col as f32 * cell_w, y),
                        Vec2::new(cell_w * span as f32, cell_h),
                    );
                    painter.rect_filled(bg_rect, CornerRadius::ZERO, bg);
                }

                // Glifo.
                let s = cell.contents();
                if !s.is_empty() {
                    painter.text(
                        Pos2::new(rect.min.x + col as f32 * cell_w, y),
                        Align2::LEFT_TOP,
                        s,
                        font_id.clone(),
                        fg,
                    );
                }

                col += span;
            }
        }

        // Selecao: overlay translucido.
        if let Some(((r0, c0), (r1, c1))) = sel {
            let overlay = Color32::from_rgba_unmultiplied(0x33, 0x99, 0xff, 0x55);
            for r in r0..=r1 {
                let (start, end) = if r0 == r1 {
                    (c0, c1)
                } else if r == r0 {
                    (c0, self.cols - 1)
                } else if r == r1 {
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

        // Cursor.
        if !screen.hide_cursor() {
            let (cr, cc) = screen.cursor_position();
            if cr < self.rows && cc < self.cols {
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

fn resolve_colors(cell: &vt100::Cell) -> (Color32, Color32) {
    let default_fg = Color32::from_rgb(0xcc, 0xcc, 0xcc);
    let default_bg = Color32::from_rgb(0x0c, 0x0c, 0x0c);

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
fn map_key(key: egui::Key, mods: &egui::Modifiers) -> Option<Vec<u8>> {
    use egui::Key;
    let bytes: Vec<u8> = match key {
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        Key::Tab => vec![b'\t'],
        Key::Escape => vec![0x1b],
        Key::ArrowUp => b"\x1b[A".to_vec(),
        Key::ArrowDown => b"\x1b[B".to_vec(),
        Key::ArrowRight => b"\x1b[C".to_vec(),
        Key::ArrowLeft => b"\x1b[D".to_vec(),
        Key::Home => b"\x1b[H".to_vec(),
        Key::End => b"\x1b[F".to_vec(),
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
