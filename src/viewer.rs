//! Visualizador somente leitura de arquivos remotos (navegador SFTP).
//!
//! Aqui nao ha UI (padrao do `download.rs`): a tarefa [`load`] le o arquivo
//! pelo SFTP com limite de tamanho, prazo e cancelamento, recusa o que nao e
//! arquivo comum (pasta, fifo, dispositivo, socket) e os binarios, e decodifica
//! e indexa as linhas fora da thread da UI. As funcoes puras (binario,
//! decodificacao, linhas, exibicao segura, copia e busca) sao testadas sem
//! rede. Nada e gravado em disco: o texto vive so na memoria do painel.
//!
//! Garantias da exibicao: controles, caracteres de direcao (bidi) e
//! invisiveis nunca chegam crus a tela nem a area de transferencia; aparecem
//! na mesma notacao visivel dos nomes remotos (`download::char_notation`).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use russh_sftp::client::error::Error;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::StatusCode;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::download::{self, char_notation, clip, is_bidi, is_invisible};
use crate::sftp::{self, RawKind};

/// Maior trecho do arquivo mostrado (o resto fica para o download).
pub const MAX_VIEW_BYTES: usize = 4 * 1024 * 1024;
/// Amostra do inicio do arquivo usada para decidir se e binario.
pub const SNIFF_BYTES: usize = 8 * 1024;
/// Bloco pedido por leitura (o OpenSSH devolve menos por vez).
const CHUNK: usize = 256 * 1024;
/// Prazo do pedido inteiro (cada pedido SFTP ainda tem os 10 s do crate).
const VIEW_DEADLINE: Duration = Duration::from_secs(60);
/// Intervalo minimo entre eventos de andamento.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Alvo de link mais longo guardado (texto vindo do servidor).
const MAX_TARGET: usize = 300;
/// Maior trecho de uma mensagem de erro vinda do servidor.
const MAX_MSG: usize = 300;
/// Parada de tabulacao (o `cat` usa 8; 4 cabe melhor no painel).
pub const TAB_WIDTH: u32 = 4;
/// Colunas exibidas antes de quebrar visualmente uma linha longa.
pub const WRAP_COLS: u32 = 1000;
/// Linhas exibidas no maximo (acima disso o texto e cortado).
pub const MAX_ROWS: usize = 200_000;
/// Ocorrencias da busca guardadas no maximo ("10000+ resultados").
pub const MAX_MATCHES: usize = 10_000;

/// So nos testes: pausa depois de cada bloco lido deste caminho, para um
/// cancelamento cair com certeza no meio da leitura (e2e_view_cancel; num
/// servidor local os 4 MB chegariam antes do primeiro andamento).
#[cfg(test)]
static SLOW_READ: std::sync::Mutex<Option<(String, Duration)>> = std::sync::Mutex::new(None);

/// Eventos de uma leitura, repassados a UI pelo loop da sessao SFTP.
pub enum ViewEvent {
    /// Andamento (no maximo a cada 100 ms).
    Progress { id: u64, got: u64, total: Option<u64> },
    /// Um por pedido, exceto se cancelado.
    Done {
        id: u64,
        result: Result<Box<ViewDoc>, ViewError>,
    },
}

/// Por que o arquivo nao foi aberto.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewError {
    /// E uma pasta (ou link para pasta): a UI entra nela.
    IsDir,
    /// Fifo, socket ou dispositivo (a leitura poderia travar a sessao).
    Special,
    /// O servidor nao informou o tipo.
    UnknownType,
    Binary,
    /// Link cujo destino nao existe (ou em ciclo: o OpenSSH responde igual).
    BrokenLink,
    /// Link que nao deu para seguir (mensagem do servidor).
    BadLink(String),
    Denied,
    NotFound,
    Timeout,
    /// Outra falha do servidor (mensagem cortada).
    Remote(String),
    /// Panico na tarefa ou na montagem do documento.
    Internal,
    /// Interface do kernel cuja leitura bloqueia ou consome os dados (ex.:
    /// /proc/kmsg, que como root tiraria as mensagens do log do sistema).
    Draining,
}

/// Codificacao usada para decodificar o arquivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Ascii,
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Windows1252,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Ascii => "ASCII",
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 com BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Windows1252 => "Windows-1252 (Latin-1)",
        }
    }
}

/// Fim de linha predominante.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eol {
    /// Uma linha so, sem terminador.
    None,
    Lf,
    CrLf,
    Cr,
    /// LF e CRLF no mesmo arquivo.
    Mixed,
}

impl Eol {
    pub fn label(self) -> Option<&'static str> {
        match self {
            Eol::None => None,
            Eol::Lf => Some("LF"),
            Eol::CrLf => Some("CRLF"),
            Eol::Cr => Some("CR"),
            Eol::Mixed => Some("misto"),
        }
    }
}

/// Por que so parte do arquivo aparece.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trunc {
    /// Maior que `MAX_VIEW_BYTES`: `total` e o tamanho informado pelo servidor.
    Bytes { shown: u64, total: Option<u64> },
    /// Mais que `MAX_ROWS` linhas exibidas.
    Rows { lines: u32 },
    /// O prazo acabou no meio da leitura.
    Deadline { shown: u64 },
}

/// Uma linha exibida: trecho `start..end` (bytes de `ViewDoc::text`, sem o
/// terminador). `line` e o numero da linha (1, 2...) ou 0 na continuacao de
/// uma linha longa quebrada.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub start: u32,
    pub end: u32,
    pub line: u32,
}

/// Arquivo pronto para exibir (vai para a UI numa `Box`).
#[derive(Debug)]
pub struct ViewDoc {
    /// Caminho pedido (logico: um link fica no caminho).
    pub path: String,
    /// Alvo do link, quando o caminho e um link.
    pub target: Option<String>,
    /// Tamanho informado pelo servidor (o /proc informa 0).
    pub size: Option<u64>,
    pub mtime: Option<u32>,
    pub text: String,
    pub rows: Vec<Row>,
    /// Linhas logicas (sem contar as continuacoes).
    pub lines: u32,
    /// Maior largura exibida, em colunas.
    pub max_cols: u32,
    pub encoding: Encoding,
    /// Bytes invalidos trocados por U+FFFD.
    pub lossy: bool,
    pub eol: Eol,
    pub truncated: Option<Trunc>,
    pub has_bidi: bool,
    /// Controles (fora o TAB), C1 ou invisiveis no texto.
    pub has_controls: bool,
}

// --- Binario -------------------------------------------------------------

/// Resultado da amostra do inicio do arquivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sniff {
    Text,
    Binary,
}

/// Controle que nao aparece em texto comum. Ficam de fora BEL, BS, TAB, LF,
/// VT, FF, CR e ESC (logs com cores, paginas do man).
fn is_suspect(c: u32) -> bool {
    matches!(c, 0x01..=0x06 | 0x0E..=0x1A | 0x1C..=0x1F | 0x7F)
}

/// Binario pelos primeiros `SNIFF_BYTES`: BOM de UTF-32, NUL ou mais de 10%
/// de controles suspeitos. Com BOM de UTF-16 a regra vale para as unidades
/// decodificadas. Vazio e texto.
pub fn sniff(bytes: &[u8]) -> Sniff {
    let s = &bytes[..bytes.len().min(SNIFF_BYTES)];
    // UTF-32 antes do UTF-16 (FF FE 00 00 comeca com o BOM de UTF-16 LE).
    if s.starts_with(&[0xFF, 0xFE, 0, 0]) || s.starts_with(&[0, 0, 0xFE, 0xFF]) {
        return Sniff::Binary;
    }
    if let Some(le) = utf16_bom(s) {
        let (mut n, mut bad) = (0usize, 0usize);
        for u in utf16_units(&s[2..], le) {
            if u == 0 {
                return Sniff::Binary;
            }
            n += 1;
            bad += usize::from(is_suspect(u.into()));
        }
        return if bad * 10 > n { Sniff::Binary } else { Sniff::Text };
    }
    let mut bad = 0usize;
    for &b in s {
        if b == 0 {
            return Sniff::Binary;
        }
        bad += usize::from(is_suspect(b.into()));
    }
    if bad * 10 > s.len() {
        Sniff::Binary
    } else {
        Sniff::Text
    }
}

/// `Some(true)` com BOM de UTF-16 LE, `Some(false)` com o de BE.
fn utf16_bom(b: &[u8]) -> Option<bool> {
    if b.starts_with(&[0xFF, 0xFE]) {
        Some(true)
    } else if b.starts_with(&[0xFE, 0xFF]) {
        Some(false)
    } else {
        None
    }
}

/// Unidades UTF-16 (o byte impar do fim e descartado).
fn utf16_units(b: &[u8], le: bool) -> impl Iterator<Item = u16> + '_ {
    b.chunks_exact(2).map(move |p| {
        if le {
            u16::from_le_bytes([p[0], p[1]])
        } else {
            u16::from_be_bytes([p[0], p[1]])
        }
    })
}

// --- Decodificacao ---------------------------------------------------------

