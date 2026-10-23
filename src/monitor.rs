//! Monitoramento dos servidores cadastrados (tela "Monitoramento").
//!
//! - Uma thread com runtime tokio proprio, com uma tarefa por servidor: cada
//!   uma mantem a sua conexao SSH aberta entre as coletas (um login por
//!   abertura da tela, nao um por minuto) e reconecta se ela cair.
//! - A UI marca o ritmo: `MonitorHandle::refresh` manda a lista de hosts e
//!   cada tarefa faz uma coleta; o resultado volta por `MonitorEvent`.
//! - Comando constante e agnostico de shell ([`COMMAND`]), por `exec`, com o
//!   stdin fechado; so le o `/proc` do Linux e o `df`. Prazos e limite de
//!   bytes como na sonda do SO (ver `osinfo`); o que chegar antes do prazo
//!   (ex.: `df` travado num NFS) ainda e aproveitado.
//! - A chave do servidor nunca e perguntada aqui: sem chave confirmada (ou
//!   com chave diferente) a coleta desiste antes de mandar credenciais, e o
//!   cartao pede para conectar pelo terminal (ver `ssh::connect_and_auth`).
//! - Credenciais recusadas nao sao tentadas de novo a cada minuto (seriam
//!   tentativas de login falhas nos logs do servidor, e um fail2ban poderia
//!   bloquear o IP do usuario): so com "Atualizar agora" ou editando o host.
//! - Tudo o que vem do servidor e hostil: numeros com conta saturada, pontos
//!   de montagem saneados (`osinfo::clean`) e quantidade limitada.
//! - Uma vez por conexao, depois da primeira coleta que deu certo, um segundo
//!   comando constante ([`KERNEL_COMMAND`]) procura um kernel mais novo que o
//!   em uso: instalado (falta reiniciar) ou nos repositorios, pelo cache que o
//!   servidor ja tem (sem baixar nada). O resultado vem por `Event::Kernel`.
//! - Fechar a tela derruba o canal de comandos: a thread termina e as
//!   conexoes caem junto com as tarefas.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use russh::{client, ChannelMsg};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::{AbortHandle, JoinSet};
use tokio::time::timeout;

use crate::hostkey::{HostKeyAnswer, HostKeyPrompt};
use crate::osinfo::{self, Banner};
use crate::ssh::{self, Client};
use crate::vault::{AuthMethod, Host};

/// Intervalo entre as coletas enquanto a tela esta aberta.
pub const INTERVAL: Duration = Duration::from_secs(60);

/// Comando constante e agnostico de shell (so `;`; sem pipe, redirecionamento,
/// aspas, `$`, glob nem `#`).
/// - CPUs: `getconf` antes do `nproc`. O load conta as CPUs online da maquina
///   toda; o `nproc` so as que a sessao pode usar (afinidade, cpuset,
///   OMP_NUM_THREADS) e dividiria o load por menos CPUs do que ele conta.
/// - O `sleep 1` entre as duas leituras do `/proc/stat` da o uso de CPU da
///   primeira coleta; nas seguintes vale a media desde a coleta anterior (ver
///   `Carry`).
/// - O `df` vai por ultimo e com prazo: num NFS fora do ar ele trava sem
///   escrever nada, e cada coleta deixaria mais um `df` preso no servidor
///   (somando ao load). Morto pelo `timeout`, o resto da resposta ja chegou.
///   Duas vezes, uma em cada sintaxe do `timeout` (GNU e BusyBox novo; e
///   BusyBox antigo, com `-t`): a errada falha na hora, sem rodar o `df`.
/// - `LC_ALL=C` no `df`: com o servidor em portugues, o cabecalho sairia
///   traduzido ("Sist. Arq.").
/// - Rede: o `/proc/net/dev` dos dois lados do `sleep 1`, cada um com a hora
///   pelo relogio do servidor (`/proc/uptime`), como a CPU (ver `Carry`). O
///   `ls -l /sys/class/net` separa as interfaces fisicas das virtuais (lo,
///   bridge, veth, docker, bond, VLAN, tun), que contariam o mesmo trafego de
///   novo.
pub const COMMAND: &str = "echo SAGUMON.load; cat /proc/loadavg; \
echo SAGUMON.cpus; getconf _NPROCESSORS_ONLN; nproc; echo SAGUMON.mem; cat /proc/meminfo; \
echo SAGUMON.up; cat /proc/uptime; echo SAGUMON.stat; head -n 1 /proc/stat; \
echo SAGUMON.net; cat /proc/net/dev; sleep 1; echo SAGUMON.stat; head -n 1 /proc/stat; \
echo SAGUMON.up2; cat /proc/uptime; echo SAGUMON.net2; cat /proc/net/dev; \
echo SAGUMON.nic; ls -l /sys/class/net; echo SAGUMON.df; \
env LC_ALL=C timeout -s KILL 5 df -P -k; env LC_ALL=C timeout -t 5 -s KILL df -P -k; \
echo SAGUMON.end";

/// Verificacao do kernel, uma vez por conexao (mesmas regras do [`COMMAND`]):
/// - `uname -r` e `/lib/modules` (uma pasta por kernel instalado): um kernel
///   mais novo instalado que o em uso espera um reboot. Debian e Ubuntu
///   tambem anotam o pacote do kernel atualizado em
///   `/var/run/reboot-required.pkgs` (ate com a mesma versao).
/// - `dnf -C` (so o cache do sistema, sem baixar nada; um usuario comum le o
///   cache do root) e `apt list --upgradable` (as listas que o servidor ja
///   tem): kernel mais novo nos repositorios. O que nao existe no servidor
///   falha na hora; o `timeout` corta um `dnf` preso numa trava do rpm.
pub const KERNEL_COMMAND: &str = "echo SAGUMON.kver; uname -r; echo SAGUMON.kmods; \
ls /lib/modules; echo SAGUMON.kboot; cat /var/run/reboot-required.pkgs; echo SAGUMON.kpkg; \
env LC_ALL=C timeout -s KILL 20 dnf -C -q check-update kernel kernel-core kernel-uek; \
env LC_ALL=C timeout -s KILL 20 apt list --upgradable; echo SAGUMON.end";

/// Prazo para conectar e autenticar.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Prazo para o servidor abrir o canal.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Prazo para o comando responder (inclui o `sleep 1` e o prazo do `df`).
const RUN_TIMEOUT: Duration = Duration::from_secs(15);
/// Prazo da verificacao do kernel (os dois `timeout` de 20 s e o resto).
const KERNEL_TIMEOUT: Duration = Duration::from_secs(45);
/// Saida maior que isto: para de ler e usa o que ja chegou (um no de
/// Kubernetes lista centenas de montagens de conteineres no `df`).
const MAX_OUTPUT: usize = 1024 * 1024;
/// Pontos de montagem guardados por servidor.
const MAX_DISKS: usize = 32;
/// Tamanho maximo do nome de um ponto de montagem.
const MAX_MOUNT: usize = 128;
/// Interfaces de rede contadas por servidor.
const MAX_IFACES: usize = 64;
/// Tamanho maximo do nome de uma interface (o Linux aceita 15).
const MAX_IFNAME: usize = 32;
/// Linhas lidas da lista de kernels instalados e da de pacotes com
/// atualizacao (um `apt list` pode listar milhares).
const MAX_KERNEL_LINES: usize = 10_000;
/// Tamanho maximo de uma versao de kernel ou nome de pacote.
const MAX_RELEASE: usize = 96;
/// Pacotes do kernel no `dnf` (o `kernel` e so o metapacote, que nem sempre
/// esta instalado; o UEK e o do Oracle Linux).
const DNF_KERNELS: [&str; 3] = ["kernel", "kernel-core", "kernel-uek"];

const MARK: &str = "SAGUMON.";
const END: &[u8] = b"SAGUMON.end";

pub const KEY_NEW: &str = "Chave do servidor ainda não confirmada: conecte uma vez pelo \
     terminal para confirmá-la.";
pub const KEY_CHANGED: &str = "A chave do servidor mudou: conecte pelo terminal para \
     conferir a chave nova.";
pub const UNSUPPORTED: &str = "Servidor Windows ou equipamento de rede: o monitoramento só \
     funciona em Linux.";
pub const NO_DATA: &str = "Resposta não reconhecida: o monitoramento lê o /proc do Linux.";
/// Resposta sem os dados num servidor que ja respondeu antes (ex.: sem
/// memoria ou processos livres para rodar o `cat`).
pub const PARTIAL: &str = "Resposta incompleta do servidor.";
pub const NO_ANSWER: &str = "O servidor não respondeu ao comando de monitoramento.";
/// Junto do erro de credenciais: por que o cartao nao muda sozinho.
pub const AUTH_RETRY: &str = "O monitoramento não tenta de novo sozinho: corrija a conexão ou \
     clique em Atualizar agora.";
pub const CONNECT_TIMED_OUT: &str = "Tempo esgotado ao conectar.";

// ------------------------------------------------------------------ limites

/// CPU (%): atencao / critico.
pub const CPU_WARN: f32 = 80.0;
pub const CPU_CRIT: f32 = 95.0;
/// Memoria (%).
pub const MEM_WARN: f32 = 80.0;
pub const MEM_CRIT: f32 = 92.0;
/// Swap (%), so quando ha swap.
pub const SWAP_WARN: f32 = 50.0;
pub const SWAP_CRIT: f32 = 80.0;
/// Disco (%), por ponto de montagem.
pub const DISK_WARN: f32 = 80.0;
pub const DISK_CRIT: f32 = 90.0;
/// Load por CPU (o menor entre o de 1 e o de 5 min: ver `Sample::load_ratio`).
pub const LOAD_WARN: f32 = 1.0;
pub const LOAD_CRIT: f32 = 2.0;

/// Saude de uma medida (ou do servidor: a pior delas).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Crit,
}

pub fn level(value: f32, warn: f32, crit: f32) -> Level {
    if value >= crit {
        Level::Crit
    } else if value >= warn {
        Level::Warn
    } else {
        Level::Ok
    }
}

// ------------------------------------------------------------------ dados

/// Memoria ou swap, em KiB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Usage {
    pub total_kb: u64,
    pub used_kb: u64,
}

impl Usage {
    pub fn pct(&self) -> f32 {
        pct(self.used_kb, self.total_kb)
    }
}

/// Um sistema de arquivos montado (linha do `df -P -k`), em KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disk {
    pub mount: String,
    pub size_kb: u64,
    pub used_kb: u64,
    pub avail_kb: u64,
}

impl Disk {
    /// Uso como o `df` mostra: usado / (usado + livre), sem o reservado ao root.
    pub fn pct(&self) -> f32 {
        pct(self.used_kb, self.used_kb.saturating_add(self.avail_kb))
    }
}

/// Trafego de rede somado das interfaces contadas, em bytes por segundo.
#[derive(Clone, Debug, PartialEq)]
pub struct NetRate {
    /// Recebido pelo servidor (download).
    pub rx: f64,
    /// Enviado pelo servidor (upload).
    pub tx: f64,
    /// Periodo da media, em segundos.
    pub secs: f64,
}

