//! Arrastar itens de um painel SFTP para fora da janela (uma pasta do
//! Explorer, a Area de Trabalho): o arrasto do egui vira um arrasto do
//! Windows (OLE) com "arquivos virtuais" (`CFSTR_FILEDESCRIPTORW` com a lista
//! e `CFSTR_FILECONTENTS` com o conteudo de cada um). Quem recebe pede cada
//! arquivo como um `IStream`, lido aqui pela sessao SFTP do painel de origem,
//! e grava na pasta onde o usuario soltou, com o proprio andamento e as
//! proprias perguntas de "ja existe". A lista (pastas inteiras inclusive) sai
//! da mesma varredura do download (`download::scan_tree`): mesmas regras de
//! nomes, links e arquivos especiais.
//!
//! Threads:
//! - o arrasto (`DoDragDrop`) roda na thread da UI, que o winit ja iniciou no
//!   OLE (STA); dura enquanto o botao estiver apertado fora da janela;
//! - o objeto de dados e os streams sao marshalados a partir do MTA (thread
//!   `sagu-dragout-mta`): os pedidos do Explorer, que e outro processo,
//!   chegam por threads RPC e nunca pela fila de mensagens da UI (ler um
//!   bloco do servidor pode demorar);
//! - o objeto aceita a extracao assincrona (`IDataObjectAsyncCapability`): o
//!   Explorer copia numa thread dele e o `DoDragDrop` volta logo apos soltar;
//! - as leituras SFTP rodam num runtime tokio proprio (`runtime`), pela
//!   sessao emprestada do painel (como a copia entre servidores).
//!
//! Voltar com o cursor para dentro da janela (botao ainda apertado) encerra o
//! arrasto do Windows e o do egui continua: soltar sobre outro painel copia
//! para ele como antes.

use std::cell::Cell;
use std::collections::HashSet;
use std::ffi::c_void;
use std::io::SeekFrom;
use std::mem::ManuallyDrop;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use russh_sftp::client::fs::File;
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::StatusCode;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::{mpsc, watch};
use windows::core::{implement, Interface, Ref, Result, BOOL, HRESULT, PWSTR};
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, DV_E_LINDEX, E_ACCESSDENIED, E_FAIL, E_INVALIDARG, E_NOTIMPL,
    E_OUTOFMEMORY, ERROR_FILE_NOT_FOUND, ERROR_UNEXP_NET_ERR, FILETIME, HGLOBAL, HWND,
    OLE_E_ADVISENOTSUPPORTED, POINT, RECT, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
    STG_E_INVALIDPOINTER, S_OK,
};
use windows::Win32::Graphics::Gdi::{ClientToScreen, ScreenToClient};
use windows::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemAlloc, IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl,
    COINIT_MULTITHREADED, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG,
    STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGM_READ, STGTY_STREAM,
    STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED, TYMED_HGLOBAL,
    TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, ReleaseStgMedium, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::Shell::{
    IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES,
    FD_FILESIZE, FD_PROGRESSUI, FD_WRITESTIME, FILEDESCRIPTORW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetClientRect, GetCursorPos, GetSystemMetrics, WindowFromPoint, GA_ROOT,
    SM_SWAPBUTTON,
};

use crate::download::{self, DownloadReport, Pick, TreeEntry};

/// Entradas (pastas e arquivos) num arrasto para fora. A lista inteira vai
/// para o outro processo de uma vez (592 bytes por entrada): 20 mil ja sao
/// uns 12 MB. Para mais que isso, o Baixar (Ctrl+S) nao tem esse limite.
const MAX_ENTRIES: usize = 20_000;
const F_TOO_MANY: &str = "a seleção tem mais de 20.000 itens; para arrastar, use partes \
     menores (ou o Baixar, Ctrl+S)";
/// Maior caminho relativo que cabe na lista (`cFileName` tem 260 unidades
/// UTF-16, com o terminador).
const MAX_REL_PATH: usize = 259;
const R_PATH_LONG: &str = "caminho longo demais para arrastar (mais de 259 caracteres); \
     use o Baixar (Ctrl+S)";
const F_INTERNAL: &str = "erro interno ao preparar o arrasto";
/// Blocos lidos a frente de quem copia, por arquivo.
const AHEAD: usize = 16;
/// Para o cursor contar como de volta a janela, ele precisa entrar estes
/// pixels alem da borda da area cliente: sem a folga, um cursor parado na
/// borda alternaria entre o arrasto do Windows e o do egui.
const BACK_INSET: i32 = 8;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_NORMAL: u32 = 0x80;
/// Segundos entre 1601-01-01 (FILETIME) e 1970-01-01 (Unix).
const FILETIME_UNIX_OFFSET: u64 = 11_644_473_600;

/// Avisos para a UI, sempre do painel de origem (`pane` e o `Pane::id`).
#[derive(Debug)]
pub enum Event {
    /// Depois de soltar: o que a varredura deixou de fora, ajustou ou nao
    /// conseguiu ler (so quando ha algo a dizer).
    Scanned { pane: u64, report: DownloadReport },
    /// Um arquivo nao pode ser lido enquanto o destino copiava.
    ReadFailed { pane: u64, name: String, msg: String },
}

/// Como terminou um `DragOut::run`.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Alguem aceitou os itens (o Explorer copia em segundo plano).
    Dropped,
    /// Esc, soltou onde nao aceita ou o arrasto nem comecou.
    Cancelled,
    /// O cursor voltou para dentro da janela: segue o arrasto do egui.
    Back,
}

/// Estado repartido entre o arrasto (UI), o objeto de dados e os streams.
struct Shared {
    pane: u64,
    tx: Sender<Event>,
    ctx: egui::Context,
    tree: Mutex<Option<Arc<Tree>>>,
    ready: Condvar,
    dropped: AtomicBool,
    reported: AtomicBool,
}

/// Resultado da varredura.
struct Tree {
    /// Na ordem da lista entregue (o `lindex` do conteudo e o indice aqui).
    entries: Vec<TreeEntry>,
    report: DownloadReport,
}

impl Shared {
    fn send(&self, ev: Event) {
        if self.tx.send(ev).is_ok() {
            self.ctx.request_repaint();
        }
    }

