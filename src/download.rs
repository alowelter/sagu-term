//! Download de arquivos e pastas pelo navegador SFTP.
//!
//! A UI escolhe itens da pasta remota atual e a pasta de destino no Windows.
//! Aqui ficam as regras de nome (nomes remotos que o Windows nao aceita, ou
//! que poderiam escapar da pasta escolhida), o planejamento dos conflitos e a
//! tarefa que baixa tudo. A tarefa roda na thread da sessao SFTP, separada do
//! loop de comandos (ver `sftp::run_session`), em duas fases:
//! 1. varredura: le a arvore remota inteira antes de gravar qualquer coisa;
//! 2. copia: cada arquivo vai para um temporario na pasta final e so ganha o
//!    nome definitivo (rename) depois de completo, fechado e com a data.
//!
//! Garantias: todo caminho local e a pasta escolhida mais componentes aceitos
//! por `is_safe_component`, conferidos por `local_target` antes de cada uso;
//! nada que ja existe e substituido sem `replace`; links e juncoes locais
//! nunca sao seguidos nem trocados.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileType;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

/// Niveis de pasta abaixo de cada item escolhido (barra ciclos de bind mount).
const MAX_DEPTH: usize = 64;
/// Entradas vistas na varredura antes de desistir (arvores enormes ou sem
/// fim). Conta entrada por entrada, depois que a listagem de cada pasta chega
/// (o russh-sftp so a devolve inteira); a memoria e barrada por `MAX_SCAN_BYTES`.
const MAX_ENTRIES: usize = 200_000;
/// Teto da memoria que a varredura guarda (plano, pastas por listar e listas
/// do relatorio), somada por `text_cost`. Cada entrada guarda caminhos
/// inteiros: sem isso, um servidor hostil com nomes longos em pastas fundas
/// faria poucos MB de listagem virarem GB na memoria.
const MAX_SCAN_BYTES: usize = 512 * 1024 * 1024;
/// Maior nome remoto aceito, em bytes: o Linux limita a 255 e o OpenSSH no
/// Windows fica abaixo de 765. Nome maior e ignorado sem entrar em caminhos.
const MAX_REMOTE_NAME: usize = 1024;
/// Maior trecho de uma mensagem de erro vinda do servidor.
const MAX_REMOTE_MSG: usize = 300;
/// Bloco pedido por leitura (o OpenSSH devolve no maximo 64 KiB por vez).
const CHUNK: usize = 256 * 1024;
/// Intervalo minimo entre eventos de andamento (nao inunda a UI).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
/// Sufixo dos temporarios: `.<nome>.<hex8>.sagu-part` na pasta final.
const TEMP_SUFFIX: &str = ".sagu-part";
/// Tempo da sonda que separa o erro de um arquivo de uma conexao perdida.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Tamanho maximo de um nome no Windows (unidades UTF-16).
const MAX_NAME: usize = 255;

// Motivos por item (textos para o usuario).
const R_BAD_ENCODING: &str = "nome com codificação não suportada";
const R_INVALID: &str = "nome inválido enviado pelo servidor";
const R_DIR_LINK: &str = "link simbólico para pasta (não seguido)";
const R_BROKEN_LINK: &str = "link simbólico quebrado";
const R_SPECIAL: &str = "arquivo especial (dispositivo, fifo ou socket)";
const R_TOO_DEEP: &str = "pasta profunda demais (mais de 64 níveis)";
const R_EXISTS: &str = "já existe no destino; nada foi substituído";
const R_DIR_EXISTS: &str = "já existe uma pasta com esse nome";
const R_FILE_EXISTS: &str = "já existe um arquivo com esse nome";
const R_DIR_IS_LINK: &str = "o destino é um atalho de pasta (link ou junção); nada foi alterado";
const R_TARGET_IS_LINK: &str = "o destino é um link; nada foi substituído";
const R_IN_USE: &str = "o arquivo existente está somente leitura ou em uso";
const R_APPEARED: &str = "apareceu no destino durante o download; nada foi substituído";
const R_SKIPPED_EXISTING: &str = "já existia no destino (pulado)";
const R_NO_TEMP: &str = "não foi possível criar o arquivo temporário";
const R_TOO_LONG: &str = "nome longo demais enviado pelo servidor";

// Motivos que interrompem o lote inteiro.
const F_DISK_FULL: &str = "disco cheio";
const F_DEST_GONE: &str = "a pasta de destino não está mais acessível";
const F_CONNECTION: &str = "conexão com o servidor perdida";
const F_TOO_MANY: &str = "a seleção tem mais de 200.000 itens; baixe partes menores";
const F_TOO_BIG: &str = "a seleção é grande demais; baixe partes menores";
const F_INTERNAL: &str = "erro interno no download";

/// Item escolhido no navegador (entrada da pasta atual).
#[derive(Clone, Debug)]
pub struct Pick {
    /// Caminho remoto completo (da listagem).
    pub remote: String,
    /// Nome remoto original.
    pub name: String,
}

/// Item enviado a sessao para baixar.
#[derive(Clone, Debug)]
pub struct DownloadItem {
    /// Caminho remoto completo (da listagem).
    pub remote: String,
    /// Nome remoto original (so para mensagens).
    pub name: String,
    /// Componente local ja saneado e unico entre os escolhidos.
    pub local: String,
    /// Usuario autorizou substituir arquivo / mesclar pasta que ja existe com `local`.
    pub replace: bool,
}

/// Resultado do planejamento feito pela UI apos escolher a pasta.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub dest: PathBuf,
    /// Validos, na ordem da listagem; todos com `replace = false`.
    pub items: Vec<DownloadItem>,
    /// Indices em `items` cujo nome local ja existe em `dest` (qualquer tipo, inclusive link).
    pub conflicts: Vec<usize>,
    /// (nome remoto, motivo) dos que nao tem nome local possivel.
    pub invalid: Vec<(String, String)>,
}

/// Resposta do usuario quando itens escolhidos ja existem no destino.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ConflictChoice {
    /// Troca os arquivos de mesmo nome; pastas existentes sao mescladas.
    Replace,
    /// Baixa so os que nao existem.
    Skip,
    Cancel,
}

/// Lado da UI do cancelamento. Soltar (painel fechado) tambem cancela.
pub struct Cancel(watch::Sender<bool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
}

/// Par de cancelamento: a UI fica com o `Cancel`, a tarefa com o `Receiver`.
pub fn cancel_pair() -> (Cancel, watch::Receiver<bool>) {
    let (tx, rx) = watch::channel(false);
    (Cancel(tx), rx)
}

/// Eventos de um download, repassados a UI pelo loop da sessao.
#[derive(Clone, Debug)]
pub enum DownloadEvent {
    /// Varrendo pastas: `found` entradas vistas ate agora (a cada 100 ms).
    Scanning { id: u64, found: usize },
    /// Arquivo `index` (0-based) de `count`; bytes somados de todo o lote.
    Progress {
        id: u64,
        index: usize,
        count: usize,
        /// Caminho remoto relativo do arquivo atual.
        name: String,
        done: u64,
        total: u64,
    },
    /// Sempre exatamente um por lote (exceto se a sessao morrer antes).
    Finished(Box<DownloadReport>),
}

/// Resultado de um lote.
#[derive(Clone, Debug, Default)]
pub struct DownloadReport {
    pub id: u64,
    pub dest: PathBuf,
    /// Arquivos gravados com o nome final / arquivos planejados.
    pub saved: usize,
    pub files: usize,
    /// Pastas criadas (ou mescladas) no destino.
    pub dirs: usize,
    /// Caminho local relativo do ultimo arquivo salvo (para "x baixado em ...").
    pub last_saved: String,
    /// (caminho remoto relativo, motivo).
    pub failed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
    /// (remoto relativo, local relativo) com nome ajustado para o Windows.
    pub renamed: Vec<(String, String)>,
    pub cancelled: bool,
    /// Motivo que interrompeu o lote todo (disco cheio, conexao perdida, grande demais).
    pub fatal: Option<String>,
}

// --- Nomes remotos no Windows ------------------------------------------------

/// Caractere de direcao de texto: disfarcaria a extensao ("foto\u{202E}gpj.exe"
/// aparece como "fotoexe.jpg").
fn is_bidi(c: char) -> bool {
    matches!(
        c,
        '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

/// Caractere que nao pode ficar num nome local: controles (C0, DEL, C1), os
/// proibidos pelo Windows (':' tambem abriria um fluxo alternativo do NTFS),
/// separadores e os de direcao de texto.
fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '<' | '>' | ':' | '"' | '\\' | '|' | '?' | '*' | '/')
        || is_bidi(c)
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Nome de dispositivo do Windows (CON, NUL, COM1...): vale o trecho antes do
/// primeiro '.', sem espacos nas pontas ("con .txt" tambem e reservado).
fn is_reserved(name: &str) -> bool {
    let radical = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_matches(' ')
        .to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"].contains(&radical.as_str()) {
        return true;
    }
    ["COM", "LPT"].iter().any(|pre| {
        let Some(rest) = radical.strip_prefix(pre) else {
            return false;
        };
        let mut cs = rest.chars();
        matches!(
            (cs.next(), cs.next()),
            (Some('0'..='9' | '\u{00B9}' | '\u{00B2}' | '\u{00B3}'), None)
        )
    })
}

/// Forma de nome curto 8.3 com "~digito" (ex.: "PROGRA~1"): o Windows poderia
/// resolve-la para um irmao de nome longo.
fn has_short_alias(name: &str) -> bool {
    let mut parts = name.split('.');
    let base = parts.next().unwrap_or("");
    let ext = parts.next().unwrap_or("");
    if parts.next().is_some() || base.chars().count() > 8 || ext.chars().count() > 3 {
        return false;
    }
    let cs: Vec<char> = base.chars().collect();
    cs.windows(2).any(|w| w[0] == '~' && w[1].is_ascii_digit())
}

/// Troca por '_' o '~' de cada "~digito" da base de um nome no formato 8.3.
fn break_short_alias(name: &str) -> String {
    if !has_short_alias(name) {
        return name.to_string();
    }
    let (base, ext) = name.split_at(name.find('.').unwrap_or(name.len()));
    let cs: Vec<char> = base.chars().collect();
    let mut out: String = cs
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            if c == '~' && cs.get(i + 1).is_some_and(char::is_ascii_digit) {
                '_'
            } else {
                c
            }
        })
        .collect();
    out.push_str(ext);
    out
}

