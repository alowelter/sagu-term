//! Colar no navegador SFTP: copiar e mover no mesmo servidor, e copiar entre
//! dois servidores (arrastar de um painel para o outro).
//!
//! A UI guarda o que foi copiado (Ctrl+C) ou recortado (Ctrl+X) e, no Ctrl+V,
//! planeja os conflitos com a listagem do painel de destino (`prepare`) antes
//! de mandar o lote. Aqui fica tambem a tarefa que roda na thread da sessao
//! SFTP, fora do loop de comandos (como o download):
//! - mover: RENAME no servidor, item a item (atomico, preserva tudo). Se o
//!   servidor recusa por serem discos diferentes, o item volta no relatorio
//!   (`cross_device`) e a UI pergunta antes de copiar e apagar a origem;
//! - copiar: le a arvore inteira antes de gravar e passa os dados pelo canal
//!   SFTP (pela memoria do app, nunca pelo disco do Windows), cada arquivo num
//!   temporario na pasta final, renomeado no fim. Sem exec de shell no
//!   servidor: nada depende de shell nem corre risco com nomes estranhos.
//!
//! Entre servidores (`run_from`): a mesma copia, lendo pela sessao do painel
//! de origem (emprestada, ver `sftp::SessionRef`) e gravando pela do destino,
//! numa unica tarefa na thread do destino. So copia (nunca move), nunca
//! preserva dono/grupo (os ids nao valem no outro servidor) e nao ha nome
//! "(cópia)" automatico nem conferencia de "para dentro de si mesma" (os
//! caminhos sao de servidores diferentes). A leitura passa pelo canal
//! principal do painel de origem: uma copia longa divide esse canal com a
//! navegacao dele, e um pedido preso la (stat em NFS parado) segura a copia
//! ate o prazo do crate. Se a sessao de origem acaba no meio, o lote para
//! com "conexão com o servidor de origem perdida".
//!
//! Garantias: nada e substituido sem `replace`, e a substituicao troca pelo
//! nome com um backup (nunca ha momento sem o original); links simbolicos
//! nunca sao seguidos (sao copiados como links); arquivos especiais nunca sao
//! abertos; os limites da varredura sao os do download.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use russh_sftp::client::error::Error as SftpError;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::watch;

use crate::download::{
    clip, close_remote, text_cost, unless_cancelled, ConflictChoice, CHUNK, F_CONNECTION, F_DEST_GONE,
    MAX_DEPTH, MAX_ENTRIES, MAX_REMOTE_MSG, MAX_REMOTE_NAME, MAX_SCAN_BYTES, PROBE_TIMEOUT,
    PROGRESS_EVERY, R_BAD_ENCODING, R_DIR_EXISTS, R_EXISTS, R_FILE_EXISTS, R_INVALID, R_NO_TEMP,
    R_SKIPPED_EXISTING, R_SPECIAL, R_TOO_DEEP, R_TOO_LONG, TEMP_SUFFIX,
};
use crate::sftp::{raw_kind, RawKind};

/// Palavra das copias criadas na propria pasta: "nome (cópia).ext".
pub const COPY_WORD: &str = "cópia";
/// Maior nome aceito pelo Linux (bytes).
const MAX_NAME_BYTES: usize = 255;
/// Maior alvo de link simbolico aceito na copia (bytes).
const MAX_LINK_TARGET: usize = 4096;
/// Tentativas de nome livre para "(cópia N)".
const MAX_COPY_NAMES: u32 = 1000;
/// Falhas de gravacao seguidas que interrompem o lote (disco cheio e cota
/// esgotada chegam do OpenSSH so como "Failure").
const MAX_WRITE_FAILS: u32 = 3;
/// Sufixos dos nomes laterais: backup da substituicao e sonda de links.
const OLD_SUFFIX: &str = ".sagu-old";
const LINK_PROBE_SUFFIX: &str = ".sagu-lnk";

// Motivos por item (textos para o usuario).
const R_SRC_GONE: &str = "não existe mais na origem";
const R_DEST_GONE: &str = "a pasta de destino não existe mais";
const R_DEST_LINK: &str = "o destino é um link simbólico; nada foi alterado";
const R_NO_READ: &str = "sem permissão para ler";
const R_NO_WRITE: &str = "sem permissão para gravar no destino";
const R_NO_MOVE: &str = "sem permissão para mover (na origem ou no destino)";
pub const R_INTO_ITSELF: &str = "a pasta de destino fica dentro da pasta de origem";
const R_SAME_DIR: &str = "já está nesta pasta";
const R_MOUNT: &str = "é um ponto de montagem; não foi movido";
const R_CHANGED: &str = "mudou de tipo durante a cópia";
const R_LINK_TARGET: &str = "alvo do link inválido ou com codificação não suportada";
const R_NO_LINKS: &str = "o servidor não criou o link simbólico";
const R_NO_NAME: &str = "não foi possível escolher um nome livre";
const R_PART_MOVED: &str = "parte já foi movida; o restante ficou na origem (discos diferentes)";
const R_NO_TYPE: &str = "o servidor não informou o tipo";

// Motivos que interrompem o lote inteiro.
const F_TOO_MANY: &str = "a seleção tem mais de 200.000 itens; copie partes menores";
const F_TOO_BIG: &str = "a seleção é grande demais; copie partes menores";
const F_WRITE: &str = "o servidor parou de aceitar gravações (disco cheio ou cota esgotada?)";
const F_INTERNAL: &str = "erro interno ao colar";
// Copia entre servidores: qual dos dois caiu.
pub const F_SRC_CONNECTION: &str = "conexão com o servidor de origem perdida";
pub const F_DST_CONNECTION: &str = "conexão com o servidor de destino perdida";

// Origem mantida num "copiar e apagar".
const K_NOT_CLEAN: &str = "nem tudo pôde ser copiado; a origem foi mantida";
const K_NEW_ITEMS: &str = "a pasta de origem tinha itens novos; não foi apagada";

/// O que fazer com os itens colados.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasteOp {
    Copy,
    Move,
    /// Mover entre discos diferentes, depois de o usuario aceitar a oferta.
    CopyThenDelete,
}

#[derive(Clone, Debug)]
pub struct PasteItem {
    /// Caminho remoto completo da origem (da listagem).
    pub src: String,
    /// Ultimo componente (nome remoto original).
    pub name: String,
    /// Usuario autorizou substituir arquivo/link ou mesclar pasta de mesmo nome.
    pub replace: bool,
}

pub struct PasteRequest {
    pub op: PasteOp,
    pub dest_dir: String,
    pub items: Vec<PasteItem>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PastePhase {
    Copying,
    Moving,
    Deleting,
}

#[derive(Debug)]
pub enum PasteEvent {
    /// Varrendo a origem (copia): entradas vistas ate agora (a cada 100 ms).
    Scanning { id: u64, found: usize },
    /// Copying: arquivo `index` (0-based) de `count`; bytes somados do lote.
    /// Moving/Deleting: item `index` de `count` (bytes zerados).
    Progress {
        id: u64,
        phase: PastePhase,
        index: usize,
        count: usize,
        name: String,
        done: u64,
        total: u64,
    },
    /// Exatamente um por lote (ver `FinishGuard`).
    Finished(Box<PasteReport>),
}

#[derive(Clone, Debug, Default)]
pub struct PasteReport {
    pub id: u64,
    pub op: Option<PasteOp>,
    /// Itens do pedido e itens concluidos (colocados no destino; no
    /// CopyThenDelete, so quando a origem tambem foi apagada).
    pub count: usize,
    pub done: usize,
    /// Arquivos planejados e colocados com o nome final (copia).
    pub files: usize,
    pub saved: usize,
    /// Nome final do ultimo item concluido.
    pub last: String,
    /// (origem, destino) dos itens que sairam da origem.
    pub moved: Vec<(String, String)>,
    /// (nome, nome novo) das copias criadas na propria pasta.
    pub renamed: Vec<(String, String)>,
    /// (caminho relativo mostrado, motivo).
    pub failed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
    /// CopyThenDelete: (nome, motivo) das origens mantidas.
    pub src_kept: Vec<(String, String)>,
    /// Move: itens que o servidor nao renomeou, provavelmente por discos diferentes.
    pub cross_device: Vec<PasteItem>,
    /// Pastas a re-listar: destino e pais das origens (Move/CopyThenDelete).
    pub refresh: Vec<String>,
    pub cancelled: bool,
    pub fatal: Option<String>,
}

// --- Funcoes puras -------------------------------------------------------------

/// Radical e extensao, separados no ultimo '.' (".bashrc" nao tem extensao).
fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Comeco de `s` com no maximo `max` bytes, sem partir um caractere.
fn take_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    &s[..i]
}

/// Nome da k-esima copia na mesma pasta: "a (cópia).txt", "a (cópia 2).txt".
/// Pasta nao separa extensao. Cabe em 255 bytes cortando o radical.
pub fn copy_name(name: &str, is_dir: bool, k: u32) -> String {
    let suffix = if k <= 1 {
        format!(" ({COPY_WORD})")
    } else {
        format!(" ({COPY_WORD} {k})")
    };
    let (stem, ext) = if is_dir { (name, "") } else { split_ext(name) };
    // Extensao enorme: o sufixo vai no fim do nome.
    let (stem, ext) = if ext.len() > 32 { (name, "") } else { (stem, ext) };
    let budget = MAX_NAME_BYTES.saturating_sub(suffix.len() + ext.len());
    format!("{}{suffix}{ext}", take_bytes(stem, budget))
}

/// Caminho sem '/' no fim (salvo a raiz).
fn trim_slash(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    if t.is_empty() && p.starts_with('/') {
        "/"
    } else {
        t
    }
}

/// `dest` e `src` ou fica dentro dele (comparacao por componentes).
pub fn is_inside(dest: &str, src: &str) -> bool {
    let (d, s) = (trim_slash(dest), trim_slash(src));
    if s == "/" {
        return true;
    }
    d == s || d.strip_prefix(s).is_some_and(|rest| rest.starts_with('/'))
}

/// Mesma pasta (ignorando a '/' do fim).
pub fn same_dir(a: &str, b: &str) -> bool {
    trim_slash(a) == trim_slash(b)
}

/// Pasta que contem `path` ("/" para a raiz e para "/x").
pub fn parent_of(path: &str) -> String {
    let t = trim_slash(path);
    match t.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(i) => t[..i].to_string(),
    }
}

/// Nome remoto que da para pedir de volta ao servidor como componente.
pub fn check_remote_name(name: &str) -> Result<(), &'static str> {
    if name.len() > MAX_REMOTE_NAME {
        return Err(R_TOO_LONG);
    }
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(R_INVALID);
    }
    // Decodificado com perda (nao UTF-8): o caminho nao existe no servidor.
    if name.contains('\u{FFFD}') {
        return Err(R_BAD_ENCODING);
    }
    Ok(())
}

pub fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Nome lateral na pasta de destino (temporario, backup, sonda): comeca com
/// '.', sorteado, sempre com no maximo 255 bytes.
fn side_name(name: &str, suffix: &str) -> String {
    format!(".{}.{:08x}{suffix}", take_bytes(name, 200), rand::random::<u32>())
}

