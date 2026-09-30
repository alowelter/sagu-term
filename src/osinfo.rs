//! Sistema operacional do servidor, detectado em segundo plano para o icone e
//! a dica do cartao da conexao.
//!
//! - Canal proprio na MESMA conexao ja autenticada (sessao de terminal SSH ou
//!   SFTP), numa tarefa propria da sessao: nada passa pelo terminal nem pelo
//!   SFTP, e uma sonda lenta nunca atrasa o shell (ver `ssh::run_session`).
//! - Comando constante e agnostico de shell ([`PROBE_COMMAND`]), por `exec`,
//!   com o stdin fechado logo em seguida; o stderr e ignorado.
//! - Prazos ([`START_DELAY`], [`OPEN_TIMEOUT`], [`PROBE_TIMEOUT`]) e limite de
//!   bytes lidos ([`MAX_OUTPUT`]): passou disso, fica sem resultado.
//! - Tudo o que vem do servidor e hostil: limite de tamanho por campo, id so
//!   com [a-z0-9._-], e caracteres de controle, bidi ou invisiveis descartam o
//!   campo inteiro (`clean`).
//! - Qualquer falha (exec recusado, ForceCommand, equipamento de rede,
//!   Windows, prazo esgotado) e silenciosa: o cartao mantem o icone atual.
//! - Desligavel por conexao (`Host::detect_os`): num servidor que forca um
//!   comando (ForceCommand, command= no authorized_keys), e ele que roda no
//!   lugar deste, uma vez a mais por conexao.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use russh::{client, ChannelMsg};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

use crate::ssh::Client;
use crate::vault::Host;

/// Comando constante e agnostico de shell (so `;`; sem redirecionamento,
/// aspas, `$`, glob nem `#`): sh/bash/zsh/dash/ksh/fish/csh/tcsh. Cada parte
/// vem depois de um marcador; o que falta (ex.: `sw_vers` fora do macOS) so
/// deixa a secao vazia, e a mensagem de erro vai para o stderr.
pub const PROBE_COMMAND: &str = "echo SAGUOS.uname; uname -s -r; \
echo SAGUOS.etc; cat /etc/os-release; echo SAGUOS.lib; cat /usr/lib/os-release; \
echo SAGUOS.rh; cat /etc/redhat-release; echo SAGUOS.sw; sw_vers; echo SAGUOS.end";

/// Espera antes da sonda: o login interativo (motd, tmux aberto pelo
/// .bashrc) vai na frente, e sessoes curtas nem chegam a rodar o comando.
const START_DELAY: Duration = Duration::from_secs(2);
/// Prazo para o servidor abrir o canal.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Prazo para o exec e a leitura da saida.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Saida maior que isto: sem resultado (nada de acumular sem limite).
const MAX_OUTPUT: usize = 64 * 1024;

/// Comeco do software na identificacao do servidor (minusculas) em que a
/// sonda nao roda: Windows e equipamentos de rede.
const SKIP_SOFTWARE: [&str; 5] = [
    "openssh_for_windows",
    "cisco",
    "rosssh",
    "huawei",
    "comware",
];

const MARK: &str = "SAGUOS.";
const MAX_ID: usize = 32;
const MAX_NAME: usize = 64;
const MAX_VERSION: usize = 32;

/// Sistema detectado no servidor, guardado no Host do cofre (so o necessario
/// para o icone e a dica). Valores saneados (ver `clean`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsInfo {
    /// ID do os-release (ou derivado do uname/sw_vers): minusculas, so [a-z0-9._-].
    pub id: String,
    /// NAME do os-release (ou equivalente).
    pub name: String,
    /// VERSION_ID (ou equivalente); distribuicoes continuas (Arch) nao tem.
    #[serde(default)]
    pub version: Option<String>,
}

impl OsInfo {
    /// Nome para mostrar, ex. "AlmaLinux 8.10" (sem versao: so o nome).
    pub fn label(&self) -> String {
        match &self.version {
            Some(v) => format!("{} {v}", self.name),
            None => self.name.clone(),
        }
    }
}

/// Icone (slug do Simple Icons em assets/os) do sistema, so pelo ID e seus
/// apelidos da mesma marca. Sem cair no ID_LIKE: derivado sem icone proprio
/// (Oracle, Amazon, Kali...) mantem o icone atual, como o usuario pediu.
pub fn icon_slug(os: &OsInfo) -> Option<&'static str> {
    let id = os.id.as_str();
    Some(match id {
        "ubuntu" | "ubuntu-core" => "ubuntu",
        "debian" => "debian",
        "raspbian" => "raspberrypi",
        "rhel" | "rhcos" => "redhat",
        "centos" => "centos",
        "fedora" | "fedora-asahi-remix" => "fedora",
        "almalinux" => "almalinux",
        "rocky" => "rockylinux",
        "arch" | "archarm" => "archlinux",
        "alpine" => "alpinelinux",
        "opensuse" => "opensuse",
        _ if id.starts_with("opensuse-") => "opensuse",
        "suse" | "sles" | "sles_sap" | "sled" | "sle-micro" | "sl-micro" => "suse",
        "linuxmint" => "linuxmint",
        "manjaro" => "manjaro",
        _ if id.starts_with("manjaro-") => "manjaro",
        "gentoo" => "gentoo",
        "nixos" => "nixos",
        "pop" => "popos",
        "elementary" => "elementary",
        "zorin" => "zorin",
        "endeavouros" => "endeavouros",
        "void" => "voidlinux",
        "slackware" => "slackware",
        "freebsd" => "freebsd",
        "macos" => "apple",
        "devuan" => "devuan",
        "openwrt" => "openwrt",
        _ => return None,
    })
}

/// Todos os slugs que `icon_slug` pode devolver (conferidos contra OS_ICONS).
#[cfg(test)]
pub const ICON_SLUGS: &[&str] = &[
    "almalinux",
    "alpinelinux",
    "apple",
    "archlinux",
    "centos",
    "debian",
    "devuan",
    "elementary",
    "endeavouros",
    "fedora",
    "freebsd",
    "gentoo",
    "linuxmint",
    "manjaro",
    "nixos",
    "opensuse",
    "openwrt",
    "popos",
    "raspberrypi",
    "redhat",
    "rockylinux",
    "slackware",
    "suse",
    "ubuntu",
    "voidlinux",
    "zorin",
];

/// O que a identificacao do servidor ("SSH-2.0-...") diz sobre a sonda.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Banner {
    Probe,
    /// Windows ou equipamento de rede: o comando nao e executado.
    Skip,
}

/// Resultado da sonda para a UI, com o endereco da conexao (a UI so grava se
/// o host do cofre ainda tiver o mesmo endereco e porta).
#[derive(Debug)]
pub struct OsReport {
    pub host_id: uuid::Uuid,
    pub host: String,
    pub port: u16,
    /// `None` = a sonda rodou e nada foi identificado.
    pub os: Option<OsInfo>,
}

// ------------------------------------------------------------------ sonda

/// Decide pela identificacao do servidor se o comando deve rodar. Windows
/// (OpenSSH for Windows, Bitvise WinSSHD) e equipamentos de rede (Cisco,
/// MikroTik, Huawei, H3C) ficam de fora; o resto (inclusive lixo) roda.
pub fn banner(server_id: &[u8]) -> Banner {
    let id = String::from_utf8_lossy(server_id);
    let Some(rest) = id
        .strip_prefix("SSH-2.0-")
        .or_else(|| id.strip_prefix("SSH-1.99-"))
    else {
        return Banner::Probe;
    };
    let low = rest.to_ascii_lowercase();
    let soft = low.split(' ').next().unwrap_or_default();
    let skip = SKIP_SOFTWARE.iter().any(|p| soft.starts_with(p)) || low.contains("winsshd");
    if skip {
        Banner::Skip
    } else {
        Banner::Probe
    }
}