/// Troca por '_' a sequencia final de '.' e ' ' (o Win32 a apagaria: "..." e
/// ".. " virariam a propria pasta ou a de cima).
fn fix_trailing(name: &str) -> String {
    let keep = name.trim_end_matches(['.', ' ']);
    format!("{keep}{}", "_".repeat(name.len() - keep.len()))
}

/// Primeiros caracteres de `s` que cabem em `budget` unidades UTF-16.
fn take_utf16(s: &str, budget: usize) -> String {
    let mut used = 0;
    s.chars()
        .take_while(|c| {
            used += c.len_utf16();
            used <= budget
        })
        .collect()
}

/// Corta um nome para `MAX_NAME` unidades UTF-16 tirando caracteres do fim do
/// radical e preservando uma extensao curta (ate 16 caracteres).
fn shorten(name: &str) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 && (1..=16).contains(&name[i + 1..].chars().count()) => name.split_at(i),
        _ => (name, ""),
    };
    let mut out = take_utf16(stem, MAX_NAME.saturating_sub(utf16_len(ext)));
    out.push_str(ext);
    out
}

/// Nome local seguro para um nome remoto, ou o motivo de nao haver um.
/// Deterministico; o resultado sempre passa em `is_safe_component`.
pub fn local_name(remote: &str) -> Result<String, &'static str> {
    if remote.is_empty() || remote == "." || remote == ".." || remote.contains(['/', '\0']) {
        return Err(R_INVALID);
    }
    // Nome nao UTF-8 (decodificado com perda): nao da para pedi-lo de volta.
    if remote.contains('\u{FFFD}') {
        return Err(R_BAD_ENCODING);
    }
    let out: String = remote
        .chars()
        .map(|c| if is_forbidden(c) { '_' } else { c })
        .collect();
    let mut out = fix_trailing(&out);
    if is_reserved(&out) {
        out.insert(0, '_');
    }
    let mut out = break_short_alias(&out);
    // So passa de 255 pelo prefixo acima (ou num servidor que nao e Linux).
    // Cortar pode deixar ponto/espaco no fim ou expor um nome reservado:
    // repete ate estabilizar (no maximo duas voltas na pratica).
    for _ in 0..4 {
        if utf16_len(&out) <= MAX_NAME {
            break;
        }
        out = fix_trailing(&shorten(&out));
        if is_reserved(&out) {
            out.insert(0, '_');
        }
    }
    debug_assert!(is_safe_component(&out), "{out:?}");
    if is_safe_component(&out) {
        Ok(out)
    } else {
        Err(R_INVALID)
    }
}

/// Componente de caminho que o Windows usa exatamente como esta: nao vazio,
/// nem "." ou "..", ate 255 unidades UTF-16, sem caractere proibido, sem
/// ponto/espaco no fim, sem nome reservado ou alias 8.3, e um unico
/// `Component::Normal` igual a ele mesmo.
pub fn is_safe_component(s: &str) -> bool {
    if s.is_empty() || s == "." || s == ".." || utf16_len(s) > MAX_NAME {
        return false;
    }
    if s.chars().any(is_forbidden) || s.ends_with(['.', ' ']) {
        return false;
    }
    if is_reserved(s) || has_short_alias(s) {
        return false;
    }
    let mut comps = Path::new(s).components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(c)), None) if c == OsStr::new(s)
    )
}

/// `root` mais os componentes de `rel`, so se todos forem seguros e o
/// resultado ficar exatamente `rel.len()` niveis abaixo de `root`.
pub fn local_target(root: &Path, rel: &[String]) -> Option<PathBuf> {
    if rel.is_empty() || !rel.iter().all(|c| is_safe_component(c)) {
        return None;
    }
    let mut p = root.to_path_buf();
    for c in rel {
        p.push(c);
    }
    (p.starts_with(root) && p.components().count() == root.components().count() + rel.len())
        .then_some(p)
}

/// Chave de comparacao sem diferenciar maiusculas, como o NTFS: maiuscula 1:1
/// por caractere ("Ä" = "ä"); quando a maiuscula tem mais de um caractere,
/// fica o original ("straße" != "STRASSE").
fn fold_key(s: &str) -> String {
    s.chars()
        .map(|c| {
            let mut up = c.to_uppercase();
            match (up.next(), up.next()) {
                (Some(u), None) => u,
                _ => c,
            }
        })
        .collect()
}

/// Radical e extensao, separados no ultimo '.' (".bashrc" nao tem extensao).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Nomes locais unicos (sem diferenciar maiusculas) para os nomes de uma
/// pasta, que o chamador passa ordenados por bytes. Colisao vira
/// "radical (2).ext", "(3)"... Mesma entrada, mesmo resultado.
pub fn unique_local_names(names: &[String]) -> Vec<Result<String, &'static str>> {
    let mut used: HashSet<String> = HashSet::new();
    // Proximo sufixo por nome base: sem custo quadratico quando muitos nomes
    // viram o mesmo (ex.: "a?", "a*", "a:"...).
    let mut next_k: HashMap<String, usize> = HashMap::new();
    names
        .iter()
        .map(|n| {
            let base = local_name(n)?;
            let key = fold_key(&base);
            if used.insert(key.clone()) {
                return Ok(base);
            }
            let (stem, ext) = match split_ext(&base) {
                // Extensao enorme: o sufixo vai no fim do nome.
                (_, ext) if utf16_len(ext) > 32 => (base.as_str(), ""),
                parts => parts,
            };
            let k0 = next_k.get(&key).copied().unwrap_or(2);
            // Com n nomes, algum destes n+1 sufixos esta livre.
            for k in k0..=k0 + names.len() {
                let suffix = format!(" ({k})");
                let budget = MAX_NAME.saturating_sub(utf16_len(&suffix) + utf16_len(ext));
                let cand = format!("{}{suffix}{ext}", take_utf16(stem, budget));
                if is_safe_component(&cand) && used.insert(fold_key(&cand)) {
                    next_k.insert(key, k + 1);
                    return Ok(cand);
                }
            }
            Err(R_INVALID)
        })
        .collect()
}

/// Nome do temporario de `local`: comeca com '.' (radical vazio, nunca
/// reservado), no maximo 120 unidades UTF-16, sorteado.
fn temp_name(local: &str) -> String {
    format!(
        ".{}.{:08x}{TEMP_SUFFIX}",
        take_utf16(local, 100),
        rand::random::<u32>()
    )
}

// --- Planejamento na UI --------------------------------------------------------

/// Nomes locais dos itens escolhidos e quais ja existem em `dest`.
pub fn prepare(dest: &Path, picks: &[Pick]) -> Prepared {
    let mut order: Vec<usize> = (0..picks.len()).collect();
    order.sort_by(|&a, &b| picks[a].name.cmp(&picks[b].name));
    let sorted: Vec<String> = order.iter().map(|&i| picks[i].name.clone()).collect();
    let mut locals: Vec<Option<Result<String, &'static str>>> = vec![None; picks.len()];
    for (res, &i) in unique_local_names(&sorted).into_iter().zip(&order) {
        locals[i] = Some(res);
    }
    let mut out = Prepared {
        dest: dest.to_path_buf(),
        items: Vec::new(),
        conflicts: Vec::new(),
        invalid: Vec::new(),
    };
    for (pick, res) in picks.iter().zip(locals) {
        match res {
            Some(Ok(local)) => {
                // Qualquer coisa com esse nome conta, inclusive link quebrado
                // (e "A.TXT" contra "a.txt": o NTFS nao diferencia).
                let exists = local_target(dest, std::slice::from_ref(&local))
                    .is_some_and(|p| std::fs::symlink_metadata(p).is_ok());
                if exists {
                    out.conflicts.push(out.items.len());
                }
                out.items.push(DownloadItem {
                    remote: pick.remote.clone(),
                    name: pick.name.clone(),
                    local,
                    replace: false,
                });
            }
            Some(Err(why)) => out.invalid.push((pick.name.clone(), why.to_string())),
            None => {}
        }
    }
    out
}

/// Aplica a escolha: Replace marca os conflitos com `replace`; Skip os tira
/// (devolvendo-os como ignorados); Cancel devolve lista vazia. Lista vazia =
/// nada a fazer.
pub fn resolve(p: Prepared, choice: ConflictChoice) -> (Vec<DownloadItem>, Vec<(String, String)>) {
    match choice {
        ConflictChoice::Cancel => (Vec::new(), Vec::new()),
        ConflictChoice::Replace => {
            let mut items = p.items;
            for &i in &p.conflicts {
                if let Some(it) = items.get_mut(i) {
                    it.replace = true;
                }
            }
            (items, Vec::new())
        }
        ConflictChoice::Skip => {
            let mut items = Vec::new();
            let mut skipped = Vec::new();
            for (i, it) in p.items.into_iter().enumerate() {
                if p.conflicts.contains(&i) {
                    skipped.push((it.name, R_SKIPPED_EXISTING.to_string()));
                } else {
                    items.push(it);
                }
            }
            (items, skipped)
        }
    }
}

// --- Operacoes locais ----------------------------------------------------------

/// Erro local: texto fixo, ou erro de E/S (este passa pela checagem de fatal).
#[derive(Debug)]
enum LocalErr {
    Msg(&'static str),
    Io(io::Error),
}

/// Garante a pasta `target`: cria se nao existe; pasta real ja existente so
/// serve com `replace` (mescla). Nunca entra num link ou juncao local.
fn ensure_dir(target: &Path, replace: bool) -> Result<(), LocalErr> {
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => match std::fs::create_dir(target) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(LocalErr::Msg(R_APPEARED)),
            Err(e) => Err(LocalErr::Io(e)),
        },
        Ok(m) if m.file_type().is_symlink() => Err(LocalErr::Msg(R_DIR_IS_LINK)),
        Ok(m) if m.is_dir() && replace => Ok(()),
        Ok(m) if m.is_dir() => Err(LocalErr::Msg(R_EXISTS)),
        Ok(_) => Err(LocalErr::Msg(R_FILE_EXISTS)),
        Err(e) => Err(LocalErr::Io(e)),
    }
}

/// Da o nome final ao temporario completo. So substitui um arquivo comum e
/// com `replace`; link, juncao ou pasta no lugar nunca sao trocados.
fn place(tmp: &Path, target: &Path, replace: bool) -> Result<(), LocalErr> {
    let rename = || std::fs::rename(tmp, target).map_err(LocalErr::Io);
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => rename(),
        Ok(m) if m.file_type().is_symlink() => Err(LocalErr::Msg(R_TARGET_IS_LINK)),
        Ok(m) if m.is_dir() => Err(LocalErr::Msg(R_DIR_EXISTS)),
        Ok(_) if replace => std::fs::rename(tmp, target).map_err(|e| {
            if e.kind() == io::ErrorKind::PermissionDenied {
                LocalErr::Msg(R_IN_USE)
            } else {
                LocalErr::Io(e)
            }
        }),
        Ok(_) => Err(LocalErr::Msg(R_EXISTS)),
        Err(e) => Err(LocalErr::Io(e)),
    }
}