/// Uma leitura do `/proc/net/dev`: hora pelo relogio do servidor (segundos
/// desde o boot) e, por interface contada, os bytes recebidos e enviados.
#[derive(Clone, Debug, PartialEq)]
pub struct NetReading {
    pub at: f64,
    pub ifaces: Vec<(String, u64, u64)>,
}

/// Trafego entre duas leituras, pelas interfaces presentes nas duas; `None`
/// se o relogio nao andou ou voltou (reboot), ou se nenhum contador andou
/// para a frente (contador zerado).
fn net_between(a: &NetReading, b: &NetReading) -> Option<NetRate> {
    let secs = b.at - a.at;
    if secs.is_nan() || secs < 0.2 {
        return None;
    }
    let (mut rx, mut tx, mut n) = (0u64, 0u64, 0usize);
    for (name, r1, t1) in &b.ifaces {
        let Some((_, r0, t0)) = a.ifaces.iter().find(|(n0, ..)| n0 == name) else {
            continue;
        };
        if let (Some(dr), Some(dt)) = (r1.checked_sub(*r0), t1.checked_sub(*t0)) {
            rx = rx.saturating_add(dr);
            tx = tx.saturating_add(dt);
            n += 1;
        }
    }
    (n > 0).then(|| NetRate {
        rx: rx as f64 / secs,
        tx: tx as f64 / secs,
        secs,
    })
}

/// Resultado da verificacao do kernel (ver [`KERNEL_COMMAND`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct KernelCheck {
    /// Kernel em uso (`uname -r`).
    pub running: String,
    /// Kernel mais novo que o em uso ja instalado: falta reiniciar.
    pub installed: Option<String>,
    /// Pacote do kernel com versao mais nova nos repositorios (nome e versao).
    pub available: Option<(String, String)>,
}

impl KernelCheck {
    pub fn has_update(&self) -> bool {
        self.installed.is_some() || self.available.is_some()
    }
}

fn pct(part: u64, whole: u64) -> f32 {
    if whole == 0 {
        0.0
    } else {
        (part as f64 * 100.0 / whole as f64).clamp(0.0, 100.0) as f32
    }
}

/// Uma coleta de um servidor. Cada campo e opcional: o que faltar na
/// resposta so deixa a medida sem valor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sample {
    /// Load de 1, 5 e 15 minutos.
    pub load: Option<[f32; 3]>,
    /// Processos (total do /proc/loadavg).
    pub procs: Option<u32>,
    pub cpus: Option<u32>,
    /// Uso de CPU (%): media desde a coleta anterior; na primeira (ou depois
    /// de um reboot), o do ultimo segundo.
    pub cpu_pct: Option<f32>,
    /// Contadores da ultima linha `cpu` do /proc/stat (total, ocioso), para a
    /// media da proxima coleta.
    pub cpu_ticks: Option<(u64, u64)>,
    pub mem: Option<Usage>,
    /// `None` = sem swap ou sem dado.
    pub swap: Option<Usage>,
    pub uptime_secs: Option<u64>,
    /// Do mais cheio ao mais vazio.
    pub disks: Vec<Disk>,
    /// O `df` desta coleta nao respondeu (travado e morto, ou ausente):
    /// `disks` e a leitura anterior (ou vazio, se nunca houve uma).
    pub disks_stale: bool,
    /// Trafego de rede: media desde a coleta anterior; na primeira (ou
    /// depois de um reboot), a do ultimo segundo.
    pub net: Option<NetRate>,
    /// Ultima leitura dos contadores de rede, para a media da proxima coleta.
    pub net_counters: Option<NetReading>,
}

impl Sample {
    /// Carga de agora por CPU: o menor entre o load de 1 e o de 5 min. So
    /// acusa carga alta que ja dura alguns minutos (um pico curto sobe o de 1
    /// min, nao o de 5) e que continua (depois que a carga cai, o de 1 min
    /// desce abaixo do limite em 1 ou 2 min; o de 5 levaria de 3 a 8 min,
    /// conforme o pico, e o cartao seguiria critico com o problema resolvido).
    pub fn load_ratio(&self) -> Option<f32> {
        let cpus = self.cpus.filter(|&c| c > 0)?;
        let [l1, l5, _] = self.load?;
        Some(l1.min(l5) / cpus as f32)
    }

    pub fn cpu_level(&self) -> Level {
        self.cpu_pct.map_or(Level::Ok, |p| level(p, CPU_WARN, CPU_CRIT))
    }

    pub fn mem_level(&self) -> Level {
        self.mem.map_or(Level::Ok, |m| level(m.pct(), MEM_WARN, MEM_CRIT))
    }

    pub fn swap_level(&self) -> Level {
        self.swap.map_or(Level::Ok, |s| level(s.pct(), SWAP_WARN, SWAP_CRIT))
    }

    pub fn load_level(&self) -> Level {
        self.load_ratio().map_or(Level::Ok, |r| level(r, LOAD_WARN, LOAD_CRIT))
    }

    pub fn disk_level(&self) -> Level {
        self.disks
            .first()
            .map_or(Level::Ok, |d| level(d.pct(), DISK_WARN, DISK_CRIT))
    }

    /// Saude geral: a pior das medidas.
    pub fn health(&self) -> Level {
        [
            self.cpu_level(),
            self.mem_level(),
            self.swap_level(),
            self.load_level(),
            self.disk_level(),
        ]
        .into_iter()
        .max()
        .unwrap_or(Level::Ok)
    }

    /// Medidas fora do normal, com o nivel de cada uma, para a dica do cartao.
    pub fn alerts(&self) -> Vec<(String, Level)> {
        let mut out = Vec::new();
        if let (Some(p), l) = (self.cpu_pct, self.cpu_level()) {
            if l != Level::Ok {
                out.push((format!("CPU em {p:.0}%"), l));
            }
        }
        if let (Some(m), l) = (self.mem, self.mem_level()) {
            if l != Level::Ok {
                out.push((format!("Memória em {:.0}%", m.pct()), l));
            }
        }
        if let (Some(s), l) = (self.swap, self.swap_level()) {
            if l != Level::Ok {
                out.push((format!("Swap em {:.0}%", s.pct()), l));
            }
        }
        if let (Some(load), l) = (self.load, self.load_level()) {
            if l != Level::Ok {
                let num = |v: f32| format!("{v:.2}").replace('.', ",");
                let text = format!(
                    "Load de {} (1 min) e {} (5 min) para {} CPU(s)",
                    num(load[0]),
                    num(load[1]),
                    self.cpus.unwrap_or(0)
                );
                out.push((text, l));
            }
        }
        for d in &self.disks {
            let l = level(d.pct(), DISK_WARN, DISK_CRIT);
            if l != Level::Ok {
                out.push((format!("Disco {} em {:.0}%", d.mount, d.pct()), l));
            }
        }
        out
    }
}

/// Por que uma coleta falhou (a UI mostra cada tipo de um jeito).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailKind {
    /// Conexao, prazo ou resposta: tenta de novo na proxima coleta.
    Error,
    /// Chave do servidor nao confirmada ou diferente da guardada.
    Key,
    /// Credenciais recusadas ou chave privada que nao abre: so tenta de novo
    /// com "Atualizar agora" ou com o host editado.
    Auth,
    /// Windows ou equipamento de rede (pela identificacao do servidor):
    /// nunca vai ter dados, nem tenta de novo.
    Unsupported,
    /// O comando rodou ate o fim mas a resposta nao trouxe os dados (sem
    /// /proc: macOS, BSD). So tenta de novo com "Atualizar agora".
    NoData,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub kind: FailKind,
    pub msg: String,
}

impl Failure {
    pub fn new(kind: FailKind, msg: impl Into<String>) -> Self {
        Failure {
            kind,
            msg: msg.into(),
        }
    }
}

/// Resultado de uma coleta para a UI.
#[derive(Debug)]
pub struct MonitorEvent {
    pub host_id: uuid::Uuid,
    /// Servidor coletado (ver `target`): a UI descarta o resultado que chega
    /// depois de o host passar a apontar para outro lugar.
    pub target: Target,
    /// Pedido mais novo que esta coleta atendeu (ver `Request::seq`).
    pub seq: u64,
    pub result: Result<Sample, Failure>,
    /// Falha repetida sem tentar de novo (ver `blocks`): nada mudou desde a
    /// anterior.
    pub replayed: bool,
}

/// Verificacao do kernel de um host para a UI (uma por conexao).
#[derive(Debug)]
pub struct KernelEvent {
    pub host_id: uuid::Uuid,
    /// Como em `MonitorEvent::target`.
    pub target: Target,
    /// `None`: o servidor nao respondeu ao comando.
    pub check: Option<KernelCheck>,
}

/// O que a tarefa de um host manda para a UI.
#[derive(Debug)]
pub enum Event {
    Sample(MonitorEvent),
    Kernel(KernelEvent),
}

impl From<MonitorEvent> for Event {
    fn from(ev: MonitorEvent) -> Self {
        Event::Sample(ev)
    }
}

/// Pedido de coleta da UI.
#[derive(Debug)]
pub struct Request {
    pub hosts: Vec<Host>,
    /// Numero crescente do pedido: a UI so da por atendido o host cujo
    /// resultado ja responde a este pedido (um resultado de um pedido antigo
    /// nao apaga o "Coletando...").
    pub seq: u64,
    /// "Atualizar agora": tenta de novo tambem os hosts com credenciais
    /// recusadas.
    pub force: bool,
}

/// Aviso de coleta para a tarefa de um host.
#[derive(Clone, Copy)]
struct Tick {
    seq: u64,
    force: bool,
}

/// Endereco (minusculas, sem espacos nas pontas) e porta: identifica o
/// servidor cujos dados o cartao mostra.
pub type Target = (String, u16);

pub fn target(h: &Host) -> Target {
    (h.host.trim().to_ascii_lowercase(), h.port)
}

// ------------------------------------------------------------------ worker

/// Lado da UI do monitoramento. Soltar o handle encerra a thread e as conexoes.
pub struct MonitorHandle {
    tx: UnboundedSender<Request>,
    pub rx: std::sync::mpsc::Receiver<Event>,
}

impl MonitorHandle {
    /// Uma coleta em cada host da lista; hosts que sairam da lista sao
    /// desconectados, e os que mudaram (endereco, usuario, credenciais ou
    /// chave do servidor) reconectam.
    pub fn refresh(&self, hosts: Vec<Host>, seq: u64, force: bool) {
        let _ = self.tx.send(Request { hosts, seq, force });
    }
}

/// Handle ligado a canais de teste: devolve tambem o lado "worker" (as
/// listas pedidas pela UI e por onde mandar resultados).
#[cfg(test)]
pub fn test_handle() -> (
    MonitorHandle,
    UnboundedReceiver<Request>,
    std::sync::mpsc::Sender<Event>,
) {
    let (tx, cmds) = tokio::sync::mpsc::unbounded_channel();
    let (events, rx) = std::sync::mpsc::channel();
    (MonitorHandle { tx, rx }, cmds, events)
}

