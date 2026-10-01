//! Agent side of backups: run the cached schedule, dump into a small spool,
//! ship each file to the master over the fleet link (windowed, resumable),
//! and carry out restores the master sends.
//!
//! The spool is a buffer, not storage: a file is deleted as soon as the
//! master confirms it stored a verified copy.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio_tungstenite::tungstenite::Message;

use super::{dump_to_file, find_target, restore_from_file, sha256_file, BackupMeta, Policy};
use crate::fleet::{frame, now_secs, AgentMsg, MasterMsg, CHUNK, FRAME_UPLOAD, WINDOW};
use crate::state::AppState;

const ACK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ATTEMPTS: i64 = 3;

/// Metadata before the target is known (for failure reports).
pub fn bare_meta(project: &str, service: &str, kind: &str, trigger: &str) -> BackupMeta {
    let now = now_secs();
    BackupMeta {
        project: project.into(),
        service: service.into(),
        engine: "postgres".into(),
        kind: kind.into(),
        trigger: trigger.into(),
        started_at: now,
        finished_at: now,
        label: String::new(),
        image: String::new(),
        db_user: String::new(),
        database: String::new(),
    }
}

pub fn random_id() -> u64 {
    use rand::RngExt;
    rand::rng().random::<u64>() >> 1 // fits an SQLite INTEGER
}

/// Senders into the live fleet link: control messages and bulk frames.
#[derive(Clone)]
pub struct LinkTx {
    pub ctrl: mpsc::Sender<Message>,
    pub bulk: mpsc::Sender<Vec<u8>>,
}

enum UploadEvent {
    Ack { id: u64, offset: u64 },
    Done { id: u64, ok: bool, error: Option<String> },
}

/// Next upload event for `id` (others are stale and dropped).
async fn next_event(events: &mut mpsc::Receiver<UploadEvent>, id: u64) -> anyhow::Result<UploadEvent> {
    loop {
        match tokio::time::timeout(ACK_TIMEOUT, events.recv()).await {
            Ok(Some(ev)) => match &ev {
                UploadEvent::Ack { id: i, .. } | UploadEvent::Done { id: i, .. } if *i == id => return Ok(ev),
                _ => continue,
            },
            _ => anyhow::bail!("no answer from master"),
        }
    }
}

pub struct BackupAgent {
    pub(super) state: AppState,
    pub(super) spool: PathBuf,
    pub(super) policies: Arc<Mutex<Vec<Policy>>>,
    changed: Arc<Notify>,
    link: Mutex<Option<LinkTx>>,
    link_up: Notify,
    spool_changed: Notify,
    upload_tx: mpsc::Sender<UploadEvent>,
    restore_rx: Mutex<Option<(u64, mpsc::Sender<(u64, Vec<u8>)>)>>,
    pub(super) dump_lock: Semaphore,
    /// Running WAL streams (continuous policies), by (project, service).
    pub(super) streams: Mutex<std::collections::HashMap<(String, String), tokio::task::AbortHandle>>,
    /// Streams paused while a restore owns the database.
    pub(super) paused: Mutex<std::collections::HashSet<(String, String)>>,
}

struct SpoolEntry {
    id: u64,
    meta: BackupMeta,
    path: PathBuf,
    size: u64,
    sha256: String,
    attempts: i64,
}