/// Windows-1252 de 0x80 a 0x9F; os cinco sem caractere viram o proprio
/// codigo (C1), como no WHATWG.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

fn cp1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x80..=0x9F => CP1252_HIGH[usize::from(b - 0x80)],
            _ => char::from(b),
        })
        .collect()
}

/// Tira do fim uma sequencia UTF-8 incompleta (o corte da leitura caiu no
/// meio de um caractere).
fn strip_incomplete_utf8(bytes: &[u8]) -> &[u8] {
    let n = bytes.len();
    for k in 1..=n.min(3) {
        let b = bytes[n - k];
        if b & 0xC0 == 0x80 {
            continue;
        }
        let need = match b {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        return if need > k { &bytes[..n - k] } else { bytes };
    }
    bytes
}

/// Texto do arquivo, a codificacao usada e se houve bytes invalidos (trocados
/// por U+FFFD). `truncated`: a leitura foi cortada (um caractere pela metade
/// no fim e descartado). Sem BOM: UTF-8 se valido (ASCII sem nenhum byte alto)
/// ou quase todo valido; senao Windows-1252 (arquivos antigos em portugues).
pub fn decode(bytes: &[u8], truncated: bool) -> (String, Encoding, bool) {
    if let Some(body) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let body = if truncated { strip_incomplete_utf8(body) } else { body };
        let s = String::from_utf8_lossy(body);
        let lossy = matches!(s, std::borrow::Cow::Owned(_));
        return (s.into_owned(), Encoding::Utf8Bom, lossy);
    }
    if let Some(le) = utf16_bom(bytes) {
        let mut units: Vec<u16> = utf16_units(&bytes[2..], le).collect();
        if truncated && units.last().is_some_and(|u| (0xD800..=0xDBFF).contains(u)) {
            units.pop();
        }
        let mut lossy = false;
        let s = char::decode_utf16(units)
            .map(|r| {
                r.unwrap_or_else(|_| {
                    lossy = true;
                    '\u{FFFD}'
                })
            })
            .collect();
        let enc = if le { Encoding::Utf16Le } else { Encoding::Utf16Be };
        return (s, enc, lossy);
    }
    let body = if truncated { strip_incomplete_utf8(bytes) } else { bytes };
    let (mut invalid, mut multibyte) = (0usize, 0usize);
    for chunk in body.utf8_chunks() {
        multibyte += chunk.valid().chars().filter(|c| c.len_utf8() > 1).count();
        invalid += usize::from(!chunk.invalid().is_empty());
    }
    if invalid == 0 {
        let s = String::from_utf8_lossy(body).into_owned();
        let enc = if multibyte == 0 { Encoding::Ascii } else { Encoding::Utf8 };
        (s, enc, false)
    } else if multibyte > invalid {
        (String::from_utf8_lossy(body).into_owned(), Encoding::Utf8, true)
    } else {
        // Todos os bytes (o corte no fim nao era de UTF-8).
        (cp1252(bytes), Encoding::Windows1252, false)
    }
}

// --- Linhas e exibicao segura --------------------------------------------

/// Classe de um caractere para a exibicao.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cls {
    Plain,
    Tab,
    /// Controle (C0, DEL, C1) ou invisivel: notacao visivel.
    Ctrl,
    /// Direcao de texto: notacao visivel, em destaque de perigo.
    Bidi,
    /// Byte invalido trocado por U+FFFD na decodificacao: nenhuma fonte do
    /// app tem esse glifo (sairia um quadrado igual ao de um caractere sem
    /// fonte), entao aparece como "?" em destaque, como os controles.
    Invalid,
}

fn classify(c: char) -> Cls {
    let n = c as u32;
    match c {
        ' '..='~' => Cls::Plain,
        '\t' => Cls::Tab,
        '\u{FFFD}' => Cls::Invalid,
        _ if is_bidi(c) => Cls::Bidi,
        _ if n < 0x20 || (0x7F..=0x9F).contains(&n) || is_invisible(c) => Cls::Ctrl,
        _ => Cls::Plain,
    }
}

/// Marca exibida no lugar de um byte invalido (U+FFFD).
pub const INVALID_MARK: &str = "?";

/// Largura exibida de `c` a partir da coluna `col` (TAB ate a proxima parada;
/// notacoes com o tamanho delas; o resto 1).
fn char_cols(c: char, col: u32) -> u32 {
    match classify(c) {
        Cls::Plain | Cls::Invalid => 1,
        Cls::Tab => TAB_WIDTH - col % TAB_WIDTH,
        _ => char_notation(c).map_or(1, |n| n.len() as u32),
    }
}

/// Linhas exibidas de um texto (ver `index_rows`).
pub struct Indexed {
    pub rows: Vec<Row>,
    pub lines: u32,
    pub max_cols: u32,
    pub eol: Eol,
    pub has_bidi: bool,
    pub has_controls: bool,
    /// Parou em `MAX_ROWS` (o resto do texto nao tem linha).
    pub capped: bool,
}

/// Quebra o texto em linhas exibidas: `\n` termina a linha (um `\r` logo
/// antes sai junto, CRLF); sem nenhum `\n`, o `\r` e o terminador (CR); outro
/// `\r` fica visivel (^M). O terminador final nao cria linha vazia. Linhas
/// com mais de `WRAP_COLS` colunas continuam na linha seguinte (numero 0),
/// sem partir caractere. No maximo `MAX_ROWS` linhas.
pub fn index_rows(text: &str) -> Indexed {
    let lf = text.contains('\n');
    let cr = !lf && text.contains('\r');
    let mut ix = Indexed {
        rows: Vec::new(),
        lines: 0,
        max_cols: 0,
        eol: Eol::None,
        has_bidi: false,
        has_controls: false,
        capped: false,
    };
    let (mut n_lf, mut n_crlf) = (0u32, 0u32);
    let mut row_start = 0usize;
    let mut cols = 0u32;
    // A proxima linha exibida comeca uma linha logica (tem numero)?
    let mut first = true;
    let push = |ix: &mut Indexed, start: usize, end: usize, first: bool| -> bool {
        if ix.rows.len() >= MAX_ROWS {
            ix.capped = true;
            return false;
        }
        let line = if first {
            ix.lines += 1;
            ix.lines
        } else {
            0
        };
        ix.rows.push(Row {
            start: start as u32,
            end: end as u32,
            line,
        });
        true
    };
    let mut it = text.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let next_start = if lf && c == '\n' {
            n_lf += 1;
            Some(i + 1)
        } else if lf && c == '\r' && it.peek().is_some_and(|&(_, n)| n == '\n') {
            it.next();
            n_crlf += 1;
            Some(i + 2)
        } else if cr && c == '\r' {
            Some(i + 1)
        } else {
            None
        };
        if let Some(next) = next_start {
            if !push(&mut ix, row_start, i, first) {
                break;
            }
            first = true;
            row_start = next;
            cols = 0;
            continue;
        }
        let mut w = char_cols(c, cols);
        if cols > 0 && cols + w > WRAP_COLS {
            if !push(&mut ix, row_start, i, first) {
                break;
            }
            first = false;
            row_start = i;
            cols = 0;
            w = char_cols(c, 0);
        }
        cols += w;
        ix.max_cols = ix.max_cols.max(cols);
        match classify(c) {
            Cls::Bidi => ix.has_bidi = true,
            Cls::Ctrl => ix.has_controls = true,
            _ => {}
        }
    }
    if !ix.capped && row_start < text.len() {
        push(&mut ix, row_start, text.len(), first);
    }
    ix.eol = match (n_lf > 0, n_crlf > 0) {
        (true, true) => Eol::Mixed,
        (false, true) => Eol::CrLf,
        (true, false) => Eol::Lf,
        (false, false) if cr => Eol::Cr,
        (false, false) => Eol::None,
    };
    ix
}

/// Monta o documento (roda fora da thread da UI: `spawn_blocking`).
pub fn build_doc(
    path: String,
    target: Option<String>,
    size: Option<u64>,
    mtime: Option<u32>,
    bytes: Vec<u8>,
    trunc: Option<Trunc>,
) -> ViewDoc {
    let (mut text, encoding, lossy) = decode(&bytes, trunc.is_some());
    drop(bytes);
    let ix = index_rows(&text);
    let mut truncated = trunc;
    if ix.capped {
        // Busca e copia so no que aparece.
        text.truncate(ix.rows.last().map_or(0, |r| r.end as usize));
        text.shrink_to_fit();
        truncated = Some(Trunc::Rows { lines: ix.lines });
    }
    ViewDoc {
        path,
        target,
        size,
        mtime,
        text,
        rows: ix.rows,
        lines: ix.lines,
        max_cols: ix.max_cols,
        encoding,
        lossy,
        eol: ix.eol,
        truncated,
        has_bidi: ix.has_bidi,
        has_controls: ix.has_controls,
    }
}

