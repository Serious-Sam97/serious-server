use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use sysinfo::{
    Components, Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System,
};

use crate::auth::check_ws_origin;
use crate::auth::perms::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// 30 minutes of samples, replayed to every new dashboard connection so a
/// reload (or a monitor waking up) doesn't start from an empty chart.
const HISTORY_LEN: usize = 900;
const TOP_PROCESSES: usize = 8;

/// One compact history sample:
/// `[unix_secs, cpu_pct, mem_used, rx_rate, tx_rate, disk_read_rate, disk_write_rate, load1]`.
pub type HistoryPoint = [f64; 8];
pub type History = Arc<Mutex<VecDeque<HistoryPoint>>>;

/// One sampler for the whole process; every client reads from the watch
/// channel instead of paying for its own sysinfo refresh.
pub fn spawn_sampler() -> (tokio::sync::watch::Receiver<String>, History) {
    let (tx, rx) = tokio::sync::watch::channel(String::from("{}"));
    let history: History = Arc::new(Mutex::new(VecDeque::with_capacity(HISTORY_LEN)));
    let hist = history.clone();
    // sysinfo refreshes do blocking syscalls; keep them off the async runtime.
    std::thread::spawn(move || {
        let mut sys = System::new();
        // Processes get their own System: a process refresh also refreshes the
        // CPU times it divides by, and sharing them with `refresh_cpu_all`
        // shrinks that window to a fraction of the interval — inflating both
        // per-process and global CPU%.
        let mut procs = System::new();
        let mut disks = Disks::new_with_refreshed_list();
        let mut networks = Networks::new_with_refreshed_list();
        let mut components = Components::new_with_refreshed_list();
        let mut disk_io = DiskIo::default();
        let host = host_info();
        // Threads are listed as processes unless excluded, which multiplies
        // the /proc walk (and the sampler's own CPU cost) by ~10x.
        let proc_kind = ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .without_tasks();
        loop {
            sys.refresh_cpu_all();
            sys.refresh_memory();
            procs.refresh_processes_specifics(ProcessesToUpdate::All, true, proc_kind);
            disks.refresh(true);
            networks.refresh(true);
            components.refresh(false);
            let io = disk_io.sample();

            let (payload, point) =
                build_payload(&sys, &procs, &disks, &networks, &components, &host, io);
            {
                let mut h = hist.lock().unwrap();
                if h.len() == HISTORY_LEN {
                    h.pop_front();
                }
                h.push_back(point);
            }
            if tx.send(payload).is_err() {
                return; // all receivers gone — process shutting down
            }
            std::thread::sleep(SAMPLE_INTERVAL);
        }
    });
    (rx, history)
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
        let is_disk = std::path::Path::new("/sys/block").join(name).exists();
        let virtual_dev = ["loop", "ram", "zram", "dm-", "md"]
            .iter()
            .any(|p| name.starts_with(p));
        if !is_disk || virtual_dev {
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

/// Best guess at "the CPU temperature": the package/die sensor when the
/// platform exposes one, otherwise the hottest sensor we can read.
fn cpu_temp(components: &Components) -> Option<f32> {
    let readings: Vec<(&str, f32)> = components
        .list()
        .iter()
        .filter_map(|c| c.temperature().map(|t| (c.label(), t)))
        .filter(|(_, t)| t.is_finite() && *t > 0.0)
        .collect();
    let preferred = ["package", "tctl", "tdie", "cpu"];
    preferred
        .iter()
        .find_map(|key| {
            readings
                .iter()
                .find(|(label, _)| label.to_lowercase().contains(key))
                .map(|(_, t)| *t)
        })
        .or_else(|| readings.iter().map(|(_, t)| *t).reduce(f32::max))
}

fn build_payload(
    sys: &System,
    procs: &System,
    disks: &Disks,
    networks: &Networks,
    components: &Components,
    host: &serde_json::Value,
    (disk_read_rate, disk_write_rate): (f64, f64),
) -> (String, HistoryPoint) {
    let load = System::load_average();
    let interval = SAMPLE_INTERVAL.as_secs_f64();
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

    let net_list: Vec<_> = networks
        .list()
        .iter()
        .filter(|(name, _)| {
            // Skip loopback and the per-container virtual interfaces docker
            // creates — they double-count traffic already seen on the uplink.
            *name != "lo"
                && !name.starts_with("veth")
                && !name.starts_with("br-")
                && !name.starts_with("docker")
        })
        .map(|(name, data)| {
            json!({
                "iface": name,
                // bytes since last refresh -> bytes/sec
                "rx_rate": (data.received() as f64 / interval) as u64,
                "tx_rate": (data.transmitted() as f64 / interval) as u64,
                "rx_total": data.total_received(),
                "tx_total": data.total_transmitted(),
            })
        })
        .collect();
    let rx_rate: f64 = net_list
        .iter()
        .map(|n| n["rx_rate"].as_u64().unwrap_or(0) as f64)
        .sum();
    let tx_rate: f64 = net_list
        .iter()
        .map(|n| n["tx_rate"].as_u64().unwrap_or(0) as f64)
        .sum();

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

    let cpu_total = sys.global_cpu_usage();
    let payload = json!({
        "ts": now.as_millis() as u64,
        "host": host,
        "cpu": {
            "total": cpu_total,
            "per_core": sys.cpus().iter().map(|c| c.cpu_usage()).collect::<Vec<f32>>(),
            "temp": cpu_temp(components),
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
        "temps": temps,
        "processes": { "count": procs.processes().len(), "top": top },
        "load": [load.one, load.five, load.fifteen],
        "uptime": System::uptime(),
    })
    .to_string();

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
    let body = state.stats_rx.borrow().clone();
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

async fn stream_stats(mut socket: WebSocket, state: AppState) {
    // Replay recent history first so charts are full the moment they open.
    let history = {
        let h = state.stats_history.lock().unwrap();
        json!({
            "history": {
                "interval": SAMPLE_INTERVAL.as_secs(),
                "points": h.iter().collect::<Vec<_>>(),
            }
        })
        .to_string()
    };
    if socket.send(Message::Text(history.into())).await.is_err() {
        return;
    }
    let mut rx = state.stats_rx.clone();
    loop {
        let payload = rx.borrow_and_update().clone();
        if socket.send(Message::Text(payload.into())).await.is_err() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
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
