//! Sessao SFTP em segundo plano usando `russh` + `russh-sftp`.
//!
//! Segue o mesmo padrao da sessao SSH ([`crate::ssh`]): a UI e sincrona, entao a
//! sessao roda numa thread dedicada com runtime tokio e conversa por dois canais:
//! - [`UiToSftp`]: a UI pede listagem de diretorio ou desconexao;
//! - [`SftpToUi`]: a sessao devolve status, listagens e erros.
//!
//! Downloads rodam em tarefas proprias (ver [`crate::download`]), fora do loop
//! de comandos: navegar, renomear etc. continuam respondendo durante a copia.
//! A deteccao do sistema do servidor e as leituras do visualizador tambem (ver
//! [`crate::osinfo`] e [`crate::viewer`]).
//!
//! O que pode travar o sftp-server (seguir links na listagem, o caminho
//! digitado na barra e as leituras do visualizador) vai por um canal SFTP
//! auxiliar e descartavel ([`AuxSftp`]), nunca pelo canal da navegacao.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::{FileAttributes, OpenFlags};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::download::{self, DownloadEvent, DownloadItem};
use crate::hostkey::HostKeyPrompt;
use crate::osinfo::{OsProbe, OsReport};
use crate::paste::{self, PasteEvent, PasteRequest};
use crate::vault::Host;
use crate::viewer::{self, ViewEvent};

/// Arquivo especial (nunca aberto nem baixado: a leitura pode travar).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Special {
    Fifo,
    Socket,
    CharDev,
    BlockDev,
}

/// Tipo efetivo de uma entrada: o do destino, quando e um link resolvido.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Special(Special),
    /// Servidor nao informou o tipo, ou link nao resolvido (ver `LinkInfo`).
    Unknown,
}

/// Situacao do destino de um link simbolico.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Destino lido (stat): `kind`, tamanho e modo sao os dele.
    Ok,
    /// Destino nao existe (ou links em ciclo: o OpenSSH devolve o mesmo erro).
    Broken,
    /// Sem permissao para chegar ao destino.
    Denied,
    /// Nao verificado (prazo, limite, nome nao UTF-8 ou erro inesperado).
    Unchecked,
}

/// Link simbolico: alvo (readlink, cortado) e situacao do destino.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    /// Alvo como gravado no link; vazio se nao foi lido.
    pub target: String,
    pub state: LinkState,
}

/// Uma entrada (arquivo ou pasta) de um diretorio remoto.
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    /// Tipo efetivo (link resolvido vale pelo destino).
    pub kind: EntryKind,
    /// `Some` quando a entrada e um link simbolico.
    pub link: Option<LinkInfo>,
    /// Tamanho (do destino, num link resolvido; 0 se desconhecido).
    pub size: u64,
    /// Bits de permissao (modo POSIX, ex.: 0o644); `None` se desconhecido
    /// (link nao resolvido: o 0777 do proprio link nao diz nada).
    pub mode: Option<u32>,
    /// Proprietario: nome do usuario quando possivel traduzir, senao o uid.
    pub owner: String,
    /// Grupo: nome quando possivel traduzir, senao o gid.
    pub group: String,
    /// Data da ultima alteracao (segundos desde a epoca Unix; 0 se ausente).
    pub mtime: u32,
}

impl RemoteEntry {
    /// Pasta, ou link para pasta (abre com Enter, vem junto das pastas). Um
    /// link sempre se exclui com remove, nunca rmdir (ver `FsNode::is_real_dir`).
    pub fn is_dir(&self) -> bool {
        self.kind == EntryKind::Dir
    }
}

/// Mensagens da sessao SFTP para a UI.
pub enum SftpToUi {
    /// Conexao estabelecida; traz o diretorio inicial (home) canonizado.
    Connected { home: String },
    /// Resultado da listagem de `path`.
    Listing {
        path: String,
        entries: Vec<RemoteEntry>,
    },
    /// Erro nao fatal (ex.: sem permissao para listar um diretorio).
    Error(String),
    /// Sessao encerrada.
    Closed,
    /// Chave do servidor nova ou diferente da guardada: a UI pergunta ao usuario
    /// e responde pelo `reply` do prompt (descartar = cancelar).
    HostKey(HostKeyPrompt),
    /// Andamento/fim de um download.
    Download(DownloadEvent),
    /// SO do servidor detectado em segundo plano (ver osinfo).
    Os(OsReport),
    /// Resposta a `UiToSftp::Goto` (caminho digitado na barra). Numa pasta, a
    /// `Listing` dela vem logo em seguida; o erro ja vem em portugues.
    Goto {
        seq: u64,
        result: Result<GotoKind, String>,
    },
    /// Andamento/resultado de uma leitura do visualizador (`UiToSftp::ReadFile`).
    View(ViewEvent),
    /// Andamento/fim de um colar (copiar ou mover no servidor).
    Paste(PasteEvent),
}

/// O que ha num caminho digitado na barra (seguindo links simbolicos).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GotoKind {
    Dir,
    File,
}

/// Mensagens da UI para a sessao SFTP.
pub enum UiToSftp {
    /// Pede a listagem do diretorio indicado.
    ListDir(String),
    /// Caminho digitado na barra: verifica (stat, seguindo links) e, se for
    /// uma pasta, lista. Responde `SftpToUi::Goto` com o mesmo `seq`.
    Goto { seq: u64, path: String },
    /// Envia um arquivo local para o diretorio remoto indicado.
    Upload { local: PathBuf, remote_dir: String },
    /// Renomeia/move um arquivo ou pasta e re-lista `refresh_dir`.
    Rename {
        from: String,
        to: String,
        refresh_dir: String,
    },
    /// Altera as permissoes (chmod) de um arquivo/pasta e re-lista `refresh_dir`.
    Chmod {
        path: String,
        mode: u32,
        refresh_dir: String,
    },
    /// Altera proprietario/grupo (chown) e re-lista `refresh_dir`. `owner` e
    /// `group` podem ser nomes (ex.: "root") ou ids numericos; nomes sao
    /// resolvidos para id pela base de usuarios/grupos do servidor.
    Chown {
        path: String,
        owner: String,
        group: String,
        refresh_dir: String,
    },
    /// Remove um arquivo ou pasta e re-lista `refresh_dir`.
    Remove {
        path: String,
        is_dir: bool,
        refresh_dir: String,
    },
    /// Baixa `items` para a pasta local `dest`.
    Download {
        id: u64,
        dest: PathBuf,
        items: Vec<DownloadItem>,
        cancel: watch::Receiver<bool>,
    },
    /// Le o arquivo para o visualizador somente leitura (so para a memoria;
    /// responde `SftpToUi::View` com o mesmo `id`).
    ReadFile {
        id: u64,
        path: String,
        cancel: watch::Receiver<bool>,
    },
    /// Cola (copia ou move) itens no servidor; responde `SftpToUi::Paste`
    /// com o mesmo `id`.
    Paste {
        id: u64,
        req: PasteRequest,
        cancel: watch::Receiver<bool>,
    },
    /// Encerra a sessao.
    Disconnect,
}

/// Lado da UI: enviar pedidos e receber eventos da sessao SFTP.
pub struct SftpHandle {
    to_sftp: UnboundedSender<UiToSftp>,
    pub from_sftp: std::sync::mpsc::Receiver<SftpToUi>,
}

impl SftpHandle {
    /// Handle ligado a canais de teste: devolve tambem o lado "sessao" (o que
    /// a UI mandou e por onde injetar eventos).
    #[cfg(test)]
    pub(crate) fn test_pair() -> (
        Self,
        UnboundedReceiver<UiToSftp>,
        std::sync::mpsc::Sender<SftpToUi>,
    ) {
        let (to_tx, to_rx) = tokio::sync::mpsc::unbounded_channel();
        let (from_tx, from_rx) = std::sync::mpsc::channel();
        let handle = SftpHandle {
            to_sftp: to_tx,
            from_sftp: from_rx,
        };
        (handle, to_rx, from_tx)
    }

    pub fn list_dir(&self, path: impl Into<String>) {
        let _ = self.to_sftp.send(UiToSftp::ListDir(path.into()));
    }

    /// Abre o caminho digitado na barra (ver `UiToSftp::Goto`).
    pub fn goto(&self, seq: u64, path: impl Into<String>) {
        let _ = self.to_sftp.send(UiToSftp::Goto {
            seq,
            path: path.into(),
        });
    }

    /// Envia um arquivo local para o diretorio remoto indicado.
    pub fn upload(&self, local: PathBuf, remote_dir: impl Into<String>) {
        let _ = self.to_sftp.send(UiToSftp::Upload {
            local,
            remote_dir: remote_dir.into(),
        });
    }

    /// Renomeia/move um item; depois re-lista `refresh_dir`.
    pub fn rename(
        &self,
        from: impl Into<String>,
        to: impl Into<String>,
        refresh_dir: impl Into<String>,
    ) {
        let _ = self.to_sftp.send(UiToSftp::Rename {
            from: from.into(),
            to: to.into(),
            refresh_dir: refresh_dir.into(),
        });
    }

    /// Altera as permissoes (chmod) de um item; depois re-lista `refresh_dir`.
    pub fn chmod(&self, path: impl Into<String>, mode: u32, refresh_dir: impl Into<String>) {
        let _ = self.to_sftp.send(UiToSftp::Chmod {
            path: path.into(),
            mode,
            refresh_dir: refresh_dir.into(),
        });
    }

    /// Altera proprietario/grupo (chown) de um item; depois re-lista `refresh_dir`.
    pub fn chown(
        &self,
        path: impl Into<String>,
        owner: impl Into<String>,
        group: impl Into<String>,
        refresh_dir: impl Into<String>,
    ) {
        let _ = self.to_sftp.send(UiToSftp::Chown {
            path: path.into(),
            owner: owner.into(),
            group: group.into(),
            refresh_dir: refresh_dir.into(),
        });
    }

    /// Remove um arquivo ou pasta; depois re-lista `refresh_dir`.
    pub fn remove(&self, path: impl Into<String>, is_dir: bool, refresh_dir: impl Into<String>) {
        let _ = self.to_sftp.send(UiToSftp::Remove {
            path: path.into(),
            is_dir,
            refresh_dir: refresh_dir.into(),
        });
    }

    /// Pede o download; `None` se a sessao ja terminou (nenhuma resposta viria).
    /// Soltar o `Cancel` devolvido tambem cancela.
    pub fn download(
        &self,
        id: u64,
        dest: PathBuf,
        items: Vec<DownloadItem>,
    ) -> Option<download::Cancel> {
        let (cancel, rx) = download::cancel_pair();
        self.to_sftp
            .send(UiToSftp::Download {
                id,
                dest,
                items,
                cancel: rx,
            })
            .ok()
            .map(|_| cancel)
    }

    /// Pede a leitura de um arquivo para o visualizador; `None` se a sessao ja
    /// terminou. Soltar o `Cancel` devolvido cancela (fechar o visualizador,
    /// o painel ou abrir outro arquivo).
    pub fn read_file(&self, id: u64, path: impl Into<String>) -> Option<download::Cancel> {
        let (cancel, rx) = download::cancel_pair();
        self.to_sftp
            .send(UiToSftp::ReadFile {
                id,
                path: path.into(),
                cancel: rx,
            })
            .ok()
            .map(|_| cancel)
    }

    /// Pede o colar (copiar/mover no servidor); `None` se a sessao ja
    /// terminou. Soltar o `Cancel` devolvido tambem cancela.
    pub fn paste(&self, id: u64, req: PasteRequest) -> Option<download::Cancel> {
        let (cancel, rx) = download::cancel_pair();
        self.to_sftp
            .send(UiToSftp::Paste { id, req, cancel: rx })
            .ok()
            .map(|_| cancel)
    }

    pub fn disconnect(&self) {
        let _ = self.to_sftp.send(UiToSftp::Disconnect);
    }
}

/// Inicia uma sessao SFTP em segundo plano. `repaint` acorda o loop do egui
/// sempre que houver novidade. `detect_os` liga a deteccao do SO do servidor
/// (resultado em `SftpToUi::Os`).
pub fn connect<F>(host: Host, detect_os: bool, repaint: F) -> SftpHandle
where
    F: Fn() + Send + 'static,
{
    let (to_sftp_tx, to_sftp_rx) = tokio::sync::mpsc::unbounded_channel::<UiToSftp>();
    let (from_sftp_tx, from_sftp_rx) = std::sync::mpsc::channel::<SftpToUi>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = from_sftp_tx.send(SftpToUi::Error(format!("runtime: {e}")));
                repaint();
                return;
            }
        };

        run_to_end(
            &rt,
            run_session(host, detect_os, to_sftp_rx, &from_sftp_tx, &repaint),
            &from_sftp_tx,
            &repaint,
        );
    });

    SftpHandle {
        to_sftp: to_sftp_tx,
        from_sftp: from_sftp_rx,
    }
}

/// Erro mostrado quando a sessao SFTP entra em panico.
const SESSION_PANIC: &str = "a sessão SFTP parou por um erro interno";