/// Sonda do SO de uma sessao (terminal SSH ou SFTP) numa tarefa propria, com o
/// relatorio para a UI (`report`). Cuida tambem do fim da sessao (drop, por
/// qualquer caminho, inclusive erro): o resultado que ja chegou e entregue; a
/// sonda que ainda roda e abortada e, se o comando ja tinha sido pedido ao
/// servidor, avisa "sem resultado". A sessao pode ter caido justamente pelo
/// canal extra (equipamento que nao aceita um segundo canal): assim a UI conta
/// a tentativa e nao repete a sonda a cada conexao nesta abertura do cofre. A
/// que nem chegou a pedir (sessao mais curta que `START_DELAY`) nao avisa
/// nada, e a proxima conexao tenta de novo.
pub(crate) struct OsProbe<R: Fn(OsReport)> {
    task: JoinSet<Option<OsInfo>>,
    /// O comando ja foi pedido ao servidor (ver `detect`).
    started: Arc<AtomicBool>,
    host_id: uuid::Uuid,
    host: String,
    port: u16,
    report: R,
}

impl<R: Fn(OsReport)> OsProbe<R> {
    /// Sonda parada da conexao com `host` (sem `spawn`, nunca avisa nada).
    pub(crate) fn new(host: &Host, report: R) -> Self {
        OsProbe {
            task: JoinSet::new(),
            started: Arc::default(),
            host_id: host.id,
            host: host.host.clone(),
            port: host.port,
            report,
        }
    }

    /// Comeca a sonda na sessao ja autenticada.
    pub(crate) fn spawn(&mut self, session: &Arc<client::Handle<Client>>, banner: Banner) {
        let started = Arc::clone(&self.started);
        self.task
            .spawn(detect(Arc::clone(session), banner, started));
    }

    /// A sonda ainda nao entregou o resultado (guarda do ramo do `select!`).
    pub(crate) fn running(&self) -> bool {
        !self.task.is_empty()
    }

    /// Espera a sonda terminar e entrega o resultado pelo `report`. Pode ser
    /// cancelado no `select!` sem perder o resultado (fica no JoinSet).
    pub(crate) async fn wait(&mut self) {
        if let Some(Ok(os)) = self.task.join_next().await {
            self.send(os);
        }
    }

    fn send(&self, os: Option<OsInfo>) {
        (self.report)(OsReport {
            host_id: self.host_id,
            host: self.host.clone(),
            port: self.port,
            os,
        });
    }
}

impl<R: Fn(OsReport)> Drop for OsProbe<R> {
    fn drop(&mut self) {
        let os = match self.task.try_join_next() {
            // Terminou, mas o laco da sessao ainda nao tinha lido.
            Some(Ok(os)) => os,
            Some(Err(_)) => return,
            // Pediu o comando e nao respondeu: tentativa sem resultado.
            None if self.running() && self.started.load(Ordering::SeqCst) => None,
            None => return,
        };
        self.send(os);
        // O drop do JoinSet, logo em seguida, aborta a que ainda roda.
    }
}

/// Sonda completa, para rodar numa tarefa propria (ver `OsProbe`). `None` =
/// nada identificado ou qualquer falha. `started` fica verdadeiro quando o
/// comando vai de fato ser pedido ao servidor.
async fn detect(
    session: Arc<client::Handle<Client>>,
    banner: Banner,
    started: Arc<AtomicBool>,
) -> Option<OsInfo> {
    sleep(START_DELAY).await;
    if banner == Banner::Skip {
        return None;
    }
    // Daqui em diante o servidor ve o canal extra.
    started.store(true, Ordering::SeqCst);
    parse_output(&run_probe(&session).await?)
}

/// Roda `PROBE_COMMAND` num canal proprio e devolve o stdout (ate o marcador
/// final, o fim do canal ou `MAX_OUTPUT`).
async fn run_probe(session: &client::Handle<Client>) -> Option<Vec<u8>> {
    let channel = timeout(OPEN_TIMEOUT, session.channel_open_session())
        .await
        .ok()?
        .ok()?;
    let (mut rd, wr) = channel.split();
    let out = timeout(PROBE_TIMEOUT, async {
        wr.exec(true, PROBE_COMMAND).await.ok()?;
        // Stdin fechado: um programa que espere entrada (ForceCommand,
        // internal-sftp, menus) recebe EOF e sai.
        wr.eof().await.ok()?;
        let mut out = Vec::new();
        let mut line_start = 0;
        loop {
            match rd.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if out.len() + data.len() > MAX_OUTPUT {
                        return None;
                    }
                    let from = out.len();
                    out.extend_from_slice(&data);
                    if end_marker_since(&out, from, &mut line_start) {
                        return Some(out);
                    }
                }
                // Exec recusado pelo servidor.
                Some(ChannelMsg::Failure) => return None,
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => return Some(out),
                // stderr (ExtendedData), exit-status, confirmacoes...
                Some(_) => {}
            }
        }
    })
    .await
    .ok()
    .flatten();
    // A metade de leitura sai ANTES do close: um receptor vivo e nao drenado
    // (servidor que continua mandando dados) trava a conexao inteira, shell
    // junto; solto, o que ainda chegar ao canal e descartado.
    drop(rd);
    let _ = wr.close().await;
    out
}

// ------------------------------------------------------------- saneamento

/// Caracteres invisiveis ou que mexem na direcao/quebra do texto.
fn invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{061C}' | '\u{180E}'
        | '\u{200B}'..='\u{200F}'
        | '\u{2028}'..='\u{202E}'
        | '\u{2060}'..='\u{206F}'
        | '\u{FEFF}'
        | '\u{FFF9}'..='\u{FFFB}'
        | '\u{E0000}'..='\u{E007F}')
}

/// Texto do servidor para mostrar: controle (menos tab), bidi e invisiveis
/// descartam o campo; espacos colapsados; no maximo `max` caracteres.
fn clean(s: &str, max: usize) -> Option<String> {
    let mut out = String::new();
    let (mut n, mut gap) = (0usize, false);
    for c in s.chars() {
        if (c.is_control() && c != '\t') || invisible(c) {
            return None;
        }
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if n >= max {
            break;
        }
        if gap && n > 0 {
            out.push(' ');
            n += 1;
            if n >= max {
                break;
            }
        }
        gap = false;
        out.push(c);
        n += 1;
    }
    let out = out.trim_end().to_string();
    (!out.is_empty()).then_some(out)
}

/// Id do sistema: minusculas, so [a-z0-9._-], ate `MAX_ID`; senao `None`.
fn clean_id(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let ok = !s.is_empty()
        && s.len() <= MAX_ID
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
    ok.then_some(s)
}

// ------------------------------------------------------------------ secoes

/// Linhas de cada parte da saida, pelos marcadores.
#[derive(Default)]
struct Sections {
    uname: Vec<String>,
    etc: Vec<String>,
    lib: Vec<String>,
    rh: Vec<String>,
    sw: Vec<String>,
}

impl Sections {
    fn get(&mut self, name: &str) -> Option<&mut Vec<String>> {
        Some(match name {
            "uname" => &mut self.uname,
            "etc" => &mut self.etc,
            "lib" => &mut self.lib,
            "rh" => &mut self.rh,
            "sw" => &mut self.sw,
            _ => return None,
        })
    }
}

/// Nome da secao se `tail` for exatamente um marcador ("SAGUOS.etc" -> "etc").
fn marker(tail: &str) -> Option<&'static str> {
    let name = tail.strip_prefix(MARK)?;
    ["uname", "etc", "lib", "rh", "sw", "end"]
        .into_iter()
        .find(|n| *n == name)
}

/// Separa a saida pelos marcadores. O que vem antes do primeiro (ruido do
/// .bashrc) ou depois do final e ignorado; o marcador pode vir grudado na
/// ultima linha de um arquivo sem '\n' final; CRLF e aceito.
fn split_sections(out: &[u8]) -> Sections {
    let mut s = Sections::default();
    let mut cur: Option<&'static str> = None;
    for raw in out.split(|&b| b == b'\n') {
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let line = String::from_utf8_lossy(raw);
        let (text, next) = match line.rfind(MARK).map(|i| (i, marker(&line[i..]))) {
            Some((i, Some(name))) => (&line[..i], Some(name)),
            _ => (&line[..], None),
        };
        if !text.is_empty() {
            if let Some(dst) = cur.and_then(|c| s.get(c)) {
                dst.push(text.to_string());
            }
        }
        if let Some(name) = next {
            // Repetir o marcador recomeca a secao (ruido nao se soma).
            if let Some(dst) = s.get(name) {
                dst.clear();
            }
            cur = Some(name);
        }
    }
    s
}