/// Cores e fonte de uma linha exibida (a UI define).
pub struct RowStyle {
    pub font: egui::FontId,
    pub normal: egui::Color32,
    /// Notacao de controle e invisivel: (texto, fundo).
    pub ctrl: (egui::Color32, egui::Color32),
    /// Notacao de direcao de texto: (texto, fundo).
    pub bidi: (egui::Color32, egui::Color32),
}

/// Uma linha pronta para a tela: o texto exibido (TAB em espacos, controles
/// e bidi em notacao visivel) e, para cada caractere exibido, o byte de
/// origem em `ViewDoc::text` (mais um no fim, o `end` da linha).
pub struct RowView {
    pub job: egui::text::LayoutJob,
    pub map: Vec<u32>,
}

impl RowView {
    /// So para as linhas visiveis (e para o acerto do mouse).
    pub fn build(text: &str, row: Row, style: &RowStyle) -> RowView {
        let mut job = egui::text::LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        let normal = egui::TextFormat::simple(style.font.clone(), style.normal);
        let marked = |(fg, bg): (egui::Color32, egui::Color32)| egui::TextFormat {
            font_id: style.font.clone(),
            color: fg,
            background: bg,
            ..Default::default()
        };
        let (s, e) = (row.start as usize, row.end as usize);
        let mut map = Vec::with_capacity(e - s + 1);
        let mut plain = String::new();
        let mut col = 0u32;
        for (i, c) in text[s..e].char_indices() {
            let at = (s + i) as u32;
            match classify(c) {
                Cls::Plain => {
                    plain.push(c);
                    map.push(at);
                    col += 1;
                }
                Cls::Tab => {
                    let w = TAB_WIDTH - col % TAB_WIDTH;
                    for _ in 0..w {
                        plain.push(' ');
                        map.push(at);
                    }
                    col += w;
                }
                Cls::Invalid => {
                    if !plain.is_empty() {
                        job.append(&std::mem::take(&mut plain), 0.0, normal.clone());
                    }
                    map.push(at);
                    col += 1;
                    job.append(INVALID_MARK, 0.0, marked(style.ctrl));
                }
                k => {
                    if !plain.is_empty() {
                        job.append(&std::mem::take(&mut plain), 0.0, normal.clone());
                    }
                    let n = char_notation(c).unwrap_or_default();
                    map.extend(std::iter::repeat_n(at, n.len()));
                    col += n.len() as u32;
                    let colors = if k == Cls::Bidi { style.bidi } else { style.ctrl };
                    job.append(&n, 0.0, marked(colors));
                }
            }
        }
        if !plain.is_empty() {
            job.append(&plain, 0.0, normal);
        }
        map.push(row.end);
        RowView { job, map }
    }
}

// --- Copia, busca e selecao ------------------------------------------------

/// Linha exibida que contem o byte (a ultima que comeca nele ou antes).
pub fn row_of(rows: &[Row], byte: u32) -> usize {
    rows.partition_point(|r| r.start <= byte).saturating_sub(1)
}

/// Trecho `a..b` de `text` para a area de transferencia: fim de linha vira
/// `\n` (CRLF e CR inclusive), TAB fica TAB, controles, bidi e invisiveis
/// viram a mesma notacao da tela (nunca vao escondidos); a continuacao de uma
/// linha longa nao ganha `\n`.
pub fn copy_text(doc: &ViewDoc, a: u32, b: u32) -> String {
    let (a, b) = (a.min(b), a.max(b));
    let mut out = String::new();
    if a == b || doc.rows.is_empty() {
        return out;
    }
    let plain = !doc.has_controls && !doc.has_bidi;
    for k in row_of(&doc.rows, a)..doc.rows.len() {
        let row = doc.rows[k];
        let (s, e) = (a.max(row.start) as usize, b.min(row.end) as usize);
        if s < e {
            let piece = &doc.text[s..e];
            if plain {
                out.push_str(piece);
            } else {
                for c in piece.chars() {
                    match classify(c) {
                        Cls::Plain | Cls::Tab | Cls::Invalid => out.push(c),
                        _ => out.push_str(&char_notation(c).unwrap_or_default()),
                    }
                }
            }
        }
        if b <= row.end {
            break;
        }
        if doc.rows.get(k + 1).is_some_and(|n| n.line > 0) {
            out.push('\n');
        }
    }
    out
}

/// Comparacao da busca: sem diferenciar maiusculas (1 para 1 por caractere,
/// para as posicoes continuarem as do texto).
fn fold_char(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_lowercase()
    } else {
        c.to_lowercase().next().unwrap_or(c)
    }
}

/// Ocorrencias de `q` em `text` (bytes `inicio..fim`), sem sobreposicao,
/// sem diferenciar maiusculas e sem atravessar o fim de linha; no maximo
/// `max` (o `bool` diz que havia mais). Consulta vazia nao acha nada.
pub fn find_all(text: &str, q: &str, max: usize) -> (Vec<(u32, u32)>, bool) {
    let needle: Vec<char> = q.chars().map(fold_char).collect();
    let mut out = Vec::new();
    let Some(&head) = needle.first() else {
        return (out, false);
    };
    if needle.iter().any(|&c| c == '\n' || c == '\r') {
        return (out, false);
    }
    let mut pos = 0usize;
    while pos < text.len() {
        let mut hay = text[pos..].char_indices();
        let Some((_, c0)) = hay.next() else {
            break;
        };
        if fold_char(c0) == head {
            let mut end = pos + c0.len_utf8();
            let mut ok = true;
            for &n in &needle[1..] {
                match hay.next() {
                    Some((i, c)) if fold_char(c) == n => end = pos + i + c.len_utf8(),
                    _ => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                if out.len() == max {
                    return (out, true);
                }
                out.push((pos as u32, end as u32));
                pos = end;
                continue;
            }
        }
        pos += c0.len_utf8();
    }
    (out, false)
}

/// Palavra sob o byte (duplo clique): letras, digitos e "_"; fora de uma
/// palavra, so o caractere; no fim de linha, nada.
pub fn word_at(text: &str, byte: u32) -> (u32, u32) {
    let b = (byte as usize).min(text.len());
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let Some(c) = text[b..].chars().next() else {
        return (b as u32, b as u32);
    };
    if c == '\n' || c == '\r' {
        return (b as u32, b as u32);
    }
    if !is_word(c) {
        return (b as u32, (b + c.len_utf8()) as u32);
    }
    let start = text[..b]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| is_word(c))
        .last()
        .map_or(b, |(i, _)| i);
    let end = text[b..]
        .char_indices()
        .find(|&(_, c)| !is_word(c))
        .map_or(text.len(), |(i, _)| b + i);
    (start as u32, end as u32)
}

/// Linha logica inteira (com as continuacoes) que contem o byte (clique triplo).
pub fn line_at(rows: &[Row], byte: u32) -> Option<(u32, u32)> {
    if rows.is_empty() {
        return None;
    }
    let r = row_of(rows, byte);
    let first = (0..=r).rev().find(|&k| rows[k].line > 0).unwrap_or(0);
    let last = (r + 1..rows.len())
        .take_while(|&k| rows[k].line == 0)
        .last()
        .unwrap_or(r);
    Some((rows[first].start, rows[last].end))
}

// --- Tarefa da leitura -----------------------------------------------------

/// Resultado de uma espera da tarefa.
enum Wait<T> {
    Done(T),
    Deadline,
    Cancelled,
}

/// Fim antecipado da tarefa.
enum Stop {
    /// Cancelado (ou o painel fechou): nada e enviado.
    Cancelled,
    Fail(ViewError),
}

/// Garante um `Done` por pedido: se a tarefa cair no meio (panico) sem ter
/// sido cancelada, o `Drop` manda `Internal` (a faixa "Abrindo" nao fica para
/// sempre). Numa tarefa abortada junto com a sessao, nada mais e entregue.
struct DoneGuard<'a> {
    tx: &'a UnboundedSender<ViewEvent>,
    id: u64,
    armed: bool,
}

impl DoneGuard<'_> {
    fn finish(mut self, result: Option<Result<Box<ViewDoc>, ViewError>>) {
        self.armed = false;
        if let Some(result) = result {
            let _ = self.tx.send(ViewEvent::Done {
                id: self.id,
                result,
            });
        }
    }
}

impl Drop for DoneGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.tx.send(ViewEvent::Done {
                id: self.id,
                result: Err(ViewError::Internal),
            });
        }
    }
}

/// Erro ao ler os atributos ou abrir o arquivo.
fn access_error(e: &Error) -> ViewError {
    match e {
        Error::Status(s) if s.status_code == StatusCode::NoSuchFile => ViewError::NotFound,
        Error::Status(s) if s.status_code == StatusCode::PermissionDenied => ViewError::Denied,
        Error::Timeout => ViewError::Timeout,
        other => ViewError::Remote(clip(&other.to_string(), MAX_MSG)),
    }
}