    /// Espera a varredura (quem chama e uma thread RPC do Explorer).
    fn wait_tree(&self) -> Arc<Tree> {
        let mut g = self.tree.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(t) = g.as_ref() {
                return Arc::clone(t);
            }
            g = self.ready.wait(g).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Guarda o resultado (so o primeiro vale) e avisa quem espera.
    fn finish(&self, tree: Tree) {
        {
            let mut g = self.tree.lock().unwrap_or_else(|e| e.into_inner());
            if g.is_some() {
                return;
            }
            *g = Some(Arc::new(tree));
        }
        self.ready.notify_all();
        self.maybe_report();
    }

    /// Avisa a UI do que a varredura deixou de fora, uma vez, so depois de
    /// soltar (cancelar o arrasto nao merece aviso).
    fn maybe_report(&self) {
        if !self.dropped.load(Ordering::SeqCst) {
            return;
        }
        let Some(tree) = self.tree.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        if self.reported.swap(true, Ordering::SeqCst) {
            return;
        }
        let r = &tree.report;
        if r.fatal.is_some() || !r.failed.is_empty() || !r.skipped.is_empty() || !r.renamed.is_empty() {
            self.send(Event::Scanned {
                pane: self.pane,
                report: r.clone(),
            });
        }
    }
}

/// Arrasto para fora de um conjunto de itens: criado na primeira saida da
/// janela e reaproveitado se o cursor voltar e sair de novo (a varredura
/// roda uma vez so). Vive na thread da UI.
pub struct DragOut {
    pane: u64,
    src_dir: String,
    paths: Vec<String>,
    /// Proxy (no STA da UI) do objeto que vive no MTA.
    data: IDataObject,
    shared: Arc<Shared>,
}

impl DragOut {
    /// Prepara o arrasto dos `items` (caminho remoto, nome) da pasta
    /// `src_dir` do painel `pane`, lidos por `session`, e comeca a varredura.
    /// `None` se o COM recusar (o arrasto segue so dentro da janela).
    pub fn new(
        pane: u64,
        session: Arc<SftpSession>,
        src_dir: &str,
        items: &[(String, String)],
        tx: Sender<Event>,
        ctx: egui::Context,
    ) -> Option<DragOut> {
        let picks: Vec<Pick> = items
            .iter()
            .map(|(remote, name)| Pick {
                remote: remote.clone(),
                name: name.clone(),
            })
            .collect();
        let (list, invalid) = download::local_items(&picks);
        let shared = Arc::new(Shared {
            pane,
            tx,
            ctx,
            tree: Mutex::new(None),
            ready: Condvar::new(),
            dropped: AtomicBool::new(false),
            reported: AtomicBool::new(false),
        });
        let (cancel, cancel_rx) = download::cancel_pair();
        let obj_shared = Arc::clone(&shared);
        let obj_session = Arc::clone(&session);
        let marshaled = in_mta(move || -> Result<SendStream> {
            let obj: IDataObject = RemoteFiles {
                shared: obj_shared,
                session: obj_session,
                _scan: cancel,
                stored: Mutex::new(Vec::new()),
                async_mode: AtomicBool::new(true),
                in_op: AtomicBool::new(false),
            }
            .into();
            let s = unsafe { CoMarshalInterThreadInterfaceInStream(&IDataObject::IID, &obj)? };
            Ok(SendStream(s))
        })?
        .ok()?;
        let data: IDataObject = unsafe { CoGetInterfaceAndReleaseStream(&marshaled.0) }.ok()?;

        // Varredura ja no arrasto: ao soltar, a lista costuma estar pronta.
        let scan_shared = Arc::clone(&shared);
        let scan = runtime().spawn(async move {
            let (plan, mut report) =
                download::scan_tree(&session, &list, MAX_ENTRIES, F_TOO_MANY, cancel_rx).await;
            report.skipped.splice(0..0, invalid);
            let entries = match plan {
                Some(plan) => fit_paths(plan, &mut report),
                None => Vec::new(),
            };
            scan_shared.finish(Tree { entries, report });
        });
        // Se a varredura cair (panico), quem espera pela lista nao fica preso.
        let guard_shared = Arc::clone(&shared);
        runtime().spawn(async move {
            if scan.await.is_err() {
                guard_shared.finish(Tree {
                    entries: Vec::new(),
                    report: DownloadReport {
                        fatal: Some(F_INTERNAL.into()),
                        ..Default::default()
                    },
                });
            }
        });
        Some(DragOut {
            pane,
            src_dir: src_dir.to_string(),
            paths: items.iter().map(|(p, _)| p.clone()).collect(),
            data,
            shared,
        })
    }

    /// E deste conjunto de itens (do mesmo arrasto do egui)?
    pub fn is_for(&self, pane: u64, src_dir: &str, paths: &[&str]) -> bool {
        self.pane == pane
            && self.src_dir == src_dir
            && self.paths.len() == paths.len()
            && self.paths.iter().zip(paths).all(|(a, b)| a == b)
    }

    /// O arrasto do Windows, ate soltar, cancelar ou voltar para a janela
    /// `hwnd`. Bloqueia a UI enquanto o botao estiver apertado fora dela.
    pub fn run(&self, hwnd: isize) -> Outcome {
        let back = Rc::new(Cell::new(false));
        let source: IDropSource = DropSource {
            hwnd: HWND(hwnd as *mut c_void),
            back: Rc::clone(&back),
        }
        .into();
        let mut effect = DROPEFFECT_NONE;
        let hr = unsafe { DoDragDrop(&self.data, &source, DROPEFFECT_COPY, &mut effect) };
        if hr == DRAGDROP_S_DROP && effect.0 & DROPEFFECT_COPY.0 != 0 {
            Outcome::Dropped
        } else if hr == DRAGDROP_S_CANCEL && back.get() {
            Outcome::Back
        } else {
            Outcome::Cancelled
        }
    }

