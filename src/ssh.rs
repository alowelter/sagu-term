//! Sessao SSH em segundo plano usando `russh` (Rust puro, backend `ring`).
//!
//! A interface grafica e sincrona, entao a sessao roda numa thread dedicada com
//! um runtime tokio. A comunicacao acontece por dois canais:
//! - `UiToSsh`: a UI envia teclado/redimensionamento/desconexao;
//! - `SshToUi`: a sessao envia status e bytes recebidos do servidor.
//!
//! Arquivos soltos sobre o terminal sao enviados por tarefas paralelas na
//! mesma conexao (ver [`crate::upload`]).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{decode_secret_key, PrivateKeyWithHashAlg, PublicKey};
use russh::ChannelMsg;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinSet;

use crate::hostkey::{self, HostKeyAnswer, HostKeyPrompt, KeyCheck};
use crate::upload::{self, UploadEvent};
use crate::vault::{AuthMethod, Host};

/// Mensagens da sessao SSH para a UI.
pub enum SshToUi {
    Connected,
    Data(Vec<u8>),
    Error(String),
    Closed,
    /// Andamento/resultado de um envio de arquivos soltos sobre o terminal.
    Upload(UploadEvent),
    /// Chave do servidor nova ou diferente da guardada: a UI pergunta ao usuario
    /// e responde pelo `reply` do prompt (descartar = cancelar).
    HostKey(HostKeyPrompt),
}

/// Mensagens da UI para a sessao SSH.
pub enum UiToSsh {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Disconnect,
    /// Arquivos soltos sobre o terminal: descobre a pasta do shell e envia,
    /// ou devolve um `UploadEvent::Plan` para o usuario decidir.
    DropFiles { id: u64, files: Vec<PathBuf> },
    /// Envia para a pasta escolhida pelo usuario (resposta a um `Plan`);
    /// `replace` lista os nomes que ele autorizou substituir.
    Upload {
        id: u64,
        dir: String,
        files: Vec<PathBuf>,
        replace: Vec<String>,
    },
}

/// Handler do cliente russh. So captura a chave que o servidor apresentou; quem
/// decide se ela e confiavel e `connect_and_auth`, logo apos a troca de chaves e
/// ANTES de qualquer autenticacao. Aceitar aqui mantem o laco da sessao russh
/// vivo durante a pergunta (esperar dentro deste metodo o congela: uma queda do
/// servidor so seria notada depois da resposta). Compartilhado com o SFTP.
///
/// O campo privado garante que so este modulo cria o handler: toda conexao
/// passa por `connect_and_auth` e, portanto, pela verificacao da chave.
pub(crate) struct Client {
    server_key: Option<oneshot::Sender<PublicKey>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKey) -> Result<bool, Self::Error> {
        // Chamado so na troca de chaves inicial (nao nas renegociacoes).
        if let Some(tx) = self.server_key.take() {
            let _ = tx.send(key.clone());
        }
        Ok(true)
    }
}