/// Roda a sessao ate o fim na thread dela e sempre avisa a UI: `Error` (se
/// falhou ou entrou em panico) e depois `Closed`. Sem isso, um panico no
/// meio (ex.: o do russh-sftp quando o servidor manda mais dados do que o
/// pedido, ja na leitura do /etc/passwd ao conectar) mataria a thread calada
/// e o painel ficaria em "Conectando..." para sempre.
fn run_to_end<F: Fn()>(
    rt: &tokio::runtime::Runtime,
    session: impl Future<Output = anyhow::Result<()>>,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    repaint: &F,
) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rt.block_on(session)))
        .unwrap_or_else(|_| Err(anyhow::anyhow!(SESSION_PANIC)));
    if let Err(e) = r {
        let _ = from_sftp.send(SftpToUi::Error(format!("{e}")));
        repaint();
    }
    let _ = from_sftp.send(SftpToUi::Closed);
    repaint();
}

async fn run_session<F>(
    host: Host,
    detect_os: bool,
    mut to_sftp_rx: UnboundedReceiver<UiToSftp>,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    repaint: &F,
) -> anyhow::Result<()>
where
    F: Fn() + Send + 'static,
{
    // Conexao e autenticacao compartilhadas com a sessao SSH (mesma politica
    // de timeouts e de chave do servidor, num unico ponto de manutencao).
    let (session, banner) = crate::ssh::connect_and_auth(&host, |p| {
        let _ = from_sftp.send(SftpToUi::HostKey(p));
        repaint();
    })
    .await?;
    // Compartilhada com a sonda do SO (canal extra na mesma conexao).
    let session = Arc::new(session);

    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    // Compartilhada com as tarefas de download (pedidos concorrentes no mesmo
    // canal; `&sftp` continua valendo nos comandos abaixo).
    let sftp = Arc::new(
        start_session(channel.into_stream())
            .await
            .map_err(|e| anyhow::anyhow!("não foi possível iniciar o SFTP: {e}"))?,
    );
    // Canal extra (aberto no primeiro uso) para o que pode travar o
    // sftp-server: stat de links, caminho digitado e visualizador.
    let aux = AuxSftp::new(aux_opener(&session));

    // Base de usuarios/grupos do servidor (para traduzir uid/gid <-> nome).
    // Lida uma vez de /etc/passwd e /etc/group; se indisponivel, usa numeros.
    let mut uid_to_name: HashMap<u32, String> = HashMap::new();
    let mut name_to_uid: HashMap<String, u32> = HashMap::new();
    let mut gid_to_name: HashMap<u32, String> = HashMap::new();
    let mut name_to_gid: HashMap<String, u32> = HashMap::new();
    if let Ok(data) = sftp.read("/etc/passwd").await {
        parse_id_db(&data, &mut uid_to_name, &mut name_to_uid);
    }
    if let Ok(data) = sftp.read("/etc/group").await {
        parse_id_db(&data, &mut gid_to_name, &mut name_to_gid);
    }

    // Diretorio inicial (home) canonizado a partir de ".".
    let home = sftp.canonicalize(".").await.unwrap_or_else(|_| "/".into());
    let _ = from_sftp.send(SftpToUi::Connected { home: home.clone() });
    repaint();

    // SO do servidor em segundo plano (ver osinfo): canal proprio, tarefa
    // propria, que segue rodando durante os comandos e downloads (so o
    // resultado espera no OsProbe). No fim da sessao (qualquer caminho), o
    // drop do OsProbe entrega o que faltar e aborta a sonda.
    let mut os_probe = OsProbe::new(&host, |r| {
        let _ = from_sftp.send(SftpToUi::Os(r));
    });
    if detect_os {
        os_probe.spawn(&session, banner);
    }

    // Downloads: cada lote roda numa tarefa propria (nunca dentro deste loop,
    // que continua atendendo a navegacao) e reporta por `dl_rx`, repassado a
    // UI aqui (mesmo padrao dos envios em `ssh::run_session`).
    let mut downloads: JoinSet<()> = JoinSet::new();
    let (dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel::<DownloadEvent>();
    // Leituras do visualizador: mesmo esquema (tarefa propria, eventos por
    // `view_rx`), para a navegacao continuar respondendo.
    let mut views: JoinSet<()> = JoinSet::new();
    let (view_tx, mut view_rx) = tokio::sync::mpsc::unbounded_channel::<ViewEvent>();
    // Colar (copiar/mover no servidor): mesmo esquema. Como root, a copia
    // mantem dono e grupo do original; a ordem dos argumentos do SYMLINK e
    // descoberta uma vez por sessao.
    let am_root = host.username == "root";
    let link_order = Arc::new(paste::LinkOrder::default());
    let mut pastes: JoinSet<()> = JoinSet::new();
    let (pa_tx, mut pa_rx) = tokio::sync::mpsc::unbounded_channel::<PasteEvent>();

    loop {
        let cmd = tokio::select! {
            Some(ev) = dl_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::Download(ev));
                repaint();
                continue;
            }
            Some(ev) = view_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::View(ev));
                repaint();
                continue;
            }
            Some(ev) = pa_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::Paste(ev));
                repaint();
                continue;
            }
            // Recolhe tarefas terminadas (o JoinSet as guarda ate serem lidas).
            Some(_) = downloads.join_next(), if !downloads.is_empty() => continue,
            Some(_) = views.join_next(), if !views.is_empty() => continue,
            Some(_) = pastes.join_next(), if !pastes.is_empty() => continue,
            () = os_probe.wait(), if os_probe.running() => {
                repaint();
                continue;
            }
            cmd = to_sftp_rx.recv() => match cmd {
                Some(cmd) => cmd,
                None => break,
            },
        };
        let cmd = match cmd {
            UiToSftp::Download {
                id,
                dest,
                items,
                cancel,
            } => {
                let (sftp, tx) = (Arc::clone(&sftp), dl_tx.clone());
                downloads.spawn(download::run(sftp, id, dest, items, cancel, tx));
                continue;
            }
            UiToSftp::ReadFile { id, path, cancel } => {
                let (aux, sftp, tx) = (aux.clone(), Arc::clone(&sftp), view_tx.clone());
                views.spawn(view_task(aux, sftp, id, path, cancel, tx));
                continue;
            }
            UiToSftp::Paste { id, req, cancel } => {
                let (sftp, links, tx) = (Arc::clone(&sftp), Arc::clone(&link_order), pa_tx.clone());
                pastes.spawn(paste::run(sftp, links, am_root, id, req, cancel, tx));
                continue;
            }
            UiToSftp::Disconnect => break,
            cmd => cmd,
        };
        // Os demais comandos esperam o servidor (um envio grande leva
        // minutos); enquanto isso, o andamento dos downloads e das leituras
        // do visualizador segue para a UI.
        let work = async {
            match cmd {
                UiToSftp::ListDir(path) => {
                    send_listing(&sftp, &aux, from_sftp, &path, &uid_to_name, &gid_to_name).await;
                    repaint();
                }
                UiToSftp::Goto { seq, path } => {
                    match goto_path(&sftp, &aux, &path, &uid_to_name, &gid_to_name).await {
                        Ok((kind, entries)) => {
                            let _ = from_sftp.send(SftpToUi::Goto {
                                seq,
                                result: Ok(kind),
                            });
                            if let Some(entries) = entries {
                                let _ = from_sftp.send(SftpToUi::Listing { path, entries });
                            }
                        }
                        Err(msg) => {
                            let _ = from_sftp.send(SftpToUi::Goto {
                                seq,
                                result: Err(msg),
                            });
                        }
                    }
                    repaint();
                }
                UiToSftp::Upload { local, remote_dir } => {
                    match upload_file(&sftp, &local, &remote_dir).await {
                        Ok(()) => {
                            // Recarrega o diretorio para o arquivo recem-enviado aparecer.
                            send_listing(&sftp, &aux, from_sftp, &remote_dir, &uid_to_name, &gid_to_name)
                                .await;
                        }
                        Err(e) => {
                            let nome = local
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            let _ = from_sftp
                                .send(SftpToUi::Error(format!("falha ao enviar {nome}: {e}")));
                        }
                    }
                    repaint();
                }
                UiToSftp::Rename {
                    from,
                    to,
                    refresh_dir,
                } => {
                    match sftp.rename(from, to).await {
                        Ok(()) => {
                            send_listing(&sftp, &aux, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
                                .await
                        }
                        Err(e) => {
                            let _ =
                                from_sftp.send(SftpToUi::Error(format!("falha ao renomear: {e}")));
                        }
                    }
                    repaint();
                }
                UiToSftp::Chmod {
                    path,
                    mode,
                    refresh_dir,
                } => {
                    let attrs = russh_sftp::protocol::FileAttributes {
                        size: None,
                        uid: None,
                        user: None,
                        gid: None,
                        group: None,
                        permissions: Some(mode),
                        atime: None,
                        mtime: None,
                    };
                    match sftp.set_metadata(path, attrs).await {
                        Ok(()) => {
                            send_listing(&sftp, &aux, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
                                .await
                        }
                        Err(e) => {
                            let _ = from_sftp
                                .send(SftpToUi::Error(format!("falha ao alterar permissões: {e}")));
                        }
                    }
                    repaint();
                }
                UiToSftp::Chown {
                    path,
                    owner,
                    group,
                    refresh_dir,
                } => {
                    // Resolve nome -> id (ou aceita id numerico direto).
                    let uid = resolve_id(&owner, &name_to_uid);
                    let gid = resolve_id(&group, &name_to_gid);
                    match (uid, gid) {
                        (Some(uid), Some(gid)) => {
                            let attrs = russh_sftp::protocol::FileAttributes {
                                size: None,
                                uid: Some(uid),
                                user: None,
                                gid: Some(gid),
                                group: None,
                                permissions: None,
                                atime: None,
                                mtime: None,
                            };
                            match sftp.set_metadata(path, attrs).await {
                                Ok(()) => {
                                    send_listing(
                                        &sftp,
                                        &aux,
                                        from_sftp,
                                        &refresh_dir,
                                        &uid_to_name,
                                        &gid_to_name,
                                    )
                                    .await
                                }
                                Err(e) => {
                                    let _ = from_sftp.send(SftpToUi::Error(format!(
                                        "falha ao alterar proprietário/grupo: {e}"
                                    )));
                                }
                            }
                        }
                        _ => {
                            let mut faltando = Vec::new();
                            if uid.is_none() {
                                faltando.push(format!("usuário \"{owner}\""));
                            }
                            if gid.is_none() {
                                faltando.push(format!("grupo \"{group}\""));
                            }
                            let _ = from_sftp.send(SftpToUi::Error(format!(
                                "não foi possível resolver {}",
                                faltando.join(" e ")
                            )));
                        }
                    }
                    repaint();
                }
                UiToSftp::Remove {
                    path,
                    is_dir,
                    refresh_dir,
                } => {
                    let res = if is_dir {
                        sftp.remove_dir(path).await
                    } else {
                        sftp.remove_file(path).await
                    };
                    match res {
                        Ok(()) => {
                            send_listing(&sftp, &aux, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
                                .await
                        }
                        Err(e) => {
                            let _ =
                                from_sftp.send(SftpToUi::Error(format!("falha ao excluir: {e}")));
                        }
                    }
                    repaint();
                }
                UiToSftp::Download { .. }
                | UiToSftp::ReadFile { .. }
                | UiToSftp::Paste { .. }
                | UiToSftp::Disconnect => {}
            }
        };
        forward_downloads(work, &mut dl_rx, &mut view_rx, &mut pa_rx, from_sftp, repaint).await;
    }

    // Entrega o que ja estava na fila (ex.: um download que terminou junto) e
    // aborta os que estao em andamento: as tarefas sao soltas e os guards
    // apagam os temporarios (a UI avisa o usuario).
    while let Ok(ev) = dl_rx.try_recv() {
        let _ = from_sftp.send(SftpToUi::Download(ev));
    }
    while let Ok(ev) = view_rx.try_recv() {
        let _ = from_sftp.send(SftpToUi::View(ev));
    }
    while let Ok(ev) = pa_rx.try_recv() {
        let _ = from_sftp.send(SftpToUi::Paste(ev));
    }
    repaint();
    downloads.abort_all();
    views.abort_all();
    pastes.abort_all();

    Ok(())
}

/// Espera `work` repassando a UI, enquanto isso, os eventos dos downloads,
/// das leituras do visualizador e dos colar: sem isso os rodapes e a faixa
/// "Abrindo" congelariam durante um envio grande.
async fn forward_downloads<F: Fn()>(
    work: impl std::future::Future<Output = ()>,
    dl_rx: &mut UnboundedReceiver<DownloadEvent>,
    view_rx: &mut UnboundedReceiver<ViewEvent>,
    pa_rx: &mut UnboundedReceiver<PasteEvent>,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    repaint: &F,
) {
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            () = &mut work => return,
            Some(ev) = dl_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::Download(ev));
                repaint();
            }
            Some(ev) = view_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::View(ev));
                repaint();
            }
            Some(ev) = pa_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::Paste(ev));
                repaint();
            }
        }
    }
}

/// Le um arquivo local e o grava no diretorio remoto (sobrescrevendo).
async fn upload_file(
    sftp: &SftpSession,
    local: &std::path::Path,
    remote_dir: &str,
) -> anyhow::Result<()> {
    let name = local
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("caminho de origem inválido"))?
        .to_string_lossy()
        .into_owned();
    let remote_path = if remote_dir.ends_with('/') {
        format!("{remote_dir}{name}")
    } else {
        format!("{remote_dir}/{name}")
    };

    // Copia em blocos: nao carrega o arquivo inteiro na memoria (um arquivo
    // de varios GB arrastado para o painel nao pode alocar tudo de uma vez).
    let mut src = tokio::fs::File::open(local).await?;
    let mut file = sftp
        .open_with_flags(
            remote_path,
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::TRUNCATE,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).await?;
    }
    file.shutdown().await?;
    Ok(())
}

