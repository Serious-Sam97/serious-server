//! Database backups: detection of database containers, streaming dumps and
//! restores through `docker exec`, cron schedules and retention.
//!
//! Storage is centralized on the master. An agent dumps into a small spool
//! and ships each finished file over the fleet link (`agent.rs`); the master
//! stores, catalogs and prunes (`master.rs`). The home server dumps straight
//! into its own storage.

pub mod agent;
pub mod master;
pub mod pitr;
pub mod wal;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::{OffsetDateTime, UtcOffset};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::auth::perms::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;

/// One database's backup policy. Owned by the master, cached by agents so a
/// schedule keeps running while the master is unreachable.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Policy {
    pub project: String,
    pub service: String,
    pub engine: String,
    /// `logical` (pg_dump). `continuous` (WAL) arrives in P3.
    pub mode: String,
    pub enabled: bool,
    /// Five-field cron: `minute hour day-of-month month day-of-week`.
    pub schedule: String,
    /// The schedule's clock, as minutes east of UTC (e.g. -180 for Brasília).
    pub utc_offset_min: i32,
    /// Optional `HH:MM-HH:MM` in the same clock: a missed run is only caught
    /// up inside it.
    pub window: String,
    pub keep_last: u32,
    pub keep_daily: u32,
    pub keep_weekly: u32,
    pub keep_monthly: u32,
    /// Dump read rate cap in KiB/s (0 = unlimited).
    pub max_rate_kbps: u32,
    /// Restore each scheduled backup into a throwaway Postgres on the master.
    #[serde(default = "yes")]
    pub verify: bool,
}

fn yes() -> bool {
    true
}

impl Policy {
    pub fn default_for(project: &str, service: &str) -> Policy {
        Policy {
            project: project.into(),
            service: service.into(),
            engine: "postgres".into(),
            mode: "logical".into(),
            enabled: true,
            schedule: "0 3 * * *".into(),
            utc_offset_min: 0,
            window: String::new(),
            keep_last: 7,
            keep_daily: 7,
            keep_weekly: 4,
            keep_monthly: 6,
            max_rate_kbps: 0,
            verify: true,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        Cron::parse(&self.schedule).map_err(|e| format!("schedule: {e}"))?;
        if !self.window.is_empty() {
            parse_window(&self.window).ok_or("window must look like 01:00-05:00")?;
        }
        if !(-14 * 60..=14 * 60).contains(&self.utc_offset_min) {
            return Err("utc offset out of range".into());
        }
        if self.engine != "postgres" {
            return Err("only postgres is supported".into());
        }
        if self.mode != "logical" && self.mode != "continuous" {
            return Err("mode must be logical or continuous".into());
        }
        if self.keep_last == 0 {
            return Err("keep_last must be at least 1".into());
        }
        Ok(())
    }

    pub fn offset(&self) -> UtcOffset {
        UtcOffset::from_whole_seconds(self.utc_offset_min * 60).unwrap_or(UtcOffset::UTC)
    }
}

/// What a backup file is, carried with it from the agent to the catalog.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct BackupMeta {
    pub project: String,
    pub service: String,
    pub engine: String,
    /// `logical`
    pub kind: String,
    /// `schedule`, `manual` or `pre-restore`
    pub trigger: String,
    pub started_at: i64,
    pub finished_at: i64,
    /// base: `wal_start=<segment>`; wal: the segment name.
    #[serde(default)]
    pub label: String,
    /// The database image, so the master can verify with the same version.
    #[serde(default)]
    pub image: String,
    #[serde(default)]
    pub db_user: String,
    #[serde(default)]
    pub database: String,
}

// ---------------------------------------------------------------------------
// Cron
// ---------------------------------------------------------------------------

/// Minimal five-field cron: `*`, `*/n`, `a`, `a-b`, `a-b/n` and lists.
#[derive(Debug, Clone)]
pub struct Cron {
    minute: [bool; 60],
    hour: [bool; 24],
    dom: [bool; 32],
    month: [bool; 13],
    dow: [bool; 7],
    dom_any: bool,
    dow_any: bool,
}

