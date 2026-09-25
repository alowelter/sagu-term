//! Atualizacao automatica a partir das Releases do GitHub.
//!
//! Na abertura, uma thread consulta a ultima Release publicada do repositorio
//! e compara a versao com a deste executavel. Havendo versao nova, a UI oferece
//! a atualizacao; ao confirmar, outra thread:
//! 1. baixa o `.sha256` publicado junto com o executavel;
//! 2. baixa o executavel novo ao lado do atual (`*.exe.new`), calculando o hash;
//! 3. confere o hash e o tamanho (qualquer divergencia descarta o download);
//! 4. troca os arquivos: o atual vira `*.exe.old` e o novo assume o nome dele.
//!
//! No Windows um `.exe` em execucao pode ser renomeado, mas nao sobrescrito;
//! por isso a troca e feita por renomeacao. A UI entao inicia o executavel novo
//! e fecha este; o `*.exe.old` e apagado na proxima abertura (`cleanup_old`).
//!
//! As Releases sao geradas pelo workflow `.github/workflows/release.yml`, que
//! anexa `SaguTerm.exe` e `SaguTerm.exe.sha256` a cada tag `v*` (e, na
//! transicao, uma copia com o nome antigo `sagu-term.exe`).

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Repositorio (dono/nome) cujas Releases publicam o executavel.
pub const REPO: &str = "alowelter/sagu-term";
/// Executavel anexado a cada Release e o arquivo com o SHA-256 dele, em
/// ordem de preferencia: o nome atual e o antigo (ate a v0.1.4), para ainda
/// aceitar Releases publicadas so com ele.
const ASSETS: &[(&str, &str)] = &[
    ("SaguTerm.exe", "SaguTerm.exe.sha256"),
    ("sagu-term.exe", "sagu-term.exe.sha256"),
];
/// Versao deste executavel (do Cargo.toml).
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// Uma Release mais nova que a versao em execucao.
#[derive(Clone, Debug)]
pub struct Release {
    /// Versao sem o prefixo `v` (ex.: "0.1.3").
    pub version: String,
    /// Notas da Release (texto livre/markdown), exibidas antes de atualizar.
    pub notes: String,
    /// Pagina da Release no GitHub (para baixar manualmente).
    pub page_url: String,
    exe_url: String,
    exe_size: u64,
    sha_url: String,
}

/// Estado da verificacao/instalacao, lido pela UI a cada quadro.
#[derive(Clone, Debug)]
pub enum Status {
    Checking,
    UpToDate,
    /// Consulta falhou (sem internet, limite da API...): nao incomoda o usuario.
    CheckFailed(String),
    Available(Release),
    Downloading {
        release: Release,
        done: u64,
        total: u64,
    },
    /// Executavel novo ja no lugar do antigo; falta reiniciar a partir de `exe`.
    Installed { exe: PathBuf },
    Failed { release: Release, error: String },
}

/// Verificacao e instalacao em segundo plano; a UI so consulta `status()`.
pub struct Updater {
    status: Arc<Mutex<Status>>,
}

impl Updater {
    /// Inicia a consulta a ultima Release em segundo plano. `repaint` acorda a
    /// UI quando o resultado chega.
    pub fn check<F>(repaint: F) -> Self
    where
        F: Fn() + Send + 'static,
    {
        let status = Arc::new(Mutex::new(Status::Checking));
        let shared = status.clone();
        std::thread::spawn(move || {
            let result = match fetch_latest() {
                Ok(Some(release)) => Status::Available(release),
                Ok(None) => Status::UpToDate,
                Err(e) => Status::CheckFailed(format!("{e:#}")),
            };
            set(&shared, result);
            repaint();
        });
        Updater { status }
    }