/// Temporario de um arquivo em andamento: apagado ao sair de escopo (erro,
/// cancelamento ou tarefa abortada), salvo `keep()` apos o rename final.
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn keep(&mut self) {
        self.0 = None;
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    // 112 = ERROR_DISK_FULL, 39 = ERROR_HANDLE_DISK_FULL (so no Windows).
    e.kind() == io::ErrorKind::StorageFull || (cfg!(windows) && matches!(e.raw_os_error(), Some(112 | 39)))
}

// --- Tarefa do download ------------------------------------------------------

/// Memoria aproximada de um texto guardado: conteudo, cabecalho da `String` e
/// o que o alocador gasta por bloco.
fn text_cost(s: &str) -> usize {
    s.len() + std::mem::size_of::<String>() + 16
}

/// Memoria aproximada dos componentes locais de uma entrada.
fn rel_cost(rel: &[String]) -> usize {
    std::mem::size_of::<Vec<String>>() + rel.iter().map(|c| text_cost(c)).sum::<usize>()
}

/// No maximo `max` caracteres de `s` (texto vindo do servidor), com "…" se cortou.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}\u{2026}", &s[..i]),
        None => s.to_string(),
    }
}

/// Espera `fut`, a menos que o usuario cancele antes ou o painel feche
/// (remetente solto): `None`. Vale com um pedido em voo, que e abandonado.
async fn unless_cancelled<T>(
    cancel: &mut watch::Receiver<bool>,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    if *cancel.borrow() || cancel.has_changed().is_err() {
        return None;
    }
    tokio::select! {
        biased;
        // Mudou (so muda para cancelar) ou o remetente foi solto.
        _ = cancel.changed() => None,
        r = fut => Some(r),
    }
}

/// Pastas que falharam na copia, e as de dentro delas. O plano traz cada
/// pasta antes do seu conteudo, entao basta olhar a pasta-mae: uma consulta
/// por entrada, mesmo com milhares de subpastas puladas.
#[derive(Default)]
struct FailedDirs<'p>(HashSet<&'p [String]>);

impl<'p> FailedDirs<'p> {
    /// A entrada fica dentro de uma pasta que falhou? Uma pasta pulada assim
    /// tambem entra no conjunto, para levar junto o proprio conteudo.
    fn skip(&mut self, rel: &'p [String], is_dir: bool) -> bool {
        let inside = rel
            .split_last()
            .is_some_and(|(_, parent)| self.0.contains(parent));
        if inside && is_dir {
            self.0.insert(rel);
        }
        inside
    }

    fn add(&mut self, rel: &'p [String]) {
        self.0.insert(rel);
    }
}

/// Uma linha do plano: pasta a criar ou arquivo a baixar.
struct Entry {
    remote: String,
    /// Componentes locais a partir do destino (o 1o e o item escolhido).
    rel: Vec<String>,
    /// Caminho remoto relativo (com "/"), para mensagens.
    shown: String,
    kind: Kind,
    replace: bool,
}

impl Entry {
    /// Memoria aproximada desta linha (conta para `MAX_SCAN_BYTES`).
    fn cost(&self) -> usize {
        std::mem::size_of::<Entry>()
            + text_cost(&self.remote)
            + text_cost(&self.shown)
            + rel_cost(&self.rel)
    }
}

enum Kind {
    Dir,
    File { size: u64, mtime: Option<u32> },
}

/// Pasta ainda por listar na varredura.
struct Pending {
    remote: String,
    rel: Vec<String>,
    shown: String,
    depth: usize,
    replace: bool,
}

impl Pending {
    /// Memoria aproximada (conta para `MAX_SCAN_BYTES`).
    fn cost(&self) -> usize {
        std::mem::size_of::<Pending>()
            + text_cost(&self.remote)
            + text_cost(&self.shown)
            + rel_cost(&self.rel)
    }
}

/// Falha ao baixar um arquivo.
enum Step {
    /// So este item; o lote continua.
    Failed(String),
    /// Interrompe o lote inteiro.
    Fatal(&'static str),
    Cancelled,
}

/// Posicao do arquivo atual no lote (para os eventos de andamento).
#[derive(Clone, Copy)]
struct Prog {
    index: usize,
    count: usize,
    done: u64,
    total: u64,
}

/// Um lote em andamento (varredura e copia).
struct Job<'a> {
    sftp: &'a SftpSession,
    id: u64,
    dest: &'a Path,
    cancel: watch::Receiver<bool>,
    tx: &'a UnboundedSender<DownloadEvent>,
    report: &'a mut DownloadReport,
    last_event: Option<Instant>,
    /// Memoria guardada pela varredura ate agora (ver `MAX_SCAN_BYTES`).
    bytes: usize,
}

/// Garante um `Finished` por lote: `send` no fim normal; se a tarefa cair no
/// meio (panico), o `Drop` manda o que ja foi feito com o motivo "erro
/// interno" (sem isso o rodape ficaria em "Baixando..." para sempre). Numa
/// tarefa abortada junto com a sessao o canal ja nao entrega nada (a UI
/// recebe o `Closed`).
struct FinishGuard<'a> {
    tx: &'a UnboundedSender<DownloadEvent>,
    report: DownloadReport,
    sent: bool,
}

impl FinishGuard<'_> {
    fn send(mut self) {
        self.sent = true;
        let report = std::mem::take(&mut self.report);
        let _ = self.tx.send(DownloadEvent::Finished(Box::new(report)));
    }
}

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        if !self.sent {
            let mut report = std::mem::take(&mut self.report);
            report.fatal.get_or_insert_with(|| F_INTERNAL.into());
            let _ = self.tx.send(DownloadEvent::Finished(Box::new(report)));
        }
    }
}

/// Tarefa do download (spawnada pela sessao SFTP). Termina sempre com um
/// `DownloadEvent::Finished`, salvo se for abortada junto com a sessao.
pub async fn run(
    sftp: Arc<SftpSession>,
    id: u64,
    dest: PathBuf,
    items: Vec<DownloadItem>,
    cancel: watch::Receiver<bool>,
    tx: UnboundedSender<DownloadEvent>,
) {
    let mut finish = FinishGuard {
        tx: &tx,
        report: DownloadReport {
            id,
            dest: dest.clone(),
            ..Default::default()
        },
        sent: false,
    };
    let mut job = Job {
        sftp: &sftp,
        id,
        dest: &dest,
        cancel,
        tx: &tx,
        report: &mut finish.report,
        last_event: None,
        bytes: 0,
    };
    if let Some(plan) = job.scan(&items).await {
        job.copy(&plan).await;
    }
    drop(job);
    finish.send();
}