fn parse_field<const N: usize>(field: &str, lo: usize, hi: usize) -> Result<([bool; N], bool), String> {
    let mut set = [false; N];
    let any = field == "*";
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<usize>().map_err(|_| format!("bad step in {part:?}"))?),
            None => (part, 1),
        };
        if step == 0 {
            return Err(format!("zero step in {part:?}"));
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (
                a.parse().map_err(|_| format!("bad number in {part:?}"))?,
                b.parse().map_err(|_| format!("bad number in {part:?}"))?,
            )
        } else {
            let v = range.parse().map_err(|_| format!("bad number in {part:?}"))?;
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("{part:?} out of range {lo}-{hi}"));
        }
        for v in (a..=b).step_by(step) {
            set[v] = true;
        }
    }
    Ok((set, any))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return Err("expected 5 fields: minute hour day month weekday".into());
        }
        let (minute, _) = parse_field::<60>(f[0], 0, 59)?;
        let (hour, _) = parse_field::<24>(f[1], 0, 23)?;
        let (dom, dom_any) = parse_field::<32>(f[2], 1, 31)?;
        let (month, _) = parse_field::<13>(f[3], 1, 12)?;
        // 0 and 7 are both Sunday.
        let (dow8, dow_any) = parse_field::<8>(f[4], 0, 7)?;
        let mut dow = [false; 7];
        for (i, on) in dow8.iter().enumerate() {
            if *on {
                dow[i % 7] = true;
            }
        }
        Ok(Cron { minute, hour, dom, month, dow, dom_any, dow_any })
    }

    fn day_matches(&self, t: &OffsetDateTime) -> bool {
        let dom = self.dom[t.day() as usize];
        let dow = self.dow[t.weekday().number_days_from_sunday() as usize];
        // Classic cron: when both are restricted, either may match.
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            (false, false) => dom || dow,
        }
    }

    /// First matching minute strictly after `after` (unix secs), evaluated in `offset`.
    pub fn next_after(&self, after: i64, offset: UtcOffset) -> Option<i64> {
        let mut t = after - after.rem_euclid(60) + 60;
        let limit = after + 366 * 86_400 * 2;
        while t <= limit {
            let dt = OffsetDateTime::from_unix_timestamp(t).ok()?.to_offset(offset);
            if !self.month[u8::from(dt.month()) as usize] || !self.day_matches(&dt) {
                // Jump to the next local midnight.
                let into_day = dt.hour() as i64 * 3600 + dt.minute() as i64 * 60;
                t += 86_400 - into_day;
                continue;
            }
            if !self.hour[dt.hour() as usize] {
                t += 3600 - dt.minute() as i64 * 60;
                continue;
            }
            if self.minute[dt.minute() as usize] {
                return Some(t);
            }
            t += 60;
        }
        None
    }
}

fn parse_hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn parse_window(w: &str) -> Option<(u32, u32)> {
    let (a, b) = w.split_once('-')?;
    Some((parse_hm(a)?, parse_hm(b)?))
}

/// Is `t` inside the policy's window? An empty window means always.
pub fn in_window(policy: &Policy, t: i64) -> bool {
    let Some((a, b)) = parse_window(&policy.window) else {
        return true;
    };
    let Ok(dt) = OffsetDateTime::from_unix_timestamp(t) else {
        return true;
    };
    let dt = dt.to_offset(policy.offset());
    let m = dt.hour() as u32 * 60 + dt.minute() as u32;
    if a <= b { m >= a && m < b } else { m >= a || m < b }
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Which backups to keep: the newest `keep_last`, plus the newest one of each
/// of the last `keep_daily` days, `keep_weekly` ISO weeks and `keep_monthly`
/// months (in the policy's clock). `backups` = `(id, started_at)`.
pub fn retention_keep(policy: &Policy, backups: &[(i64, i64)]) -> Vec<i64> {
    let mut sorted = backups.to_vec();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));
    let mut keep: Vec<i64> = sorted.iter().take(policy.keep_last as usize).map(|b| b.0).collect();
    let offset = policy.offset();
    let bucket = |ts: i64, kind: u8| -> (i32, u32) {
        let dt = OffsetDateTime::from_unix_timestamp(ts)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH)
            .to_offset(offset);
        match kind {
            0 => (dt.year(), dt.ordinal() as u32),
            1 => {
                let (y, w, _) = dt.to_iso_week_date();
                (y, w as u32)
            }
            _ => (dt.year(), u8::from(dt.month()) as u32),
        }
    };
    for (kind, count) in [(0u8, policy.keep_daily), (1, policy.keep_weekly), (2, policy.keep_monthly)] {
        let mut seen = Vec::new();
        for (id, ts) in &sorted {
            let b = bucket(*ts, kind);
            if seen.contains(&b) {
                continue;
            }
            if seen.len() >= count as usize {
                break;
            }
            seen.push(b);
            if !keep.contains(id) {
                keep.push(*id);
            }
        }
    }
    keep
}