/// Lista um diretorio remoto e envia o resultado (ou erro) para a UI.
/// `uid_to_name`/`gid_to_name` traduzem id numerico em nome de usuario/grupo.
async fn send_listing(
    sftp: &Arc<SftpSession>,
    aux: &AuxSftp,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    path: &str,
    uid_to_name: &HashMap<u32, String>,
    gid_to_name: &HashMap<u32, String>,
) {
    match list_entries(sftp, aux, path, uid_to_name, gid_to_name).await {
        Ok(entries) => {
            let _ = from_sftp.send(SftpToUi::Listing {
                path: path.to_string(),
                entries,
            });
        }
        Err(e) => {
            let _ = from_sftp.send(SftpToUi::Error(path_error(path, &e)));
        }
    }
}

/// Prazo para o servidor dizer o que ha num caminho digitado na barra.
const GOTO_STAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Caminho mais longo mostrado numa mensagem (texto vindo do usuario ou do
/// servidor, neutralizado por `download::safe_text`).
const PATH_SHOWN_MAX: usize = 300;

/// Mensagem do prazo estourado ao abrir um caminho.
const GOTO_TIMEOUT_MSG: &str = "O servidor não respondeu a tempo.";

/// Caminho digitado na barra: stat (segue links); pasta tambem e listada
/// (sem permissao de listar vira erro, e a barra continua em edicao).
/// Servidor que nao manda o tipo: tenta listar (se abrir, e pasta).
async fn goto_path(
    sftp: &Arc<SftpSession>,
    aux: &AuxSftp,
    path: &str,
    uids: &HashMap<u32, String>,
    gids: &HashMap<u32, String>,
) -> Result<(GotoKind, Option<Vec<RemoteEntry>>), String> {
    use russh_sftp::client::error::Error;
    // Prazo do proprio crate (10 s por pedido) tem a mesma mensagem do nosso.
    let err = |e: Error| match e {
        Error::Timeout => GOTO_TIMEOUT_MSG.to_string(),
        e => path_error(path, &e),
    };
    // O stat segue o caminho digitado (links inclusive) e pode travar o
    // sftp-server: vai pelo canal auxiliar (o principal, se nao houver).
    let ch = aux.get().await;
    let s = ch.clone().unwrap_or_else(|| Arc::clone(sftp));
    let stat = match tokio::time::timeout(GOTO_STAT_TIMEOUT, s.metadata(path.to_string())).await {
        Err(_) => Err(Error::Timeout),
        Ok(r) => r,
    };
    if let (Err(Error::Timeout), Some(c)) = (&stat, &ch) {
        // O pedido pode ter ficado preso no auxiliar: o proximo uso abre outro.
        aux.discard(c, false).await;
    }
    let meta = stat.map_err(err)?;
    let kind = raw_kind(meta.permissions);
    if kind == RawKind::File {
        return Ok((GotoKind::File, None));
    }
    if kind != RawKind::Dir && meta.permissions.is_some() {
        return Err(format!(
            "Não é uma pasta nem um arquivo comum: {}",
            download::safe_text(path, PATH_SHOWN_MAX)
        ));
    }
    let entries = list_entries(sftp, aux, path, uids, gids).await.map_err(err)?;
    Ok((GotoKind::Dir, Some(entries)))
}

/// Mensagem (em portugues) de uma falha ao abrir ou listar `path`.
fn path_error(path: &str, e: &russh_sftp::client::error::Error) -> String {
    use russh_sftp::client::error::Error;
    use russh_sftp::protocol::StatusCode;
    let p = download::safe_text(path, PATH_SHOWN_MAX);
    match e {
        Error::Status(s) if s.status_code == StatusCode::NoSuchFile => {
            format!("Caminho não encontrado: {p}")
        }
        Error::Status(s) if s.status_code == StatusCode::PermissionDenied => {
            format!("Sem permissão para abrir {p}")
        }
        // A mensagem do servidor tambem e texto remoto: neutralizada.
        other => format!(
            "Não foi possível abrir {p}: {}",
            download::safe_text(&other.to_string(), PATH_SHOWN_MAX)
        ),
    }
}

// --- Tipos e links simbolicos na listagem ------------------------------------

/// Links (e entradas sem tipo) verificados ao mesmo tempo.
const LINK_CONCURRENCY: usize = 32;
/// Maximo de verificacoes por listagem; o resto fica "nao verificado".
const LINK_MAX: usize = 2000;
/// Prazo das verificacoes (inclusive abrir o canal auxiliar na primeira
/// vez), contado do fim do read_dir: uma pasta com muitos links numa rede
/// lenta nao segura a listagem.
const LINK_BUDGET: std::time::Duration = std::time::Duration::from_millis(1500);
/// Sem nenhuma resposta ha este tempo quando o prazo acaba, com verificacoes
/// em voo: o canal esta preso (o sftp-server atende um pedido por vez, e um
/// stat preso segura todos). Uma rede lenta ainda responde nesse intervalo.
const STUCK_GAP: std::time::Duration = std::time::Duration::from_millis(500);
/// Canais auxiliares presos (e descartados) ate a verificacao automatica de
/// links parar nesta sessao: cada um pode deixar um sftp-server preso no
/// servidor. Os links ficam "nao verificados" e o Enter ainda os abre.
const AUX_MAX_STALLS: u32 = 3;
/// O servidor recusou o canal auxiliar (ex.: MaxSessions): nova tentativa so
/// depois disto; ate la usa-se o principal.
const AUX_RETRY: std::time::Duration = std::time::Duration::from_secs(30);
/// Alvo de link mais longo guardado (texto vindo do servidor).
const LINK_TARGET_MAX: usize = 1024;

/// Mascara do tipo no modo POSIX (S_IFMT).
const S_IFMT: u32 = 0o170000;

/// Tipo cru de uma entrada, pelos bits de tipo do modo (o visualizador
/// tambem usa).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawKind {
    Dir,
    File,
    Link,
    Fifo,
    CharDev,
    BlockDev,
    Socket,
    Unknown,
}

/// Tipo pelos bits de tipo do modo (mascara 0o170000). Os `is_*` do
/// russh-sftp testam bits com `contains()` e dao falso positivo (um link sai
/// como arquivo comum e como dispositivo de caractere): nunca usa-los.
pub(crate) fn raw_kind(perms: Option<u32>) -> RawKind {
    match perms.map(|p| p & S_IFMT) {
        Some(0o040000) => RawKind::Dir,
        Some(0o100000) => RawKind::File,
        Some(0o120000) => RawKind::Link,
        Some(0o010000) => RawKind::Fifo,
        Some(0o020000) => RawKind::CharDev,
        Some(0o060000) => RawKind::BlockDev,
        Some(0o140000) => RawKind::Socket,
        _ => RawKind::Unknown,
    }
}

/// Tipo exibido para um tipo cru que nao e link.
fn entry_kind(raw: RawKind) -> EntryKind {
    match raw {
        RawKind::Dir => EntryKind::Dir,
        RawKind::File => EntryKind::File,
        RawKind::Fifo => EntryKind::Special(Special::Fifo),
        RawKind::CharDev => EntryKind::Special(Special::CharDev),
        RawKind::BlockDev => EntryKind::Special(Special::BlockDev),
        RawKind::Socket => EntryKind::Special(Special::Socket),
        RawKind::Link | RawKind::Unknown => EntryKind::Unknown,
    }
}

/// Situacao do destino a partir do erro do stat. O OpenSSH devolve "no such
/// file" tambem para links em ciclo (ELOOP).
fn link_state_from_err(e: &russh_sftp::client::error::Error) -> LinkState {
    use russh_sftp::client::error::Error;
    use russh_sftp::protocol::StatusCode;
    match e {
        Error::Status(s) if s.status_code == StatusCode::NoSuchFile => LinkState::Broken,
        Error::Status(s) if s.status_code == StatusCode::PermissionDenied => LinkState::Denied,
        _ => LinkState::Unchecked,
    }
}

/// Proprietario/grupo: nome enviado pelo servidor; senao traduz o id pela
/// base /etc/passwd|group; senao o numero.
fn owner_group(
    attrs: &FileAttributes,
    uids: &HashMap<u32, String>,
    gids: &HashMap<u32, String>,
) -> (String, String) {
    let owner = attrs
        .user
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| attrs.uid.and_then(|u| uids.get(&u).cloned()))
        .or_else(|| attrs.uid.map(|u| u.to_string()))
        .unwrap_or_default();
    let group = attrs
        .group
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| attrs.gid.and_then(|g| gids.get(&g).cloned()))
        .or_else(|| attrs.gid.map(|g| g.to_string()))
        .unwrap_or_default();
    (owner, group)
}

/// Destino de um link: stat (ou a situacao do erro) e alvo (readlink).
struct LinkRes {
    stat: Result<FileAttributes, LinkState>,
    target: Option<String>,
}

/// Verificacao de uma entrada da listagem.
struct Resolved {
    /// lstat refeito (entrada que veio sem tipo); `None` = vale o do readdir.
    lstat: Option<FileAttributes>,
    /// Destino, quando a entrada e um link.
    link: Option<LinkRes>,
}

/// Monta a entrada a partir do lstat (readdir) e, num link, do destino.
fn build_entry(
    name: String,
    path: String,
    lstat: &FileAttributes,
    res: Option<LinkRes>,
    uids: &HashMap<u32, String>,
    gids: &HashMap<u32, String>,
) -> RemoteEntry {
    let (owner, group) = owner_group(lstat, uids, gids);
    let raw = raw_kind(lstat.permissions);
    if raw != RawKind::Link {
        return RemoteEntry {
            name,
            path,
            kind: entry_kind(raw),
            link: None,
            size: lstat.size.unwrap_or(0),
            mode: lstat.permissions.map(|p| p & 0o7777),
            owner,
            group,
            mtime: lstat.mtime.unwrap_or(0),
        };
    }
    // Nome nao UTF-8 (virou U+FFFD): o caminho montado nao existe no
    // servidor, entao um stat diria "quebrado" sem ser.
    let res = if name.contains('\u{FFFD}') { None } else { res };
    let target = res
        .as_ref()
        .and_then(|r| r.target.as_deref())
        .map(|t| download::clip(t, LINK_TARGET_MAX))
        .unwrap_or_default();
    let unresolved = |state| RemoteEntry {
        name: name.clone(),
        path: path.clone(),
        kind: EntryKind::Unknown,
        link: Some(LinkInfo {
            target: target.clone(),
            state,
        }),
        size: 0,
        mode: None,
        owner: owner.clone(),
        group: group.clone(),
        mtime: lstat.mtime.unwrap_or(0),
    };
    match res.map(|r| r.stat) {
        Some(Ok(st)) => match raw_kind(st.permissions) {
            // stat nao segue ate o fim (nao deveria) ou nao traz o tipo.
            RawKind::Link | RawKind::Unknown => unresolved(LinkState::Unchecked),
            k => {
                let (owner, group) = owner_group(&st, uids, gids);
                RemoteEntry {
                    kind: entry_kind(k),
                    link: Some(LinkInfo {
                        target: target.clone(),
                        state: LinkState::Ok,
                    }),
                    size: st.size.unwrap_or(0),
                    mode: st.permissions.map(|p| p & 0o7777),
                    owner,
                    group,
                    mtime: st.mtime.unwrap_or(0),
                    ..unresolved(LinkState::Ok)
                }
            }
        },
        Some(Err(state)) => unresolved(state),
        None => unresolved(LinkState::Unchecked),
    }
}

/// Pastas (inclusive links para pasta) primeiro; cada grupo por nome sem
/// diferenciar maiusculas nem acentos (a mesma chave da busca por letras:
/// "Ábaco" fica junto de "abacate"), com o nome exato de desempate (ordem
/// estavel).
fn sort_entries(entries: &mut [RemoteEntry]) {
    entries.sort_by_cached_key(|e| (!e.is_dir(), download::fold_name(&e.name), e.name.clone()));
}

/// Roda `f` sobre `jobs` com no maximo `conc` em voo, so os `limit`
/// primeiros e ate `deadline`: o que nao terminou fica `None` (o drop do
/// JoinSet aborta o que estava em voo). O segundo valor diz se o canal
/// parece preso: o prazo acabou com pedidos em voo e sem nenhuma resposta
/// nos ultimos `STUCK_GAP`.
async fn bounded<T, R, F, Fut>(
    jobs: Vec<T>,
    conc: usize,
    limit: usize,
    deadline: tokio::time::Instant,
    f: F,
) -> (Vec<Option<R>>, bool)
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    let mut out: Vec<Option<R>> = (0..jobs.len()).map(|_| None).collect();
    let mut js = JoinSet::new();
    let mut it = jobs.into_iter().enumerate().take(limit);
    // Ultima resposta (ou o inicio).
    let mut last = tokio::time::Instant::now();
    loop {
        while js.len() < conc.max(1) {
            let Some((i, job)) = it.next() else {
                break;
            };
            let fut = f(job);
            js.spawn(async move { (i, fut.await) });
        }
        match tokio::time::timeout_at(deadline, js.join_next()).await {
            Ok(Some(Ok((i, r)))) => {
                out[i] = Some(r);
                last = tokio::time::Instant::now();
            }
            Ok(Some(Err(_))) => last = tokio::time::Instant::now(),
            Ok(None) => break,
            Err(_) => {
                let stuck = !js.is_empty() && last.elapsed() >= STUCK_GAP;
                return (out, stuck);
            }
        }
    }
    (out, false)
}