impl BackupAgent {
    pub fn spawn(state: AppState) -> Arc<Self> {
        let cached: Vec<Policy> = std::fs::read_to_string(state.config.data_dir.join("policies.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        let (upload_tx, upload_rx) = mpsc::channel(256);
        let agent = Arc::new(BackupAgent {
            spool: state.config.backup_dir.clone(),
            state: state.clone(),
            policies: Arc::new(Mutex::new(cached)),
            changed: Arc::new(Notify::new()),
            link: Mutex::new(None),
            link_up: Notify::new(),
            spool_changed: Notify::new(),
            upload_tx,
            restore_rx: Mutex::new(None),
            dump_lock: Semaphore::new(1),
            streams: Mutex::new(Default::default()),
            paused: Mutex::new(Default::default()),
        });
        if let Err(e) = std::fs::create_dir_all(&agent.spool) {
            tracing::error!("backup spool {}: {e}", agent.spool.display());
        }
        let runner = agent.clone();
        tokio::spawn(super::scheduler_loop(
            state,
            String::new(),
            agent.policies.clone(),
            agent.changed.clone(),
            move |p: Policy| {
                let a = runner.clone();
                async move {
                    if p.mode == "logical" {
                        let _ = a.take_backup(&p.project, &p.service, "schedule").await;
                    } else {
                        let _ = a.take_base_backup(&p.project, &p.service, "schedule").await;
                    }
                }
            },
        ));
        tokio::spawn(agent.clone().uploader(upload_rx));
        agent.reconcile_streams();
        agent
    }

    fn allowed(&self) -> bool {
        self.state.config.agent_allow.iter().any(|c| c == "backups")
    }

    pub fn link_up(&self, link: LinkTx) {
        *self.link.lock().unwrap() = Some(link);
        self.link_up.notify_one();
    }

    pub fn link_down(&self) {
        *self.link.lock().unwrap() = None;
    }

    pub(super) async fn send(&self, msg: &AgentMsg) -> bool {
        let ctrl = self.link.lock().unwrap().as_ref().map(|l| l.ctrl.clone());
        match ctrl {
            Some(tx) => tx
                .send(Message::Text(serde_json::to_string(msg).expect("serialize").into()))
                .await
                .is_ok(),
            None => false,
        }
    }

    /// Backup-related messages from the master.
    pub async fn handle(self: &Arc<Self>, msg: MasterMsg) {
        match msg {
            MasterMsg::Policies { policies } => {
                let policies = if self.allowed() { policies } else { Vec::new() };
                let path = self.state.config.data_dir.join("policies.json");
                if let Ok(text) = serde_json::to_string_pretty(&policies) {
                    let _ = tokio::fs::write(path, text).await;
                }
                *self.policies.lock().unwrap() = policies;
                self.changed.notify_one();
                self.reconcile_streams();
            }
            MasterMsg::RunBackup { project, service, trigger } => {
                let a = self.clone();
                tokio::spawn(async move {
                    let _ = a.take_backup(&project, &service, &trigger).await;
                });
            }
            MasterMsg::UploadAck { id, offset } => {
                let _ = self.upload_tx.try_send(UploadEvent::Ack { id, offset });
            }
            MasterMsg::UploadDone { id, ok, error } => {
                let _ = self.upload_tx.try_send(UploadEvent::Done { id, ok, error });
            }
            MasterMsg::RestoreStart { rid, project, service, size, sha256, kind, target_time } => {
                let a = self.clone();
                tokio::spawn(async move {
                    if let Err(e) =
                        a.clone().restore(rid, &project, &service, size, &sha256, &kind, target_time).await
                    {
                        tracing::warn!("restore {project}/{service}: {e:#}");
                        a.status(rid, "failed", Some(false), &format!("{e:#}")).await;
                    }
                });
            }
            _ => {}
        }
    }

    /// A restore chunk from the master (binary frame).
    pub fn restore_chunk(&self, rid: u64, offset: u64, payload: &[u8]) {
        let tx = self
            .restore_rx
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(r, _)| *r == rid)
            .map(|(_, tx)| tx.clone());
        if let Some(tx) = tx {
            let _ = tx.try_send((offset, payload.to_vec()));
        }
    }

    // -- taking backups ------------------------------------------------------

    pub(super) async fn take_backup(&self, project: &str, service: &str, trigger: &str) -> anyhow::Result<()> {
        let mut meta = bare_meta(project, service, "logical", trigger);
        if !self.allowed() {
            let error = "backups are not allowed on this node (SS_AGENT_ALLOW)".to_string();
            self.send(&AgentMsg::BackupFailed { meta, error: error.clone() }).await;
            anyhow::bail!(error);
        }
        let _slot = self.dump_lock.acquire().await?;
        let max_rate = self
            .policies
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.project == project && p.service == service)
            .map(|p| p.max_rate_kbps)
            .unwrap_or(0);
        let id = random_id();
        let path = self.spool.join(format!("{id:016x}.dump"));
        let dumped = async {
            let target = find_target(&self.state.docker, project, service).await?;
            meta = target.meta("logical", trigger, String::new());
            dump_to_file(&self.state.docker, &target, &path, max_rate).await
        }
        .await;
        let (size, sha) = match dumped {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("backup {project}/{service} failed: {e:#}");
                self.send(&AgentMsg::BackupFailed { meta, error: format!("{e:#}") }).await;
                return Err(e);
            }
        };
        meta.finished_at = now_secs();
        self.spool_file(id, &meta, &path, size, sha).await?;
        tracing::info!(project, service, size, trigger, "backup spooled");
        Ok(())
    }

    /// Queue a finished file for shipping to the master.
    pub(super) async fn spool_file(
        &self,
        id: u64,
        meta: &BackupMeta,
        path: &std::path::Path,
        size: u64,
        sha: String,
    ) -> anyhow::Result<()> {
        let (meta_json, path_s) = (serde_json::to_string(meta)?, path.to_string_lossy().to_string());
        let now = now_secs();
        self.state
            .db
            .call(move |c| {
                c.execute(
                    "INSERT INTO agent_spool (id, meta, path, size, sha256, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![id as i64, meta_json, path_s, size as i64, sha, now],
                )
            })
            .await?;
        self.spool_changed.notify_one();
        Ok(())
    }

    // -- shipping ------------------------------------------------------------

    async fn oldest(&self) -> Option<SpoolEntry> {
        self.state
            .db
            .call(|c| {
                c.query_row(
                    "SELECT id, meta, path, size, sha256, attempts FROM agent_spool ORDER BY created_at LIMIT 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    },
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
            .and_then(|(id, meta, path, size, sha256, attempts)| {
                Some(SpoolEntry {
                    id: id as u64,
                    meta: serde_json::from_str(&meta).ok()?,
                    path: PathBuf::from(path),
                    size: size as u64,
                    sha256,
                    attempts,
                })
            })
    }

    async fn forget(&self, e: &SpoolEntry) {
        let _ = tokio::fs::remove_file(&e.path).await;
        let id = e.id as i64;
        let _ = self.state.db.call(move |c| c.execute("DELETE FROM agent_spool WHERE id = ?1", [id])).await;
    }

    async fn uploader(self: Arc<Self>, mut events: mpsc::Receiver<UploadEvent>) {
        loop {
            let Some(entry) = self.oldest().await else {
                tokio::select! {
                    _ = self.spool_changed.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(300)) => {}
                }
                continue;
            };
            let link = self.link.lock().unwrap().clone();
            let Some(link) = link else {
                tokio::select! {
                    _ = self.link_up.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {}
                }
                continue;
            };
            match self.upload_one(&entry, &link, &mut events).await {
                Ok(true) => {
                    tracing::info!(project = entry.meta.project, service = entry.meta.service, "backup shipped to master");
                    self.forget(&entry).await;
                }
                Ok(false) => {
                    let (id, attempts) = (entry.id as i64, entry.attempts + 1);
                    let _ = self
                        .state
                        .db
                        .call(move |c| c.execute("UPDATE agent_spool SET attempts = ?1 WHERE id = ?2", [attempts, id]))
                        .await;
                    if attempts >= MAX_ATTEMPTS {
                        let error = format!("master rejected the upload {attempts} times; giving up");
                        self.send(&AgentMsg::BackupFailed { meta: entry.meta.clone(), error }).await;
                        self.forget(&entry).await;
                    }
                }
                Err(e) => {
                    tracing::info!("backup upload interrupted ({e:#}); will resume");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    /// Ship one spooled file. Ok(true) = stored by the master, Ok(false) =
    /// rejected (start over), Err = link trouble (resume later).
    async fn upload_one(
        &self,
        e: &SpoolEntry,
        link: &LinkTx,
        events: &mut mpsc::Receiver<UploadEvent>,
    ) -> anyhow::Result<bool> {
        while events.try_recv().is_ok() {} // stale acks from an older attempt
        let begin = AgentMsg::UploadBegin {
            id: e.id,
            meta: e.meta.clone(),
            size: e.size,
            sha256: e.sha256.clone(),
        };
        link.ctrl
            .send(Message::Text(serde_json::to_string(&begin)?.into()))
            .await
            .map_err(|_| anyhow::anyhow!("link down"))?;

        let mut acked = match next_event(events, e.id).await? {
            UploadEvent::Done { ok, error, .. } => {
                if !ok {
                    tracing::warn!("master rejected upload: {}", error.unwrap_or_default());
                }
                return Ok(ok);
            }
            UploadEvent::Ack { offset, .. } => offset,
        };
        if acked > 0 {
            tracing::info!(offset = acked, size = e.size, "resuming backup upload");
        }
        let rate = self
            .policies
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.project == e.meta.project && p.service == e.meta.service)
            .map(|p| p.max_rate_kbps)
            .unwrap_or(0);
        let mut file = tokio::fs::File::open(&e.path).await?;
        file.seek(std::io::SeekFrom::Start(acked)).await?;
        let mut sent = acked;
        let mut buf = vec![0u8; CHUNK];
        let started = Instant::now();
        let base = sent;
        while sent < e.size {
            while sent - acked >= WINDOW * CHUNK as u64 {
                match next_event(events, e.id).await? {
                    UploadEvent::Ack { offset, .. } => acked = acked.max(offset),
                    UploadEvent::Done { ok, .. } => return Ok(ok),
                }
            }
            let n = file.read(&mut buf).await?;
            anyhow::ensure!(n > 0, "spool file shorter than recorded");
            link.bulk
                .send(frame(FRAME_UPLOAD, e.id, sent, &buf[..n]))
                .await
                .map_err(|_| anyhow::anyhow!("link down"))?;
            sent += n as u64;
            if rate > 0 {
                let due = Duration::from_secs_f64((sent - base) as f64 / (rate as f64 * 1024.0));
                if due > started.elapsed() {
                    tokio::time::sleep(due - started.elapsed()).await;
                }
            }
        }
        loop {
            match next_event(events, e.id).await? {
                UploadEvent::Done { ok, error, .. } => {
                    if !ok {
                        tracing::warn!("master rejected upload: {}", error.unwrap_or_default());
                    }
                    return Ok(ok);
                }
                UploadEvent::Ack { .. } => {}
            }
        }
    }

    // -- restores ------------------------------------------------------------

    pub(super) async fn status(&self, rid: u64, stage: &str, ok: Option<bool>, message: &str) {
        self.send(&AgentMsg::RestoreStatus { rid, stage: stage.into(), ok, message: message.into() })
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn restore(
        self: Arc<Self>,
        rid: u64,
        project: &str,
        service: &str,
        size: u64,
        sha256: &str,
        kind: &str,
        target_time: Option<i64>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.allowed(), "backups are not allowed on this node (SS_AGENT_ALLOW)");
        self.status(rid, "downloading", None, "").await;
        let path = self.spool.join(format!("restore-{rid}.dump"));
        let result = async {
            let mut file = tokio::fs::File::create(&path).await?;
            let (tx, mut rx) = mpsc::channel::<(u64, Vec<u8>)>((WINDOW * 2) as usize);
            *self.restore_rx.lock().unwrap() = Some((rid, tx));
            self.send(&AgentMsg::RestoreAck { rid, offset: 0 }).await;
            let mut len = 0u64;
            while len < size {
                let (offset, data) = tokio::time::timeout(ACK_TIMEOUT, rx.recv())
                    .await
                    .map_err(|_| anyhow::anyhow!("restore download stalled"))?
                    .ok_or_else(|| anyhow::anyhow!("restore download aborted"))?;
                if offset != len {
                    continue;
                }
                file.write_all(&data).await?;
                len += data.len() as u64;
                self.send(&AgentMsg::RestoreAck { rid, offset: len }).await;
            }
            *self.restore_rx.lock().unwrap() = None;
            file.sync_all().await?;
            drop(file);
            self.status(rid, "verifying", None, "").await;
            anyhow::ensure!(
                sha256_file(&path).await? == sha256,
                "checksum mismatch after download — nothing was changed"
            );
            if kind == "pitr" {
                return self.pitr_restore(rid, project, service, &path, target_time).await;
            }
            self.status(rid, "safety-backup", None, "").await;
            // Never overwrite a database without a fresh copy of what's there now.
            self.take_backup(project, service, "pre-restore")
                .await
                .map_err(|e| anyhow::anyhow!("safety backup failed, nothing was changed: {e:#}"))?;
            self.status(rid, "restoring", None, "").await;
            let target = find_target(&self.state.docker, project, service).await?;
            let _slot = self.dump_lock.acquire().await?;
            restore_from_file(&self.state.docker, &target, &path).await
        }
        .await;
        *self.restore_rx.lock().unwrap() = None;
        let _ = tokio::fs::remove_file(&path).await;
        let log = result?;
        self.status(rid, "done", Some(true), &log).await;
        Ok(())
    }
}