    /// Os itens foram soltos: avisos da varredura passam a valer.
    pub fn dropped(self) {
        self.shared.dropped.store(true, Ordering::SeqCst);
        self.shared.maybe_report();
    }
}

/// Tira da lista o que nao cabe em `cFileName` (com tudo o que estiver
/// dentro de uma pasta tirada), anotando como ignorado.
fn fit_paths(plan: Vec<TreeEntry>, report: &mut DownloadReport) -> Vec<TreeEntry> {
    let mut out = Vec::with_capacity(plan.len());
    let mut cut: HashSet<Vec<String>> = HashSet::new();
    for e in plan {
        let inside = e
            .rel
            .split_last()
            .is_some_and(|(_, parent)| !parent.is_empty() && cut.contains(parent));
        let long = !inside && rel_units(&e.rel) > MAX_REL_PATH;
        if inside || long {
            if long {
                report.skipped.push((e.shown.clone(), R_PATH_LONG.into()));
            }
            if e.file.is_none() {
                cut.insert(e.rel);
            }
            continue;
        }
        out.push(e);
    }
    out
}

/// Tamanho em UTF-16 do caminho relativo com `\`.
fn rel_units(rel: &[String]) -> usize {
    rel.iter().map(|c| c.encode_utf16().count()).sum::<usize>() + rel.len().saturating_sub(1)
}

fn filetime(unix: u32) -> FILETIME {
    let t = (u64::from(unix) + FILETIME_UNIX_OFFSET) * 10_000_000;
    FILETIME {
        dwLowDateTime: t as u32,
        dwHighDateTime: (t >> 32) as u32,
    }
}

/// Bytes de um FILEGROUPDESCRIPTORW: a contagem e um FILEDESCRIPTORW por
/// entrada (ao menos um, como na estrutura do Windows).
fn descriptor_bytes(entries: &[TreeEntry]) -> Vec<u8> {
    let one = std::mem::size_of::<FILEDESCRIPTORW>();
    let mut out = Vec::with_capacity(4 + one * entries.len().max(1));
    out.extend_from_slice(&(entries.len() as u32).to_ne_bytes());
    for e in entries {
        let mut d = FILEDESCRIPTORW::default();
        let mut flags = FD_ATTRIBUTES.0 | FD_PROGRESSUI.0;
        let mut name = [0u16; 260];
        for (slot, u) in name
            .iter_mut()
            .take(MAX_REL_PATH)
            .zip(e.rel.join("\\").encode_utf16())
        {
            *slot = u;
        }
        d.cFileName = name;
        match e.file {
            None => d.dwFileAttributes = ATTR_DIRECTORY,
            Some((size, mtime)) => {
                d.dwFileAttributes = ATTR_NORMAL;
                flags |= FD_FILESIZE.0;
                d.nFileSizeHigh = (size >> 32) as u32;
                d.nFileSizeLow = size as u32;
                if let Some(t) = mtime.filter(|t| *t > 0) {
                    flags |= FD_WRITESTIME.0;
                    d.ftLastWriteTime = filetime(t);
                }
            }
        }
        d.dwFlags = flags as u32;
        // SAFETY: FILEDESCRIPTORW e `repr(C, packed)` de inteiros e
        // arrays: qualquer byte e valido e nao ha preenchimento.
        let bytes = unsafe { std::slice::from_raw_parts((&d as *const FILEDESCRIPTORW).cast::<u8>(), one) };
        out.extend_from_slice(bytes);
    }
    if entries.is_empty() {
        out.resize(4 + one, 0);
    }
    out
}

/// Formatos registrados da area de transferencia usados aqui.
struct Formats {
    descriptor: u16,
    contents: u16,
    preferred: u16,
}

fn formats() -> &'static Formats {
    static F: OnceLock<Formats> = OnceLock::new();
    F.get_or_init(|| unsafe {
        Formats {
            descriptor: RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16,
            contents: RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16,
            preferred: RegisterClipboardFormatW(CFSTR_PREFERREDDROPEFFECT) as u16,
        }
    })
}

fn has_tymed(fmt: &FORMATETC, t: TYMED) -> bool {
    fmt.tymed & t.0 as u32 != 0
}

fn format_etc(cf: u16, t: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: t.0 as u32,
    }
}