/// Verifica uma entrada: sem tipo, refaz o lstat; se for link, le o destino
/// (stat, que segue links) e o alvo (readlink) juntos.
async fn resolve(sftp: Arc<SftpSession>, path: String, is_link: bool) -> Resolved {
    let mut lstat = None;
    if !is_link {
        match sftp.symlink_metadata(path.clone()).await {
            Ok(a) => {
                let link = raw_kind(a.permissions) == RawKind::Link;
                lstat = Some(a);
                if !link {
                    return Resolved { lstat, link: None };
                }
            }
            Err(_) => return Resolved { lstat, link: None },
        }
    }
    let (stat, target) = tokio::join!(sftp.metadata(path.clone()), sftp.read_link(path));
    Resolved {
        lstat,
        link: Some(LinkRes {
            stat: stat.map_err(|e| link_state_from_err(&e)),
            target: target.ok(),
        }),
    }
}

/// Lista `path` com os links resolvidos (ver `LinkState`) e ordenada.
/// Custa um pedido a mais por link (em paralelo, com limite e prazo, no
/// canal auxiliar); pasta sem links nao faz nenhum pedido extra.
async fn list_entries(
    sftp: &Arc<SftpSession>,
    aux: &AuxSftp,
    path: &str,
    uids: &HashMap<u32, String>,
    gids: &HashMap<u32, String>,
) -> Result<Vec<RemoteEntry>, russh_sftp::client::error::Error> {
    let dir = sftp.read_dir(path.to_string()).await?;
    let mut raw: Vec<(String, String, FileAttributes)> = dir
        .map(|e| (e.file_name(), e.path(), e.metadata()))
        .collect();
    // Ordem de nome antes de verificar (a mesma da lista): se o prazo
    // cortar, o topo visivel da lista ja foi resolvido.
    raw.sort_by_cached_key(|(n, ..)| (download::fold_name(n), n.clone()));
    let (idx, jobs): (Vec<usize>, Vec<(String, bool)>) = raw
        .iter()
        .enumerate()
        .filter(|(_, (n, ..))| !n.contains('\u{FFFD}'))
        .filter_map(|(i, (_, p, a))| match raw_kind(a.permissions) {
            _ if a.permissions.is_none() => Some((i, (p.clone(), false))),
            RawKind::Link => Some((i, (p.clone(), true))),
            _ => None,
        })
        .unzip();
    let done = if jobs.is_empty() {
        Vec::new()
    } else {
        resolve_links(sftp, aux, jobs).await
    };
    let mut res: Vec<Option<Resolved>> = (0..raw.len()).map(|_| None).collect();
    for (i, r) in idx.into_iter().zip(done) {
        res[i] = r;
    }
    let mut entries: Vec<RemoteEntry> = raw
        .into_iter()
        .zip(res)
        .map(|((name, path, attrs), r)| {
            let (lstat, link) = match r {
                Some(Resolved { lstat, link }) => (lstat.unwrap_or(attrs), link),
                None => (attrs, None),
            };
            build_entry(name, path, &lstat, link, uids, gids)
        })
        .collect();
    sort_entries(&mut entries);
    Ok(entries)
}

/// Verifica links (e entradas sem tipo) no canal auxiliar, com limite e
/// prazo. Canal preso no prazo: descartado (e contado no disjuntor). Sem
/// auxiliar (o servidor recusou outro canal), usa o principal, como antes;
/// se ele prender, a verificacao automatica para nesta sessao. Com o
/// disjuntor desligado, nada e verificado (links "nao verificados").
async fn resolve_links(
    sftp: &Arc<SftpSession>,
    aux: &AuxSftp,
    jobs: Vec<(String, bool)>,
) -> Vec<Option<Resolved>> {
    let unchecked = |n: usize| (0..n).map(|_| None).collect();
    if aux.links_off().await {
        return unchecked(jobs.len());
    }
    let deadline = tokio::time::Instant::now() + LINK_BUDGET;
    // O primeiro uso abre o canal auxiliar, dentro do mesmo prazo.
    let Ok(ch) = tokio::time::timeout_at(deadline, aux.get()).await else {
        return unchecked(jobs.len());
    };
    let chan = ch.clone().unwrap_or_else(|| Arc::clone(sftp));
    let (done, stuck) = bounded(jobs, LINK_CONCURRENCY, LINK_MAX, deadline, move |(p, is_link)| {
        resolve(Arc::clone(&chan), p, is_link)
    })
    .await;
    if stuck {
        match &ch {
            Some(c) => aux.discard(c, true).await,
            None => aux.trip().await,
        }
    }
    done
}

// --- Canal SFTP: teto de pacote e canal auxiliar ------------------------------

/// Maior pacote SFTP aceito do servidor. O russh-sftp aloca o tamanho que o
/// servidor anunciar (ate 4 GiB, e uma falha de alocacao derruba o app
/// inteiro) antes de ler o pacote; o OpenSSH manda no maximo 256 KiB (e o
/// cliente dele recusa mais que isso).
pub(crate) const MAX_PACKET_IN: u32 = 1024 * 1024;

/// Stream do canal SFTP que confere o tamanho de cada pacote recebido antes
/// de entrega-lo ao russh-sftp: acima de `max`, o stream termina (EOF) e a
/// sessao SFTP acaba, em vez de alocar o que o servidor pedir.
pub(crate) struct CappedStream<S> {
    inner: S,
    max: u32,
    /// Bytes ja lidos do tamanho do pacote atual (0 a 3) e o valor parcial.
    head: u8,
    len: u32,
    /// Bytes que faltam do corpo do pacote atual.
    body: u32,
    /// Teto estourado: so EOF daqui em diante.
    dead: bool,
}

impl<S> CappedStream<S> {
    pub(crate) fn new(inner: S, max: u32) -> Self {
        CappedStream {
            inner,
            max,
            head: 0,
            len: 0,
            body: 0,
            dead: false,
        }
    }