    /// Updater inerte (sem consulta a rede), para testes.
    #[cfg(test)]
    pub fn idle() -> Self {
        Updater {
            status: Arc::new(Mutex::new(Status::UpToDate)),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Baixa, confere e instala a Release em segundo plano.
    pub fn install<F>(&self, release: Release, repaint: F)
    where
        F: Fn() + Send + 'static,
    {
        set(
            &self.status,
            Status::Downloading {
                release: release.clone(),
                done: 0,
                total: release.exe_size,
            },
        );
        let shared = self.status.clone();
        std::thread::spawn(move || {
            let progress = |done: u64| {
                set(
                    &shared,
                    Status::Downloading {
                        release: release.clone(),
                        done,
                        total: release.exe_size,
                    },
                );
                repaint();
            };
            let result = match download_and_replace(&release, progress) {
                Ok(exe) => Status::Installed { exe },
                Err(e) => Status::Failed {
                    release: release.clone(),
                    error: format!("{e:#}"),
                },
            };
            set(&shared, result);
            repaint();
        });
    }
}

fn set(status: &Mutex<Status>, value: Status) {
    *status.lock().unwrap_or_else(|e| e.into_inner()) = value;
}

/// Cliente HTTP com tempos limite folgados o bastante para o download do
/// executavel (~12 MB) numa conexao lenta.
fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .user_agent(format!("SaguTerm/{CURRENT}"))
            .timeout_connect(Some(Duration::from_secs(15)))
            .timeout_recv_response(Some(Duration::from_secs(30)))
            .timeout_recv_body(Some(Duration::from_secs(600)))
            .build(),
    )
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// Consulta a ultima Release (o GitHub ja exclui rascunhos e pre-releases).
/// `None` quando nao ha versao mais nova que a atual.
fn fetch_latest() -> anyhow::Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut resp = match agent()
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        // Repositorio sem nenhuma Release publicada ainda.
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let api: ApiRelease = serde_json::from_str(&resp.body_mut().read_to_string()?)?;
    parse_release(api)
}

fn parse_release(api: ApiRelease) -> anyhow::Result<Option<Release>> {
    let version = api.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, CURRENT) {
        return Ok(None);
    }
    let asset = |name: &str| api.assets.iter().find(|a| a.name == name);
    let Some((exe, sha)) = ASSETS
        .iter()
        .find_map(|(exe, sha)| Some((asset(exe)?, asset(sha)?)))
    else {
        anyhow::bail!("a Release {} nao traz o executavel e o .sha256", api.tag_name);
    };
    Ok(Some(Release {
        version,
        notes: api.body.unwrap_or_default().trim().to_string(),
        page_url: api.html_url,
        exe_url: exe.browser_download_url.clone(),
        exe_size: exe.size,
        sha_url: sha.browser_download_url.clone(),
    }))
}

/// Versao `x.y.z` (com `v` opcional). Versoes com sufixo (`-rc1`...) ou fora
/// do formato nao sao comparaveis e nunca contam como mais novas.
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim().trim_start_matches('v').split('.');
    let v = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(v)
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// Extrai o hash do conteudo de um `.sha256`, em minusculas: o primeiro
/// trecho de 64 digitos hexadecimais. Aceita o formato `sha256sum`
/// (`<hash>  <arquivo>`) e o do PowerShell (`SHA256 hash of x:` + `<hash>`).
fn parse_sha256_file(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == ':' || c == '\u{feff}')
        .find(|t| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|t| t.to_ascii_lowercase())
}

/// `SaguTerm.exe` -> `SaguTerm.exe.<suffix>` (no mesmo diretorio).
fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    exe.with_file_name(name)
}

/// Baixa e instala a Release; devolve o caminho do executavel (ja o novo).
fn download_and_replace(release: &Release, progress: impl Fn(u64)) -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let new_path = sibling(&exe, "new");
    download_verified(release, &new_path, progress).map_err(|e| {
        let _ = std::fs::remove_file(&new_path);
        e
    })?;
    replace_exe(&exe, &new_path, &sibling(&exe, "old"))?;
    Ok(exe)
}

