//! Envio de arquivos soltos sobre um terminal SSH para a pasta atual do shell.
//!
//! Tudo roda na MESMA conexao do terminal (sem nova autenticacao), em tarefas
//! separadas do loop do shell (`ssh::run_session`):
//! 1. um canal exec roda `sh -s` com o script `cwd_probe.sh` no stdin, que
//!    acha o shell interativo do terminal em /proc e devolve a pasta dele (ou
//!    do programa em primeiro plano);
//! 2. um canal com o subsistema SFTP confere o destino e envia os arquivos.
//!
//! So envia sem perguntar quando a pasta e confiavel (`confident`), gravavel e
//! nenhum arquivo com o mesmo nome existe la. Nos demais casos devolve um
//! `DropPlan` para a UI perguntar ao usuario. Arquivos existentes nunca sao
//! sobrescritos sem autorizacao explicita (e nunca atraves de symlinks).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use russh::client;
use russh::ChannelMsg;
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::{FileAttributes, OpenFlags, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::timeout;

use crate::ssh::Client;

/// Script de descoberta da pasta (sempre LF; ver .gitattributes).
const PROBE_SCRIPT: &str = include_str!("cwd_probe.sh");

/// Tempo para o servidor abrir um canal. Um pedido de abertura nao pode ser
/// cancelado no russh, entao o limite e folgado (conexao lenta, nao travada).
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Tempo total do script de descoberta (exec + resposta).
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
/// Resposta maxima lida do script (ruido de .bashrc incluso).
const PROBE_MAX_OUTPUT: usize = 64 * 1024;
/// 255 KiB = max-write-length do OpenSSH: cada bloco vira um SSH_FXP_WRITE.
const CHUNK: usize = 255 * 1024;
/// Tempo maximo sem avancar um bloco (ou para finalizar) antes de desistir.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// Intervalo minimo entre eventos de progresso (nao inunda a UI).
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// Programas cujo cwd equivale ao "onde estou" do usuario quando estao em
/// primeiro plano (um shell aninhado, por exemplo).
const KNOWN_SHELLS: &[&str] = &[
    "bash", "zsh", "fish", "sh", "dash", "ash", "ksh", "mksh", "tcsh", "csh", "busybox",
];

/// Resultado da descoberta da pasta do shell remoto.
#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    /// Como a pasta foi obtida: tmux | fg | shell | home | none.
    pub method: String,
    /// ok | tmux-failed | multiplexer | fg-unreadable | cwd-unreadable |
    /// no-shell | no-proc; ou, do lado do app: timeout | exec-refused | no-marker.
    pub reason: String,
    /// O usuario SSH pode gravar em `dir`.
    pub writable: bool,
    /// Programa em primeiro plano no terminal (ou o proprio shell).
    pub fg_comm: String,
    /// Pasta proposta (absoluta), se houver.
    pub dir: Option<String>,
    /// Pasta do shell interativo, para comparar com a do programa em uso.
    pub shell_dir: Option<String>,
}

impl Probe {
    fn failed(reason: &str) -> Self {
        Probe {
            method: "none".into(),
            reason: reason.into(),
            writable: false,
            fg_comm: String::new(),
            dir: None,
            shell_dir: None,
        }
    }
}

/// Destino incerto: a UI mostra as opcoes e o usuario decide.
#[derive(Clone, Debug)]
pub struct DropPlan {
    pub id: u64,
    pub files: Vec<PathBuf>,
    pub probe: Probe,
    /// Pasta pessoal do usuario SSH (alternativa sempre disponivel).
    pub home: String,
    /// Nomes que ja existem em `probe.dir`.
    pub conflicts: Vec<String>,
}

/// Eventos de um envio, repassados a UI pelo loop da sessao.
#[derive(Clone, Debug)]
pub enum UploadEvent {
    /// O usuario precisa escolher o destino (ou autorizar substituicoes).
    Plan(DropPlan),
    /// Andamento do arquivo `index` (0-based) de `count`.
    Progress {
        id: u64,
        dir: String,
        index: usize,
        count: usize,
        name: String,
        sent: u64,
        size: u64,
    },
    /// Lote concluido: enviados e falhas (nome, motivo).
    Finished {
        id: u64,
        dir: String,
        sent: Vec<String>,
        failed: Vec<(String, String)>,
    },
    /// O lote inteiro falhou (ex.: servidor sem SFTP).
    Failed { id: u64, error: String },
}