/// Codigo de status SFTP v3 (subconjunto do `StatusCode` do russh-sftp).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Code {
    NoSuchFile,
    PermissionDenied,
    Failure,
    BadMessage,
    OpUnsupported,
    Other,
}

fn code_of(e: &SftpError) -> Code {
    match e {
        SftpError::Status(s) => match s.status_code {
            StatusCode::NoSuchFile => Code::NoSuchFile,
            StatusCode::PermissionDenied => Code::PermissionDenied,
            StatusCode::Failure => Code::Failure,
            StatusCode::BadMessage => Code::BadMessage,
            StatusCode::OpUnsupported => Code::OpUnsupported,
            _ => Code::Other,
        },
        _ => Code::Other,
    }
}

/// Porque um RENAME falhou, depois de conferir origem e destino com lstat.
#[derive(Debug, PartialEq)]
enum RenameFail {
    /// Algo com o nome de destino ja existe (nada foi substituido).
    Exists,
    SrcGone,
    /// A pasta de destino sumiu ou nao e pasta.
    DestGone,
    NoPermission,
    /// Pasta para dentro dela mesma (EINVAL vira BAD_MESSAGE no OpenSSH).
    IntoItself,
    /// Origem ainda la, destino livre e falha generica: quase sempre discos
    /// ou particoes diferentes (EXDEV). A UI oferece copiar e apagar.
    CrossDevice,
    Other,
}

/// `src_exists`/`dst_exists`/`dest_dir_ok`: lstat logo apos a falha
/// (`None` = nao foi possivel conferir: nunca oferece copiar e apagar).
fn classify_rename(
    code: Code,
    src_exists: Option<bool>,
    dst_exists: Option<bool>,
    dest_dir_ok: Option<bool>,
    src_is_dir: bool,
) -> RenameFail {
    if dst_exists == Some(true) {
        return RenameFail::Exists;
    }
    if src_exists == Some(false) {
        return RenameFail::SrcGone;
    }
    if dest_dir_ok == Some(false) {
        return RenameFail::DestGone;
    }
    match code {
        Code::PermissionDenied => RenameFail::NoPermission,
        Code::BadMessage if src_is_dir => RenameFail::IntoItself,
        Code::Failure | Code::OpUnsupported
            if src_exists == Some(true) && dst_exists == Some(false) && dest_dir_ok == Some(true) =>
        {
            RenameFail::CrossDevice
        }
        Code::NoSuchFile => RenameFail::SrcGone,
        _ => RenameFail::Other,
    }
}

/// Permissoes de uma copia: rwx do original; setuid/setgid/sticky nao sao
/// copiados (como o `cp` sem `-p`).
fn copy_mode(src_mode: u32) -> u32 {
    src_mode & 0o777
}

/// Modo provisorio de uma pasta criada na copia: nunca mais aberto que o
/// original, mas com rwx do dono para gravar o conteudo.
fn provisional_dir_mode(src_mode: u32) -> u32 {
    (src_mode & 0o777) | 0o700
}

/// Atributos aplicados a uma copia pronta: permissoes, datas (o atime vai
/// junto do mtime no protocolo; sem mtime, nada de datas) e, conectado como
/// root, o dono e o grupo do original (os dois juntos).
fn copy_attrs(mode: u32, atime: Option<u32>, mtime: Option<u32>, owner: Option<(u32, u32)>) -> FileAttributes {
    let mut a = FileAttributes::empty();
    a.permissions = Some(copy_mode(mode));
    if let Some(m) = mtime {
        a.mtime = Some(m);
        a.atime = Some(atime.unwrap_or(m));
    }
    if let Some((uid, gid)) = owner {
        a.uid = Some(uid);
        a.gid = Some(gid);
    }
    a
}

// --- Planejamento na UI ---------------------------------------------------------

/// Item da area de transferencia visto pelo planejamento.
pub struct Source<'a> {
    pub path: &'a str,
    pub name: &'a str,
    /// Pasta de verdade (um link para pasta nao conta).
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct Prepared {
    pub op: PasteOp,
    pub dest_dir: String,
    /// Validos, na ordem da origem, todos com `replace = false`.
    pub items: Vec<PasteItem>,
    /// Indices em `items` cujo nome ja existe no destino.
    pub conflicts: Vec<usize>,
    /// Conflitos em que um e pasta e o outro nao (o servidor sempre os pula).
    pub mismatched: usize,
    /// (nome, motivo) tirados antes de enviar.
    pub invalid: Vec<(String, String)>,
}

/// Separa os validos e os que ja existem no destino. `dest`: listagem do
/// painel de destino, nome -> e pasta de verdade. Copiar para a propria pasta
/// nunca conflita (o servidor escolhe o nome "(cópia)"). `cross`: origem e
/// destino em servidores diferentes; caminhos iguais nao sao a mesma pasta
/// nem uma pasta "dentro de si mesma" (so os nomes ainda conflitam).
pub fn prepare(
    op: PasteOp,
    src_dir: &str,
    dest_dir: &str,
    sources: &[Source],
    dest: &HashMap<&str, bool>,
    cross: bool,
) -> Prepared {
    let mut p = Prepared {
        op,
        dest_dir: dest_dir.to_string(),
        items: Vec::new(),
        conflicts: Vec::new(),
        mismatched: 0,
        invalid: Vec::new(),
    };
    let same = !cross && same_dir(src_dir, dest_dir);
    for s in sources {
        if let Err(why) = check_remote_name(s.name) {
            p.invalid.push((s.name.to_string(), why.to_string()));
            continue;
        }
        if !cross && s.is_dir && is_inside(dest_dir, s.path) {
            p.invalid.push((s.name.to_string(), R_INTO_ITSELF.to_string()));
            continue;
        }
        let idx = p.items.len();
        p.items.push(PasteItem {
            src: s.path.to_string(),
            name: s.name.to_string(),
            replace: false,
        });
        if op == PasteOp::Copy && same {
            continue;
        }
        // O Linux diferencia maiusculas: "A.txt" e "a.txt" nao conflitam.
        if let Some(&d) = dest.get(s.name) {
            p.conflicts.push(idx);
            if d != s.is_dir {
                p.mismatched += 1;
            }
        }
    }
    p
}

/// Resposta do dialogo de conflito: Substituir marca `replace` nos
/// conflitos; Pular os tira (devolvidos como ignorados); Cancelar devolve nada.
pub fn resolve(p: Prepared, choice: ConflictChoice) -> (Vec<PasteItem>, Vec<(String, String)>) {
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
            let conflicts: HashSet<usize> = p.conflicts.into_iter().collect();
            let mut keep = Vec::new();
            let mut skipped = Vec::new();
            for (i, it) in p.items.into_iter().enumerate() {
                if conflicts.contains(&i) {
                    skipped.push((it.name, R_SKIPPED_EXISTING.to_string()));
                } else {
                    keep.push(it);
                }
            }
            (keep, skipped)
        }
    }
}

// --- Links simbolicos -------------------------------------------------------------

/// Ordem dos argumentos do SSH_FXP_SYMLINK neste servidor: o OpenSSH le ao
/// contrario do draft do protocolo. Descoberta por sonda, uma vez por sessao.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymlinkArgs {
    /// OpenSSH: para criar L -> T, pede-se symlink(T, L).
    TargetFirst,
    /// Draft: symlink(L, T).
    LinkFirst,
    /// O servidor nao cria links (ou a sonda nao conseguiu).
    NoLinks,
}

/// Ordem descoberta (compartilhada pelos lotes da sessao).
#[derive(Default)]
pub struct LinkOrder(Mutex<Option<SymlinkArgs>>);

// --- Tarefa -----------------------------------------------------------------------

/// Falha de um passo.
enum Step {
    /// So este item; o lote continua.
    Failed(String),
    /// Interrompe o lote inteiro.
    Fatal(String),
    Cancelled,
}

/// Tipo de uma entrada pelos bits do modo (lstat: nunca segue link).
fn kind_of(attrs: &FileAttributes) -> RawKind {
    raw_kind(attrs.permissions)
}

/// Uma linha do plano de copia.
struct Entry {
    src: String,
    /// Componentes a partir do item copiado (vazio = o proprio item).
    sub: Vec<String>,
    /// Caminho relativo mostrado nas mensagens.
    shown: String,
    kind: Kind,
}

impl Entry {
    fn cost(&self) -> usize {
        std::mem::size_of::<Entry>()
            + text_cost(&self.src)
            + text_cost(&self.shown)
            + self.sub.iter().map(|c| text_cost(c)).sum::<usize>()
            + match &self.kind {
                Kind::Link { target } => text_cost(target),
                _ => 0,
            }
    }
}

enum Kind {
    Dir {
        mode: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        owner: Option<(u32, u32)>,
    },
    File {
        size: u64,
        mode: u32,
        atime: Option<u32>,
        mtime: Option<u32>,
        owner: Option<(u32, u32)>,
    },
    Link {
        target: String,
    },
}

/// Pasta criada na copia: (caminho, modo, atime, mtime, dono), para ajustar
/// permissoes e datas depois do conteudo.
type CreatedDir = (String, u32, Option<u32>, Option<u32>, Option<(u32, u32)>);

/// Plano de um item colado (a entrada 0 e o proprio item).
struct ItemPlan {
    item: usize,
    entries: Vec<Entry>,
}

/// Garante um `Finished` por lote, inclusive se a tarefa cair no meio.
struct FinishGuard<'a> {
    tx: &'a UnboundedSender<PasteEvent>,
    report: PasteReport,
    sent: bool,
}

impl FinishGuard<'_> {
    fn send(mut self) {
        self.sent = true;
        let report = std::mem::take(&mut self.report);
        let _ = self.tx.send(PasteEvent::Finished(Box::new(report)));
    }
}

impl Drop for FinishGuard<'_> {
    fn drop(&mut self) {
        if !self.sent {
            let mut report = std::mem::take(&mut self.report);
            report.fatal.get_or_insert_with(|| F_INTERNAL.into());
            let _ = self.tx.send(PasteEvent::Finished(Box::new(report)));
        }
    }
}

/// Tarefa do colar no mesmo servidor (spawnada pela sessao SFTP). Termina
/// sempre com um `PasteEvent::Finished`, salvo se for abortada junto com a
/// sessao.
pub async fn run(
    sftp: Arc<SftpSession>,
    links: Arc<LinkOrder>,
    am_root: bool,
    id: u64,
    req: PasteRequest,
    cancel: watch::Receiver<bool>,
    tx: UnboundedSender<PasteEvent>,
) {
    run_job(&sftp, &sftp, false, &links, am_root, id, req, cancel, &tx).await;
}

/// Copia entre servidores: le em `src` (sessao de outro painel, emprestada)
/// e grava em `dst` (a sessao que roda esta tarefa). So copia (`op` e
/// forcado a `Copy`); dono/grupo nunca sao preservados. Termina sempre com
/// um `PasteEvent::Finished`, salvo se for abortada junto com a sessao.
pub async fn run_from(
    src: Arc<SftpSession>,
    dst: Arc<SftpSession>,
    links: Arc<LinkOrder>,
    id: u64,
    req: PasteRequest,
    cancel: watch::Receiver<bool>,
    tx: UnboundedSender<PasteEvent>,
) {
    let req = PasteRequest {
        op: PasteOp::Copy,
        ..req
    };
    run_job(&src, &dst, true, &links, false, id, req, cancel, &tx).await;
}