// ---------------------------------------------------------------------------
// Targets (database containers) and dump / restore
// ---------------------------------------------------------------------------

#[derive(Serialize, Clone, Debug)]
pub struct Target {
    pub project: String,
    pub service: String,
    pub container: String,
    pub container_id: String,
    pub engine: String,
    pub image: String,
    pub database: String,
    pub user: String,
    pub running: bool,
}

/// Postgres containers in compose projects. Detected by image, overridable
/// with labels: `serious.backup=false` opts out, `serious.backup.engine=postgres`
/// opts in a custom image.
pub async fn list_targets(docker: &bollard::Docker) -> anyhow::Result<Vec<Target>> {
    let opts = bollard::query_parameters::ListContainersOptionsBuilder::default()
        .all(true)
        .build();
    let containers = docker.list_containers(Some(opts)).await?;
    let mut out = Vec::new();
    for c in containers {
        let labels = c.labels.clone().unwrap_or_default();
        let (Some(project), Some(service)) = (
            labels.get("com.docker.compose.project"),
            labels.get("com.docker.compose.service"),
        ) else {
            continue;
        };
        if labels.get("serious.backup").is_some_and(|v| v == "false") {
            continue;
        }
        let image = c.image.clone().unwrap_or_default();
        let engine = match labels.get("serious.backup.engine") {
            Some(e) => e.clone(),
            None if is_postgres_image(&image) => "postgres".into(),
            None => continue,
        };
        if engine != "postgres" {
            continue;
        }
        let id = c.id.clone().unwrap_or_default();
        let env = container_env(docker, &id).await;
        let user = env.get("POSTGRES_USER").cloned().unwrap_or_else(|| "postgres".into());
        let database = env.get("POSTGRES_DB").cloned().unwrap_or_else(|| user.clone());
        out.push(Target {
            project: project.clone(),
            service: service.clone(),
            container: c
                .names
                .unwrap_or_default()
                .first()
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default(),
            container_id: id,
            engine,
            image,
            database,
            user,
            running: c
                .state
                .is_some_and(|s| format!("{s:?}").to_lowercase().contains("running")),
        });
    }
    out.sort_by(|a, b| (&a.project, &a.service).cmp(&(&b.project, &b.service)));
    Ok(out)
}

fn is_postgres_image(image: &str) -> bool {
    let name = image.rsplit('/').next().unwrap_or(image);
    let base = name.split([':', '@']).next().unwrap_or(name);
    // postgres, postgis/postgis, timescale/timescaledb… but not exporters.
    (base.starts_with("postgres") || base == "postgis" || base.starts_with("timescaledb"))
        && !base.contains("exporter")
}

async fn container_env(docker: &bollard::Docker, id: &str) -> HashMap<String, String> {
    docker
        .inspect_container(id, None::<bollard::query_parameters::InspectContainerOptions>)
        .await
        .ok()
        .and_then(|i| i.config)
        .and_then(|c| c.env)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())))
        .collect()
}

pub async fn find_target(docker: &bollard::Docker, project: &str, service: &str) -> anyhow::Result<Target> {
    list_targets(docker)
        .await?
        .into_iter()
        .find(|t| t.project == project && t.service == service)
        .ok_or_else(|| anyhow::anyhow!("no postgres container for {project}/{service}"))
}