/// Programas que abrem outra sessao (outra maquina, conteiner, usuario ou
/// terminal): o "onde estou" do usuario fica dentro deles, invisivel daqui.
/// Com um deles em primeiro plano, sempre pergunta.
const SESSION_HOSTS: &[&str] = &[
    "ssh", "mosh-client", "telnet", "sshpass", "autossh", "script", "asciinema", "docker",
    "podman", "kubectl", "lxc", "incus", "nsenter", "machinectl", "chroot", "distrobox",
    "toolbox", "su", "sudo", "doas", "tmux", "screen", "zellij",
];

/// A pasta da descoberta e confiavel o bastante para enviar sem perguntar?
pub fn confident(p: &Probe) -> bool {
    if !p.writable || p.dir.is_none() || p.reason != "ok" {
        return false;
    }
    let comm = p.fg_comm.as_str();
    match p.method.as_str() {
        // Shell no prompt (o lider da sessao precisa ser um shell de fato: um
        // login que troca o shell por `exec ssh ...` nao conta).
        "shell" => KNOWN_SHELLS.contains(&comm),
        // Programa em primeiro plano: um shell aninhado, ou um programa local
        // na mesma pasta do shell. Um `ssh`, `docker exec` ou `sudo` em uso
        // nunca muda de pasta, mas leva o usuario para outro lugar.
        "fg" => {
            KNOWN_SHELLS.contains(&comm)
                || (p.dir == p.shell_dir && !SESSION_HOSTS.contains(&comm))
        }
        // tmux: a pasta do painel ativo e boa, mas o painel pode estar num
        // ssh/sudo invisivel daqui; confirma.
        _ => false,
    }
}

/// Descobre a pasta do shell, confere o destino e envia (se confiavel) ou
/// devolve um `DropPlan` para o usuario decidir.
pub async fn handle_drop(
    session: &client::Handle<Client>,
    id: u64,
    files: Vec<PathBuf>,
    tx: &UnboundedSender<UploadEvent>,
) -> anyhow::Result<()> {
    let probe = probe_cwd(session).await;
    let sftp = open_sftp(session).await?;
    let home = sftp.canonicalize(".").await.unwrap_or_default();
    let conflicts = match &probe.dir {
        Some(dir) => existing_names(&sftp, dir, &files).await,
        None => Vec::new(),
    };
    match &probe.dir {
        Some(dir) if confident(&probe) && conflicts.is_empty() => {
            upload_files(&sftp, id, dir, &files, &[], tx).await;
        }
        _ => {
            let _ = tx.send(UploadEvent::Plan(DropPlan {
                id,
                files,
                probe,
                home,
                conflicts,
            }));
        }
    }
    let _ = sftp.close().await;
    Ok(())
}

/// Envia para a pasta escolhida pelo usuario. `replace`: nomes que ele
/// autorizou substituir.
pub async fn handle_upload(
    session: &client::Handle<Client>,
    id: u64,
    dir: String,
    files: Vec<PathBuf>,
    replace: Vec<String>,
    tx: &UnboundedSender<UploadEvent>,
) -> anyhow::Result<()> {
    let sftp = open_sftp(session).await?;
    upload_files(&sftp, id, &dir, &files, &replace, tx).await;
    let _ = sftp.close().await;
    Ok(())
}

/// Roda o script de descoberta num canal exec. Nunca falha: problemas viram
/// um `Probe` sem pasta (e o usuario escolhe o destino).
async fn probe_cwd(session: &client::Handle<Client>) -> Probe {
    let nonce = format!("{:016x}", rand::random::<u64>());
    // `replace('\r')`: defesa extra caso o arquivo seja salvo com CRLF.
    let script = PROBE_SCRIPT.replace('\r', "").replace("@NONCE@", &nonce);

    let mut ch = match timeout(OPEN_TIMEOUT, session.channel_open_session()).await {
        Ok(Ok(ch)) => ch,
        Ok(Err(_)) => return Probe::failed("exec-refused"),
        Err(_) => return Probe::failed("timeout"),
    };
    let result = timeout(PROBE_TIMEOUT, async {
        // `sh -s`: o shell de login do usuario (bash, zsh, fish, csh...) so
        // precisa interpretar isso; o script vai pelo stdin, sem aspas.
        ch.exec(true, "sh -s").await?;
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Success) => break,
                Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => {
                    return Ok::<_, anyhow::Error>(None)
                }
                _ => {}
            }
        }
        ch.data(script.as_bytes()).await?;
        ch.eof().await?;
        // Le ate a linha de resposta chegar inteira; nao espera o canal fechar
        // (um processo de fundo do .bashrc pode segurar a saida aberta).
        let mut out: Vec<u8> = Vec::new();
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if out.len() < PROBE_MAX_OUTPUT {
                        out.extend_from_slice(&data);
                    }
                    if let Some(p) = parse_probe(&out, &nonce) {
                        return Ok(Some(p));
                    }
                }
                Some(ChannelMsg::Close) | None => return Ok(parse_probe(&out, &nonce)),
                // stderr/avisos do shell: ignorados.
                _ => {}
            }
        }
    })
    .await;
    // Um canal russh comum nao fecha sozinho ao ser descartado.
    let _ = ch.close().await;
    match result {
        Ok(Ok(Some(p))) => p,
        Ok(Ok(None)) => Probe::failed("no-marker"),
        Ok(Err(_)) => Probe::failed("exec-refused"),
        Err(_) => Probe::failed("timeout"),
    }
}

