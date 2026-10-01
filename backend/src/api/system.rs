use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sysinfo::{
    Components, Disks, ProcessRefreshKind, ProcessesToUpdate, System,
};

use crate::auth::check_ws_origin;
use crate::auth::perms::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;

/// 30 minutes of samples, replayed to every new dashboard connection so a
/// reload (or a monitor waking up) doesn't start from an empty chart.
const HISTORY_SECS: u64 = 30 * 60;
const TOP_PROCESSES: usize = 8;
/// Filesystem usage moves slowly; statvfs on every mount each tick is waste.
const DISK_REFRESH: Duration = Duration::from_secs(30);
/// Gap between the priming process refresh and the first full sample, so
/// per-process CPU% is measured over a real window instead of the time
/// since the last viewer left.
const PRIME_WINDOW: Duration = Duration::from_secs(1);

/// One compact history sample:
/// `[unix_secs, cpu_pct, mem_used, rx_rate, tx_rate, disk_read_rate, disk_write_rate, load1]`.
pub type HistoryPoint = [f64; 8];
pub type History = Arc<Mutex<VecDeque<HistoryPoint>>>;

/// Handle to the process-wide sampler thread.
///
/// Two tiers keep the idle cost near zero: the *lite* sample (cpu, memory,
/// network, disk I/O, load, the CPU temperature sensor) runs every tick and
/// feeds the history and the status line. The *full* sample adds the
/// process walk and every hardware sensor — by far the expensive reads — and
/// only runs while at least one System page holds a [`FullGuard`].
#[derive(Clone)]
pub struct Sampler {
    pub rx: tokio::sync::watch::Receiver<String>,
    pub history: History,
    pub interval: Duration,
    full_watchers: Arc<AtomicUsize>,
    nudge: std::sync::mpsc::Sender<()>,
}

impl Sampler {
    /// Ask for full samples until the returned guard is dropped.
    pub fn want_full(&self) -> FullGuard {
        if self.full_watchers.fetch_add(1, Ordering::Relaxed) == 0 {
            // Wake the sampler now instead of up to one interval later.
            let _ = self.nudge.send(());
        }
        FullGuard(self.full_watchers.clone())
    }
}

pub struct FullGuard(Arc<AtomicUsize>);

impl Drop for FullGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One sampler for the whole process; every client reads from the watch
/// channel instead of paying for its own sysinfo refresh.
pub fn spawn_sampler(interval: Duration) -> Sampler {
    let (tx, rx) = tokio::sync::watch::channel(String::from("{}"));
    let history_len = (HISTORY_SECS / interval.as_secs().max(1)) as usize;
    let history: History = Arc::new(Mutex::new(VecDeque::with_capacity(history_len)));
    let full_watchers = Arc::new(AtomicUsize::new(0));
    let (nudge, nudged) = std::sync::mpsc::channel::<()>();

    let hist = history.clone();
    let watchers = full_watchers.clone();
    // sysinfo refreshes do blocking syscalls; keep them off the async runtime.
    std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut sys = System::new();
            // Processes get their own System: a process refresh also refreshes the
            // CPU times it divides by, and sharing them with `refresh_cpu_all`
            // shrinks that window to a fraction of the interval — inflating both
            // per-process and global CPU%.
            let mut procs = System::new();
            let mut disks = Disks::new_with_refreshed_list();
            let mut last_disks = Instant::now();
            let mut net_io = NetIo::default();
            let mut components = Components::new_with_refreshed_list();
            // Refreshing every hwmon sensor costs 10-25 ms; the status line
            // only needs the CPU one.
            let cpu_sensor = cpu_sensor_index(&components);
            let mut disk_io = DiskIo::default();
            let mut primed = false;
            let host = host_info();
            // Threads are listed as processes unless excluded, which multiplies
            // the /proc walk (and the sampler's own CPU cost) by ~10x.
            let proc_kind = ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .without_tasks();
            loop {
                let full = watchers.load(Ordering::Relaxed) > 0;
                if full && !primed {
                    procs.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind);
                    primed = true;
                    std::thread::sleep(PRIME_WINDOW);
                } else if !full {
                    primed = false;
                }

                sys.refresh_cpu_all();
                sys.refresh_memory();
                let net = net_io.sample();
                if last_disks.elapsed() >= DISK_REFRESH {
                    disks.refresh(true);
                    last_disks = Instant::now();
                }
                if full {
                    procs.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind);
                    components.refresh(false);
                } else if let Some(i) = cpu_sensor {
                    if let Some(c) = components.list_mut().get_mut(i) {
                        c.refresh();
                    }
                }
                let io = disk_io.sample();

                let sample = Sample {
                    sys: &sys,
                    procs: full.then_some(&procs),
                    disks: &disks,
                    net: &net,
                    components: &components,
                    cpu_sensor,
                    host: &host,
                    disk_io: io,
                };
                let (payload, point) = build_payload(&sample);
                {
                    let mut h = hist.lock().unwrap();
                    if h.len() >= history_len {
                        h.pop_front();
                    }
                    h.push_back(point);
                }
                if tx.send(payload).is_err() {
                    return; // all receivers gone — process shutting down
                }
                match nudged.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })
        .expect("spawn sampler thread");

    Sampler {
        rx,
        history,
        interval,
        full_watchers,
        nudge,
    }
}