/// Chegou uma linha completa (terminada em '\n', '\r' opcional) que termina no
/// marcador final: a leitura pode parar sem esperar o canal fechar. So olha os
/// bytes novos (`out[from..]`); `line_start` guarda, entre as chamadas, o
/// comeco da linha ainda sem '\n'. Custo linear no total: um servidor que manda
/// um byte por pacote nao faz o cliente reler a saida inteira a cada pacote.
fn end_marker_since(out: &[u8], from: usize, line_start: &mut usize) -> bool {
    for (i, &b) in out.iter().enumerate().skip(from) {
        if b != b'\n' {
            continue;
        }
        let line = &out[*line_start..i];
        *line_start = i + 1;
        if line
            .strip_suffix(b"\r")
            .unwrap_or(line)
            .ends_with(b"SAGUOS.end")
        {
            return true;
        }
    }
    false
}

/// `end_marker_since` sobre a saida inteira de uma vez.
#[cfg(test)]
fn finished(out: &[u8]) -> bool {
    end_marker_since(out, 0, &mut 0)
}

// --------------------------------------------------------------- os-release

/// Valor de uma atribuicao do os-release (sintaxe de shell simplificada):
/// aspas duplas com os escapes `\" \\ \$ \``, aspas simples literais, ou
/// palavra sem aspas ate o primeiro espaco. Aspas sem fechar: `None`.
fn unquote(v: &str) -> Option<String> {
    let mut it = v.chars();
    match it.clone().next() {
        Some(q @ ('"' | '\'')) => {
            it.next();
            let mut out = String::new();
            loop {
                match it.next()? {
                    c if c == q => return Some(out),
                    '\\' if q == '"' => match it.next()? {
                        c @ ('\\' | '"' | '$' | '`') => out.push(c),
                        c => {
                            out.push('\\');
                            out.push(c);
                        }
                    },
                    c => out.push(c),
                }
            }
        }
        _ => {
            let mut out = String::new();
            while let Some(c) = it.next() {
                if c.is_whitespace() {
                    break;
                }
                if c == '\\' {
                    if let Some(n) = it.next() {
                        out.push(n);
                    }
                    continue;
                }
                out.push(c);
            }
            Some(out)
        }
    }
}

/// `CHAVE=valor` (chave so com [A-Z0-9_]); comentarios e linhas vazias: `None`.
fn assignment(line: &str) -> Option<(&str, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, rest) = line.split_once('=')?;
    let key_ok = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    if !key_ok {
        return None;
    }
    Some((key, unquote(rest)?))
}

/// os-release: ID, NAME e VERSION_ID (a ultima atribuicao vale). Precisa de
/// ID ou NAME validos; o que faltar vira "linux"/"Linux".
fn parse_os_release(lines: &[String]) -> Option<OsInfo> {
    let (mut id, mut name, mut version) = (None, None, None);
    for l in lines {
        let Some((k, v)) = assignment(l) else {
            continue;
        };
        match k {
            "ID" => id = Some(v),
            "NAME" => name = Some(v),
            "VERSION_ID" => version = Some(v),
            _ => {}
        }
    }
    let id = id.as_deref().and_then(clean_id);
    let name = name.as_deref().and_then(|n| clean(n, MAX_NAME));
    if id.is_none() && name.is_none() {
        return None;
    }
    Some(OsInfo {
        id: id.unwrap_or_else(|| "linux".into()),
        name: name.unwrap_or_else(|| "Linux".into()),
        version: version.as_deref().and_then(|v| clean(v, MAX_VERSION)),
    })
}

// --------------------------------------------------- redhat-release / uname

/// /etc/redhat-release ("CentOS release 6.10 (Final)"), para sistemas sem
/// os-release.
fn parse_redhat_release(lines: &[String]) -> Option<OsInfo> {
    let line = lines.iter().find(|l| !l.trim().is_empty())?;
    let (name, rest) = line.split_once(" release ")?;
    let name = clean(name, MAX_NAME)?;
    let version = rest
        .split_whitespace()
        .next()
        .filter(|v| v.starts_with(|c: char| c.is_ascii_digit()))
        .and_then(|v| clean(v, MAX_VERSION));
    let low = name.to_ascii_lowercase();
    let id = if low.starts_with("centos") {
        "centos".to_string()
    } else if low.starts_with("fedora") {
        "fedora".to_string()
    } else if low.starts_with("red hat") {
        "rhel".to_string()
    } else {
        // Ex.: "Scientific Linux" -> "scientific".
        low.split_whitespace().next().and_then(clean_id)?
    };
    Some(OsInfo { id, name, version })
}

/// Versao de um release do uname ("12.4-RELEASE-p9" -> "12.4").
fn release_version(rel: &str) -> Option<String> {
    let v = rel.split('-').next()?;
    if v.starts_with(|c: char| c.is_ascii_digit()) {
        clean(v, MAX_VERSION)
    } else {
        None
    }
}

/// ProductName e ProductVersion do `sw_vers` (macOS).
fn parse_sw_vers(lines: &[String]) -> (Option<String>, Option<String>) {
    let (mut name, mut version) = (None, None);
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        match k.trim() {
            "ProductName" => name = clean(v.trim(), MAX_NAME),
            "ProductVersion" => version = clean(v.trim(), MAX_VERSION),
            _ => {}
        }
    }
    (name, version)
}

/// `uname -s -r` (e `sw_vers` no macOS), para sistemas sem os-release. So
/// "Linux" nao identifica a distribuicao: `None`.
fn parse_uname(lines: &[String], sw: &[String]) -> Option<OsInfo> {
    let line = lines.iter().find(|l| !l.trim().is_empty())?;
    let mut it = line.split_whitespace();
    let sys = it.next()?;
    let rel = it.next();
    match sys {
        "Darwin" => {
            let (name, version) = parse_sw_vers(sw);
            let name = match name {
                Some(n) if !n.contains("Mac OS X") && n != "macOS" => n,
                _ => "macOS".to_string(),
            };
            Some(OsInfo {
                id: "macos".into(),
                name,
                version,
            })
        }
        "Linux" => None,
        s if s.starts_with("CYGWIN") || s.starts_with("MINGW") || s.starts_with("MSYS") => {
            Some(OsInfo {
                id: "windows".into(),
                name: "Windows".into(),
                version: None,
            })
        }
        s => Some(OsInfo {
            id: clean_id(s)?,
            name: clean(s, MAX_NAME)?,
            version: rel.and_then(release_version),
        }),
    }
}