/// Copia `bytes` num HGLOBAL novo (de quem recebe o STGMEDIUM).
fn hglobal_medium(bytes: &[u8]) -> Result<STGMEDIUM> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1))?;
        let p = GlobalLock(h);
        if p.is_null() {
            let _ = windows::Win32::Foundation::GlobalFree(Some(h));
            return Err(E_OUTOFMEMORY.into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        // Devolve "erro" ao chegar a zero travas; nao e falha.
        let _ = GlobalUnlock(h);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: h },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

/// Conteudo de um HGLOBAL recebido (no maximo `max` bytes; `None` se maior).
fn hglobal_bytes(h: HGLOBAL, max: usize) -> Option<Vec<u8>> {
    unsafe {
        let n = GlobalSize(h);
        if n == 0 || n > max {
            return None;
        }
        let p = GlobalLock(h);
        if p.is_null() {
            return None;
        }
        let v = std::slice::from_raw_parts(p.cast::<u8>(), n).to_vec();
        let _ = GlobalUnlock(h);
        Some(v)
    }
}

/// Maior dado guardado por `SetData` (as imagens do arrasto do Explorer
/// cabem com folga) e quantos formatos no maximo.
const STORED_MAX: usize = 4 * 1024 * 1024;
const STORED_FORMATS: usize = 32;

/// Objeto de dados do arrasto (vive no MTA; ver o topo do modulo).
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct RemoteFiles {
    shared: Arc<Shared>,
    session: Arc<SftpSession>,
    /// Solto junto com o objeto: interrompe a varredura.
    _scan: download::Cancel,
    /// O que quem recebe guardou no objeto (`SetData`: descricao do
    /// arrasto, efeito realizado...), devolvido a quem pedir.
    stored: Mutex<Vec<(u16, Vec<u8>)>>,
    async_mode: AtomicBool,
    in_op: AtomicBool,
}

impl IDataObject_Impl for RemoteFiles_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        let fmt = unsafe { pformatetcin.as_ref() }.ok_or(windows::core::Error::from(E_INVALIDARG))?;
        let f = formats();
        if fmt.cfFormat == f.descriptor && has_tymed(fmt, TYMED_HGLOBAL) {
            let tree = self.shared.wait_tree();
            if tree.report.fatal.is_some() {
                return Err(E_FAIL.into());
            }
            return hglobal_medium(&descriptor_bytes(&tree.entries));
        }
        if fmt.cfFormat == f.contents && has_tymed(fmt, TYMED_ISTREAM) {
            let tree = self.shared.wait_tree();
            let entry = usize::try_from(fmt.lindex)
                .ok()
                .and_then(|i| tree.entries.get(i))
                .ok_or(windows::core::Error::from(DV_E_LINDEX))?;
            let Some((size, mtime)) = entry.file else {
                return Err(DV_E_LINDEX.into());
            };
            let stream: IStream = RemoteStream {
                session: Arc::clone(&self.session),
                remote: entry.remote.clone(),
                name: entry.rel.last().cloned().unwrap_or_default(),
                shown: entry.shown.clone(),
                size,
                mtime,
                shared: Arc::clone(&self.shared),
                state: Mutex::new(StreamState::default()),
            }
            .into();
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }
        if fmt.cfFormat == f.preferred && has_tymed(fmt, TYMED_HGLOBAL) {
            return hglobal_medium(&DROPEFFECT_COPY.0.to_ne_bytes());
        }
        if has_tymed(fmt, TYMED_HGLOBAL) {
            let stored = self.stored.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, bytes)) = stored.iter().find(|(cf, _)| *cf == fmt.cfFormat) {
                return hglobal_medium(bytes);
            }
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        let Some(fmt) = (unsafe { pformatetc.as_ref() }) else {
            return E_INVALIDARG;
        };
        let f = formats();
        let ok = (fmt.cfFormat == f.descriptor && has_tymed(fmt, TYMED_HGLOBAL))
            || (fmt.cfFormat == f.contents && has_tymed(fmt, TYMED_ISTREAM))
            || (fmt.cfFormat == f.preferred && has_tymed(fmt, TYMED_HGLOBAL))
            || (has_tymed(fmt, TYMED_HGLOBAL)
                && self
                    .stored
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .any(|(cf, _)| *cf == fmt.cfFormat));
        if ok {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, pformatetc: *const FORMATETC, pmedium: *const STGMEDIUM, frelease: BOOL) -> Result<()> {
        let (Some(fmt), Some(medium)) = (unsafe { pformatetc.as_ref() }, unsafe { pmedium.as_ref() }) else {
            return Err(E_INVALIDARG.into());
        };
        let f = formats();
        if medium.tymed != TYMED_HGLOBAL.0 as u32 || fmt.cfFormat == f.descriptor || fmt.cfFormat == f.contents {
            return Err(E_NOTIMPL.into());
        }
        let Some(bytes) = hglobal_bytes(unsafe { medium.u.hGlobal }, STORED_MAX) else {
            return Err(E_OUTOFMEMORY.into());
        };
        {
            let mut stored = self.stored.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(slot) = stored.iter_mut().find(|(cf, _)| *cf == fmt.cfFormat) {
                slot.1 = bytes;
            } else if stored.len() < STORED_FORMATS {
                stored.push((fmt.cfFormat, bytes));
            } else {
                return Err(E_OUTOFMEMORY.into());
            }
        }
        // Aceito com `fRelease`: o meio passa a ser nosso.
        if frelease.as_bool() {
            unsafe { ReleaseStgMedium(pmedium.cast_mut()) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let f = formats();
        let list = [
            format_etc(f.descriptor, TYMED_HGLOBAL),
            format_etc(f.contents, TYMED_ISTREAM),
            format_etc(f.preferred, TYMED_HGLOBAL),
        ];
        unsafe { SHCreateStdEnumFmtEtc(&list) }
    }

    fn DAdvise(&self, _pformatetc: *const FORMATETC, _advf: u32, _padvsink: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// Extracao assincrona: o Explorer copia numa thread dele, sem segurar o
/// `DoDragDrop` (e a UI) ate o fim da copia.
impl IDataObjectAsyncCapability_Impl for RemoteFiles_Impl {
    fn SetAsyncMode(&self, fdoopasync: BOOL) -> Result<()> {
        self.async_mode.store(fdoopasync.as_bool(), Ordering::SeqCst);
        Ok(())
    }

    fn GetAsyncMode(&self) -> Result<BOOL> {
        Ok(self.async_mode.load(Ordering::SeqCst).into())
    }

    fn StartOperation(&self, _pbcreserved: Ref<IBindCtx>) -> Result<()> {
        self.in_op.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn InOperation(&self) -> Result<BOOL> {
        Ok(self.in_op.load(Ordering::SeqCst).into())
    }

    fn EndOperation(&self, _hresult: HRESULT, _pbcreserved: Ref<IBindCtx>, _dweffects: u32) -> Result<()> {
        self.in_op.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Mensagem da tarefa que le um arquivo para o stream.
enum Msg {
    Data(Vec<u8>),
    End,
    Fail { hr: HRESULT, msg: String },
}

/// Leitura em andamento a partir de uma posicao.
struct Fetch {
    rx: mpsc::Receiver<Msg>,
    chunk: Vec<u8>,
    off: usize,
    done: bool,
    /// Solto (stream liberado ou Seek): a tarefa para e fecha o arquivo.
    _stop: watch::Sender<bool>,
}

#[derive(Default)]
struct StreamState {
    /// Bytes ja entregues a quem le.
    pos: u64,
    fetch: Option<Fetch>,
    /// A falha ja foi avisada a UI (uma vez por arquivo).
    reported: bool,
}

/// Conteudo de um arquivo remoto, lido sob demanda (vive no MTA).
#[implement(IStream)]
struct RemoteStream {
    session: Arc<SftpSession>,
    remote: String,
    /// Nome local (ultimo componente).
    name: String,
    /// Caminho remoto relativo, para a mensagem de erro.
    shown: String,
    size: u64,
    mtime: Option<u32>,
    shared: Arc<Shared>,
    state: Mutex<StreamState>,
}

impl RemoteStream {
    /// Le ate encher `out` (menos so no fim do arquivo). Depois de uma falha,
    /// a proxima leitura reabre o arquivo na mesma posicao ("Tentar de novo"
    /// do Explorer).
    fn read_into(&self, out: &mut [u8]) -> std::result::Result<usize, HRESULT> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let StreamState { pos, fetch, reported } = &mut *st;
        let mut n = 0;
        while n < out.len() {
            let f = fetch.get_or_insert_with(|| self.start_fetch(*pos));
            if f.off < f.chunk.len() {
                let k = (f.chunk.len() - f.off).min(out.len() - n);
                out[n..n + k].copy_from_slice(&f.chunk[f.off..f.off + k]);
                f.off += k;
                n += k;
                *pos += k as u64;
                continue;
            }
            if f.done {
                break;
            }
            let (hr, msg) = match f.rx.blocking_recv() {
                Some(Msg::Data(c)) => {
                    f.chunk = c;
                    f.off = 0;
                    continue;
                }
                Some(Msg::End) => {
                    f.done = true;
                    continue;
                }
                Some(Msg::Fail { hr, msg }) => (hr, msg),
                // A tarefa sumiu sem dizer o motivo.
                None => (HRESULT::from_win32(ERROR_UNEXP_NET_ERR.0), download::F_CONNECTION.to_string()),
            };
            *fetch = None;
            if !*reported {
                *reported = true;
                self.shared.send(Event::ReadFailed {
                    pane: self.shared.pane,
                    name: self.shown.clone(),
                    msg,
                });
            }
            // O que ja foi copiado vale; a falha aparece na proxima leitura.
            return if n > 0 { Ok(n) } else { Err(hr) };
        }
        Ok(n)
    }

    fn start_fetch(&self, pos: u64) -> Fetch {
        let (tx, rx) = mpsc::channel(AHEAD);
        let (stop, stop_rx) = watch::channel(false);
        let session = Arc::clone(&self.session);
        let remote = self.remote.clone();
        runtime().spawn(async move {
            if let Some(msg) = pump(&session, remote, pos, &tx, stop_rx).await {
                let _ = tx.send(msg).await;
            }
        });
        Fetch {
            rx,
            chunk: Vec::new(),
            off: 0,
            done: false,
            _stop: stop,
        }
    }
}

/// Abre o arquivo, le a partir de `pos` para `tx` e fecha. `None` = quem
/// lia desistiu (nada mais a mandar).
async fn pump(
    session: &SftpSession,
    remote: String,
    pos: u64,
    tx: &mpsc::Sender<Msg>,
    mut stop: watch::Receiver<bool>,
) -> Option<Msg> {
    let mut file = match download::unless_cancelled(&mut stop, session.open(remote)).await? {
        Ok(f) => f,
        Err(e) => return Some(open_failure(&e)),
    };
    let res = copy_out(&mut file, pos, tx, &mut stop).await;
    // Fecha esperando o CLOSE (ver `download::close_remote`), mesmo com quem
    // lia ja longe: cada handle solto sem resposta conta no limite do servidor.
    let (_keep, mut never) = watch::channel(false);
    download::close_remote(file, &mut never).await;
    res
}

async fn copy_out(
    file: &mut File,
    pos: u64,
    tx: &mpsc::Sender<Msg>,
    stop: &mut watch::Receiver<bool>,
) -> Option<Msg> {
    if pos > 0 {
        if let Err(e) = download::unless_cancelled(stop, file.seek(SeekFrom::Start(pos))).await? {
            return Some(read_failure(&e));
        }
    }
    loop {
        let mut buf = vec![0u8; download::CHUNK];
        let n = match download::unless_cancelled(stop, file.read(&mut buf)).await? {
            Ok(n) => n,
            Err(e) => return Some(read_failure(&e)),
        };
        if n == 0 {
            return Some(Msg::End);
        }
        buf.truncate(n);
        download::unless_cancelled(stop, tx.send(Msg::Data(buf))).await?.ok()?;
    }
}

fn open_failure(e: &russh_sftp::client::error::Error) -> Msg {
    use russh_sftp::client::error::Error;
    match e {
        Error::Status(s) if s.status_code == StatusCode::PermissionDenied => Msg::Fail {
            hr: E_ACCESSDENIED,
            msg: "sem permissão para ler no servidor".into(),
        },
        Error::Status(s) if s.status_code == StatusCode::NoSuchFile => Msg::Fail {
            hr: HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0),
            msg: "o arquivo não existe mais no servidor".into(),
        },
        other => Msg::Fail {
            hr: HRESULT::from_win32(ERROR_UNEXP_NET_ERR.0),
            msg: format!(
                "não foi possível ler: {}",
                download::safe_text(&other.to_string(), download::MAX_REMOTE_MSG)
            ),
        },
    }
}

fn read_failure(e: &std::io::Error) -> Msg {
    Msg::Fail {
        hr: HRESULT::from_win32(ERROR_UNEXP_NET_ERR.0),
        msg: format!(
            "erro ao ler do servidor: {}",
            download::safe_text(&e.to_string(), download::MAX_REMOTE_MSG)
        ),
    }
}

/// Texto em memoria do COM (de quem recebe o STATSTG).
fn co_wide(s: &str) -> PWSTR {
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let p = CoTaskMemAlloc(w.len() * 2).cast::<u16>();
        if p.is_null() {
            return PWSTR::null();
        }
        std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
        PWSTR(p)
    }
}

impl ISequentialStream_Impl for RemoteStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return STG_E_INVALIDPOINTER;
        }
        let out = unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) };
        let (n, hr) = match self.read_into(out) {
            Ok(n) => (n, S_OK),
            Err(hr) => (0, hr),
        };
        if let Some(r) = unsafe { pcbread.as_mut() } {
            *r = n as u32;
        }
        hr
    }

    fn Write(&self, _pv: *const c_void, _cb: u32, pcbwritten: *mut u32) -> HRESULT {
        if let Some(w) = unsafe { pcbwritten.as_mut() } {
            *w = 0;
        }
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for RemoteStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let base = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => st.pos as i64,
            STREAM_SEEK_END => self.size as i64,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let new = base
            .checked_add(dlibmove)
            .filter(|p| *p >= 0)
            .ok_or(windows::core::Error::from(STG_E_INVALIDFUNCTION))? as u64;
        if new != st.pos {
            // Outra posicao: a leitura recomeca de la (a atual para).
            st.fetch = None;
            st.pos = new;
        }
        if let Some(p) = unsafe { plibnewposition.as_mut() } {
            *p = new;
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn CopyTo(&self, pstm: Ref<IStream>, cb: u64, pcbread: *mut u64, pcbwritten: *mut u64) -> Result<()> {
        let dst = pstm.ok()?;
        let mut buf = vec![0u8; download::CHUNK];
        let (mut read, mut written) = (0u64, 0u64);
        let res = loop {
            if read >= cb {
                break Ok(());
            }
            let want = (cb - read).min(buf.len() as u64) as usize;
            let n = match self.read_into(&mut buf[..want]) {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(hr) => break Err(hr.into()),
            };
            read += n as u64;
            let mut w = 0u32;
            let hr = unsafe { dst.Write(buf.as_ptr().cast(), n as u32, Some(&mut w)) };
            written += u64::from(w);
            if hr.is_err() {
                break Err(hr.into());
            }
        };
        if let Some(r) = unsafe { pcbread.as_mut() } {
            *r = read;
        }
        if let Some(w) = unsafe { pcbwritten.as_mut() } {
            *w = written;
        }
        res
    }

    fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
        Ok(())
    }

    fn Revert(&self) -> Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> Result<()> {
        let s = unsafe { pstatstg.as_mut() }.ok_or(windows::core::Error::from(STG_E_INVALIDPOINTER))?;
        *s = STATSTG::default();
        s.r#type = STGTY_STREAM.0 as u32;
        s.cbSize = self.size;
        s.grfMode = STGM_READ;
        if let Some(t) = self.mtime.filter(|t| *t > 0) {
            s.mtime = filetime(t);
        }
        if grfstatflag.0 & STATFLAG_NONAME.0 == 0 {
            s.pwcsName = co_wide(&self.name);
        }
        Ok(())
    }

    fn Clone(&self) -> Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// Origem do arrasto (na thread da UI): solta com o botao, cancela com Esc
/// ou quando o cursor volta para dentro da janela.
#[implement(IDropSource)]
struct DropSource {
    hwnd: HWND,
    back: Rc<Cell<bool>>,
}

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if grfkeystate.0 & MK_LBUTTON.0 == 0 {
            // Soltou sobre a propria janela: e com o arrasto do egui.
            if cursor_over(self.hwnd, 0) {
                self.back.set(true);
                return DRAGDROP_S_CANCEL;
            }
            return DRAGDROP_S_DROP;
        }
        if cursor_over(self.hwnd, BACK_INSET) {
            self.back.set(true);
            return DRAGDROP_S_CANCEL;
        }
        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// O cursor esta sobre a janela `hwnd` (nao coberto por outra), a pelo menos
/// `inset` pixels das bordas da area cliente quando `inset > 0`?
fn cursor_over(hwnd: HWND, inset: i32) -> bool {
    unsafe {
        let mut pt = POINT::default();
        if GetCursorPos(&mut pt).is_err() {
            return false;
        }
        let under = WindowFromPoint(pt);
        if under.is_invalid() || GetAncestor(under, GA_ROOT) != hwnd {
            return false;
        }
        if inset <= 0 {
            return true;
        }
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return false;
        }
        let mut tl = POINT {
            x: rc.left,
            y: rc.top,
        };
        if !ClientToScreen(hwnd, &mut tl).as_bool() {
            return false;
        }
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        pt.x >= tl.x + inset && pt.x < tl.x + w - inset && pt.y >= tl.y + inset && pt.y < tl.y + h - inset
    }
}

