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
pub const COMMAND: &str = "echo SAGUMON.load; cat /proc/loadavg; \
echo SAGUMON.cpus; getconf _NPROCESSORS_ONLN; nproc; echo SAGUMON.mem; cat /proc/meminfo; \
echo SAGUMON.up; cat /proc/uptime; echo SAGUMON.stat; head -n 1 /proc/stat; sleep 1; \
head -n 1 /proc/stat; echo SAGUMON.df; env LC_ALL=C timeout -s KILL 5 df -P -k; \
env LC_ALL=C timeout -t 5 -s KILL df -P -k; echo SAGUMON.end";

/// Prazo para conectar e autenticar.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Prazo para o servidor abrir o canal.
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
/// Prazo para o comando responder (inclui o `sleep 1` e o prazo do `df`).
const RUN_TIMEOUT: Duration = Duration::from_secs(15);
/// Saida maior que isto: para de ler e usa o que ja chegou (um no de
/// Kubernetes lista centenas de montagens de conteineres no `df`).
const MAX_OUTPUT: usize = 1024 * 1024;
/// Pontos de montagem guardados por servidor.
const MAX_DISKS: usize = 32;
/// Tamanho maximo do nome de um ponto de montagem.
const MAX_MOUNT: usize = 128;

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
    pub rx: std::sync::mpsc::Receiver<MonitorEvent>,
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
    std::sync::mpsc::Sender<MonitorEvent>,
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
    let (events, rx) = std::sync::mpsc::channel::<MonitorEvent>();
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
    events: std::sync::mpsc::Sender<MonitorEvent>,
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

/// Laco de um host: uma coleta por aviso da UI, na mesma conexao.
async fn watch(
    host: Host,
    mut ticks: UnboundedReceiver<Tick>,
    events: std::sync::mpsc::Sender<MonitorEvent>,
    repaint: Repaint,
) {
    let mut conn: Option<client::Handle<Client>> = None;
    // Falha que nao adianta repetir a cada minuto (ver `blocks`).
    let mut blocked: Option<Failure> = None;
    let mut carry = Carry::default();
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
            _ => sample(&host, &mut conn).await,
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
        if events
            .send(MonitorEvent {
                host_id: host.id,
                target: target(&host),
                seq: tick.seq,
                result,
                replayed,
            })
            .is_err()
        {
            break;
        }
        repaint();
    }
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
    let Some(reply) = run_command(session).await else {
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
}

impl Carry {
    /// CPU pela media desde a coleta anterior: uma janela de 1 s no mesmo
    /// segundo de cada minuto coincidiria com um cron (`* * * * *`) e
    /// acusaria critico a cada coleta, ou nunca o veria. Sem `df` nesta
    /// coleta, os discos da anterior continuam valendo (marcados): sumir com
    /// eles tiraria o disco da conta da saude.
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
    /// Parou pelo prazo (`RUN_TIMEOUT`).
    timed_out: bool,
}

/// Roda `COMMAND` num canal proprio. Devolve o stdout ate o marcador final,
/// o fim do canal, o prazo ou `MAX_OUTPUT` (o que vier antes); `None` se o
/// canal nao abriu, o exec foi recusado ou nada chegou.
async fn run_command(session: &client::Handle<Client>) -> Option<Reply> {
    let channel = timeout(OPEN_TIMEOUT, session.channel_open_session())
        .await
        .ok()?
        .ok()?;
    let (mut rd, wr) = channel.split();
    let mut out = Vec::new();
    let mut size_cut = false;
    let done = timeout(RUN_TIMEOUT, async {
        wr.exec(true, COMMAND).await.ok()?;
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
    df: Vec<String>,
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
                "df" => Some(&mut s.df),
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

/// Le a saida do `COMMAND`. `None` se nao veio nada reconhecivel (sem load
/// nem memoria: provavelmente nao e Linux). `size_cut` = a leitura parou por
/// `MAX_OUTPUT` (a ultima linha, talvez pela metade, e descartada e o resto
/// vale).
fn parse_reply(out: &[u8], size_cut: bool) -> Option<Sample> {
    let out = if size_cut {
        out.iter()
            .rposition(|&b| b == b'\n')
            .map_or(&out[..0], |i| &out[..=i])
    } else {
        out
    };
    let s = split_sections(out);
    let rows = df_rows(&s.df);
    let (load, procs) = parse_loadavg(&s.load);
    let (mem, swap) = parse_meminfo(&s.mem);
    if load.is_none() && mem.is_none() {
        return None;
    }
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
        uptime_secs: s.up.first().and_then(|l| {
            let secs = l.split_whitespace().next()?.parse::<f64>().ok()?;
            (secs.is_finite() && secs >= 0.0).then_some(secs as u64)
        }),
        // Sem o cabecalho do `df` ate o marcador final (ou ate o corte por
        // tamanho), ele nao respondeu.
        disks_stale: !((s.end || size_cut) && rows.is_some()),
        disks: rows.map(parse_df).unwrap_or_default(),
    })
}

fn finite(v: f32) -> Option<f32> {
    (v.is_finite() && v >= 0.0).then_some(v)
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

    /// A politica de privacidade cita o comando exato (portugues e ingles).
    #[test]
    fn privacy_policy_quotes_the_command() {
        let policy = include_str!("../PRIVACY.md");
        assert_eq!(policy.matches(COMMAND).count(), 2);
    }

    #[test]
    fn command_is_shell_agnostic() {
        for c in ['|', '>', '<', '"', '\'', '$', '*', '?', '#', '&', '`', '\n'] {
            assert!(!COMMAND.contains(c), "{c:?}");
        }
        assert!(COMMAND.contains("env LC_ALL=C timeout -s KILL 5 df -P -k;"));
        assert!(COMMAND.contains("env LC_ALL=C timeout -t 5 -s KILL df -P -k;"));
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

    fn next(h: &MonitorHandle) -> MonitorEvent {
        h.rx.recv_timeout(Duration::from_secs(30)).expect("sem resultado em 30 s")
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
            let s = next(&h).result.unwrap();
            assert!(s.load.is_some(), "{s:?}");
            assert!(s.cpus.is_some_and(|c| c > 0), "{s:?}");
            assert!(s.cpu_pct.is_some(), "{s:?}");
            assert!(!s.disks_stale, "{s:?}");
            assert!(s.mem.is_some_and(|m| m.total_kb > 0), "{s:?}");
            assert!(s.uptime_secs.is_some(), "{s:?}");
            assert!(!s.disks.is_empty(), "{s:?}");
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