pub struct IfaceRates {
    iface: String,
    rx_rate: f64,
    tx_rate: f64,
    rx_total: u64,
    tx_total: u64,
}

/// Per-interface traffic from one read of /proc/net/dev. sysinfo opens
/// several `/sys/class/net/<iface>/statistics/*` files per interface, and a
/// docker host has one veth + bridge per container (78 here) — that walk
/// was most of the agent's idle CPU.
#[derive(Default)]
struct NetIo {
    last: Option<(Instant, Vec<(String, u64, u64)>)>,
}

impl NetIo {
    fn sample(&mut self) -> Vec<IfaceRates> {
        let now = Instant::now();
        let current = read_net_dev();
        let rates = current
            .iter()
            .map(|(iface, rx, tx)| {
                let (rx_rate, tx_rate) = match &self.last {
                    Some((t, prev)) => {
                        let secs = now.duration_since(*t).as_secs_f64().max(0.001);
                        prev.iter()
                            .find(|(name, _, _)| name == iface)
                            .map(|(_, prx, ptx)| {
                                (
                                    rx.saturating_sub(*prx) as f64 / secs,
                                    tx.saturating_sub(*ptx) as f64 / secs,
                                )
                            })
                            .unwrap_or((0.0, 0.0))
                    }
                    None => (0.0, 0.0),
                };
                IfaceRates {
                    iface: iface.clone(),
                    rx_rate,
                    tx_rate,
                    rx_total: *rx,
                    tx_total: *tx,
                }
            })
            .collect();
        self.last = Some((now, current));
        rates
    }
}

/// `(iface, rx_bytes, tx_bytes)` for the interfaces worth showing.
fn read_net_dev() -> Vec<(String, u64, u64)> {
    let Ok(text) = std::fs::read_to_string("/proc/net/dev") else {
        return Vec::new();
    };
    text.lines()
        .skip(2) // two header lines
        .filter_map(|line| {
            let (name, counters) = line.split_once(':')?;
            let name = name.trim();
            // Skip loopback and the per-container virtual interfaces docker
            // creates — they double-count traffic already seen on the uplink.
            if name == "lo"
                || name.starts_with("veth")
                || name.starts_with("br-")
                || name.starts_with("docker")
            {
                return None;
            }
            let f: Vec<&str> = counters.split_whitespace().collect();
            // rx: bytes packets errs drop fifo frame compressed multicast | tx: bytes …
            let rx = f.first()?.parse().ok()?;
            let tx = f.get(8)?.parse().ok()?;
            Some((name.to_string(), rx, tx))
        })
        .collect()
}

/// Whole-disk I/O from /proc/diskstats. sysinfo resolves each mount's
/// `/dev/...` path first, which doesn't exist inside a container, so read the
/// kernel's counters directly (they aren't namespaced).
#[derive(Default)]
struct DiskIo {
    last: Option<(std::time::Instant, u64, u64)>,
}

impl DiskIo {
    /// Bytes/sec read and written since the previous call.
    fn sample(&mut self) -> (f64, f64) {
        let Some((read, written)) = read_diskstats() else {
            return (0.0, 0.0);
        };
        let now = std::time::Instant::now();
        let rates = match self.last {
            Some((t, r, w)) => {
                let secs = now.duration_since(t).as_secs_f64().max(0.001);
                (
                    read.saturating_sub(r) as f64 / secs,
                    written.saturating_sub(w) as f64 / secs,
                )
            }
            None => (0.0, 0.0),
        };
        self.last = Some((now, read, written));
        rates
    }
}