/// Interpreta a saida do script. `None` enquanto a linha `@@SAGUCWD` com o
/// nonce nao chegou inteira (terminada em '\n').
fn parse_probe(out: &[u8], nonce: &str) -> Option<Probe> {
    let cwd_prefix = format!("@@SAGUCWD {nonce} 1 ");
    let shd_prefix = format!("@@SAGUSHD {nonce} ");
    let mut shell_dir = None;
    // Divide so em '\n' e nunca apara: um '\r' no fim de um nome de pasta e
    // parte dele (aparar levaria a outra pasta).
    let mut lines: Vec<&[u8]> = out.split(|&b| b == b'\n').collect();
    // O ultimo pedaco ainda nao terminou em '\n': incompleto.
    lines.pop();
    for line in lines {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if let Some(rest) = line.strip_prefix(&shd_prefix) {
            shell_dir = valid_dir(rest);
        } else if let Some(rest) = line.strip_prefix(&cwd_prefix) {
            let mut f = rest.splitn(6, ' ');
            let (method, w, reason, comm, _host, path) =
                (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
            return Some(Probe {
                method: method.into(),
                reason: reason.into(),
                writable: w == "1",
                fg_comm: if comm == "-" { String::new() } else { comm.into() },
                dir: valid_dir(path),
                shell_dir,
            });
        }
    }
    None
}

/// Pasta aceitavel como destino: absoluta e sem caracteres de controle.
fn valid_dir(s: &str) -> Option<String> {
    (s.starts_with('/') && !s.chars().any(char::is_control)).then(|| s.to_string())
}

/// Abre o subsistema SFTP num canal novo da conexao do terminal.
async fn open_sftp(session: &client::Handle<Client>) -> anyhow::Result<SftpSession> {
    let mut ch = timeout(OPEN_TIMEOUT, session.channel_open_session())
        .await
        .map_err(|_| anyhow::anyhow!("o servidor nao respondeu ao abrir o canal"))??;
    ch.request_subsystem(true, "sftp").await?;
    // Espera a resposta ANTES de `into_stream()`: depois dele o aceite/recusa
    // do subsistema nao seria mais visivel.
    let accepted = timeout(OPEN_TIMEOUT, async {
        loop {
            match ch.wait().await {
                Some(ChannelMsg::Success) => return true,
                Some(ChannelMsg::Failure) | Some(ChannelMsg::Close) | None => return false,
                _ => {}
            }
        }
    })
    .await
    .unwrap_or(false);
    if !accepted {
        let _ = ch.close().await;
        anyhow::bail!("o servidor nao oferece SFTP nesta conexao");
    }
    SftpSession::new(ch.into_stream())
        .await
        .map_err(|e| anyhow::anyhow!("falha ao iniciar o SFTP: {e}"))
}

/// Nomes (dos arquivos soltos) que ja existem em `dir` — como qualquer tipo
/// de entrada, inclusive symlink quebrado.
async fn existing_names(sftp: &SftpSession, dir: &str, files: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for f in files {
        if let Some(name) = remote_name(f) {
            if sftp.symlink_metadata(join_remote(dir, &name)).await.is_ok() {
                out.push(name);
            }
        }
    }
    out
}

/// Nome do arquivo local como nome remoto valido (UTF-8, sem '/', nem
/// "."/"..", sem caracteres de controle).
fn remote_name(local: &Path) -> Option<String> {
    let name = local.file_name()?.to_str()?;
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.chars().any(char::is_control);
    ok.then(|| name.to_string())
}

fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Envia os arquivos para `dir`, um por vez; cada um termina em enviado ou
/// falha, e o lote termina com `Finished`.
async fn upload_files(
    sftp: &SftpSession,
    id: u64,
    dir: &str,
    files: &[PathBuf],
    replace: &[String],
    tx: &UnboundedSender<UploadEvent>,
) {
    let count = files.len();
    let mut sent = Vec::new();
    let mut failed = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    for (index, local) in files.iter().enumerate() {
        let label = local
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = async {
            let name = remote_name(local)
                .ok_or_else(|| anyhow::anyhow!("nome de arquivo nao suportado"))?;
            let size = tokio::fs::metadata(local).await?.len();
            let progress = |bytes: u64| {
                let _ = tx.send(UploadEvent::Progress {
                    id,
                    dir: dir.to_string(),
                    index,
                    count,
                    name: name.clone(),
                    sent: bytes,
                    size,
                });
            };
            progress(0);
            let replace_ok = replace.contains(&name);
            copy_file(sftp, local, dir, &name, replace_ok, &mut buf, progress).await
        }
        .await;
        match result {
            Ok(()) => sent.push(label),
            Err(e) => failed.push((label, format!("{e:#}"))),
        }
    }
    let _ = tx.send(UploadEvent::Finished {
        id,
        dir: dir.to_string(),
        sent,
        failed,
    });
}

/// Envia um arquivo para `dir/name`.
///
/// Sem `replace`: cria com EXCLUDE, que falha se qualquer entrada ja existir
/// (inclusive symlink) sem janela de corrida.
///
/// Com `replace` (autorizado pelo usuario), so para um arquivo comum: o novo
/// conteudo vai para um nome temporario e so depois de completo toma o lugar
/// do antigo. Remover e renomear nunca seguem symlink, e um envio
/// interrompido nao destroi o original. As permissoes do original sao
/// aplicadas antes de gravar o conteudo.
async fn copy_file(
    sftp: &SftpSession,
    local: &Path,
    dir: &str,
    name: &str,
    replace: bool,
    buf: &mut [u8],
    progress: impl Fn(u64),
) -> anyhow::Result<()> {
    let target = join_remote(dir, name);
    if !replace {
        let r = write_new(sftp, local, &target, None, buf, &progress).await;
        if r.is_err() && sftp.symlink_metadata(target).await.is_ok() {
            anyhow::bail!("ja existe no servidor");
        }
        return r;
    }
    let mode = match sftp.symlink_metadata(target.clone()).await {
        Ok(m) if m.file_type().is_file() => m.permissions.map(|p| p & 0o7777),
        Ok(_) => anyhow::bail!("o destino nao e um arquivo comum; nada foi substituido"),
        // Sumiu desde a pergunta: vira um envio comum de arquivo novo.
        Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
            return write_new(sftp, local, &target, None, buf, &progress).await;
        }
        Err(e) => anyhow::bail!("nao foi possivel conferir o destino: {e}"),
    };
    let tmp = join_remote(dir, &format!(".{name}.sagu-{:08x}.part", rand::random::<u32>()));
    if let Err(e) = write_new(sftp, local, &tmp, mode, buf, &progress).await {
        let _ = sftp.remove_file(tmp).await;
        return Err(e);
    }
    if let Err(e) = sftp.remove_file(target.clone()).await {
        let _ = sftp.remove_file(tmp).await;
        anyhow::bail!("nao foi possivel substituir o original: {e}");
    }
    sftp.rename(tmp.clone(), target)
        .await
        .map_err(|e| anyhow::anyhow!("o novo conteudo ficou em {tmp}: {e}"))
}

