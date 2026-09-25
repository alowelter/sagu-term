//! Sessao SFTP em segundo plano usando `russh` + `russh-sftp`.
//!
//! Segue o mesmo padrao da sessao SSH ([`crate::ssh`]): a UI e sincrona, entao a
//! sessao roda numa thread dedicada com runtime tokio e conversa por dois canais:
//! - [`UiToSftp`]: a UI pede listagem de diretorio ou desconexao;
//! - [`SftpToUi`]: a sessao devolve status, listagens e erros.
//!
//! Downloads rodam em tarefas proprias (ver [`crate::download`]), fora do loop
//! de comandos: navegar, renomear etc. continuam respondendo durante a copia.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::download::{self, DownloadEvent, DownloadItem};
use crate::hostkey::HostKeyPrompt;
use crate::vault::Host;

/// Uma entrada (arquivo ou pasta) de um diretorio remoto.
pub struct RemoteEntry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Bits de permissao (modo POSIX, ex.: 0o644).
    pub mode: u32,
    /// Proprietario: nome do usuario quando possivel traduzir, senao o uid.
    pub owner: String,
    /// Grupo: nome quando possivel traduzir, senao o gid.
    pub group: String,
    /// Data da ultima alteracao (segundos desde a epoca Unix; 0 se ausente).
    pub mtime: u32,
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
}

/// Mensagens da UI para a sessao SFTP.
pub enum UiToSftp {
    /// Pede a listagem do diretorio indicado.
    ListDir(String),
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

    pub fn disconnect(&self) {
        let _ = self.to_sftp.send(UiToSftp::Disconnect);
    }
}

/// Inicia uma sessao SFTP em segundo plano. `repaint` acorda o loop do egui
/// sempre que houver novidade.
pub fn connect<F>(host: Host, repaint: F) -> SftpHandle
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

        rt.block_on(async move {
            if let Err(e) = run_session(host, to_sftp_rx, &from_sftp_tx, &repaint).await {
                let _ = from_sftp_tx.send(SftpToUi::Error(format!("{e}")));
                repaint();
            }
            let _ = from_sftp_tx.send(SftpToUi::Closed);
            repaint();
        });
    });

    SftpHandle {
        to_sftp: to_sftp_tx,
        from_sftp: from_sftp_rx,
    }
}