/// Um lote, do `Finished` garantido ao plano executado.
#[allow(clippy::too_many_arguments)]
async fn run_job(
    src: &SftpSession,
    dst: &SftpSession,
    cross: bool,
    links: &LinkOrder,
    am_root: bool,
    id: u64,
    req: PasteRequest,
    cancel: watch::Receiver<bool>,
    tx: &UnboundedSender<PasteEvent>,
) {
    let PasteRequest { op, dest_dir, items } = req;
    let mut finish = FinishGuard {
        tx,
        report: PasteReport {
            id,
            op: Some(op),
            count: items.len(),
            refresh: vec![dest_dir.clone()],
            ..Default::default()
        },
        sent: false,
    };
    if op != PasteOp::Copy {
        for it in &items {
            let p = parent_of(&it.src);
            if !finish.report.refresh.contains(&p) {
                finish.report.refresh.push(p);
            }
        }
    }
    let mut job = Job {
        src,
        dst,
        cross,
        links,
        am_root,
        id,
        op,
        dest_dir: dest_dir.clone(),
        dest_real: String::new(),
        cancel,
        tx,
        report: &mut finish.report,
        last_event: None,
        bytes: 0,
        write_fails: 0,
        parents: HashMap::new(),
    };
    if job.open_dest().await {
        match op {
            PasteOp::Move => job.move_items(&items).await,
            PasteOp::Copy | PasteOp::CopyThenDelete => job.copy_items(&items).await,
        }
    }
    drop(job);
    finish.send();
}