/// Conecta e autentica uma sessao russh com os dados do host. Unico ponto de
/// configuracao (timeouts, politica de chave do servidor, metodos de
/// autenticacao), compartilhado entre as sessoes de terminal SSH e SFTP.
///
/// `ask` entrega a UI a pergunta sobre uma chave de servidor nova ou diferente
/// da guardada em `host.host_key` (no maximo uma vez por conexao).
///
/// INVARIANTE DE SEGURANCA: ate `verify_host_key` aceitar a chave, so trafegam
/// a troca de chaves e o pedido do servico de autenticacao (sem segredo algum).
/// Nenhum `authenticate_*`, `best_supported_rsa_hash` ou abertura de canal pode
/// vir antes dela: usuario, senha e assinatura da chave so saem depois.
pub(crate) async fn connect_and_auth(
    host: &Host,
    ask: impl Fn(HostKeyPrompt),
) -> anyhow::Result<client::Handle<Client>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        preferred: hostkey::preferred(host.host_key.as_deref()),
        ..Default::default()
    });

    let (key_tx, mut key_rx) = oneshot::channel();
    let handler = Client {
        server_key: Some(key_tx),
    };
    let mut session = client::connect(config, (host.host.as_str(), host.port), handler)
        .await
        .map_err(|e| anyhow::anyhow!("nao foi possivel conectar: {e}"))?;

    // A chave chega no oneshot antes do Handle existir (troca de chaves inicial).
    let presented = key_rx
        .try_recv()
        .map_err(|_| anyhow::anyhow!("O servidor não apresentou a chave de host."))?;
    verify_host_key(&mut session, host, &presented, &ask).await?;

    let authenticated = match &host.auth {
        AuthMethod::Password { password } => session
            .authenticate_password(host.username.clone(), password.clone())
            .await?
            .success(),
        AuthMethod::Key {
            private_key,
            passphrase,
        } => {
            let key = decode_secret_key(private_key, passphrase.as_deref())
                .map_err(|e| anyhow::anyhow!("chave privada invalida: {e}"))?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            session
                .authenticate_publickey(
                    host.username.clone(),
                    PrivateKeyWithHashAlg::new(Arc::new(key), hash),
                )
                .await?
                .success()
        }
    };

    if !authenticated {
        anyhow::bail!("falha na autenticacao (credenciais rejeitadas)");
    }
    Ok(session)
}

/// Confere a chave apresentada com a guardada no host; se for nova ou
/// diferente, pergunta a UI e espera. Nenhuma credencial sai antes disso.
async fn verify_host_key(
    session: &mut client::Handle<Client>,
    host: &Host,
    presented: &PublicKey,
    ask: &impl Fn(HostKeyPrompt),
) -> anyhow::Result<()> {
    let line = hostkey::openssh_line(presented)
        .map_err(|e| anyhow::anyhow!("Chave do servidor inválida: {e}"))?;
    if hostkey::check(host.host_key.as_deref(), &line) == KeyCheck::Match {
        return Ok(());
    }
    let (tx, rx) = oneshot::channel();
    ask(HostKeyPrompt {
        host_id: host.id,
        host: host.host.clone(),
        port: host.port,
        presented: line,
        reply: tx,
    });
    // Espera a resposta sem travar a sessao russh: se o servidor derrubar a
    // conexao (ex.: LoginGraceTime), o Handle termina e a pergunta cai. Depois
    // que esse ramo fica pronto o Handle nunca mais e consultado (so abortar).
    let answer = tokio::select! {
        a = rx => a,
        _ = &mut *session => anyhow::bail!(
            "O servidor encerrou a conexão enquanto aguardava a confirmação da chave; conecte de novo."
        ),
    };
    let msg = match answer {
        Ok(HostKeyAnswer::Accept) => return Ok(()),
        Ok(HostKeyAnswer::Cancel(msg)) => msg,
        Err(_) => {
            "Conexão cancelada: a confirmação da chave do servidor foi interrompida.".to_string()
        }
    };
    let _ = session
        .disconnect(russh::Disconnect::HostKeyNotVerifiable, "", "")
        .await;
    anyhow::bail!("{msg}")
}

/// Lado da UI: enviar comandos e receber eventos da sessao.
pub struct SshHandle {
    to_ssh: UnboundedSender<UiToSsh>,
    pub from_ssh: std::sync::mpsc::Receiver<SshToUi>,
    /// Aceita arquivos soltos (so sessoes SSH; terminais locais nao).
    supports_upload: bool,
}

impl SshHandle {
    /// Constroi um handle a partir dos canais ja criados. Usado por backends
    /// alternativos (ex.: terminal local via PTY) que reutilizam o mesmo
    /// protocolo `UiToSsh`/`SshToUi`.
    pub(crate) fn from_parts(
        to_ssh: UnboundedSender<UiToSsh>,
        from_ssh: std::sync::mpsc::Receiver<SshToUi>,
    ) -> Self {
        SshHandle {
            to_ssh,
            from_ssh,
            supports_upload: false,
        }
    }