async fn run_session<F>(
    host: Host,
    mut to_sftp_rx: UnboundedReceiver<UiToSftp>,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    repaint: &F,
) -> anyhow::Result<()>
where
    F: Fn() + Send + 'static,
{
    // Conexao e autenticacao compartilhadas com a sessao SSH (mesma politica
    // de timeouts e de chave do servidor, num unico ponto de manutencao).
    let session = crate::ssh::connect_and_auth(&host, |p| {
        let _ = from_sftp.send(SftpToUi::HostKey(p));
        repaint();
    })
    .await?;

    let channel = session.channel_open_session().await?;
    channel.request_subsystem(true, "sftp").await?;
    // Compartilhada com as tarefas de download (pedidos concorrentes no mesmo
    // canal; `&sftp` continua valendo nos comandos abaixo).
    let sftp = Arc::new(
        SftpSession::new(channel.into_stream())
            .await
            .map_err(|e| anyhow::anyhow!("nao foi possivel iniciar o SFTP: {e}"))?,
    );

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

    // Downloads: cada lote roda numa tarefa propria (nunca dentro deste loop,
    // que continua atendendo a navegacao) e reporta por `dl_rx`, repassado a
    // UI aqui (mesmo padrao dos envios em `ssh::run_session`).
    let mut downloads: JoinSet<()> = JoinSet::new();
    let (dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel::<DownloadEvent>();

    loop {
        let cmd = tokio::select! {
            Some(ev) = dl_rx.recv() => {
                let _ = from_sftp.send(SftpToUi::Download(ev));
                repaint();
                continue;
            }
            // Recolhe tarefas terminadas (o JoinSet as guarda ate serem lidas).
            Some(_) = downloads.join_next(), if !downloads.is_empty() => continue,
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
            UiToSftp::Disconnect => break,
            cmd => cmd,
        };
        // Os demais comandos esperam o servidor (um envio grande leva
        // minutos); enquanto isso, o andamento dos downloads segue para a UI.
        let work = async {
            match cmd {
                UiToSftp::ListDir(path) => {
                    send_listing(&sftp, from_sftp, &path, &uid_to_name, &gid_to_name).await;
                    repaint();
                }
                UiToSftp::Upload { local, remote_dir } => {
                    match upload_file(&sftp, &local, &remote_dir).await {
                        Ok(()) => {
                            // Recarrega o diretorio para o arquivo recem-enviado aparecer.
                            send_listing(&sftp, from_sftp, &remote_dir, &uid_to_name, &gid_to_name)
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
                            send_listing(&sftp, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
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
                            send_listing(&sftp, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
                                .await
                        }
                        Err(e) => {
                            let _ = from_sftp
                                .send(SftpToUi::Error(format!("falha ao alterar permissoes: {e}")));
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
                                        from_sftp,
                                        &refresh_dir,
                                        &uid_to_name,
                                        &gid_to_name,
                                    )
                                    .await
                                }
                                Err(e) => {
                                    let _ = from_sftp.send(SftpToUi::Error(format!(
                                        "falha ao alterar proprietario/grupo: {e}"
                                    )));
                                }
                            }
                        }
                        _ => {
                            let mut faltando = Vec::new();
                            if uid.is_none() {
                                faltando.push(format!("usuario \"{owner}\""));
                            }
                            if gid.is_none() {
                                faltando.push(format!("grupo \"{group}\""));
                            }
                            let _ = from_sftp.send(SftpToUi::Error(format!(
                                "nao foi possivel resolver {}",
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
                            send_listing(&sftp, from_sftp, &refresh_dir, &uid_to_name, &gid_to_name)
                                .await
                        }
                        Err(e) => {
                            let _ =
                                from_sftp.send(SftpToUi::Error(format!("falha ao excluir: {e}")));
                        }
                    }
                    repaint();
                }
                UiToSftp::Download { .. } | UiToSftp::Disconnect => {}
            }
        };
        forward_downloads(work, &mut dl_rx, from_sftp, repaint).await;
    }

    // Entrega o que ja estava na fila (ex.: um download que terminou junto) e
    // aborta os que estao em andamento: as tarefas sao soltas e os guards
    // apagam os temporarios (a UI avisa o usuario).
    while let Ok(ev) = dl_rx.try_recv() {
        let _ = from_sftp.send(SftpToUi::Download(ev));
    }
    repaint();
    downloads.abort_all();

    Ok(())
}

/// Espera `work` repassando a UI, enquanto isso, os eventos dos downloads:
/// sem isso o rodape do download congelaria durante um envio grande.
async fn forward_downloads<F: Fn()>(
    work: impl std::future::Future<Output = ()>,
    dl_rx: &mut UnboundedReceiver<DownloadEvent>,
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
        .ok_or_else(|| anyhow::anyhow!("caminho de origem invalido"))?
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
    sftp: &SftpSession,
    from_sftp: &std::sync::mpsc::Sender<SftpToUi>,
    path: &str,
    uid_to_name: &HashMap<u32, String>,
    gid_to_name: &HashMap<u32, String>,
) {
    match sftp.read_dir(path.to_string()).await {
        Ok(dir) => {
            let mut entries: Vec<RemoteEntry> = dir
                .map(|e| {
                    let ft = e.file_type();
                    let meta = e.metadata();
                    // Proprietario/grupo: nome enviado pelo servidor; senao
                    // traduz o id pela base /etc/passwd|group; senao o numero.
                    let owner = meta
                        .user
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| meta.uid.and_then(|u| uid_to_name.get(&u).cloned()))
                        .or_else(|| meta.uid.map(|u| u.to_string()))
                        .unwrap_or_default();
                    let group = meta
                        .group
                        .clone()
                        .filter(|s| !s.is_empty())
                        .or_else(|| meta.gid.and_then(|g| gid_to_name.get(&g).cloned()))
                        .or_else(|| meta.gid.map(|g| g.to_string()))
                        .unwrap_or_default();
                    RemoteEntry {
                        name: e.file_name(),
                        path: e.path(),
                        is_dir: ft.is_dir(),
                        size: meta.size.unwrap_or(0),
                        mode: meta.permissions.unwrap_or(0) & 0o7777,
                        owner,
                        group,
                        mtime: meta.mtime.unwrap_or(0),
                    }
                })
                .collect();
            // Pastas primeiro, depois arquivos; cada grupo em ordem alfabetica
            // (sem diferenciar maiusculas/minusculas).
            entries.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            let _ = from_sftp.send(SftpToUi::Listing {
                path: path.to_string(),
                entries,
            });
        }
        Err(e) => {
            let _ = from_sftp.send(SftpToUi::Error(format!("{path}: {e}")));
        }
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
                forward_downloads(work, &mut dl_rx, &from_tx, &|| {}),
            )
            .await
            .expect("o andamento ficou preso ate o fim do comando");
        });
    }
}