/// Um lote em andamento. Lado de leitura (`src`: lstat, listagem, readlink,
/// open para ler) e lado de gravacao (`dst`: tudo o que e criado, gravado,
/// renomeado ou apagado no destino). No mesmo servidor sao a mesma sessao;
/// mover e "copiar e apagar" so existem nesse caso.
struct Job<'a> {
    src: &'a SftpSession,
    dst: &'a SftpSession,
    /// Servidores diferentes (ver `run_from`).
    cross: bool,
    links: &'a LinkOrder,
    am_root: bool,
    id: u64,
    op: PasteOp,
    /// Pasta de destino como a UI a mostra (mensagens e listagens).
    dest_dir: String,
    /// A mesma, canonizada no servidor (onde tudo e gravado).
    dest_real: String,
    cancel: watch::Receiver<bool>,
    tx: &'a UnboundedSender<PasteEvent>,
    report: &'a mut PasteReport,
    last_event: Option<Instant>,
    /// Memoria guardada pela varredura (ver `MAX_SCAN_BYTES`).
    bytes: usize,
    /// Falhas de gravacao seguidas (ver `MAX_WRITE_FAILS`).
    write_fails: u32,
    /// Pasta canonizada de cada pasta de origem (cache).
    parents: HashMap<String, Option<String>>,
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

    #[allow(clippy::too_many_arguments)]
    fn progress(&mut self, phase: PastePhase, index: usize, count: usize, name: &str, done: u64, total: u64, force: bool) {
        if self.due(force) {
            let _ = self.tx.send(PasteEvent::Progress {
                id: self.id,
                phase,
                index,
                count,
                name: name.to_string(),
                done: done.min(total),
                total,
            });
        }
    }

    /// Espera um pedido sem travar o "Cancelar"; `None` = cancelado, ja
    /// anotado no relatorio.
    async fn ask<T>(&mut self, fut: impl std::future::Future<Output = T>) -> Option<T> {
        let r = unless_cancelled(&mut self.cancel, fut).await;
        if r.is_none() {
            self.report.cancelled = true;
        }
        r
    }

    /// Um pedido de conferencia com prazo curto (depois de uma falha).
    async fn probe<T>(&self, fut: impl std::future::Future<Output = Result<T, SftpError>>) -> Option<Result<T, SftpError>> {
        tokio::time::timeout(PROBE_TIMEOUT, fut).await.ok()
    }

    /// A sessao `s` ainda responde? (Uma sessao emprestada cuja thread
    /// acabou falha na hora: "session closed".)
    async fn alive(s: &SftpSession) -> bool {
        matches!(
            tokio::time::timeout(PROBE_TIMEOUT, s.canonicalize(".")).await,
            Ok(Ok(_))
        )
    }

    /// Erro remoto num item: falha so dele, ou fatal se a conexao caiu.
    /// Entre servidores as duas sessoes sao sondadas (juntas), e a mensagem
    /// diz qual delas caiu.
    async fn remote_problem(&self, msg: String) -> Step {
        let failed = || Step::Failed(clip(&msg, MAX_REMOTE_MSG));
        if !self.cross {
            return if Self::alive(self.dst).await {
                failed()
            } else {
                Step::Fatal(F_CONNECTION.into())
            };
        }
        let (src_ok, dst_ok) = tokio::join!(Self::alive(self.src), Self::alive(self.dst));
        if !src_ok {
            Step::Fatal(F_SRC_CONNECTION.into())
        } else if !dst_ok {
            Step::Fatal(F_DST_CONNECTION.into())
        } else {
            failed()
        }
    }

    /// A conexao com o destino caiu: nada mais e gravado nele.
    fn dest_lost(&self) -> bool {
        matches!(
            self.report.fatal.as_deref(),
            Some(F_CONNECTION | F_DST_CONNECTION)
        )
    }

    /// Registra a falha de um item; `false` = parar o lote.
    fn record(&mut self, shown: &str, step: Step) -> bool {
        match step {
            Step::Failed(m) => {
                self.bytes += text_cost(shown) + text_cost(&m);
                self.report.failed.push((shown.to_string(), m));
                true
            }
            Step::Fatal(m) => {
                self.report.fatal = Some(m);
                false
            }
            Step::Cancelled => {
                self.report.cancelled = true;
                false
            }
        }
    }

    /// Passo que interrompe quem chamou depois de um `record` que parou o
    /// lote: o fatal anotado continua valendo (nao vira "cancelado").
    fn stop(&self) -> Step {
        match &self.report.fatal {
            Some(m) => Step::Fatal(m.clone()),
            None => Step::Cancelled,
        }
    }

    fn note_skipped(&mut self, shown: &str, why: &str) {
        self.bytes += text_cost(shown) + text_cost(why);
        self.report.skipped.push((shown.to_string(), why.to_string()));
    }

    /// Canoniza e confere a pasta de destino; `false` = fatal (anotado).
    async fn open_dest(&mut self) -> bool {
        let dest = self.dest_dir.clone();
        let Some(real) = self.ask(self.dst.canonicalize(dest)).await else {
            return false;
        };
        let Ok(real) = real else {
            self.report.fatal = Some(F_DEST_GONE.into());
            return false;
        };
        let Some(meta) = self.ask(self.dst.metadata(real.clone())).await else {
            return false;
        };
        if !matches!(meta.map(|m| kind_of(&m)), Ok(RawKind::Dir)) {
            self.report.fatal = Some(F_DEST_GONE.into());
            return false;
        }
        self.dest_real = real;
        true
    }

    /// lstat no destino que distingue "nao existe" (`Ok(None)`) de erro.
    async fn lstat(&mut self, path: &str) -> Result<Option<FileAttributes>, Step> {
        let r = self.ask(self.dst.symlink_metadata(path.to_string())).await;
        self.lstat_result(r).await
    }

    /// O mesmo lstat, na origem.
    async fn lstat_src(&mut self, path: &str) -> Result<Option<FileAttributes>, Step> {
        let r = self.ask(self.src.symlink_metadata(path.to_string())).await;
        self.lstat_result(r).await
    }

    async fn lstat_result(&mut self, r: Option<Result<FileAttributes, SftpError>>) -> Result<Option<FileAttributes>, Step> {
        match r {
            None => Err(Step::Cancelled),
            Some(Ok(m)) => Ok(Some(m)),
            Some(Err(e)) if code_of(&e) == Code::NoSuchFile => Ok(None),
            Some(Err(e)) => Err(self.remote_problem(format!("não foi possível ler: {e}")).await),
        }
    }

    /// Pasta canonizada que contem `src` (cache por pasta).
    async fn parent_real(&mut self, src: &str) -> Option<String> {
        let parent = parent_of(src);
        if let Some(r) = self.parents.get(&parent) {
            return r.clone();
        }
        let r = match self.ask(self.src.canonicalize(parent.clone())).await {
            Some(Ok(p)) => Some(p),
            _ => None,
        };
        self.parents.insert(parent, r.clone());
        r
    }

    /// Conferencias comuns de um item: nome, caminho e lstat da origem.
    async fn preflight(&mut self, item: &PasteItem) -> Result<FileAttributes, Step> {
        check_remote_name(&item.name).map_err(|w| Step::Failed(w.into()))?;
        if !item.src.ends_with(&format!("/{}", item.name)) {
            return Err(Step::Failed(R_INVALID.into()));
        }
        match self.lstat_src(&item.src).await? {
            Some(m) => Ok(m),
            None => Err(Step::Failed(R_SRC_GONE.into())),
        }
    }

    // --- Mover ---------------------------------------------------------------

    async fn move_items(&mut self, items: &[PasteItem]) {
        let count = items.len();
        for (index, item) in items.iter().enumerate() {
            if self.cancelled() {
                self.report.cancelled = true;
                return;
            }
            self.progress(PastePhase::Moving, index, count, &item.name, 0, 0, index == 0);
            match self.move_item(item).await {
                Ok(()) => {}
                Err(step) => {
                    if !self.record(&item.name, step) {
                        return;
                    }
                }
            }
        }
    }

    async fn move_item(&mut self, item: &PasteItem) -> Result<(), Step> {
        let meta = self.preflight(item).await?;
        let is_dir = kind_of(&meta) == RawKind::Dir;
        // Mesma pasta de verdade (inclusive chegando por um link).
        if self.parent_real(&item.src).await.as_deref() == Some(self.dest_real.as_str()) {
            self.note_skipped(&item.name, R_SAME_DIR);
            return Ok(());
        }
        if is_dir {
            if let Some(Ok(src_real)) = self.ask(self.src.canonicalize(item.src.clone())).await {
                if is_inside(&self.dest_real, &src_real) {
                    return Err(Step::Failed(R_INTO_ITSELF.into()));
                }
            }
        }
        let target = join_remote(&self.dest_real, &item.name);
        let shown_to = join_remote(&self.dest_dir, &item.name);
        match self.lstat(&target).await? {
            None => match self.ask(self.dst.rename(item.src.clone(), target.clone())).await {
                None => Err(Step::Cancelled),
                Some(Ok(())) => {
                    self.moved(item, shown_to);
                    Ok(())
                }
                Some(Err(e)) => match self.rename_failed(&e, &item.src, &target, is_dir, None).await {
                    RenameFail::CrossDevice => self.cross_device(item, is_dir).await,
                    fail => Err(self.fail_step(fail, &e).await),
                },
            },
            Some(_) if !item.replace => Err(Step::Failed(R_EXISTS.into())),
            Some(t) => match (is_dir, kind_of(&t)) {
                (false, RawKind::Dir) => Err(Step::Failed(R_DIR_EXISTS.into())),
                (true, RawKind::Dir) => {
                    // Pasta sobre pasta: mescla (recursivo, limitado).
                    match self.merge(&item.src, &target, &item.name).await? {
                        Merge::CrossDevice => self.cross_device(item, true).await,
                        Merge::Done => {
                            if self.lstat_src(&item.src).await?.is_none() {
                                self.moved(item, shown_to);
                            }
                            Ok(())
                        }
                    }
                }
                (true, RawKind::Link) => Err(Step::Failed(R_DEST_LINK.into())),
                (true, _) => Err(Step::Failed(R_FILE_EXISTS.into())),
                (false, _) => match self.replace_by_backup(&item.src, &target, &item.name).await {
                    Ok(warn) => {
                        self.moved(item, shown_to);
                        if let Some(w) = warn {
                            self.report.failed.push((item.name.clone(), w));
                        }
                        Ok(())
                    }
                    Err(Replace::CrossDevice) => self.cross_device(item, false).await,
                    Err(Replace::Step(s)) => Err(s),
                },
            },
        }
    }

    fn moved(&mut self, item: &PasteItem, to: String) {
        self.report.done += 1;
        self.report.last = item.name.clone();
        self.report.moved.push((item.src.clone(), to));
    }

    /// Origem e destino em discos diferentes: nunca oferece copiar e apagar
    /// um ponto de montagem; o resto volta para a UI perguntar.
    async fn cross_device(&mut self, item: &PasteItem, is_dir: bool) -> Result<(), Step> {
        if is_dir {
            let a = self.probe(self.src.fs_info(item.src.clone())).await;
            let b = self.probe(self.src.fs_info(parent_of(&item.src))).await;
            if let (Some(Ok(Some(a))), Some(Ok(Some(b)))) = (a, b) {
                if a.fs_id != b.fs_id {
                    return Err(Step::Failed(R_MOUNT.into()));
                }
            }
        }
        self.report.cross_device.push(item.clone());
        Ok(())
    }

    /// Classifica a falha de um RENAME conferindo origem, destino e a pasta de
    /// destino (o OpenSSH devolve so "Failure" para varios motivos), tudo em
    /// `dst`, que e onde todo RENAME roda.
    /// `dst_known`: situacao do destino ja sabida (troca por backup).
    async fn rename_failed(&mut self, e: &SftpError, src: &str, target: &str, src_is_dir: bool, dst_known: Option<bool>) -> RenameFail {
        let exists = |r: Option<Result<FileAttributes, SftpError>>| match r {
            Some(Ok(_)) => Some(true),
            Some(Err(e)) if code_of(&e) == Code::NoSuchFile => Some(false),
            _ => None,
        };
        // O RENAME roda sempre em `dst`, entao a origem dele tambem esta la
        // (entre servidores e o temporario, ou o link ao lado, ja gravado
        // no destino).
        let src_exists = exists(self.probe(self.dst.symlink_metadata(src.to_string())).await);
        let dst_exists = match dst_known {
            Some(k) => Some(k),
            None => exists(self.probe(self.dst.symlink_metadata(target.to_string())).await),
        };
        let dest_dir_ok = match self.probe(self.dst.metadata(parent_of(target))).await {
            Some(Ok(m)) => Some(kind_of(&m) == RawKind::Dir),
            Some(Err(e)) if code_of(&e) == Code::NoSuchFile => Some(false),
            _ => None,
        };
        classify_rename(code_of(e), src_exists, dst_exists, dest_dir_ok, src_is_dir)
    }

    async fn fail_step(&self, fail: RenameFail, e: &SftpError) -> Step {
        match fail {
            RenameFail::Exists => Step::Failed(R_EXISTS.into()),
            RenameFail::SrcGone => Step::Failed(R_SRC_GONE.into()),
            RenameFail::DestGone => Step::Failed(R_DEST_GONE.into()),
            RenameFail::NoPermission => Step::Failed(R_NO_MOVE.into()),
            RenameFail::IntoItself => Step::Failed(R_INTO_ITSELF.into()),
            // Nao deveria chegar aqui (tratado antes por quem chama).
            RenameFail::CrossDevice | RenameFail::Other => {
                self.remote_problem(format!("falha no servidor: {e}")).await
            }
        }
    }

    /// Troca `target` (arquivo ou link) por `new` sem momento sem o original:
    /// o antigo vira um backup, o novo toma o nome e so entao o backup sai.
    /// `Ok(Some(aviso))`: trocou, mas o backup ficou.
    async fn replace_by_backup(&mut self, new: &str, target: &str, name: &str) -> Result<Option<String>, Replace> {
        let bak = join_remote(&parent_of(target), &side_name(name, OLD_SUFFIX));
        match self.ask(self.dst.rename(target.to_string(), bak.clone())).await {
            None => return Err(Replace::Step(Step::Cancelled)),
            Some(Err(e)) => return Err(Replace::Step(Step::Failed(clip(&format!("não foi possível substituir: {e}"), MAX_REMOTE_MSG)))),
            Some(Ok(())) => {}
        }
        match self.dst.rename(new.to_string(), target.to_string()).await {
            Ok(()) => match self.probe(self.dst.remove_file(bak.clone())).await {
                Some(Ok(())) => Ok(None),
                _ => Ok(Some(format!("substituído, mas o arquivo antigo ficou como {bak}"))),
            },
            Err(e) => {
                // Classifica antes de desfazer (o destino esta livre agora).
                let fail = self.rename_failed(&e, new, target, false, Some(false)).await;
                if self.dst.rename(bak.clone(), target.to_string()).await.is_err() {
                    return Err(Replace::Step(Step::Failed(format!(
                        "não foi possível substituir; o original ficou como {bak}"
                    ))));
                }
                match fail {
                    RenameFail::CrossDevice => Err(Replace::CrossDevice),
                    fail => Err(Replace::Step(self.fail_step(fail, &e).await)),
                }
            }
        }
    }

    /// Mescla a pasta `src` na pasta `dst` (mover com Substituir): cada filho
    /// e movido para dentro; pasta sobre pasta, em profundidade (pilha
    /// explicita, com os limites da varredura). No fim as pastas de origem
    /// que ficaram vazias saem (as de dentro primeiro).
    async fn merge(&mut self, src: &str, dst: &str, shown: &str) -> Result<Merge, Step> {
        // Mover: lado de leitura e de gravacao sao a mesma sessao.
        let (rs, ws) = (self.src, self.dst);
        let mut stack = vec![(src.to_string(), dst.to_string(), shown.to_string(), 1usize)];
        let mut emptied: Vec<String> = Vec::new();
        let mut moved_any = false;
        let mut seen = 0usize;
        while let Some((s, d, sh, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                if !self.record(&sh, Step::Failed(R_TOO_DEEP.into())) {
                    return Err(self.stop());
                }
                continue;
            }
            let listing = match self.ask(rs.read_dir(s.clone())).await {
                None => return Err(Step::Cancelled),
                Some(Ok(l)) => l,
                Some(Err(e)) => {
                    let step = self.remote_problem(format!("não foi possível listar: {e}")).await;
                    if depth == 1 {
                        return Err(step);
                    }
                    if !self.record(&sh, step) {
                        return Err(self.stop());
                    }
                    continue;
                }
            };
            emptied.push(s.clone());
            let mut names: Vec<String> = listing.map(|e| e.file_name()).collect();
            names.sort();
            for name in names {
                if self.cancelled() {
                    return Err(Step::Cancelled);
                }
                seen += 1;
                if seen > MAX_ENTRIES {
                    return Err(Step::Fatal(F_TOO_MANY.into()));
                }
                let child_shown = format!("{sh}/{}", clip(&name, 200));
                if let Err(why) = check_remote_name(&name) {
                    self.note_skipped(&child_shown, why);
                    continue;
                }
                let child_src = join_remote(&s, &name);
                let child_dst = join_remote(&d, &name);
                let Some(cm) = self.lstat_src(&child_src).await? else {
                    continue;
                };
                let child_dir = kind_of(&cm) == RawKind::Dir;
                let r = match self.lstat(&child_dst).await? {
                    None => match self.ask(ws.rename(child_src.clone(), child_dst.clone())).await {
                        None => return Err(Step::Cancelled),
                        Some(Ok(())) => Ok(()),
                        Some(Err(e)) => match self.rename_failed(&e, &child_src, &child_dst, child_dir, None).await {
                            RenameFail::CrossDevice if !moved_any => return Ok(Merge::CrossDevice),
                            RenameFail::CrossDevice => return Err(Step::Failed(R_PART_MOVED.into())),
                            fail => Err(self.fail_step(fail, &e).await),
                        },
                    },
                    Some(t) => match (child_dir, kind_of(&t)) {
                        (true, RawKind::Dir) => {
                            stack.push((child_src, child_dst, child_shown.clone(), depth + 1));
                            continue;
                        }
                        (false, RawKind::Dir) => Err(Step::Failed(R_DIR_EXISTS.into())),
                        (true, RawKind::Link) => Err(Step::Failed(R_DEST_LINK.into())),
                        (true, _) => Err(Step::Failed(R_FILE_EXISTS.into())),
                        (false, _) => match self.replace_by_backup(&child_src, &child_dst, &name).await {
                            Ok(warn) => {
                                if let Some(w) = warn {
                                    self.report.failed.push((child_shown.clone(), w));
                                }
                                Ok(())
                            }
                            Err(Replace::CrossDevice) if !moved_any => return Ok(Merge::CrossDevice),
                            Err(Replace::CrossDevice) => return Err(Step::Failed(R_PART_MOVED.into())),
                            Err(Replace::Step(st)) => Err(st),
                        },
                    },
                };
                match r {
                    Ok(()) => moved_any = true,
                    Err(step) => {
                        if !self.record(&child_shown, step) {
                            return Err(self.stop());
                        }
                    }
                }
            }
        }
        // Sobrou algo (falhas ja anotadas): essa pasta de origem fica.
        for s in emptied.iter().rev() {
            let _ = self.probe(rs.remove_dir(s.clone())).await;
        }
        Ok(Merge::Done)
    }

    // --- Copiar ----------------------------------------------------------------

    async fn copy_items(&mut self, items: &[PasteItem]) {
        let Some(plans) = self.scan(items).await else {
            return;
        };
        let files: Vec<u64> = plans
            .iter()
            .flat_map(|p| &p.entries)
            .filter_map(|e| match e.kind {
                Kind::File { size, .. } => Some(size),
                _ => None,
            })
            .collect();
        let total: u64 = files.iter().sum();
        self.report.files = files.len();
        // Espaco livre (statvfs): so quando o servidor informa. A cota do
        // cPanel nao aparece aqui (ver MAX_WRITE_FAILS).
        if total > 0 {
            if let Some(Ok(Some(s))) = self.probe(self.dst.fs_info(self.dest_real.clone())).await {
                let free = s.blocks_avail.saturating_mul(s.fragment_size);
                if free < total {
                    let mb = |b: u64| b.div_ceil(1024 * 1024);
                    self.report.fatal = Some(format!(
                        "espaço insuficiente no servidor: a cópia precisa de {} MB e há {} MB livres",
                        mb(total),
                        mb(free)
                    ));
                    return;
                }
            }
        }
        let mut prog = Prog {
            index: 0,
            count: files.len(),
            done: 0,
            total,
        };
        for plan in &plans {
            if self.cancelled() {
                self.report.cancelled = true;
                return;
            }
            let item = &items[plan.item];
            let (failed0, skipped0) = (self.report.failed.len(), self.report.skipped.len());
            let placed = match self.copy_plan(item, plan, &mut prog).await {
                Ok(p) => p,
                Err(step) => {
                    if !self.record(&item.name, step) {
                        return;
                    }
                    None
                }
            };
            if self.report.fatal.is_some() || self.report.cancelled {
                return;
            }
            let Some(dest_name) = placed else {
                if self.op == PasteOp::CopyThenDelete {
                    self.report.src_kept.push((item.name.clone(), K_NOT_CLEAN.into()));
                }
                continue;
            };
            let clean = self.report.failed.len() == failed0 && self.report.skipped.len() == skipped0;
            match self.op {
                PasteOp::CopyThenDelete if clean => {
                    if self.delete_source(item, plan).await {
                        self.report.done += 1;
                        self.report.last = dest_name.clone();
                        self.report.moved.push((item.src.clone(), join_remote(&self.dest_dir, &dest_name)));
                    }
                }
                PasteOp::CopyThenDelete => {
                    self.report.src_kept.push((item.name.clone(), K_NOT_CLEAN.into()));
                }
                _ => {
                    self.report.done += 1;
                    self.report.last = dest_name;
                }
            }
        }
    }

    /// Conta uma entrada vista na varredura; `false` = parar.
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
            let _ = self.tx.send(PasteEvent::Scanning {
                id: self.id,
                found: *seen,
            });
        }
        true
    }

    /// Dono/grupo a preservar (so conectado como root).
    fn owner(&self, a: &FileAttributes) -> Option<(u32, u32)> {
        if self.am_root {
            a.uid.zip(a.gid)
        } else {
            None
        }
    }

    /// Linha do plano para uma entrada ja lida (lstat). `None` = ignorada
    /// (anotada).
    async fn entry(&mut self, src: String, sub: Vec<String>, shown: String, a: &FileAttributes) -> Result<Option<Entry>, Step> {
        let kind = match kind_of(a) {
            RawKind::Dir => Kind::Dir {
                mode: a.permissions.unwrap_or(0o755),
                atime: a.atime,
                mtime: a.mtime,
                owner: self.owner(a),
            },
            RawKind::File => Kind::File {
                size: a.size.unwrap_or(0),
                mode: a.permissions.unwrap_or(0o644),
                atime: a.atime,
                mtime: a.mtime,
                owner: self.owner(a),
            },
            RawKind::Link => {
                let target = match self.ask(self.src.read_link(src.clone())).await {
                    None => return Err(Step::Cancelled),
                    Some(Ok(t)) => t,
                    Some(Err(e)) => return Err(self.remote_problem(format!("não foi possível ler o link: {e}")).await),
                };
                if target.is_empty()
                    || target.len() > MAX_LINK_TARGET
                    || target.contains(['\0', '\u{FFFD}'])
                {
                    self.note_skipped(&shown, R_LINK_TARGET);
                    return Ok(None);
                }
                Kind::Link { target }
            }
            RawKind::Unknown => {
                self.note_skipped(&shown, R_NO_TYPE);
                return Ok(None);
            }
            // FIFO, socket, dispositivo: nunca abertos (um FIFO travaria).
            _ => {
                self.note_skipped(&shown, R_SPECIAL);
                return Ok(None);
            }
        };
        Ok(Some(Entry { src, sub, shown, kind }))
    }

    /// Varredura: le tudo antes de gravar. `None` = parar (anotado).
    async fn scan(&mut self, items: &[PasteItem]) -> Option<Vec<ItemPlan>> {
        let mut plans = Vec::new();
        let mut seen = 0usize;
        if self.due(true) {
            let _ = self.tx.send(PasteEvent::Scanning { id: self.id, found: 0 });
        }
        for (i, item) in items.iter().enumerate() {
            if !self.tick(&mut seen) {
                return None;
            }
            let meta = match self.preflight(item).await {
                Ok(m) => m,
                Err(step) => {
                    if !self.record(&item.name, step) {
                        return None;
                    }
                    continue;
                }
            };
            let top = match self.entry(item.src.clone(), Vec::new(), item.name.clone(), &meta).await {
                Ok(Some(e)) => e,
                Ok(None) => continue,
                Err(step) => {
                    if !self.record(&item.name, step) {
                        return None;
                    }
                    continue;
                }
            };
            let is_dir = matches!(top.kind, Kind::Dir { .. });
            self.bytes += top.cost();
            let mut plan = ItemPlan {
                item: i,
                entries: vec![top],
            };
            if is_dir {
                self.walk(&item.src, &item.name, &mut plan, &mut seen).await?;
            }
            plans.push(plan);
        }
        Some(plans)
    }

    /// Lista a subarvore de um item (pilha explicita). Links nunca seguidos.
    async fn walk(&mut self, root: &str, shown: &str, plan: &mut ItemPlan, seen: &mut usize) -> Option<()> {
        // (caminho remoto, componentes, mostrado, profundidade)
        let mut stack: Vec<(String, Vec<String>, String, usize)> =
            vec![(root.to_string(), Vec::new(), shown.to_string(), 1)];
        while let Some((dir, sub, dshown, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                // A pasta em si e criada; o conteudo nao e lido.
                self.note_skipped(&dshown, R_TOO_DEEP);
                continue;
            }
            let listing = match self.ask(self.src.read_dir(dir.clone())).await? {
                Ok(l) => l,
                Err(e) => {
                    let step = self.remote_problem(format!("não foi possível listar: {e}")).await;
                    if !self.record(&dshown, step) {
                        return None;
                    }
                    continue;
                }
            };
            let mut entries: Vec<(String, FileAttributes)> =
                listing.map(|e| (e.file_name(), e.metadata())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut subdirs = Vec::new();
            for (name, mut meta) in entries {
                if !self.tick(seen) {
                    return None;
                }
                let child_shown = format!("{dshown}/{}", clip(&name, 200));
                if let Err(why) = check_remote_name(&name) {
                    self.note_skipped(&child_shown, why);
                    continue;
                }
                let src = join_remote(&dir, &name);
                if meta.permissions.is_none() {
                    // Listagem sem o tipo: confirma com lstat.
                    match self.ask(self.src.symlink_metadata(src.clone())).await? {
                        Ok(m) => meta = m,
                        Err(e) => {
                            let step = self.remote_problem(format!("não foi possível ler: {e}")).await;
                            if !self.record(&child_shown, step) {
                                return None;
                            }
                            continue;
                        }
                    }
                }
                let mut child_sub = sub.clone();
                child_sub.push(name);
                let e = match self.entry(src.clone(), child_sub.clone(), child_shown.clone(), &meta).await {
                    Ok(Some(e)) => e,
                    Ok(None) => continue,
                    Err(step) => {
                        if !self.record(&child_shown, step) {
                            return None;
                        }
                        continue;
                    }
                };
                if matches!(e.kind, Kind::Dir { .. }) {
                    self.bytes += text_cost(&src) + text_cost(&child_shown);
                    subdirs.push((src, child_sub, child_shown, depth + 1));
                }
                self.bytes += e.cost();
                plan.entries.push(e);
            }
            // Ordem inversa na pilha: a primeira subpasta sai primeiro.
            stack.extend(subdirs.into_iter().rev());
        }
        Some(())
    }

    /// Executa o plano de um item. `Ok(Some(nome))`: o item foi colocado no
    /// destino com esse nome (falhas internas ja anotadas); `Ok(None)`: nao
    /// foi colocado (anotado).
    async fn copy_plan(&mut self, item: &PasteItem, plan: &ItemPlan, prog: &mut Prog) -> Result<Option<String>, Step> {
        // Copiar para a propria pasta: nome automatico "(cópia)". Entre
        // servidores nunca (caminhos iguais sao pastas diferentes).
        let auto = self.op == PasteOp::Copy
            && !self.cross
            && self.parent_real(&item.src).await.as_deref() == Some(self.dest_real.as_str());
        let mut top_name = item.name.clone();
        // Pastas criadas neste item (para ajustar permissoes/datas no fim).
        let mut created: Vec<CreatedDir> = Vec::new();
        // Pastas cujo conteudo nao deve ser copiado (falharam).
        let mut failed_dirs: HashSet<Vec<String>> = HashSet::new();
        let mut placed = false;
        let mut result = Ok(());
        for e in &plan.entries {
            if self.cancelled() {
                self.report.cancelled = true;
                break;
            }
            let is_top = e.sub.is_empty();
            if !is_top {
                let parent = &e.sub[..e.sub.len() - 1];
                if !placed || failed_dirs.contains(parent) {
                    if matches!(e.kind, Kind::Dir { .. }) {
                        failed_dirs.insert(e.sub.clone());
                    }
                    if let Kind::File { size, .. } = e.kind {
                        prog.index += 1;
                        prog.done += size;
                    }
                    continue;
                }
            }
            let target = if is_top {
                join_remote(&self.dest_real, &item.name)
            } else {
                let mut t = join_remote(&self.dest_real, &top_name);
                for c in &e.sub {
                    t = join_remote(&t, c);
                }
                t
            };
            let replace = item.replace;
            let step = match &e.kind {
                Kind::Dir { mode, atime, mtime, owner } => {
                    match self.make_dir(&target, &item.name, is_top && auto, replace, *mode).await {
                        Ok((path, name, new)) => {
                            if new {
                                created.push((path, *mode, *atime, *mtime, *owner));
                            }
                            if is_top {
                                top_name = name;
                            }
                            Ok(())
                        }
                        Err(s) => {
                            failed_dirs.insert(e.sub.clone());
                            Err(s)
                        }
                    }
                }
                Kind::File { size, mode, atime, mtime, owner } => {
                    let attrs = copy_attrs(*mode, *atime, *mtime, *owner);
                    let r = self
                        .copy_file(e, &target, &item.name, is_top && auto, replace, attrs, *prog)
                        .await;
                    prog.index += 1;
                    prog.done += size;
                    match r {
                        Ok(name) => {
                            self.report.saved += 1;
                            if is_top {
                                top_name = name;
                            }
                            Ok(())
                        }
                        Err(s) => Err(s),
                    }
                }
                Kind::Link { target: to } => match self.place_link(&target, to, &item.name, is_top && auto, replace).await {
                    Ok(Some(name)) => {
                        if is_top {
                            top_name = name;
                        }
                        Ok(())
                    }
                    Ok(None) => {
                        // Servidor sem links: ignorado (anotado).
                        self.note_skipped(&e.shown, R_NO_LINKS);
                        if is_top {
                            break;
                        }
                        continue;
                    }
                    Err(s) => Err(s),
                },
            };
            match step {
                Ok(()) => {
                    if is_top {
                        placed = true;
                        if auto && top_name != item.name {
                            self.report.renamed.push((item.name.clone(), top_name.clone()));
                        }
                    }
                }
                Err(s) => {
                    if is_top {
                        result = Err(s);
                        break;
                    }
                    if !self.record(&e.shown, s) {
                        break;
                    }
                }
            }
        }
        // Permissoes e datas das pastas criadas, filhas antes das maes (uma
        // pasta 0555 bloquearia as gravacoes; gravar filhos muda o mtime).
        if !self.dest_lost() {
            for (path, mode, atime, mtime, owner) in created.iter().rev() {
                let _ = self.probe(self.dst.set_metadata(path.clone(), copy_attrs(*mode, *atime, *mtime, *owner))).await;
            }
        }
        result?;
        Ok(placed.then_some(top_name))
    }

    /// Cria uma pasta do plano: `Ok((caminho, nome, criada))`; com
    /// `auto`, escolhe "nome (cópia N)". Pasta existente com `replace` e
    /// mesclada (so pasta de verdade).
    async fn make_dir(&mut self, target: &str, name: &str, auto: bool, replace: bool, mode: u32) -> Result<(String, String, bool), Step> {
        let parent = parent_of(target);
        let own_name = target.rsplit('/').next().unwrap_or(name).to_string();
        let tries = if auto { MAX_COPY_NAMES } else { 1 };
        for k in 1..=tries {
            let (path, nm) = if auto {
                let n = copy_name(name, true, k);
                (join_remote(&parent, &n), n)
            } else {
                (target.to_string(), own_name.clone())
            };
            match self.ask(self.dst.create_dir(path.clone())).await {
                None => return Err(Step::Cancelled),
                Some(Ok(())) => {
                    let mut a = FileAttributes::empty();
                    a.permissions = Some(provisional_dir_mode(mode));
                    let _ = self.probe(self.dst.set_metadata(path.clone(), a)).await;
                    return Ok((path, nm, true));
                }
                Some(Err(e)) => match self.lstat(&path).await? {
                    Some(_) if auto => continue,
                    Some(t) => {
                        return match kind_of(&t) {
                            RawKind::Dir if replace => Ok((path, nm, false)),
                            RawKind::Dir => Err(Step::Failed(R_EXISTS.into())),
                            RawKind::Link => Err(Step::Failed(R_DEST_LINK.into())),
                            _ => Err(Step::Failed(R_FILE_EXISTS.into())),
                        };
                    }
                    None if code_of(&e) == Code::PermissionDenied => return Err(Step::Failed(R_NO_WRITE.into())),
                    None => return Err(self.remote_problem(format!("não foi possível criar a pasta: {e}")).await),
                },
            }
        }
        Err(Step::Failed(R_NO_NAME.into()))
    }

    /// Copia um arquivo: temporario na pasta final (EXCLUDE: nunca segue link
    /// nem sobrescreve), dados em blocos (o Cancelar vale com um pedido em
    /// voo), atributos e, por fim, o nome definitivo. Devolve o nome final.
    #[allow(clippy::too_many_arguments)]
    async fn copy_file(
        &mut self,
        e: &Entry,
        target: &str,
        top: &str,
        auto: bool,
        replace: bool,
        attrs: FileAttributes,
        p: Prog,
    ) -> Result<String, Step> {
        let dir = parent_of(target);
        let fname = target.rsplit('/').next().unwrap_or(top).to_string();
        let mut src = match self.ask(self.src.open(e.src.clone())).await {
            None => return Err(Step::Cancelled),
            Some(Ok(f)) => f,
            Some(Err(err)) if code_of(&err) == Code::PermissionDenied => return Err(Step::Failed(R_NO_READ.into())),
            Some(Err(err)) => return Err(self.remote_problem(format!("não foi possível ler: {err}")).await),
        };
        // Aberto de verdade um arquivo comum (fstat)?
        match self.ask(src.metadata()).await {
            None => return Err(Step::Cancelled),
            Some(Ok(m)) if kind_of(&m) == RawKind::File => {}
            Some(_) => return Err(Step::Failed(R_CHANGED.into())),
        }
        // Temporario na pasta final: o rename no fim e atomico.
        let mut create = FileAttributes::empty();
        create.permissions = attrs.permissions;
        let mut out = None;
        let mut tmp = String::new();
        for _ in 0..3 {
            tmp = join_remote(&dir, &side_name(&fname, TEMP_SUFFIX));
            let flags = OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE;
            match self.ask(self.dst.open_with_flags_and_attributes(tmp.clone(), flags, create.clone())).await {
                None => return Err(Step::Cancelled),
                Some(Ok(f)) => {
                    out = Some(f);
                    break;
                }
                Some(Err(err)) => match code_of(&err) {
                    Code::PermissionDenied => return Err(Step::Failed(R_NO_WRITE.into())),
                    // Nome sorteado ja existe: sorteia outro.
                    Code::Failure => continue,
                    _ => return Err(self.remote_problem(format!("não foi possível gravar: {err}")).await),
                },
            }
        }
        let Some(mut out) = out else {
            return Err(Step::Failed(R_NO_TEMP.into()));
        };
        self.progress(PastePhase::Copying, p.index, p.count, &e.shown, p.done, p.total, p.index == 0);
        let mut buf = vec![0u8; CHUNK];
        let mut got = 0u64;
        let failure: Option<Step> = loop {
            let n = match unless_cancelled(&mut self.cancel, src.read(&mut buf)).await {
                None => break Some(Step::Cancelled),
                Some(Ok(n)) => n,
                Some(Err(err)) => break Some(self.remote_problem(format!("erro ao ler do servidor: {err}")).await),
            };
            if n == 0 {
                break None;
            }
            match unless_cancelled(&mut self.cancel, out.write_all(&buf[..n])).await {
                None => break Some(Step::Cancelled),
                Some(Ok(())) => {}
                Some(Err(err)) => break Some(self.write_failed(format!("erro ao gravar no servidor: {err}")).await),
            }
            got += n as u64;
            self.progress(PastePhase::Copying, p.index, p.count, &e.shown, p.done + got, p.total, false);
        };
        // Fecha a origem esperando o CLOSE (ver `download::close_remote`).
        // Numa falha ou cancelamento fica so o drop (a conexao pode ter
        // caido, e o CLOSE esperaria o prazo).
        if failure.is_none() {
            close_remote(src, &mut self.cancel).await;
        } else {
            drop(src);
        }
        if let Some(step) = failure {
            drop(out);
            self.discard(&tmp).await;
            return Err(step);
        }
        // Atributos antes de fechar (o servidor processa em ordem, depois das
        // escritas: o mtime nao e mexido por uma escrita atrasada).
        let attrs_warn = match self.probe(out.set_metadata(attrs)).await {
            Some(Ok(())) => None,
            Some(Err(err)) => Some(format!("permissões/data não aplicadas: {err}")),
            None => Some("permissões/data não aplicadas".to_string()),
        };
        if let Err(err) = out.shutdown().await {
            let step = self.write_failed(format!("erro ao gravar no servidor: {err}")).await;
            self.discard(&tmp).await;
            return Err(step);
        }
        self.write_fails = 0;
        // Nome definitivo.
        let name = if auto {
            let mut chosen = None;
            for k in 1..=MAX_COPY_NAMES {
                let n = copy_name(top, false, k);
                match self.dst.rename(tmp.clone(), join_remote(&dir, &n)).await {
                    Ok(()) => {
                        chosen = Some(n);
                        break;
                    }
                    Err(err) => match self.lstat(&join_remote(&dir, &n)).await {
                        Ok(Some(_)) => continue,
                        Ok(None) => {
                            let step = self.remote_problem(format!("não foi possível gravar: {err}")).await;
                            self.discard(&tmp).await;
                            return Err(step);
                        }
                        Err(step) => {
                            self.discard(&tmp).await;
                            return Err(step);
                        }
                    },
                }
            }
            match chosen {
                Some(n) => n,
                None => {
                    self.discard(&tmp).await;
                    return Err(Step::Failed(R_NO_NAME.into()));
                }
            }
        } else {
            match self.dst.rename(tmp.clone(), target.to_string()).await {
                Ok(()) => fname,
                Err(err) => match self.lstat(target).await {
                    Ok(Some(_)) if !replace => {
                        self.discard(&tmp).await;
                        return Err(Step::Failed(R_EXISTS.into()));
                    }
                    Ok(Some(t)) if kind_of(&t) == RawKind::Dir => {
                        self.discard(&tmp).await;
                        return Err(Step::Failed(R_DIR_EXISTS.into()));
                    }
                    Ok(Some(_)) => match self.replace_by_backup(&tmp, target, &fname).await {
                        Ok(warn) => {
                            if let Some(w) = warn {
                                self.report.failed.push((e.shown.clone(), w));
                            }
                            fname
                        }
                        Err(r) => {
                            self.discard(&tmp).await;
                            return Err(match r {
                                Replace::Step(s) => s,
                                Replace::CrossDevice => Step::Failed(format!("não foi possível substituir: {err}")),
                            });
                        }
                    },
                    Ok(None) => {
                        let step = self.remote_problem(format!("não foi possível gravar: {err}")).await;
                        self.discard(&tmp).await;
                        return Err(step);
                    }
                    Err(step) => {
                        self.discard(&tmp).await;
                        return Err(step);
                    }
                },
            }
        };
        if let Some(w) = attrs_warn {
            self.report.failed.push((e.shown.clone(), w));
        }
        Ok(name)
    }

    /// Falha de gravacao: 3 seguidas no lote viram fatal (disco cheio ou cota
    /// esgotada chegam so como "Failure").
    async fn write_failed(&mut self, msg: String) -> Step {
        self.write_fails += 1;
        if self.write_fails >= MAX_WRITE_FAILS {
            return Step::Fatal(F_WRITE.into());
        }
        self.remote_problem(msg).await
    }

    /// Apaga um temporario deixado por uma falha (prazo curto, melhor esforco).
    async fn discard(&self, tmp: &str) {
        let _ = self.probe(self.dst.remove_file(tmp.to_string())).await;
    }

    /// Ordem do SYMLINK neste servidor (sonda uma vez por sessao, dentro da
    /// pasta de destino, que ja vai ser gravada).
    async fn link_order(&mut self) -> SymlinkArgs {
        if let Some(o) = *self.links.0.lock().unwrap_or_else(|e| e.into_inner()) {
            return o;
        }
        let a = join_remote(&self.dest_real, &side_name("sagu", LINK_PROBE_SUFFIX));
        let b = format!("{a}-alvo");
        // OpenSSH: cria o link em `a` (apontando para `b`); servidor do
        // draft: cria em `b`.
        let _ = self.probe(self.dst.symlink(b.clone(), a.clone())).await;
        let is_link = |r: Option<Result<FileAttributes, SftpError>>| {
            matches!(r, Some(Ok(m)) if kind_of(&m) == RawKind::Link)
        };
        let order = if is_link(self.probe(self.dst.symlink_metadata(a.clone())).await) {
            let _ = self.probe(self.dst.remove_file(a)).await;
            SymlinkArgs::TargetFirst
        } else if is_link(self.probe(self.dst.symlink_metadata(b.clone())).await) {
            let _ = self.probe(self.dst.remove_file(b)).await;
            SymlinkArgs::LinkFirst
        } else {
            // Sem link nenhum: nao guarda (pode ter sido uma falha passageira).
            return SymlinkArgs::NoLinks;
        };
        *self.links.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(order);
        order
    }

    async fn make_symlink(&mut self, order: SymlinkArgs, link: &str, target: &str) -> Option<Result<(), SftpError>> {
        match order {
            SymlinkArgs::TargetFirst => self.ask(self.dst.symlink(target.to_string(), link.to_string())).await,
            SymlinkArgs::LinkFirst => self.ask(self.dst.symlink(link.to_string(), target.to_string())).await,
            SymlinkArgs::NoLinks => Some(Ok(())),
        }
    }

    /// Cria o link `target` apontando para `to` (mesmo alvo, relativo ou
    /// absoluto). `Ok(None)`: o servidor nao cria links. Devolve o nome final.
    async fn place_link(&mut self, target: &str, to: &str, top: &str, auto: bool, replace: bool) -> Result<Option<String>, Step> {
        let order = self.link_order().await;
        if order == SymlinkArgs::NoLinks {
            return Ok(None);
        }
        let dir = parent_of(target);
        let fname = target.rsplit('/').next().unwrap_or(top).to_string();
        let tries = if auto { MAX_COPY_NAMES } else { 1 };
        for k in 1..=tries {
            let (path, nm) = if auto {
                let n = copy_name(top, false, k);
                (join_remote(&dir, &n), n)
            } else {
                (target.to_string(), fname.clone())
            };
            match self.make_symlink(order, &path, to).await {
                None => return Err(Step::Cancelled),
                Some(Ok(())) => return Ok(Some(nm)),
                Some(Err(err)) => match self.lstat(&path).await? {
                    Some(_) if auto => continue,
                    Some(_) if !replace => return Err(Step::Failed(R_EXISTS.into())),
                    Some(t) if kind_of(&t) == RawKind::Dir => return Err(Step::Failed(R_DIR_EXISTS.into())),
                    Some(_) => {
                        // Cria ao lado e troca pelo backup.
                        let side = join_remote(&dir, &side_name(&nm, LINK_PROBE_SUFFIX));
                        match self.make_symlink(order, &side, to).await {
                            None => return Err(Step::Cancelled),
                            Some(Err(e)) => return Err(self.remote_problem(format!("não foi possível criar o link: {e}")).await),
                            Some(Ok(())) => {}
                        }
                        return match self.replace_by_backup(&side, &path, &nm).await {
                            Ok(warn) => {
                                if let Some(w) = warn {
                                    self.report.failed.push((nm.clone(), w));
                                }
                                Ok(Some(nm))
                            }
                            Err(r) => {
                                self.discard(&side).await;
                                Err(match r {
                                    Replace::Step(s) => s,
                                    Replace::CrossDevice => Step::Failed(R_NO_LINKS.into()),
                                })
                            }
                        };
                    }
                    None if code_of(&err) == Code::PermissionDenied => return Err(Step::Failed(R_NO_WRITE.into())),
                    None => return Err(self.remote_problem(format!("não foi possível criar o link: {err}")).await),
                },
            }
        }
        Err(Step::Failed(R_NO_NAME.into()))
    }

    /// Apaga a origem de um item ja copiado sem erro (copiar e apagar), do
    /// fim do plano para o comeco: so o que foi varrido e copiado, nunca
    /// seguindo link. `true` se o proprio item saiu da origem.
    async fn delete_source(&mut self, item: &PasteItem, plan: &ItemPlan) -> bool {
        let count = self.report.count;
        let index = self.report.done;
        self.progress(PastePhase::Deleting, index, count, &item.name, 0, 0, true);
        let mut top_gone = false;
        for e in plan.entries.iter().rev() {
            if self.cancelled() {
                self.report.cancelled = true;
                break;
            }
            let is_top = e.sub.is_empty();
            let r = match e.kind {
                Kind::Dir { .. } => self.probe(self.src.remove_dir(e.src.clone())).await,
                _ => self.probe(self.src.remove_file(e.src.clone())).await,
            };
            match r {
                Some(Ok(())) => top_gone |= is_top,
                Some(Err(err)) if code_of(&err) == Code::NoSuchFile => top_gone |= is_top,
                Some(Err(err)) if is_top => {
                    let why = if matches!(e.kind, Kind::Dir { .. }) && code_of(&err) == Code::Failure {
                        K_NEW_ITEMS.to_string()
                    } else {
                        clip(&format!("não foi possível apagar a origem: {err}"), MAX_REMOTE_MSG)
                    };
                    self.report.src_kept.push((item.name.clone(), why));
                }
                _ => {}
            }
        }
        top_gone
    }
}