    /// Handle ligado a canais de teste: devolve tambem o lado "sessao" (o que
    /// a UI mandou e por onde injetar eventos).
    #[cfg(test)]
    pub(crate) fn test_pair(
        supports_upload: bool,
    ) -> (
        Self,
        UnboundedReceiver<UiToSsh>,
        std::sync::mpsc::Sender<SshToUi>,
    ) {
        let (to_tx, to_rx) = tokio::sync::mpsc::unbounded_channel();
        let (from_tx, from_rx) = std::sync::mpsc::channel();
        let handle = SshHandle {
            to_ssh: to_tx,
            from_ssh: from_rx,
            supports_upload,
        };
        (handle, to_rx, from_tx)
    }

    pub fn supports_upload(&self) -> bool {
        self.supports_upload
    }

    /// Arquivos soltos sobre o terminal (ver `UiToSsh::DropFiles`). `false`
    /// se a sessao ja terminou (nenhuma resposta vira).
    pub fn drop_files(&self, id: u64, files: Vec<PathBuf>) -> bool {
        self.to_ssh.send(UiToSsh::DropFiles { id, files }).is_ok()
    }

    /// Envia para a pasta escolhida pelo usuario (ver `UiToSsh::Upload`).
    /// `false` se a sessao ja terminou.
    pub fn upload(&self, id: u64, dir: String, files: Vec<PathBuf>, replace: Vec<String>) -> bool {
        self.to_ssh
            .send(UiToSsh::Upload {
                id,
                dir,
                files,
                replace,
            })
            .is_ok()
    }

    pub fn send_data(&self, data: Vec<u8>) {
        let _ = self.to_ssh.send(UiToSsh::Data(data));
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let _ = self.to_ssh.send(UiToSsh::Resize { cols, rows });
    }

    pub fn disconnect(&self) {
        let _ = self.to_ssh.send(UiToSsh::Disconnect);
    }
}

/// Inicia uma sessao SSH em segundo plano. `repaint` e chamado sempre que houver
/// novidade, para acordar o loop de renderizacao do egui.
pub fn connect<F>(host: Host, cols: u16, rows: u16, repaint: F) -> SshHandle
where
    F: Fn() + Send + 'static,
{
    let (to_ssh_tx, to_ssh_rx) = tokio::sync::mpsc::unbounded_channel::<UiToSsh>();
    let (from_ssh_tx, from_ssh_rx) = std::sync::mpsc::channel::<SshToUi>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                let _ = from_ssh_tx.send(SshToUi::Error(format!("runtime: {e}")));
                repaint();
                return;
            }
        };

        rt.block_on(async move {
            if let Err(e) = run_session(host, cols, rows, to_ssh_rx, &from_ssh_tx, &repaint).await {
                let _ = from_ssh_tx.send(SshToUi::Error(format!("{e}")));
                repaint();
            }
            let _ = from_ssh_tx.send(SshToUi::Closed);
            repaint();
        });
    });

    SshHandle {
        to_ssh: to_ssh_tx,
        from_ssh: from_ssh_rx,
        supports_upload: true,
    }
}