/// O cursor saiu da janela do app (esta sobre outra janela ou a area de
/// trabalho)?
pub fn cursor_outside(hwnd: isize) -> bool {
    !cursor_over(HWND(hwnd as *mut c_void), 0)
}

/// O botao principal do mouse esta apertado agora (estado fisico, com os
/// botoes trocados para canhotos)?
pub fn button_down() -> bool {
    unsafe {
        let vk = if GetSystemMetrics(SM_SWAPBUTTON) != 0 {
            VK_RBUTTON
        } else {
            VK_LBUTTON
        };
        GetAsyncKeyState(i32::from(vk.0)) as u16 & 0x8000 != 0
    }
}

/// Posicao do cursor na area cliente da janela, em pixels fisicos.
pub fn cursor_client_px(hwnd: isize) -> Option<(f32, f32)> {
    unsafe {
        let mut pt = POINT::default();
        GetCursorPos(&mut pt).ok()?;
        ScreenToClient(HWND(hwnd as *mut c_void), &mut pt)
            .as_bool()
            .then_some((pt.x as f32, pt.y as f32))
    }
}

/// Runtime das leituras do arrasto (as sessoes SFTP vivem nas threads dos
/// paineis; aqui so se espera por elas).
fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("sagu-dragout")
            .enable_all()
            .build()
            .expect("runtime do arrastar para fora")
    })
}