/// Cria `remote` (EXCLUDE) e grava o arquivo local em blocos. `mode`:
/// permissoes aplicadas antes do conteudo (ao substituir um arquivo).
async fn write_new(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    mode: Option<u32>,
    buf: &mut [u8],
    progress: &impl Fn(u64),
) -> anyhow::Result<()> {
    let mut src = tokio::fs::File::open(local).await?;
    let mut dst = sftp
        .open_with_flags(
            remote.to_string(),
            OpenFlags::CREATE | OpenFlags::WRITE | OpenFlags::EXCLUDE,
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(mode) = mode {
        let mut attrs = FileAttributes::empty();
        attrs.permissions = Some(mode);
        dst.set_metadata(attrs)
            .await
            .map_err(|e| anyhow::anyhow!("nao foi possivel aplicar as permissoes: {e}"))?;
    }
    let mut done: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = fill(&mut src, buf).await?;
        if n == 0 {
            break;
        }
        timeout(STALL_TIMEOUT, dst.write_all(&buf[..n]))
            .await
            .map_err(|_| anyhow::anyhow!("envio parado ha {}s", STALL_TIMEOUT.as_secs()))??;
        done += n as u64;
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            progress(done);
        }
    }
    // Aguarda as escritas pendentes e fecha: sem isso um erro final (ex.:
    // disco cheio) se perderia.
    timeout(STALL_TIMEOUT, dst.shutdown())
        .await
        .map_err(|_| anyhow::anyhow!("tempo esgotado ao finalizar o arquivo"))??;
    progress(done);
    Ok(())
}

