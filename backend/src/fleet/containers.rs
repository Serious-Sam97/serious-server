//! Per-container usage, sampled once a minute on every node (agents ship it,
//! the master records its own). CPU% is the average over the minute — the
//! delta between two one-shot readings — not an instantaneous spike.

use std::collections::HashMap;
use std::time::Instant;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

pub const EVERY: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ContainerStat {
    pub project: String,
    pub service: String,
    pub container: String,
    /// % of one core, like `docker stats` (can exceed 100 on multi-core hosts).
    pub cpu: f32,
    /// Bytes, excluding reclaimable page cache (as `docker stats` shows it).
    pub mem: u64,
    pub mem_limit: u64,
    /// Bytes/s over the minute.
    pub rx: f32,
    pub tx: f32,
}

struct Prev {
    cpu: u64,
    system: u64,
    rx: u64,
    tx: u64,
    at: Instant,
}

#[derive(Default)]
pub struct Collector {
    prev: HashMap<String, Prev>,
}

impl Collector {
    /// One reading per running container. A container's first reading only
    /// primes the deltas, so it shows up from the second minute on.
    pub async fn sample(&mut self, docker: &bollard::Docker) -> Vec<ContainerStat> {
        let opts = bollard::query_parameters::ListContainersOptionsBuilder::default().all(false).build();
        let Ok(list) = docker.list_containers(Some(opts)).await else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for c in list {
            let Some(id) = c.id.clone() else { continue };
            seen.push(id.clone());
            let labels = c.labels.clone().unwrap_or_default();
            let name = c
                .names
                .unwrap_or_default()
                .first()
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default();
            let stats_opts = bollard::query_parameters::StatsOptionsBuilder::default()
                .stream(false)
                .one_shot(true)
                .build();
            let Some(Ok(s)) = docker.stats(&id, Some(stats_opts)).next().await else { continue };
            let cpu_stats = s.cpu_stats.as_ref();
            let total = cpu_stats.and_then(|c| c.cpu_usage.as_ref()).and_then(|u| u.total_usage).unwrap_or(0);
            let system = cpu_stats.and_then(|c| c.system_cpu_usage).unwrap_or(0);
            let cores = cpu_stats.and_then(|c| c.online_cpus).unwrap_or(1).max(1) as f64;
            let mem_stats = s.memory_stats.as_ref();
            let usage = mem_stats.and_then(|m| m.usage).unwrap_or(0);
            let cache = mem_stats
                .and_then(|m| m.stats.as_ref())
                .and_then(|st| {
                    st.get("inactive_file")
                        .or_else(|| st.get("total_inactive_file"))
                        .or_else(|| st.get("cache"))
                        .copied()
                })
                .unwrap_or(0);
            let limit = mem_stats.and_then(|m| m.limit).unwrap_or(0);
            let (rx, tx) = s
                .networks
                .as_ref()
                .map(|n| {
                    n.values().fold((0u64, 0u64), |(r, t), v| {
                        (r + v.rx_bytes.unwrap_or(0), t + v.tx_bytes.unwrap_or(0))
                    })
                })
                .unwrap_or((0, 0));
            let now = Instant::now();
            if let Some(p) = self.prev.get(&id) {
                let secs = now.duration_since(p.at).as_secs_f64().max(1.0);
                let sys_delta = system.saturating_sub(p.system) as f64;
                let cpu = if sys_delta > 0.0 {
                    total.saturating_sub(p.cpu) as f64 / sys_delta * cores * 100.0
                } else {
                    0.0
                };
                out.push(ContainerStat {
                    project: labels.get("com.docker.compose.project").cloned().unwrap_or_default(),
                    service: labels.get("com.docker.compose.service").cloned().unwrap_or_default(),
                    container: name,
                    cpu: cpu as f32,
                    mem: usage.saturating_sub(cache),
                    mem_limit: limit,
                    rx: (rx.saturating_sub(p.rx) as f64 / secs) as f32,
                    tx: (tx.saturating_sub(p.tx) as f64 / secs) as f32,
                });
            }
            self.prev.insert(id, Prev { cpu: total, system, rx, tx, at: now });
        }
        self.prev.retain(|id, _| seen.contains(id));
        out
    }
}