/// Saida inteira da sonda -> sistema. Precedencia: os-release de /etc, o de
/// /usr/lib, redhat-release e por fim uname. `None` = nada identificado.
fn parse_output(out: &[u8]) -> Option<OsInfo> {
    let s = split_sections(out);
    // So os padroes ("linux"/"Linux") nao identificam nada.
    let useful = |o: &OsInfo| o.id != "linux" || o.name != "Linux";
    parse_os_release(&s.etc)
        .filter(useful)
        .or_else(|| parse_os_release(&s.lib).filter(useful))
        .or_else(|| parse_redhat_release(&s.rh))
        .or_else(|| parse_uname(&s.uname, &s.sw))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    /// Saida da sonda montada como o servidor devolveria.
    fn out(uname: &str, etc: &str, lib: &str, rh: &str, sw: &str) -> Vec<u8> {
        format!(
            "SAGUOS.uname\n{uname}SAGUOS.etc\n{etc}SAGUOS.lib\n{lib}SAGUOS.rh\n{rh}SAGUOS.sw\n{sw}SAGUOS.end\n"
        )
        .into_bytes()
    }

    fn linux(osr: &str) -> OsInfo {
        parse_output(&out("Linux 5.14.0-427.el9.x86_64\n", osr, osr, "", "")).expect(osr)
    }

    fn os(id: &str, name: &str, version: Option<&str>) -> OsInfo {
        OsInfo {
            id: id.into(),
            name: name.into(),
            version: version.map(String::from),
        }
    }

    // Coletados do WSL (AlmaLinux-8, Debian, kali-linux).
    const ALMA8: &str = "NAME=\"AlmaLinux\"\nVERSION=\"8.10 (Cerulean Leopard)\"\nID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"8.10\"\nPLATFORM_ID=\"platform:el8\"\nPRETTY_NAME=\"AlmaLinux 8.10 (Cerulean Leopard)\"\nANSI_COLOR=\"0;34\"\nLOGO=\"fedora-logo-icon\"\nCPE_NAME=\"cpe:/o:almalinux:almalinux:8::baseos\"\nHOME_URL=\"https://almalinux.org/\"\nDOCUMENTATION_URL=\"https://wiki.almalinux.org/\"\nBUG_REPORT_URL=\"https://bugs.almalinux.org/\"\n\nALMALINUX_MANTISBT_PROJECT=\"AlmaLinux-8\"\nALMALINUX_MANTISBT_PROJECT_VERSION=\"8.10\"\nREDHAT_SUPPORT_PRODUCT=\"AlmaLinux\"\nREDHAT_SUPPORT_PRODUCT_VERSION=\"8.10\"\nSUPPORT_END=2029-06-01\n";
    const DEBIAN11: &str = "PRETTY_NAME=\"Debian GNU/Linux 11 (bullseye)\"\nNAME=\"Debian GNU/Linux\"\nVERSION_ID=\"11\"\nVERSION=\"11 (bullseye)\"\nVERSION_CODENAME=bullseye\nID=debian\nHOME_URL=\"https://www.debian.org/\"\nSUPPORT_URL=\"https://www.debian.org/support\"\nBUG_REPORT_URL=\"https://bugs.debian.org/\"\n";
    const KALI: &str = "PRETTY_NAME=\"Kali GNU/Linux Rolling\"\nNAME=\"Kali GNU/Linux\"\nVERSION=\"2023.1\"\nVERSION_ID=\"2023.1\"\nVERSION_CODENAME=\"kali-rolling\"\nID=kali\nID_LIKE=debian\nHOME_URL=\"https://www.kali.org/\"\n";
    // Transcritos dos arquivos publicados pelas distros.
    const UBUNTU2204: &str = "PRETTY_NAME=\"Ubuntu 22.04.4 LTS\"\nNAME=\"Ubuntu\"\nVERSION_ID=\"22.04\"\nVERSION=\"22.04.4 LTS (Jammy Jellyfish)\"\nVERSION_CODENAME=jammy\nID=ubuntu\nID_LIKE=debian\nHOME_URL=\"https://www.ubuntu.com/\"\nUBUNTU_CODENAME=jammy\n";
    const ROCKY9: &str = "NAME=\"Rocky Linux\"\nVERSION=\"9.4 (Blue Onyx)\"\nID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=\"9.4\"\nPLATFORM_ID=\"platform:el9\"\nPRETTY_NAME=\"Rocky Linux 9.4 (Blue Onyx)\"\n";
    const RHEL9: &str = "NAME=\"Red Hat Enterprise Linux\"\nVERSION=\"9.4 (Plow)\"\nID=\"rhel\"\nID_LIKE=\"fedora\"\nVERSION_ID=\"9.4\"\nPRETTY_NAME=\"Red Hat Enterprise Linux 9.4 (Plow)\"\nREDHAT_BUGZILLA_PRODUCT_VERSION=9.4\n";
    const ALPINE: &str = "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.20.3\nPRETTY_NAME=\"Alpine Linux v3.20\"\nHOME_URL=\"https://alpinelinux.org/\"\n";
    const ARCH: &str = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\nBUILD_ID=rolling\nANSI_COLOR=\"38;2;23;147;209\"\nLOGO=archlinux-logo\n";
    const LEAP: &str = "NAME=\"openSUSE Leap\"\nVERSION=\"15.6\"\nID=\"opensuse-leap\"\nID_LIKE=\"suse opensuse\"\nVERSION_ID=\"15.6\"\nPRETTY_NAME=\"openSUSE Leap 15.6\"\n";
    const AMZN2023: &str = "NAME=\"Amazon Linux\"\nVERSION=\"2023\"\nID=\"amzn\"\nID_LIKE=\"fedora\"\nVERSION_ID=\"2023\"\nPLATFORM_ID=\"platform:al2023\"\nPRETTY_NAME=\"Amazon Linux 2023.5.20240805\"\n";
    const OL8: &str = "NAME=\"Oracle Linux Server\"\nVERSION=\"8.10\"\nID=\"ol\"\nID_LIKE=\"fedora\"\nVARIANT=\"Server\"\nVARIANT_ID=\"server\"\nVERSION_ID=\"8.10\"\nPRETTY_NAME=\"Oracle Linux Server 8.10\"\nORACLE_BUGZILLA_PRODUCT_VERSION=8.10\n";
    const RASPBIAN: &str = "PRETTY_NAME=\"Raspbian GNU/Linux 12 (bookworm)\"\nNAME=\"Raspbian GNU/Linux\"\nVERSION_ID=\"12\"\nVERSION=\"12 (bookworm)\"\nVERSION_CODENAME=bookworm\nID=raspbian\nID_LIKE=debian\n";

    #[test]
    fn real_os_release_samples() {
        let cases = [
            (ALMA8, os("almalinux", "AlmaLinux", Some("8.10"))),
            (DEBIAN11, os("debian", "Debian GNU/Linux", Some("11"))),
            (KALI, os("kali", "Kali GNU/Linux", Some("2023.1"))),
            (UBUNTU2204, os("ubuntu", "Ubuntu", Some("22.04"))),
            (ROCKY9, os("rocky", "Rocky Linux", Some("9.4"))),
            (RHEL9, os("rhel", "Red Hat Enterprise Linux", Some("9.4"))),
            (ALPINE, os("alpine", "Alpine Linux", Some("3.20.3"))),
            (ARCH, os("arch", "Arch Linux", None)),
            (LEAP, os("opensuse-leap", "openSUSE Leap", Some("15.6"))),
            (AMZN2023, os("amzn", "Amazon Linux", Some("2023"))),
            (OL8, os("ol", "Oracle Linux Server", Some("8.10"))),
            (RASPBIAN, os("raspbian", "Raspbian GNU/Linux", Some("12"))),
        ];
        for (osr, want) in cases {
            assert_eq!(linux(osr), want);
        }
        // Saida real e completa do AlmaLinux 8 no WSL (com redhat-release).
        let real = format!(
            "SAGUOS.uname\nLinux 6.6.87.2-microsoft-standard-WSL2\nSAGUOS.etc\n{ALMA8}SAGUOS.lib\n{ALMA8}SAGUOS.rh\nAlmaLinux release 8.10 (Cerulean Leopard)\nSAGUOS.sw\nSAGUOS.end\n"
        );
        assert!(finished(real.as_bytes()));
        assert_eq!(
            parse_output(real.as_bytes()),
            Some(os("almalinux", "AlmaLinux", Some("8.10")))
        );
    }

    #[test]
    fn noise_and_missing_newline_before_markers() {
        // Ruido do .bashrc antes do primeiro marcador; os-release sem '\n' final
        // (o proximo marcador gruda na ultima linha); CRLF.
        let raw = "RC-SOURCED\nID=bogus\nSAGUOS.uname\r\nLinux 6.6\r\nSAGUOS.etc\nID=debian\nVERSION_ID=\"12\"SAGUOS.lib\nID=ubuntu\nSAGUOS.rh\nSAGUOS.sw\nSAGUOS.end\n";
        let o = parse_output(raw.as_bytes()).unwrap();
        assert_eq!(o.id, "debian");
        assert_eq!(o.version.as_deref(), Some("12"));
        assert!(finished(raw.as_bytes()));
        // So com a linha completa.
        assert!(!finished(b"SAGUOS.uname\nLinux\nSAGUOS.en"));
        assert!(!finished(b"SAGUOS.uname\nLinux\nSAGUOS.end"));
        // Marcador repetido recomeca a secao.
        let raw = "SAGUOS.etc\nID=lixo\nSAGUOS.etc\nID=debian\nSAGUOS.end\n";
        assert_eq!(parse_output(raw.as_bytes()).unwrap().id, "debian");
    }

    #[test]
    fn falls_back_to_lib_redhat_uname_and_sw_vers() {
        // So /usr/lib/os-release.
        let o = parse_output(&out("Linux 6\n", "", ALMA8, "", "")).unwrap();
        assert_eq!(o.id, "almalinux");
        // CentOS e RHEL 6: sem os-release.
        let o = parse_output(&out(
            "Linux 2.6.32\n",
            "",
            "",
            "CentOS release 6.10 (Final)\n",
            "",
        ));
        assert_eq!(o, Some(os("centos", "CentOS", Some("6.10"))));
        let o = parse_output(&out(
            "Linux 2.6.32\n",
            "",
            "",
            "Red Hat Enterprise Linux Server release 6.10 (Santiago)\n",
            "",
        ));
        assert_eq!(
            o,
            Some(os("rhel", "Red Hat Enterprise Linux Server", Some("6.10")))
        );
        let o = parse_output(&out(
            "Linux 2.6.32\n",
            "",
            "",
            "Scientific Linux release 6.10 (Carbon)\n",
            "",
        ));
        assert_eq!(o, Some(os("scientific", "Scientific Linux", Some("6.10"))));
        let o = parse_output(&out(
            "Linux 3.10\n",
            "",
            "",
            "Fedora release 20 (Heisenbug)\n",
            "",
        ));
        assert_eq!(o.unwrap().id, "fedora");
        // macOS.
        let sw = "ProductName:\t\tmacOS\nProductVersion:\t\t14.5\nBuildVersion:\t\t23F79\n";
        let o = parse_output(&out("Darwin 23.5.0\n", "", "", "", sw));
        assert_eq!(o, Some(os("macos", "macOS", Some("14.5"))));
        let sw = "ProductName:\tMac OS X\nProductVersion:\t10.15.7\n";
        let o = parse_output(&out("Darwin 19.6.0\n", "", "", "", sw));
        assert_eq!(o, Some(os("macos", "macOS", Some("10.15.7"))));
        let o = parse_output(&out("Darwin 19.6.0\n", "", "", "", ""));
        assert_eq!(o, Some(os("macos", "macOS", None)));
        // BSDs e Windows (Cygwin/MSYS) sem os-release.
        let o = parse_output(&out("OpenBSD 7.5\n", "", "", "", ""));
        assert_eq!(o, Some(os("openbsd", "OpenBSD", Some("7.5"))));
        let o = parse_output(&out("FreeBSD 12.4-RELEASE-p9\n", "", "", "", ""));
        assert_eq!(o, Some(os("freebsd", "FreeBSD", Some("12.4"))));
        let o = parse_output(&out("MSYS_NT-10.0-19045 3.4.10.x86_64\n", "", "", "", ""));
        assert_eq!(o, Some(os("windows", "Windows", None)));
        // Linux sem nada: so o kernel nao identifica a distribuicao.
        assert_eq!(parse_output(&out("Linux 6.6\n", "", "", "", "")), None);
        // os-release so com os padroes tambem nao conta.
        assert_eq!(
            parse_output(&out("Linux 6.6\n", "ID=linux\nNAME=Linux\n", "", "", "")),
            None
        );
        // Nada.
        assert_eq!(parse_output(b""), None);
        assert_eq!(
            parse_output(b"This account is currently not available.\n"),
            None
        );
    }

    #[test]
    fn os_release_quoting_is_tolerant() {
        let o = parse_os_release(&lines(
            "# comentario\n  ID=foo # comentario\nNAME='Meu SO'\nVERSION_ID=\"1.0\nID=Bar\n",
        ))
        .unwrap();
        assert_eq!(o.id, "bar", "a ultima atribuicao vale, em minusculas");
        assert_eq!(o.name, "Meu SO");
        assert_eq!(o.version, None, "aspas sem fechar: linha ignorada");
        // Escapes dentro de aspas duplas; barra desconhecida fica.
        let o = parse_os_release(&lines("NAME=\"Meu \\\"SO\\\" \\$HOME \\\\ \\x\"\n")).unwrap();
        assert_eq!(o.name, "Meu \"SO\" $HOME \\ \\x");
        assert_eq!(o.id, "linux");
        // Aspas simples sao literais.
        let o = parse_os_release(&lines("ID=x\nNAME='a \\\" b'\n")).unwrap();
        assert_eq!(o.name, "a \\\" b");
        // Chave invalida ou sem '=': ignorada.
        assert_eq!(parse_os_release(&lines("id=x\nNAME\n")), None);
    }

    #[test]
    fn hostile_values_are_rejected_or_trimmed() {
        let long = "A".repeat(10_000);
        // ID invalido e NAME com ESC: nada utilizavel.
        let osr = "ID=\"Evil Distro\"\nNAME=\"x\u{1b}[31mred\"\n";
        assert_eq!(parse_os_release(&lines(osr)), None);
        // NAME com ESC descartado; ID valido fica.
        let o = parse_os_release(&lines("ID=ok\nNAME=\"x\u{1b}[31mred\"\n")).unwrap();
        assert_eq!(o.name, "Linux");
        let osr =
            format!("ID=ok\nNAME=\"  muitos   espacos\tе unicode  \"\nVERSION_ID=\"{long}\"\n");
        let o = parse_os_release(&lines(&osr)).unwrap();
        assert_eq!(o.name, "muitos espacos е unicode");
        assert_eq!(o.version.as_ref().unwrap().chars().count(), MAX_VERSION);
        let o = parse_os_release(&lines(&format!("ID=ok\nNAME=\"{long}\"\n"))).unwrap();
        assert_eq!(o.name.chars().count(), MAX_NAME);
        // Controle, separador de linha, bidi e invisiveis descartam o campo.
        for bad in [
            "a\u{0}b",
            "a\u{1b}b",
            "a\u{2028}b",
            "abc\u{202E}gpj",
            "zw\u{200B}sp",
            "a\u{85}b",
        ] {
            assert_eq!(clean(bad, 10), None, "{bad:?}");
        }
        assert_eq!(clean("a\tb", 10).as_deref(), Some("a b"));
        assert_eq!(clean_id("Ubuntu"), Some("ubuntu".into()));
        assert_eq!(clean_id("evil distro"), None);
        assert_eq!(clean_id("../x"), None);
        assert_eq!(clean_id(&"a".repeat(33)), None);
        assert_eq!(clean_id("ubuntu\u{200B}"), None);
        // UTF-8 invalido vira U+FFFD, sem panico.
        let o = parse_output(b"SAGUOS.etc\nID=x\nNAME=\"\xff\xfe\"\nSAGUOS.end\n").unwrap();
        assert_eq!(o.name, "\u{FFFD}\u{FFFD}");
    }

    #[test]
    fn banner_decides_probe() {
        for skip in [
            "SSH-2.0-OpenSSH_for_Windows_8.1",
            "SSH-2.0-OpenSSH_for_Windows_9.5",
            "SSH-2.0-9.35 FlowSsh: Bitvise SSH Server (WinSSHD) 9.35",
            "SSH-2.0-Cisco-1.25",
            "SSH-2.0-ROSSSH",
            "SSH-2.0-HUAWEI-1.5",
            "SSH-2.0-Comware-7.1.064",
        ] {
            assert_eq!(banner(skip.as_bytes()), Banner::Skip, "{skip}");
        }
        for probe in [
            "SSH-2.0-OpenSSH_8.0",
            "SSH-2.0-OpenSSH_8.9p1 Ubuntu-3ubuntu0.10",
            "SSH-2.0-dropbear_2022.83",
            "lixo",
            "",
        ] {
            assert_eq!(banner(probe.as_bytes()), Banner::Probe, "{probe}");
        }
        assert_eq!(banner(b"SSH-2.0-\xff\xfe"), Banner::Probe);
    }

    #[test]
    fn cmd_exe_echoes_the_whole_line() {
        // cmd.exe (se um Windows escapar do banner): `echo` imprime a linha toda.
        let raw = format!("{}\r\n", PROBE_COMMAND.replacen("echo ", "", 1));
        assert!(finished(raw.as_bytes()));
        assert_eq!(parse_output(raw.as_bytes()), None);
        assert!(finished(b"x\r\nSAGUOS.end\r\n"));
        assert!(!finished(b"SAGUOS.end"));
    }

    #[test]
    fn command_is_shell_agnostic() {
        for bad in [
            '>', '<', '|', '&', '$', '`', '"', '\'', '*', '?', '[', '#', '!', '~', '{', '(', '\\',
            '\n', '%', '^', '@', '=',
        ] {
            assert!(!PROBE_COMMAND.contains(bad), "{bad:?}");
        }
        assert!(PROBE_COMMAND.ends_with("echo SAGUOS.end"));
        // Todo marcador que o comando imprime e reconhecido.
        for part in PROBE_COMMAND.split("; ") {
            if let Some(m) = part.strip_prefix("echo ") {
                assert!(marker(m).is_some(), "{m}");
            }
        }
    }

    /// A leitura incremental (pacote a pacote) acha o marcador final no mesmo
    /// ponto que a saida inteira, inclusive com um byte por pacote.
    #[test]
    fn end_marker_found_incrementally() {
        let real = format!(
            "RC\nSAGUOS.uname\r\nLinux 6.6\r\nSAGUOS.etc\n{ALMA8}SAGUOS.lib\nSAGUOS.rh\nSAGUOS.sw\nSAGUOS.end\r\nlixo depois"
        );
        let samples: [&[u8]; 6] = [
            real.as_bytes(),
            b"SAGUOS.end\n",
            b"x\r\nSAGUOS.end\r\n",
            b"SAGUOS.uname\nLinux\nSAGUOS.end",
            b"ID=xSAGUOS.end\n",
            b"\n\n\nSAGUOS.en\nd\n",
        ];
        for sample in samples {
            for chunk in [1, 2, 7, 64, sample.len()] {
                let (mut out, mut line_start, mut found_at) = (Vec::new(), 0, None);
                for part in sample.chunks(chunk) {
                    let from = out.len();
                    out.extend_from_slice(part);
                    if end_marker_since(&out, from, &mut line_start) {
                        found_at = Some(out.len());
                        break;
                    }
                }
                // Primeiro prefixo (em pedacos de `chunk`) em que a saida inteira
                // ja tem a linha final completa.
                let want = (1..=sample.len().div_ceil(chunk))
                    .map(|k| (k * chunk).min(sample.len()))
                    .find(|&n| finished(&sample[..n]));
                let text = String::from_utf8_lossy(sample);
                assert_eq!(found_at, want, "{text:?} em pedacos de {chunk}");
            }
        }
        assert!(finished(real.as_bytes()));
        assert!(!finished(b"SAGUOS.uname\nLinux\nSAGUOS.end"));
    }

    /// Servidor hostil: 64 KiB, um byte por pacote, so quebras de linha. A
    /// leitura nao rele a saida a cada pacote (antes: segundos de CPU).
    #[test]
    fn end_marker_scan_is_linear() {
        let t0 = std::time::Instant::now();
        let (mut out, mut line_start) = (Vec::new(), 0);
        for _ in 0..MAX_OUTPUT {
            let from = out.len();
            out.push(b'\n');
            assert!(!end_marker_since(&out, from, &mut line_start));
        }
        assert_eq!(line_start, MAX_OUTPUT);
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
    }

    // --- OsProbe: relatorio e fim da sessao --------------------------------

    fn probe_host() -> Host {
        let mut h = Host::new();
        h.id = uuid::Uuid::from_u128(7);
        h.host = "srv".into();
        h.port = 2222;
        h
    }

    type TestProbe = OsProbe<Box<dyn Fn(OsReport)>>;

    /// Roda `f` num runtime de uma thread, com um OsProbe cujos relatorios vao
    /// para a lista devolvida (inclusive os do drop, no fim).
    fn with_probe(f: impl AsyncFnOnce(&mut TestProbe)) -> Vec<OsReport> {
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let report: Box<dyn Fn(OsReport)> = Box::new(move |r| {
                let _ = tx.send(r);
            });
            let mut probe = OsProbe::new(&probe_host(), report);
            f(&mut probe).await;
        });
        rx.try_iter().collect()
    }

    /// Deixa as tarefas prontas rodarem (runtime de uma thread).
    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    #[test]
    fn probe_reports_result_with_connection_endpoint() {
        // Resultado lido no laco da sessao (wait).
        let got = with_probe(async |p| {
            p.task
                .spawn(async { Some(os("debian", "Debian GNU/Linux", Some("12"))) });
            assert!(p.running());
            p.wait().await;
            assert!(!p.running());
        });
        assert_eq!(got.len(), 1, "um relatorio so (o drop nao repete)");
        let r = &got[0];
        assert_eq!(
            (r.host_id, r.host.as_str(), r.port),
            (uuid::Uuid::from_u128(7), "srv", 2222)
        );
        assert_eq!(r.os.as_ref().unwrap().id, "debian");

        // Terminou, mas a sessao acabou antes de ler: vai no drop.
        let got = with_probe(async |p| {
            p.task.spawn(async { None });
            settle().await;
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].os, None);
    }

    #[test]
    fn probe_pending_at_session_end() {
        // Comando ja pedido e sem resposta (a sessao caiu no meio): conta como
        // tentativa sem resultado.
        let got = with_probe(async |p| {
            let started = Arc::clone(&p.started);
            p.task.spawn(async move {
                started.store(true, Ordering::SeqCst);
                std::future::pending::<Option<OsInfo>>().await
            });
            settle().await;
        });
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].os, None);

        // Ainda no atraso inicial (sessao curta): nada a avisar; a proxima
        // conexao tenta de novo.
        let got = with_probe(async |p| {
            p.task.spawn(std::future::pending::<Option<OsInfo>>());
            settle().await;
        });
        assert!(got.is_empty());

        // Sem sonda (deteccao desligada): nunca avisa.
        let got = with_probe(async |p| {
            assert!(!p.running());
            settle().await;
        });
        assert!(got.is_empty());
    }

    #[test]
    fn icon_slug_by_id_only() {
        let slug = |id: &str| icon_slug(&os(id, "X", None));
        let cases = [
            ("ubuntu", "ubuntu"),
            ("ubuntu-core", "ubuntu"),
            ("debian", "debian"),
            ("raspbian", "raspberrypi"),
            ("rhel", "redhat"),
            ("rhcos", "redhat"),
            ("centos", "centos"),
            ("fedora", "fedora"),
            ("fedora-asahi-remix", "fedora"),
            ("almalinux", "almalinux"),
            ("rocky", "rockylinux"),
            ("arch", "archlinux"),
            ("archarm", "archlinux"),
            ("alpine", "alpinelinux"),
            ("opensuse", "opensuse"),
            ("opensuse-leap", "opensuse"),
            ("opensuse-tumbleweed", "opensuse"),
            ("opensuse-microos", "opensuse"),
            ("suse", "suse"),
            ("sles", "suse"),
            ("sles_sap", "suse"),
            ("sled", "suse"),
            ("sle-micro", "suse"),
            ("sl-micro", "suse"),
            ("linuxmint", "linuxmint"),
            ("manjaro", "manjaro"),
            ("manjaro-arm", "manjaro"),
            ("gentoo", "gentoo"),
            ("nixos", "nixos"),
            ("pop", "popos"),
            ("elementary", "elementary"),
            ("zorin", "zorin"),
            ("endeavouros", "endeavouros"),
            ("void", "voidlinux"),
            ("slackware", "slackware"),
            ("freebsd", "freebsd"),
            ("macos", "apple"),
            ("devuan", "devuan"),
            ("openwrt", "openwrt"),
        ];
        let mut reached = std::collections::HashSet::new();
        for (id, want) in cases {
            assert_eq!(slug(id), Some(want), "{id}");
            assert!(ICON_SLUGS.contains(&want), "{want} fora de ICON_SLUGS");
            reached.insert(want);
        }
        assert_eq!(
            reached.len(),
            ICON_SLUGS.len(),
            "slug sem id que chegue nele"
        );
        // Sem icone proprio (nem pelo ID_LIKE): mantem o icone atual.
        for id in [
            "ol",
            "amzn",
            "kali",
            "scientific",
            "cloudlinux",
            "mx",
            "openbsd",
            "netbsd",
            "windows",
            "linux",
            "foobar",
            "",
            "opensuse_x",
            "manjaro_",
            "ubuntu-",
            "debian-x",
        ] {
            assert_eq!(slug(id), None, "{id:?}");
        }
    }

    /// Os exemplos reais e transcritos dos testes acima caem no icone certo
    /// (ou em nenhum, para derivados sem icone proprio).
    #[test]
    fn icon_slug_covers_samples() {
        for (osr, want) in [
            (ALMA8, Some("almalinux")),
            (DEBIAN11, Some("debian")),
            (KALI, None),
            (UBUNTU2204, Some("ubuntu")),
            (ROCKY9, Some("rockylinux")),
            (RHEL9, Some("redhat")),
            (ALPINE, Some("alpinelinux")),
            (ARCH, Some("archlinux")),
            (LEAP, Some("opensuse")),
            (AMZN2023, None),
            (OL8, None),
            (RASPBIAN, Some("raspberrypi")),
        ] {
            assert_eq!(icon_slug(&linux(osr)), want, "{osr}");
        }
        // Sem os-release: redhat-release e uname/sw_vers.
        let rh = |l: &str| parse_output(&out("Linux 2.6.32\n", "", "", l, "")).unwrap();
        assert_eq!(
            icon_slug(&rh("CentOS release 6.10 (Final)\n")),
            Some("centos")
        );
        assert_eq!(
            icon_slug(&rh(
                "Red Hat Enterprise Linux Server release 6.10 (Santiago)\n"
            )),
            Some("redhat")
        );
        assert_eq!(
            icon_slug(&rh("Fedora release 20 (Heisenbug)\n")),
            Some("fedora")
        );
        assert_eq!(
            icon_slug(&rh("Scientific Linux release 6.10 (Carbon)\n")),
            None
        );
        let un = |u: &str, sw: &str| parse_output(&out(u, "", "", "", sw)).unwrap();
        assert_eq!(
            icon_slug(&un("Darwin 23.5.0\n", "ProductVersion: 14.5\n")),
            Some("apple")
        );
        assert_eq!(
            icon_slug(&un("FreeBSD 12.4-RELEASE-p9\n", "")),
            Some("freebsd")
        );
        assert_eq!(icon_slug(&un("OpenBSD 7.5\n", "")), None);
        assert_eq!(icon_slug(&un("MSYS_NT-10.0-19045 3.4.10\n", "")), None);
    }

    #[test]
    fn label_joins_name_and_version() {
        assert_eq!(
            os("almalinux", "AlmaLinux", Some("8.10")).label(),
            "AlmaLinux 8.10"
        );
        assert_eq!(linux(ARCH).label(), "Arch Linux");
        let sw = "ProductName:\t\tmacOS\nProductVersion:\t\t14.5\n";
        assert_eq!(
            parse_output(&out("Darwin 23.5.0\n", "", "", "", sw))
                .unwrap()
                .label(),
            "macOS 14.5"
        );
        let o = parse_output(&out("FreeBSD 12.4-RELEASE-p9\n", "", "", "", "")).unwrap();
        assert_eq!(o.label(), "FreeBSD 12.4");
        // Maior rotulo possivel: nome e versao no limite.
        let o = os("x", &"n".repeat(MAX_NAME), Some(&"9".repeat(MAX_VERSION)));
        assert_eq!(o.label().chars().count(), MAX_NAME + 1 + MAX_VERSION);
    }

    // --- Ponta a ponta contra sshd reais (ignorados) -----------------------
    //
    // Mesmas variaveis dos testes de `upload` e `hostkey`: SAGU_E2E_PORT (2222;
    // 2223 para `e2e_os_detect_login_tmux`), SAGU_E2E_PORT_B (2224, com
    // `MaxSessions 1`), SAGU_E2E_USER e SAGU_E2E_KEY. A chave do sshd
    // descartavel e aceita (TOFU).
    // Rodar com: cargo test e2e_os_detect -- --ignored --test-threads=1

    use crate::hostkey::HostKeyAnswer;
    use crate::sftp::{self, SftpToUi};
    use crate::ssh::{self, SshHandle, SshToUi};
    use crate::vault::AuthMethod;
    use std::time::Instant;

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"))
    }

    fn env_port(k: &str) -> u16 {
        env(k)
            .parse()
            .unwrap_or_else(|_| panic!("{k} nao e uma porta"))
    }

    /// Host de teste (127.0.0.1) com a chave de cliente do sshd descartavel.
    fn e2e_host(port: u16) -> Host {
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = port;
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        host
    }

    /// Nome do evento, para as mensagens de falha.
    fn ev_name(ev: &SshToUi) -> String {
        match ev {
            SshToUi::Connected => "Connected".into(),
            SshToUi::Data(_) => "Data".into(),
            SshToUi::Error(e) => format!("Error({e})"),
            SshToUi::Closed => "Closed".into(),
            SshToUi::Upload(u) => format!("Upload({u:?})"),
            SshToUi::HostKey(_) => "HostKey".into(),
            SshToUi::Os(r) => format!("Os({r:?})"),
        }
    }

    /// Terminal SSH de teste: junta a saida e aceita a chave do servidor.
    struct Term {
        h: SshHandle,
        out: Vec<u8>,
    }

    impl Term {
        fn connect(host: &Host, detect_os: bool) -> Self {
            Term {
                h: ssh::connect(host.clone(), 80, 24, detect_os, || {}),
                out: Vec::new(),
            }
        }

        /// Proximo evento que nao e saida do terminal nem pergunta de chave;
        /// `None` se nada chegar em `secs`.
        fn next(&mut self, secs: u64) -> Option<SshToUi> {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(secs) {
                match self.h.from_ssh.recv_timeout(Duration::from_millis(100)) {
                    Ok(SshToUi::Data(d)) => self.out.extend_from_slice(&d),
                    Ok(SshToUi::HostKey(p)) => {
                        let _ = p.reply.send(HostKeyAnswer::Accept);
                    }
                    Ok(ev) => return Some(ev),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return None,
                }
            }
            None
        }

        fn expect_connected(&mut self) {
            match self.next(20) {
                Some(SshToUi::Connected) => {}
                other => panic!("esperava Connected: {:?}", other.as_ref().map(ev_name)),
            }
        }

        /// Espera o relatorio do SO.
        fn expect_os(&mut self, secs: u64) -> OsReport {
            match self.next(secs) {
                Some(SshToUi::Os(r)) => r,
                other => panic!("esperava Os: {:?}", other.as_ref().map(ev_name)),
            }
        }

        /// O shell ainda responde: `echo SAGU-""OK` (o eco da digitacao nao
        /// contem "SAGU-OK"; so a saida do comando).
        fn assert_shell_answers(&mut self) {
            let from = self.out.len();
            self.h.send_data(b"echo SAGU-\"\"OK\n".to_vec());
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(10) {
                if String::from_utf8_lossy(&self.out[from..]).contains("SAGU-OK") {
                    return;
                }
                if let Some(ev) = self.next(1) {
                    panic!("evento inesperado esperando o shell: {}", ev_name(&ev));
                }
            }
            panic!(
                "o shell nao respondeu: {:?}",
                String::from_utf8_lossy(&self.out[from..])
            );
        }

        /// Nada da sonda apareceu no terminal.
        fn assert_no_probe_output(&self) {
            let text = String::from_utf8_lossy(&self.out);
            for needle in ["SAGUOS", "PRETTY_NAME", "os-release"] {
                assert!(!text.contains(needle), "{needle} no terminal: {text:?}");
            }
        }

        /// Encerra pelo protocolo e espera o `Closed`.
        fn disconnect(&mut self) {
            self.h.disconnect();
            match self.next(10) {
                Some(SshToUi::Closed) => {}
                other => panic!("esperava Closed: {:?}", other.as_ref().map(ev_name)),
            }
        }
    }

    fn assert_alma8(r: &OsReport, host: &Host) {
        assert_eq!(
            (r.host_id, r.host.as_str(), r.port),
            (host.id, "127.0.0.1", host.port)
        );
        let o = r.os.as_ref().expect("SO nao identificado");
        assert_eq!(o.id, "almalinux", "{o:?}");
        assert!(
            o.version.as_deref().is_some_and(|v| v.starts_with("8.")),
            "{o:?}"
        );
        // O cartao troca para o icone do AlmaLinux.
        assert_eq!(icon_slug(o), Some("almalinux"));
    }

    /// Stdout de `cmd` rodado no servidor por exec (o stderr e descartado).
    fn remote_stdout(host: &Host, cmd: &str) -> String {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            // sshd descartavel de teste: confia na chave (TOFU).
            let (session, _) = ssh::connect_and_auth(host, |p| {
                let _ = p.reply.send(HostKeyAnswer::Accept);
            })
            .await
            .expect("conexao do exec");
            let mut ch = session.channel_open_session().await.expect("canal do exec");
            ch.exec(true, cmd).await.expect("exec");
            ch.eof().await.expect("eof");
            let mut out = Vec::new();
            loop {
                match tokio::time::timeout(Duration::from_secs(20), ch.wait()).await {
                    Err(_) => panic!("tempo esgotado em {cmd:?}"),
                    Ok(Some(ChannelMsg::Data { data })) => out.extend_from_slice(&data),
                    Ok(Some(ChannelMsg::Eof | ChannelMsg::Close) | None) => break,
                    Ok(Some(_)) => {}
                }
            }
            let _ = session
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
            String::from_utf8_lossy(&out).into_owned()
        })
    }

    /// Sessoes do servidor tmux do exec (na 2223, o isolado de TMUX_TMPDIR).
    fn tmux_sessions(host: &Host) -> Vec<String> {
        remote_stdout(host, "tmux ls")
            .lines()
            .filter_map(|l| l.split_once(": "))
            .filter(|(_, rest)| rest.contains("window"))
            .map(|(name, _)| name.to_string())
            .collect()
    }

    #[test]
    #[ignore]
    fn e2e_os_detect_ssh() {
        let host = e2e_host(env_port("SAGU_E2E_PORT"));
        let mut t = Term::connect(&host, true);
        t.expect_connected();
        let r = t.expect_os(20);
        assert_alma8(&r, &host);
        t.assert_shell_answers();
        t.assert_no_probe_output();
        t.disconnect();
    }

    #[test]
    #[ignore]
    fn e2e_os_detect_sftp() {
        let host = e2e_host(env_port("SAGU_E2E_PORT"));
        let h = sftp::connect(host.clone(), true, || {});
        let next = |secs: u64| -> Option<SftpToUi> {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(secs) {
                match h.from_sftp.recv_timeout(Duration::from_millis(100)) {
                    Ok(SftpToUi::HostKey(p)) => {
                        let _ = p.reply.send(HostKeyAnswer::Accept);
                    }
                    Ok(ev) => return Some(ev),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => return None,
                }
            }
            None
        };
        assert!(
            matches!(next(20), Some(SftpToUi::Connected { .. })),
            "SFTP nao conectou"
        );
        match next(20) {
            Some(SftpToUi::Os(r)) => assert_alma8(&r, &host),
            Some(SftpToUi::Error(e)) => panic!("erro da sessao: {e}"),
            _ => panic!("esperava Os"),
        }
        h.disconnect();
        assert!(
            matches!(next(10), Some(SftpToUi::Closed)),
            "SFTP nao fechou"
        );
    }

    #[test]
    #[ignore]
    fn e2e_os_detect_disabled() {
        let host = e2e_host(env_port("SAGU_E2E_PORT"));
        let mut t = Term::connect(&host, false);
        t.expect_connected();
        if let Some(ev) = t.next(6) {
            panic!("evento inesperado sem a deteccao: {}", ev_name(&ev));
        }
        t.assert_shell_answers();
        t.disconnect();
    }

    /// `.bashrc` que abre o tmux no login (sshd 2223, servidor tmux isolado
    /// em TMUX_TMPDIR): a sonda nao trava nem cria sessao. So a sessao
    /// "padrao" do login deste teste aparece, e ela e fechada no fim.
    #[test]
    #[ignore]
    fn e2e_os_detect_login_tmux() {
        let host = e2e_host(env_port("SAGU_E2E_PORT"));
        let tmpdir = remote_stdout(&host, "echo $TMUX_TMPDIR");
        assert!(
            tmpdir.lines().any(|l| l.trim() == "/tmp/sagu-e2e-tmuxdir"),
            "rode com SAGU_E2E_PORT=2223 (sshd com o tmux isolado): {tmpdir:?}"
        );
        let before = tmux_sessions(&host);
        assert!(
            before.is_empty(),
            "o servidor tmux isolado ja tem sessoes {before:?}: nada foi encerrado"
        );

        let mut t = Term::connect(&host, true);
        let connected = matches!(t.next(20), Some(SshToUi::Connected));
        let os = if connected { t.next(20) } else { None };
        let during = tmux_sessions(&host);

        // Limpeza antes das conferencias: fecha so a sessao "padrao" que o
        // login deste teste criou (a lista comecou vazia) e sai do shell.
        if connected {
            t.h.send_data(b"tmux kill-session -t padrao\n".to_vec());
            std::thread::sleep(Duration::from_millis(1500));
            t.h.send_data(b"exit\n".to_vec());
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(10) {
                if matches!(t.next(1), Some(SshToUi::Closed)) {
                    break;
                }
            }
        }
        let mut after = tmux_sessions(&host);
        let t0 = Instant::now();
        while !after.is_empty() && t0.elapsed() < Duration::from_secs(10) {
            std::thread::sleep(Duration::from_millis(500));
            after = tmux_sessions(&host);
        }

        assert!(connected, "nao conectou");
        match os {
            Some(SshToUi::Os(r)) => assert_alma8(&r, &host),
            other => panic!("esperava Os: {:?}", other.as_ref().map(ev_name)),
        }
        t.assert_no_probe_output();
        assert_eq!(during, ["padrao"], "sessoes durante o teste");
        assert_eq!(after, before, "sessao tmux sobrando");
    }

    /// Servidor que recusa o canal extra (sshd B com `MaxSessions 1`): a sonda
    /// termina sem resultado, sem erro, e o terminal segue normal.
    #[test]
    #[ignore]
    fn e2e_os_detect_exec_refused() {
        let host = e2e_host(env_port("SAGU_E2E_PORT_B"));
        let mut t = Term::connect(&host, true);
        t.expect_connected();
        let r = t.expect_os(10);
        assert_eq!((r.host_id, r.port), (host.id, host.port));
        assert!(
            r.os.is_none(),
            "o sshd de SAGU_E2E_PORT_B aceitou o canal extra ({:?}): falta `MaxSessions 1` \
             no sshd_2224 (receita dos e2e)",
            r.os
        );
        t.assert_shell_answers();
        t.assert_no_probe_output();
        t.disconnect();
    }
}