/// IStream com o objeto marshalado, levado da thread do MTA para a da UI
/// (feito para isso: `CoMarshalInterThreadInterfaceInStream`).
struct SendStream(IStream);
// SAFETY: o stream so carrega os bytes do marshaling e e consumido uma vez,
// por `CoGetInterfaceAndReleaseStream`, na thread de destino.
unsafe impl Send for SendStream {}

type MtaJob = Box<dyn FnOnce() + Send>;

/// Roda `f` na thread do MTA e espera o resultado. A thread fica no MTA
/// enquanto o processo viver: os objetos marshalados de la seguem
/// atendendo o Explorer depois que o arrasto acaba.
fn in_mta<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    static MTA: OnceLock<Option<Sender<MtaJob>>> = OnceLock::new();
    let tx = MTA
        .get_or_init(|| {
            let (tx, rx) = std::sync::mpsc::channel::<MtaJob>();
            std::thread::Builder::new()
                .name("sagu-dragout-mta".into())
                .spawn(move || {
                    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                    for job in rx {
                        job();
                    }
                })
                .ok()?;
            Some(tx)
        })
        .as_ref()?;
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    tx.send(Box::new(move || {
        let _ = done_tx.send(f());
    }))
    .ok()?;
    done_rx.recv().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(rel: &[&str], file: Option<(u64, Option<u32>)>) -> TreeEntry {
        TreeEntry {
            remote: format!("/srv/{}", rel.join("/")),
            rel: rel.iter().map(|s| s.to_string()).collect(),
            shown: rel.join("/"),
            file,
        }
    }

    #[test]
    fn descriptor_layout_matches_windows() {
        assert_eq!(std::mem::size_of::<FILEDESCRIPTORW>(), 592);
        let entries = vec![
            entry(&["pasta"], None),
            entry(&["pasta", "nota.txt"], Some((5_000_000_000, Some(1_700_000_000)))),
        ];
        let b = descriptor_bytes(&entries);
        assert_eq!(b.len(), 4 + 2 * 592);
        assert_eq!(u32::from_ne_bytes(b[0..4].try_into().unwrap()), 2);
        let d: FILEDESCRIPTORW = unsafe { std::ptr::read_unaligned(b[4 + 592..].as_ptr().cast()) };
        let flags = d.dwFlags as i32;
        assert_ne!(flags & FD_FILESIZE.0, 0);
        assert_ne!(flags & FD_WRITESTIME.0, 0);
        assert_eq!({ d.dwFileAttributes }, ATTR_NORMAL);
        let size = (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow);
        assert_eq!(size, 5_000_000_000);
        let name = { d.cFileName };
        let len = name.iter().position(|&c| c == 0).unwrap();
        assert_eq!(String::from_utf16(&name[..len]).unwrap(), "pasta\\nota.txt");
        let dir: FILEDESCRIPTORW = unsafe { std::ptr::read_unaligned(b[4..].as_ptr().cast()) };
        assert_eq!({ dir.dwFileAttributes }, ATTR_DIRECTORY);
        assert_eq!(dir.dwFlags as i32 & FD_FILESIZE.0, 0);
        // Lista vazia ainda tem o tamanho da estrutura do Windows.
        assert_eq!(descriptor_bytes(&[]).len(), 4 + 592);
    }

    #[test]
    fn filetime_from_unix() {
        // 2023-11-14 22:13:20 UTC = 133.444.736.000.000.000 em FILETIME.
        let ft = filetime(1_700_000_000);
        let t = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
        assert_eq!(t, 133_444_736_000_000_000);
    }

    #[test]
    fn long_paths_are_cut_with_their_contents() {
        // "a\\" + 258 = 260 unidades: uma a mais que cabe.
        let long = "x".repeat(258);
        let plan = vec![
            entry(&["a"], None),
            entry(&["a", long.as_str()], None),
            entry(&["a", long.as_str(), "f1"], Some((1, None))),
            entry(&["a", long.as_str(), "sub"], None),
            entry(&["a", long.as_str(), "sub", "f2"], Some((1, None))),
            entry(&["a", "ok.txt"], Some((1, None))),
        ];
        let mut report = DownloadReport::default();
        let out = fit_paths(plan, &mut report);
        let kept: Vec<String> = out.iter().map(|e| e.rel.join("\\")).collect();
        assert_eq!(kept, ["a", "a\\ok.txt"]);
        // So a pasta longa e anotada, nao cada item dentro dela.
        assert_eq!(report.skipped.len(), 1, "{:?}", report.skipped);
        assert_eq!(report.skipped[0].1, R_PATH_LONG);
    }

    #[test]
    fn rel_units_counts_separators() {
        assert_eq!(rel_units(&["ab".into(), "c".into()]), 4);
        assert_eq!(rel_units(&["ção".into()]), 3);
        assert_eq!(rel_units(&[]), 0);
    }

    // --- Ponta a ponta contra um sshd real (ignorado) ----------------------
    //
    // Mesmas variaveis dos outros e2e (SAGU_E2E_PORT, SAGU_E2E_USER,
    // SAGU_E2E_KEY). Faz o papel do Explorer sem o mouse: a thread do teste e
    // um STA (como a UI) e pede a lista e o conteudo pelo proxy, entao os
    // pedidos atravessam o marshaling ate o objeto no MTA.
    // Rodar com: cargo test e2e_drag_out -- --ignored

    use crate::download::tests::{close, e2e_host, remote_sh, sftp_session};
    use crate::vault::Host;
    use std::time::{Duration, Instant};
    use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};

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

    /// (caminho relativo, e pasta, tamanho, FILETIME da modificacao) de cada
    /// entrada da lista.
    fn parse_descriptor(b: &[u8]) -> Vec<(String, bool, u64, u64)> {
        let n = u32::from_ne_bytes(b[0..4].try_into().unwrap()) as usize;
        let one = std::mem::size_of::<FILEDESCRIPTORW>();
        assert!(b.len() >= 4 + n * one, "lista curta: {} bytes para {n}", b.len());
        (0..n)
            .map(|i| {
                let d: FILEDESCRIPTORW =
                    unsafe { std::ptr::read_unaligned(b[4 + i * one..].as_ptr().cast()) };
                let name = { d.cFileName };
                let len = name.iter().position(|&c| c == 0).unwrap();
                let size = (u64::from(d.nFileSizeHigh) << 32) | u64::from(d.nFileSizeLow);
                let ft = { d.ftLastWriteTime };
                let t = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
                (
                    String::from_utf16(&name[..len]).unwrap(),
                    d.dwFileAttributes & ATTR_DIRECTORY != 0,
                    size,
                    t,
                )
            })
            .collect()
    }

    fn get_stream(data: &IDataObject, index: usize) -> Result<IStream> {
        let mut fmt = format_etc(formats().contents, TYMED_ISTREAM);
        fmt.lindex = index as i32;
        let mut medium = unsafe { data.GetData(&fmt) }?;
        assert_eq!(medium.tymed, TYMED_ISTREAM.0 as u32);
        Ok(unsafe { ManuallyDrop::take(&mut medium.u.pstm) }.expect("stream vazio"))
    }

    /// Le o stream ate o fim em pedidos de `cb` bytes.
    fn read_all(s: &IStream, cb: usize) -> std::result::Result<Vec<u8>, HRESULT> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; cb];
        loop {
            let mut n = 0u32;
            let hr = unsafe { s.Read(buf.as_mut_ptr().cast(), cb as u32, Some(&mut n)) };
            if hr.is_err() {
                return Err(hr);
            }
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
    }

    /// A lista (pastas inteiras, nomes ajustados, links e fifo fora) e o
    /// conteudo de cada arquivo, com Seek e Stat; os avisos so depois de
    /// soltar; um arquivo apagado no meio vira erro de leitura com aviso.
    #[test]
    #[ignore]
    fn e2e_drag_out_reads_tree() {
        let host = e2e_host();
        let r = format!("/tmp/sagu-e2e-out-{}", std::process::id());
        let _cleanup = RemoteCleanup {
            host: host.clone(),
            dir: r.clone(),
        };
        let script = format!(
            r#"set -e; R='{r}'; rm -rf "$R"; mkdir -p "$R/pasta/sub"
printf 'conteudo a' > "$R/a.txt"; touch -d '2020-01-02 03:04:05 UTC' "$R/a.txt"
printf y > "$R/pasta/sub/y.txt"; printf z > "$R/pasta/a:b.txt"
ln -s sub "$R/pasta/link-pasta"; mkfifo "$R/pasta/fifo"
seq 1 400000 > "$R/grande.txt"
"#
        );
        remote_sh(&host, &script).expect("preparo da arvore remota");
        let grande: Vec<u8> = (1..=400_000).map(|i| format!("{i}\n")).collect::<String>().into_bytes();

        unsafe { OleInitialize(None) }.expect("OleInitialize");
        let h = sftp_session(&host);
        let session = h.session_ref().expect("sem sessao emprestavel").session();
        let (tx, rx) = std::sync::mpsc::channel();
        let items: Vec<(String, String)> = ["a.txt", "pasta", "grande.txt"]
            .iter()
            .map(|n| (format!("{r}/{n}"), n.to_string()))
            .collect();
        let out = DragOut::new(9, session, &r, &items, tx, egui::Context::default())
            .expect("objeto de dados");
        assert!(out.is_for(9, &r, &items.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>()));

        // Lista (espera a varredura).
        let fmt = format_etc(formats().descriptor, TYMED_HGLOBAL);
        assert_eq!(unsafe { out.data.QueryGetData(&fmt) }, S_OK);
        let mut medium = unsafe { out.data.GetData(&fmt) }.expect("lista");
        let bytes = hglobal_bytes(unsafe { medium.u.hGlobal }, usize::MAX).unwrap();
        unsafe { ReleaseStgMedium(&mut medium) };
        let list = parse_descriptor(&bytes);
        let names: Vec<&str> = list.iter().map(|(n, ..)| n.as_str()).collect();
        assert_eq!(
            names,
            ["a.txt", "pasta", "pasta\\a_b.txt", "pasta\\sub", "pasta\\sub\\y.txt", "grande.txt"]
        );
        assert!(list[1].1 && list[3].1 && !list[0].1 && !list[5].1, "{list:?}");
        assert_eq!(list[0].2, 10);
        // 2020-01-02 03:04:05 UTC.
        assert_eq!(list[0].3, (1_577_934_245 + FILETIME_UNIX_OFFSET) * 10_000_000);
        assert_eq!(list[5].2, grande.len() as u64);

        // Conteudo pelo stream (pedidos de tamanhos variados).
        let a = get_stream(&out.data, 0).expect("stream de a.txt");
        assert_eq!(read_all(&a, 3).unwrap(), b"conteudo a");
        assert_eq!(read_all(&get_stream(&out.data, 2).unwrap(), 64 * 1024).unwrap(), b"z");
        let g = get_stream(&out.data, 5).expect("stream de grande.txt");
        let t0 = Instant::now();
        assert!(read_all(&g, 1 << 20).unwrap() == grande, "grande.txt diferente");
        eprintln!("grande.txt ({} bytes) em {:?}", grande.len(), t0.elapsed());
        // Seek recomeca a leitura de outro ponto.
        let mut at = 0u64;
        unsafe { g.Seek(1000, STREAM_SEEK_SET, Some(&mut at)) }.unwrap();
        assert_eq!(at, 1000);
        let mut buf = [0u8; 10];
        let mut n = 0u32;
        let hr = unsafe { g.Read(buf.as_mut_ptr().cast(), 10, Some(&mut n)) };
        assert!(hr.is_ok() && n == 10, "{hr:?} {n}");
        assert_eq!(&buf, &grande[1000..1010]);
        unsafe { g.Seek(0, STREAM_SEEK_CUR, Some(&mut at)) }.unwrap();
        assert_eq!(at, 1010);
        let mut st = STATSTG::default();
        unsafe { g.Stat(&mut st, STATFLAG(0)) }.unwrap();
        assert_eq!(st.cbSize, grande.len() as u64);
        assert_eq!(unsafe { st.pwcsName.to_string() }.unwrap(), "grande.txt");
        unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(st.pwcsName.0.cast())) };
        // Pasta e indice fora da lista nao tem conteudo.
        assert_eq!(get_stream(&out.data, 1).unwrap_err().code(), DV_E_LINDEX);
        assert_eq!(get_stream(&out.data, 6).unwrap_err().code(), DV_E_LINDEX);

        // Nada e avisado antes de soltar; depois, o que ficou de fora.
        assert!(rx.try_recv().is_err());
        out_dropped_report(&out, &rx);

        // Apagado no servidor depois da varredura: erro ao ler, com aviso.
        remote_sh(&host, &format!("rm '{r}/pasta/sub/y.txt'")).unwrap();
        let y = get_stream(&out.data, 4).expect("stream de y.txt");
        let err = read_all(&y, 4096).unwrap_err();
        assert_eq!(err, HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0));
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::ReadFailed { pane: 9, name, msg }) => {
                assert_eq!(name, "pasta/sub/y.txt");
                assert!(msg.contains("não existe mais"), "{msg}");
            }
            other => panic!("esperava a falha de leitura: {other:?}"),
        }

        drop((a, g, y, out));
        close(h);
        unsafe { OleUninitialize() };
    }

    /// Marca como solto (sem consumir o `DragOut`, que o teste ainda usa) e
    /// confere o aviso da varredura.
    fn out_dropped_report(out: &DragOut, rx: &std::sync::mpsc::Receiver<Event>) {
        out.shared.dropped.store(true, Ordering::SeqCst);
        out.shared.maybe_report();
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Scanned { pane: 9, report }) => {
                let skipped: Vec<&str> = report.skipped.iter().map(|(p, _)| p.as_str()).collect();
                assert_eq!(skipped, ["pasta/fifo", "pasta/link-pasta"], "{report:?}");
                assert_eq!(report.renamed.len(), 1, "{report:?}");
                assert!(report.fatal.is_none() && report.failed.is_empty(), "{report:?}");
            }
            other => panic!("esperava o aviso da varredura: {other:?}"),
        }
        // Uma vez so.
        out.shared.maybe_report();
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }
}