    /// Acompanha os bytes recebidos (tamanho de 4 bytes + corpo, pacote a
    /// pacote). Se um pacote passa do teto, devolve quantos bytes deste
    /// trecho vem antes do tamanho dele (so esses seguem adiante).
    fn scan(&mut self, data: &[u8]) -> Option<usize> {
        let mut i = 0;
        // Onde comecou, neste trecho, o tamanho em leitura (0 se comecou
        // num trecho anterior).
        let mut head_at = 0;
        while i < data.len() {
            if self.body > 0 {
                let n = (self.body as usize).min(data.len() - i);
                self.body -= n as u32;
                i += n;
                continue;
            }
            if self.head == 0 {
                head_at = i;
            }
            self.len = (self.len << 8) | u32::from(data[i]);
            self.head += 1;
            i += 1;
            if self.head == 4 {
                if self.len > self.max {
                    return Some(head_at);
                }
                (self.body, self.head, self.len) = (self.len, 0, 0);
            }
        }
        None
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for CappedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.dead {
            return Poll::Ready(Ok(()));
        }
        let before = buf.filled().len();
        std::task::ready!(Pin::new(&mut this.inner).poll_read(cx, buf))?;
        if let Some(keep) = this.scan(&buf.filled()[before..]) {
            // Nada do pacote grande chega ao crate: o stream termina antes
            // do tamanho dele.
            this.dead = true;
            buf.set_filled(before + keep);
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CappedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Inicia o SFTP num canal com o subsistema ja pedido, com o teto de pacote
/// (`CappedStream`). Toda sessao SFTP do app passa por aqui.
pub(crate) async fn start_session<S>(
    stream: S,
) -> Result<SftpSession, russh_sftp::client::error::Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    SftpSession::new(CappedStream::new(stream, MAX_PACKET_IN)).await
}

/// Abre um canal SFTP extra na mesma conexao (`None`: o servidor recusou).
pub(crate) type AuxOpen =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Option<SftpSession>> + Send>> + Send + Sync>;

/// Canal SFTP auxiliar e descartavel, para o que pode travar o sftp-server:
/// seguir links (stat) na listagem, o caminho digitado na barra e as
/// leituras do visualizador. O sftp-server do OpenSSH atende um pedido por
/// vez e o protocolo nao cancela pedidos: um stat preso (NFS parado, FUSE
/// que nunca responde) ou o open de um fifo trocado depois do stat
/// segurariam o canal principal, e com ele a navegacao, os downloads e os
/// envios, ate reconectar. No auxiliar, o preso e so ele: e descartado
/// (fechado) e o proximo uso abre outro.
#[derive(Clone)]
pub(crate) struct AuxSftp {
    state: Arc<tokio::sync::Mutex<AuxState>>,
    open: AuxOpen,
}

#[derive(Default)]
struct AuxState {
    sftp: Option<Arc<SftpSession>>,
    /// Abertura em andamento (tarefa propria; fica `true` ao terminar).
    opening: Option<watch::Receiver<bool>>,
    /// Ate quando nao tentar abrir de novo (o servidor recusou).
    retry_at: Option<tokio::time::Instant>,
    /// Canais descartados presos na verificacao de links (disjuntor).
    stalls: u32,
}

impl AuxSftp {
    pub(crate) fn new(open: AuxOpen) -> Self {
        AuxSftp {
            state: Arc::new(tokio::sync::Mutex::new(AuxState::default())),
            open,
        }
    }

    /// Canal auxiliar, aberto no primeiro uso. `None` se o servidor nao deu
    /// outro canal: quem chama usa o principal. A abertura corre numa tarefa
    /// propria: quem desiste de esperar (o prazo de uma listagem, numa rede
    /// lenta) nao a cancela, e o proximo uso ja encontra o canal pronto.
    pub(crate) async fn get(&self) -> Option<Arc<SftpSession>> {
        loop {
            let mut done = {
                let mut st = self.state.lock().await;
                if let Some(s) = &st.sftp {
                    return Some(Arc::clone(s));
                }
                if st.retry_at.is_some_and(|t| tokio::time::Instant::now() < t) {
                    return None;
                }
                match &st.opening {
                    Some(rx) => rx.clone(),
                    None => {
                        let (tx, rx) = watch::channel(false);
                        st.opening = Some(rx.clone());
                        let (state, open) = (Arc::clone(&self.state), Arc::clone(&self.open));
                        tokio::spawn(async move {
                            let s = open().await;
                            let mut st = state.lock().await;
                            st.opening = None;
                            match s {
                                Some(s) => {
                                    st.sftp = Some(Arc::new(s));
                                    st.retry_at = None;
                                }
                                None => {
                                    st.retry_at = Some(tokio::time::Instant::now() + AUX_RETRY);
                                }
                            }
                            let _ = tx.send(true);
                        });
                        rx
                    }
                }
            };
            // Espera a abertura sem segurar o estado.
            if done.wait_for(|fim| *fim).await.is_err() {
                // A tarefa caiu sem terminar: como uma recusa.
                let mut st = self.state.lock().await;
                st.opening = None;
                st.retry_at = Some(tokio::time::Instant::now() + AUX_RETRY);
                return None;
            }
        }
    }

    /// Descarta o canal `s` (um pedido pode ter ficado preso nele): soltar a
    /// ultima referencia fecha o canal, e o proximo `get` abre outro.
    /// `stall` conta para o disjuntor da verificacao de links.
    pub(crate) async fn discard(&self, s: &Arc<SftpSession>, stall: bool) {
        let mut st = self.state.lock().await;
        if st.sftp.as_ref().is_some_and(|c| Arc::ptr_eq(c, s)) {
            st.sftp = None;
        }
        if stall {
            st.stalls += 1;
        }
    }

    /// Verificacao automatica de links desligada nesta sessao.
    async fn links_off(&self) -> bool {
        self.state.lock().await.stalls >= AUX_MAX_STALLS
    }

    /// Desliga a verificacao automatica (prendeu o canal principal, sem
    /// auxiliar para descartar).
    async fn trip(&self) {
        self.state.lock().await.stalls = AUX_MAX_STALLS;
    }
}

/// Abridor do canal auxiliar na conexao da sessao.
fn aux_opener(session: &Arc<russh::client::Handle<crate::ssh::Client>>) -> AuxOpen {
    let session = Arc::clone(session);
    Arc::new(move || {
        let session = Arc::clone(&session);
        Box::pin(async move { crate::upload::open_sftp(&session).await.ok() })
    })
}

/// Tarefa de uma leitura do visualizador: no canal auxiliar (o principal, se
/// nao houver). Se a leitura terminou com um pedido possivelmente preso
/// (cancelada, prazo, servidor sem resposta), o auxiliar e descartado.
async fn view_task(
    aux: AuxSftp,
    main: Arc<SftpSession>,
    id: u64,
    path: String,
    cancel: watch::Receiver<bool>,
    tx: UnboundedSender<ViewEvent>,
) {
    let ch = aux.get().await;
    let chan = ch.clone().unwrap_or(main);
    let clean = viewer::load(chan, id, path, cancel, tx).await;
    if let (false, Some(c)) = (clean, &ch) {
        aux.discard(c, false).await;
    }
}

/// Faz o parse de um arquivo no formato `/etc/passwd` ou `/etc/group`
/// (`nome:x:id:...`) preenchendo os mapas id->nome e nome->id.
fn parse_id_db(
    data: &[u8],
    id_to_name: &mut HashMap<u32, String>,
    name_to_id: &mut HashMap<String, u32>,
) {
    for line in String::from_utf8_lossy(data).lines() {
        let mut fields = line.split(':');
        let (Some(name), Some(_), Some(id)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        if let Ok(id) = id.trim().parse::<u32>() {
            id_to_name.entry(id).or_insert_with(|| name.to_string());
            name_to_id.insert(name.to_string(), id);
        }
    }
}

/// Resolve um texto em id numerico: aceita um numero direto ou um nome
/// existente em `name_to_id`. `None` se nao for possivel resolver.
fn resolve_id(text: &str, name_to_id: &HashMap<String, u32>) -> Option<u32> {
    let t = text.trim();
    if let Ok(n) = t.parse::<u32>() {
        return Some(n);
    }
    name_to_id.get(t).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn download_events_reach_ui_during_long_commands() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel();
            let (_view_tx, mut view_rx) = tokio::sync::mpsc::unbounded_channel();
            let (from_tx, from_rx) = std::sync::mpsc::channel();
            // Comando longo (ex.: envio grande) que so termina depois de a UI
            // ver o andamento do download que corre em paralelo.
            let work = async {
                dl_tx
                    .send(DownloadEvent::Scanning { id: 1, found: 5 })
                    .unwrap();
                loop {
                    if let Ok(SftpToUi::Download(DownloadEvent::Scanning { found, .. })) =
                        from_rx.try_recv()
                    {
                        assert_eq!(found, 5);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            };
            tokio::time::timeout(
                Duration::from_secs(5),
                forward_downloads(work, &mut dl_rx, &mut view_rx, &mut tokio::sync::mpsc::unbounded_channel().1, &from_tx, &|| {}),
            )
            .await
            .expect("o andamento ficou preso ate o fim do comando");
        });
    }

    /// A faixa "Abrindo" do visualizador tambem anda durante um comando longo.
    #[test]
    fn view_events_reach_ui_during_long_commands() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (_dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel();
            let (view_tx, mut view_rx) = tokio::sync::mpsc::unbounded_channel();
            let (from_tx, from_rx) = std::sync::mpsc::channel();
            let work = async {
                view_tx
                    .send(ViewEvent::Progress {
                        id: 7,
                        got: 512,
                        total: Some(1024),
                    })
                    .unwrap();
                loop {
                    if let Ok(SftpToUi::View(ViewEvent::Progress { id, got, total })) =
                        from_rx.try_recv()
                    {
                        assert_eq!((id, got, total), (7, 512, Some(1024)));
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            };
            tokio::time::timeout(
                Duration::from_secs(5),
                forward_downloads(work, &mut dl_rx, &mut view_rx, &mut tokio::sync::mpsc::unbounded_channel().1, &from_tx, &|| {}),
            )
            .await
            .expect("o andamento da leitura ficou preso ate o fim do comando");
        });
    }

    // --- Tipos e links simbolicos na listagem (1.1.0) ----------------------

    use russh_sftp::client::error::Error as SftpError;
    use russh_sftp::protocol::{Status, StatusCode};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn attrs(perms: u32, size: u64, uid: u32, mtime: u32) -> FileAttributes {
        FileAttributes {
            size: Some(size),
            uid: Some(uid),
            gid: Some(uid),
            permissions: Some(perms),
            mtime: Some(mtime),
            ..FileAttributes::empty()
        }
    }

    fn ids() -> HashMap<u32, String> {
        [(1000, "dono".to_string()), (0, "root".to_string())].into()
    }

    fn status(code: StatusCode) -> SftpError {
        SftpError::Status(Status {
            id: 1,
            status_code: code,
            error_message: String::new(),
            language_tag: String::new(),
        })
    }

    fn entry(name: &str, kind: EntryKind, link: Option<LinkState>) -> RemoteEntry {
        RemoteEntry {
            name: name.into(),
            path: format!("/srv/{name}"),
            kind,
            link: link.map(|state| LinkInfo {
                target: "alvo".into(),
                state,
            }),
            size: 0,
            mode: None,
            owner: String::new(),
            group: String::new(),
            mtime: 0,
        }
    }

    #[test]
    fn raw_kind_uses_type_bits_only() {
        let cases = [
            (0o040755, RawKind::Dir),
            (0o041777, RawKind::Dir),
            (0o100644, RawKind::File),
            (0o104755, RawKind::File),
            (0o120777, RawKind::Link),
            (0o010644, RawKind::Fifo),
            (0o020666, RawKind::CharDev),
            (0o060660, RawKind::BlockDev),
            (0o140755, RawKind::Socket),
            (0o000644, RawKind::Unknown),
        ];
        for (perms, want) in cases {
            assert_eq!(raw_kind(Some(perms)), want, "{perms:o}");
        }
        assert_eq!(raw_kind(None), RawKind::Unknown);
        // Os falsos positivos dos `is_*` do crate: um link nunca e arquivo
        // comum nem dispositivo; bloco e socket nunca sao pasta.
        assert_eq!(entry_kind(raw_kind(Some(0o060660))), EntryKind::Special(Special::BlockDev));
        assert_eq!(entry_kind(raw_kind(Some(0o140755))), EntryKind::Special(Special::Socket));
        assert_eq!(entry_kind(raw_kind(Some(0o120777))), EntryKind::Unknown);
    }

    #[test]
    fn link_state_from_stat_errors() {
        assert_eq!(link_state_from_err(&status(StatusCode::NoSuchFile)), LinkState::Broken);
        assert_eq!(
            link_state_from_err(&status(StatusCode::PermissionDenied)),
            LinkState::Denied
        );
        assert_eq!(link_state_from_err(&status(StatusCode::Failure)), LinkState::Unchecked);
        assert_eq!(link_state_from_err(&SftpError::Timeout), LinkState::Unchecked);
    }

    #[test]
    fn build_entry_link_uses_target_attrs() {
        let (uids, gids) = (ids(), ids());
        let lstat = attrs(0o120777, 11, 0, 111);
        // Link resolvido para arquivo: tipo, tamanho, modo e dono do destino.
        let ok = LinkRes {
            stat: Ok(attrs(0o100640, 4, 1000, 222)),
            target: Some("arquivo.txt".into()),
        };
        let e = build_entry("l".into(), "/d/l".into(), &lstat, Some(ok), &uids, &gids);
        assert_eq!(e.kind, EntryKind::File);
        assert!(!e.is_dir());
        assert_eq!((e.size, e.mode, e.mtime), (4, Some(0o640), 222));
        assert_eq!((e.owner.as_str(), e.group.as_str()), ("dono", "dono"));
        assert_eq!(
            e.link,
            Some(LinkInfo {
                target: "arquivo.txt".into(),
                state: LinkState::Ok
            })
        );
        // Link para pasta: pasta (entra com Enter).
        let dir = LinkRes {
            stat: Ok(attrs(0o040755, 4096, 0, 1)),
            target: Some("public_html".into()),
        };
        let e = build_entry("www".into(), "/d/www".into(), &lstat, Some(dir), &uids, &gids);
        assert!(e.is_dir());
        assert_eq!(e.mode, Some(0o755));
        // Quebrado: sem tipo, sem modo (nunca o 0777 do link), sem tamanho;
        // dono e data do proprio link.
        let broken = LinkRes {
            stat: Err(LinkState::Broken),
            target: Some("/nao/existe".into()),
        };
        let e = build_entry("q".into(), "/d/q".into(), &lstat, Some(broken), &uids, &gids);
        assert_eq!((e.kind, e.mode, e.size), (EntryKind::Unknown, None, 0));
        assert_eq!((e.owner.as_str(), e.mtime), ("root", 111));
        assert_eq!(e.link.as_ref().map(|l| l.state), Some(LinkState::Broken));
        assert_eq!(e.link.as_ref().map(|l| l.target.as_str()), Some("/nao/existe"));
        // stat que devolve link ou nada: nao verificado.
        let odd = LinkRes {
            stat: Ok(attrs(0o120777, 1, 0, 1)),
            target: None,
        };
        let e = build_entry("x".into(), "/d/x".into(), &lstat, Some(odd), &uids, &gids);
        assert_eq!(e.kind, EntryKind::Unknown);
        assert_eq!(
            e.link,
            Some(LinkInfo {
                target: String::new(),
                state: LinkState::Unchecked
            })
        );
        // Nao verificado (prazo/limite).
        let e = build_entry("n".into(), "/d/n".into(), &lstat, None, &uids, &gids);
        assert_eq!(e.link.map(|l| l.state), Some(LinkState::Unchecked));
        // Nome nao UTF-8 (U+FFFD): nunca "quebrado", mesmo com stat falho.
        let lossy = LinkRes {
            stat: Err(LinkState::Broken),
            target: None,
        };
        let e = build_entry("a\u{FFFD}".into(), "/d/a\u{FFFD}".into(), &lstat, Some(lossy), &uids, &gids);
        assert_eq!(e.link.map(|l| l.state), Some(LinkState::Unchecked));
        // Alvo gigante: cortado.
        let long = LinkRes {
            stat: Err(LinkState::Broken),
            target: Some("a".repeat(5000)),
        };
        let e = build_entry("g".into(), "/d/g".into(), &lstat, Some(long), &uids, &gids);
        assert_eq!(e.link.unwrap().target.chars().count(), LINK_TARGET_MAX + 1);
        // Entrada comum: tipo e modo pelo lstat; sem modo, desconhecido.
        let e = build_entry("f".into(), "/d/f".into(), &attrs(0o100600, 9, 0, 5), None, &uids, &gids);
        assert_eq!((e.kind, e.mode, e.size, e.link), (EntryKind::File, Some(0o600), 9, None));
        let e = build_entry("u".into(), "/d/u".into(), &FileAttributes::empty(), None, &uids, &gids);
        assert_eq!((e.kind, e.mode), (EntryKind::Unknown, None));
        let e = build_entry("p".into(), "/d/p".into(), &attrs(0o010644, 0, 0, 5), None, &uids, &gids);
        assert_eq!(e.kind, EntryKind::Special(Special::Fifo));
    }

    #[test]
    fn sort_entries_dirs_and_dir_links_first() {
        let mut v = vec![
            entry("b", EntryKind::File, None),
            entry("A", EntryKind::Dir, None),
            entry("www", EntryKind::Dir, Some(LinkState::Ok)),
            entry("c", EntryKind::File, Some(LinkState::Ok)),
            entry("z", EntryKind::Unknown, Some(LinkState::Broken)),
            entry("fifo", EntryKind::Special(Special::Fifo), None),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "www", "b", "c", "fifo", "z"]);
        // Sem diferenciar acentos (a mesma chave da busca por letras): "Ábaco"
        // fica junto de "abacate", nao depois de tudo o que e ASCII.
        let mut v = vec![
            entry("banana.txt", EntryKind::File, None),
            entry("Ábaco.txt", EntryKind::File, None),
            entry("abacate.txt", EntryKind::File, None),
            entry("Érica", EntryKind::Dir, None),
            entry("zeta", EntryKind::Dir, None),
            entry("casa", EntryKind::Dir, None),
        ];
        sort_entries(&mut v);
        let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["casa", "Érica", "zeta", "abacate.txt", "Ábaco.txt", "banana.txt"]);
        // Mesmo nome sem diferenciar maiusculas: ordem deterministica.
        for start in [["a.txt", "A.TXT"], ["A.TXT", "a.txt"]] {
            let mut v: Vec<RemoteEntry> =
                start.iter().map(|n| entry(n, EntryKind::File, None)).collect();
            sort_entries(&mut v);
            let names: Vec<&str> = v.iter().map(|e| e.name.as_str()).collect();
            assert_eq!(names, ["A.TXT", "a.txt"]);
        }
    }

    #[test]
    fn bounded_respects_concurrency_limit_and_deadline() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let live = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let t0 = std::time::Instant::now();
            let (l, p) = (Arc::clone(&live), Arc::clone(&peak));
            let em = |ms: u64| tokio::time::Instant::now() + Duration::from_millis(ms);
            let (out, stuck) = bounded((0..10).collect(), 3, 8, em(300), move |i: usize| {
                let (live, peak) = (Arc::clone(&l), Arc::clone(&p));
                async move {
                    let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    let ms = if i == 7 { 5000 } else { 10 };
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    live.fetch_sub(1, Ordering::SeqCst);
                    i * 10
                }
            })
            .await;
            assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
            let got: Vec<Option<usize>> = out;
            for (i, r) in got.iter().enumerate() {
                if i < 7 {
                    assert_eq!(*r, Some(i * 10), "job {i}");
                } else {
                    assert_eq!(*r, None, "job {i}");
                }
            }
            assert!(peak.load(Ordering::SeqCst) <= 3, "pico {}", peak.load(Ordering::SeqCst));
            // Respostas ate pouco antes do prazo: canal lento, nao preso.
            assert!(!stuck);