fn read_diskstats() -> Option<(u64, u64)> {
    const SECTOR: u64 = 512; // diskstats always counts 512-byte sectors
    let text = std::fs::read_to_string("/proc/diskstats").ok()?;
    let mut totals = (0u64, 0u64);
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 {
            continue;
        }
        let name = f[2];
        // Physical disks only: partitions, device-mapper and md arrays would
        // count the same bytes twice; loop/ram/zram aren't real disk traffic.
        // Prefix check first: it is free, the /sys stat is a syscall.
        let virtual_dev = ["loop", "ram", "zram", "dm-", "md"]
            .iter()
            .any(|p| name.starts_with(p));
        if virtual_dev || !std::path::Path::new("/sys/block").join(name).exists() {
            continue;
        }
        totals.0 += f[5].parse::<u64>().unwrap_or(0) * SECTOR;
        totals.1 += f[9].parse::<u64>().unwrap_or(0) * SECTOR;
    }
    Some(totals)
}

fn host_info() -> serde_json::Value {
    let sys = System::new_with_specifics(
        sysinfo::RefreshKind::nothing().with_cpu(sysinfo::CpuRefreshKind::nothing()),
    );
    json!({
        "hostname": System::host_name(),
        "os": System::long_os_version(),
        "kernel": System::kernel_version(),
        "cpu_brand": sys.cpus().first().map(|c| c.brand().trim().to_string()),
        "cores": sys.cpus().len(),
        "physical_cores": System::physical_core_count(),
    })
}

/// Best guess at "the CPU temperature" sensor: the package/die sensor when
/// the platform exposes one, otherwise the hottest sensor we can read.
fn cpu_sensor_index(components: &Components) -> Option<usize> {
    let readings: Vec<(usize, String, f32)> = components
        .list()
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.temperature().map(|t| (i, c.label().to_lowercase(), t)))
        .filter(|(_, _, t)| t.is_finite() && *t > 0.0)
        .collect();
    let preferred = ["package", "tctl", "tdie", "cpu"];
    preferred
        .iter()
        .find_map(|key| readings.iter().find(|(_, label, _)| label.contains(key)))
        .or_else(|| readings.iter().max_by(|a, b| a.2.total_cmp(&b.2)))
        .map(|(i, _, _)| *i)
}

struct Sample<'a> {
    sys: &'a System,
    /// `Some` only on full samples.
    procs: Option<&'a System>,
    disks: &'a Disks,
    net: &'a [IfaceRates],
    components: &'a Components,
    cpu_sensor: Option<usize>,
    host: &'a serde_json::Value,
    disk_io: (f64, f64),
}

fn build_payload(s: &Sample) -> (String, HistoryPoint) {
    let Sample { sys, disks, components, host, .. } = s;
    let (disk_read_rate, disk_write_rate) = s.disk_io;
    let load = System::load_average();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();

    // One row per filesystem: bind mounts, btrfs subvolumes and container
    // file mounts all repeat the same device — keep its shortest mount path.
    let mut by_mount: Vec<_> = disks.list().iter().filter(|d| d.total_space() > 0).collect();
    by_mount.sort_by_key(|d| d.mount_point().as_os_str().len());
    let mut seen_devices = HashSet::new();
    let disk_list: Vec<_> = by_mount
        .into_iter()
        .filter(|d| seen_devices.insert(d.name().to_os_string()))
        .map(|d| {
            json!({
                "mount": d.mount_point().to_string_lossy(),
                "total": d.total_space(),
                "used": d.total_space() - d.available_space(),
            })
        })
        .collect();

    let net_list: Vec<_> = s
        .net
        .iter()
        .map(|n| {
            json!({
                "iface": n.iface,
                "rx_rate": n.rx_rate as u64,
                "tx_rate": n.tx_rate as u64,
                "rx_total": n.rx_total,
                "tx_total": n.tx_total,
            })
        })
        .collect();
    let rx_rate: f64 = s.net.iter().map(|n| n.rx_rate).sum();
    let tx_rate: f64 = s.net.iter().map(|n| n.tx_rate).sum();

    let cpu_total = sys.global_cpu_usage();
    let cpu_temp = s
        .cpu_sensor
        .and_then(|i| components.list().get(i))
        .and_then(|c| c.temperature())
        .filter(|t| t.is_finite() && *t > 0.0);
    let mut payload = json!({
        "ts": now.as_millis() as u64,
        "host": host,
        "cpu": {
            "total": cpu_total,
            "per_core": sys.cpus().iter().map(|c| c.cpu_usage()).collect::<Vec<f32>>(),
            "temp": cpu_temp,
        },
        "mem": {
            "total": sys.total_memory(),
            "used": sys.used_memory(),
            "available": sys.available_memory(),
            "swap_total": sys.total_swap(),
            "swap_used": sys.used_swap(),
        },
        "disks": disk_list,
        "disk_io": { "read_rate": disk_read_rate as u64, "write_rate": disk_write_rate as u64 },
        "net": net_list,
        "load": [load.one, load.five, load.fifteen],
        "uptime": System::uptime(),
    });

    // Full-tier fields; absent on lite samples (the UI treats both as optional).
    if let Some(procs) = s.procs {
        let mut by_cpu: Vec<_> = procs.processes().values().collect();
        by_cpu.sort_by(|a, b| b.cpu_usage().total_cmp(&a.cpu_usage()));
        let top: Vec<_> = by_cpu
            .iter()
            .take(TOP_PROCESSES)
            .map(|p| {
                json!({
                    "pid": p.pid().as_u32(),
                    "name": p.name().to_string_lossy(),
                    "cpu": p.cpu_usage(),
                    "mem": p.memory(),
                })
            })
            .collect();
        let temps: Vec<_> = components
            .list()
            .iter()
            .filter_map(|c| {
                c.temperature()
                    .filter(|t| t.is_finite() && *t > 0.0)
                    .map(|t| json!({ "label": c.label(), "temp": t, "critical": c.critical() }))
            })
            .collect();
        payload["processes"] = json!({ "count": procs.processes().len(), "top": top });
        payload["temps"] = json!(temps);
    }
    let payload = payload.to_string();

    let point = [
        now.as_secs() as f64,
        cpu_total as f64,
        sys.used_memory() as f64,
        rx_rate,
        tx_rate,
        disk_read_rate,
        disk_write_rate,
        load.one,
    ];
    (payload, point)
}