/// Credentials come from the container's own environment, and the command
/// connects over the local socket inside it, so the client always matches
/// the server version and nothing secret leaves the container.
pub(super) const PG_ENV: &str = r#"export PGUSER="${POSTGRES_USER:-postgres}" PGPASSWORD="$POSTGRES_PASSWORD" PGDATABASE="${POSTGRES_DB:-${POSTGRES_USER:-postgres}}"; NICE=""; command -v nice >/dev/null && NICE="nice -n 19";"#;

impl Target {
    /// What a backup of this target carries in its metadata.
    pub fn meta(&self, kind: &str, trigger: &str, label: String) -> BackupMeta {
        let now = crate::fleet::now_secs();
        BackupMeta {
            project: self.project.clone(),
            service: self.service.clone(),
            engine: self.engine.clone(),
            kind: kind.into(),
            trigger: trigger.into(),
            started_at: now,
            finished_at: now,
            label,
            image: self.image.clone(),
            db_user: self.user.clone(),
            database: self.database.clone(),
        }
    }
}

/// Stream `pg_dump -Fc` from the container into `path`, hashing on the way.
/// `max_rate_kbps` throttles by reading slower (pg_dump then waits on its
/// pipe), which caps both disk and network pressure on the droplet.
pub async fn dump_to_file(
    docker: &bollard::Docker,
    target: &Target,
    path: &Path,
    max_rate_kbps: u32,
) -> anyhow::Result<(u64, String)> {
    anyhow::ensure!(target.running, "{} is not running", target.container);
    let cmd = format!("{PG_ENV} exec $NICE pg_dump -Fc --no-password");
    exec_to_file(docker, &target.container_id, &cmd, path, max_rate_kbps).await
}

/// Run `script` (sh) in a container and stream its stdout into `path`.
pub async fn exec_to_file(
    docker: &bollard::Docker,
    container_id: &str,
    cmd: &str,
    path: &Path,
    max_rate_kbps: u32,
) -> anyhow::Result<(u64, String)> {
    let exec = docker
        .create_exec(
            container_id,
            CreateExecOptions {
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec!["sh", "-c", cmd]),
                ..Default::default()
            },
        )
        .await?;
    let StartExecResults::Attached { mut output, .. } = docker
        .start_exec(&exec.id, Some(StartExecOptions { detach: false, tty: false, output_capacity: Some(1 << 20) }))
        .await?
    else {
        anyhow::bail!("exec detached unexpectedly");
    };
    let mut file = tokio::fs::File::create(path).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stderr = String::new();
    let started = Instant::now();
    while let Some(chunk) = output.next().await {
        match chunk? {
            bollard::container::LogOutput::StdOut { message } => {
                hasher.update(&message);
                file.write_all(&message).await?;
                size += message.len() as u64;
                throttle(size, started, max_rate_kbps).await;
            }
            bollard::container::LogOutput::StdErr { message } => {
                if stderr.len() < 4096 {
                    stderr.push_str(&String::from_utf8_lossy(&message));
                }
            }
            _ => {}
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    let code = docker.inspect_exec(&exec.id).await?.exit_code.unwrap_or(-1);
    if code != 0 {
        let _ = tokio::fs::remove_file(path).await;
        anyhow::bail!("exited {code}: {}", stderr.trim());
    }
    anyhow::ensure!(size > 0, "command produced no output");
    let sha = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((size, sha))
}

async fn throttle(bytes: u64, started: Instant, max_rate_kbps: u32) {
    if max_rate_kbps == 0 {
        return;
    }
    let should_take = Duration::from_secs_f64(bytes as f64 / (max_rate_kbps as f64 * 1024.0));
    let elapsed = started.elapsed();
    if should_take > elapsed {
        tokio::time::sleep(should_take - elapsed).await;
    }
}

/// Feed a `pg_dump -Fc` archive into `pg_restore --clean --if-exists` inside
/// the container. Returns pg_restore's output.
pub async fn restore_from_file(docker: &bollard::Docker, target: &Target, path: &Path) -> anyhow::Result<String> {
    anyhow::ensure!(target.running, "{} is not running", target.container);
    let cmd = format!(
        "{PG_ENV} exec pg_restore --clean --if-exists --no-owner --single-transaction --no-password -d \"$PGDATABASE\""
    );
    exec_with_stdin(docker, &target.container_id, &cmd, Some(path)).await
}

/// Run a short `sh` script in a container; error on a non-zero exit.
/// Returns stdout + stderr.
pub async fn exec_sh(docker: &bollard::Docker, container_id: &str, script: &str) -> anyhow::Result<String> {
    exec_with_stdin(docker, container_id, script, None).await
}

/// psql one statement as the container's own superuser, unaligned output.
pub async fn psql(docker: &bollard::Docker, container_id: &str, sql: &str) -> anyhow::Result<String> {
    let quoted = sql.replace('\'', "'\\''");
    let out = exec_sh(
        docker,
        container_id,
        &format!("{PG_ENV} exec psql -X -q -v ON_ERROR_STOP=1 -tA --no-password -c '{quoted}'"),
    )
    .await?;
    Ok(out.trim().to_string())
}

/// `sh -c script` in a container, optionally feeding a file to its stdin.
pub async fn exec_with_stdin(
    docker: &bollard::Docker,
    container_id: &str,
    cmd: &str,
    stdin: Option<&Path>,
) -> anyhow::Result<String> {
    let exec = docker
        .create_exec(
            container_id,
            CreateExecOptions {
                attach_stdin: Some(stdin.is_some()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec!["sh", "-c", cmd]),
                ..Default::default()
            },
        )
        .await?;
    let StartExecResults::Attached { mut output, mut input } =
        docker.start_exec(&exec.id, None::<StartExecOptions>).await?
    else {
        anyhow::bail!("exec detached unexpectedly");
    };
    let path = stdin.map(|p| p.to_path_buf());
    let feeder = tokio::spawn(async move {
        if let Some(path) = path {
            let mut file = tokio::fs::File::open(&path).await?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                input.write_all(&buf[..n]).await?;
            }
        }
        input.shutdown().await?;
        anyhow::Ok(())
    });
    let mut log = String::new();
    while let Some(chunk) = output.next().await {
        if let Ok(o) = chunk {
            if log.len() < 16 * 1024 {
                log.push_str(&String::from_utf8_lossy(&o.into_bytes()));
            }
        }
    }
    feeder.await??;
    let code = docker.inspect_exec(&exec.id).await?.exit_code.unwrap_or(-1);
    anyhow::ensure!(code == 0, "exited {code}: {}", log.trim());
    Ok(log)
}