/// Caminho real (sem links) de uma interface do kernel que parece arquivo
/// comum mas cuja leitura bloqueia a espera de dados ou os consome (quem
/// ler rouba do servico que os usa): /proc/kmsg (log do kernel) e os
/// trace_pipe do tracefs. Todos informam tamanho 0.
pub fn is_draining(real: &str) -> bool {
    let name = real.rsplit('/').next().unwrap_or("");
    real == "/proc/kmsg" || (real.starts_with("/sys/kernel/") && name.starts_with("trace_pipe"))
}

/// Uma leitura em andamento.
struct Load<'a> {
    sftp: &'a SftpSession,
    id: u64,
    path: &'a str,
    cancel: watch::Receiver<bool>,
    tx: &'a UnboundedSender<ViewEvent>,
    deadline: Instant,
    /// Uma espera foi interrompida (cancelamento ou prazo) com um pedido em
    /// voo: ele pode ter ficado preso no sftp-server.
    interrupted: bool,
}

impl Load<'_> {
    /// Espera `fut`, a menos que o usuario cancele (ou o painel feche) ou o
    /// prazo do pedido acabe antes.
    async fn wait<T>(&mut self, fut: impl Future<Output = T>) -> Wait<T> {
        let deadline = self.deadline;
        let w = tokio::select! {
            biased;
            r = download::unless_cancelled(&mut self.cancel, fut) => match r {
                Some(v) => Wait::Done(v),
                None => Wait::Cancelled,
            },
            () = tokio::time::sleep_until(deadline) => Wait::Deadline,
        };
        if !matches!(w, Wait::Done(_)) {
            self.interrupted = true;
        }
        w
    }

    /// `wait` em que o prazo estourado e falha (`Timeout`).
    async fn ask<T>(&mut self, fut: impl Future<Output = T>) -> Result<T, Stop> {
        match self.wait(fut).await {
            Wait::Done(v) => Ok(v),
            Wait::Deadline => Err(Stop::Fail(ViewError::Timeout)),
            Wait::Cancelled => Err(Stop::Cancelled),
        }
    }

    async fn run(&mut self) -> Result<Box<ViewDoc>, Stop> {
        let sftp = self.sftp;
        let path = self.path.to_string();
        let fail = |e: ViewError| Err(Stop::Fail(e));
        // a) Atributos do proprio caminho (sem seguir link).
        let lstat = self
            .ask(sftp.symlink_metadata(path.clone()))
            .await?
            .map_err(|e| Stop::Fail(access_error(&e)))?;
        // b) Link: o destino (stat segue a cadeia) e o alvo (opcional).
        let mut target = None;
        let attrs = if sftp::raw_kind(lstat.permissions) == RawKind::Link {
            let (st, tg) = self
                .ask(async { tokio::join!(sftp.metadata(path.clone()), sftp.read_link(path.clone())) })
                .await?;
            target = tg.ok().map(|t| clip(&t, MAX_TARGET));
            match st {
                Ok(a) => a,
                Err(Error::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                    return fail(ViewError::BrokenLink)
                }
                Err(e) => return fail(ViewError::BadLink(clip(&e.to_string(), MAX_MSG))),
            }
        } else {
            lstat
        };
        // c) So arquivo comum. Nunca abrir antes: o open() de um fifo trava o
        // sftp-server (processo unico) ate aparecer quem escreva.
        match sftp::raw_kind(attrs.permissions) {
            RawKind::File => {}
            RawKind::Dir => return fail(ViewError::IsDir),
            RawKind::Fifo | RawKind::Socket | RawKind::CharDev | RawKind::BlockDev => {
                return fail(ViewError::Special)
            }
            RawKind::Link | RawKind::Unknown => return fail(ViewError::UnknownType),
        }
        // Tamanho 0 pode ser uma interface do kernel (/proc, /sys): pelo
        // caminho real (links resolvidos), recusa as que bloqueiam ou
        // consomem os dados ao ler. Custa um pedido so nesse caso.
        if attrs.size.unwrap_or(0) == 0 {
            if let Ok(real) = self.ask(sftp.canonicalize(path.clone())).await? {
                if is_draining(&real) {
                    return fail(ViewError::Draining);
                }
            }
        }
        // d) Abre e confere de novo pelo handle (o tipo pode ter mudado). O
        // fstat vem tarde para um fifo trocado entre o stat e o open (o open
        // ja teria travado) e nada impede um arquivo comum cuja leitura
        // bloqueia (FUSE, NFS parado): por isso a leitura corre num canal
        // SFTP descartavel (`sftp::AuxSftp`), com prazo e cancelamento, e um
        // pedido preso nele nao segura a navegacao.
        let mut file = self
            .ask(sftp.open(path.clone()))
            .await?
            .map_err(|e| Stop::Fail(access_error(&e)))?;
        let mut size = attrs.size;
        if let Ok(fa) = self.ask(file.metadata()).await? {
            if fa.permissions.is_some() && sftp::raw_kind(fa.permissions) != RawKind::File {
                return fail(ViewError::Special);
            }
            if fa.size.is_some() {
                size = fa.size;
            }
        }
        // e) Le ate o fim ou ate passar do limite (o tamanho informado nao
        // vale: o /proc informa 0).
        let max = MAX_VIEW_BYTES;
        let mut data: Vec<u8> = Vec::new();
        let mut sniffed = false;
        let mut timed_out = false;
        let mut last = std::time::Instant::now();
        while data.len() <= max {
            let old = data.len();
            data.resize(old + CHUNK.min(max + 1 - old), 0);
            match self.wait(file.read(&mut data[old..])).await {
                Wait::Cancelled => return Err(Stop::Cancelled),
                Wait::Deadline => {
                    data.truncate(old);
                    timed_out = true;
                    break;
                }
                Wait::Done(Err(e)) => {
                    return fail(ViewError::Remote(clip(&e.to_string(), MAX_MSG)))
                }
                Wait::Done(Ok(n)) => {
                    data.truncate(old + n);
                    if n == 0 {
                        break;
                    }
                }
            }
            if !sniffed && data.len() >= SNIFF_BYTES {
                sniffed = true;
                if sniff(&data) == Sniff::Binary {
                    return fail(ViewError::Binary);
                }
            }
            if last.elapsed() >= PROGRESS_EVERY {
                last = std::time::Instant::now();
                let _ = self.tx.send(ViewEvent::Progress {
                    id: self.id,
                    got: data.len() as u64,
                    total: size,
                });
            }
            #[cfg(test)]
            {
                let pause = SLOW_READ
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .filter(|(p, _)| p == self.path)
                    .map(|(_, d)| *d);
                if let Some(d) = pause {
                    tokio::time::sleep(d).await;
                }
            }
        }
        // Fecha o handle sem esperar a resposta (close_nowait).
        drop(file);
        if !sniffed && sniff(&data) == Sniff::Binary {
            return fail(ViewError::Binary);
        }
        // f) Corte.
        let trunc = if data.len() > max {
            data.truncate(max);
            Some(Trunc::Bytes {
                shown: max as u64,
                total: size,
            })
        } else if timed_out {
            if data.is_empty() {
                return fail(ViewError::Timeout);
            }
            Some(Trunc::Deadline {
                shown: data.len() as u64,
            })
        } else {
            None
        };
        // g) Decodifica e indexa fora da thread da sessao.
        let mtime = attrs.mtime;
        let job =
            tokio::task::spawn_blocking(move || build_doc(path, target, size, mtime, data, trunc));
        match download::unless_cancelled(&mut self.cancel, job).await {
            None => Err(Stop::Cancelled),
            Some(Ok(doc)) => Ok(Box::new(doc)),
            Some(Err(_)) => fail(ViewError::Internal),
        }
    }
}

