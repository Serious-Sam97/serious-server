//! Continuous backups on an agent: stream WAL with `pg_receivewal` (run via
//! exec inside the database container itself, so the client always matches
//! the server and replication uses the local socket), ship each finished
//! segment, take scheduled base backups, and perform point-in-time restores.

use std::sync::Arc;
use std::time::Duration;

use bollard::exec::{CreateExecOptions, StartExecResults};
use futures_util::StreamExt;

use super::agent::{random_id, BackupAgent};
use super::{exec_sh, exec_to_file, find_target, pitr, psql, Target, PG_ENV};
use crate::fleet::{now_secs, AgentMsg};

/// Where pg_receivewal writes, inside the database container (a buffer: the
/// agent copies each finished segment out and deletes it).
const WAL_DIR: &str = "/var/lib/postgresql/serious-wal";
/// Force a segment switch this often when there were writes, so the data-loss
/// window stays ≤ 5 min even on a quiet database.
const SWITCH_EVERY: Duration = Duration::from_secs(300);
/// Safety net in case a "finished segment" line is missed.
const COLLECT_EVERY: Duration = Duration::from_secs(60);
/// Caps WAL held for a disconnected stream, so the droplet's disk can't fill.
const SLOT_CAP: &str = "10GB";

fn slot_name(service: &str) -> String {
    let clean: String = service
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(48)
        .collect();
    format!("serious_{clean}")
}