/// Baixa o executavel da Release em `dest`, conferindo tamanho e SHA-256.
fn download_verified(release: &Release, dest: &Path, progress: impl Fn(u64)) -> anyhow::Result<()> {
    let agent = agent();

    let expected = {
        let mut resp = agent.get(&release.sha_url).call()?;
        let text = resp.body_mut().read_to_string()?;
        parse_sha256_file(&text)
            .ok_or_else(|| anyhow::anyhow!("o .sha256 publicado na Release e invalido"))?
    };

    // Download calculando o hash durante a gravacao. O limite de leitura
    // impede um servidor de enviar mais que o anunciado (o ureq recusa corpo
    // do tamanho exato do limite, dai o +1; o tamanho exato e conferido
    // depois).
    let resp = agent.get(&release.exe_url).call()?;
    let mut reader = resp
        .into_body()
        .into_with_config()
        .limit(release.exe_size + 1)
        .reader();
    let mut file = File::create(dest).map_err(|e| {
        anyhow::anyhow!(
            "sem permissao para gravar em {} ({e}); baixe a nova versao manualmente",
            dest.parent().map(|p| p.display().to_string()).unwrap_or_default()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    let written: anyhow::Result<()> = (|| {
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            done += n as u64;
            progress(done);
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    written?;
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if done != release.exe_size {
        anyhow::bail!("download incompleto ({done} de {} bytes)", release.exe_size);
    }
    if got != expected {
        anyhow::bail!("o arquivo baixado nao confere com o SHA-256 publicado");
    }
    Ok(())
}

/// Troca o executavel: o atual (em execucao) vira `old`; `new` assume o nome
/// dele. Se a 2ª etapa falhar, desfaz a 1ª para nao deixar o app sem
/// executavel.
fn replace_exe(exe: &Path, new: &Path, old: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(old);
    std::fs::rename(exe, old)?;
    if let Err(e) = std::fs::rename(new, exe) {
        let _ = std::fs::rename(old, exe);
        let _ = std::fs::remove_file(new);
        return Err(e.into());
    }
    Ok(())
}

/// Apaga o `*.exe.old` deixado por uma atualizacao. O processo antigo pode
/// ainda estar terminando (arquivo em uso), entao tenta por alguns segundos
/// numa thread, sem atrasar a abertura.
pub fn cleanup_old() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let old = sibling(&exe, "old");
    if !old.exists() {
        return;
    }
    std::thread::spawn(move || {
        for _ in 0..40 {
            if std::fs::remove_file(&old).is_ok() || !old.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Inicia o executavel (ja atualizado) num processo novo.
pub fn restart(exe: &Path) -> std::io::Result<()> {
    std::process::Command::new(exe).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.1.3", "0.1.2"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        // Pre-releases e formatos estranhos nunca disparam atualizacao.
        assert!(!is_newer("0.2.0-rc1", "0.1.2"));
        assert!(!is_newer("0.2", "0.1.2"));
        assert!(!is_newer("0.2.0.1", "0.1.2"));
        assert!(!is_newer("", "0.1.2"));
    }

    #[test]
    fn sha256_file_parsing() {
        let h = "A".repeat(64);
        assert_eq!(parse_sha256_file(&format!("{h}  sagu-term.exe")), Some("a".repeat(64)));
        assert_eq!(parse_sha256_file(&format!("\u{feff}{h}\r\n")), Some("a".repeat(64)));
        let ps = format!("SHA256 hash of x.zip:\r\n{h}\r\n");
        assert_eq!(parse_sha256_file(&ps), Some("a".repeat(64)));
        assert_eq!(parse_sha256_file("abc  sagu-term.exe"), None);
        assert_eq!(parse_sha256_file(&"g".repeat(64)), None);
        assert_eq!(parse_sha256_file(""), None);
    }

    #[test]
    fn sibling_paths() {
        let exe = Path::new(r"C:\apps\SaguTerm.exe");
        assert_eq!(sibling(exe, "old"), Path::new(r"C:\apps\SaguTerm.exe.old"));
        assert_eq!(sibling(exe, "new"), Path::new(r"C:\apps\SaguTerm.exe.new"));
    }

    fn api(tag: &str, assets: &[&str]) -> ApiRelease {
        ApiRelease {
            tag_name: tag.into(),
            body: Some("  notas  ".into()),
            html_url: "https://github.com/x/y/releases/tag/v9.9.9".into(),
            assets: assets
                .iter()
                .map(|n| ApiAsset {
                    name: (*n).into(),
                    browser_download_url: format!("https://example/{n}"),
                    size: 10,
                })
                .collect(),
        }
    }

    #[test]
    fn replace_exe_swaps_files() {
        let dir = std::env::temp_dir().join(format!("sagu-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app.exe");
        let (new, old) = (sibling(&exe, "new"), sibling(&exe, "old"));
        std::fs::write(&exe, "v1").unwrap();
        std::fs::write(&new, "v2").unwrap();
        std::fs::write(&old, "v0").unwrap(); // sobra de uma atualizacao anterior
        replace_exe(&exe, &new, &old).unwrap();
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "v1");
        assert!(!new.exists());
        // Sem o arquivo novo: falha e o executavel original volta ao lugar.
        assert!(replace_exe(&exe, &new, &old).is_err());
        assert_eq!(std::fs::read_to_string(&exe).unwrap(), "v2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Download real (rede): uma Release publica que tambem publica `.sha256`
    /// no formato `sha256sum`. Rodar com `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn download_verified_real_release() {
        let api: ApiRelease = serde_json::from_str(
            &agent()
                .get("https://api.github.com/repos/BurntSushi/ripgrep/releases/latest")
                .call()
                .unwrap()
                .body_mut()
                .read_to_string()
                .unwrap(),
        )
        .unwrap();
        let exe = api
            .assets
            .iter()
            .find(|a| a.name.ends_with("x86_64-pc-windows-msvc.zip"))
            .unwrap();
        let sha = api
            .assets
            .iter()
            .find(|a| a.name == format!("{}.sha256", exe.name))
            .unwrap();
        let mut release = Release {
            version: api.tag_name.clone(),
            notes: String::new(),
            page_url: api.html_url.clone(),
            exe_url: exe.browser_download_url.clone(),
            exe_size: exe.size,
            sha_url: sha.browser_download_url.clone(),
        };
        let dest = std::env::temp_dir().join(format!("sagu-dl-{}", std::process::id()));
        let last = std::cell::Cell::new(0);
        download_verified(&release, &dest, |d| last.set(d)).unwrap();
        assert_eq!(last.get(), exe.size);
        // Hash de outro arquivo: o download e rejeitado.
        release.sha_url = api
            .assets
            .iter()
            .find(|a| a.name.ends_with(".sha256") && a.name != sha.name)
            .unwrap()
            .browser_download_url
            .clone();
        let err = download_verified(&release, &dest, |_| {}).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn release_parsing() {
        let all = [
            "SaguTerm.exe",
            "SaguTerm.exe.sha256",
            "sagu-term.exe",
            "sagu-term.exe.sha256",
        ];
        let r = parse_release(api("v9.9.9", &all)).unwrap().unwrap();
        assert_eq!(r.version, "9.9.9");
        assert_eq!(r.notes, "notas");
        // Com os dois nomes publicados, prefere o atual.
        assert_eq!(r.exe_url, "https://example/SaguTerm.exe");
        assert_eq!(r.sha_url, "https://example/SaguTerm.exe.sha256");
        assert_eq!(r.exe_size, 10);
        // Release so com o nome antigo ainda e aceita.
        let r = parse_release(api("v9.9.9", &all[2..])).unwrap().unwrap();
        assert_eq!(r.exe_url, "https://example/sagu-term.exe");
        // Mesma versao: nada a fazer.
        assert!(parse_release(api(&format!("v{CURRENT}"), &all)).unwrap().is_none());
        // Versao nova sem o .sha256: erro (nunca instala sem conferir).
        assert!(parse_release(api("v9.9.9", &["SaguTerm.exe"])).is_err());
        // Nunca mistura o executavel de um nome com o hash do outro.
        assert!(parse_release(api("v9.9.9", &["SaguTerm.exe", "sagu-term.exe.sha256"])).is_err());
    }
}