/// Tarefa da leitura (spawnada pela sessao SFTP, fora do loop de comandos).
/// Termina com um `ViewEvent::Done`, salvo se for cancelada (a UI soltou o
/// `Cancel`) ou abortada junto com a sessao.
///
/// Devolve se o canal ficou livre: falso quando uma espera foi interrompida
/// (cancelamento, prazo) ou o servidor nao respondeu (ou respondeu com um
/// erro de canal): um pedido pode ter ficado preso no sftp-server, e quem
/// chamou descarta o canal (ver `sftp::AuxSftp`).
pub async fn load(
    sftp: Arc<SftpSession>,
    id: u64,
    path: String,
    cancel: watch::Receiver<bool>,
    tx: UnboundedSender<ViewEvent>,
) -> bool {
    let guard = DoneGuard {
        tx: &tx,
        id,
        armed: true,
    };
    let mut job = Load {
        sftp: &sftp,
        id,
        path: &path,
        cancel,
        tx: &tx,
        deadline: Instant::now() + VIEW_DEADLINE,
        interrupted: false,
    };
    let result = job.run().await;
    let interrupted = job.interrupted;
    drop(job);
    let clean = !interrupted
        && match &result {
            Ok(_) | Err(Stop::Cancelled) => true,
            Err(Stop::Fail(e)) => !matches!(
                e,
                ViewError::Timeout | ViewError::Remote(_) | ViewError::BadLink(_) | ViewError::Internal
            ),
        };
    guard.finish(match result {
        Ok(doc) => Some(Ok(doc)),
        Err(Stop::Fail(e)) => Some(Err(e)),
        Err(Stop::Cancelled) => None,
    });
    clean
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_of(text: &str) -> ViewDoc {
        build_doc("/t".into(), None, None, None, text.as_bytes().to_vec(), None)
    }

    fn style() -> RowStyle {
        RowStyle {
            font: egui::FontId::monospace(13.0),
            normal: egui::Color32::WHITE,
            ctrl: (egui::Color32::YELLOW, egui::Color32::DARK_GRAY),
            bidi: (egui::Color32::RED, egui::Color32::BLACK),
        }
    }

    /// Texto exibido de uma linha.
    fn shown(doc: &ViewDoc, r: usize) -> String {
        RowView::build(&doc.text, doc.rows[r], &style()).job.text
    }

    fn lines_of(doc: &ViewDoc) -> Vec<String> {
        (0..doc.rows.len()).map(|r| shown(doc, r)).collect()
    }

    #[test]
    fn sniff_nul_is_binary() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\x01\0";
        let elf = b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0\x02\0>\0";
        let gzip = b"\x1f\x8b\x08\0\0\0\0\0\0\x03\xcbH\xcd\xc9\xc9\x07\0";
        for (name, b) in [("png", &png[..]), ("elf", &elf[..]), ("gzip", &gzip[..])] {
            assert_eq!(sniff(b), Sniff::Binary, "{name}");
        }
        assert_eq!(sniff(b"abc\0def"), Sniff::Binary);
        assert_eq!(sniff(b"abc def\n"), Sniff::Text);
    }

    #[test]
    fn sniff_control_ratio() {
        // 12% de 0x01: binario; 5%: texto.
        let mix = |pct: usize| {
            let mut v = vec![b'a'; 1000];
            for x in v.iter_mut().take(pct * 10) {
                *x = 0x01;
            }
            v
        };
        assert_eq!(sniff(&mix(12)), Sniff::Binary);
        assert_eq!(sniff(&mix(5)), Sniff::Text);
        // Cores ANSI, backspace do man, FF, BEL e VT: texto.
        let ansi = b"\x1b[1;31mERRO\x1b[0m linha\n".repeat(20);
        assert_eq!(sniff(&ansi), Sniff::Text);
        let man = b"N\x08NA\x08AM\x08ME\x08E\n\x0c\x07\x0b fim\n".repeat(10);
        assert_eq!(sniff(&man), Sniff::Text);
        // FIX (separador SOH): recusado, falso positivo documentado.
        let fix = b"8=FIX.4.2\x019=65\x0135=A\x01".repeat(20);
        assert_eq!(sniff(&fix), Sniff::Binary);
    }

    #[test]
    fn sniff_only_first_8k() {
        let mut v = vec![b'x'; 9000];
        v[8999] = 0;
        assert_eq!(sniff(&v), Sniff::Text);
        let doc = build_doc("/t".into(), None, None, None, v, None);
        assert!(doc.has_controls);
        assert!(lines_of(&doc).concat().ends_with("^@"), "NUL depois dos 8 KiB aparece como ^@");
        let mut v = vec![b'x'; 9000];
        v[8000] = 0;
        assert_eq!(sniff(&v), Sniff::Binary);
    }

    #[test]
    fn sniff_boms() {
        assert_eq!(sniff(b"\xff\xfeo\0i\0\n\0"), Sniff::Text, "UTF-16 LE");
        assert_eq!(sniff(b"\xfe\xff\0o\0i\0\n"), Sniff::Text, "UTF-16 BE");
        assert_eq!(sniff(b"\xff\xfe\0\0o\0\0\0"), Sniff::Binary, "UTF-32 LE");
        assert_eq!(sniff(b"\0\0\xfe\xff\0\0\0o"), Sniff::Binary, "UTF-32 BE");
        assert_eq!(sniff(b"\xff\xfeo\0\0\0i\0"), Sniff::Binary, "UTF-16 com U+0000");
        assert_eq!(sniff(b""), Sniff::Text, "vazio");
        assert_eq!(sniff(b"\xef\xbb\xbfola"), Sniff::Text, "BOM UTF-8");
    }

    #[test]
    fn decode_utf8_ascii_bom() {
        assert_eq!(decode(b"abc\n", false), ("abc\n".into(), Encoding::Ascii, false));
        assert_eq!(
            decode("ação\n".as_bytes(), false),
            ("ação\n".into(), Encoding::Utf8, false)
        );
        assert_eq!(
            decode(b"\xef\xbb\xbfcom bom", false),
            ("com bom".into(), Encoding::Utf8Bom, false)
        );
        assert_eq!(Encoding::Utf8Bom.label(), "UTF-8 com BOM");
        assert_eq!(decode(b"", false), (String::new(), Encoding::Ascii, false));
    }

    #[test]
    fn decode_latin1_portuguese() {
        let (s, enc, lossy) = decode(b"a\xe7\xe3o \xe9 f\xe1cil", false);
        assert_eq!(s, "ação é fácil");
        assert_eq!(enc, Encoding::Windows1252);
        assert!(!lossy);
        assert_eq!(enc.label(), "Windows-1252 (Latin-1)");
        assert_eq!(decode(b"\x80 \xe9", false).0, "\u{20AC} é");
        assert_eq!(decode(b"\x93oi\x94 \xe9", false).0, "\u{201C}oi\u{201D} é");
        // 0x81 nao tem caractere: vira C1, exibido como notacao.
        let (s, ..) = decode(b"\x81 \xe9", false);
        assert_eq!(s, "\u{81} é");
        let doc = build_doc("/t".into(), None, None, None, b"x\x81 \xe9".to_vec(), None);
        assert_eq!(shown(&doc, 0), "x<U+0081> é");
    }

    #[test]
    fn decode_mostly_utf8_stray_byte() {
        let mut v = "ação e são fácil ".as_bytes().to_vec();
        v.push(0xFF);
        v.extend_from_slice(" fim".as_bytes());
        let (s, enc, lossy) = decode(&v, false);
        assert_eq!(enc, Encoding::Utf8);
        assert!(lossy);
        assert_eq!(s, "ação e são fácil \u{FFFD} fim");
    }

    #[test]
    fn decode_truncated_tail() {
        // Corte no meio do euro (E2 82 AC): o pedaco sai, sem U+FFFD.
        let mut v = "preço ".as_bytes().to_vec();
        v.extend_from_slice(&[0xE2, 0x82]);
        let (s, enc, lossy) = decode(&v, true);
        assert_eq!((s.as_str(), enc, lossy), ("preço ", Encoding::Utf8, false));
        let mut v = "ç".as_bytes().to_vec();
        v.push(0xF0);
        assert_eq!(decode(&v, true).0, "ç");
        // Sem corte, o mesmo pedaco e um byte invalido.
        let mut v = "preço é ".as_bytes().to_vec();
        v.extend_from_slice(&[0xE2, 0x82]);
        let (s, _, lossy) = decode(&v, false);
        assert!(lossy && s.ends_with('\u{FFFD}'), "{s:?}");
        // Com BOM tambem.
        let mut v = b"\xef\xbb\xbfa".to_vec();
        v.push(0xC3);
        assert_eq!(decode(&v, true), ("a".into(), Encoding::Utf8Bom, false));
        // Caractere completo no fim fica.
        assert_eq!(decode("fim €".as_bytes(), true).0, "fim €");
    }

    #[test]
    fn decode_utf16() {
        assert_eq!(
            decode(b"\xff\xfeo\0i\0\n\0", false),
            ("oi\n".into(), Encoding::Utf16Le, false)
        );
        assert_eq!(
            decode(b"\xfe\xff\0o\0i\0\n", false),
            ("oi\n".into(), Encoding::Utf16Be, false)
        );
        // Byte impar do fim sai; surrogate solto vira U+FFFD.
        assert_eq!(decode(b"\xff\xfeo\0i", false).0, "o");
        let (s, _, lossy) = decode(b"\xff\xfe\x00\xd8o\0", false);
        assert_eq!(s, "\u{FFFD}o");
        assert!(lossy);
        // Cortado num high surrogate: ele sai, sem U+FFFD.
        let (s, _, lossy) = decode(b"\xff\xfeo\0\x3d\xd8", true);
        assert_eq!((s.as_str(), lossy), ("o", false));
        assert_eq!(Encoding::Utf16Le.label(), "UTF-16 LE");
    }

    #[test]
    fn rows_line_endings() {
        let ix = index_rows("");
        assert_eq!((ix.rows.len(), ix.lines, ix.eol), (0, 0, Eol::None));
        let ix = index_rows("a\n");
        assert_eq!((ix.rows.len(), ix.lines, ix.eol), (1, 1, Eol::Lf));
        let ix = index_rows("\n");
        assert_eq!(ix.rows, [Row { start: 0, end: 0, line: 1 }]);
        let ix = index_rows("a\nb");
        assert_eq!((ix.rows.len(), ix.eol), (2, Eol::Lf));
        let doc = doc_of("um\r\ndois\r\n");
        assert_eq!(doc.eol, Eol::CrLf);
        assert_eq!(lines_of(&doc), ["um", "dois"]);
        let doc = doc_of("um\r\ndois\ntres");
        assert_eq!(doc.eol, Eol::Mixed);
        assert_eq!(lines_of(&doc), ["um", "dois", "tres"]);
        // Sem nenhum \n: o \r termina a linha (Mac antigo).
        let doc = doc_of("um\rdois\r");
        assert_eq!(doc.eol, Eol::Cr);
        assert_eq!(lines_of(&doc), ["um", "dois"]);
        // Com \n, um \r solto fica visivel.
        let doc = doc_of("a\rb\nc\n");
        assert_eq!(lines_of(&doc), ["a^Mb", "c"]);
        assert!(doc.has_controls);
        assert_eq!(Eol::Mixed.label(), Some("misto"));
        assert_eq!(Eol::None.label(), None);
    }

    #[test]
    fn rows_wrap_and_tabs() {
        let long = "x".repeat(2500);
        let ix = index_rows(&format!("{long}\ncurta\n"));
        assert_eq!(ix.rows.len(), 4);
        let lines: Vec<u32> = ix.rows.iter().map(|r| r.line).collect();
        assert_eq!(lines, [1, 0, 0, 2], "continuacoes com numero 0");
        assert_eq!(ix.rows[0].end - ix.rows[0].start, WRAP_COLS);
        assert_eq!(ix.max_cols, WRAP_COLS);
        assert_eq!(ix.lines, 2);
        // Nao parte caractere: o "e" com acento (2 bytes) fica inteiro numa linha.
        let long = "é".repeat(1500);
        let doc = doc_of(&long);
        assert_eq!(doc.rows.len(), 2);
        assert_eq!(doc.rows[0].end, 2000);
        assert!(doc.text.is_char_boundary(doc.rows[0].end as usize));
        // TAB vai ate a proxima parada de 4.
        let ix = index_rows("a\tb\t\tc");
        assert_eq!(ix.max_cols, 1 + 3 + 1 + 3 + 4 + 1);
        // Notacao conta as colunas dela.
        let ix = index_rows("\u{1b}\u{202E}");
        assert_eq!(ix.max_cols, 2 + 8);
        assert!(ix.has_bidi && ix.has_controls);
    }

    #[test]
    fn rows_cap() {
        let text = "l\n".repeat(MAX_ROWS + 10);
        let doc = doc_of(&text);
        assert_eq!(doc.rows.len(), MAX_ROWS);
        assert_eq!(doc.lines, MAX_ROWS as u32);
        assert_eq!(doc.truncated, Some(Trunc::Rows { lines: MAX_ROWS as u32 }));
        // O texto termina na ultima linha exibida (busca e copia so nela).
        assert_eq!(doc.text.len(), 2 * MAX_ROWS - 1);
        let doc = doc_of(&"l\n".repeat(MAX_ROWS));
        assert_eq!(doc.truncated, None, "exatamente no limite nao corta");
    }

    #[test]
    fn display_tabs_and_map() {
        let doc = doc_of("a\tb\u{1b}c\n");
        let rv = RowView::build(&doc.text, doc.rows[0], &style());
        assert_eq!(rv.job.text, "a   b^[c");
        // Cada caractere exibido aponta para o byte de origem; o ultimo e o fim.
        assert_eq!(rv.map, [0, 1, 1, 1, 2, 3, 3, 4, 5]);
        // Continuacao comeca na coluna 0 (TAB conta de novo).
        let text = format!("{}\tz", "x".repeat(1000));
        let doc = doc_of(&text);
        assert_eq!(shown(&doc, 1), "    z");
        // Tres estilos: normal, controle e bidi.
        let doc = doc_of("a\u{1b}b\u{202E}c");
        let rv = RowView::build(&doc.text, doc.rows[0], &style());
        let colors: Vec<egui::Color32> =
            rv.job.sections.iter().map(|s| s.format.color).collect();
        let st = style();
        assert_eq!(colors, [st.normal, st.ctrl.0, st.normal, st.bidi.0, st.normal]);
        assert_eq!(rv.job.sections[1].format.background, st.ctrl.1);
    }

    #[test]
    fn display_controls_visible() {
        let doc = doc_of("x\u{7f}\u{85}\u{200B}\u{FEFF}\u{2066}\u{E0041}\u{0}");
        assert_eq!(shown(&doc, 0), "x^?<U+0085><U+200B><U+FEFF><U+2066><U+E0041>^@");
        // Amostra de todo o Unicode: a tela nunca recebe controle, bidi nem
        // invisivel crus.
        let mut text = String::new();
        let mut n = 0u32;
        while n <= 0x10FFFF {
            if let Some(c) = char::from_u32(n) {
                if c != '\n' && c != '\r' {
                    text.push(c);
                }
            }
            n += if n < 0x3000 { 1 } else { 37 };
        }
        for c in "\u{2028}\u{2029}\u{202A}\u{202B}\u{202C}\u{202D}\u{061C}\u{3164}\u{FFA0}".chars() {
            text.push(c);
        }
        let doc = doc_of(&text);
        assert!(doc.rows.len() > 10);
        for r in 0..doc.rows.len() {
            for c in shown(&doc, r).chars() {
                assert!(
                    !c.is_control() && !is_bidi(c) && !is_invisible(c),
                    "U+{:04X} cru na tela",
                    c as u32
                );
            }
        }
    }

    /// Byte invalido (U+FFFD, sem glifo nas fontes do app): "?" com o estilo
    /// dos controles, uma coluna, e o mapa aponta o byte certo. A copia leva
    /// o proprio U+FFFD (nao esconde nada).
    #[test]
    fn invalid_bytes_marked() {
        let doc = build_doc("/t".into(), None, None, None, b"a\xffb\xe7\xe3o\n".to_vec(), None);
        // Mais invalidos que multibyte: Windows-1252, sem U+FFFD.
        assert!(!doc.lossy && !doc.text.contains('\u{FFFD}'));
        let doc = build_doc(
            "/t".into(),
            None,
            None,
            None,
            "ação é fácil ".bytes().chain(*b"\xff fim\n").collect(),
            None,
        );
        assert!(doc.lossy && doc.text.contains('\u{FFFD}'), "{:?}", doc.text);
        let rv = RowView::build(&doc.text, doc.rows[0], &style());
        assert_eq!(rv.job.text, "ação é fácil ? fim");
        let at = doc.text.find('\u{FFFD}').unwrap();
        let q = rv.job.text.find('?').unwrap();
        let sec = rv
            .job
            .sections
            .iter()
            .find(|s| s.byte_range.contains(&q))
            .unwrap();
        assert_eq!((sec.format.color, sec.format.background), style().ctrl);
        let col = rv.job.text[..q].chars().count();
        assert_eq!(rv.map[col], at as u32);
        assert_eq!(rv.map[col + 1], (at + '\u{FFFD}'.len_utf8()) as u32);
        assert_eq!(char_cols('\u{FFFD}', 0), 1);
        assert_eq!(doc.max_cols, "ação é fácil ? fim".chars().count() as u32);
        assert!(copy_text(&doc, 0, doc.text.len() as u32).contains('\u{FFFD}'));
        assert!(!doc.has_controls, "byte invalido nao e controle");
    }

    #[test]
    fn draining_kernel_files() {
        for p in [
            "/proc/kmsg",
            "/sys/kernel/tracing/trace_pipe",
            "/sys/kernel/debug/tracing/trace_pipe",
            "/sys/kernel/tracing/per_cpu/cpu0/trace_pipe_raw",
        ] {
            assert!(is_draining(p), "{p}");
        }
        for p in [
            "/proc/cpuinfo",
            "/proc/kmsg.txt",
            "/home/u/kmsg",
            "/sys/kernel/tracing/trace",
            "/home/u/sys/kernel/trace_pipe",
            "/var/log/messages",
        ] {
            assert!(!is_draining(p), "{p}");
        }
    }

    #[test]
    fn hit_mapping() {
        let doc = doc_of("ab\tç\u{1b}d\nsegunda");
        let rv = RowView::build(&doc.text, doc.rows[0], &style());
        let shown: Vec<char> = rv.job.text.chars().collect();
        assert_eq!(shown.len() + 1, rv.map.len());
        // O byte de cada caractere exibido e o inicio de um caractere do texto
        // e o mapa nunca volta.
        for (i, &b) in rv.map.iter().enumerate() {
            assert!(doc.text.is_char_boundary(b as usize), "{i}");
            if i > 0 {
                assert!(rv.map[i - 1] <= b);
            }
        }
        // O "c" cedilha esta no byte 3 e ocupa o indice exibido 4 (depois de "ab" + 2
        // espacos do TAB); "^[" aponta para o ESC (byte 5).
        assert_eq!(shown[4], 'ç');
        assert_eq!(rv.map[4], 3);
        assert_eq!(&shown[5..7], &['^', '[']);
        assert_eq!((rv.map[5], rv.map[6]), (5, 5));
        assert_eq!(*rv.map.last().unwrap(), doc.rows[0].end);
        // Segunda linha comeca depois do \n.
        let rv = RowView::build(&doc.text, doc.rows[1], &style());
        assert_eq!(rv.map[0], doc.rows[1].start);
        assert_eq!(&doc.text[rv.map[0] as usize..], "segunda");
    }

    #[test]
    fn copy_text_rules() {
        let doc = doc_of("um\r\ndois\tx\r\n");
        let all = copy_text(&doc, 0, doc.text.len() as u32);
        assert_eq!(all, "um\ndois\tx", "CRLF vira \\n e TAB fica");
        let doc = doc_of("a\u{1b}[31mb\u{202E}c\nd");
        assert_eq!(copy_text(&doc, 0, doc.text.len() as u32), "a^[[31mb<U+202E>c\nd");
        // Continuacao de linha longa nao ganha \n.
        let long = "y".repeat(1500);
        let doc = doc_of(&format!("{long}\nz"));
        let got = copy_text(&doc, 0, doc.text.len() as u32);
        assert_eq!(got, format!("{long}\nz"));
        // Trecho no meio, de tras para frente, e vazio.
        let doc = doc_of("abc\ndef\n");
        assert_eq!(copy_text(&doc, 6, 1), "bc\nde");
        assert_eq!(copy_text(&doc, 2, 2), "");
        // Modo CR: o terminador vira \n.
        let doc = doc_of("a\rb\r");
        assert_eq!(copy_text(&doc, 0, 4), "a\nb");
    }

    #[test]
    fn find_all_rules() {
        let t = "Linha um\nLINHA dois linha\nlinhalinha";
        let (m, capped) = find_all(t, "linha", MAX_MATCHES);
        assert!(!capped);
        assert_eq!(m.len(), 5);
        for (a, b) in &m {
            assert!(t[*a as usize..*b as usize].eq_ignore_ascii_case("linha"));
        }
        // Sem sobreposicao.
        assert_eq!(find_all("aaaa", "aa", 10).0, [(0, 2), (2, 4)]);
        // Acentos de caixa: o "E" maiusculo com acento acha o minusculo, e "CAO"
        // (com cedilha e til) acha o minusculo tambem.
        let (m, _) = find_all("Até é ação", "É", 10);
        assert_eq!(m.len(), 2);
        assert_eq!(find_all("ação", "ÇÃO", 10).0, [(1, 6)]);
        // Nao atravessa o fim de linha; vazio nao acha nada.
        assert!(find_all("fim\ninicio", "m\ni", 10).0.is_empty());
        assert!(find_all("fim\r\ninicio", "fim\r", 10).0.is_empty());
        assert!(find_all("abc", "", 10).0.is_empty());
        // Limite.
        let (m, capped) = find_all(&"x ".repeat(50), "x", 10);
        assert_eq!(m.len(), 10);
        assert!(capped);
        let (m, capped) = find_all(&"x ".repeat(10), "x", 10);
        assert_eq!((m.len(), capped), (10, false));
    }

    #[test]
    fn row_of_and_word_at() {
        let doc = doc_of("um dois\n\ntres_4 x\n");
        assert_eq!(row_of(&doc.rows, 0), 0);
        assert_eq!(row_of(&doc.rows, 7), 0);
        assert_eq!(row_of(&doc.rows, 8), 1, "linha vazia");
        assert_eq!(row_of(&doc.rows, 9), 2);
        assert_eq!(row_of(&doc.rows, 1000), 2);
        let t = &doc.text;
        assert_eq!(word_at(t, 4), (3, 7));
        assert_eq!(word_at(t, 3), (3, 7));
        assert_eq!(word_at(t, 12), (9, 15), "com digito e _");
        assert_eq!(word_at(t, 2), (2, 3), "fora de palavra: o caractere");
        assert_eq!(word_at(t, 7), (7, 7), "fim de linha");
        assert_eq!(word_at("ação!", 1), (0, 6));
        assert_eq!(word_at(t, 99), (t.len() as u32, t.len() as u32));
        // Linha inteira, com continuacoes.
        let long = "w".repeat(1500);
        let doc = doc_of(&format!("a\n{long}\nb"));
        assert_eq!(line_at(&doc.rows, 1200), Some((2, 1502)));
        assert_eq!(line_at(&doc.rows, 0), Some((0, 1)));
        assert_eq!(line_at(&[], 0), None);
    }

    // --- Ponta a ponta contra um sshd real (ignorados) ---------------------
    //
    // Mesmas variaveis dos outros e2e (SAGU_E2E_PORT com o sshd de
    // `SetEnv TMUX`, SAGU_E2E_USER, SAGU_E2E_KEY). As fixtures ficam em
    // /tmp/sagu-e2e-view-<pid>-<tag> e sao apagadas no Drop.
    // Rodar com: cargo test e2e_view -- --ignored --test-threads=1

    use crate::download::tests::{close, e2e_host, remote_sh, sftp_session};
    use crate::sftp::{SftpHandle, SftpToUi};
    use crate::vault::Host;

    /// Apaga a fixture remota no fim (arquivos 000 precisam de chmod antes).
    struct Fixture {
        host: Host,
        dir: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let d = &self.dir;
            if let Err(e) = remote_sh(&self.host, &format!("chmod -R u+rwx '{d}'; rm -rf '{d}'")) {
                eprintln!("limpeza remota falhou: {e}");
            }
        }
    }

    fn fixture(tag: &str, script: &str) -> (Host, String, Fixture) {
        let host = e2e_host();
        let t = format!("/tmp/sagu-e2e-view-{}-{tag}", std::process::id());
        let fx = Fixture {
            host: host.clone(),
            dir: t.clone(),
        };
        let full = format!("set -e; T='{t}'; rm -rf \"$T\"; mkdir -p \"$T\"; cd \"$T\"\n{script}");
        remote_sh(&host, &full).expect("preparo da fixture");
        (host, t, fx)
    }

    /// Liga `SLOW_READ` para um caminho enquanto existir (desliga no fim,
    /// inclusive se o teste falhar).
    struct SlowRead;

    impl SlowRead {
        fn on(path: &str, pause: Duration) -> Self {
            *SLOW_READ.lock().unwrap_or_else(|e| e.into_inner()) = Some((path.into(), pause));
            SlowRead
        }
    }

    impl Drop for SlowRead {
        fn drop(&mut self) {
            *SLOW_READ.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Pede a leitura e espera o `Done` dela (andamentos sao ignorados).
    fn view(h: &SftpHandle, id: u64, path: &str) -> Result<Box<ViewDoc>, ViewError> {
        let _cancel = h.read_file(id, path).expect("sessao encerrada");
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(90) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::View(ViewEvent::Done { id: i, result })) => {
                    assert_eq!(i, id, "{path}");
                    return result;
                }
                Ok(SftpToUi::Error(e)) => panic!("erro da sessao: {e}"),
                Ok(SftpToUi::Closed) => panic!("sessao fechou"),
                _ => {}
            }
        }
        panic!("{path}: sem resposta");
    }

    /// Lista `path` e espera a resposta (a sessao continua atendendo).
    fn list_names(h: &SftpHandle, path: &str) -> Vec<String> {
        h.list_dir(path);
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(20) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Listing { path: p, entries }) if p == path => {
                    return entries.into_iter().map(|e| e.name).collect();
                }
                Ok(SftpToUi::Error(e)) => panic!("erro da sessao: {e}"),
                _ => {}
            }
        }
        panic!("listagem de {path} nao chegou");
    }

    #[test]
    #[ignore]
    fn e2e_view_texts() {
        let (host, t, _fx) = fixture(
            "textos",
            r#"printf 'linha 1\nlinha 2\n' > lf.txt
printf 'linha 1\r\nlinha 2\r\n' > crlf.txt
printf 'ação é fácil\n' > utf8.txt
printf 'a\347\343o \351 f\341cil\r\n' > latin1.txt
printf '\357\273\277com bom\n' > bom.txt
printf '\377\376o\000i\000\n\000' > utf16.txt
: > vazio.txt
printf 'cor \033[31mvermelho\033[0m e \342\200\256 fim\n' > controles.txt
yes 'linha de texto do arquivo grande' | head -c 9437184 > grande.txt
{ printf '\303\247'; head -c 4194301 /dev/zero | tr '\000' a; printf '\342\202\254\342\202\254\n'; } > euro.txt
"#,
        );
        let h = sftp_session(&host);
        let p = |n: &str| format!("{t}/{n}");
        let lf = view(&h, 1, &p("lf.txt")).expect("lf");
        assert_eq!((lf.encoding, lf.eol, lf.lines), (Encoding::Ascii, Eol::Lf, 2));
        assert_eq!((lf.text.as_str(), lf.truncated, lf.size), ("linha 1\nlinha 2\n", None, Some(16)));
        assert_eq!(lf.path, p("lf.txt"));
        assert!(lf.mtime.is_some() && lf.target.is_none());
        let crlf = view(&h, 2, &p("crlf.txt")).expect("crlf");
        assert_eq!((crlf.eol, crlf.lines), (Eol::CrLf, 2));
        assert_eq!(copy_text(&crlf, 0, crlf.text.len() as u32), "linha 1\nlinha 2");
        let utf8 = view(&h, 3, &p("utf8.txt")).expect("utf8");
        assert_eq!((utf8.encoding, utf8.text.as_str()), (Encoding::Utf8, "ação é fácil\n"));
        let latin1 = view(&h, 4, &p("latin1.txt")).expect("latin1");
        assert_eq!(latin1.encoding, Encoding::Windows1252);
        assert_eq!((latin1.text.as_str(), latin1.eol), ("ação é fácil\r\n", Eol::CrLf));
        let bom = view(&h, 5, &p("bom.txt")).expect("bom");
        assert_eq!((bom.encoding, bom.text.as_str()), (Encoding::Utf8Bom, "com bom\n"));
        let utf16 = view(&h, 6, &p("utf16.txt")).expect("utf16");
        assert_eq!((utf16.encoding, utf16.text.as_str()), (Encoding::Utf16Le, "oi\n"));
        let vazio = view(&h, 7, &p("vazio.txt")).expect("vazio");
        assert!(vazio.text.is_empty() && vazio.rows.is_empty() && vazio.lines == 0);
        let ctl = view(&h, 8, &p("controles.txt")).expect("controles");
        assert!(ctl.has_bidi && ctl.has_controls);
        let grande = view(&h, 9, &p("grande.txt")).expect("grande");
        assert_eq!(
            grande.truncated,
            Some(Trunc::Bytes {
                shown: MAX_VIEW_BYTES as u64,
                total: Some(9_437_184)
            })
        );
        assert_eq!(grande.text.len(), MAX_VIEW_BYTES);
        assert!(!grande.lossy);
        let euro = view(&h, 10, &p("euro.txt")).expect("euro");
        assert!(matches!(euro.truncated, Some(Trunc::Bytes { .. })));
        assert!(!euro.lossy && !euro.text.contains('\u{FFFD}'), "corte no meio do euro");
        assert_eq!(euro.encoding, Encoding::Utf8);
        assert_eq!(euro.text.len(), MAX_VIEW_BYTES - 1);
        close(h);
    }

    #[test]
    #[ignore]
    fn e2e_view_refusals() {
        let (host, t, _fx) = fixture(
            "recusas",
            r#"printf '\211PNG\r\n\032\n\000\000\000\rIHDR\000\000\001\000' > img.png
head -c 20000 /dev/zero > zeros.bin
printf 'ola\n' > alvo.txt; ln -s alvo.txt link-arq
mkdir pasta; ln -s pasta link-pasta; ln -s /nao/existe/sagu quebrado
ln -s ciclo-b ciclo-a; ln -s ciclo-a ciclo-b; mkfifo fifo
printf 'x' > fechado.txt; chmod 000 fechado.txt
"#,
        );
        let h = sftp_session(&host);
        let p = |n: &str| format!("{t}/{n}");
        assert_eq!(view(&h, 1, &p("img.png")).unwrap_err(), ViewError::Binary);
        assert_eq!(view(&h, 2, &p("zeros.bin")).unwrap_err(), ViewError::Binary);
        let link = view(&h, 3, &p("link-arq")).expect("link para arquivo");
        assert_eq!(link.text, "ola\n");
        assert_eq!(link.target.as_deref(), Some("alvo.txt"));
        assert_eq!(link.path, p("link-arq"));
        assert_eq!(view(&h, 4, &p("link-pasta")).unwrap_err(), ViewError::IsDir);
        assert_eq!(view(&h, 5, &p("pasta")).unwrap_err(), ViewError::IsDir);
        assert_eq!(view(&h, 6, &p("quebrado")).unwrap_err(), ViewError::BrokenLink);
        let t0 = std::time::Instant::now();
        let ciclo = view(&h, 7, &p("ciclo-a")).unwrap_err();
        assert!(
            matches!(ciclo, ViewError::BrokenLink | ViewError::BadLink(_)),
            "{ciclo:?}"
        );
        assert!(t0.elapsed() < Duration::from_secs(15));
        assert_eq!(view(&h, 8, &p("nao-existe")).unwrap_err(), ViewError::NotFound);
        // Fifo: recusado sem abrir (o open travaria o sftp-server); a mesma
        // sessao continua respondendo.
        let t0 = std::time::Instant::now();
        assert_eq!(view(&h, 9, &p("fifo")).unwrap_err(), ViewError::Special);
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert!(list_names(&h, &t).contains(&"fifo".to_string()));
        // Dispositivo (link para /dev/null) tambem.
        assert_eq!(view(&h, 10, "/dev/null").unwrap_err(), ViewError::Special);
        // Sem permissao (root le tudo: pula).
        if remote_sh(&host, "[ \"$(id -u)\" = 0 ]").is_err() {
            assert_eq!(view(&h, 11, &p("fechado.txt")).unwrap_err(), ViewError::Denied);
        }
        // /proc/kmsg (direto ou por link): a leitura bloquearia e, como root,
        // tiraria as mensagens do log do sistema. Recusado antes do open.
        if remote_sh(&host, "[ -e /proc/kmsg ]").is_ok() {
            remote_sh(&host, &format!("ln -s /proc/kmsg '{}'", p("kmsg-link"))).expect("link");
            assert_eq!(view(&h, 12, "/proc/kmsg").unwrap_err(), ViewError::Draining);
            assert_eq!(view(&h, 13, &p("kmsg-link")).unwrap_err(), ViewError::Draining);
        }
        // Outro arquivo do /proc (tamanho 0 tambem): abre normalmente.
        assert!(view(&h, 14, "/proc/cpuinfo").is_ok_and(|d| !d.text.is_empty()));
        close(h);
    }

    #[test]
    #[ignore]
    fn e2e_view_cancel() {
        let (host, t, _fx) = fixture(
            "cancela",
            "yes 'linha de texto para cancelar' | head -c 3000000 > grande.txt\n\
             yes 'linha de texto para cancelar no meio' | head -c 40000000 > maior.txt\n",
        );
        let h = sftp_session(&host);
        let cancel = h.read_file(1, format!("{t}/grande.txt")).expect("sessao encerrada");
        cancel.cancel();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            if let Ok(SftpToUi::View(ViewEvent::Done { id, .. })) =
                h.from_sftp.recv_timeout(Duration::from_millis(100))
            {
                panic!("Done do pedido {id} cancelado");
            }
        }
        // Soltar o Cancel (visualizador ou painel fechado) tambem cancela.
        let cancel = h.read_file(2, format!("{t}/grande.txt")).expect("sessao encerrada");
        drop(cancel);
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            if let Ok(SftpToUi::View(ViewEvent::Done { id: 2, .. })) =
                h.from_sftp.recv_timeout(Duration::from_millis(100))
            {
                panic!("Done do pedido 2 depois de soltar o Cancel");
            }
        }
        // A sessao continua listando e lendo.
        assert!(list_names(&h, &t).contains(&"grande.txt".to_string()));
        let doc = view(&h, 3, &format!("{t}/grande.txt")).expect("leitura depois do cancelamento");
        assert_eq!(doc.text.len(), 3_000_000);
        // Cancelado no meio da leitura (depois do primeiro andamento): nenhum
        // Done, e a sessao segue listando e lendo (o canal auxiliar e trocado).
        // Cada bloco deste arquivo pausa 50 ms (`SLOW_READ`): os 4 MB levam
        // mais de 0,8 s, e o cancelamento cai com certeza no meio.
        let maior = format!("{t}/maior.txt");
        let _lenta = SlowRead::on(&maior, Duration::from_millis(50));
        let cancel = h.read_file(4, maior.clone()).expect("sessao encerrada");
        let t0 = std::time::Instant::now();
        let got = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "sem andamento da leitura");
            match h.from_sftp.recv_timeout(Duration::from_millis(50)) {
                Ok(SftpToUi::View(ViewEvent::Progress { id: 4, got, .. })) if got > 0 => break got,
                Ok(SftpToUi::View(ViewEvent::Done { id: 4, .. })) => {
                    panic!("a leitura terminou antes do primeiro andamento")
                }
                _ => {}
            }
        };
        cancel.cancel();
        assert!(got < MAX_VIEW_BYTES as u64, "cancelado so no fim ({got} bytes)");
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(2) {
            if let Ok(SftpToUi::View(ViewEvent::Done { id: 4, .. })) =
                h.from_sftp.recv_timeout(Duration::from_millis(100))
            {
                panic!("Done do pedido 4 cancelado no meio da leitura");
            }
        }
        drop(cancel);
        assert!(list_names(&h, &t).contains(&"maior.txt".to_string()));
        let doc = view(&h, 5, &format!("{t}/grande.txt")).expect("leitura depois do cancelamento no meio");
        assert_eq!(doc.text.len(), 3_000_000);
        close(h);
    }
}