/// Inicia o monitoramento (ainda sem coletar: ver `MonitorHandle::refresh`).
/// `repaint` e chamado a cada resultado.
pub fn start<F>(repaint: F) -> MonitorHandle
where
    F: Fn() + Send + Sync + 'static,
{
    let (tx, cmds) = tokio::sync::mpsc::unbounded_channel::<Request>();
    let (events, rx) = std::sync::mpsc::channel::<Event>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(run(cmds, events, Arc::new(repaint)));
    });
    MonitorHandle { tx, rx }
}

/// Tarefa de um host: o host com que foi criada e por onde recebe as coletas.
struct Watcher {
    host: Host,
    tick: UnboundedSender<Tick>,
    abort: AbortHandle,
}

type Repaint = Arc<dyn Fn() + Send + Sync>;

async fn run(
    mut cmds: UnboundedReceiver<Request>,
    events: std::sync::mpsc::Sender<Event>,
    repaint: Repaint,
) {
    let mut tasks: JoinSet<()> = JoinSet::new();
    let mut watchers: HashMap<uuid::Uuid, Watcher> = HashMap::new();
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                let Some(Request { hosts, seq, force }) = cmd else { break };
                // Quem saiu da lista (excluido ou desligado) cai fora.
                watchers.retain(|id, w| {
                    let keep = hosts.iter().any(|h| h.id == *id);
                    if !keep {
                        w.abort.abort();
                    }
                    keep
                });
                for host in hosts {
                    let same = watchers.get(&host.id).is_some_and(|w| same_target(&w.host, &host));
                    if !same {
                        if let Some(old) = watchers.remove(&host.id) {
                            old.abort.abort();
                        }
                        let (tick, ticks) = tokio::sync::mpsc::unbounded_channel();
                        let abort = tasks.spawn(watch(
                            host.clone(),
                            ticks,
                            events.clone(),
                            Arc::clone(&repaint),
                        ));
                        watchers.insert(host.id, Watcher { host, tick, abort });
                    }
                }
                for w in watchers.values() {
                    let _ = w.tick.send(Tick { seq, force });
                }
            }
            // Recolhe tarefas terminadas (abortadas).
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    // O drop do JoinSet aborta as tarefas; as conexoes caem com elas.
}

/// Mesmo destino e mesmas credenciais: a conexao aberta continua valendo.
pub fn same_target(a: &Host, b: &Host) -> bool {
    let auth = match (&a.auth, &b.auth) {
        (AuthMethod::Password { password: x }, AuthMethod::Password { password: y }) => x == y,
        (
            AuthMethod::Key {
                private_key: k1,
                passphrase: p1,
            },
            AuthMethod::Key {
                private_key: k2,
                passphrase: p2,
            },
        ) => k1 == k2 && p1 == p2,
        _ => false,
    };
    auth && a.host == b.host
        && a.port == b.port
        && a.username == b.username
        && a.host_key == b.host_key
}

/// Laco de um host: uma coleta por aviso da UI, na mesma conexao, e a
/// verificacao do kernel logo depois da primeira coleta que deu certo em
/// cada conexao (um reboot derruba a conexao: a seguinte verifica de novo).
async fn watch(
    host: Host,
    mut ticks: UnboundedReceiver<Tick>,
    events: std::sync::mpsc::Sender<Event>,
    repaint: Repaint,
) {
    let mut conn: Option<client::Handle<Client>> = None;
    // Falha que nao adianta repetir a cada minuto (ver `blocks`).
    let mut blocked: Option<Failure> = None;
    let mut carry = Carry::default();
    // Conexao aberta ainda sem a verificacao do kernel.
    let mut kernel_due = false;
    while let Some(mut tick) = ticks.recv().await {
        // Avisos acumulados durante uma coleta lenta valem uma coleta so.
        while let Ok(t) = ticks.try_recv() {
            tick = Tick {
                seq: tick.seq.max(t.seq),
                force: tick.force || t.force,
            };
        }
        let replayed = blocked
            .as_ref()
            .is_some_and(|f| f.kind == FailKind::Unsupported || !tick.force);
        let result = match &blocked {
            Some(f) if replayed => Err(f.clone()),
            _ => {
                let fresh = conn.as_ref().is_none_or(|c| c.is_closed());
                let r = sample(&host, &mut conn).await;
                kernel_due |= fresh && conn.is_some();
                r
            }
        };
        let result = match result {
            // Ja respondeu antes: e um problema passageiro, nao falta de
            // suporte (tenta de novo na proxima coleta).
            Err(f) if f.kind == FailKind::NoData && carry.seen => {
                Err(Failure::new(FailKind::Error, PARTIAL))
            }
            r => r,
        };
        blocked = result.as_ref().err().filter(|f| blocks(f.kind)).cloned();
        if blocked.is_some() {
            // Nada a fazer com a conexao ate um "Atualizar agora".
            conn = None;
        }
        let result = result.map(|mut s| {
            carry.apply(&mut s);
            s
        });
        let ok = result.is_ok();
        let ev = MonitorEvent {
            host_id: host.id,
            target: target(&host),
            seq: tick.seq,
            result,
            replayed,
        };
        if events.send(ev.into()).is_err() {
            break;
        }
        repaint();
        // Depois de mandar a coleta: o cartao nao espera pelo `dnf`.
        if let (true, true, Some(session)) = (ok, kernel_due, conn.as_ref()) {
            kernel_due = false;
            let ev = KernelEvent {
                host_id: host.id,
                target: target(&host),
                check: check_kernel(session).await,
            };
            if events.send(Event::Kernel(ev)).is_err() {
                break;
            }
            repaint();
        }
    }
}

/// Roda o [`KERNEL_COMMAND`]; `None` se nada chegou.
async fn check_kernel(session: &client::Handle<Client>) -> Option<KernelCheck> {
    let reply = run_command(session, KERNEL_COMMAND, KERNEL_TIMEOUT).await?;
    parse_kernel(whole_lines(&reply.out, reply.size_cut))
}

/// Falhas que a tarefa nao repete sozinha: servidor sem suporte (nunca
/// muda), sem /proc e credenciais recusadas (cada tentativa e um login falho
/// no servidor). As duas ultimas voltam a tentar com "Atualizar agora".
fn blocks(kind: FailKind) -> bool {
    matches!(kind, FailKind::Unsupported | FailKind::NoData | FailKind::Auth)
}

/// Tipo e texto da falha de conexao/autenticacao.
fn connect_failure(e: &anyhow::Error) -> Failure {
    let msg = format!("{e:#}");
    if msg == KEY_NEW || msg == KEY_CHANGED {
        Failure::new(FailKind::Key, msg)
    } else if msg == ssh::AUTH_FAILED || msg.starts_with(ssh::KEY_INVALID) {
        Failure::new(FailKind::Auth, format!("{}. {AUTH_RETRY}", first_upper(&msg)))
    } else {
        Failure::new(FailKind::Error, first_upper(&msg))
    }
}

/// Uma coleta: conecta se preciso e roda o comando.
async fn sample(
    host: &Host,
    conn: &mut Option<client::Handle<Client>>,
) -> Result<Sample, Failure> {
    if conn.as_ref().is_none_or(|c| c.is_closed()) {
        *conn = None;
        // Nunca pergunta pela chave aqui: sem chave confirmada, desiste antes
        // de qualquer credencial sair (ver `ssh::connect_and_auth`).
        let changed = host.host_key.is_some();
        let ask = move |p: HostKeyPrompt| {
            let msg = if changed { KEY_CHANGED } else { KEY_NEW };
            let _ = p.reply.send(HostKeyAnswer::Cancel(msg.to_string()));
        };
        let (session, banner) = timeout(CONNECT_TIMEOUT, ssh::connect_and_auth(host, ask))
            .await
            .map_err(|_| Failure::new(FailKind::Error, CONNECT_TIMED_OUT))?
            .map_err(|e| connect_failure(&e))?;
        if banner == Banner::Skip {
            return Err(Failure::new(FailKind::Unsupported, UNSUPPORTED));
        }
        *conn = Some(session);
    }
    let Some(session) = conn.as_ref() else {
        return Err(Failure::new(FailKind::Error, NO_ANSWER));
    };
    let Some(reply) = run_command(session, COMMAND, RUN_TIMEOUT).await else {
        // Exec recusado, nada no stdout ou prazo esgotado. A conexao so e
        // refeita se caiu (um login novo por minuto num servidor que nunca
        // responde ao comando encheria o log dele).
        if session.is_closed() {
            *conn = None;
        }
        return Err(Failure::new(FailKind::Error, NO_ANSWER));
    };
    match parse_reply(&reply.out, reply.size_cut) {
        Some(s) => Ok(s),
        // Cortado pelo prazo (servidor sobrecarregado): passageiro.
        None if reply.timed_out => Err(Failure::new(FailKind::Error, NO_ANSWER)),
        None => Err(Failure::new(FailKind::NoData, NO_DATA)),
    }
}

/// O que passa de uma coleta para a seguinte do mesmo servidor.
#[derive(Default)]
struct Carry {
    /// Ja houve uma coleta que deu certo neste servidor.
    seen: bool,
    /// Contadores de CPU da coleta anterior.
    ticks: Option<(u64, u64)>,
    /// Discos da ultima coleta em que o `df` respondeu.
    disks: Option<Vec<Disk>>,
    /// Contadores de rede da coleta anterior.
    net: Option<NetReading>,
}

impl Carry {
    /// CPU pela media desde a coleta anterior: uma janela de 1 s no mesmo
    /// segundo de cada minuto coincidiria com um cron (`* * * * *`) e
    /// acusaria critico a cada coleta, ou nunca o veria. Sem `df` nesta
    /// coleta, os discos da anterior continuam valendo (marcados): sumir com
    /// eles tiraria o disco da conta da saude. A rede segue a mesma media da
    /// CPU.
    fn apply(&mut self, s: &mut Sample) {
        self.seen = true;
        if let (Some(prev), Some(cur)) = (self.ticks, s.cpu_ticks) {
            if let Some(p) = cpu_between(prev, cur) {
                s.cpu_pct = Some(p);
            }
        }
        if s.cpu_ticks.is_some() {
            self.ticks = s.cpu_ticks;
        }
        if let (Some(prev), Some(cur)) = (&self.net, &s.net_counters) {
            if let Some(r) = net_between(prev, cur) {
                s.net = Some(r);
            }
        }
        if s.net_counters.is_some() {
            self.net = s.net_counters.clone();
        }
        if s.disks_stale {
            s.disks = self.disks.clone().unwrap_or_default();
        } else {
            self.disks = Some(s.disks.clone());
        }
    }
}

/// Uso de CPU (%) entre dois pares (total, ocioso) do /proc/stat; `None` se
/// os contadores nao andaram ou voltaram (reboot).
fn cpu_between((t0, i0): (u64, u64), (t1, i1): (u64, u64)) -> Option<f32> {
    let dt = t1.checked_sub(t0)?;
    let di = i1.checked_sub(i0)?;
    if dt == 0 || di > dt {
        return None;
    }
    Some(((dt - di) as f64 * 100.0 / dt as f64) as f32)
}