            // Vazio e rapido: tudo pronto.
            let (none, stuck): (Vec<Option<u8>>, _) =
                bounded(Vec::<u8>::new(), 3, 8, em(300), |x| async move { x }).await;
            assert!(none.is_empty() && !stuck);
            let (all, stuck) =
                bounded((0..20u32).collect(), 4, 100, em(5000), |x| async move { x + 1 }).await;
            assert_eq!(all, (1..=20u32).map(Some).collect::<Vec<_>>());
            assert!(!stuck);

            // Nenhuma resposta por mais de STUCK_GAP ate o prazo: preso.
            let (out, stuck) = bounded((0..3u32).collect(), 2, 10, em(800), |x| async move {
                if x == 0 {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                x
            })
            .await;
            assert_eq!(out, [None, Some(1), Some(2)]);
            assert!(stuck, "o job 0 nunca respondeu");
        });
    }

    /// Falhas ao listar ou abrir um caminho, em portugues e com o caminho
    /// neutralizado (texto do servidor ou digitado).
    #[test]
    fn listing_error_is_portuguese() {
        assert_eq!(
            path_error("/srv/x", &status(StatusCode::NoSuchFile)),
            "Caminho não encontrado: /srv/x"
        );
        assert_eq!(
            path_error("/srv/x", &status(StatusCode::PermissionDenied)),
            "Sem permissão para abrir /srv/x"
        );
        let outro = path_error("/srv/x", &status(StatusCode::Failure));
        assert!(outro.starts_with("Não foi possível abrir /srv/x: "), "{outro}");
        let outro = path_error("/srv/x", &SftpError::Timeout);
        assert!(outro.starts_with("Não foi possível abrir /srv/x: "), "{outro}");
        assert_eq!(
            path_error("/a\u{202E}b\u{1b}", &status(StatusCode::NoSuchFile)),
            "Caminho não encontrado: /a<U+202E>b^["
        );
        let longo = path_error(&"/d".repeat(1000), &status(StatusCode::NoSuchFile));
        assert!(longo.ends_with('\u{2026}'), "{longo}");
        assert!(longo.chars().count() < 400);
    }

    // --- Teto de pacote e canal auxiliar (servidor SFTP em memoria) --------

    use russh_sftp::protocol::{Attrs, Data, File, Handle, Name};

    /// Leitor que entrega um byte por vez (o tamanho do pacote chega picado).
    struct Drip(std::io::Cursor<Vec<u8>>);

    impl AsyncRead for Drip {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            let mut one = [0u8; 1];
            let n = std::io::Read::read(&mut self.get_mut().0, &mut one)?;
            buf.put_slice(&one[..n]);
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for Drip {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, b: &[u8]) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(b.len()))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Pacote SFTP cru: tamanho (4 bytes) e corpo.
    fn packet(len: u32, fill: u8) -> Vec<u8> {
        let mut v = len.to_be_bytes().to_vec();
        v.extend(std::iter::repeat_n(fill, len as usize));
        v
    }

    /// Um servidor hostil anuncia um pacote de ~4 GiB: nada dele chega ao
    /// russh-sftp (que alocaria o tamanho anunciado antes de ler) e o stream
    /// termina ali. Pacotes dentro do teto passam inteiros, mesmo picados.
    #[test]
    fn capped_stream_stops_at_oversized_packet() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut ok = packet(5, 1);
            ok.extend(packet(0, 0));
            ok.extend(packet(300, 2));
            let mut s = CappedStream::new(Drip(std::io::Cursor::new(ok.clone())), 300);
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, ok);
            let mut s = CappedStream::new(std::io::Cursor::new(ok.clone()), 300);
            let mut out = Vec::new();
            s.read_to_end(&mut out).await.unwrap();
            assert_eq!(out, ok, "em blocos grandes");

            let mut bad = packet(5, 1);
            bad.extend(0xFFFF_FFF0u32.to_be_bytes());
            bad.extend([9u8; 64]);
            bad.extend(packet(3, 3));
            for picado in [true, false] {
                let mut out = Vec::new();
                if picado {
                    let mut s = CappedStream::new(Drip(std::io::Cursor::new(bad.clone())), MAX_PACKET_IN);
                    s.read_to_end(&mut out).await.unwrap();
                } else {
                    let mut s = CappedStream::new(std::io::Cursor::new(bad.clone()), MAX_PACKET_IN);
                    s.read_to_end(&mut out).await.unwrap();
                }
                // So o primeiro pacote (e, picado, os bytes do tamanho do
                // outro que chegaram antes de ele ficar completo).
                assert!(out.len() < 9 + 4, "picado={picado}: {} bytes", out.len());
                assert_eq!(&out[..9], &packet(5, 1)[..], "picado={picado}");
                if !picado {
                    assert_eq!(out.len(), 9);
                }
            }
        });
    }

    /// Um caminho do servidor falso.
    #[derive(Clone)]
    struct FakeNode {
        lstat: FileAttributes,
        /// `None`: o stat nunca responde (NFS parado, FUSE preso).
        stat: Option<FileAttributes>,
        target: Option<String>,
        /// Caminho real (realpath), se diferente.
        real: Option<String>,
        data: Vec<u8>,
        /// Leitura a partir deste ponto nunca responde.
        hang_from: Option<u64>,
        /// Cada leitura devolve mais bytes do que o pedido (fora do protocolo).
        overflow: bool,
    }

    fn dir_node() -> FakeNode {
        let a = attrs(0o040755, 4096, 1000, 1);
        FakeNode {
            lstat: a.clone(),
            stat: Some(a),
            target: None,
            real: None,
            data: Vec::new(),
            hang_from: None,
            overflow: false,
        }
    }

    fn file_node(data: &[u8]) -> FakeNode {
        let a = attrs(0o100644, data.len() as u64, 1000, 1);
        FakeNode {
            lstat: a.clone(),
            stat: Some(a),
            data: data.to_vec(),
            ..dir_node()
        }
    }

    fn link_node(target: &str, stat: Option<FileAttributes>) -> FakeNode {
        FakeNode {
            lstat: attrs(0o120777, target.len() as u64, 1000, 1),
            stat,
            target: Some(target.into()),
            ..dir_node()
        }
    }

    /// Sistema de arquivos do servidor falso.
    #[derive(Default)]
    struct FakeFs {
        nodes: HashMap<String, FakeNode>,
        /// Pasta -> nomes.
        dirs: HashMap<String, Vec<String>>,
        /// Caminhos abertos com open().
        opens: std::sync::Mutex<Vec<String>>,
    }

    impl FakeFs {
        fn add(&mut self, path: &str, node: FakeNode) {
            if let Some((dir, name)) = path.rsplit_once('/').filter(|(d, _)| !d.is_empty()) {
                self.dirs.entry(dir.into()).or_default().push(name.into());
            }
            if node.stat.as_ref().is_some_and(|a| raw_kind(a.permissions) == RawKind::Dir)
                && node.target.is_none()
            {
                self.dirs.entry(path.into()).or_default();
            }
            self.nodes.insert(path.into(), node);
        }
    }

    /// /d com um link para pasta ("a"), um arquivo, um link cujo destino
    /// trava o stat ("z", se `hang`), um arquivo grande que trava no meio da
    /// leitura e um link para /proc/kmsg.
    fn fake_fs(hang: bool) -> Arc<FakeFs> {
        let mut fs = FakeFs::default();
        fs.add("/d", dir_node());
        fs.add("/d/alvo", dir_node());
        fs.add("/d/a", link_node("alvo", Some(attrs(0o040755, 4096, 1000, 1))));
        fs.add("/d/f.txt", file_node(b"ola\n"));
        if hang {
            fs.add("/d/z", link_node("/fuse/x", None));
        }
        fs.add(
            "/d/grande.txt",
            FakeNode {
                hang_from: Some(256 * 1024),
                ..file_node(&vec![b'a'; 600 * 1024])
            },
        );
        fs.add(
            "/d/kmsg",
            FakeNode {
                real: Some("/proc/kmsg".into()),
                ..link_node("/proc/kmsg", Some(attrs(0o100400, 0, 0, 1)))
            },
        );
        Arc::new(fs)
    }

    /// Servidor SFTP em memoria que, como o do OpenSSH, atende um pedido por
    /// vez: um stat preso segura tudo o que vier depois naquele canal.
    struct FakeServer {
        fs: Arc<FakeFs>,
        /// Pastas abertas (handle -> ja listou).
        listed: HashMap<String, bool>,
    }

    fn ok_status(id: u32) -> Status {
        Status {
            id,
            status_code: StatusCode::Ok,
            error_message: "Ok".into(),
            language_tag: "en-US".into(),
        }
    }

    impl russh_sftp::server::Handler for FakeServer {
        type Error = StatusCode;

        fn unimplemented(&self) -> StatusCode {
            StatusCode::OpUnsupported
        }

        fn opendir(&mut self, id: u32, path: String) -> impl Future<Output = Result<Handle, StatusCode>> + Send {
            let ok = self.fs.dirs.contains_key(&path);
            if ok {
                self.listed.insert(path.clone(), false);
            }
            async move { ok.then_some(Handle { id, handle: path }).ok_or(StatusCode::NoSuchFile) }
        }

        fn readdir(&mut self, id: u32, handle: String) -> impl Future<Output = Result<Name, StatusCode>> + Send {
            let first = self.listed.insert(handle.clone(), true) == Some(false);
            let files: Vec<File> = if first {
                self.fs.dirs[&handle]
                    .iter()
                    .map(|n| File {
                        filename: n.clone(),
                        longname: String::new(),
                        attrs: self.fs.nodes[&format!("{handle}/{n}")].lstat.clone(),
                    })
                    .collect()
            } else {
                Vec::new()
            };
            async move {
                if files.is_empty() {
                    Err(StatusCode::Eof)
                } else {
                    Ok(Name { id, files })
                }
            }
        }

        async fn close(&mut self, id: u32, _handle: String) -> Result<Status, StatusCode> {
            Ok(ok_status(id))
        }

        fn lstat(&mut self, id: u32, path: String) -> impl Future<Output = Result<Attrs, StatusCode>> + Send {
            let a = self.fs.nodes.get(&path).map(|n| n.lstat.clone());
            async move { a.map(|attrs| Attrs { id, attrs }).ok_or(StatusCode::NoSuchFile) }
        }

        fn stat(&mut self, id: u32, path: String) -> impl Future<Output = Result<Attrs, StatusCode>> + Send {
            let n = self.fs.nodes.get(&path).cloned();
            async move {
                match n.ok_or(StatusCode::NoSuchFile)?.stat {
                    Some(attrs) => Ok(Attrs { id, attrs }),
                    None => std::future::pending().await,
                }
            }
        }

        fn fstat(&mut self, id: u32, handle: String) -> impl Future<Output = Result<Attrs, StatusCode>> + Send {
            let a = self.fs.nodes.get(&handle).and_then(|n| n.stat.clone());
            async move { a.map(|attrs| Attrs { id, attrs }).ok_or(StatusCode::Failure) }
        }

        fn readlink(&mut self, id: u32, path: String) -> impl Future<Output = Result<Name, StatusCode>> + Send {
            let t = self.fs.nodes.get(&path).and_then(|n| n.target.clone());
            async move {
                t.map(|t| Name {
                    id,
                    files: vec![File::dummy(t)],
                })
                .ok_or(StatusCode::NoSuchFile)
            }
        }

        fn realpath(&mut self, id: u32, path: String) -> impl Future<Output = Result<Name, StatusCode>> + Send {
            let real = self.fs.nodes.get(&path).and_then(|n| n.real.clone()).unwrap_or(path);
            async move {
                Ok(Name {
                    id,
                    files: vec![File::dummy(real)],
                })
            }
        }

        fn open(
            &mut self,
            id: u32,
            filename: String,
            _pflags: OpenFlags,
            _attrs: FileAttributes,
        ) -> impl Future<Output = Result<Handle, StatusCode>> + Send {
            self.fs.opens.lock().unwrap().push(filename.clone());
            let ok = self.fs.nodes.contains_key(&filename);
            async move {
                ok.then_some(Handle {
                    id,
                    handle: filename,
                })
                .ok_or(StatusCode::NoSuchFile)
            }
        }

        fn read(
            &mut self,
            id: u32,
            handle: String,
            offset: u64,
            len: u32,
        ) -> impl Future<Output = Result<Data, StatusCode>> + Send {
            let n = self.fs.nodes.get(&handle).cloned();
            async move {
                let n = n.ok_or(StatusCode::Failure)?;
                if n.hang_from.is_some_and(|h| offset >= h) {
                    return std::future::pending().await;
                }
                let s = offset as usize;
                if s >= n.data.len() {
                    return Err(StatusCode::Eof);
                }
                let e = (s + len as usize).min(n.data.len());
                let mut data = n.data[s..e].to_vec();
                if n.overflow {
                    data.resize(len as usize + 16, b'x');
                }
                Ok(Data { id, data })
            }
        }
    }

    /// Sessao SFTP ligada a um servidor falso (canal proprio, em memoria).
    async fn fake_session(fs: &Arc<FakeFs>) -> SftpSession {
        let (client, server) = tokio::io::duplex(1 << 20);
        let handler = FakeServer {
            fs: Arc::clone(fs),
            listed: HashMap::new(),
        };
        russh_sftp::server::run(server, handler).await;
        start_session(client).await.expect("sessao falsa")
    }

    /// Canal auxiliar sobre servidores falsos (conta as aberturas). `refuse`:
    /// o servidor recusa o canal extra; `timeout`: prazo (s) por pedido.
    fn fake_aux(fs: &Arc<FakeFs>, refuse: bool, timeout: Option<u64>) -> (AuxSftp, Arc<AtomicUsize>) {
        let opened = Arc::new(AtomicUsize::new(0));
        let (fs, n) = (Arc::clone(fs), Arc::clone(&opened));
        let aux = AuxSftp::new(Arc::new(move || {
            n.fetch_add(1, Ordering::SeqCst);
            let fs = Arc::clone(&fs);
            Box::pin(async move {
                if refuse {
                    return None;
                }
                let s = fake_session(&fs).await;
                if let Some(t) = timeout {
                    s.set_timeout(t);
                }
                Some(s)
            })
        }));
        (aux, opened)
    }

    fn link_state_of(v: &[RemoteEntry], name: &str) -> Option<LinkState> {
        v.iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} nao listado"))
            .link
            .as_ref()
            .map(|l| l.state)
    }

    /// Um link cujo destino trava o stat (NFS parado, FUSE hostil) prende so
    /// o canal auxiliar: a listagem sai no prazo, o canal principal continua
    /// respondendo e o auxiliar preso e trocado. Depois de AUX_MAX_STALLS
    /// canais presos, a verificacao automatica para (sem abrir mais canais).
    #[test]
    fn link_stats_go_to_aux_and_never_hold_main() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = fake_fs(true);
            let main = Arc::new(fake_session(&fs).await);
            let (aux, opened) = fake_aux(&fs, false, None);
            let (uids, gids) = (ids(), ids());
            let t0 = tokio::time::Instant::now();
            let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
            assert!(t0.elapsed() < LINK_BUDGET + Duration::from_millis(700), "{:?}", t0.elapsed());
            assert_eq!(link_state_of(&v, "a"), Some(LinkState::Ok));
            assert!(v.iter().find(|e| e.name == "a").unwrap().is_dir());
            assert_eq!(link_state_of(&v, "z"), Some(LinkState::Unchecked));
            // O principal responde na hora: o stat preso ficou no auxiliar.
            let m = tokio::time::timeout(Duration::from_secs(2), main.metadata("/d/f.txt"))
                .await
                .expect("o canal principal ficou preso atras do stat");
            assert!(m.is_ok());
            assert_eq!(opened.load(Ordering::SeqCst), 1);
            // O auxiliar preso foi descartado: cada listagem abre outro.
            for n in 2..=AUX_MAX_STALLS as usize {
                let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
                assert_eq!(link_state_of(&v, "a"), Some(LinkState::Ok));
                assert_eq!(opened.load(Ordering::SeqCst), n);
            }
            // Disjuntor: nada verificado nem aberto, e a listagem sai na hora.
            let t0 = tokio::time::Instant::now();
            let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
            assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
            assert_eq!(opened.load(Ordering::SeqCst), AUX_MAX_STALLS as usize);
            assert_eq!(link_state_of(&v, "a"), Some(LinkState::Unchecked));
            assert!(tokio::time::timeout(Duration::from_secs(2), main.metadata("/d")).await.is_ok());
        });
    }

    /// Abrir o auxiliar demora mais que o prazo da listagem (rede lenta): a
    /// listagem sai no prazo sem verificar, mas a abertura continua, e a
    /// listagem seguinte ja usa o canal (sem abrir outro).
    #[test]
    fn slow_aux_open_is_not_cancelled_by_the_listing() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = fake_fs(false);
            let main = Arc::new(fake_session(&fs).await);
            let opened = Arc::new(AtomicUsize::new(0));
            let (fs2, n) = (Arc::clone(&fs), Arc::clone(&opened));
            let aux = AuxSftp::new(Arc::new(move || {
                n.fetch_add(1, Ordering::SeqCst);
                let fs = Arc::clone(&fs2);
                Box::pin(async move {
                    tokio::time::sleep(LINK_BUDGET + Duration::from_millis(300)).await;
                    Some(fake_session(&fs).await)
                })
            }));
            let (uids, gids) = (ids(), ids());
            let t0 = tokio::time::Instant::now();
            let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
            assert!(t0.elapsed() < LINK_BUDGET + Duration::from_millis(500), "{:?}", t0.elapsed());
            assert_eq!(link_state_of(&v, "a"), Some(LinkState::Unchecked));
            tokio::time::sleep(Duration::from_millis(700)).await;
            let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
            assert_eq!(link_state_of(&v, "a"), Some(LinkState::Ok));
            assert_eq!(opened.load(Ordering::SeqCst), 1, "a listagem cancelou a abertura");
        });
    }

    /// Servidor que nao da outro canal (ex.: MaxSessions 1): os links sao
    /// verificados no principal, como antes, sem tentar abrir o auxiliar a
    /// cada listagem.
    #[test]
    fn aux_refused_falls_back_to_main() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = fake_fs(false);
            let main = Arc::new(fake_session(&fs).await);
            let (aux, opened) = fake_aux(&fs, true, None);
            let (uids, gids) = (ids(), ids());
            for _ in 0..2 {
                let v = list_entries(&main, &aux, "/d", &uids, &gids).await.unwrap();
                assert_eq!(link_state_of(&v, "a"), Some(LinkState::Ok));
                assert!(v.iter().find(|e| e.name == "a").unwrap().is_dir());
            }
            assert_eq!(opened.load(Ordering::SeqCst), 1, "tentou de novo dentro de AUX_RETRY");
        });
    }

    /// O caminho digitado na barra (stat, que segue links) vai pelo auxiliar:
    /// um destino que trava vira "nao respondeu a tempo" sem prender o
    /// principal, e o auxiliar preso e trocado no proximo uso.
    #[test]
    fn goto_stat_goes_to_aux() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = fake_fs(true);
            let main = Arc::new(fake_session(&fs).await);
            let (aux, opened) = fake_aux(&fs, false, Some(1));
            let (uids, gids) = (ids(), ids());
            let t0 = tokio::time::Instant::now();
            let r = goto_path(&main, &aux, "/d/z", &uids, &gids).await;
            assert_eq!(r.err().as_deref(), Some(GOTO_TIMEOUT_MSG));
            assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
            let m = tokio::time::timeout(Duration::from_secs(2), main.metadata("/d/f.txt"))
                .await
                .expect("o canal principal ficou preso atras do stat");
            assert!(m.is_ok());
            let (kind, entries) = goto_path(&main, &aux, "/d/alvo", &uids, &gids).await.unwrap();
            assert_eq!((kind, entries.map(|v| v.len())), (GotoKind::Dir, Some(0)));
            assert_eq!(opened.load(Ordering::SeqCst), 2, "o auxiliar preso nao foi trocado");
        });
    }

    /// Leituras do visualizador no auxiliar: uma leitura normal o mantem; um
    /// link para /proc/kmsg e recusado sem open(); uma leitura presa no meio e
    /// cancelada sem `Done`, e o auxiliar e trocado (o preso nao volta).
    #[test]
    fn view_task_uses_aux_and_drops_it_when_stuck() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fs = fake_fs(false);
            let main = Arc::new(fake_session(&fs).await);
            let (aux, opened) = fake_aux(&fs, false, None);
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let done = |rx: &mut UnboundedReceiver<ViewEvent>| {
                let mut out = Vec::new();
                while let Ok(ev) = rx.try_recv() {
                    if let ViewEvent::Done { id, result } = ev {
                        out.push((id, result.map(|d| d.text)));
                    }
                }
                out
            };
            let (_c1, r1) = download::cancel_pair();
            view_task(aux.clone(), Arc::clone(&main), 1, "/d/f.txt".into(), r1, tx.clone()).await;
            assert_eq!(done(&mut rx), [(1, Ok("ola\n".to_string()))]);
            let (_c2, r2) = download::cancel_pair();
            view_task(aux.clone(), Arc::clone(&main), 2, "/d/kmsg".into(), r2, tx.clone()).await;
            assert_eq!(done(&mut rx), [(2, Err(viewer::ViewError::Draining))]);
            assert!(!fs.opens.lock().unwrap().contains(&"/d/kmsg".to_string()), "abriu o kmsg");
            assert_eq!(opened.load(Ordering::SeqCst), 1, "leituras limpas mantem o auxiliar");

            let (c3, r3) = download::cancel_pair();
            let task = tokio::spawn(view_task(
                aux.clone(),
                Arc::clone(&main),
                3,
                "/d/grande.txt".into(),
                r3,
                tx.clone(),
            ));
            // O primeiro bloco chega; o segundo trava no servidor.
            tokio::time::sleep(Duration::from_millis(400)).await;
            assert!(!task.is_finished(), "a leitura devia estar presa");
            c3.cancel();
            tokio::time::timeout(Duration::from_secs(2), task)
                .await
                .expect("a tarefa nao terminou ao cancelar")
                .unwrap();
            assert!(done(&mut rx).is_empty(), "Done de uma leitura cancelada");
            // O principal nunca leu arquivo nenhum e continua livre.
            assert!(tokio::time::timeout(Duration::from_secs(2), main.metadata("/d")).await.is_ok());
            let _ = aux.get().await;
            assert_eq!(opened.load(Ordering::SeqCst), 2, "o auxiliar preso continuou em uso");
        });
    }

    /// Um panico dentro da sessao (aqui o do russh-sftp quando o servidor
    /// manda mais dados do que o pedido, como na leitura do /etc/passwd ao
    /// conectar) chega a UI como erro, seguido do `Closed`: o painel nao fica
    /// em "Conectando..." para sempre. O gancho padrao imprime o panico.
    #[test]
    fn session_panic_reaches_ui_as_error() {
        let runtime = || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
        };
        let mut fs = FakeFs::default();
        fs.add(
            "/etc/passwd",
            FakeNode {
                overflow: true,
                ..file_node(b"root:x:0:0:root:/root:/bin/bash\n")
            },
        );
        let fs = Arc::new(fs);
        let (tx, rx) = std::sync::mpsc::channel();
        let repaints = AtomicUsize::new(0);
        let repaint = || {
            repaints.fetch_add(1, Ordering::SeqCst);
        };
        let session = async {
            let sftp = fake_session(&fs).await;
            sftp.read("/etc/passwd").await?;
            Ok::<(), anyhow::Error>(())
        };
        run_to_end(&runtime(), session, &tx, &repaint);
        let got: Vec<SftpToUi> = rx.try_iter().collect();
        assert!(
            matches!(got.as_slice(), [SftpToUi::Error(e), SftpToUi::Closed] if e == SESSION_PANIC),
            "{} eventos",
            got.len()
        );
        assert_eq!(repaints.load(Ordering::SeqCst), 2);
        // Fim normal: so o Closed; com erro, o erro antes.
        run_to_end(&runtime(), async { Ok(()) }, &tx, &|| {});
        assert!(matches!(rx.try_iter().collect::<Vec<_>>().as_slice(), [SftpToUi::Closed]));
        run_to_end(&runtime(), async { Err(anyhow::anyhow!("recusado")) }, &tx, &|| {});
        let got: Vec<SftpToUi> = rx.try_iter().collect();
        assert!(matches!(got.as_slice(), [SftpToUi::Error(e), SftpToUi::Closed] if e == "recusado"));
    }

    // --- Ponta a ponta contra um sshd real (ignorados) ---------------------
    //
    // Mesmas variaveis dos outros e2e (SAGU_E2E_PORT com o sshd de
    // `SetEnv TMUX`, SAGU_E2E_USER, SAGU_E2E_KEY). As fixtures ficam em
    // /tmp/sagu-e2e-*-<pid> e sao apagadas no Drop (inclusive se falhar).
    // Rodar com: cargo test e2e_listing -- --ignored --test-threads=1

    use crate::download::tests::{close, e2e_host, remote_sh, sftp_session};

    /// Apaga a fixture remota no fim (a pasta 000 precisa de chmod antes).
    struct RemoteFixture {
        host: Host,
        dir: String,
    }

    impl Drop for RemoteFixture {
        fn drop(&mut self) {
            let d = &self.dir;
            if let Err(e) = remote_sh(&self.host, &format!("chmod -R u+rwx '{d}'; rm -rf '{d}'")) {
                eprintln!("limpeza remota falhou: {e}");
            }
        }
    }

    /// Pede a listagem de `path` e espera a resposta (erro da sessao = panico).
    fn listing(h: &SftpHandle, path: &str) -> Vec<RemoteEntry> {
        h.list_dir(path);
        let t0 = std::time::Instant::now();
        while t0.elapsed() < Duration::from_secs(30) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Listing { path: p, entries }) if p == path => return entries,
                Ok(SftpToUi::Error(e)) => panic!("erro da sessao: {e}"),
                Ok(SftpToUi::Closed) => panic!("sessao fechou"),
                _ => {}
            }
        }
        panic!("listagem de {path} nao chegou");
    }

    fn find<'a>(v: &'a [RemoteEntry], name: &str) -> &'a RemoteEntry {
        v.iter()
            .find(|e| e.name == name)
            .unwrap_or_else(|| panic!("{name} nao listado"))
    }

    fn state(e: &RemoteEntry) -> Option<LinkState> {
        e.link.as_ref().map(|l| l.state)
    }

    fn target(e: &RemoteEntry) -> &str {
        e.link.as_ref().map_or("", |l| l.target.as_str())
    }

    #[test]
    #[ignore]
    fn e2e_listing_kinds_and_links() {
        let host = e2e_host();
        let t = format!("/tmp/sagu-e2e-tipos-{}", std::process::id());
        let _fixture = RemoteFixture {
            host: host.clone(),
            dir: t.clone(),
        };
        let script = format!(
            r#"set -e; T='{t}'; rm -rf "$T"; mkdir -p "$T"; cd "$T"
mkdir pasta; printf 'x' > pasta/dentro.txt
printf 'ola\n' > arquivo.txt; chmod 640 arquivo.txt
ln -s pasta link-pasta; ln -s arquivo.txt link-arquivo; ln -s link-pasta link-de-link
ln -s "$T" link-para-si; ln -s "$T/nao-existe" link-quebrado
ln -s ciclo-b ciclo-a; ln -s ciclo-a ciclo-b; mkfifo fifo
mkdir 'pasta com espaço'; printf 'x' > 'arquivo ação.txt'; ln -s 'pasta com espaço' 'link acentuação'
mkdir sem-permissao; ln -s sem-permissao/dentro link-atraves-sem-perm; chmod 000 sem-permissao
ln -s /dev/null link-dispositivo
"#
        );
        remote_sh(&host, &script).expect("preparo da fixture");
        let h = sftp_session(&host);
        let v = listing(&h, &t);

        let pasta = find(&v, "pasta");
        assert_eq!((pasta.kind, &pasta.link), (EntryKind::Dir, &None));
        let arq = find(&v, "arquivo.txt");
        assert_eq!((arq.kind, arq.size, arq.mode), (EntryKind::File, 4, Some(0o640)));
        for (name, alvo) in [
            ("link-pasta", "pasta".to_string()),
            ("link-de-link", "link-pasta".to_string()),
            ("link-para-si", t.clone()),
            ("link acentuação", "pasta com espaço".to_string()),
        ] {
            let e = find(&v, name);
            assert!(e.is_dir(), "{name}: {:?}", e.kind);
            assert_eq!((state(e), target(e)), (Some(LinkState::Ok), alvo.as_str()), "{name}");
        }
        // Link para arquivo: atributos do destino (nunca os 11 B/0777 do link).
        let la = find(&v, "link-arquivo");
        assert_eq!((la.kind, la.size, la.mode), (EntryKind::File, 4, Some(0o640)));
        assert_eq!((state(la), target(la)), (Some(LinkState::Ok), "arquivo.txt"));
        let q = find(&v, "link-quebrado");
        assert_eq!((q.kind, q.size, q.mode), (EntryKind::Unknown, 0, None));
        assert_eq!(state(q), Some(LinkState::Broken));
        assert_eq!(target(q), format!("{t}/nao-existe"));
        for c in ["ciclo-a", "ciclo-b"] {
            assert_eq!(state(find(&v, c)), Some(LinkState::Broken), "{c}");
        }
        assert_eq!(state(find(&v, "link-atraves-sem-perm")), Some(LinkState::Denied));
        let dev = find(&v, "link-dispositivo");
        assert_eq!(dev.kind, EntryKind::Special(Special::CharDev));
        assert_eq!((state(dev), target(dev)), (Some(LinkState::Ok), "/dev/null"));
        assert_eq!(find(&v, "fifo").kind, EntryKind::Special(Special::Fifo));
        assert!(find(&v, "pasta com espaço").is_dir());
        assert_eq!(find(&v, "arquivo ação.txt").kind, EntryKind::File);
        let sp = find(&v, "sem-permissao");
        assert_eq!((sp.kind, sp.mode), (EntryKind::Dir, Some(0)));
        // Pastas (inclusive links para pasta) primeiro.
        let first_other = v.iter().position(|e| !e.is_dir()).unwrap();
        assert!(v[first_other..].iter().all(|e| !e.is_dir()), "pasta depois de arquivo");
        assert_eq!(first_other, 7, "{:?}", v.iter().map(|e| &e.name).collect::<Vec<_>>());

        // Listar pelo link entra no destino.
        let dentro = listing(&h, &format!("{t}/link-pasta"));
        assert_eq!(dentro.len(), 1);
        assert_eq!(dentro[0].name, "dentro.txt");
        assert_eq!(dentro[0].path, format!("{t}/link-pasta/dentro.txt"));

        // Excluir o link (is_dir falso, como a UI manda): so o link sai.
        h.remove(format!("{t}/link-pasta"), false, t.clone());
        let v = listing(&h, &t);
        assert!(v.iter().all(|e| e.name != "link-pasta"), "o link continua");
        assert!(find(&v, "pasta").is_dir());
        let dentro = listing(&h, &format!("{t}/pasta"));
        assert_eq!(dentro.len(), 1, "o conteudo do destino sumiu");
        close(h);
    }

    #[test]
    #[ignore]
    fn e2e_listing_many_links_is_bounded() {
        let host = e2e_host();
        let t = format!("/tmp/sagu-e2e-links-{}", std::process::id());
        let _fixture = RemoteFixture {
            host: host.clone(),
            dir: t.clone(),
        };
        let script = format!(
            r#"set -e; T='{t}'; rm -rf "$T"; mkdir -p "$T/alvo"; cd "$T"
i=1; while [ $i -le 3000 ]; do ln -s alvo "l$(printf %04d $i)"; i=$((i+1)); done
"#
        );
        remote_sh(&host, &script).expect("preparo da fixture");
        let h = sftp_session(&host);
        let t0 = std::time::Instant::now();
        let v = listing(&h, &t);
        let took = t0.elapsed();
        assert!(took < LINK_BUDGET + Duration::from_secs(3), "demorou {took:?}");
        assert_eq!(v.len(), 3001);
        // Resolvidos (links para pasta) junto das pastas, no topo; o resto
        // nao verificado, depois delas.
        let dirs = v.iter().take_while(|e| e.is_dir()).count();
        assert!(v[dirs..].iter().all(|e| !e.is_dir()));
        let resolved: Vec<&RemoteEntry> =
            v.iter().filter(|e| state(e) == Some(LinkState::Ok)).collect();
        assert_eq!(dirs, resolved.len() + 1, "alvo + links resolvidos");
        assert!(!resolved.is_empty() && resolved.len() <= LINK_MAX, "{}", resolved.len());
        assert!(v[dirs..].iter().all(|e| state(e) == Some(LinkState::Unchecked)
            && e.kind == EntryKind::Unknown));
        // Os resolvidos sao os primeiros por nome (a menos dos que estavam
        // em voo quando o prazo cortou).
        let last = resolved.iter().map(|e| e.name.clone()).max().unwrap();
        let n: usize = last[1..].parse().unwrap();
        assert!(n <= resolved.len() + LINK_CONCURRENCY, "{last} com {} resolvidos", resolved.len());
        eprintln!("{} links resolvidos em {took:?}", resolved.len());
        close(h);
    }

    /// Resposta a um caminho digitado na barra: o `Goto` e, numa pasta, a
    /// listagem que vem atras dele (nomes).
    type GotoAnswer = (Result<GotoKind, String>, Option<(String, Vec<String>)>);

    fn goto_answer(h: &SftpHandle, seq: u64, path: &str) -> GotoAnswer {
        h.goto(seq, path);
        let t0 = std::time::Instant::now();
        let mut res = None;
        while t0.elapsed() < Duration::from_secs(30) {
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Goto { seq: s, result }) => {
                    assert_eq!(s, seq, "{path}");
                    if result != Ok(GotoKind::Dir) {
                        // Sem listagem atras: confere que nada mais chega.
                        if let Ok(m) = h.from_sftp.recv_timeout(Duration::from_millis(300)) {
                            let extra = match m {
                                SftpToUi::Listing { path, .. } => format!("listagem de {path}"),
                                SftpToUi::Error(e) => format!("erro {e}"),
                                _ => "outra mensagem".into(),
                            };
                            panic!("{path}: {extra} depois de {result:?}");
                        }
                        return (result, None);
                    }
                    res = Some(result);
                }
                Ok(SftpToUi::Listing { path: p, entries }) => {
                    let r = res.expect("listagem antes do Goto");
                    let names = entries.iter().map(|e| e.name.clone()).collect();
                    return (r, Some((p, names)));
                }
                Ok(SftpToUi::Error(e)) => panic!("{path}: erro solto: {e}"),
                Ok(SftpToUi::Closed) => panic!("sessao fechou"),
                _ => {}
            }
        }
        panic!("{path}: sem resposta");
    }

    #[test]
    #[ignore]
    fn e2e_goto_kinds_and_errors() {
        let host = e2e_host();
        let t = format!("/tmp/sagu-e2e-goto-{}", std::process::id());
        let _fixture = RemoteFixture {
            host: host.clone(),
            dir: t.clone(),
        };
        let script = format!(
            r#"set -e; T='{t}'; rm -rf "$T"; mkdir -p "$T/pasta/sub" "$T/fechada"; cd "$T"
printf 'ola' > arquivo.txt
ln -s pasta link-pasta; ln -s arquivo.txt link-arq; ln -s /nao/existe/sagu quebrado
ln -s ciclo-b ciclo-a; ln -s ciclo-a ciclo-b; mkfifo fifo; chmod 000 fechada
"#
        );
        remote_sh(&host, &script).expect("preparo da fixture");
        let h = sftp_session(&host);
        let p = |n: &str| format!("{t}/{n}");

        // Pasta e link para pasta: Dir, com a listagem no caminho digitado.
        for (seq, n) in [(1, "pasta"), (2, "link-pasta")] {
            let (r, listing) = goto_answer(&h, seq, &p(n));
            assert_eq!(r, Ok(GotoKind::Dir), "{n}");
            assert_eq!(listing, Some((p(n), vec!["sub".to_string()])), "{n}");
        }
        // Arquivo e link para arquivo: File, sem listagem.
        for (seq, n) in [(3, "arquivo.txt"), (4, "link-arq")] {
            assert_eq!(goto_answer(&h, seq, &p(n)), (Ok(GotoKind::File), None), "{n}");
        }
        // Erros em portugues (a barra continua em edicao com eles).
        let casos = [
            (5, "quebrado", format!("Caminho não encontrado: {}", p("quebrado"))),
            (6, "ciclo-a", format!("Caminho não encontrado: {}", p("ciclo-a"))),
            (7, "nao-existe", format!("Caminho não encontrado: {}", p("nao-existe"))),
            (8, "fifo", format!("Não é uma pasta nem um arquivo comum: {}", p("fifo"))),
            (9, "fechada", format!("Sem permissão para abrir {}", p("fechada"))),
        ];
        for (seq, n, msg) in casos {
            assert_eq!(goto_answer(&h, seq, &p(n)), (Err(msg), None), "{n}");
        }
        // A listagem comum (ListDir) tambem erra em portugues.
        h.list_dir(p("nao-existe"));
        let t0 = std::time::Instant::now();
        let msg = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "sem resposta do ListDir");
            match h.from_sftp.recv_timeout(Duration::from_millis(200)) {
                Ok(SftpToUi::Error(e)) => break e,
                Ok(SftpToUi::Listing { path, .. }) => panic!("listou {path}"),
                _ => {}
            }
        };
        assert_eq!(msg, format!("Caminho não encontrado: {}", p("nao-existe")));
        close(h);
    }

    /// Servidor que nao da um segundo canal (a porta B da receita tem
    /// `MaxSessions 1`): os links continuam resolvidos (no canal principal,
    /// como antes do canal auxiliar), e o caminho digitado e o visualizador
    /// tambem funcionam. Mesmas variaveis, mais SAGU_E2E_PORT_B.
    #[test]
    #[ignore]
    fn e2e_single_channel_server_still_resolves() {
        let mut host = e2e_host();
        host.port = std::env::var("SAGU_E2E_PORT_B")
            .expect("defina SAGU_E2E_PORT_B")
            .parse()
            .expect("SAGU_E2E_PORT_B invalida");
        let t = format!("/tmp/sagu-e2e-canal-unico-{}", std::process::id());
        let _fixture = RemoteFixture {
            host: host.clone(),
            dir: t.clone(),
        };
        let script = format!(
            r#"set -e; T='{t}'; rm -rf "$T"; mkdir -p "$T/pasta"; cd "$T"
printf 'ola\n' > a.txt; ln -s pasta www; ln -s a.txt link-a; ln -s /nao/existe quebrado
"#
        );
        remote_sh(&host, &script).expect("preparo da fixture");
        let h = sftp_session(&host);
        let v = listing(&h, &t);
        let www = find(&v, "www");
        assert!(www.is_dir(), "{:?}", www.kind);
        assert_eq!((state(www), target(www)), (Some(LinkState::Ok), "pasta"));
        assert_eq!(find(&v, "link-a").kind, EntryKind::File);
        assert_eq!(state(find(&v, "quebrado")), Some(LinkState::Broken));
        assert_eq!(goto_answer(&h, 1, &format!("{t}/link-a")), (Ok(GotoKind::File), None));
        let _cancel = h.read_file(7, format!("{t}/link-a")).expect("sessao encerrada");
        let t0 = std::time::Instant::now();
        let doc = loop {
            assert!(t0.elapsed() < Duration::from_secs(30), "leitura sem resposta");
            if let Ok(SftpToUi::View(ViewEvent::Done { id: 7, result })) =
                h.from_sftp.recv_timeout(Duration::from_millis(200))
            {
                break result.expect("leitura pelo canal principal");
            }
        };
        assert_eq!(doc.text, "ola\n");
        close(h);
    }
}