pub async fn stats(State(state): State<AppState>, user: CurrentUser) -> AppResult<Response> {
    user.require(user.can_system())?;
    let body = state.sampler.rx.borrow().clone();
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response())
}

pub async fn stats_ws(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    check_ws_origin(&headers, &state)?;
    user.require(user.can_system())?;
    Ok(ws.on_upgrade(move |socket| stream_stats(socket, state)))
}

/// Client → server control message: `{"full": true}` while a page that shows
/// processes/sensors is open, `{"full": false}` when it closes.
#[derive(serde::Deserialize)]
struct StatsControl {
    full: bool,
}

async fn stream_stats(mut socket: WebSocket, state: AppState) {
    let sampler = &state.sampler;
    // Replay recent history first so charts are full the moment they open.
    let history = {
        let h = sampler.history.lock().unwrap();
        json!({
            "history": {
                "interval": sampler.interval.as_secs(),
                "points": h.iter().collect::<Vec<_>>(),
            }
        })
        .to_string()
    };
    if socket.send(Message::Text(history.into())).await.is_err() {
        return;
    }
    let mut rx = sampler.rx.clone();
    // Dropped with the socket, so a vanished tab can't pin full sampling on.
    let mut full: Option<FullGuard> = None;
    loop {
        let payload = rx.borrow_and_update().clone();
        if socket.send(Message::Text(payload.into())).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                changed = rx.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(text))) => {
                        if let Ok(ctl) = serde_json::from_str::<StatsControl>(&text) {
                            full = match (ctl.full, full.take()) {
                                (true, None) => Some(sampler.want_full()),
                                (true, guard) => guard,
                                (false, _) => None,
                            };
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => return,
                },
            }
        }
    }
}

pub async fn services(
    State(state): State<AppState>,
    user: CurrentUser,
) -> AppResult<Json<serde_json::Value>> {
    user.require(user.can_system())?;
    let mut out = Vec::new();
    for unit in &state.config.services {
        out.push(service_status(unit).await);
    }
    Ok(Json(json!(out)))
}

async fn service_status(unit: &str) -> serde_json::Value {
    let result = tokio::process::Command::new("systemctl")
        .args([
            "show",
            "--property=ActiveState,SubState,ActiveEnterTimestamp",
            unit,
        ])
        .output()
        .await;

    let mut active = "unknown".to_string();
    let mut sub = String::new();
    let mut since = String::new();
    if let Ok(output) = result {
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            match line.split_once('=') {
                Some(("ActiveState", v)) => active = v.to_string(),
                Some(("SubState", v)) => sub = v.to_string(),
                Some(("ActiveEnterTimestamp", v)) => since = v.to_string(),
                _ => {}
            }
        }
    }
    json!({ "unit": unit, "active_state": active, "sub_state": sub, "since": since })
}