fn first_upper(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// Resposta do `COMMAND`.
struct Reply {
    out: Vec<u8>,
    /// Parou de ler por `MAX_OUTPUT`: o que chegou e valido, so incompleto
    /// (diferente de um corte pelo prazo, em que o `df` nao respondeu).
    size_cut: bool,
    /// Parou pelo prazo.
    timed_out: bool,
}

/// Roda `command` ([`COMMAND`] ou [`KERNEL_COMMAND`]) num canal proprio.
/// Devolve o stdout ate o marcador final, o fim do canal, o prazo `limit` ou
/// `MAX_OUTPUT` (o que vier antes); `None` se o canal nao abriu, o exec foi
/// recusado ou nada chegou.
async fn run_command(
    session: &client::Handle<Client>,
    command: &str,
    limit: Duration,
) -> Option<Reply> {
    let channel = timeout(OPEN_TIMEOUT, session.channel_open_session())
        .await
        .ok()?
        .ok()?;
    let (mut rd, wr) = channel.split();
    let mut out = Vec::new();
    let mut size_cut = false;
    let done = timeout(limit, async {
        wr.exec(true, command).await.ok()?;
        // Stdin fechado: um programa que espere entrada recebe EOF e sai.
        wr.eof().await.ok()?;
        loop {
            match rd.wait().await {
                Some(ChannelMsg::Data { data }) => {
                    if out.len() + data.len() > MAX_OUTPUT {
                        size_cut = true;
                        return Some(());
                    }
                    out.extend_from_slice(&data);
                    if ends_with_end(&out) {
                        return Some(());
                    }
                }
                Some(ChannelMsg::Failure) => return None,
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => return Some(()),
                Some(_) => {}
            }
        }
    })
    .await;
    // Como na sonda do SO: a leitura sai antes do close (um receptor vivo e
    // nao drenado trava a conexao inteira).
    drop(rd);
    let _ = wr.close().await;
    let timed_out = done.is_err();
    match done {
        Ok(None) => None,
        _ => (!out.is_empty()).then_some(Reply {
            out,
            size_cut,
            timed_out,
        }),
    }
}

fn ends_with_end(out: &[u8]) -> bool {
    let mut t = out;
    while let Some((&b, rest)) = t.split_last() {
        if b == b'\n' || b == b'\r' {
            t = rest;
        } else {
            break;
        }
    }
    t.ends_with(END) && (t.len() == END.len() || matches!(t[t.len() - END.len() - 1], b'\n' | b'\r'))
}

// ------------------------------------------------------------------ parser

#[derive(Default)]
struct Sections {
    load: Vec<String>,
    cpus: Vec<String>,
    mem: Vec<String>,
    up: Vec<String>,
    stat: Vec<String>,
    /// `/proc/net/dev` antes e depois do `sleep 1`, e a hora da segunda.
    net: Vec<String>,
    up2: Vec<String>,
    net2: Vec<String>,
    /// `ls -l /sys/class/net`.
    nic: Vec<String>,
    df: Vec<String>,
    // Verificacao do kernel (`KERNEL_COMMAND`).
    kver: Vec<String>,
    kmods: Vec<String>,
    kboot: Vec<String>,
    kpkg: Vec<String>,
    /// O marcador final chegou (a resposta nao foi cortada pelo prazo).
    end: bool,
}

fn split_sections(out: &[u8]) -> Sections {
    let text = String::from_utf8_lossy(out);
    let mut s = Sections::default();
    let mut cur: Option<&mut Vec<String>> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.trim().strip_prefix(MARK) {
            cur = match name {
                "load" => Some(&mut s.load),
                "cpus" => Some(&mut s.cpus),
                "mem" => Some(&mut s.mem),
                "up" => Some(&mut s.up),
                "stat" => Some(&mut s.stat),
                "net" => Some(&mut s.net),
                "up2" => Some(&mut s.up2),
                "net2" => Some(&mut s.net2),
                "nic" => Some(&mut s.nic),
                "df" => Some(&mut s.df),
                "kver" => Some(&mut s.kver),
                "kmods" => Some(&mut s.kmods),
                "kboot" => Some(&mut s.kboot),
                "kpkg" => Some(&mut s.kpkg),
                "end" => {
                    s.end = true;
                    None
                }
                _ => None,
            };
            continue;
        }
        if let Some(v) = cur.as_deref_mut() {
            v.push(line.to_string());
        }
    }
    s
}

/// `parse_reply` de uma resposta lida ate o fim (testes).
#[cfg(test)]
fn parse_output(out: &[u8]) -> Option<Sample> {
    parse_reply(out, false)
}

/// Saida cortada por `MAX_OUTPUT` (`size_cut`): sem a ultima linha, talvez
/// pela metade.
fn whole_lines(out: &[u8], size_cut: bool) -> &[u8] {
    if size_cut {
        out.iter()
            .rposition(|&b| b == b'\n')
            .map_or(&out[..0], |i| &out[..=i])
    } else {
        out
    }
}

/// Le a saida do `COMMAND`. `None` se nao veio nada reconhecivel (sem load
/// nem memoria: provavelmente nao e Linux). `size_cut` = a leitura parou por
/// `MAX_OUTPUT` (a ultima linha, talvez pela metade, e descartada e o resto
/// vale).
fn parse_reply(out: &[u8], size_cut: bool) -> Option<Sample> {
    let s = split_sections(whole_lines(out, size_cut));
    let rows = df_rows(&s.df);
    let (load, procs) = parse_loadavg(&s.load);
    let (mem, swap) = parse_meminfo(&s.mem);
    if load.is_none() && mem.is_none() {
        return None;
    }
    let phys = physical_ifaces(&s.nic);
    let net0 = net_reading(&s.net, parse_uptime(&s.up), &phys);
    let net1 = net_reading(&s.net2, parse_uptime(&s.up2), &phys);
    let net = match (&net0, &net1) {
        (Some(a), Some(b)) => net_between(a, b),
        _ => None,
    };
    Some(Sample {
        load,
        procs,
        cpus: s
            .cpus
            .iter()
            .find_map(|l| l.trim().parse::<u32>().ok())
            .filter(|&c| c > 0),
        cpu_pct: parse_stat(&s.stat),
        cpu_ticks: stat_ticks(&s.stat).last().copied(),
        mem,
        swap,
        uptime_secs: parse_uptime(&s.up).map(|secs| secs as u64),
        // Sem o cabecalho do `df` ate o marcador final (ou ate o corte por
        // tamanho), ele nao respondeu.
        disks_stale: !((s.end || size_cut) && rows.is_some()),
        disks: rows.map(parse_df).unwrap_or_default(),
        net,
        net_counters: net1.or(net0),
    })
}

fn finite(v: f32) -> Option<f32> {
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// `/proc/uptime` (`93784.52 180000.00`): segundos desde o boot.
fn parse_uptime(lines: &[String]) -> Option<f64> {
    let secs = lines.first()?.split_whitespace().next()?.parse::<f64>().ok()?;
    (secs.is_finite() && secs >= 0.0).then_some(secs)
}

/// Interfaces fisicas pelo `ls -l /sys/class/net`: cada uma e um link para o
/// dispositivo, e as virtuais (lo, bridge, veth, docker, bond, VLAN, tun,
/// wireguard) apontam para `devices/virtual`. Contar so as fisicas evita somar
/// o mesmo trafego duas vezes (a bridge e a placa por baixo dela, o bond e
/// as placas dele).
fn physical_ifaces(lines: &[String]) -> Vec<&str> {
    lines
        .iter()
        .filter_map(|l| {
            let (left, dest) = l.split_once(" -> ")?;
            let name = left.split_whitespace().last()?;
            (!dest.contains("/virtual/")).then_some(name)
        })
        .collect()
}

/// Uma leitura do `/proc/net/dev` (`  eth0: 1234 56 0 0 0 0 0 0 7890 ...`:
/// bytes recebidos no 1o numero, enviados no 9o), so com as interfaces de
/// `phys`. Sem nenhuma fisica (conteiner, em que a placa e um veth; ou sem o
/// `/sys`), todas menos a `lo`.
fn net_reading(lines: &[String], at: Option<f64>, phys: &[&str]) -> Option<NetReading> {
    let at = at?;
    let mut ifaces: Vec<(String, u64, u64)> = Vec::new();
    for l in lines {
        let Some((name, rest)) = l.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let counted = if phys.is_empty() {
            name != "lo"
        } else {
            phys.contains(&name)
        };
        if !counted {
            continue;
        }
        let f: Vec<&str> = rest.split_whitespace().collect();
        let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
        let (Some(rx), Some(tx)) = (num(0), num(8)) else {
            continue;
        };
        let Some(name) = osinfo::clean(name, MAX_IFNAME) else {
            continue;
        };
        if ifaces.iter().any(|(n, ..)| *n == name) {
            continue;
        }
        ifaces.push((name, rx, tx));
        if ifaces.len() >= MAX_IFACES {
            break;
        }
    }
    (!ifaces.is_empty()).then_some(NetReading { at, ifaces })
}

/// `0.52 0.58 0.59 1/389 12345`.
fn parse_loadavg(lines: &[String]) -> (Option<[f32; 3]>, Option<u32>) {
    let Some(line) = lines.first() else {
        return (None, None);
    };
    let f: Vec<&str> = line.split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<f32>().ok()).and_then(finite);
    let load = match (num(0), num(1), num(2)) {
        (Some(a), Some(b), Some(c)) => Some([a, b, c]),
        _ => None,
    };
    let procs = f
        .get(3)
        .and_then(|v| v.split('/').nth(1))
        .and_then(|v| v.parse::<u32>().ok());
    (load, procs)
}

/// `/proc/meminfo` (kB). Sem `MemAvailable` (kernel antigo): livre + buffers
/// + cache.
fn parse_meminfo(lines: &[String]) -> (Option<Usage>, Option<Usage>) {
    let mut kv: HashMap<&str, u64> = HashMap::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        if let Some(n) = v.split_whitespace().next().and_then(|n| n.parse::<u64>().ok()) {
            kv.insert(k.trim(), n);
        }
    }
    let mem = kv.get("MemTotal").filter(|&&t| t > 0).map(|&total| {
        let avail = kv.get("MemAvailable").copied().unwrap_or_else(|| {
            ["MemFree", "Buffers", "Cached"]
                .iter()
                .filter_map(|k| kv.get(k))
                .fold(0u64, |a, &b| a.saturating_add(b))
        });
        Usage {
            total_kb: total,
            used_kb: total.saturating_sub(avail.min(total)),
        }
    });
    let swap = match (kv.get("SwapTotal"), kv.get("SwapFree")) {
        (Some(&total), Some(&free)) if total > 0 => Some(Usage {
            total_kb: total,
            used_kb: total.saturating_sub(free.min(total)),
        }),
        _ => None,
    };
    (mem, swap)
}

