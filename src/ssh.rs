//! Sessao SSH em segundo plano usando `russh` (Rust puro, backend `ring`).
//!
//! A interface grafica e sincrona, entao a sessao roda numa thread dedicada com
//! um runtime tokio. A comunicacao acontece por dois canais:
//! - `UiToSsh`: a UI envia teclado/redimensionamento/desconexao;
//! - `SshToUi`: a sessao envia status e bytes recebidos do servidor.

use std::sync::Arc;
use std::time::Duration;

use russh::client;
use russh::keys::{decode_secret_key, PrivateKeyWithHashAlg};
use russh::ChannelMsg;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::vault::{AuthMethod, Host};

/// Mensagens da sessao SSH para a UI.
pub enum SshToUi {
    Connected,
    Data(Vec<u8>),
    Error(String),
    Closed,
}

/// Mensagens da UI para a sessao SSH.
pub enum UiToSsh {
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
    Disconnect,
}

/// Handler do cliente russh. Aceita a chave do servidor automaticamente
/// (TOFU desabilitado para simplicidade). Compartilhado com a sessao SFTP.
pub(crate) struct Client;

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// Conecta e autentica uma sessao russh com os dados do host. Unico ponto de
/// configuracao (timeouts, politica de chave do servidor, metodos de
/// autenticacao), compartilhado entre as sessoes de terminal SSH e SFTP.
pub(crate) async fn connect_and_auth(host: &Host) -> anyhow::Result<client::Handle<Client>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let mut session = client::connect(config, (host.host.as_str(), host.port), Client)
        .await
        .map_err(|e| anyhow::anyhow!("nao foi possivel conectar: {e}"))?;

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

/// Lado da UI: enviar comandos e receber eventos da sessao.
pub struct SshHandle {
    to_ssh: UnboundedSender<UiToSsh>,
    pub from_ssh: std::sync::mpsc::Receiver<SshToUi>,
}

impl SshHandle {
    /// Constroi um handle a partir dos canais ja criados. Usado por backends
    /// alternativos (ex.: terminal local via PTY) que reutilizam o mesmo
    /// protocolo `UiToSsh`/`SshToUi`.
    pub(crate) fn from_parts(
        to_ssh: UnboundedSender<UiToSsh>,
        from_ssh: std::sync::mpsc::Receiver<SshToUi>,
    ) -> Self {
        SshHandle { to_ssh, from_ssh }
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
    let session = connect_and_auth(&host).await?;

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

    loop {
        tokio::select! {
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

    if !clean_exit {
        anyhow::bail!("conexao perdida (a sessao caiu sem encerramento normal)");
    }
    Ok(())
}