/// Le ate encher `buf` (ou EOF), para cada escrita SFTP ter tamanho cheio.
async fn fill(src: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = src.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: &str = "0123456789abcdef";

    fn out(lines: &[&str]) -> Vec<u8> {
        let mut v = Vec::new();
        for l in lines {
            v.extend_from_slice(l.as_bytes());
            v.push(b'\n');
        }
        v
    }

    #[test]
    fn parses_probe_output_with_noise() {
        let o = out(&[
            "Bem-vindo! (motd do .bashrc)",
            "@@SAGUCWD ffffffffffffffff 1 shell 1 ok bash h /errado",
            &format!("@@SAGUSHD {N} /srv/app"),
            &format!("@@SAGUCWD {N} 1 fg 1 ok vim host /srv/app/pasta com espaco"),
        ]);
        let p = parse_probe(&o, N).unwrap();
        assert_eq!(p.method, "fg");
        assert_eq!(p.reason, "ok");
        assert!(p.writable);
        assert_eq!(p.fg_comm, "vim");
        assert_eq!(p.dir.as_deref(), Some("/srv/app/pasta com espaco"));
        assert_eq!(p.shell_dir.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn incomplete_or_foreign_output_is_not_accepted() {
        // Linha ainda sem '\n': espera mais dados.
        let partial = format!("@@SAGUCWD {N} 1 shell 1 ok bash h /tmp");
        assert!(parse_probe(partial.as_bytes(), N).is_none());
        // Nonce de outra execucao.
        assert!(parse_probe(&out(&["@@SAGUCWD 1111111111111111 1 shell 1 ok bash h /tmp"]), N)
            .is_none());
        assert!(parse_probe(b"", N).is_none());
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        // '\r' no fim: aparar levaria a outra pasta; tem de ser rejeitado.
        let p = parse_probe(&out(&[&format!("@@SAGUCWD {N} 1 shell 1 ok bash h /tmp/x\r")]), N)
            .unwrap();
        assert_eq!(p.dir, None);
        let p = parse_probe(&out(&[&format!("@@SAGUCWD {N} 1 home 1 no-shell - h relativo")]), N)
            .unwrap();
        assert_eq!(p.dir, None);
        let p = parse_probe(&out(&[&format!("@@SAGUCWD {N} 1 none 0 no-proc - h ")]), N).unwrap();
        assert_eq!(p.dir, None);
        assert_eq!(p.fg_comm, "");
    }

    fn probe(method: &str, reason: &str, w: bool, comm: &str, dir: &str, shd: &str) -> Probe {
        let opt = |s: &str| (!s.is_empty()).then(|| s.to_string());
        Probe {
            method: method.into(),
            reason: reason.into(),
            writable: w,
            fg_comm: comm.into(),
            dir: opt(dir),
            shell_dir: opt(shd),
        }
    }

    #[test]
    fn confidence_rules() {
        assert!(confident(&probe("shell", "ok", true, "bash", "/a", "/a")));
        // Shell aninhado em outra pasta: e onde o usuario esta.
        assert!(confident(&probe("fg", "ok", true, "zsh", "/b", "/a")));
        // Programa qualquer na mesma pasta do shell.
        assert!(confident(&probe("fg", "ok", true, "vim", "/a", "/a")));
        // Sessao em outro lugar (ssh, sudo...), mesmo na pasta do shell.
        assert!(!confident(&probe("fg", "ok", true, "ssh", "/a", "/a")));
        assert!(!confident(&probe("fg", "ok", true, "sudo", "/a", "/a")));
        // Programa local em outra pasta (servidor com chdir...).
        assert!(!confident(&probe("fg", "ok", true, "python3", "/b", "/a")));
        // Lider da sessao que nao e um shell (login com `exec ssh ...`).
        assert!(!confident(&probe("shell", "ok", true, "ssh", "/a", "/a")));
        // Sem permissao, multiplexador, sudo invisivel, tmux, pasta pessoal.
        assert!(!confident(&probe("shell", "ok", false, "bash", "/a", "/a")));
        assert!(!confident(&probe("shell", "multiplexer", true, "tmux:_client", "/a", "/a")));
        assert!(!confident(&probe("shell", "fg-unreadable", true, "sudo", "/a", "/a")));
        assert!(!confident(&probe("tmux", "ok", true, "tmux:_client", "/b", "/a")));
        assert!(!confident(&probe("home", "no-shell", true, "", "/root", "")));
        assert!(!confident(&Probe::failed("timeout")));
    }

    /// Ponta a ponta contra um sshd real (ex.: OpenSSH no WSL). Rodar com:
    /// SAGU_E2E_PORT=2222 SAGU_E2E_USER=... SAGU_E2E_KEY=<chave privada>
    /// SAGU_E2E_ROOT=\\wsl.localhost\<distro> cargo test e2e -- --ignored
    #[test]
    #[ignore]
    fn e2e_drop_on_real_sshd() {
        use crate::hostkey::HostKeyAnswer;
        use crate::ssh::{self, SshToUi};
        use crate::vault::{AuthMethod, Host};
        use std::time::Instant;

        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"));
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = env("SAGU_E2E_PORT").parse().unwrap();
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        let root = PathBuf::from(env("SAGU_E2E_ROOT"));
        // Caminho UNC do Windows para o arquivo remoto (so '\' como separador).
        let remote = |p: &str| root.join(p.trim_start_matches('/').replace('/', "\\"));

        let h = ssh::connect(host, 120, 30, || {});
        // Espera um evento que satisfaca `f`, ignorando a saida do terminal.
        let wait = |what: &str, f: &mut dyn FnMut(&SshToUi) -> bool| -> SshToUi {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(20) {
                if let Ok(ev) = h.from_ssh.recv_timeout(Duration::from_millis(200)) {
                    // sshd descartavel de teste: confia na chave (TOFU).
                    let ev = match ev {
                        SshToUi::HostKey(p) => {
                            let _ = p.reply.send(HostKeyAnswer::Accept);
                            continue;
                        }
                        ev => ev,
                    };
                    if let SshToUi::Error(e) = &ev {
                        panic!("erro da sessao esperando {what}: {e}");
                    }
                    if f(&ev) {
                        return ev;
                    }
                }
            }
            panic!("tempo esgotado esperando {what}");
        };
        let run = |cmd: &str| {
            h.send_data(format!("{cmd}\n").into_bytes());
            std::thread::sleep(Duration::from_millis(900));
        };
        let upload_ev = |ev: SshToUi| match ev {
            SshToUi::Upload(u) => u,
            _ => unreachable!(),
        };
        let is_final = |ev: &SshToUi| {
            matches!(
                ev,
                SshToUi::Upload(UploadEvent::Plan(_))
                    | SshToUi::Upload(UploadEvent::Finished { .. })
                    | SshToUi::Upload(UploadEvent::Failed { .. })
            )
        };
        wait("conexao", &mut |ev| matches!(ev, SshToUi::Connected));

        let local_dir = std::env::temp_dir().join(format!("sagu-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&local_dir).unwrap();
        let a = local_dir.join("a.txt");
        std::fs::write(&a, "versao 1").unwrap();

        // 1. Shell no prompt depois de `cd`: envia direto para la.
        run("rm -rf /tmp/sagu-e2e-dest /tmp/sagu-e2e-other /tmp/sagu-e2e-tmux; mkdir -p /tmp/sagu-e2e-dest /tmp/sagu-e2e-other /tmp/sagu-e2e-tmux; cd /tmp/sagu-e2e-dest");
        h.drop_files(1, vec![a.clone()]);
        match upload_ev(wait("envio 1", &mut |ev| is_final(ev))) {
            UploadEvent::Finished { dir, sent, failed, .. } => {
                assert_eq!(dir, "/tmp/sagu-e2e-dest");
                assert_eq!(sent, vec!["a.txt".to_string()]);
                assert!(failed.is_empty(), "{failed:?}");
            }
            other => panic!("esperava envio direto: {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(remote("/tmp/sagu-e2e-dest/a.txt")).unwrap(), "versao 1");

        // 2. Mesmo nome de novo: pergunta (conflito) e so substitui com aval.
        std::fs::write(&a, "versao 2").unwrap();
        h.drop_files(2, vec![a.clone()]);
        let plan = match upload_ev(wait("plano de conflito", &mut |ev| is_final(ev))) {
            UploadEvent::Plan(p) => p,
            other => panic!("esperava plano: {other:?}"),
        };
        assert_eq!(plan.conflicts, vec!["a.txt".to_string()]);
        assert_eq!(plan.probe.method, "shell");
        assert_eq!(plan.probe.dir.as_deref(), Some("/tmp/sagu-e2e-dest"));
        assert_eq!(std::fs::read_to_string(remote("/tmp/sagu-e2e-dest/a.txt")).unwrap(), "versao 1");
        h.upload(2, "/tmp/sagu-e2e-dest".into(), vec![a.clone()], vec!["a.txt".into()]);
        match upload_ev(wait("substituicao", &mut |ev| is_final(ev))) {
            UploadEvent::Finished { failed, .. } => assert!(failed.is_empty(), "{failed:?}"),
            other => panic!("esperava substituicao: {other:?}"),
        }
        assert_eq!(std::fs::read_to_string(remote("/tmp/sagu-e2e-dest/a.txt")).unwrap(), "versao 2");

        // 3. Programa em primeiro plano em outra pasta: pergunta.
        let b = local_dir.join("b.txt");
        std::fs::write(&b, "b").unwrap();
        run("(cd /tmp/sagu-e2e-other && exec sleep 60)");
        h.drop_files(3, vec![b.clone()]);
        match upload_ev(wait("plano fg", &mut |ev| is_final(ev))) {
            UploadEvent::Plan(p) => {
                assert_eq!(p.probe.method, "fg");
                assert_eq!(p.probe.fg_comm, "sleep");
                assert_eq!(p.probe.dir.as_deref(), Some("/tmp/sagu-e2e-other"));
                assert_eq!(p.probe.shell_dir.as_deref(), Some("/tmp/sagu-e2e-dest"));
            }
            other => panic!("esperava plano (fg): {other:?}"),
        }
        h.send_data(vec![0x03]); // Ctrl+C no sleep
        std::thread::sleep(Duration::from_millis(500));

        // 4. Shell aninhado apos `cd`: e onde o usuario esta; envia direto.
        run("bash");
        run("cd /tmp/sagu-e2e-other");
        h.drop_files(4, vec![b.clone()]);
        match upload_ev(wait("envio no shell aninhado", &mut |ev| is_final(ev))) {
            UploadEvent::Finished { dir, failed, .. } => {
                assert_eq!(dir, "/tmp/sagu-e2e-other");
                assert!(failed.is_empty(), "{failed:?}");
            }
            other => panic!("esperava envio direto (bash aninhado): {other:?}"),
        }
        run("exit");

        // 4b. Filho de substituicao de processo fica no grupo do shell (com
        // entrada em pipe): nao pode ser tomado pelo programa em uso. Depois
        // de `cd`, o envio vai para a pasta nova, sem perguntar.
        run("exec 2> >(while read -r l; do echo \"$l\"; done)");
        run("cd /tmp/sagu-e2e-other");
        let d = local_dir.join("d.txt");
        std::fs::write(&d, "d").unwrap();
        h.drop_files(41, vec![d]);
        match upload_ev(wait("envio com substituicao de processo", &mut |ev| is_final(ev))) {
            UploadEvent::Finished { dir, failed, .. } => {
                assert_eq!(dir, "/tmp/sagu-e2e-other");
                assert!(failed.is_empty(), "{failed:?}");
            }
            other => panic!("esperava envio direto para a pasta nova: {other:?}"),
        }

        // 4c. Um "ssh" em primeiro plano na mesma pasta do shell: pergunta
        // (o usuario esta em outra maquina, invisivel daqui).
        run("cp /bin/sleep /tmp/sagu-e2e-other/ssh && /tmp/sagu-e2e-other/ssh 60");
        let e = local_dir.join("e.txt");
        std::fs::write(&e, "e").unwrap();
        h.drop_files(42, vec![e]);
        match upload_ev(wait("plano com ssh", &mut |ev| is_final(ev))) {
            UploadEvent::Plan(p) => {
                assert_eq!(p.probe.method, "fg", "{p:?}");
                assert_eq!(p.probe.fg_comm, "ssh");
                assert_eq!(p.probe.dir, p.probe.shell_dir);
            }
            other => panic!("esperava pergunta (ssh em uso): {other:?}"),
        }
        h.send_data(vec![0x03]);
        std::thread::sleep(Duration::from_millis(500));
        run("cd /tmp/sagu-e2e-dest");

        // 5. Dentro do tmux: pergunta, sugerindo a pasta do painel ativo.
        run("tmux -L sagu-e2e new-session");
        run("cd /tmp/sagu-e2e-tmux");
        let c = local_dir.join("c.txt");
        std::fs::write(&c, "c").unwrap();
        h.drop_files(5, vec![c]);
        match upload_ev(wait("plano tmux", &mut |ev| is_final(ev))) {
            UploadEvent::Plan(p) => {
                assert_eq!(p.probe.method, "tmux", "{p:?}");
                assert_eq!(p.probe.dir.as_deref(), Some("/tmp/sagu-e2e-tmux"));
            }
            other => panic!("esperava plano (tmux): {other:?}"),
        }
        run("tmux -L sagu-e2e kill-server");

        // Encerramento normal.
        run("exit");
        wait("encerramento", &mut |ev| matches!(ev, SshToUi::Closed));
        let _ = std::fs::remove_dir_all(&local_dir);
    }

    /// Ponta a ponta: `.bashrc` que abre o tmux no login (sem `exec`). O
    /// shell de login fica parado e o tmux em primeiro plano: deve perguntar,
    /// sugerindo a pasta do painel ativo do tmux. Mesmas variaveis do teste
    /// acima, com SAGU_E2E_PORT apontando para um sshd sem `SetEnv TMUX`.
    #[test]
    #[ignore]
    fn e2e_login_tmux_asks() {
        use crate::hostkey::HostKeyAnswer;
        use crate::ssh::{self, SshToUi};
        use crate::vault::{AuthMethod, Host};
        use std::time::Instant;

        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"));
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = env("SAGU_E2E_PORT").parse().unwrap();
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        let h = ssh::connect(host, 120, 30, || {});
        let t0 = Instant::now();
        let mut plan = None;
        let mut connected = false;
        let mut dropped = false;
        while t0.elapsed() < Duration::from_secs(25) && plan.is_none() {
            if let Ok(ev) = h.from_ssh.recv_timeout(Duration::from_millis(200)) {
                match ev {
                    // sshd descartavel de teste: confia na chave (TOFU).
                    SshToUi::HostKey(p) => {
                        let _ = p.reply.send(HostKeyAnswer::Accept);
                    }
                    SshToUi::Connected => connected = true,
                    SshToUi::Upload(UploadEvent::Plan(p)) => plan = Some(p),
                    SshToUi::Upload(other) => panic!("esperava pergunta: {other:?}"),
                    SshToUi::Error(e) => panic!("erro da sessao: {e}"),
                    _ => {}
                }
            }
            // Da tempo ao .bashrc abrir o tmux; entao entra numa pasta e solta.
            if connected && !dropped && t0.elapsed() > Duration::from_secs(3) {
                h.send_data(b"cd /tmp/sagu-e2e-tmux\n".to_vec());
                std::thread::sleep(Duration::from_millis(900));
                let f = std::env::temp_dir().join(format!("sagu-e2e-login-{}.txt", std::process::id()));
                std::fs::write(&f, "x").unwrap();
                h.drop_files(9, vec![f]);
                dropped = true;
            }
        }
        let p = plan.expect("sem resposta da sessao");
        assert_eq!(p.probe.method, "tmux", "{p:?}");
        assert_eq!(p.probe.dir.as_deref(), Some("/tmp/sagu-e2e-tmux"));
        assert!(!confident(&p.probe));
        // Fecha a sessao tmux criada pelo teste e sai.
        h.send_data(b"tmux kill-session\n".to_vec());
        std::thread::sleep(Duration::from_millis(1500));
        h.send_data(b"exit\n".to_vec());
    }

    #[test]
    fn remote_names() {
        assert_eq!(remote_name(Path::new(r"C:\x\relatorio final.pdf")).as_deref(), Some("relatorio final.pdf"));
        assert_eq!(remote_name(Path::new("C:\\x\\a\u{7}b.txt")), None);
        assert_eq!(join_remote("/", "a"), "/a");
        assert_eq!(join_remote("/srv/app", "a"), "/srv/app/a");
    }
}