pub async fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

// ---------------------------------------------------------------------------
// Scheduler
// ---------------------------------------------------------------------------

async fn last_run(state: &AppState, node: &str, p: &Policy) -> Option<i64> {
    let (n, pr, sv) = (node.to_string(), p.project.clone(), p.service.clone());
    state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT last_run FROM backup_last_run WHERE node = ?1 AND project = ?2 AND service = ?3",
                [&n, &pr, &sv],
                |r| r.get::<_, i64>(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await
        .ok()
        .flatten()
}

async fn set_last_run(state: &AppState, node: &str, p: &Policy, t: i64) {
    let (n, pr, sv) = (node.to_string(), p.project.clone(), p.service.clone());
    let _ = state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO backup_last_run (node, project, service, last_run) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (node, project, service) DO UPDATE SET last_run = excluded.last_run",
                rusqlite::params![n, pr, sv, t],
            )
        })
        .await;
}

/// Run each enabled policy on its cron schedule. Sleeps until the next due
/// time (or a policy change) instead of polling. A run missed while the
/// process was down is caught up at start — inside the policy's window, if
/// it has one; otherwise it is skipped and logged.
pub async fn scheduler_loop<F, Fut>(
    state: AppState,
    node: String,
    policies: std::sync::Arc<std::sync::Mutex<Vec<Policy>>>,
    changed: std::sync::Arc<tokio::sync::Notify>,
    run: F,
) where
    F: Fn(Policy) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    loop {
        let now = crate::fleet::now_secs();
        let list: Vec<Policy> = policies.lock().unwrap().iter().filter(|p| p.enabled).cloned().collect();
        let mut soonest: Option<i64> = None;
        for p in list {
            let Ok(cron) = Cron::parse(&p.schedule) else { continue };
            let last = match last_run(&state, &node, &p).await {
                Some(l) => l,
                None => {
                    // New policy: first run is the next scheduled time, not now.
                    set_last_run(&state, &node, &p, now).await;
                    now
                }
            };
            let Some(due) = cron.next_after(last, p.offset()) else { continue };
            if due <= now {
                set_last_run(&state, &node, &p, now).await;
                if now - due > 120 && !in_window(&p, now) {
                    tracing::warn!(
                        project = p.project,
                        service = p.service,
                        "missed a scheduled backup while down; outside its window, skipping to the next"
                    );
                } else {
                    tokio::spawn(run(p.clone()));
                }
                if let Some(next) = cron.next_after(now, p.offset()) {
                    soonest = Some(soonest.map_or(next, |s| s.min(next)));
                }
            } else {
                soonest = Some(soonest.map_or(due, |s| s.min(due)));
            }
        }
        let wait = soonest
            .map(|t| (t - crate::fleet::now_secs()).clamp(1, 3600))
            .unwrap_or(3600) as u64;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            _ = changed.notified() => {}
        }
    }
}