/// Pares (total, ocioso) de cada linha `cpu ...` do `/proc/stat`.
fn stat_ticks(lines: &[String]) -> Vec<(u64, u64)> {
    lines
        .iter()
        .filter(|l| l.starts_with("cpu "))
        .filter_map(|l| {
            let v: Vec<u64> = l
                .split_whitespace()
                .skip(1)
                .take(8)
                .map(|n| n.parse::<u64>().ok())
                .collect::<Option<_>>()?;
            if v.len() < 4 {
                return None;
            }
            // user nice system idle iowait irq softirq steal
            let total = v.iter().fold(0u64, |a, &b| a.saturating_add(b));
            let idle = v[3].saturating_add(v.get(4).copied().unwrap_or(0));
            Some((total, idle))
        })
        .collect()
}

/// Duas linhas `cpu ...` do `/proc/stat` com 1 s entre elas: uso no periodo.
fn parse_stat(lines: &[String]) -> Option<f32> {
    let times = stat_ticks(lines);
    let [a, b] = times.get(..2)? else {
        return None;
    };
    cpu_between(*a, *b)
}

/// Origens que nao sao disco de verdade (memoria, pseudo-sistemas, snaps,
/// CD/ISO).
fn pseudo_fs(src: &str) -> bool {
    const PSEUDO: [&str; 12] = [
        "tmpfs", "devtmpfs", "udev", "overlay", "shm", "none", "efivarfs", "cgroup",
        "cgroup2", "proc", "sysfs", "squashfs",
    ];
    // `/dev/sr*`: CD/ISO montado (repositorio local), sempre 100% cheio.
    PSEUDO.contains(&src) || src.starts_with("/dev/loop") || src.starts_with("/dev/sr")
}

/// Linhas do `df` depois do cabecalho, que e a primeira linha nao vazia,
/// qualquer que seja o idioma; `None` se o `df` nao escreveu nada.
fn df_rows(lines: &[String]) -> Option<&[String]> {
    let i = lines.iter().position(|l| !l.trim().is_empty())?;
    Some(&lines[i + 1..])
}

/// `df -P -k`: origem, tamanho, usado, livre, %, ponto de montagem (que pode
/// ter espacos). Sem pseudo-sistemas e sem repetir a origem (bind mounts).
/// Os `MAX_DISKS` mais cheios (ordena antes de cortar: o disco cheio pode vir
/// depois de dezenas de outros).
fn parse_df(lines: &[String]) -> Vec<Disk> {
    let mut seen: Vec<String> = Vec::new();
    let mut disks = Vec::new();
    for l in lines {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 6 || pseudo_fs(f[0]) {
            continue;
        }
        let (Ok(size), Ok(used), Ok(avail)) =
            (f[1].parse::<u64>(), f[2].parse::<u64>(), f[3].parse::<u64>())
        else {
            continue;
        };
        if size == 0 || seen.iter().any(|s| s == f[0]) {
            continue;
        }
        let Some(mount) = osinfo::clean(&f[5..].join(" "), MAX_MOUNT) else {
            continue;
        };
        seen.push(f[0].to_string());
        disks.push(Disk {
            mount,
            size_kb: size,
            used_kb: used,
            avail_kb: avail,
        });
    }
    disks.sort_by(|a, b| b.pct().total_cmp(&a.pct()));
    disks.truncate(MAX_DISKS);
    disks
}

// ------------------------------------------------------------------ kernel

/// Versao de um kernel (`uname -r`) para comparar: os numeros do comeco
/// (`4.18.0-553.22.1` em `4.18.0-553.22.1.el8_10.x86_64`) e o "sabor", as
/// letras do resto (`elx`). So kernels do mesmo sabor se comparam: o de
/// depuracao (`+debug`), o `-lowlatency` e o do WSL nao sao atualizacao do
/// kernel em uso, e o numero da versao do RHEL (`el8_9`, `el8_10`) nao conta.
#[derive(Debug, PartialEq, Eq)]
struct KernelVer {
    nums: Vec<u64>,
    flavor: String,
}

impl KernelVer {
    fn parse(release: &str) -> Option<KernelVer> {
        let mut nums = Vec::new();
        let mut rest = "";
        let mut pos = 0;
        for tok in release.split(['.', '-', '_', '+', '~']) {
            if tok.is_empty() || !tok.bytes().all(|b| b.is_ascii_digit()) {
                rest = &release[pos..];
                break;
            }
            nums.push(tok.parse::<u64>().ok()?);
            pos = (pos + tok.len() + 1).min(release.len());
        }
        let flavor = rest
            .chars()
            .filter(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        (nums.len() >= 2).then_some(KernelVer { nums, flavor })
    }

    /// Mesmo sabor e numeros maiores (`4.18.0-553.22.1` > `4.18.0-553`).
    fn newer_than(&self, other: &KernelVer) -> bool {
        self.flavor == other.flavor && self.nums > other.nums
    }
}

/// Versao ou nome de pacote vindo do servidor: so letras, digitos e
/// `.-_+~:`, ate `MAX_RELEASE`.
fn clean_release(s: &str) -> Option<String> {
    let s = s.trim();
    let ok = !s.is_empty()
        && s.len() <= MAX_RELEASE
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b".-_+~:".contains(&b));
    ok.then(|| s.to_string())
}

/// Le a saida do `KERNEL_COMMAND`. `None` sem o `uname -r`.
fn parse_kernel(out: &[u8]) -> Option<KernelCheck> {
    let s = split_sections(out);
    let running = s.kver.iter().find_map(|l| clean_release(l))?;
    let mut check = KernelCheck {
        running,
        ..Default::default()
    };
    let Some(run) = KernelVer::parse(&check.running) else {
        return Some(check);
    };
    let mods: Vec<&str> = s.kmods.iter().take(MAX_KERNEL_LINES).map(|l| l.trim()).collect();
    // Sem a pasta do kernel em uso, ele nao e um dos instalados: num
    // conteiner (LXC, OpenVZ) o kernel e o do hospedeiro, e os pacotes de
    // kernel la dentro nao sao os que rodam.
    if !mods.contains(&check.running.as_str()) {
        return Some(check);
    }
    check.installed = mods
        .iter()
        .filter_map(|m| Some((KernelVer::parse(m)?, clean_release(m)?)))
        .filter(|(v, _)| v.newer_than(&run))
        .max_by(|a, b| a.0.nums.cmp(&b.0.nums))
        .map(|(_, m)| m);
    if check.installed.is_none() {
        // Debian/Ubuntu: `linux-image-6.1.0-13-amd64`, ate com a mesma versao
        // do em uso (pacote atualizado sem mudar o nome).
        check.installed = s
            .kboot
            .iter()
            .take(MAX_KERNEL_LINES)
            .filter_map(|l| l.trim().strip_prefix("linux-image-"))
            .filter(|r| KernelVer::parse(r).is_some_and(|v| v.flavor == run.flavor && v.nums >= run.nums))
            .find_map(clean_release);
    }
    check.available = parse_pkg_update(&s.kpkg, &run);
    Some(check)
}