impl BackupAgent {
    /// Start or stop WAL streams to match the continuous policies.
    pub(super) fn reconcile_streams(self: &Arc<Self>) {
        let wanted: Vec<(String, String)> = if self.state.config.agent_allow.iter().any(|c| c == "backups") {
            self.policies
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.enabled && p.mode == "continuous")
                .map(|p| (p.project.clone(), p.service.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let mut streams = self.streams.lock().unwrap();
        streams.retain(|key, handle| {
            let keep = wanted.contains(key);
            if !keep {
                tracing::info!(project = key.0, service = key.1, "WAL stream stopped");
                handle.abort();
            }
            keep
        });
        for key in wanted {
            if !streams.contains_key(&key) {
                let task = tokio::spawn(self.clone().wal_supervisor(key.0.clone(), key.1.clone()));
                streams.insert(key, task.abort_handle());
            }
        }
    }

    fn is_paused(&self, project: &str, service: &str) -> bool {
        self.paused.lock().unwrap().contains(&(project.to_string(), service.to_string()))
    }

    async fn wal_supervisor(self: Arc<Self>, project: String, service: String) {
        loop {
            if self.is_paused(&project, &service) {
                tokio::time::sleep(Duration::from_secs(3)).await;
                continue;
            }
            if let Err(e) = self.wal_session(&project, &service).await {
                tracing::warn!(project, service, "WAL stream: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    }

    /// Prepare the slot, then run pg_receivewal until it exits.
    async fn wal_session(self: &Arc<Self>, project: &str, service: &str) -> anyhow::Result<()> {
        let docker = &self.state.docker;
        let target = find_target(docker, project, service).await?;
        anyhow::ensure!(target.running, "{} is not running", target.container);
        let cid = &target.container_id;
        let slot = slot_name(service);

        let wal_level = psql(docker, cid, "SHOW wal_level").await?;
        anyhow::ensure!(wal_level != "minimal", "wal_level is 'minimal' — set it to 'replica' to stream WAL");
        if psql(docker, cid, "SHOW max_slot_wal_keep_size").await.is_ok_and(|v| v == "-1") {
            psql(docker, cid, &format!("ALTER SYSTEM SET max_slot_wal_keep_size = '{SLOT_CAP}'")).await?;
            psql(docker, cid, "SELECT pg_reload_conf()").await?;
            tracing::info!(project, service, "capped replication slot WAL at {SLOT_CAP}");
        }
        let status = psql(
            docker,
            cid,
            &format!("SELECT coalesce(wal_status, 'unknown') FROM pg_replication_slots WHERE slot_name = '{slot}'"),
        )
        .await?;
        let mut fresh = false;
        if status == "lost" {
            // The cap was hit while we were away: the chain has a hole.
            psql(docker, cid, &format!("SELECT pg_drop_replication_slot('{slot}')")).await?;
            self.gap_event(project, service, "replication slot exceeded its WAL cap and was invalidated").await;
        }
        if status.is_empty() || status == "lost" {
            psql(docker, cid, &format!("SELECT pg_create_physical_replication_slot('{slot}', true)")).await?;
            fresh = true;
        }
        exec_sh(docker, cid, &format!("mkdir -p {WAL_DIR}")).await?;
        // Ship anything left from a previous run first.
        self.collect(&target).await?;
        if fresh {
            // A new chain (after a restore or a lost slot): a leftover .partial
            // from the old timeline would make pg_receivewal ask the server
            // for a position that no longer exists.
            exec_sh(docker, cid, &format!("rm -f {WAL_DIR}/*")).await?;
        }
        if fresh {
            // A new chain starts with a base backup.
            let a = self.clone();
            let (p, s) = (project.to_string(), service.to_string());
            tokio::spawn(async move {
                if let Err(e) = a.take_base_backup(&p, &s, "chain-start").await {
                    tracing::warn!("base backup for new WAL chain: {e:#}");
                }
            });
        }

        let cmd = format!(
            "{PG_ENV} exec pg_receivewal -D {WAL_DIR} -S {slot} -Z 3 -n -v --no-password"
        );
        let exec = docker
            .create_exec(
                cid,
                CreateExecOptions {
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    cmd: Some(vec!["sh", "-c", cmd.as_str()]),
                    ..Default::default()
                },
            )
            .await?;
        let StartExecResults::Attached { mut output, .. } = docker.start_exec(&exec.id, None).await? else {
            anyhow::bail!("exec detached unexpectedly");
        };
        tracing::info!(project, service, slot, "WAL stream running");

        let mut collect = tokio::time::interval(COLLECT_EVERY);
        collect.tick().await;
        let mut switch = tokio::time::interval(SWITCH_EVERY);
        switch.tick().await;
        let mut last_switch_lsn = String::new();
        let mut tail = String::new();
        loop {
            tokio::select! {
                chunk = output.next() => match chunk {
                    Some(Ok(out)) => {
                        let text = String::from_utf8_lossy(&out.into_bytes()).to_string();
                        tail.push_str(&text);
                        if tail.len() > 4096 {
                            tail = tail[tail.len() - 2048..].to_string();
                        }
                        if text.contains("finished segment") {
                            self.collect(&target).await?;
                        }
                    }
                    _ => break,
                },
                _ = collect.tick() => self.collect(&target).await?,
                _ = switch.tick() => {
                    let lsn = psql(docker, cid, "SELECT pg_current_wal_insert_lsn()").await?;
                    if lsn != last_switch_lsn {
                        psql(docker, cid, "SELECT pg_switch_wal()").await?;
                        last_switch_lsn = psql(docker, cid, "SELECT pg_current_wal_insert_lsn()").await?;
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(1)), if self.is_paused(project, service) => break,
            }
        }
        // pg_receivewal exited: work out why.
        let code = docker.inspect_exec(&exec.id).await.ok().and_then(|i| i.exit_code);
        let _ = self.collect(&target).await;
        if tail.contains("has already been removed") || tail.contains("invalidated") {
            psql(docker, cid, &format!("SELECT pg_drop_replication_slot('{slot}')")).await.ok();
            self.gap_event(project, service, "WAL needed by the stream was already removed").await;
        }
        anyhow::bail!("pg_receivewal exited ({code:?}): {}", tail.lines().last().unwrap_or("").trim())
    }

    async fn gap_event(&self, project: &str, service: &str, why: &str) {
        tracing::warn!(project, service, "WAL chain broken: {why}; a new base backup starts a new chain");
        self.send(&AgentMsg::Event {
            kind: "wal_gap".into(),
            detail: serde_json::json!({ "project": project, "service": service, "detail": why }),
        })
        .await;
    }

    /// Copy every finished segment out of the container into the spool.
    async fn collect(&self, target: &Target) -> anyhow::Result<()> {
        let docker = &self.state.docker;
        let cid = &target.container_id;
        let list = exec_sh(docker, cid, &format!("ls -1 {WAL_DIR} 2>/dev/null || true")).await?;
        let mut done: Vec<&str> = list.lines().map(str::trim).filter(|n| n.ends_with(".gz")).collect();
        done.sort();
        for name in done {
            let segment = name.trim_end_matches(".gz");
            if segment.len() != 24 || !segment.chars().all(|c| c.is_ascii_hexdigit()) {
                continue; // history files etc. are recreated by recovery
            }
            let id = random_id();
            let path = self.spool.join(format!("{id:016x}.wal.gz"));
            let (size, sha) = exec_to_file(docker, cid, &format!("cat '{WAL_DIR}/{name}'"), &path, 0).await?;
            let meta = target.meta("wal", "stream", segment.to_string());
            self.spool_file(id, &meta, &path, size, sha).await?;
            exec_sh(docker, cid, &format!("rm -f '{WAL_DIR}/{name}'")).await?;
        }
        Ok(())
    }

    /// `pg_basebackup` (tar, gzip level 1, no WAL — the stream has it) into the spool.
    pub(super) async fn take_base_backup(&self, project: &str, service: &str, trigger: &str) -> anyhow::Result<()> {
        let docker = &self.state.docker;
        let mut meta = super::agent::bare_meta(project, service, "base", trigger);
        let result = async {
            anyhow::ensure!(
                self.state.config.agent_allow.iter().any(|c| c == "backups"),
                "backups are not allowed on this node (SS_AGENT_ALLOW)"
            );
            let _slot = self.dump_lock.acquire().await?;
            let target = find_target(docker, project, service).await?;
            anyhow::ensure!(target.running, "{} is not running", target.container);
            let wal_start = psql(docker, &target.container_id, "SELECT pg_walfile_name(pg_current_wal_lsn())").await?;
            meta = target.meta("base", trigger, format!("wal_start={wal_start}"));
            let rate = self
                .policies
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.project == project && p.service == service)
                .map(|p| p.max_rate_kbps)
                .unwrap_or(0);
            let max_rate = if rate > 0 { format!(" --max-rate={}k", rate.max(32)) } else { String::new() };
            let id = random_id();
            let path = self.spool.join(format!("{id:016x}.base.tar.gz"));
            let cmd = format!("{PG_ENV} exec $NICE pg_basebackup -D - -Ft -X none -z -Z 1 -c fast --no-password{max_rate}");
            let (size, sha) = exec_to_file(docker, &target.container_id, &cmd, &path, 0).await?;
            // The segment holding the backup's end: restores need WAL up to here.
            let wal_end = psql(docker, &target.container_id, "SELECT pg_walfile_name(pg_current_wal_lsn())").await?;
            meta.label = format!("wal_start={wal_start};wal_end={wal_end}");
            meta.finished_at = now_secs();
            self.spool_file(id, &meta, &path, size, sha).await?;
            // Close the segment holding the backup's end, so the chain is
            // restorable (and verifiable) within minutes instead of at the next switch.
            psql(docker, &target.container_id, "SELECT pg_switch_wal()").await?;
            tracing::info!(project, service, size, trigger, "base backup spooled");
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = &result {
            self.send(&AgentMsg::BackupFailed { meta, error: format!("{e:#}") }).await;
        }
        result
    }

    /// Replace the database's data with a base backup + WAL replayed to
    /// `target_time`. A full copy of the current data is kept in a new
    /// volume first; any failure after that point rolls back to it.
    pub(super) async fn pitr_restore(
        self: &Arc<Self>,
        rid: u64,
        project: &str,
        service: &str,
        bundle: &std::path::Path,
        target_time: Option<i64>,
    ) -> anyhow::Result<String> {
        let docker = &self.state.docker;
        let key = (project.to_string(), service.to_string());
        let target = find_target(docker, project, service).await?;
        let layout = pitr::layout(docker, &target.container_id).await?;
        let _slot = self.dump_lock.acquire().await?;
        self.paused.lock().unwrap().insert(key.clone());
        let result = async {
            self.status(rid, "stopping", None, "").await;
            docker
                .stop_container(
                    &target.container_id,
                    Some(bollard::query_parameters::StopContainerOptionsBuilder::default().t(60).build()),
                )
                .await?;
            let stamp = time::OffsetDateTime::now_utc()
                .format(&time::macros::format_description!("[year][month][day]-[hour][minute][second]"))?;
            let rollback = format!("serious-rollback-{project}-{service}-{stamp}")
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
                .collect::<String>();
            self.status(rid, "snapshot", None, &format!("copying current data to volume {rollback}")).await;
            pitr::snapshot_to_volume(docker, &target.image, &layout, &rollback).await?;

            let restored = async {
                self.status(rid, "materializing", None, "").await;
                pitr::materialize(docker, &target.image, &layout, bundle, target_time).await?;
                self.status(rid, "recovering", None, "replaying WAL").await;
                docker
                    .start_container(&target.container_id, None::<bollard::query_parameters::StartContainerOptions>)
                    .await?;
                pitr::wait_promoted(docker, &target.container_id, Duration::from_secs(1800)).await?;
                pitr::cleanup_after_recovery(docker, &target.container_id, &layout.pgdata).await?;
                anyhow::Ok(())
            }
            .await;
            if let Err(e) = restored {
                self.status(rid, "rolling-back", None, &format!("{e:#}")).await;
                let _ = docker
                    .stop_container(&target.container_id, None::<bollard::query_parameters::StopContainerOptions>)
                    .await;
                pitr::rollback_from_volume(docker, &target.image, &layout, &rollback).await?;
                docker
                    .start_container(&target.container_id, None::<bollard::query_parameters::StartContainerOptions>)
                    .await?;
                anyhow::bail!("restore failed and the previous data was put back: {e:#}");
            }
            anyhow::Ok(format!(
                "recovered{}; previous data kept in docker volume {rollback} (delete it when you're satisfied)",
                target_time.map(|_| " to the requested time").unwrap_or(" to the latest WAL")
            ))
        }
        .await;
        // The restored cluster has no replication slot (base backups exclude
        // them): the stream recreates it and starts a new chain.
        self.paused.lock().unwrap().remove(&key);
        result
    }
}