/// Resultado da mescla de uma pasta ao mover.
enum Merge {
    Done,
    /// O primeiro rename ja deu "outro disco", nada foi movido: o item todo
    /// vai para a oferta de copiar e apagar.
    CrossDevice,
}

/// Falha da troca por backup.
enum Replace {
    Step(Step),
    CrossDevice,
}

/// Posicao do arquivo atual no lote (andamento).
#[derive(Clone, Copy)]
struct Prog {
    index: usize,
    count: usize,
    done: u64,
    total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Servidor SFTP falso que so responde lstat/stat (NoSuchFile fora do
    /// mapa), para separar o que existe em cada lado da copia.
    struct StatServer(HashMap<String, FileAttributes>);

    impl russh_sftp::server::Handler for StatServer {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        fn lstat(
            &mut self,
            id: u32,
            path: String,
        ) -> impl std::future::Future<Output = Result<russh_sftp::protocol::Attrs, StatusCode>> + Send {
            let a = self.0.get(&path).cloned();
            async move { a.map(|attrs| russh_sftp::protocol::Attrs { id, attrs }).ok_or(StatusCode::NoSuchFile) }
        }

        fn stat(
            &mut self,
            id: u32,
            path: String,
        ) -> impl std::future::Future<Output = Result<russh_sftp::protocol::Attrs, StatusCode>> + Send {
            let a = self.0.get(&path).cloned();
            async move { a.map(|attrs| russh_sftp::protocol::Attrs { id, attrs }).ok_or(StatusCode::NoSuchFile) }
        }
    }

    async fn stat_session(nodes: &[(&str, u32)]) -> SftpSession {
        let map = nodes
            .iter()
            .map(|(p, mode)| {
                (
                    p.to_string(),
                    FileAttributes {
                        permissions: Some(*mode),
                        size: Some(1),
                        ..FileAttributes::empty()
                    },
                )
            })
            .collect();
        let (client, server) = tokio::io::duplex(1 << 16);
        russh_sftp::server::run(server, StatServer(map)).await;
        crate::sftp::start_session(client).await.expect("sessao falsa")
    }

    /// Entre servidores, a origem de um RENAME que falhou (o temporario ja
    /// gravado no destino) e conferida no destino, nao na sessao de origem,
    /// onde ela nunca existe: senao toda falha viraria "nao existe mais na
    /// origem".
    #[test]
    fn rename_failed_probes_rename_source_on_dst() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // Origem: so /srv/a.txt. Destino: /dst e o temporario ja gravado
            // la; /dst/a.txt virou backup (destino livre).
            let src = stat_session(&[("/srv", 0o040755), ("/srv/a.txt", 0o100644)]).await;
            let dst = stat_session(&[("/dst", 0o040755), ("/dst/.a.txt.deadbeef.sagu-part", 0o100644)]).await;
            let links = LinkOrder::default();
            let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
            let (_keep, cancel) = watch::channel(false);
            let mut report = PasteReport::default();
            let mut job = Job {
                src: &src,
                dst: &dst,
                cross: true,
                links: &links,
                am_root: false,
                id: 1,
                op: PasteOp::Copy,
                dest_dir: "/dst".into(),
                dest_real: "/dst".into(),
                cancel,
                tx: &tx,
                report: &mut report,
                last_event: None,
                bytes: 0,
                write_fails: 0,
                parents: HashMap::new(),
            };
            let e = SftpError::Status(russh_sftp::protocol::Status {
                id: 7,
                status_code: StatusCode::Failure,
                error_message: "Failure".into(),
                language_tag: "en-US".into(),
            });
            let fail = job
                .rename_failed(&e, "/dst/.a.txt.deadbeef.sagu-part", "/dst/a.txt", false, Some(false))
                .await;
            assert_eq!(fail, RenameFail::CrossDevice, "temporario presente no destino");
        });
    }

    #[test]
    fn copy_names() {
        assert_eq!(copy_name("a.txt", false, 1), "a (cópia).txt");
        assert_eq!(copy_name("a.txt", false, 2), "a (cópia 2).txt");
        assert_eq!(copy_name(".bashrc", false, 1), ".bashrc (cópia)");
        assert_eq!(copy_name("x.tar.gz", false, 1), "x.tar (cópia).gz");
        assert_eq!(copy_name("pasta.v2", true, 1), "pasta.v2 (cópia)");
        let n = format!("{}.txt", "ç".repeat(125));
        assert!(copy_name(&n, false, 1000).len() <= 255);
    }

    #[test]
    fn paths() {
        assert!(is_inside("/a/b", "/a"));
        assert!(!is_inside("/ab", "/a"));
        assert_eq!(parent_of("/a/b"), "/a");
        assert_eq!(parent_of("/a"), "/");
        assert!(same_dir("/a/", "/a"));
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            assert!(check_remote_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn rename_failures() {
        let (t, f) = (Some(true), Some(false));
        assert_eq!(classify_rename(Code::Failure, t, t, t, false), RenameFail::Exists);
        assert_eq!(classify_rename(Code::Failure, t, f, t, true), RenameFail::CrossDevice);
        assert_eq!(classify_rename(Code::Failure, None, f, t, false), RenameFail::Other);
        assert_eq!(classify_rename(Code::BadMessage, t, f, t, true), RenameFail::IntoItself);
    }

    /// Entre servidores (`cross`): o mesmo caminho dos dois lados nao e "a
    /// mesma pasta" (os nomes conflitam normalmente) e um destino com o
    /// prefixo da pasta de origem nao e "dentro dela mesma". No mesmo
    /// servidor as duas regras continuam valendo, e nomes invalidos sao
    /// barrados nos dois casos.
    #[test]
    fn prepare_between_servers_ignores_path_coincidences() {
        let srcs = [
            Source {
                path: "/home/u/a.txt",
                name: "a.txt",
                is_dir: false,
            },
            Source {
                path: "/home/u/pasta",
                name: "pasta",
                is_dir: true,
            },
        ];
        let dest: HashMap<&str, bool> = [("a.txt", false), ("pasta", false)].into();
        // Mesmo caminho dos dois lados: conflita por nome (pasta sobre
        // arquivo conta como tipos diferentes).
        let p = prepare(PasteOp::Copy, "/home/u", "/home/u/", &srcs, &dest, true);
        assert_eq!(p.items.len(), 2, "{p:?}");
        assert_eq!(p.conflicts, [0, 1]);
        assert_eq!(p.mismatched, 1);
        assert!(p.invalid.is_empty(), "{p:?}");
        assert!(p.items.iter().all(|i| !i.replace));
        let p = prepare(PasteOp::Copy, "/home/u", "/home/u/", &srcs, &dest, false);
        assert_eq!(p.items.len(), 2, "{p:?}");
        assert!(p.conflicts.is_empty() && p.invalid.is_empty(), "{p:?}");
        // Destino com o caminho "dentro" da pasta de origem.
        let none = HashMap::new();
        let p = prepare(PasteOp::Copy, "/home/u", "/home/u/pasta/sub", &srcs, &none, true);
        assert_eq!(p.items.len(), 2, "{p:?}");
        assert!(p.invalid.is_empty() && p.conflicts.is_empty(), "{p:?}");
        let p = prepare(PasteOp::Copy, "/home/u", "/home/u/pasta/sub", &srcs, &none, false);
        assert_eq!(p.items.len(), 1, "{p:?}");
        assert_eq!(p.invalid, [("pasta".to_string(), R_INTO_ITSELF.to_string())]);
        // Nome invalido: barrado tambem entre servidores.
        let bad = [Source {
            path: "/x/a/b",
            name: "a/b",
            is_dir: false,
        }];
        let p = prepare(PasteOp::Copy, "/x", "/y", &bad, &none, true);
        assert!(p.items.is_empty(), "{p:?}");
        assert_eq!(p.invalid.len(), 1);
    }

    // --- Ponta a ponta contra um sshd real (ignorados) ---------------------
    //
    // Mesmas variaveis dos outros e2e (SAGU_E2E_PORT, SAGU_E2E_USER,
    // SAGU_E2E_KEY). Duas sessoes independentes no mesmo sshd fazem o papel
    // de dois paineis (cada uma na sua thread, como no app).
    // Rodar com: cargo test e2e_copy_between -- --ignored --test-threads=1

    use crate::download::tests::{close, e2e_host, remote_sh, sftp_session};
    use crate::sftp::{SftpHandle, SftpToUi};
    use crate::vault::Host;
    use std::time::{Duration, Instant};

    /// Apaga a arvore remota no fim (inclusive se o teste falhar).
    struct RemoteCleanup {
        host: Host,
        dir: String,
    }

    impl Drop for RemoteCleanup {
        fn drop(&mut self) {
            if let Err(e) = remote_sh(&self.host, &format!("rm -rf '{}'", self.dir)) {
                eprintln!("limpeza remota falhou: {e}");
            }
        }
    }

    /// Espera o `Finished` do lote `id` em `h` por ate `secs`, passando os
    /// eventos de andamento a `on_event`.
    fn wait_finished(h: &SftpHandle, id: u64, secs: u64, mut on_event: impl FnMut(&PasteEvent)) -> PasteReport {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(secs) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Paste(PasteEvent::Finished(r))) => {
                    assert_eq!(r.id, id);
                    return *r;
                }
                Ok(SftpToUi::Paste(ev)) => on_event(&ev),
                Ok(SftpToUi::Error(e)) => panic!("erro da sessao de destino: {e}"),
                Ok(SftpToUi::Closed) => panic!("a sessao de destino fechou no meio da cópia"),
                _ => {}
            }
        }
        panic!("tempo esgotado esperando o fim da cópia");
    }

    fn item(dir: &str, name: &str) -> PasteItem {
        PasteItem {
            src: format!("{dir}/{name}"),
            name: name.into(),
            replace: false,
        }
    }

    /// Copia de uma sessao para outra: arquivo (conteudo, modo e data),
    /// pastas aninhadas (modo), links copiados como links (inclusive
    /// quebrado), fifo pulado, nada movido nem renomeado, sem temporarios; o
    /// `op` pedido e forcado a `Copy`.
    #[test]
    #[ignore]
    fn e2e_copy_between_sessions() {
        let host = e2e_host();
        let r = format!("/tmp/sagu-e2e-entre-{}", std::process::id());
        let _cleanup = RemoteCleanup {
            host: host.clone(),
            dir: r.clone(),
        };
        let script = format!(
            r#"set -e; R='{r}'; rm -rf "$R"; mkdir -p "$R/src/pasta/sub1/sub2" "$R/src/pasta/vazia" "$R/dst"
printf 'conteudo a' > "$R/src/a.txt"; chmod 640 "$R/src/a.txt"; touch -d '2020-01-02 03:04:05 UTC' "$R/src/a.txt"
printf x > "$R/src/pasta/sub1/x.txt"; printf y > "$R/src/pasta/sub1/sub2/y.txt"; chmod 750 "$R/src/pasta/sub1"
ln -s ../a.txt "$R/src/pasta/link-arq"; ln -s sub1 "$R/src/pasta/link-pasta"; ln -s /nao/existe "$R/src/pasta/quebrado"
mkfifo "$R/src/pasta/fifo"
"#
        );
        remote_sh(&host, &script).expect("preparo da arvore remota");
        let a = sftp_session(&host);
        let b = sftp_session(&host);
        let src = a.session_ref().expect("sessao de origem sem sessao emprestavel");
        let src_dir = format!("{r}/src");
        let req = PasteRequest {
            // Forcado a Copy: a origem tem de ficar.
            op: PasteOp::Move,
            dest_dir: format!("{r}/dst"),
            items: vec![item(&src_dir, "a.txt"), item(&src_dir, "pasta")],
        };
        let _cancel = b.copy_from(1, src, req).expect("sessao de destino encerrada");
        let mut progress = 0;
        let rep = wait_finished(&b, 1, 60, |ev| {
            if matches!(ev, PasteEvent::Progress { phase: PastePhase::Copying, count: 3, .. }) {
                progress += 1;
            }
        });
        assert_eq!(rep.op, Some(PasteOp::Copy));
        assert!(rep.fatal.is_none() && !rep.cancelled, "{rep:?}");
        assert!(rep.failed.is_empty(), "{:?}", rep.failed);
        assert_eq!((rep.count, rep.done, rep.files, rep.saved), (2, 2, 3, 3), "{rep:?}");
        assert_eq!(rep.last, "pasta");
        assert_eq!(rep.skipped, [("pasta/fifo".to_string(), R_SPECIAL.to_string())]);
        assert!(rep.renamed.is_empty() && rep.moved.is_empty(), "{rep:?}");
        assert_eq!(rep.refresh, [format!("{r}/dst")]);
        assert!(progress > 0, "nenhum andamento da cópia");
        // Destino fiel; origem intacta; nenhum temporario.
        let check = format!(
            r#"set -e; R='{r}'; S="$R/src"; D="$R/dst"
cmp "$S/a.txt" "$D/a.txt"; [ "$(stat -c %a "$D/a.txt")" = 640 ]; [ "$(stat -c %Y "$D/a.txt")" = 1577934245 ]
cmp "$S/pasta/sub1/x.txt" "$D/pasta/sub1/x.txt"; cmp "$S/pasta/sub1/sub2/y.txt" "$D/pasta/sub1/sub2/y.txt"
[ "$(stat -c %a "$D/pasta/sub1")" = 750 ]; [ -d "$D/pasta/vazia" ]
[ -L "$D/pasta/link-arq" ]; [ "$(readlink "$D/pasta/link-arq")" = ../a.txt ]
[ -L "$D/pasta/link-pasta" ]; [ "$(readlink "$D/pasta/link-pasta")" = sub1 ]
[ -L "$D/pasta/quebrado" ]; [ "$(readlink "$D/pasta/quebrado")" = /nao/existe ]
! [ -e "$D/pasta/fifo" ]; [ -p "$S/pasta/fifo" ]; [ -f "$S/a.txt" ]; [ -f "$S/pasta/sub1/sub2/y.txt" ]
[ -z "$(find "$D" -name '*{TEMP_SUFFIX}' -o -name '*{LINK_PROBE_SUFFIX}*')" ]
"#
        );
        remote_sh(&host, &check).expect("conferencia do destino");
        close(a);
        close(b);
    }

    /// A sessao de origem e encerrada no meio de um arquivo de 64 MiB: o
    /// lote para com o fatal "de origem" (a leitura em voo so falha pelo
    /// prazo do russh-sftp, 10 s; a sonda da sessao emprestada falha na
    /// hora), o temporario e apagado e a sessao de destino segue viva.
    #[test]
    #[ignore]
    fn e2e_copy_between_sessions_source_lost() {
        let host = e2e_host();
        let r = format!("/tmp/sagu-e2e-entre-queda-{}", std::process::id());
        let _cleanup = RemoteCleanup {
            host: host.clone(),
            dir: r.clone(),
        };
        let script = format!(
            r#"set -e; R='{r}'; rm -rf "$R"; mkdir -p "$R/src" "$R/dst"; head -c 67108864 /dev/urandom > "$R/src/grande.bin""#
        );
        remote_sh(&host, &script).expect("preparo da arvore remota");
        let a = sftp_session(&host);
        let b = sftp_session(&host);
        let src = a.session_ref().expect("sessao de origem sem sessao emprestavel");
        let req = PasteRequest {
            op: PasteOp::Copy,
            dest_dir: format!("{r}/dst"),
            items: vec![item(&format!("{r}/src"), "grande.bin")],
        };
        let _cancel = b.copy_from(2, src, req).expect("sessao de destino encerrada");
        let t0 = Instant::now();
        let mut dropped_at = None;
        let rep = wait_finished(&b, 2, 90, |ev| {
            if let PasteEvent::Progress { done, .. } = ev {
                if *done > 0 && dropped_at.is_none() {
                    a.disconnect();
                    dropped_at = Some(Instant::now());
                }
            }
        });
        let dropped_at = dropped_at.expect("a cópia terminou antes do primeiro bloco");
        assert_eq!(rep.fatal.as_deref(), Some(F_SRC_CONNECTION), "{rep:?}");
        assert!(rep.fatal.as_deref().unwrap().contains("origem"));
        assert_eq!((rep.done, rep.saved, rep.files), (0, 0, 1), "{rep:?}");
        assert!(!rep.cancelled);
        // Nem na hora (a leitura em voo espera o prazo) nem para sempre.
        let took = dropped_at.elapsed();
        assert!(took < Duration::from_secs(40), "fatal demorou {took:?}");
        eprintln!("fatal {:?} depois da queda ({:?} no total)", took, t0.elapsed());
        let check = format!(r#"set -e; R='{r}'; ! [ -e "$R/dst/grande.bin" ]; [ -z "$(ls -A "$R/dst")" ]"#);
        remote_sh(&host, &check).expect("destino limpo");
        // A sessao de origem ja fechou; a de destino continua respondendo.
        let t1 = Instant::now();
        loop {
            assert!(t1.elapsed() < Duration::from_secs(10), "a origem nao fechou");
            if let Ok(SftpToUi::Closed) = a.from_sftp.recv_timeout(Duration::from_millis(200)) {
                break;
            }
        }
        b.list_dir(format!("{r}/dst"));
        let t2 = Instant::now();
        loop {
            assert!(t2.elapsed() < Duration::from_secs(10), "o destino parou de responder");
            if let Ok(SftpToUi::Listing { entries, .. }) = b.from_sftp.recv_timeout(Duration::from_millis(200)) {
                assert!(entries.is_empty());
                break;
            }
        }
        close(b);
    }
}