/// Pacote do kernel com versao nova nos repositorios, pela saida do `dnf
/// check-update` (`kernel-core.x86_64  4.18.0-553.22.1.el8_10  baseos`, so se
/// for do sabor do kernel em uso e mais novo que ele) ou do `apt list
/// --upgradable` (`linux-image-generic/jammy-updates 5.15.0.92.89 amd64
/// [upgradable from: 5.15.0.91.88]`: a versao do metapacote nao e a do
/// `uname -r`, nao ha o que comparar).
fn parse_pkg_update(lines: &[String], run: &KernelVer) -> Option<(String, String)> {
    for l in lines.iter().take(MAX_KERNEL_LINES) {
        // dnf: o pacote instalado que um outro torna obsoleto vem recuado.
        if l.starts_with(char::is_whitespace) {
            continue;
        }
        let f: Vec<&str> = l.split_whitespace().collect();
        let (Some(&pkg), Some(ver)) = (f.first(), f.get(1).and_then(|v| clean_release(v))) else {
            continue;
        };
        let found = if let Some((name, arch)) =
            pkg.rsplit_once('.').filter(|(n, _)| DNF_KERNELS.contains(n))
        {
            let release = format!("{}.{arch}", ver.split_once(':').map_or(ver.as_str(), |(_, v)| v));
            KernelVer::parse(&release)
                .is_some_and(|v| v.newer_than(run))
                .then_some(name)
        } else {
            pkg.split_once('/')
                .map(|(name, _)| name)
                .filter(|name| name.starts_with("linux-image-") && l.contains("[upgradable from"))
        };
        if let Some(name) = found.and_then(clean_release) {
            return Some((name, ver));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "SAGUMON.load\n0.52 1.58 0.59 2/389 12345\nSAGUMON.cpus\n2\n1\n\
SAGUMON.mem\nMemTotal:        8000000 kB\nMemFree:          100000 kB\nMemAvailable:    2000000 kB\n\
SwapTotal:       1000000 kB\nSwapFree:         900000 kB\n\
SAGUMON.up\n93784.52 180000.00\n\
SAGUMON.stat\ncpu  100 0 100 800 0 0 0 0 0 0\ncpu  150 0 150 900 0 0 0 0 0 0\n\
SAGUMON.df\nFilesystem     1024-blocks      Used Available Capacity Mounted on\n\
/dev/sda1         10000000   9500000    500000      95% /\n\
tmpfs               100000         0    100000       0% /run\n\
/dev/loop3           60000     60000         0     100% /snap/core/1\n\
/dev/sdb1         20000000   2000000  18000000      10% /dados com espaco\n\
/dev/sda1         10000000   9500000    500000      95% /var/bind\n\
/dev/sr0             4000000   4000000         0     100% /mnt/cdrom\n\
SAGUMON.end\n";

    #[test]
    fn parses_full_output() {
        let s = parse_output(OUT.as_bytes()).unwrap();
        assert_eq!(s.load, Some([0.52, 1.58, 0.59]));
        assert_eq!(s.procs, Some(389));
        assert_eq!(s.cpus, Some(2));
        assert_eq!(s.mem, Some(Usage { total_kb: 8_000_000, used_kb: 6_000_000 }));
        assert_eq!(s.swap, Some(Usage { total_kb: 1_000_000, used_kb: 100_000 }));
        assert_eq!(s.uptime_secs, Some(93784));
        // (150+150) de 400 no periodo: 50%.
        assert_eq!(s.cpu_pct, Some(50.0));
        assert_eq!(s.cpu_ticks, Some((1200, 900)));
        assert!(!s.disks_stale);
        // Sem tmpfs, snap, bind repetido nem a ISO montada.
        let mounts: Vec<&str> = s.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/", "/dados com espaco"]);
        assert_eq!(s.disks[0].pct(), 95.0);
        assert_eq!(s.mem_level(), Level::Ok);
        assert_eq!(s.disk_level(), Level::Crit);
        // min(0,52; 1,58) / 2 CPUs.
        assert_eq!(s.load_ratio(), Some(0.26));
        assert_eq!(s.health(), Level::Crit);
        assert_eq!(s.alerts(), [("Disco / em 95%".to_string(), Level::Crit)]);
    }

    #[test]
    fn old_kernel_without_memavailable_and_no_swap() {
        let out = "SAGUMON.mem\nMemTotal: 1000 kB\nMemFree: 200 kB\nBuffers: 100 kB\n\
Cached: 200 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\nSAGUMON.end\n";
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.mem, Some(Usage { total_kb: 1000, used_kb: 500 }));
        assert_eq!(s.swap, None);
        assert_eq!(s.load, None);
        assert_eq!(s.cpu_pct, None);
        assert!(s.disks.is_empty());
    }

    #[test]
    fn non_linux_gives_none() {
        let out = "SAGUMON.load\nSAGUMON.cpus\n8\nSAGUMON.mem\nSAGUMON.end\n";
        assert_eq!(parse_output(out.as_bytes()), None);
        assert_eq!(parse_output(b""), None);
    }

    #[test]
    fn partial_output_without_df_still_parses() {
        let cut = OUT.split("SAGUMON.df").next().unwrap();
        let s = parse_output(cut.as_bytes()).unwrap();
        assert!(s.disks.is_empty());
        assert!(s.disks_stale);
        assert_eq!(s.cpus, Some(2));
        // `df` morto pelo prazo: a secao chega vazia, mas o fim chega.
        let killed = format!("{cut}SAGUMON.df\nSAGUMON.end\n");
        let s = parse_output(killed.as_bytes()).unwrap();
        assert!(s.disks_stale && s.disks.is_empty());
        // Cortado no meio do `df` (sem o marcador final): tambem nao vale.
        let half = OUT.split("/dev/sdb1").next().unwrap();
        assert!(parse_output(half.as_bytes()).unwrap().disks_stale);
    }

    /// Servidor em portugues (sem o LC_ALL=C valer, por exemplo): o
    /// cabecalho traduzido do `df` ainda e o cabecalho.
    #[test]
    fn df_header_in_any_language() {
        let out = "SAGUMON.mem\nMemTotal: 1000 kB\nMemAvailable: 500 kB\nSAGUMON.df\n\
Sist. Arq.     Blocos de 1024     Usado Disponível Capacid. Montado em\n\
/dev/vda1             10000      9900        100      99% /\nSAGUMON.end\n";
        let s = parse_output(out.as_bytes()).unwrap();
        assert!(!s.disks_stale);
        assert_eq!(s.disks.len(), 1);
        assert_eq!(s.disk_level(), Level::Crit);
        // So o erro (no stderr, que nao chega): secao vazia, sem resposta.
        let out = "SAGUMON.mem\nMemTotal: 1000 kB\nSAGUMON.df\n\nSAGUMON.end\n";
        assert!(parse_output(out.as_bytes()).unwrap().disks_stale);
        // Duas sintaxes do timeout: so uma imprime; uma 2a tabela nao duplica.
        let twice = "SAGUMON.mem\nMemTotal: 1000 kB\nSAGUMON.df\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10 5 5 50% /\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 10 5 5 50% /\n\
SAGUMON.end\n";
        assert_eq!(parse_output(twice.as_bytes()).unwrap().disks.len(), 1);
    }

    /// Leitura cortada por tamanho (centenas de montagens de conteineres):
    /// os discos que chegaram valem; a ultima linha, pela metade, nao.
    #[test]
    fn size_cut_keeps_received_disks() {
        let mut out = String::from(
            "SAGUMON.mem\nMemTotal: 1000 kB\nSAGUMON.df\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/vda1 100 95 5 95% /\n",
        );
        out.push_str("/dev/vdb1 100 99 1 99% /meio-cort");
        let s = parse_reply(out.as_bytes(), true).unwrap();
        assert!(!s.disks_stale);
        let mounts: Vec<&str> = s.disks.iter().map(|d| d.mount.as_str()).collect();
        assert_eq!(mounts, ["/"]);
        // O mesmo sem o corte por tamanho (prazo): o df nao respondeu.
        assert!(parse_reply(out.as_bytes(), false).unwrap().disks_stale);
    }

    /// Mais de `MAX_DISKS` montagens: o mais cheio continua na conta mesmo
    /// vindo por ultimo.
    #[test]
    fn fullest_disk_survives_the_limit() {
        let mut out = String::from(
            "SAGUMON.mem\nMemTotal: 1000 kB\nSAGUMON.df\n\
Filesystem 1024-blocks Used Available Capacity Mounted on\n",
        );
        for i in 0..40 {
            out.push_str(&format!("/dev/sd{i} 100 10 90 10% /data/disk{i:02}\n"));
        }
        out.push_str("/dev/sdz 100 97 3 97% /data/cheio\nSAGUMON.end\n");
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.disks.len(), MAX_DISKS);
        assert_eq!(s.disks[0].mount, "/data/cheio");
        assert_eq!(s.disk_level(), Level::Crit);
    }

    #[test]
    fn connect_failures_are_classified() {
        let f = |m: &str| connect_failure(&anyhow::anyhow!("{m}"));
        assert_eq!(f(KEY_NEW).kind, FailKind::Key);
        assert_eq!(f(KEY_CHANGED).kind, FailKind::Key);
        let auth = f(ssh::AUTH_FAILED);
        assert_eq!(auth.kind, FailKind::Auth);
        assert!(auth.msg.starts_with("Falha na autenticação") && auth.msg.ends_with(AUTH_RETRY));
        assert_eq!(f(&format!("{}: passphrase", ssh::KEY_INVALID)).kind, FailKind::Auth);
        let other = f("não foi possível conectar: recusado");
        assert_eq!(other.kind, FailKind::Error);
        assert_eq!(other.msg, "Não foi possível conectar: recusado");
        assert!(blocks(FailKind::Auth) && blocks(FailKind::Unsupported));
        assert!(blocks(FailKind::NoData));
        assert!(!blocks(FailKind::Error) && !blocks(FailKind::Key));
        // Conexao encerrada no meio da autenticacao (PAM no boot): passageiro.
        assert_eq!(f(ssh::AUTH_CLOSED).kind, FailKind::Error);
    }

    /// Load conta as CPUs da maquina toda: o `getconf` (1a linha) vale mais
    /// que o `nproc` (limitado pela afinidade da sessao).
    #[test]
    fn cpu_count_prefers_getconf() {
        let out = "SAGUMON.load\n12 12 12 13/400 1\nSAGUMON.cpus\n16\n2\nSAGUMON.end\n";
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.cpus, Some(16));
        assert_eq!(s.load_level(), Level::Ok);
        // Sem getconf: o nproc.
        let out = "SAGUMON.load\n1 1 1 1/9 1\nSAGUMON.cpus\n4\nSAGUMON.end\n";
        assert_eq!(parse_output(out.as_bytes()).unwrap().cpus, Some(4));
    }

    /// CPU pela media desde a coleta anterior; discos da coleta anterior
    /// quando o `df` nao responde.
    #[test]
    fn carry_between_collections() {
        let mut carry = Carry::default();
        let disk = |pct: u64| Disk {
            mount: "/".into(),
            size_kb: 100,
            used_kb: pct,
            avail_kb: 100 - pct,
        };
        // 1a coleta: CPU do ultimo segundo (pico de 100%).
        let mut s1 = Sample {
            cpu_pct: Some(100.0),
            cpu_ticks: Some((10_000, 9_000)),
            disks: vec![disk(95)],
            ..Default::default()
        };
        carry.apply(&mut s1);
        assert_eq!(s1.cpu_pct, Some(100.0));
        // 2a: o segundo medido caiu num cron (100%), mas no minuto a maquina
        // ficou 95% ociosa; o df travou.
        let mut s2 = Sample {
            cpu_pct: Some(100.0),
            cpu_ticks: Some((16_000, 14_700)),
            disks_stale: true,
            ..Default::default()
        };
        carry.apply(&mut s2);
        assert_eq!(s2.cpu_pct, Some(5.0));
        assert_eq!(s2.disks, vec![disk(95)]);
        assert_eq!(s2.disk_level(), Level::Crit);
        // Reboot: contadores voltaram; fica o do ultimo segundo.
        let mut s3 = Sample {
            cpu_pct: Some(40.0),
            cpu_ticks: Some((500, 300)),
            disks: vec![disk(10)],
            ..Default::default()
        };
        carry.apply(&mut s3);
        assert_eq!(s3.cpu_pct, Some(40.0));
        assert_eq!(s3.disks, vec![disk(10)]);
        // E a seguinte ja compara com os contadores novos.
        let mut s4 = Sample {
            cpu_pct: Some(0.0),
            cpu_ticks: Some((1_500, 800)),
            ..Default::default()
        };
        carry.apply(&mut s4);
        assert_eq!(s4.cpu_pct, Some(50.0));
        // Sem df antes nenhum: continua sem discos (e marcado).
        let mut fresh = Carry::default();
        let mut s5 = Sample {
            disks_stale: true,
            ..Default::default()
        };
        fresh.apply(&mut s5);
        assert!(s5.disks.is_empty() && s5.disks_stale);
    }

    /// Linha do `/proc/net/dev` com bytes recebidos e enviados.
    fn dev(name: &str, rx: u64, tx: u64) -> String {
        format!("{name:>6}: {rx} 10 0 0 0 0 0 0 {tx} 10 0 0 0 0 0 0\n")
    }

    fn link(name: &str, dest: &str) -> String {
        format!("lrwxrwxrwx 1 root root 0 Oct 23 06:16 {name} -> ../../devices/{dest}/net/{name}\n")
    }

    /// Resposta com as duas leituras da rede (`t0`/`t1`: hora de cada uma).
    fn net_out(t0: &str, a: &[(&str, u64, u64)], t1: &str, b: &[(&str, u64, u64)], nic: &str) -> String {
        let header = "Inter-|   Receive                                                |  Transmit\n \
face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo \
colls carrier compressed\n";
        let rows = |v: &[(&str, u64, u64)]| v.iter().map(|(n, r, t)| dev(n, *r, *t)).collect::<String>();
        format!(
            "SAGUMON.mem\nMemTotal: 1000 kB\nSAGUMON.up\n{t0} 50.00\nSAGUMON.net\n{header}{}\
SAGUMON.up2\n{t1} 51.00\nSAGUMON.net2\n{header}{}SAGUMON.nic\ntotal 0\n{nic}SAGUMON.end\n",
            rows(a),
            rows(b)
        )
    }

    /// So as placas fisicas contam: a bridge e o veth do conteiner repetiriam
    /// o trafego da placa.
    #[test]
    fn network_counts_physical_interfaces() {
        let nic = [
            link("br0", "virtual"),
            link("eth0", "pci0000:00/0000:00:03.0/virtio0"),
            link("lo", "virtual"),
            link("veth12ab", "virtual"),
        ]
        .concat();
        let out = net_out(
            "100.00",
            &[("lo", 5_000, 5_000), ("eth0", 1_000_000, 2_000_000), ("br0", 900_000, 1_900_000), ("veth12ab", 0, 0)],
            "102.00",
            &[("lo", 9_000, 9_000), ("eth0", 1_250_000, 2_100_000), ("br0", 1_150_000, 2_000_000), ("veth12ab", 99, 99)],
            &nic,
        );
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.net, Some(NetRate { rx: 125_000.0, tx: 50_000.0, secs: 2.0 }));
        let c = s.net_counters.unwrap();
        assert_eq!(c.at, 102.0);
        assert_eq!(c.ifaces, [("eth0".to_string(), 1_250_000, 2_100_000)]);

        // Conteiner: a placa e um veth (virtual). Contam todas menos a lo.
        let nic = [link("eth0", "virtual"), link("lo", "virtual")].concat();
        let out = net_out("10", &[("lo", 0, 0), ("eth0", 0, 0)], "11", &[("lo", 50, 50), ("eth0", 800, 400)], &nic);
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.net, Some(NetRate { rx: 800.0, tx: 400.0, secs: 1.0 }));
        // Sem o `ls` (sem /sys): idem.
        let out = net_out("10", &[("lo", 0, 0), ("eth0", 0, 0)], "11", &[("lo", 50, 50), ("eth0", 800, 400)], "");
        assert_eq!(parse_output(out.as_bytes()).unwrap().net.unwrap().rx, 800.0);

        // Kernel antigo: numero grudado no nome (`eth0:4294967296`).
        let lines: Vec<String> = ["Inter-|", "eth0:4294967296 1 0 0 0 0 0 0 7 1 0 0 0 0 0 0"]
            .map(String::from)
            .to_vec();
        let r = net_reading(&lines, Some(1.0), &[]).unwrap();
        assert_eq!(r.ifaces, [("eth0".to_string(), 4_294_967_296, 7)]);
        // Sem a hora, sem leitura; sem a 2a leitura, sem trafego.
        assert_eq!(net_reading(&lines, None, &[]), None);
        let cut = out.split("SAGUMON.up2").next().unwrap();
        let s = parse_output(cut.as_bytes()).unwrap();
        assert_eq!(s.net, None);
        assert!(s.net_counters.is_some());
    }

    /// Trafego pela media desde a coleta anterior, como a CPU; contador
    /// zerado (reboot ou placa recarregada) fica com o do ultimo segundo.
    #[test]
    fn network_average_between_collections() {
        let reading = |at: f64, rx: u64, tx: u64| NetReading {
            at,
            ifaces: vec![("eth0".into(), rx, tx)],
        };
        let window = NetRate { rx: 1e6, tx: 1e6, secs: 1.0 };
        let with = |r: NetReading| Sample {
            net: Some(window.clone()),
            net_counters: Some(r),
            ..Default::default()
        };
        let mut carry = Carry::default();
        let mut s1 = with(reading(100.0, 1_000, 2_000));
        carry.apply(&mut s1);
        assert_eq!(s1.net, Some(window.clone()));
        let mut s2 = with(reading(160.0, 61_000, 8_000));
        carry.apply(&mut s2);
        assert_eq!(s2.net, Some(NetRate { rx: 1_000.0, tx: 100.0, secs: 60.0 }));
        // Reboot: o relogio voltou.
        let mut s3 = with(reading(30.0, 10, 10));
        carry.apply(&mut s3);
        assert_eq!(s3.net, Some(window.clone()));
        // Contador menor que o anterior: tambem nao ha media.
        let mut s4 = with(reading(90.0, 5, 5));
        carry.apply(&mut s4);
        assert_eq!(s4.net, Some(window));
    }

    #[test]
    fn kernel_versions_compare_by_flavor() {
        let v = |r: &str| KernelVer::parse(r).unwrap();
        assert_eq!(v("4.18.0-553.el8_10.x86_64").nums, [4, 18, 0, 553]);
        assert_eq!(v("4.18.0-553.el8_10.x86_64").flavor, "elx");
        assert!(v("4.18.0-553.22.1.el8_10.x86_64").newer_than(&v("4.18.0-553.el8_10.x86_64")));
        // De uma versao do RHEL para a seguinte (el8_9 -> el8_10).
        assert!(v("4.18.0-553.el8_10.x86_64").newer_than(&v("4.18.0-513.24.1.el8_9.x86_64")));
        assert!(!v("4.18.0-553.el8_10.x86_64").newer_than(&v("4.18.0-553.el8_10.x86_64")));
        assert!(v("5.15.0-92-generic").newer_than(&v("5.15.0-91-generic")));
        assert!(v("6.5.0-14-generic").newer_than(&v("5.15.0-91-generic")));
        // Outro sabor: depuracao, lowlatency, cloud, WSL.
        assert!(!v("4.18.0-553.el8_10.x86_64+debug").newer_than(&v("4.18.0-553.el8_10.x86_64")));
        assert!(!v("5.15.0-92-lowlatency").newer_than(&v("5.15.0-91-generic")));
        assert!(!v("6.1.0-15-cloud-amd64").newer_than(&v("6.1.0-13-amd64")));
        assert!(!v("6.6.87.2-microsoft-standard-WSL2").newer_than(&v("4.18.0-553.el8_10.x86_64")));
        // Nao e kernel: pasta extra do Arch, lixo, numero enorme.
        for r in ["extramodules-6.6-arch1", "x", "", "7", "99999999999999999999999.1"] {
            assert_eq!(KernelVer::parse(r), None, "{r:?}");
        }
    }

    fn kernel_out(running: &str, mods: &[&str], boot: &str, pkg: &str) -> String {
        format!(
            "SAGUMON.kver\n{running}\nSAGUMON.kmods\n{}\nSAGUMON.kboot\n{boot}SAGUMON.kpkg\n{pkg}\
SAGUMON.end\n",
            mods.join("\n")
        )
    }

    #[test]
    fn kernel_installed_waiting_for_reboot() {
        let run = "4.18.0-553.el8_10.x86_64";
        let mods = [
            "4.18.0-513.24.1.el8_9.x86_64",
            run,
            "4.18.0-553.22.1.el8_10.x86_64",
            "4.18.0-553.8.1.el8_10.x86_64",
            "4.18.0-553.30.1.el8_10.x86_64+debug",
        ];
        let c = parse_kernel(kernel_out(run, &mods, "", "").as_bytes()).unwrap();
        assert_eq!(c.running, run);
        assert_eq!(c.installed.as_deref(), Some("4.18.0-553.22.1.el8_10.x86_64"));
        assert_eq!(c.available, None);
        assert!(c.has_update());
        // Ja no mais novo: nada a fazer.
        let c = parse_kernel(kernel_out("4.18.0-553.22.1.el8_10.x86_64", &mods, "", "").as_bytes()).unwrap();
        assert!(!c.has_update(), "{c:?}");
        // Conteiner: o kernel em uso (o do hospedeiro) nao esta instalado.
        let c = parse_kernel(kernel_out("5.14.0-427.el9.x86_64", &mods, "", "").as_bytes()).unwrap();
        assert!(!c.has_update(), "{c:?}");
        // WSL: os kernels da distro instalados nao sao do sabor do em uso.
        let wsl = "6.6.87.2-microsoft-standard-WSL2";
        let mods = ["4.18.0-553.80.1.el8_10.x86_64", "6.6.87.1-microsoft-standard-WSL2", wsl];
        let pkg = "kernel-core.x86_64                4.18.0-553.156.1.el8_10                 baseos\n";
        let c = parse_kernel(kernel_out(wsl, &mods, "", pkg).as_bytes()).unwrap();
        assert_eq!(c, KernelCheck { running: wsl.into(), ..Default::default() });
        // Debian: pacote atualizado com o mesmo nome do kernel em uso.
        let deb = "6.1.0-13-amd64";
        let boot = "linux-image-6.1.0-13-amd64\nlinux-base\n";
        let c = parse_kernel(kernel_out(deb, &[deb], boot, "").as_bytes()).unwrap();
        assert_eq!(c.installed.as_deref(), Some(deb));
        // ... mas nao um pacote mais velho que o em uso.
        let boot = "linux-image-6.1.0-12-amd64\n";
        assert!(!parse_kernel(kernel_out(deb, &[deb], boot, "").as_bytes()).unwrap().has_update());
    }

    #[test]
    fn kernel_available_in_repositories() {
        let run = "4.18.0-553.80.1.el8_10.x86_64";
        // dnf: o metapacote e o kernel-core; o recuado e o obsoleto.
        let pkg = "\nkernel.x86_64          4.18.0-553.156.1.el8_10    baseos\n\
kernel-core.x86_64     4.18.0-553.156.1.el8_10    baseos\n";
        let c = parse_kernel(kernel_out(run, &[run], "", pkg).as_bytes()).unwrap();
        assert_eq!(c.available, Some(("kernel".into(), "4.18.0-553.156.1.el8_10".into())));
        assert_eq!(c.installed, None);
        // Pacote mais velho que o em uso (ex.: kernel instalado a mao): nao.
        let old = "kernel-core.x86_64 4.18.0-553.22.1.el8_10 baseos\n    kernel.x86_64 x @anaconda\n";
        assert_eq!(parse_kernel(kernel_out(run, &[run], "", old).as_bytes()).unwrap().available, None);
        // Outros pacotes com "kernel" no nome nao contam.
        let other = "kernel-headers.x86_64 4.18.0-553.156.1.el8_10 baseos\n\
kernel-tools.x86_64 4.18.0-553.156.1.el8_10 baseos\n";
        assert_eq!(parse_kernel(kernel_out(run, &[run], "", other).as_bytes()).unwrap().available, None);

        // apt: metapacote do kernel na lista do `apt list --upgradable`.
        let ub = "5.15.0-91-generic";
        let pkg = "Listing...\nbash/jammy-updates 5.1-6ubuntu1.1 amd64 [upgradable from: 5.1-6ubuntu1]\n\
linux-generic/jammy-updates,jammy-security 5.15.0.92.89 amd64 [upgradable from: 5.15.0.91.88]\n\
linux-image-generic/jammy-updates,jammy-security 5.15.0.92.89 amd64 [upgradable from: 5.15.0.91.88]\n";
        let c = parse_kernel(kernel_out(ub, &[ub], "", pkg).as_bytes()).unwrap();
        assert_eq!(c.available, Some(("linux-image-generic".into(), "5.15.0.92.89".into())));
        // Nada no servidor alem do `uname`: so o kernel em uso.
        let c = parse_kernel(b"SAGUMON.kver\n5.15.0-91-generic\nSAGUMON.kmods\nSAGUMON.end\n").unwrap();
        assert_eq!(c, KernelCheck { running: ub.into(), ..Default::default() });
        assert_eq!(parse_kernel(b"SAGUMON.kver\nSAGUMON.end\n"), None);
    }

    /// Texto do servidor com controle ou fora do formato de versao some.
    #[test]
    fn kernel_hostile_values_are_rejected() {
        let run = "5.15.0-91-generic";
        let evil = "5.15.0-99-generic\u{202E}";
        let c = parse_kernel(kernel_out(run, &[run, evil], "", "").as_bytes()).unwrap();
        assert_eq!(c.installed, None);
        let pkg = "linux-image-generic/x 5.15.0.92\u{1b}[31m amd64 [upgradable from: 1]\n";
        assert_eq!(parse_kernel(kernel_out(run, &[run], "", pkg).as_bytes()).unwrap().available, None);
        assert_eq!(parse_kernel(b"SAGUMON.kver\nlinux \x1b]0;x\x07\nSAGUMON.end\n"), None);
        assert_eq!(clean_release(&"9".repeat(MAX_RELEASE + 1)), None);
    }

    #[test]
    fn hostile_values_are_rejected() {
        let out = "SAGUMON.load\nNaN inf -1 x/y\nSAGUMON.stat\ncpu 9 9 9 9\ncpu 1 1 1 1\n\
SAGUMON.df\nFilesystem x\n/dev/sdc1 100 10 90 10% /mnt/\u{202E}evil\n\
/dev/sdd1 18446744073709551615 18446744073709551615 18446744073709551615 100% /big\n\
SAGUMON.mem\nMemTotal: 10 kB\nMemAvailable: 99 kB\nSAGUMON.end\n";
        let s = parse_output(out.as_bytes()).unwrap();
        assert_eq!(s.load, None);
        assert_eq!(s.procs, None);
        // Contadores que andaram para tras: sem valor.
        assert_eq!(s.cpu_pct, None);
        // Nome com bidi descartado; numeros enormes nao estouram.
        assert_eq!(s.disks.len(), 1);
        assert_eq!(s.disks[0].mount, "/big");
        assert_eq!(s.disks[0].pct(), 100.0);
        // Disponivel maior que o total: zero usado, nunca negativo.
        assert_eq!(s.mem, Some(Usage { total_kb: 10, used_kb: 0 }));
    }

    #[test]
    fn end_marker_detection() {
        assert!(ends_with_end(b"x\nSAGUMON.end\n"));
        assert!(ends_with_end(b"SAGUMON.end\r\n"));
        assert!(!ends_with_end(b"xSAGUMON.end\n"));
        assert!(!ends_with_end(b"SAGUMON.en"));
    }

    /// Load pelo menor entre o de 1 e o de 5 min, por CPU.
    #[test]
    fn load_level_follows_current_sustained_load() {
        let with = |load: [f32; 3], cpus: u32| Sample {
            load: Some(load),
            cpus: Some(cpus),
            ..Default::default()
        };
        // Problema resolvido: o de 1 min ja caiu, os de 5 e 15 ainda altos.
        let s = with([0.28, 2.07, 3.44], 1);
        assert_eq!(s.load_level(), Level::Ok);
        assert_eq!(s.health(), Level::Ok);
        assert!(s.alerts().is_empty(), "{:?}", s.alerts());
        // Pico curto: so o de 1 min subiu.
        assert_eq!(with([5.0, 0.5, 0.2], 1).load_level(), Level::Ok);
        // Carga alta que dura e continua.
        let s = with([3.0, 2.5, 1.0], 1);
        assert_eq!(s.load_level(), Level::Crit);
        assert_eq!(
            s.alerts(),
            [("Load de 3,00 (1 min) e 2,50 (5 min) para 1 CPU(s)".to_string(), Level::Crit)]
        );
        assert_eq!(with([2.5, 2.2, 1.0], 2).load_level(), Level::Warn);
        // Por CPU: o mesmo load numa maquina de 4 CPUs e normal.
        assert_eq!(with([3.0, 2.5, 1.0], 4).load_level(), Level::Ok);
        // Sem o numero de CPUs nao ha como julgar.
        let mut s = with([9.0, 9.0, 9.0], 1);
        s.cpus = None;
        assert_eq!(s.load_level(), Level::Ok);
    }

    #[test]
    fn levels() {
        assert_eq!(level(79.9, 80.0, 90.0), Level::Ok);
        assert_eq!(level(80.0, 80.0, 90.0), Level::Warn);
        assert_eq!(level(95.0, 80.0, 90.0), Level::Crit);
    }

    /// A politica de privacidade cita os comandos exatos (portugues e ingles).
    #[test]
    fn privacy_policy_quotes_the_command() {
        let policy = include_str!("../PRIVACY.md");
        assert_eq!(policy.matches(COMMAND).count(), 2);
        assert_eq!(policy.matches(KERNEL_COMMAND).count(), 2);
    }

    #[test]
    fn command_is_shell_agnostic() {
        for cmd in [COMMAND, KERNEL_COMMAND] {
            for c in ['|', '>', '<', '"', '\'', '$', '*', '?', '#', '&', '`', '\n'] {
                assert!(!cmd.contains(c), "{c:?}");
            }
            assert!(cmd.ends_with("; echo SAGUMON.end"));
        }
        assert!(COMMAND.contains("env LC_ALL=C timeout -s KILL 5 df -P -k;"));
        assert!(COMMAND.contains("env LC_ALL=C timeout -t 5 -s KILL df -P -k;"));
        // O `sleep 1` fica entre as duas leituras da CPU e da rede.
        let (before, after) = COMMAND.split_once("sleep 1;").unwrap();
        for part in [before, after] {
            assert!(part.contains("head -n 1 /proc/stat;") && part.contains("cat /proc/net/dev;"));
        }
    }

    #[test]
    fn same_target_tracks_credentials_and_key() {
        let mut a = Host::new();
        a.host = "h".into();
        let mut b = a.clone();
        assert!(same_target(&a, &b));
        b.name = "outro nome".into();
        assert!(same_target(&a, &b));
        b.host_key = Some("ssh-ed25519 AAAA".into());
        assert!(!same_target(&a, &b));
        let mut c = a.clone();
        c.auth = AuthMethod::Password { password: "x".into() };
        assert!(!same_target(&a, &c));
    }

    // Mesmas variaveis dos outros e2e (ver `osinfo`): SAGU_E2E_PORT (2222),
    // SAGU_E2E_USER e SAGU_E2E_KEY.
    // Rodar com: cargo test e2e_monitor -- --ignored

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("defina {k}"))
    }

    fn e2e_host() -> Host {
        let mut host = Host::new();
        host.host = "127.0.0.1".into();
        host.port = env("SAGU_E2E_PORT").parse().unwrap();
        host.username = env("SAGU_E2E_USER");
        host.auth = AuthMethod::Key {
            private_key: std::fs::read_to_string(env("SAGU_E2E_KEY")).unwrap(),
            passphrase: None,
        };
        host
    }

    /// Chave do sshd de teste, como o usuario a teria aceitado no terminal.
    fn server_key(host: &Host) -> String {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let seen = std::sync::Mutex::new(None);
        rt.block_on(ssh::connect_and_auth(host, |p: HostKeyPrompt| {
            *seen.lock().unwrap() = Some(p.presented.clone());
            let _ = p.reply.send(HostKeyAnswer::Accept);
        }))
        .unwrap();
        seen.into_inner().unwrap().unwrap()
    }

    /// Proxima coleta (pula a verificacao do kernel).
    fn next(h: &MonitorHandle) -> MonitorEvent {
        loop {
            match h.rx.recv_timeout(Duration::from_secs(30)).expect("sem resultado em 30 s") {
                Event::Sample(ev) => return ev,
                Event::Kernel(_) => {}
            }
        }
    }

    #[test]
    #[ignore]
    fn e2e_monitor_collects_and_never_asks_for_the_key() {
        let mut host = e2e_host();
        let h = start(|| {});

        // Sem chave confirmada: desiste sem perguntar nem autenticar.
        h.refresh(vec![host.clone()], 1, false);
        let ev = next(&h);
        assert_eq!(ev.host_id, host.id);
        assert_eq!(ev.seq, 1);
        assert_eq!(ev.result.unwrap_err(), Failure::new(FailKind::Key, KEY_NEW));

        // Chave diferente da guardada: tambem desiste.
        host.host_key = Some(
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl".into(),
        );
        h.refresh(vec![host.clone()], 2, false);
        assert_eq!(next(&h).result.unwrap_err(), Failure::new(FailKind::Key, KEY_CHANGED));

        // Com a chave aceita: coleta, e de novo na mesma conexao.
        host.host_key = Some(server_key(&host));
        for seq in 3..5 {
            h.refresh(vec![host.clone()], seq, false);
            let ev = match h.rx.recv_timeout(Duration::from_secs(30)).expect("sem coleta") {
                Event::Sample(ev) => ev,
                Event::Kernel(k) => panic!("kernel antes da coleta: {k:?}"),
            };
            let s = ev.result.unwrap();
            assert!(s.load.is_some(), "{s:?}");
            assert!(s.cpus.is_some_and(|c| c > 0), "{s:?}");
            assert!(s.cpu_pct.is_some(), "{s:?}");
            assert!(!s.disks_stale, "{s:?}");
            assert!(s.mem.is_some_and(|m| m.total_kb > 0), "{s:?}");
            assert!(s.uptime_secs.is_some(), "{s:?}");
            assert!(!s.disks.is_empty(), "{s:?}");
            let net = s.net.as_ref().expect("sem trafego de rede");
            assert!(net.rx >= 0.0 && net.tx >= 0.0 && net.secs > 0.5, "{net:?}");
            assert!(s.net_counters.as_ref().is_some_and(|c| !c.ifaces.is_empty()), "{s:?}");
            if seq == 3 {
                // 1a coleta da conexao: a verificacao do kernel vem logo
                // depois (uma vez so: a 2a coleta nao repete).
                match h.rx.recv_timeout(Duration::from_secs(60)).expect("sem kernel") {
                    Event::Kernel(k) => {
                        assert_eq!(k.host_id, host.id);
                        let c = k.check.expect("kernel sem resposta");
                        assert!(KernelVer::parse(&c.running).is_some(), "{c:?}");
                        eprintln!("SAGU-E2E: kernel {c:?}");
                    }
                    Event::Sample(ev) => panic!("coleta no lugar do kernel: {ev:?}"),
                }
            } else {
                // Media desde a coleta anterior (nao a do ultimo segundo).
                assert!(net.secs > 1.5 && net.secs < 120.0, "{net:?}");
            }
        }

        // Fora da lista: nenhuma coleta.
        h.refresh(Vec::new(), 5, false);
        assert!(h.rx.recv_timeout(Duration::from_secs(3)).is_err());
    }

    /// Credenciais recusadas: uma tentativa so; as coletas seguintes repetem
    /// o erro sem conectar, ate "Atualizar agora" (force).
    #[test]
    #[ignore]
    fn e2e_monitor_auth_failure_is_not_retried() {
        let mut host = e2e_host();
        host.host_key = Some(server_key(&host));
        // Senha num servidor que so aceita chave: recusada.
        host.auth = AuthMethod::Password {
            password: "errada".into(),
        };
        let h = start(|| {});
        h.refresh(vec![host.clone()], 1, false);
        let first = next(&h).result.unwrap_err();
        assert_eq!(first.kind, FailKind::Auth, "{first:?}");
        // Sem force: o mesmo erro, na hora (sem nova conexao).
        let t0 = std::time::Instant::now();
        h.refresh(vec![host.clone()], 2, false);
        let ev = next(&h);
        assert_eq!(ev.seq, 2);
        assert!(ev.replayed);
        assert_eq!(ev.result.unwrap_err(), first);
        assert!(t0.elapsed() < Duration::from_millis(500), "{:?}", t0.elapsed());
        // Com force: tenta de novo (e falha de novo).
        h.refresh(vec![host.clone()], 3, true);
        let ev = next(&h);
        assert!(!ev.replayed);
        assert_eq!(ev.result.unwrap_err().kind, FailKind::Auth);
        eprintln!("SAGU-E2E: 2 tentativas de login esperadas no log do sshd");
    }
}