/// Node-local route (`/backups/targets`, tunneled on agents): database
/// containers in projects this user can see.
pub async fn targets(State(state): State<AppState>, user: CurrentUser) -> AppResult<Json<Vec<Target>>> {
    let all = list_targets(&state.docker).await?;
    Ok(Json(all.into_iter().filter(|t| user.project(&t.project).view).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        let fmt = time::format_description::well_known::Rfc3339;
        OffsetDateTime::parse(s, &fmt).unwrap().unix_timestamp()
    }

    #[test]
    fn cron_daily_at_three_in_brasilia() {
        let c = Cron::parse("0 3 * * *").unwrap();
        let off = UtcOffset::from_whole_seconds(-3 * 3600).unwrap();
        // 2026-09-30 10:00 UTC = 07:00 BRT → next 03:00 BRT = 06:00 UTC on Oct 1.
        assert_eq!(c.next_after(ts("2026-09-30T10:00:00Z"), off), Some(ts("2026-10-01T06:00:00Z")));
    }

    #[test]
    fn cron_steps_lists_and_weekday() {
        let c = Cron::parse("*/15 * * * *").unwrap();
        assert_eq!(c.next_after(ts("2026-09-30T10:07:00Z"), UtcOffset::UTC), Some(ts("2026-09-30T10:15:00Z")));
        let c = Cron::parse("30 2 * * 0").unwrap(); // Sundays 02:30
        assert_eq!(c.next_after(ts("2026-09-30T10:00:00Z"), UtcOffset::UTC), Some(ts("2026-10-04T02:30:00Z")));
        let c = Cron::parse("0 1,13 * * 1-5").unwrap();
        assert_eq!(c.next_after(ts("2026-10-02T13:00:00Z"), UtcOffset::UTC), Some(ts("2026-10-05T01:00:00Z")));
        assert!(Cron::parse("61 * * * *").is_err());
        assert!(Cron::parse("* * *").is_err());
    }

    #[test]
    fn window_wraps_midnight() {
        let mut p = Policy::default_for("a", "b");
        p.window = "23:00-02:00".into();
        assert!(in_window(&p, ts("2026-09-30T23:30:00Z")));
        assert!(in_window(&p, ts("2026-10-01T01:59:00Z")));
        assert!(!in_window(&p, ts("2026-10-01T02:00:00Z")));
    }

    #[test]
    fn retention_keeps_last_and_one_per_day() {
        let mut p = Policy::default_for("a", "b");
        p.keep_last = 2;
        p.keep_daily = 3;
        p.keep_weekly = 0;
        p.keep_monthly = 0;
        let day = 86_400;
        let base = ts("2026-09-30T03:00:00Z");
        // Three backups today, one each of the previous 4 days.
        let b = vec![
            (1, base),
            (2, base + 60),
            (3, base + 120),
            (4, base - day),
            (5, base - 2 * day),
            (6, base - 3 * day),
            (7, base - 4 * day),
        ];
        let mut keep = retention_keep(&p, &b);
        keep.sort();
        // last 2 = {3, 2}; daily 3 = {3 (today), 4, 5}.
        assert_eq!(keep, vec![2, 3, 4, 5]);
    }
}