async fn run_session<F>(
    host: Host,
    cols: u16,
    rows: u16,
    mut to_ssh_rx: UnboundedReceiver<UiToSsh>,
    from_ssh: &std::sync::mpsc::Sender<SshToUi>,
    repaint: &F,
) -> anyhow::Result<()>
where
    F: Fn() + Send + 'static,
{
    // Pergunta sobre a chave do servidor vai para o painel (ver `hostkey`).
    let ask = |p| {
        let _ = from_ssh.send(SshToUi::HostKey(p));
        repaint();
    };
    // Compartilhada com as tarefas de envio de arquivos (canais extras na
    // mesma conexao; abrir canal so precisa de `&self`).
    let session = Arc::new(connect_and_auth(&host, ask).await?);

    let mut channel = session.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
        .await?;
    channel.request_shell(true).await?;

    let _ = from_ssh.send(SshToUi::Connected);
    repaint();

    // Distingue o encerramento esperado (usuario desconectou ou o shell saiu
    // com `exit`) da queda de conexao: nesse ultimo caso devolve erro para a UI
    // preservar o painel com a mensagem em vez de fecha-lo silenciosamente.
    //
    // No `exit`, o OpenSSH envia EOF, depois exit-status e por fim CLOSE. Por
    // isso o EOF nao encerra o loop (o exit-status ainda esta a caminho); um
    // CLOSE do servidor e um fechamento ordenado do canal. Queda de conexao
    // aparece como fim do canal (`None`) sem CLOSE nem exit-status.
    let mut clean_exit = false;

    // Envios de arquivos: cada lote roda numa tarefa propria (nunca dentro
    // deste loop: um canal nao drenado trava a conexao inteira, shell junto)
    // e reporta por `up_rx`, repassado a UI aqui.
    let mut uploads: JoinSet<()> = JoinSet::new();
    let (up_tx, mut up_rx) = tokio::sync::mpsc::unbounded_channel::<UploadEvent>();

    loop {
        tokio::select! {
            Some(ev) = up_rx.recv() => {
                let _ = from_ssh.send(SshToUi::Upload(ev));
                repaint();
            }
            // Recolhe tarefas terminadas (o JoinSet as guarda ate serem lidas).
            Some(_) = uploads.join_next(), if !uploads.is_empty() => {}
            cmd = to_ssh_rx.recv() => {
                match cmd {
                    Some(UiToSsh::Data(data)) => {
                        channel.data(&data[..]).await?;
                    }
                    Some(UiToSsh::Resize { cols, rows }) => {
                        channel.window_change(cols as u32, rows as u32, 0, 0).await?;
                    }
                    Some(UiToSsh::Disconnect) | None => {
                        clean_exit = true;
                        let _ = channel.eof().await;
                        break;
                    }
                    Some(UiToSsh::DropFiles { id, files }) => {
                        let (session, tx) = (Arc::clone(&session), up_tx.clone());
                        uploads.spawn(async move {
                            if let Err(e) = upload::handle_drop(&session, id, files, &tx).await {
                                let _ = tx.send(UploadEvent::Failed { id, error: format!("{e:#}") });
                            }
                        });
                    }
                    Some(UiToSsh::Upload { id, dir, files, replace }) => {
                        let (session, tx) = (Arc::clone(&session), up_tx.clone());
                        uploads.spawn(async move {
                            let r = upload::handle_upload(&session, id, dir, files, replace, &tx).await;
                            if let Err(e) = r {
                                let _ = tx.send(UploadEvent::Failed { id, error: format!("{e:#}") });
                            }
                        });
                    }
                }
            }
            msg = channel.wait() => {
                match msg {
                    Some(ChannelMsg::Data { data }) => {
                        let _ = from_ssh.send(SshToUi::Data(data.to_vec()));
                        repaint();
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let _ = from_ssh.send(SshToUi::Data(data.to_vec()));
                        repaint();
                    }
                    Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::ExitSignal { .. }) => {
                        clean_exit = true;
                    }
                    // Fim da saida do shell; exit-status e CLOSE vem em seguida.
                    Some(ChannelMsg::Eof) => {}
                    Some(ChannelMsg::Close) => {
                        clean_exit = true;
                        break;
                    }
                    None => {
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    // Entrega o que ja estava na fila (ex.: um envio que terminou junto com
    // o `exit`) antes de encerrar; os envios em andamento morrem com a
    // sessao (a UI avisa o usuario).
    while let Ok(ev) = up_rx.try_recv() {
        let _ = from_ssh.send(SshToUi::Upload(ev));
    }
    repaint();
    uploads.abort_all();

    if !clean_exit {
        anyhow::bail!("conexao perdida (a sessao caiu sem encerramento normal)");
    }
    Ok(())
}