impl Job<'_> {
    /// Cancelado pela UI, ou o painel foi fechado (remetente solto).
    fn cancelled(&self) -> bool {
        *self.cancel.borrow() || self.cancel.has_changed().is_err()
    }

    /// Hora de mandar outro evento de andamento?
    fn due(&mut self, force: bool) -> bool {
        let now = Instant::now();
        if force || self.last_event.is_none_or(|t| now.duration_since(t) >= PROGRESS_EVERY) {
            self.last_event = Some(now);
            true
        } else {
            false
        }
    }

    /// Conta uma entrada vista na varredura; `false` = parar (cancelado ou
    /// grande demais, em quantidade ou em memoria).
    fn tick(&mut self, seen: &mut usize) -> bool {
        if self.cancelled() {
            self.report.cancelled = true;
            return false;
        }
        *seen += 1;
        if *seen > MAX_ENTRIES {
            self.report.fatal = Some(F_TOO_MANY.into());
            return false;
        }
        if self.bytes > MAX_SCAN_BYTES {
            self.report.fatal = Some(F_TOO_BIG.into());
            return false;
        }
        if self.due(false) {
            let _ = self.tx.send(DownloadEvent::Scanning {
                id: self.id,
                found: *seen,
            });
        }
        true
    }

    /// Espera um pedido da varredura sem travar o "Cancelar" (uma listagem
    /// grande so volta inteira); `None` = cancelado, ja anotado no relatorio.
    async fn ask<T>(&mut self, fut: impl std::future::Future<Output = T>) -> Option<T> {
        let r = unless_cancelled(&mut self.cancel, fut).await;
        if r.is_none() {
            self.report.cancelled = true;
        }
        r
    }

    /// Anota um item ignorado na varredura (conta para `MAX_SCAN_BYTES`).
    fn note_skipped(&mut self, shown: String, why: &str) {
        self.bytes += text_cost(&shown) + text_cost(why);
        self.report.skipped.push((shown, why.into()));
    }

    /// Anota um nome ajustado para o Windows (conta para `MAX_SCAN_BYTES`).
    fn note_renamed(&mut self, shown: String, local: String) {
        self.bytes += text_cost(&shown) + text_cost(&local);
        self.report.renamed.push((shown, local));
    }

    /// A conexao ainda responde? Sem isso, com a rede caida cada arquivo
    /// restante esperaria o timeout do pedido.
    async fn connection_alive(&self) -> bool {
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, self.sftp.canonicalize(".")).await,
            Ok(Ok(_))
        )
    }

    /// Erro remoto num item: falha so dele, ou fatal se a conexao caiu. A
    /// mensagem traz texto do servidor, entao e cortada.
    async fn remote_problem(&self, msg: String) -> Step {
        if self.connection_alive().await {
            Step::Failed(clip(&msg, MAX_REMOTE_MSG))
        } else {
            Step::Fatal(F_CONNECTION)
        }
    }

    /// Erro local num item: disco cheio ou destino sumido param o lote.
    fn local_problem(&self, err: LocalErr) -> Step {
        match err {
            LocalErr::Msg(m) => Step::Failed(m.into()),
            LocalErr::Io(e) if is_disk_full(&e) => Step::Fatal(F_DISK_FULL),
            LocalErr::Io(_) if !self.dest.is_dir() => Step::Fatal(F_DEST_GONE),
            LocalErr::Io(e) => Step::Failed(e.to_string()),
        }
    }

    /// Registra a falha de um item; `false` = parar o lote.
    fn record(&mut self, shown: String, step: Step) -> bool {
        match step {
            Step::Failed(m) => {
                self.bytes += text_cost(&shown) + text_cost(&m);
                self.report.failed.push((shown, m));
                true
            }
            Step::Fatal(m) => {
                self.report.fatal = Some(m.into());
                false
            }
            Step::Cancelled => {
                self.report.cancelled = true;
                false
            }
        }
    }

    /// Varredura: le a arvore remota inteira (nada e gravado ainda) e devolve
    /// o plano em ordem deterministica, cada pasta antes do seu conteudo.
    /// `None` = parar (cancelado, grande demais ou conexao perdida).
    async fn scan(&mut self, items: &[DownloadItem]) -> Option<Vec<Entry>> {
        let sftp = self.sftp;
        let mut plan = Vec::new();
        let mut seen = 0usize;
        if self.due(true) {
            let _ = self.tx.send(DownloadEvent::Scanning { id: self.id, found: 0 });
        }
        for item in items {
            if !self.tick(&mut seen) {
                return None;
            }
            // Mesma regra dos nomes de dentro das pastas (ver `walk`).
            if item.name.len() > MAX_REMOTE_NAME {
                self.note_skipped(clip(&item.name, 40), R_TOO_LONG);
                continue;
            }
            let shown = item.name.clone();
            // Defesa em profundidade: a UI ja saneou o nome.
            if !is_safe_component(&item.local) {
                self.record(shown, Step::Failed(R_INVALID.into()));
                continue;
            }
            let meta = match self.ask(sftp.symlink_metadata(item.remote.clone())).await? {
                Ok(m) => m,
                Err(e) => {
                    let step = self.remote_problem(format!("não foi possível ler: {e}")).await;
                    if !self.record(shown, step) {
                        return None;
                    }
                    continue;
                }
            };
            let kind = match meta.file_type() {
                FileType::Dir => Kind::Dir,
                FileType::File => Kind::File {
                    size: meta.size.unwrap_or(0),
                    mtime: meta.mtime,
                },
                // Link escolhido explicitamente: segue uma vez (inclusive
                // para pasta; os links dentro dela nao sao seguidos).
                FileType::Symlink => match self.ask(sftp.metadata(item.remote.clone())).await? {
                    Ok(m) if m.file_type() == FileType::Dir => Kind::Dir,
                    Ok(m) if m.file_type() == FileType::File => Kind::File {
                        size: m.size.unwrap_or(0),
                        mtime: m.mtime,
                    },
                    Ok(_) => {
                        self.note_skipped(shown, R_SPECIAL);
                        continue;
                    }
                    Err(_) => {
                        self.note_skipped(shown, R_BROKEN_LINK);
                        continue;
                    }
                },
                // FIFO, socket, dispositivo: nunca abertos (um FIFO travaria).
                FileType::Other => {
                    self.note_skipped(shown, R_SPECIAL);
                    continue;
                }
            };
            if item.local != item.name {
                self.note_renamed(item.name.clone(), item.local.clone());
            }
            let rel = vec![item.local.clone()];
            let is_dir = matches!(kind, Kind::Dir);
            let line = Entry {
                remote: item.remote.clone(),
                rel: rel.clone(),
                shown: shown.clone(),
                kind,
                replace: item.replace,
            };
            self.bytes += line.cost();
            plan.push(line);
            if is_dir {
                let root = Pending {
                    remote: item.remote.clone(),
                    rel,
                    shown,
                    depth: 1,
                    replace: item.replace,
                };
                self.bytes += root.cost();
                self.walk(root, &mut plan, &mut seen).await?;
            }
        }
        Some(plan)
    }

    /// Lista a subarvore de `root` (pilha explicita, sem recursao) e acrescenta
    /// o conteudo ao plano. Links para pasta aqui dentro nao sao seguidos.
    async fn walk(&mut self, root: Pending, plan: &mut Vec<Entry>, seen: &mut usize) -> Option<()> {
        let sftp = self.sftp;
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            if dir.depth > MAX_DEPTH {
                // A pasta em si e criada; o conteudo nao e lido.
                self.note_skipped(dir.shown, R_TOO_DEEP);
                continue;
            }
            let listing = match self.ask(sftp.read_dir(dir.remote.clone())).await? {
                Ok(l) => l,
                Err(e) => {
                    // A pasta e criada vazia.
                    let step = self.remote_problem(format!("não foi possível listar: {e}")).await;
                    if !self.record(dir.shown, step) {
                        return None;
                    }
                    continue;
                }
            };
            let mut entries: Vec<(String, _)> = listing.map(|e| (e.file_name(), e)).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            // Nome maior que qualquer sistema de arquivos aceita: ignorado
            // antes de entrar em algum caminho (no resumo, so o comeco dele).
            let (entries, too_long): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|(name, _)| name.len() <= MAX_REMOTE_NAME);
            for (name, _) in too_long {
                if !self.tick(seen) {
                    return None;
                }
                self.note_skipped(format!("{}/{}", dir.shown, clip(&name, 40)), R_TOO_LONG);
            }
            let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
            let mut subdirs = Vec::new();
            for ((name, de), local) in entries.into_iter().zip(unique_local_names(&names)) {
                if !self.tick(seen) {
                    return None;
                }
                let shown = format!("{}/{name}", dir.shown);
                let local = match local {
                    Ok(l) => l,
                    Err(why) => {
                        self.note_skipped(shown, why);
                        continue;
                    }
                };
                let remote = de.path();
                let mut meta = de.metadata();
                if meta.permissions.is_none() {
                    // Listagem sem o tipo: confirma com lstat.
                    match self.ask(sftp.symlink_metadata(remote.clone())).await? {
                        Ok(m) => meta = m,
                        Err(e) => {
                            let step = self.remote_problem(format!("não foi possível ler: {e}")).await;
                            if !self.record(shown, step) {
                                return None;
                            }
                            continue;
                        }
                    }
                }
                let kind = match meta.file_type() {
                    FileType::Dir => Kind::Dir,
                    FileType::File => Kind::File {
                        size: meta.size.unwrap_or(0),
                        mtime: meta.mtime,
                    },
                    FileType::Symlink => match self.ask(sftp.metadata(remote.clone())).await? {
                        Ok(m) if m.file_type() == FileType::File => Kind::File {
                            size: m.size.unwrap_or(0),
                            mtime: m.mtime,
                        },
                        Ok(m) if m.file_type() == FileType::Dir => {
                            self.note_skipped(shown, R_DIR_LINK);
                            continue;
                        }
                        Ok(_) => {
                            self.note_skipped(shown, R_SPECIAL);
                            continue;
                        }
                        Err(_) => {
                            self.note_skipped(shown, R_BROKEN_LINK);
                            continue;
                        }
                    },
                    FileType::Other => {
                        self.note_skipped(shown, R_SPECIAL);
                        continue;
                    }
                };
                let mut rel = dir.rel.clone();
                rel.push(local);
                if rel.last() != Some(&name) {
                    self.note_renamed(shown.clone(), rel.join("\\"));
                }
                if matches!(kind, Kind::Dir) {
                    let sub = Pending {
                        remote: remote.clone(),
                        rel: rel.clone(),
                        shown: shown.clone(),
                        depth: dir.depth + 1,
                        replace: dir.replace,
                    };
                    self.bytes += sub.cost();
                    subdirs.push(sub);
                }
                let line = Entry {
                    remote,
                    rel,
                    shown,
                    kind,
                    replace: dir.replace,
                };
                self.bytes += line.cost();
                plan.push(line);
            }
            // Ordem inversa na pilha: a primeira subpasta sai primeiro.
            stack.extend(subdirs.into_iter().rev());
        }
        Some(())
    }

    /// Execucao do plano, na ordem. Uma pasta que falha leva junto o seu
    /// conteudo (sem uma mensagem por arquivo).
    async fn copy(&mut self, plan: &[Entry]) {
        let count = plan
            .iter()
            .filter(|e| matches!(e.kind, Kind::File { .. }))
            .count();
        let total: u64 = plan
            .iter()
            .map(|e| match e.kind {
                Kind::File { size, .. } => size,
                Kind::Dir => 0,
            })
            .sum();
        self.report.files = count;
        let mut index = 0;
        let mut done = 0u64;
        let mut failed_dirs = FailedDirs::default();
        for e in plan {
            if self.cancelled() {
                self.report.cancelled = true;
                return;
            }
            let size = match e.kind {
                Kind::File { size, .. } => size,
                Kind::Dir => 0,
            };
            let skip = failed_dirs.skip(&e.rel, matches!(e.kind, Kind::Dir));
            let target = if skip { None } else { local_target(self.dest, &e.rel) };
            let Some(target) = target else {
                if !skip {
                    // Nao deveria ocorrer: os nomes ja sao seguros.
                    self.report.failed.push((e.shown.clone(), R_INVALID.into()));
                    if matches!(e.kind, Kind::Dir) {
                        failed_dirs.add(&e.rel);
                    }
                }
                if matches!(e.kind, Kind::File { .. }) {
                    index += 1;
                    done += size;
                }
                continue;
            };
            match e.kind {
                Kind::Dir => match ensure_dir(&target, e.replace) {
                    Ok(()) => self.report.dirs += 1,
                    Err(err) => match self.local_problem(err) {
                        Step::Failed(m) => {
                            let msg = format!("{m} (conteúdo não baixado)");
                            self.report.failed.push((e.shown.clone(), msg));
                            failed_dirs.add(&e.rel);
                        }
                        step => {
                            self.record(e.shown.clone(), step);
                            return;
                        }
                    },
                },
                Kind::File { mtime, .. } => {
                    let prog = Prog {
                        index,
                        count,
                        done,
                        total,
                    };
                    index += 1;
                    match self.fetch(e, &target, mtime, prog).await {
                        Ok(()) => {
                            self.report.saved += 1;
                            self.report.last_saved = e.rel.join("\\");
                        }
                        Err(Step::Failed(m)) => self.report.failed.push((e.shown.clone(), m)),
                        Err(step) => {
                            self.record(e.shown.clone(), step);
                            return;
                        }
                    }
                    done += size;
                }
            }
        }
    }

    /// Baixa um arquivo: temporario na pasta final, copia em blocos (o
    /// cancelamento vale mesmo com uma leitura em voo), data de modificacao e,
    /// por fim, o nome definitivo. Nunca ha arquivo pela metade com o nome final.
    async fn fetch(&mut self, e: &Entry, target: &Path, mtime: Option<u32>, p: Prog) -> Result<(), Step> {
        let (Some(dir), Some(local)) = (target.parent(), e.rel.last()) else {
            return Err(Step::Failed(R_INVALID.into()));
        };
        // O guard vem antes do arquivo: e solto depois dele e apaga o resto.
        let mut guard = TempFile(None);
        let mut out = None;
        for _ in 0..3 {
            // Mesma pasta do destino (mesmo volume): o rename final e atomico.
            let tmp = dir.join(temp_name(local));
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .await
            {
                Ok(f) => {
                    guard.0 = Some(tmp);
                    out = Some(f);
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(err) => return Err(self.local_problem(LocalErr::Io(err))),
            }
        }
        let Some(mut out) = out else {
            return Err(Step::Failed(R_NO_TEMP.into()));
        };
        let mut src = match unless_cancelled(&mut self.cancel, self.sftp.open(e.remote.clone())).await {
            None => return Err(Step::Cancelled),
            Some(Ok(f)) => f,
            Some(Err(err)) => return Err(self.remote_problem(format!("não foi possível ler: {err}")).await),
        };
        // O primeiro arquivo sempre avisa (a UI sai de "preparando").
        if self.due(p.index == 0) {
            let _ = self.tx.send(DownloadEvent::Progress {
                id: self.id,
                index: p.index,
                count: p.count,
                name: e.shown.clone(),
                done: p.done.min(p.total),
                total: p.total,
            });
        }
        let mut buf = vec![0u8; CHUNK];
        let mut got = 0u64;
        loop {
            let read = unless_cancelled(&mut self.cancel, src.read(&mut buf)).await;
            let n = match read {
                None => return Err(Step::Cancelled),
                Some(Ok(n)) => n,
                Some(Err(err)) => {
                    let msg = format!("erro ao ler do servidor: {err}");
                    return Err(self.remote_problem(msg).await);
                }
            };
            if n == 0 {
                break;
            }
            if let Err(err) = out.write_all(&buf[..n]).await {
                return Err(self.local_problem(LocalErr::Io(err)));
            }
            got += n as u64;
            if self.due(false) {
                let _ = self.tx.send(DownloadEvent::Progress {
                    id: self.id,
                    index: p.index,
                    count: p.count,
                    name: e.shown.clone(),
                    done: (p.done + got).min(p.total),
                    total: p.total,
                });
            }
        }
        // Fecha o handle remoto sem esperar a resposta (close_nowait).
        drop(src);
        if let Err(err) = out.flush().await {
            return Err(self.local_problem(LocalErr::Io(err)));
        }
        let f = out.into_std().await;
        if let Some(t) = mtime.filter(|t| *t > 0) {
            // Falha na data nao derruba o arquivo.
            let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(t.into()));
        }
        // Fechado antes do rename.
        drop(f);
        let Some(tmp) = guard.0.as_deref() else {
            return Err(Step::Failed(R_NO_TEMP.into()));
        };
        place(tmp, target, e.replace).map_err(|err| self.local_problem(err))?;
        guard.keep();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ok(s: &str) -> String {
        local_name(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Pasta temporaria exclusiva do teste (removida pelo `Drop`).
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("sagu-dl-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            TestDir(p)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn local_name_keeps_ordinary_names() {
        for n in ["relatorio final.pdf", ".bashrc", "ação.txt", "日本語.txt", "x .txt", "a~b.txt"] {
            assert_eq!(ok(n), n);
        }
    }

    #[test]
    fn local_name_replaces_forbidden_and_control_chars() {
        assert_eq!(ok("a:b.txt"), "a_b.txt");
        assert_eq!(ok("q?.txt"), "q_.txt");
        assert_eq!(ok("x\\y"), "x_y");
        assert_eq!(ok("<>|*\""), "_____");
        assert_eq!(ok("a\u{7}b"), "a_b");
        assert_eq!(ok("a\u{7f}b\u{85}c"), "a_b_c");
        assert_eq!(ok("foto\u{202E}gpj.exe"), "foto_gpj.exe");
        assert_eq!(ok("x\u{200F}\u{2066}y"), "x__y");
    }

    #[test]
    fn local_name_fixes_trailing_dots_and_spaces() {
        assert_eq!(ok("a."), "a_");
        assert_eq!(ok("a. "), "a__");
        assert_eq!(ok("..."), "___");
        assert_eq!(ok(".. "), "___");
        assert_eq!(ok(" "), "_");
        assert_eq!(ok("nota.txt "), "nota.txt_");
    }

    #[test]
    fn local_name_prefixes_reserved_device_names() {
        for (a, b) in [
            ("CON", "_CON"),
            ("con.txt", "_con.txt"),
            ("NUL.tar.gz", "_NUL.tar.gz"),
            ("COM1", "_COM1"),
            ("LPT9.log", "_LPT9.log"),
            ("COM\u{00B9}", "_COM\u{00B9}"),
            ("CONOUT$", "_CONOUT$"),
            ("conin$.x", "_conin$.x"),
            ("CON .txt", "_CON .txt"),
            ("prn", "_prn"),
            // O ponto final vira '_' antes: "aux_" nao e reservado.
            ("aux.", "aux_"),
        ] {
            assert_eq!(ok(a), b, "{a:?}");
        }
        for n in ["CONSOLE", "COM10", "aux_", "LPT", "nul-x.txt"] {
            assert_eq!(ok(n), n, "{n:?}");
        }
    }

    #[test]
    fn local_name_breaks_short_name_aliases() {
        assert_eq!(ok("PROGRA~1"), "PROGRA_1");
        assert_eq!(ok("LONGNA~1.TXT"), "LONGNA_1.TXT");
        assert_eq!(ok("backup~1.tar"), "backup_1.tar");
        assert_eq!(ok("~1"), "_1");
        for n in ["a~b.txt", "arquivo-longo~1.txt", "a~1.tar.gz", "AB~1.HTML"] {
            assert_eq!(ok(n), n, "{n:?}");
        }
    }

    #[test]
    fn local_name_rejects_unrepresentable() {
        for n in ["", ".", "..", "a/b", "a\0b", "/"] {
            assert_eq!(local_name(n), Err(R_INVALID), "{n:?}");
        }
        assert_eq!(local_name("caf\u{FFFD}.txt"), Err(R_BAD_ENCODING));
    }

    #[test]
    fn local_name_limits_length_to_255_utf16() {
        let a255 = "a".repeat(255);
        assert_eq!(ok(&a255), a255);
        let long = format!("CON.{}", "x".repeat(251));
        let out = ok(&long);
        assert_eq!(utf16_len(&out), 255, "{out}");
        assert!(out.starts_with("_CON."));
        assert!(is_safe_component(&out));
        // Extensao curta preservada ao cortar.
        let out = ok(&format!("nul.{}.txt", "y".repeat(247)));
        assert_eq!(utf16_len(&out), 255);
        assert!(out.starts_with("_nul.") && out.ends_with("y.txt"), "{out}");
        // Surrogates (2 unidades cada) nao sao partidos ao meio.
        let emoji = format!("CON{}", "\u{1F600}".repeat(126));
        let out = ok(&emoji);
        assert!(utf16_len(&out) <= 255 && is_safe_component(&out), "{out}");
        // Corte que expoe nome reservado ou espaco no fim continua seguro.
        let tricky = format!("AUX{}q.{}", " ".repeat(250), "z".repeat(20));
        let out = ok(&tricky);
        assert!(is_safe_component(&out), "{out:?}");
    }

    #[test]
    fn sanitized_names_are_single_normal_components() {
        let hostile = [
            "C:", "C:\\x", "\\\\srv\\share", "/etc/passwd", "..\\..\\x", "a:stream", "NUL", "CON.",
            "con .", "COM1:", "~1", "\u{202E}exe.txt", "\\\\?\\C:\\x", "...", "a\\..", " .. ", "c:..",
            "PRN.txt:ads", "\u{2066}..\u{2069}", "LPT\u{00B3}", "x\r\ny",
        ];
        for h in hostile {
            if let Ok(n) = local_name(h) {
                assert!(is_safe_component(&n), "{h:?} -> {n:?}");
                let comps: Vec<_> = Path::new(&n).components().collect();
                assert_eq!(comps, vec![Component::Normal(OsStr::new(&n))], "{h:?} -> {n:?}");
            }
        }
        // Os inseguros sao reconhecidos como tal.
        for bad in ["", ".", "..", "a.", "a ", "CON", "con.txt", "PROGRA~1", "a:b", "a\\b", "a/b", "x\u{202E}"] {
            assert!(!is_safe_component(bad), "{bad:?}");
        }
    }

    #[test]
    fn local_target_never_leaves_root() {
        let root = std::env::temp_dir().join("sagu-root");
        let rel = strs(&["pasta", "sub", "a.txt"]);
        let p = local_target(&root, &rel).unwrap();
        assert!(p.starts_with(&root));
        assert_eq!(p.components().count(), root.components().count() + 3);
        assert_eq!(p, root.join("pasta").join("sub").join("a.txt"));
        for bad in [
            strs(&[".."]),
            strs(&["C:"]),
            strs(&["a\\b"]),
            strs(&[""]),
            strs(&["ok", ".."]),
            strs(&["ok", "C:\\Windows"]),
            strs(&["\\\\srv\\x"]),
            vec![],
        ] {
            assert_eq!(local_target(&root, &bad), None, "{bad:?}");
        }
    }

    #[test]
    fn unique_local_names_dedupes_case_insensitively() {
        let u = |v: &[&str]| -> Vec<String> {
            unique_local_names(&strs(v)).into_iter().map(|r| r.unwrap()).collect()
        };
        assert_eq!(u(&["A.TXT", "a.txt"]), strs(&["A.TXT", "a (2).txt"]));
        assert_eq!(u(&["a:b", "a_b"]), strs(&["a_b", "a_b (2)"]));
        assert_eq!(u(&["fim ", "fim."]), strs(&["fim_", "fim_ (2)"]));
        assert_eq!(u(&["Ä.txt", "ä.txt"]), strs(&["Ä.txt", "ä (2).txt"]));
        assert_eq!(u(&["STRASSE", "straße"]), strs(&["STRASSE", "straße"]));
        assert_eq!(u(&[".bashrc", ".BASHRC"]), strs(&[".bashrc", ".BASHRC (2)"]));
        // Invalidos nao ocupam nome.
        let r = unique_local_names(&strs(&["..", "x"]));
        assert_eq!(r, vec![Err(R_INVALID), Ok("x".to_string())]);
    }

    #[test]
    fn unique_local_names_is_deterministic() {
        let input = strs(&["X", "x", "x (2)"]);
        let a = unique_local_names(&input);
        assert_eq!(a, unique_local_names(&input));
        let a: Vec<String> = a.into_iter().map(|r| r.unwrap()).collect();
        assert_eq!(a, strs(&["X", "x (2)", "x (2) (2)"]));
        // Muitos nomes que viram o mesmo: todos unicos e seguros.
        let many: Vec<String> = (0..300).map(|i| format!("a{}", char::from_u32(0x80 + (i % 32)).unwrap())).collect();
        let mut many = many;
        many.extend((0..300).map(|i| format!("a?{i}")));
        many.sort();
        let out: Vec<String> = unique_local_names(&many).into_iter().map(|r| r.unwrap()).collect();
        let keys: HashSet<String> = out.iter().map(|s| fold_key(s)).collect();
        assert_eq!(keys.len(), out.len());
        assert!(out.iter().all(|s| is_safe_component(s)));
    }

    #[test]
    fn temp_name_is_short_safe_and_distinct() {
        let local = "n".repeat(255);
        let a = temp_name(&local);
        let b = temp_name(&local);
        assert!(utf16_len(&a) <= 120, "{a}");
        assert!(is_safe_component(&a), "{a}");
        assert!(a.starts_with('.') && a.ends_with(TEMP_SUFFIX), "{a}");
        assert_ne!(a, b);
        // Nome reservado ou com surrogates continua seguro no temporario.
        assert!(is_safe_component(&temp_name("_CON")));
        assert!(is_safe_component(&temp_name(&"\u{1F600}".repeat(127))));
    }

    fn pick(name: &str) -> Pick {
        Pick {
            remote: format!("/srv/{name}"),
            name: name.into(),
        }
    }

    #[test]
    fn prepare_marks_existing_names_as_conflicts() {
        let d = TestDir::new();
        std::fs::write(d.0.join("a.txt"), "x").unwrap();
        std::fs::create_dir(d.0.join("fotos")).unwrap();
        let picks = vec![pick("a.txt"), pick("fotos"), pick("b.txt"), pick("A.TXT"), pick(".."), pick("c:d")];
        let p = prepare(&d.0, &picks);
        let locals: Vec<&str> = p.items.iter().map(|i| i.local.as_str()).collect();
        // Ordem da listagem; "A.TXT" vem antes de "a.txt" na ordem de bytes.
        assert_eq!(locals, vec!["a (2).txt", "fotos", "b.txt", "A.TXT", "c_d"]);
        assert_eq!(p.conflicts, vec![1, 3]);
        assert_eq!(p.invalid, vec![("..".to_string(), R_INVALID.to_string())]);
        assert!(p.items.iter().all(|i| !i.replace));
        assert_eq!(p.items[0].remote, "/srv/a.txt");
        assert_eq!(p.items[0].name, "a.txt");

        // "a.txt" sozinho conflita com o existente.
        let p = prepare(&d.0, &[pick("a.txt"), pick("A.txt.bak")]);
        assert_eq!(p.conflicts, vec![0]);
    }

    #[test]
    fn resolve_conflict_choices() {
        let d = TestDir::new();
        std::fs::write(d.0.join("a.txt"), "x").unwrap();
        let p = prepare(&d.0, &[pick("a.txt"), pick("b.txt")]);
        assert_eq!(p.conflicts, vec![0]);

        let (items, skipped) = resolve(p.clone(), ConflictChoice::Replace);
        assert_eq!(items.len(), 2);
        assert!(items[0].replace && !items[1].replace);
        assert!(skipped.is_empty());

        let (items, skipped) = resolve(p.clone(), ConflictChoice::Skip);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].local, "b.txt");
        assert_eq!(skipped, vec![("a.txt".to_string(), R_SKIPPED_EXISTING.to_string())]);

        let (items, skipped) = resolve(p, ConflictChoice::Cancel);
        assert!(items.is_empty() && skipped.is_empty());

        let all = prepare(&d.0, &[pick("a.txt")]);
        let (items, skipped) = resolve(all, ConflictChoice::Skip);
        assert!(items.is_empty());
        assert_eq!(skipped.len(), 1);
    }

    #[test]
    fn place_file_respects_replace_flag() {
        let d = TestDir::new();
        let target = d.0.join("a.txt");
        let tmp = |content: &str| {
            let t = d.0.join(temp_name("a.txt"));
            std::fs::write(&t, content).unwrap();
            TempFile(Some(t))
        };

        // Destino ausente: rename.
        let mut t = tmp("um");
        place(t.0.as_deref().unwrap(), &target, false).unwrap();
        t.keep();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "um");

        // Existe e sem replace: erro, original intacto, temporario removido.
        let t = tmp("dois");
        let path = t.0.clone().unwrap();
        match place(&path, &target, false) {
            Err(LocalErr::Msg(m)) => assert_eq!(m, R_EXISTS),
            other => panic!("{other:?}"),
        }
        drop(t);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "um");

        // Com replace: conteudo novo.
        let mut t = tmp("tres");
        place(t.0.as_deref().unwrap(), &target, true).unwrap();
        t.keep();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "tres");

        // Pasta no lugar: nunca substituida, mesmo com replace.
        std::fs::create_dir(d.0.join("pasta")).unwrap();
        let t = tmp("quatro");
        match place(t.0.as_deref().unwrap(), &d.0.join("pasta"), true) {
            Err(LocalErr::Msg(m)) => assert_eq!(m, R_DIR_EXISTS),
            other => panic!("{other:?}"),
        }
        assert!(d.0.join("pasta").is_dir());

        // Somente leitura: mensagem clara, original intacto.
        let ro = d.0.join("ro.txt");
        std::fs::write(&ro, "original").unwrap();
        let orig = std::fs::metadata(&ro).unwrap().permissions();
        let mut perm = orig.clone();
        perm.set_readonly(true);
        std::fs::set_permissions(&ro, perm).unwrap();
        let t = tmp("novo");
        let r = place(t.0.as_deref().unwrap(), &ro, true);
        let _ = std::fs::set_permissions(&ro, orig);
        if cfg!(windows) {
            match r {
                Err(LocalErr::Msg(m)) => assert_eq!(m, R_IN_USE),
                other => panic!("{other:?}"),
            }
            assert_eq!(std::fs::read_to_string(&ro).unwrap(), "original");
        }
    }

    /// Cria uma juncao (ou link de pasta) em `link` apontando para `to`;
    /// `false` se o sistema nao deixar.
    fn make_dir_link(link: &Path, to: &Path) -> bool {
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_dir(to, link).is_ok() {
                return true;
            }
            std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(to)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
                && link.exists()
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(to, link).is_ok()
        }
    }

    #[test]
    fn ensure_dir_merges_only_real_directories() {
        let d = TestDir::new();
        let p = d.0.join("nova");
        ensure_dir(&p, false).unwrap();
        assert!(p.is_dir());
        // Pasta real existente: so com replace (mescla).
        ensure_dir(&p, true).unwrap();
        match ensure_dir(&p, false) {
            Err(LocalErr::Msg(m)) => assert_eq!(m, R_EXISTS),
            other => panic!("{other:?}"),
        }
        // Arquivo no lugar: recusado.
        std::fs::write(d.0.join("arq"), "x").unwrap();
        match ensure_dir(&d.0.join("arq"), true) {
            Err(LocalErr::Msg(m)) => assert_eq!(m, R_FILE_EXISTS),
            other => panic!("{other:?}"),
        }
        // Link/juncao: nunca seguido, mesmo com replace.
        let fora = d.0.join("fora");
        std::fs::create_dir(&fora).unwrap();
        let link = d.0.join("atalho");
        if make_dir_link(&link, &fora) {
            match ensure_dir(&link, true) {
                Err(LocalErr::Msg(m)) => assert_eq!(m, R_DIR_IS_LINK),
                other => panic!("{other:?}"),
            }
            let t = d.0.join(temp_name("atalho"));
            std::fs::write(&t, "x").unwrap();
            match place(&t, &link, true) {
                Err(LocalErr::Msg(m)) => assert_eq!(m, R_TARGET_IS_LINK),
                other => panic!("{other:?}"),
            }
            let _ = std::fs::remove_file(&t);
            assert_eq!(std::fs::read_dir(&fora).unwrap().count(), 0);
            // Remove o link sem tocar no alvo.
            let _ = std::fs::remove_dir(&link);
        } else {
            eprintln!("sem permissao para criar link/juncao: caso pulado");
        }
    }

    #[test]
    fn temp_guard_removes_partial_file() {
        let d = TestDir::new();
        let a = d.0.join(temp_name("a"));
        std::fs::write(&a, "parcial").unwrap();
        drop(TempFile(Some(a.clone())));
        assert!(!a.exists());

        let b = d.0.join(temp_name("b"));
        std::fs::write(&b, "completo").unwrap();
        let mut g = TempFile(Some(b.clone()));
        g.keep();
        drop(g);
        assert!(b.exists());
    }

    #[test]
    fn finish_guard_always_reports_once() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let report = DownloadReport {
            id: 3,
            saved: 2,
            files: 5,
            ..Default::default()
        };
        // Fim normal: um `Finished` com o relatorio como esta.
        let g = FinishGuard {
            tx: &tx,
            report: report.clone(),
            sent: false,
        };
        g.send();
        match rx.try_recv() {
            Ok(DownloadEvent::Finished(r)) => {
                assert_eq!((r.id, r.saved, r.files), (3, 2, 5));
                assert!(r.fatal.is_none());
            }
            other => panic!("{other:?}"),
        }
        assert!(rx.try_recv().is_err());
        // Tarefa que cai no meio: o `Drop` avisa, com o que ja foi salvo.
        drop(FinishGuard {
            tx: &tx,
            report,
            sent: false,
        });
        match rx.try_recv() {
            Ok(DownloadEvent::Finished(r)) => {
                assert_eq!((r.id, r.saved), (3, 2));
                assert_eq!(r.fatal.as_deref(), Some(F_INTERNAL));
            }
            other => panic!("{other:?}"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn cancel_pair_signals_and_drop_cancels() {
        let (c, rx) = cancel_pair();
        assert!(!*rx.borrow());
        c.cancel();
        assert!(*rx.borrow());
        let (c, rx) = cancel_pair();
        drop(c);
        assert!(rx.has_changed().is_err());
    }

    #[test]
    fn unless_cancelled_gives_up_on_cancel_or_closed_pane() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let limit = Duration::from_secs(5);
            // Pedido que termina: devolve o resultado.
            let (_c, mut rx) = cancel_pair();
            assert_eq!(unless_cancelled(&mut rx, async { 7 }).await, Some(7));
            // Pedido parado (ex.: listagem enorme): o Cancelar interrompe na hora.
            let (c, mut rx) = cancel_pair();
            let waiting = unless_cancelled(&mut rx, std::future::pending::<()>());
            let later = async {
                tokio::task::yield_now().await;
                c.cancel();
            };
            let (r, ()) = tokio::time::timeout(limit, async { tokio::join!(waiting, later) })
                .await
                .expect("o cancelamento nao interrompeu o pedido");
            assert_eq!(r, None);
            // Ja cancelado: nem comeca.
            assert_eq!(unless_cancelled(&mut rx, async { 1 }).await, None);
            // Painel fechado (remetente solto) com o pedido em voo.
            let (c, mut rx) = cancel_pair();
            let waiting = unless_cancelled(&mut rx, std::future::pending::<()>());
            let later = async move {
                tokio::task::yield_now().await;
                drop(c);
            };
            let (r, ()) = tokio::time::timeout(limit, async { tokio::join!(waiting, later) })
                .await
                .expect("fechar o painel nao interrompeu o pedido");
            assert_eq!(r, None);
        });
    }

    #[test]
    fn failed_dirs_skip_whole_subtree_in_linear_time() {
        // Plano como a varredura monta (cada pasta antes do conteudo): uma
        // pasta que falha com 100.000 subpastas, cada uma com um arquivo.
        let mut plan: Vec<(Vec<String>, bool)> = vec![(strs(&["ruim"]), true)];
        for i in 0..100_000 {
            plan.push((vec!["ruim".into(), format!("d{i}")], true));
            plan.push((vec!["ruim".into(), format!("d{i}"), "f".into()], false));
        }
        plan.push((strs(&["ruim2"]), true));
        plan.push((strs(&["ruim2", "f"]), false));
        let mut failed = FailedDirs::default();
        let mut skipped = 0;
        let mut kept = Vec::new();
        let t0 = Instant::now();
        for (rel, is_dir) in &plan {
            if failed.skip(rel, *is_dir) {
                skipped += 1;
                continue;
            }
            // "ruim" nao pode ser criada; o resto segue.
            if rel == &strs(&["ruim"]) {
                failed.add(rel);
                continue;
            }
            kept.push(rel.join("/"));
        }
        assert_eq!(skipped, 200_000);
        assert_eq!(kept, ["ruim2", "ruim2/f"]);
        // Com a busca por prefixo de antes, isto levava minutos.
        assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
    }

    #[test]
    fn scan_budget_fits_real_trees_and_stops_hostile_ones() {
        let cost = |depth: usize, name_len: usize| {
            let name = "n".repeat(name_len);
            let rel: Vec<String> = (0..depth).map(|_| name.clone()).collect();
            let shown = rel.join("/");
            Entry {
                remote: format!("/home/usuario/{shown}"),
                shown,
                rel,
                kind: Kind::File { size: 1, mtime: None },
                replace: false,
            }
            .cost()
        };
        // Arvore grande de verdade (200.000 entradas a 15 niveis), com folga
        // para as pastas por listar e o relatorio: cabe no teto.
        assert!(MAX_ENTRIES * cost(15, 12) * 5 / 4 < MAX_SCAN_BYTES);
        // Hostil (nomes de 1 KiB em 64 niveis): o teto para a varredura em
        // poucos milhares de entradas, muito antes de MAX_ENTRIES.
        assert!(MAX_SCAN_BYTES / cost(MAX_DEPTH, MAX_REMOTE_NAME) < 5_000);
    }

    #[test]
    fn clip_cuts_long_server_text() {
        assert_eq!(clip("curto", 10), "curto");
        assert_eq!(clip("ação", 4), "ação");
        assert_eq!(clip("ação!", 3), "açã\u{2026}");
        let long = "x".repeat(10_000);
        assert_eq!(clip(&long, MAX_REMOTE_MSG).chars().count(), MAX_REMOTE_MSG + 1);
    }

    // --- Ponta a ponta contra um sshd real (ignorados) ---------------------
    //
    // Mesmas variaveis dos testes de `upload` e `hostkey`: SAGU_E2E_PORT (o
    // sshd com `SetEnv TMUX`, para o .bashrc nao abrir o tmux nos canais
    // exec), SAGU_E2E_USER e SAGU_E2E_KEY. A chave do sshd descartavel e
    // aceita (TOFU). Nenhuma sessao tmux e criada.
    // Rodar com: cargo test e2e_download -- --ignored --test-threads=1

    use crate::hostkey::HostKeyAnswer;
    use crate::sftp::{self, SftpHandle, SftpToUi};
    use crate::vault::{AuthMethod, Host};
    use russh::ChannelMsg;

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"))
    }

    fn e2e_host() -> Host {
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = env("SAGU_E2E_PORT").parse().expect("SAGU_E2E_PORT invalida");
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        host
    }

    /// Roda `script` no servidor com `sh -s` (script no stdin) e exige saida 0.
    fn remote_sh(host: &Host, script: &str) -> Result<(), String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        rt.block_on(async {
            let err = |e: &dyn std::fmt::Display| e.to_string();
            // sshd descartavel de teste: confia na chave (TOFU).
            let session = crate::ssh::connect_and_auth(host, |p| {
                let _ = p.reply.send(HostKeyAnswer::Accept);
            })
            .await
            .map_err(|e| err(&e))?;
            let mut ch = session.channel_open_session().await.map_err(|e| err(&e))?;
            ch.exec(true, "sh -s").await.map_err(|e| err(&e))?;
            ch.data(script.as_bytes()).await.map_err(|e| err(&e))?;
            ch.eof().await.map_err(|e| err(&e))?;
            let mut status = None;
            let mut out = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(60), ch.wait()).await {
                    Err(_) => return Err("tempo esgotado no script remoto".into()),
                    Ok(None) => break,
                    Ok(Some(ChannelMsg::ExitStatus { exit_status })) => status = Some(exit_status),
                    Ok(Some(ChannelMsg::Data { data }))
                    | Ok(Some(ChannelMsg::ExtendedData { data, .. })) => out.extend_from_slice(&data),
                    Ok(Some(_)) => {}
                }
            }
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
            if status == Some(0) {
                Ok(())
            } else {
                Err(format!(
                    "script saiu com {status:?}: {}",
                    String::from_utf8_lossy(&out)
                ))
            }
        })
    }

    /// Arvore de teste no servidor, em `r`.
    fn tree_script(r: &str) -> String {
        format!(
            r#"set -e; R={r}; rm -rf "$R"; mkdir -p "$R/pasta/sub1/sub2" "$R/pasta/vazia" "$R/estranhos"
printf 'conteudo a' > "$R/a.txt"; touch -d '2020-01-02 03:04:05 UTC' "$R/a.txt"
printf x > "$R/pasta/sub1/x.txt"; printf y > "$R/pasta/sub1/sub2/y.txt"
ln -s ../a.txt "$R/pasta/link-arq"; ln -s sub1 "$R/pasta/link-pasta"; ln -s /nao/existe "$R/pasta/quebrado"; mkfifo "$R/pasta/fifo"
cd "$R/estranhos"; for n in '...' '<>|*"' 'A.TXT' 'CON' 'PROGRA~1' 'a.txt' 'a:b.txt' 'fim ' 'fim.' 'nul.txt' 'q?.txt' "$(printf 'rlo\342\200\256gpj.exe')" 'x\y'; do printf '%s' "$n" > "./$n"; done
head -c 67108864 /dev/zero > "$R/grande.bin"
"#
        )
    }

    /// Apaga no fim (inclusive se o teste falhar) a arvore remota e a local.
    struct Cleanup {
        host: Host,
        remote: String,
        local: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if let Err(e) = remote_sh(&self.host, &format!("rm -rf '{}'", self.remote)) {
                eprintln!("limpeza remota falhou: {e}");
            }
            let _ = std::fs::remove_dir_all(&self.local);
        }
    }

    /// Prepara a arvore remota e a pasta local de um teste (`tag` os separa).
    fn e2e_setup(tag: &str) -> (Host, String, PathBuf, Cleanup) {
        let host = e2e_host();
        let r = format!("/tmp/sagu-e2e-dl-{}-{tag}", std::process::id());
        let local = std::env::temp_dir().join(format!("sagu-e2e-dl-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&local);
        std::fs::create_dir_all(&local).unwrap();
        let cleanup = Cleanup {
            host: host.clone(),
            remote: r.clone(),
            local: local.clone(),
        };
        remote_sh(&host, &tree_script(&r)).expect("preparo da arvore remota");
        (host, r, local, cleanup)
    }

    /// Sessao SFTP pelo caminho normal do app (aceitando a chave do servidor).
    fn sftp_session(host: &Host) -> SftpHandle {
        let h = sftp::connect(host.clone(), || {});
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(20) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::HostKey(p)) => {
                    let _ = p.reply.send(HostKeyAnswer::Accept);
                }
                Ok(SftpToUi::Connected { .. }) => return h,
                Ok(SftpToUi::Error(e)) => panic!("SFTP: {e}"),
                Ok(SftpToUi::Closed) => panic!("SFTP fechou sem conectar"),
                _ => {}
            }
        }
        panic!("SFTP nao conectou");
    }

    fn close(h: SftpHandle) {
        h.disconnect();
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(10) {
            if let Ok(SftpToUi::Closed) = h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                return;
            }
        }
        panic!("SFTP nao fechou");
    }

    fn pick_at(r: &str, name: &str) -> Pick {
        Pick {
            remote: format!("{r}/{name}"),
            name: name.into(),
        }
    }

    /// Baixa pelo caminho completo (pedido a sessao, tarefa, eventos) ate o
    /// `Finished`. `pre_cancel` cancela logo apos o pedido; `on_event` ve os
    /// eventos de andamento e pode cancelar.
    fn run_download(
        h: &SftpHandle,
        id: u64,
        dest: &Path,
        items: Vec<DownloadItem>,
        pre_cancel: bool,
        mut on_event: impl FnMut(&DownloadEvent, &Cancel),
    ) -> (DownloadReport, Vec<DownloadEvent>) {
        let cancel = h
            .download(id, dest.to_path_buf(), items)
            .expect("sessao encerrada");
        if pre_cancel {
            cancel.cancel();
        }
        let t0 = Instant::now();
        let mut events = Vec::new();
        while t0.elapsed() < Duration::from_secs(30) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Download(DownloadEvent::Finished(r))) => {
                    assert_eq!(r.id, id);
                    return (*r, events);
                }
                Ok(SftpToUi::Download(ev)) => {
                    on_event(&ev, &cancel);
                    events.push(ev);
                }
                Ok(SftpToUi::Error(e)) => panic!("erro da sessao: {e}"),
                Ok(SftpToUi::Closed) => panic!("sessao fechou no meio do download"),
                _ => {}
            }
        }
        panic!("tempo esgotado esperando o fim do download");
    }

    /// Tudo o que ha abaixo de `dir` (sem seguir links).
    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if std::fs::symlink_metadata(&p).unwrap().is_dir() {
                    stack.push(p.clone());
                }
                out.push(p);
            }
        }
        out
    }

    fn no_temp_files(dir: &Path) -> bool {
        walk(dir).iter().all(|p| {
            !p.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with(TEMP_SUFFIX)
        })
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
    }

    fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
        v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    #[ignore]
    fn e2e_download_files_and_folders() {
        let (host, r, local, _cleanup) = e2e_setup("arvore");
        let dest = local.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let h = sftp_session(&host);

        let p = prepare(&dest, &[pick_at(&r, "a.txt"), pick_at(&r, "pasta")]);
        assert!(p.conflicts.is_empty() && p.invalid.is_empty());
        let t0 = Instant::now();
        let (rep, events) = run_download(&h, 1, &dest, p.items, false, |_, _| {});
        // O FIFO nunca e aberto (a leitura travaria).
        assert!(t0.elapsed() < Duration::from_secs(20), "demorou {:?}", t0.elapsed());
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert!(rep.fatal.is_none() && !rep.cancelled, "{rep:?}");
        assert_eq!((rep.saved, rep.files, rep.dirs), (4, 4, 4), "{rep:?}");
        assert_eq!(rep.last_saved, "pasta\\sub1\\sub2\\y.txt");
        let mut skipped = rep.skipped.clone();
        skipped.sort();
        assert_eq!(
            skipped,
            pairs(&[
                ("pasta/fifo", R_SPECIAL),
                ("pasta/link-pasta", R_DIR_LINK),
                ("pasta/quebrado", R_BROKEN_LINK),
            ])
        );
        assert!(rep.renamed.is_empty(), "{:?}", rep.renamed);

        assert_eq!(read(&dest.join("a.txt")), "conteudo a");
        let mtime = std::fs::metadata(dest.join("a.txt")).unwrap().modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_577_934_245));
        let pasta = dest.join("pasta");
        assert_eq!(read(&pasta.join("sub1").join("x.txt")), "x");
        assert_eq!(read(&pasta.join("sub1").join("sub2").join("y.txt")), "y");
        // Link para arquivo: vira arquivo comum com o conteudo do alvo.
        assert!(std::fs::symlink_metadata(pasta.join("link-arq")).unwrap().is_file());
        assert_eq!(read(&pasta.join("link-arq")), "conteudo a");
        assert!(pasta.join("vazia").is_dir());
        assert_eq!(std::fs::read_dir(pasta.join("vazia")).unwrap().count(), 0);
        for n in ["link-pasta", "quebrado", "fifo"] {
            assert!(std::fs::symlink_metadata(pasta.join(n)).is_err(), "{n} nao deveria existir");
        }
        assert!(no_temp_files(&dest));
        assert!(events
            .iter()
            .any(|e| matches!(e, DownloadEvent::Progress { count: 4, .. })));
        close(h);
    }

    #[test]
    #[ignore]
    fn e2e_download_strange_names() {
        let (host, r, local, _cleanup) = e2e_setup("nomes");
        let dest = local.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let before: Vec<_> = std::fs::read_dir(&local)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let h = sftp_session(&host);

        let p = prepare(&dest, &[pick_at(&r, "estranhos")]);
        let (rep, _) = run_download(&h, 7, &dest, p.items, false, |_, _| {});
        assert!(rep.failed.is_empty() && rep.skipped.is_empty(), "{rep:?}");
        assert_eq!((rep.saved, rep.files), (13, 13), "{rep:?}");
        let mapping = [
            ("...", "___"),
            ("<>|*\"", "_____"),
            ("A.TXT", "A.TXT"),
            ("CON", "_CON"),
            ("PROGRA~1", "PROGRA_1"),
            ("a.txt", "a (2).txt"),
            ("a:b.txt", "a_b.txt"),
            ("fim ", "fim_"),
            ("fim.", "fim_ (2)"),
            ("nul.txt", "_nul.txt"),
            ("q?.txt", "q_.txt"),
            ("rlo\u{202E}gpj.exe", "rlo_gpj.exe"),
            ("x\\y", "x_y"),
        ];
        let dir = dest.join("estranhos");
        for (remoto, local_name) in mapping {
            // O conteudo de cada arquivo e o proprio nome remoto.
            assert_eq!(read(&dir.join(local_name)), remoto, "{remoto:?} -> {local_name:?}");
        }
        let mut got: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        got.sort();
        let mut want: Vec<String> = mapping.iter().map(|(_, l)| l.to_string()).collect();
        want.sort();
        assert_eq!(got, want);
        // Nada fora do destino; a pasta-mae ficou como estava.
        let n = dest.components().count();
        for p in walk(&dest) {
            assert!(p.starts_with(&dest), "{}", p.display());
            assert!(p.components().count() <= n + 2, "{}", p.display());
        }
        let after: Vec<_> = std::fs::read_dir(&local)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
        assert!(!rep.renamed.is_empty());
        assert!(rep
            .renamed
            .contains(&("estranhos/CON".to_string(), "estranhos\\_CON".to_string())));
        assert!(no_temp_files(&dest));
        close(h);
    }

    #[test]
    #[ignore]
    fn e2e_download_conflicts_and_cancel() {
        let (host, r, local, _cleanup) = e2e_setup("conflitos");
        let dest = local.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let h = sftp_session(&host);
        let noop = |_: &DownloadEvent, _: &Cancel| {};
        let a = [pick_at(&r, "a.txt")];

        // Primeira vez: baixa.
        let (rep, _) = run_download(&h, 1, &dest, prepare(&dest, &a).items, false, noop);
        assert_eq!(rep.saved, 1, "{rep:?}");
        assert_eq!(read(&dest.join("a.txt")), "conteudo a");

        // O arquivo muda no servidor: o planejamento acusa o conflito.
        remote_sh(&host, &format!("printf 'versao 2' > {r}/a.txt")).unwrap();
        let p = prepare(&dest, &a);
        assert_eq!(p.conflicts, vec![0]);

        // Sem autorizacao (replace=false), o backend confere de novo e nao troca.
        let (rep, _) = run_download(&h, 2, &dest, p.items.clone(), false, noop);
        assert_eq!(rep.saved, 0);
        assert_eq!(rep.failed, pairs(&[("a.txt", R_EXISTS)]));
        assert_eq!(read(&dest.join("a.txt")), "conteudo a");
        assert!(no_temp_files(&dest));

        // Substituir: troca.
        let (items, _) = resolve(p, ConflictChoice::Replace);
        let (rep, _) = run_download(&h, 3, &dest, items, false, noop);
        assert!(rep.failed.is_empty(), "{rep:?}");
        assert_eq!(read(&dest.join("a.txt")), "versao 2");

        // Mescla de pasta: arquivo local extra fica, o de mesmo nome e trocado.
        let pasta = [pick_at(&r, "pasta")];
        let (rep, _) = run_download(&h, 4, &dest, prepare(&dest, &pasta).items, false, noop);
        assert_eq!(rep.saved, 3, "{rep:?}");
        std::fs::write(dest.join("pasta").join("local-extra.txt"), "extra").unwrap();
        remote_sh(&host, &format!("printf x2 > {r}/pasta/sub1/x.txt")).unwrap();
        let p = prepare(&dest, &pasta);
        assert_eq!(p.conflicts, vec![0]);
        let (items, _) = resolve(p, ConflictChoice::Replace);
        let (rep, _) = run_download(&h, 5, &dest, items, false, noop);
        assert!(rep.failed.is_empty(), "{rep:?}");
        assert_eq!(rep.saved, 3);
        assert_eq!(read(&dest.join("pasta").join("sub1").join("x.txt")), "x2");
        assert_eq!(read(&dest.join("pasta").join("local-extra.txt")), "extra");

        // Arquivo remoto sobre uma pasta local de mesmo nome: falha, nada muda.
        let dest2 = local.join("dest2");
        std::fs::create_dir_all(dest2.join("a.txt")).unwrap();
        std::fs::write(dest2.join("a.txt").join("dentro.txt"), "meu").unwrap();
        let p = prepare(&dest2, &a);
        assert_eq!(p.conflicts, vec![0]);
        let (items, _) = resolve(p, ConflictChoice::Replace);
        let (rep, _) = run_download(&h, 6, &dest2, items, false, noop);
        assert_eq!(rep.failed, pairs(&[("a.txt", R_DIR_EXISTS)]));
        assert_eq!(read(&dest2.join("a.txt").join("dentro.txt")), "meu");
        assert!(no_temp_files(&dest2));

        // Cancelado logo depois do pedido.
        let grande = [pick_at(&r, "grande.bin")];
        let (rep, _) = run_download(&h, 7, &dest, prepare(&dest, &grande).items, true, noop);
        assert!(rep.cancelled, "{rep:?}");
        assert_eq!(rep.saved, 0);
        assert!(!dest.join("grande.bin").exists());
        assert!(no_temp_files(&dest));

        // Cancelado no primeiro andamento do arquivo de 64 MiB.
        let mut cancelled_at = None;
        let (rep, events) = run_download(
            &h,
            8,
            &dest,
            prepare(&dest, &grande).items,
            false,
            |ev, c| {
                if let DownloadEvent::Progress { name, .. } = ev {
                    if name == "grande.bin" && cancelled_at.is_none() {
                        c.cancel();
                        cancelled_at = Some(Instant::now());
                    }
                }
            },
        );
        let at = cancelled_at.expect("nenhum andamento de grande.bin");
        assert!(at.elapsed() < Duration::from_secs(5), "cancelamento demorou");
        assert!(rep.cancelled, "{rep:?}");
        assert_eq!((rep.saved, rep.files), (0, 1));
        assert!(events
            .iter()
            .any(|e| matches!(e, DownloadEvent::Progress { total: 67_108_864, .. })));
        assert!(!dest.join("grande.bin").exists());
        assert!(no_temp_files(&dest));
        close(h);
    }
}
