//! Master side of backups: the central store and catalog, upload receiving
//! (resumable, verified), retention, restores, the home server's own
//! backups, and the user-facing API.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, Notify, Semaphore};

use super::{find_target, restore_from_file, retention_keep, BackupMeta, Policy};
use crate::audit::audit;
use crate::auth::client_ip;
use crate::auth::perms::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::fleet::{frame, now_secs, MasterMsg, CHUNK, FRAME_RESTORE, WINDOW};
use crate::state::AppState;

pub const HOME: &str = "home";

pub struct Backups {
    pub root: PathBuf,
    restores: Mutex<HashMap<u64, RestoreState>>,
    restore_acks: Mutex<HashMap<u64, mpsc::Sender<u64>>>,
    next_rid: AtomicU64,
    /// The home server's own policies, for its local scheduler.
    local_policies: Arc<Mutex<Vec<Policy>>>,
    local_changed: Arc<Notify>,
    /// One dump at a time on this machine.
    dump_lock: Arc<Semaphore>,
    /// One verification at a time (each starts a throwaway Postgres).
    verify_lock: Semaphore,
}

#[derive(Clone, Serialize)]
pub struct RestoreState {
    rid: u64,
    node: String,
    backup_id: i64,
    project: String,
    service: String,
    stage: String,
    ok: Option<bool>,
    message: String,
    started_at: i64,
}

impl Backups {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            restores: Mutex::new(HashMap::new()),
            restore_acks: Mutex::new(HashMap::new()),
            next_rid: AtomicU64::new(now_secs() as u64 * 1000),
            local_policies: Arc::new(Mutex::new(Vec::new())),
            local_changed: Arc::new(Notify::new()),
            dump_lock: Arc::new(Semaphore::new(1)),
            verify_lock: Semaphore::new(1),
        }
    }

    fn set_restore(&self, rid: u64, f: impl FnOnce(&mut RestoreState)) {
        if let Some(r) = self.restores.lock().unwrap().get_mut(&rid) {
            f(r);
        }
    }

    /// Agent → master restore progress.
    pub fn restore_status(&self, rid: u64, stage: String, ok: Option<bool>, message: String) {
        self.set_restore(rid, |r| {
            r.stage = stage;
            r.ok = ok;
            r.message = message;
        });
        if ok.is_some() {
            self.restore_acks.lock().unwrap().remove(&rid);
        }
    }

    pub fn restore_ack(&self, rid: u64, offset: u64) {
        let tx = self.restore_acks.lock().unwrap().get(&rid).cloned();
        if let Some(tx) = tx {
            let _ = tx.try_send(offset);
        }
    }
}

/// Keep path components boring: agents supply project/service names.
fn safe(component: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !component.is_empty()
            && component.len() <= 128
            && component != "."
            && component != ".."
            && component.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
        "unsafe name {component:?}"
    );
    Ok(component)
}

fn final_rel_path(node: &str, meta: &BackupMeta) -> anyhow::Result<PathBuf> {
    let stamp = time::OffsetDateTime::from_unix_timestamp(meta.started_at)?
        .format(&time::macros::format_description!("[year][month][day]T[hour][minute][second]Z"))?;
    let dir = PathBuf::from(safe(node)?).join(safe(&meta.project)?).join(safe(&meta.service)?);
    Ok(match meta.kind.as_str() {
        "base" => dir.join("base").join(format!("{stamp}-{}.tar.gz", safe(&meta.trigger)?)),
        "wal" => {
            anyhow::ensure!(
                meta.label.len() == 24 && meta.label.chars().all(|c| c.is_ascii_hexdigit()),
                "bad WAL segment name {:?}",
                meta.label
            );
            dir.join("wal").join(format!("{}.gz", meta.label))
        }
        _ => dir.join(format!("{stamp}-{}.dump", safe(&meta.trigger)?)),
    })
}

async fn insert_backup(
    state: &AppState,
    node: &str,
    meta: &BackupMeta,
    size: u64,
    sha: &str,
    status: &str,
    error: &str,
    path: &str,
    upload_id: Option<u64>,
) -> anyhow::Result<i64> {
    let (node, meta, sha, status, error, path) = (
        node.to_string(),
        meta.clone(),
        sha.to_string(),
        status.to_string(),
        error.to_string(),
        path.to_string(),
    );
    state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO backups (node, project, service, engine, kind, trigger, started_at, finished_at,
                                      size, sha256, status, error, path, upload_id, label, image, db_user, database)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                rusqlite::params![
                    node, meta.project, meta.service, meta.engine, meta.kind, meta.trigger,
                    meta.started_at, meta.finished_at, size as i64, sha, status, error, path,
                    upload_id.map(|u| u as i64), meta.label, meta.image, meta.db_user, meta.database
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await
}

pub async fn load_policy(state: &AppState, node: &str, project: &str, service: &str) -> anyhow::Result<Option<Policy>> {
    let (n, p, s) = (node.to_string(), project.to_string(), service.to_string());
    let raw: Option<String> = state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT policy FROM backup_policies WHERE node = ?1 AND project = ?2 AND service = ?3",
                [&n, &p, &s],
                |r| r.get(0),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await?;
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

pub async fn node_policies(state: &AppState, node: &str) -> anyhow::Result<Vec<Policy>> {
    let n = node.to_string();
    let rows: Vec<String> = state
        .db
        .call(move |c| {
            c.prepare("SELECT policy FROM backup_policies WHERE node = ?1")?
                .query_map([&n], |r| r.get(0))?
                .collect()
        })
        .await?;
    Ok(rows.iter().filter_map(|r| serde_json::from_str(r).ok()).collect())
}

/// Prune stored backups of one database to what its policy keeps.
pub async fn apply_retention(state: &AppState, node: &str, project: &str, service: &str) -> anyhow::Result<usize> {
    let policy = load_policy(state, node, project, service)
        .await?
        .unwrap_or_else(|| Policy::default_for(project, service));
    let (n, p, s) = (node.to_string(), project.to_string(), service.to_string());
    // (id, started_at, path, status, kind, label)
    let rows: Vec<(i64, i64, String, String, String, String)> = state
        .db
        .call(move |c| {
            c.prepare(
                "SELECT id, started_at, path, status, kind, label FROM backups
                 WHERE node = ?1 AND project = ?2 AND service = ?3
                 ORDER BY started_at DESC",
            )?
            .query_map([&n, &p, &s], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect()
        })
        .await?;
    let ok_of = |kind: &str| -> Vec<(i64, i64)> {
        rows.iter().filter(|r| r.3 == "ok" && r.4 == kind).map(|r| (r.0, r.1)).collect()
    };
    // Logical dumps and base backups each keep what the policy says.
    let mut keep = retention_keep(&policy, &ok_of("logical"));
    let kept_bases = retention_keep(&policy, &ok_of("base"));
    keep.extend(kept_bases.iter().copied());
    // WAL is needed from the oldest kept base onwards; with no base at all
    // there is nothing to restore it onto yet, so it all stays.
    let oldest_start: Option<String> = rows
        .iter()
        .filter(|r| kept_bases.contains(&r.0))
        .filter_map(|r| base_range(&r.5).map(|(s, _)| s))
        .min();
    // Failed attempts are only history: keep the latest 20.
    let failed_keep: Vec<i64> = rows.iter().filter(|r| r.3 == "failed").take(20).map(|r| r.0).collect();
    let doomed: Vec<(i64, String)> = rows
        .iter()
        .filter(|r| match (r.3.as_str(), r.4.as_str()) {
            ("failed", _) => !failed_keep.contains(&r.0),
            ("ok", "wal") => oldest_start.as_ref().is_some_and(|start| &r.5 < start),
            ("ok", _) => !keep.contains(&r.0),
            _ => false,
        })
        .map(|r| (r.0, r.2.clone()))
        .collect();
    let root = state.backups.as_ref().map(|b| b.root.clone()).unwrap_or_default();
    for (_, path) in &doomed {
        if !path.is_empty() {
            let _ = tokio::fs::remove_file(root.join(path)).await;
        }
    }
    let ids: Vec<i64> = doomed.iter().map(|d| d.0).collect();
    let count = ids.len();
    state
        .db
        .call(move |c| {
            for id in ids {
                c.execute("DELETE FROM backups WHERE id = ?1", [id])?;
            }
            Ok(())
        })
        .await?;
    Ok(count)
}

pub async fn record_failure(state: &AppState, node: &str, meta: &BackupMeta, error: &str) {
    tracing::warn!(node, project = meta.project, service = meta.service, "backup failed: {error}");
    let _ = insert_backup(state, node, meta, 0, "", "failed", error, "", None).await;
    if let Some(fleet) = &state.fleet {
        fleet.backup_event(node, &meta.project, &meta.service, false, error);
    }
}

// ---------------------------------------------------------------------------
// Upload receiving (from agents)
// ---------------------------------------------------------------------------

pub struct Incoming {
    file: tokio::fs::File,
    part: PathBuf,
    len: u64,
    size: u64,
    sha: String,
    hasher: Sha256,
    meta: BackupMeta,
}

/// Per-connection upload state, keyed by the agent's upload id.
pub type Uploads = HashMap<u64, Incoming>;

pub async fn upload_begin(
    state: &AppState,
    node: &str,
    uploads: &mut Uploads,
    id: u64,
    meta: BackupMeta,
    size: u64,
    sha: String,
) -> MasterMsg {
    match try_upload_begin(state, node, uploads, id, meta, size, sha).await {
        Ok(msg) => msg,
        Err(e) => MasterMsg::UploadDone { id, ok: false, error: Some(e.to_string()) },
    }
}

async fn try_upload_begin(
    state: &AppState,
    node: &str,
    uploads: &mut Uploads,
    id: u64,
    meta: BackupMeta,
    size: u64,
    sha: String,
) -> anyhow::Result<MasterMsg> {
    let backups = state.backups.as_ref().ok_or_else(|| anyhow::anyhow!("no backup store"))?;
    final_rel_path(node, &meta)?; // validate names before touching the disk
    let n = node.to_string();
    let done: bool = state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT COUNT(*) FROM backups WHERE node = ?1 AND upload_id = ?2 AND status = 'ok'",
                rusqlite::params![n, id as i64],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
        })
        .await?;
    if done {
        // Already stored (the agent missed our last answer): idempotent.
        return Ok(MasterMsg::UploadDone { id, ok: true, error: None });
    }
    let incoming = backups.root.join(".incoming");
    tokio::fs::create_dir_all(&incoming).await?;
    let part = incoming.join(format!("{node}-{id:016x}.part"));
    let mut file = tokio::fs::OpenOptions::new().create(true).read(true).append(true).open(&part).await?;
    let mut len = file.metadata().await?.len();
    let mut hasher = Sha256::new();
    if len > size {
        file.set_len(0).await?;
        len = 0;
    } else if len > 0 {
        // Resuming: re-hash what we already have.
        file.seek(std::io::SeekFrom::Start(0)).await?;
        let mut buf = vec![0u8; 256 * 1024];
        let cap = buf.len() as u64;
        let mut left = len;
        while left > 0 {
            let n = file.read(&mut buf[..left.min(cap) as usize]).await?;
            anyhow::ensure!(n > 0, "short read while resuming");
            hasher.update(&buf[..n]);
            left -= n as u64;
        }
        tracing::info!(node, id, offset = len, size, "backup upload resuming");
    }
    uploads.insert(id, Incoming { file, part, len, size, sha, hasher, meta });
    if len == size {
        return Ok(finish_upload(state, node, uploads, id).await);
    }
    Ok(MasterMsg::UploadAck { id, offset: len })
}

pub async fn upload_chunk(
    state: &AppState,
    node: &str,
    uploads: &mut Uploads,
    id: u64,
    offset: u64,
    payload: &[u8],
) -> Option<MasterMsg> {
    let inc = uploads.get_mut(&id)?;
    if offset != inc.len {
        // Out of order (a retransmit after a hiccup): tell it where we are.
        return Some(MasterMsg::UploadAck { id, offset: inc.len });
    }
    if inc.len + payload.len() as u64 > inc.size {
        let part = inc.part.clone();
        uploads.remove(&id);
        let _ = tokio::fs::remove_file(part).await;
        return Some(MasterMsg::UploadDone { id, ok: false, error: Some("more data than announced".into()) });
    }
    if let Err(e) = inc.file.write_all(payload).await {
        uploads.remove(&id);
        return Some(MasterMsg::UploadDone { id, ok: false, error: Some(format!("write: {e}")) });
    }
    inc.hasher.update(payload);
    inc.len += payload.len() as u64;
    if inc.len == inc.size {
        return Some(finish_upload(state, node, uploads, id).await);
    }
    Some(MasterMsg::UploadAck { id, offset: inc.len })
}

async fn finish_upload(state: &AppState, node: &str, uploads: &mut Uploads, id: u64) -> MasterMsg {
    let Some(mut inc) = uploads.remove(&id) else {
        return MasterMsg::UploadDone { id, ok: false, error: Some("unknown upload".into()) };
    };
    let result = async {
        inc.file.flush().await?;
        inc.file.sync_all().await?;
        let digest: String = inc.hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if digest != inc.sha {
            let _ = tokio::fs::remove_file(&inc.part).await;
            anyhow::bail!("checksum mismatch — upload discarded, the agent will resend");
        }
        let rel = final_rel_path(node, &inc.meta)?;
        let root = &state.backups.as_ref().expect("store").root;
        let dest = root.join(&rel);
        tokio::fs::create_dir_all(dest.parent().expect("parent")).await?;
        tokio::fs::rename(&inc.part, &dest).await?;
        if inc.meta.kind == "wal" {
            // A segment re-sent after a crash replaces its file; one row per segment.
            let (n, m, rel_s) = (node.to_string(), inc.meta.clone(), rel.to_string_lossy().to_string());
            let exists: bool = state
                .db
                .call(move |c| {
                    c.query_row(
                        "SELECT COUNT(*) FROM backups WHERE node = ?1 AND project = ?2 AND service = ?3
                           AND kind = 'wal' AND label = ?4 AND path = ?5",
                        rusqlite::params![n, m.project, m.service, m.label, rel_s],
                        |r| r.get::<_, i64>(0),
                    )
                    .map(|c| c > 0)
                })
                .await?;
            if !exists {
                insert_backup(state, node, &inc.meta, inc.size, &inc.sha, "ok", "", &rel.to_string_lossy(), Some(id))
                    .await?;
            }
            return anyhow::Ok(());
        }
        let bid = insert_backup(
            state, node, &inc.meta, inc.size, &inc.sha, "ok", "", &rel.to_string_lossy(), Some(id),
        )
        .await?;
        tracing::info!(node, project = inc.meta.project, service = inc.meta.service, kind = inc.meta.kind, size = inc.size, "backup stored");
        if let Some(fleet) = &state.fleet {
            fleet.backup_event(node, &inc.meta.project, &inc.meta.service, true, &format!("{} {} bytes", inc.meta.kind, inc.size));
        }
        apply_retention(state, node, &inc.meta.project, &inc.meta.service).await?;
        maybe_verify(state, node, &inc.meta, bid).await;
        anyhow::Ok(())
    }
    .await;
    match result {
        Ok(_) => MasterMsg::UploadDone { id, ok: true, error: None },
        Err(e) => MasterMsg::UploadDone { id, ok: false, error: Some(e.to_string()) },
    }
}

// ---------------------------------------------------------------------------
// Home server: local backups and scheduler
// ---------------------------------------------------------------------------

/// Dump one of the home server's own databases straight into the store.
pub async fn run_local(state: &AppState, project: &str, service: &str, trigger: &str) -> anyhow::Result<i64> {
    let backups = state.backups.clone().ok_or_else(|| anyhow::anyhow!("no backup store"))?;
    let _slot = backups.dump_lock.acquire().await?;
    let policy = load_policy(state, HOME, project, service)
        .await?
        .unwrap_or_else(|| Policy::default_for(project, service));
    let started_at = now_secs();
    let mut meta = super::agent::bare_meta(project, service, "logical", trigger);
    let incoming = backups.root.join(".incoming");
    tokio::fs::create_dir_all(&incoming).await?;
    let part = incoming.join(format!("home-{started_at}-{}.part", super::agent::random_id()));
    let dumped = async {
        let target = find_target(&state.docker, project, service).await?;
        meta = target.meta("logical", trigger, String::new());
        super::dump_to_file(&state.docker, &target, &part, policy.max_rate_kbps).await
    }
    .await;
    let (size, sha) = match dumped {
        Ok(v) => v,
        Err(e) => {
            record_failure(state, HOME, &meta, &e.to_string()).await;
            return Err(e);
        }
    };
    meta.finished_at = now_secs();
    let rel = final_rel_path(HOME, &meta)?;
    let dest = backups.root.join(&rel);
    tokio::fs::create_dir_all(dest.parent().expect("parent")).await?;
    tokio::fs::rename(&part, &dest).await?;
    let id = insert_backup(state, HOME, &meta, size, &sha, "ok", "", &rel.to_string_lossy(), None).await?;
    if let Some(fleet) = &state.fleet {
        fleet.backup_event(HOME, project, service, true, &format!("{size} bytes"));
    }
    apply_retention(state, HOME, project, service).await?;
    maybe_verify(state, HOME, &meta, id).await;
    Ok(id)
}

/// Start the home server's scheduler (master and standalone modes).
pub async fn spawn_local(state: AppState) -> anyhow::Result<()> {
    let backups = state.backups.clone().expect("store");
    tokio::fs::create_dir_all(&backups.root).await?;
    *backups.local_policies.lock().unwrap() = node_policies(&state, HOME).await?;
    let (policies, changed) = (backups.local_policies.clone(), backups.local_changed.clone());
    let st = state.clone();
    tokio::spawn(super::scheduler_loop(state.clone(), HOME.into(), policies, changed, move |p: Policy| {
        let st = st.clone();
        async move {
            if let Err(e) = run_local(&st, &p.project, &p.service, "schedule").await {
                tracing::warn!("scheduled backup {}/{}: {e}", p.project, p.service);
            }
        }
    }));
    Ok(())
}

// ---------------------------------------------------------------------------
// Restores
// ---------------------------------------------------------------------------

struct BackupRow {
    id: i64,
    node: String,
    project: String,
    service: String,
    kind: String,
    started_at: i64,
    size: u64,
    sha256: String,
    path: String,
    status: String,
    label: String,
    image: String,
    db_user: String,
    database: String,
}

const ROW_COLS: &str =
    "id, node, project, service, kind, trigger, started_at, size, sha256, path, status, label, image, db_user, database";

fn row(r: &rusqlite::Row) -> rusqlite::Result<BackupRow> {
    Ok(BackupRow {
        id: r.get(0)?,
        node: r.get(1)?,
        project: r.get(2)?,
        service: r.get(3)?,
        kind: r.get(4)?,
        started_at: r.get(6)?,
        size: r.get::<_, i64>(7)? as u64,
        sha256: r.get(8)?,
        path: r.get(9)?,
        status: r.get(10)?,
        label: r.get(11)?,
        image: r.get(12)?,
        db_user: r.get(13)?,
        database: r.get(14)?,
    })
}

async fn get_backup(state: &AppState, id: i64) -> anyhow::Result<Option<BackupRow>> {
    state
        .db
        .call(move |c| {
            c.query_row(&format!("SELECT {ROW_COLS} FROM backups WHERE id = ?1"), [id], row)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
        })
        .await
}

/// WAL segments of one database with name >= `from` (sorted).
async fn wal_from(state: &AppState, node: &str, project: &str, service: &str, from: &str) -> anyhow::Result<Vec<BackupRow>> {
    let (n, p, s, f) = (node.to_string(), project.to_string(), service.to_string(), from.to_string());
    state
        .db
        .call(move |c| {
            c.prepare(&format!(
                "SELECT {ROW_COLS} FROM backups WHERE node = ?1 AND project = ?2 AND service = ?3
                   AND kind = 'wal' AND status = 'ok' AND label >= ?4 ORDER BY label"
            ))?
            .query_map([&n, &p, &s, &f], row)?
            .collect()
        })
        .await
}

/// A base backup's WAL range from its label: `wal_start=A[;wal_end=B]`.
fn base_range(label: &str) -> Option<(String, Option<String>)> {
    let mut start = None;
    let mut end = None;
    for part in label.split(';') {
        if let Some(v) = part.strip_prefix("wal_start=") {
            start = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("wal_end=") {
            end = Some(v.to_string());
        }
    }
    start.map(|s| (s, end))
}

/// The next segment name after `seg` (16 MB segments: 256 per log file).
fn next_segment(seg: &str) -> Option<String> {
    let tli = &seg[..8];
    let log = u32::from_str_radix(&seg[8..16], 16).ok()?;
    let n = u32::from_str_radix(&seg[16..24], 16).ok()?;
    let (log, n) = if n >= 0xFF { (log + 1, 0) } else { (log, n + 1) };
    Some(format!("{tli}{log:08X}{n:08X}"))
}

/// First missing segment in the chain starting at `start`, if any.
fn chain_gap(start: &str, labels: &[String]) -> Option<String> {
    let tli = &start[..8];
    let mut expect = start.to_string();
    for l in labels.iter().filter(|l| l.starts_with(tli)) {
        if l < &expect {
            continue;
        }
        if l != &expect {
            return Some(expect);
        }
        expect = next_segment(&expect)?;
    }
    None
}

/// Broken WAL chains of continuous policies: (node, project, service, first missing segment).
pub async fn wal_gaps(state: &AppState) -> anyhow::Result<Vec<(String, String, String, String)>> {
    let policies: Vec<(String, String)> = state
        .db
        .call(|c| c.prepare("SELECT node, policy FROM backup_policies")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .await?;
    let mut out = Vec::new();
    for (node, raw) in policies {
        let Ok(p) = serde_json::from_str::<Policy>(&raw) else { continue };
        if !p.enabled || p.mode != "continuous" {
            continue;
        }
        let (n, pr, sv) = (node.clone(), p.project.clone(), p.service.clone());
        let start: Option<String> = state
            .db
            .call(move |c| {
                c.query_row(
                    "SELECT label FROM backups WHERE node = ?1 AND project = ?2 AND service = ?3
                       AND kind = 'base' AND status = 'ok' ORDER BY started_at DESC LIMIT 1",
                    [&n, &pr, &sv],
                    |r| r.get::<_, String>(0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
            })
            .await?;
        let Some(start) = start.and_then(|l| base_range(&l).map(|(s, _)| s)) else { continue };
        let labels: Vec<String> = wal_from(state, &node, &p.project, &p.service, &start)
            .await?
            .into_iter()
            .map(|w| w.label)
            .collect();
        // Only a hole *before* the newest segment counts; the tail is simply not shipped yet.
        if let Some(missing) = chain_gap(&start, &labels) {
            if labels.last().is_some_and(|last| last > &missing) {
                out.push((node, p.project, p.service, missing));
            }
        }
    }
    Ok(out)
}

/// Base + WAL bundle for a point-in-time restore or a verification.
/// Returns (bundle path, size, sha256, the base used).
async fn build_pitr_bundle(
    state: &AppState,
    backups: &Backups,
    node: &str,
    project: &str,
    service: &str,
    base_id: Option<i64>,
    target_time: Option<i64>,
) -> anyhow::Result<(PathBuf, u64, String, BackupRow)> {
    let (n, p, s) = (node.to_string(), project.to_string(), service.to_string());
    let bases: Vec<BackupRow> = state
        .db
        .call(move |c| {
            c.prepare(&format!(
                "SELECT {ROW_COLS} FROM backups WHERE node = ?1 AND project = ?2 AND service = ?3
                   AND kind = 'base' AND status = 'ok' ORDER BY started_at DESC"
            ))?
            .query_map([&n, &p, &s], row)?
            .collect()
        })
        .await?;
    let base = match (base_id, target_time) {
        (Some(id), _) => bases.into_iter().find(|b| b.id == id),
        (None, Some(t)) => bases.into_iter().find(|b| b.started_at <= t),
        (None, None) => bases.into_iter().next(),
    }
    .ok_or_else(|| anyhow::anyhow!("no base backup old enough for that point in time"))?;
    let (start, end) = base_range(&base.label).ok_or_else(|| anyhow::anyhow!("base backup has no WAL start recorded"))?;
    let wal = wal_from(state, node, project, service, &start).await?;
    let labels: Vec<String> = wal.iter().map(|w| w.label.clone()).collect();
    // Refuse up front — before anything is stopped — unless every segment
    // from the backup's start through its end has arrived: without them the
    // restored server cannot even reach a consistent state.
    if let Some(end) = &end {
        let needed: Vec<String> = labels.iter().filter(|l| *l <= end).cloned().collect();
        let complete = needed.last() == Some(end) && chain_gap(&start, &needed).is_none();
        anyhow::ensure!(
            complete,
            "the WAL that completes base backup #{} (segments {start}..{end}) has not all reached the master yet — try again in a few minutes",
            base.id
        );
    }
    if let Some(missing) = chain_gap(&start, &labels) {
        // Recovery can still stop before the gap; only refuse if the target is past it.
        let reaches = wal.iter().filter(|w| w.label < missing).map(|w| w.started_at).max().unwrap_or(0);
        anyhow::ensure!(
            target_time.is_some_and(|t| t <= reaches),
            "WAL segment {missing} is missing: this base can only be restored up to {}",
            reaches
        );
    }
    let bundles = backups.root.join(".bundles");
    tokio::fs::create_dir_all(&bundles).await?;
    let dest = bundles.join(format!("{node}-{}-{}.tar", base.id, super::agent::random_id()));
    let mut files = vec![("base.tar.gz".to_string(), backups.root.join(&base.path))];
    for w in &wal {
        files.push((format!("wal/{}.gz", w.label), backups.root.join(&w.path)));
    }
    let (size, sha) = super::pitr::write_bundle(&dest, &files).await?;
    Ok((dest, size, sha, base))
}

struct RestoreJob {
    node: String,
    project: String,
    service: String,
    backup_id: i64,
    file: PathBuf,
    size: u64,
    sha256: String,
    kind: String,
    target_time: Option<i64>,
    /// Delete `file` when done (a generated bundle).
    temporary: bool,
}

fn start_restore(state: &AppState, job: RestoreJob) -> anyhow::Result<u64> {
    let backups = state.backups.clone().ok_or_else(|| anyhow::anyhow!("no backup store"))?;
    let rid = backups.next_rid.fetch_add(1, Ordering::Relaxed);
    backups.restores.lock().unwrap().insert(
        rid,
        RestoreState {
            rid,
            node: job.node.clone(),
            backup_id: job.backup_id,
            project: job.project.clone(),
            service: job.service.clone(),
            stage: "starting".into(),
            ok: None,
            message: String::new(),
            started_at: now_secs(),
        },
    );
    let state = state.clone();
    tokio::spawn(async move {
        let result = if job.node == HOME {
            restore_local(&state, &backups, rid, &job).await
        } else {
            restore_remote(&state, &backups, rid, &job).await
        };
        if let Err(e) = result {
            backups.restore_status(rid, "failed".into(), Some(false), e.to_string());
        }
        if job.temporary {
            let _ = tokio::fs::remove_file(&job.file).await;
        }
    });
    Ok(rid)
}

async fn restore_local(state: &AppState, backups: &Backups, rid: u64, job: &RestoreJob) -> anyhow::Result<()> {
    anyhow::ensure!(job.kind == "logical", "the home server only keeps logical backups");
    backups.set_restore(rid, |r| r.stage = "safety-backup".into());
    // Never overwrite a database without a fresh copy of what's there now.
    run_local(state, &job.project, &job.service, "pre-restore")
        .await
        .map_err(|e| anyhow::anyhow!("safety backup failed, nothing was changed: {e}"))?;
    backups.set_restore(rid, |r| r.stage = "restoring".into());
    let target = find_target(&state.docker, &job.project, &job.service).await?;
    let _slot = backups.dump_lock.acquire().await?;
    let log = restore_from_file(&state.docker, &target, &job.file).await?;
    backups.restore_status(rid, "done".into(), Some(true), log);
    Ok(())
}

async fn restore_remote(state: &AppState, backups: &Backups, rid: u64, job: &RestoreJob) -> anyhow::Result<()> {
    let fleet = state.fleet.clone().ok_or_else(|| anyhow::anyhow!("not a master"))?;
    let (ack_tx, mut acks) = mpsc::channel::<u64>(64);
    backups.restore_acks.lock().unwrap().insert(rid, ack_tx);
    backups.set_restore(rid, |r| r.stage = "sending".into());
    fleet
        .send(
            &job.node,
            &MasterMsg::RestoreStart {
                rid,
                project: job.project.clone(),
                service: job.service.clone(),
                size: job.size,
                sha256: job.sha256.clone(),
                kind: job.kind.clone(),
                target_time: job.target_time,
            },
        )
        .await
        .map_err(|_| anyhow::anyhow!("{} is offline", job.node))?;
    let mut file = tokio::fs::File::open(&job.file).await?;
    let mut acked = match tokio::time::timeout(Duration::from_secs(60), acks.recv()).await {
        Ok(Some(o)) => o,
        _ => anyhow::bail!("node did not accept the restore"),
    };
    let mut sent = acked;
    file.seek(std::io::SeekFrom::Start(sent)).await?;
    let mut buf = vec![0u8; CHUNK];
    while sent < job.size {
        while sent - acked >= WINDOW * CHUNK as u64 {
            match tokio::time::timeout(Duration::from_secs(60), acks.recv()).await {
                Ok(Some(o)) => acked = acked.max(o),
                _ => anyhow::bail!("node stopped acknowledging the restore upload"),
            }
        }
        let n = file.read(&mut buf).await?;
        anyhow::ensure!(n > 0, "backup file shorter than catalogued");
        fleet
            .send_bulk(&job.node, frame(FRAME_RESTORE, rid, sent, &buf[..n]))
            .await
            .map_err(|_| anyhow::anyhow!("{} went offline during the restore", job.node))?;
        sent += n as u64;
    }
    // Wait for the agent's final ack so a temporary bundle isn't deleted
    // while the tail is still in flight.
    while acked < job.size {
        match tokio::time::timeout(Duration::from_secs(60), acks.recv()).await {
            Ok(Some(o)) => acked = acked.max(o),
            _ => break,
        }
    }
    // The agent now verifies and restores; its RestoreStatus messages finish the state.
    backups.set_restore(rid, |r| {
        if r.ok.is_none() && r.stage == "sending" {
            r.stage = "verifying".into();
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Verification: restore into a throwaway Postgres on this machine
// ---------------------------------------------------------------------------

/// Verify scheduled backups automatically when the policy asks for it.
async fn maybe_verify(state: &AppState, node: &str, meta: &BackupMeta, id: i64) {
    if meta.kind == "wal" || meta.trigger == "pre-restore" || meta.trigger == "manual" {
        return;
    }
    let verify = load_policy(state, node, &meta.project, &meta.service)
        .await
        .ok()
        .flatten()
        .is_none_or(|p| p.verify);
    if !verify {
        return;
    }
    let state = state.clone();
    // A base backup needs the WAL up to its end, which ships a few minutes later.
    let delay = if meta.kind == "base" { 180 } else { 0 };
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(delay)).await;
        let _ = verify_backup(&state, id).await;
    });
}

async fn pull_if_missing(docker: &bollard::Docker, image: &str) -> anyhow::Result<()> {
    if docker.inspect_image(image).await.is_ok() {
        return Ok(());
    }
    let opts = bollard::query_parameters::CreateImageOptionsBuilder::default().from_image(image).build();
    let mut pull = docker.create_image(Some(opts), None, None);
    while let Some(step) = futures_util::StreamExt::next(&mut pull).await {
        step?;
    }
    Ok(())
}

/// Restore backup `id` into a disposable Postgres (same image, no network)
/// and check it: logical → pg_restore + table count; base → WAL replay to
/// the end + pg_amcheck when available. Records the outcome on the row.
pub async fn verify_backup(state: &AppState, id: i64) -> anyhow::Result<String> {
    let backups = state.backups.clone().ok_or_else(|| anyhow::anyhow!("no backup store"))?;
    let _one = backups.verify_lock.acquire().await?;
    let row = get_backup(state, id).await?.ok_or_else(|| anyhow::anyhow!("no such backup"))?;
    let result = verify_row(state, &backups, &row).await;
    let (verdict, ok) = match &result {
        Ok(msg) => (format!("ok: {msg}"), true),
        Err(e) => (format!("failed: {e:#}"), false),
    };
    let (v, now) = (verdict.clone(), now_secs());
    let _ = state
        .db
        .call(move |c| {
            c.execute("UPDATE backups SET verified = ?1, verified_at = ?2 WHERE id = ?3", rusqlite::params![v, now, id])
        })
        .await;
    if let Some(fleet) = &state.fleet {
        fleet.backup_event(
            &row.node,
            &row.project,
            &row.service,
            ok,
            &format!("verify {} #{id}: {verdict}", row.kind),
        );
    }
    tracing::info!(node = row.node, project = row.project, service = row.service, id, "verification {verdict}");
    result
}

async fn verify_row(state: &AppState, backups: &Backups, row: &BackupRow) -> anyhow::Result<String> {
    anyhow::ensure!(row.status == "ok", "only successful backups can be verified");
    anyhow::ensure!(!row.image.is_empty(), "backup predates image tracking; take a new one");
    let docker = &state.docker;
    pull_if_missing(docker, &row.image).await?;
    let user = if row.db_user.is_empty() { "postgres".to_string() } else { row.db_user.clone() };
    let db = if row.database.is_empty() { user.clone() } else { row.database.clone() };
    let tag = super::agent::random_id();
    let pgroot = "/var/lib/postgresql/serious-verify";
    let volume = format!("serious-verify-{tag:x}");
    let mut env = vec![
        format!("POSTGRES_USER={user}"),
        format!("POSTGRES_DB={db}"),
        "POSTGRES_PASSWORD=verify".to_string(),
        format!("PGDATA={pgroot}/data"),
    ];
    let mut bundle = None;
    if row.kind == "base" {
        docker
            .create_volume(bollard::models::VolumeCreateRequest { name: Some(volume.clone()), ..Default::default() })
            .await?;
        let (path, _, _, _) =
            build_pitr_bundle(state, backups, &row.node, &row.project, &row.service, Some(row.id), None).await?;
        bundle = Some(path.clone());
        let layout = super::pitr::Layout {
            pgdata: format!("{pgroot}/data"),
            source: volume.clone(),
            bind: false,
            rel: "data".into(),
        };
        super::pitr::materialize(docker, &row.image, &layout, &path, None).await?;
        env.retain(|e| !e.starts_with("POSTGRES_PASSWORD"));
        env.push("POSTGRES_PASSWORD=unused".into());
    }
    let body = bollard::models::ContainerCreateBody {
        image: Some(row.image.clone()),
        env: Some(env),
        labels: Some([("serious.helper".to_string(), "true".to_string())].into()),
        host_config: Some(bollard::models::HostConfig {
            network_mode: Some("none".into()),
            mounts: (row.kind == "base").then(|| {
                vec![bollard::models::Mount {
                    target: Some(pgroot.into()),
                    source: Some(volume.clone()),
                    typ: Some(bollard::models::MountType::VOLUME),
                    ..Default::default()
                }]
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let name = format!("serious-verify-{tag:x}");
    let opts = bollard::query_parameters::CreateContainerOptionsBuilder::default().name(&name).build();
    let created = docker.create_container(Some(opts), body).await?;
    let cid = created.id.clone();
    let checked = async {
        docker.start_container(&cid, None::<bollard::query_parameters::StartContainerOptions>).await?;
        if row.kind == "base" {
            super::pitr::wait_promoted(docker, &cid, Duration::from_secs(1800)).await?;
        } else {
            // Fresh cluster: wait until it accepts connections, then load the dump.
            let end = std::time::Instant::now() + Duration::from_secs(120);
            loop {
                // The official entrypoint restarts once after init; require two
                // consecutive successes a moment apart.
                if super::psql(docker, &cid, "SELECT 1").await.is_ok() {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    if super::psql(docker, &cid, "SELECT 1").await.is_ok() {
                        break;
                    }
                }
                anyhow::ensure!(std::time::Instant::now() < end, "throwaway postgres did not start");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let target = super::Target {
                project: row.project.clone(),
                service: row.service.clone(),
                container: name.clone(),
                container_id: cid.clone(),
                engine: "postgres".into(),
                image: row.image.clone(),
                database: db.clone(),
                user: user.clone(),
                running: true,
            };
            restore_from_file(docker, &target, &backups.root.join(&row.path)).await?;
        }
        let tables = super::psql(
            docker,
            &cid,
            "SELECT count(*) FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema')",
        )
        .await?;
        let amcheck = super::exec_sh(
            docker,
            &cid,
            &format!(
                "{} command -v pg_amcheck >/dev/null || {{ echo skipped; exit 0; }}; pg_amcheck --install-missing --no-password -d \"$PGDATABASE\" && echo clean",
                super::PG_ENV
            ),
        )
        .await
        .map(|o| o.lines().last().unwrap_or("").trim().to_string())
        .map_err(|e| anyhow::anyhow!("pg_amcheck found problems: {e}"))?;
        anyhow::Ok(format!("{tables} tables, amcheck {amcheck}"))
    }
    .await;
    super::pitr::remove(docker, &cid).await;
    if row.kind == "base" {
        let _ = docker.remove_volume(&volume, None::<bollard::query_parameters::RemoveVolumeOptions>).await;
    }
    if let Some(b) = bundle {
        let _ = tokio::fs::remove_file(b).await;
    }
    checked
}

// ---------------------------------------------------------------------------
// API (master-level, under /api/fleet/backups)
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/fleet/backups", get(list))
        .route("/fleet/backups/policy", put(set_policy))
        .route("/fleet/backups/run", post(run_now))
        .route("/fleet/backups/{id}/download", get(download))
        .route("/fleet/backups/{id}/restore", post(restore))
        .route("/fleet/backups/{id}/verify", post(verify_now))
        .route("/fleet/backups/pitr", post(pitr))
        .route("/fleet/backups/{id}", axum::routing::delete(delete_backup))
        .route("/fleet/restores/{rid}", get(restore_state))
}

fn store(state: &AppState) -> Result<Arc<Backups>, AppError> {
    state.backups.clone().ok_or(AppError::NotFound)
}

fn can_node(user: &CurrentUser, node: &str) -> bool {
    node == HOME || user.can_node(node)
}

#[derive(Deserialize)]
pub struct ListQuery {
    node: String,
    project: Option<String>,
    service: Option<String>,
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<serde_json::Value>> {
    store(&state)?;
    user.require(can_node(&user, &q.node))?;
    let node = q.node.clone();
    let rows: Vec<serde_json::Value> = state
        .db
        .call(move |c| {
            c.prepare(
                "SELECT id, project, service, engine, kind, trigger, started_at, finished_at, size, sha256, status, error,
                        label, verified, verified_at
                 FROM backups WHERE node = ?1 AND kind != 'wal' ORDER BY started_at DESC LIMIT 1000",
            )?
            .query_map([&node], |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?, "project": r.get::<_, String>(1)?, "service": r.get::<_, String>(2)?,
                    "engine": r.get::<_, String>(3)?, "kind": r.get::<_, String>(4)?, "trigger": r.get::<_, String>(5)?,
                    "started_at": r.get::<_, i64>(6)?, "finished_at": r.get::<_, Option<i64>>(7)?,
                    "size": r.get::<_, i64>(8)?, "sha256": r.get::<_, String>(9)?, "status": r.get::<_, String>(10)?,
                    "error": r.get::<_, String>(11)?, "label": r.get::<_, String>(12)?,
                    "verified": r.get::<_, String>(13)?, "verified_at": r.get::<_, Option<i64>>(14)?,
                }))
            })?
            .collect()
        })
        .await?;
    let keep = |project: &str, service: &str| {
        user.project(project).view
            && q.project.as_deref().is_none_or(|p| p == project)
            && q.service.as_deref().is_none_or(|s| s == service)
    };
    let backups: Vec<_> = rows
        .into_iter()
        .filter(|r| keep(r["project"].as_str().unwrap_or(""), r["service"].as_str().unwrap_or("")))
        .collect();
    let policies: Vec<_> = node_policies(&state, &q.node)
        .await?
        .into_iter()
        .filter(|p| keep(&p.project, &p.service))
        .map(|p| {
            let next = super::Cron::parse(&p.schedule)
                .ok()
                .and_then(|c| c.next_after(now_secs(), p.offset()));
            json!({ "policy": p, "next_run": next })
        })
        .collect();
    // WAL chain per database: how far it reaches, and whether it is whole.
    let n2 = q.node.clone();
    let wal_rows: Vec<(String, String, String, i64, i64)> = state
        .db
        .call(move |c| {
            c.prepare(
                "SELECT project, service, label, finished_at, size FROM backups
                 WHERE node = ?1 AND kind = 'wal' AND status = 'ok' ORDER BY project, service, label",
            )?
            .query_map([&n2], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, Option<i64>>(3)?.unwrap_or(0), r.get(4)?)))?
            .collect()
        })
        .await?;
    let mut wal: Vec<serde_json::Value> = Vec::new();
    let mut keys: Vec<(String, String)> = wal_rows.iter().map(|r| (r.0.clone(), r.1.clone())).collect();
    keys.dedup();
    for (project, service) in keys {
        if !keep(&project, &service) {
            continue;
        }
        let segs: Vec<&(String, String, String, i64, i64)> =
            wal_rows.iter().filter(|r| r.0 == project && r.1 == service).collect();
        let labels: Vec<String> = segs.iter().map(|r| r.2.clone()).collect();
        let latest_base_start = backups
            .iter()
            .filter(|b| b["project"] == project.as_str() && b["service"] == service.as_str() && b["kind"] == "base" && b["status"] == "ok")
            .filter_map(|b| b["label"].as_str().and_then(base_range).map(|(s, _)| s))
            .next();
        let gap = latest_base_start.as_deref().and_then(|start| chain_gap(start, &labels));
        wal.push(json!({
            "project": project,
            "service": service,
            "segments": segs.len(),
            "bytes": segs.iter().map(|r| r.4).sum::<i64>(),
            "first": labels.first(),
            "last": labels.last(),
            "last_at": segs.iter().map(|r| r.3).max(),
            "gap": gap,
        }));
    }
    Ok(Json(json!({ "backups": backups, "policies": policies, "wal": wal })))
}

#[derive(Deserialize)]
pub struct PolicyReq {
    node: String,
    policy: Policy,
}

pub async fn set_policy(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<PolicyReq>,
) -> AppResult<Json<serde_json::Value>> {
    let backups = store(&state)?;
    let p = req.policy;
    user.require(can_node(&user, &req.node) && user.project(&p.project).control)?;
    p.validate().map_err(AppError::BadRequest)?;
    if req.node == HOME && p.mode == "continuous" {
        return Err(AppError::BadRequest(
            "continuous mode streams WAL from a droplet to this server; use logical for the home server's own databases".into(),
        ));
    }
    let (node, json_p) = (req.node.clone(), serde_json::to_string(&p).expect("serialize"));
    let (project, service) = (p.project.clone(), p.service.clone());
    let now = now_secs();
    state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO backup_policies (node, project, service, policy, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (node, project, service) DO UPDATE SET policy = excluded.policy, updated_at = excluded.updated_at",
                rusqlite::params![node, project, service, json_p, now],
            )
        })
        .await?;
    audit(
        &state.db,
        &client_ip(&headers, &peer),
        &user.username,
        "backup.policy",
        &format!("{} {}/{} {} {}", req.node, p.project, p.service, p.mode, p.schedule),
        true,
    );
    // Push the new set where it runs.
    if req.node == HOME {
        *backups.local_policies.lock().unwrap() = node_policies(&state, HOME).await?;
        backups.local_changed.notify_one();
    } else if let Some(fleet) = &state.fleet {
        let policies = node_policies(&state, &req.node).await?;
        let _ = fleet.send(&req.node, &MasterMsg::Policies { policies }).await;
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct RunReq {
    node: String,
    project: String,
    service: String,
}

pub async fn run_now(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RunReq>,
) -> AppResult<Response> {
    store(&state)?;
    user.require(can_node(&user, &req.node) && user.project(&req.project).control)?;
    audit(
        &state.db,
        &client_ip(&headers, &peer),
        &user.username,
        "backup.run",
        &format!("{} {}/{}", req.node, req.project, req.service),
        true,
    );
    if req.node == HOME {
        let st = state.clone();
        tokio::spawn(async move {
            let _ = run_local(&st, &req.project, &req.service, "manual").await;
        });
    } else {
        let fleet = state.fleet.clone().ok_or(AppError::NotFound)?;
        let msg = MasterMsg::RunBackup { project: req.project, service: req.service, trigger: "manual".into() };
        if fleet.send(&req.node, &msg).await.is_err() {
            return Ok((StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": "node offline" }))).into_response());
        }
    }
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response())
}

pub async fn download(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    let backups = store(&state)?;
    let row = get_backup(&state, id).await?.ok_or(AppError::NotFound)?;
    // A dump holds all the data: needs the files capability, like reading configs.
    user.require(can_node(&user, &row.node) && user.project(&row.project).files)?;
    if row.status != "ok" {
        return Err(AppError::NotFound);
    }
    let file = tokio::fs::File::open(backups.root.join(&row.path)).await.map_err(|_| AppError::Gone)?;
    audit(
        &state.db,
        &client_ip(&headers, &peer),
        &user.username,
        "backup.download",
        &format!("{} {}/{} #{id}", row.node, row.project, row.service),
        true,
    );
    let name = FsPath::new(&row.path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "backup.dump".into());
    let name = format!("{}-{}-{}-{name}", row.node, row.project, row.service);
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, row.size.to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
        ],
        Body::from_stream(tokio_util::io::ReaderStream::new(file)),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct RestoreReq {
    /// Must equal `project/service`: typed by the user.
    confirm: String,
}

pub async fn restore(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<RestoreReq>,
) -> AppResult<Json<serde_json::Value>> {
    store(&state)?;
    // Overwrites a live database: admin only, with the name typed out.
    user.require(user.is_admin())?;
    let row = get_backup(&state, id).await?.ok_or(AppError::NotFound)?;
    if row.status != "ok" {
        return Err(AppError::BadRequest("only a successful backup can be restored".into()));
    }
    let expected = format!("{}/{}", row.project, row.service);
    if req.confirm != expected {
        return Err(AppError::BadRequest(format!("type {expected} to confirm")));
    }
    let detail = format!("{} {expected} from #{id} ({})", row.node, row.kind);
    let backups = store(&state)?;
    let job = match row.kind.as_str() {
        "logical" => RestoreJob {
            node: row.node.clone(),
            project: row.project.clone(),
            service: row.service.clone(),
            backup_id: row.id,
            file: backups.root.join(&row.path),
            size: row.size,
            sha256: row.sha256.clone(),
            kind: "logical".into(),
            target_time: None,
            temporary: false,
        },
        "base" => {
            // This base + all WAL after it, replayed to the end.
            let (file, size, sha256, _) =
                build_pitr_bundle(&state, &backups, &row.node, &row.project, &row.service, Some(row.id), None)
                    .await
                    .map_err(|e| AppError::BadRequest(e.to_string()))?;
            RestoreJob {
                node: row.node.clone(),
                project: row.project.clone(),
                service: row.service.clone(),
                backup_id: row.id,
                file,
                size,
                sha256,
                kind: "pitr".into(),
                target_time: None,
                temporary: true,
            }
        }
        _ => return Err(AppError::BadRequest("WAL segments are restored through a base backup".into())),
    };
    let rid = start_restore(&state, job).map_err(AppError::Internal)?;
    audit(&state.db, &client_ip(&headers, &peer), &user.username, "backup.restore", &detail, true);
    Ok(Json(json!({ "rid": rid })))
}

#[derive(Deserialize)]
pub struct PitrReq {
    node: String,
    project: String,
    service: String,
    /// Unix seconds; omitted = as late as the WAL goes.
    target_time: Option<i64>,
    confirm: String,
}

/// Point-in-time restore of a continuous-mode database on a droplet.
pub async fn pitr(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<PitrReq>,
) -> AppResult<Json<serde_json::Value>> {
    let backups = store(&state)?;
    user.require(user.is_admin())?;
    let expected = format!("{}/{}", req.project, req.service);
    if req.confirm != expected {
        return Err(AppError::BadRequest(format!("type {expected} to confirm")));
    }
    if req.node == HOME {
        return Err(AppError::BadRequest("point-in-time restore is for droplets in continuous mode".into()));
    }
    let (file, size, sha256, base) =
        build_pitr_bundle(&state, &backups, &req.node, &req.project, &req.service, None, req.target_time)
            .await
            .map_err(|e| AppError::BadRequest(e.to_string()))?;
    let job = RestoreJob {
        node: req.node.clone(),
        project: req.project.clone(),
        service: req.service.clone(),
        backup_id: base.id,
        file,
        size,
        sha256,
        kind: "pitr".into(),
        target_time: req.target_time,
        temporary: true,
    };
    let rid = start_restore(&state, job).map_err(AppError::Internal)?;
    audit(
        &state.db,
        &client_ip(&headers, &peer),
        &user.username,
        "backup.pitr",
        &format!("{} {expected} to {:?} via base #{}", req.node, req.target_time, base.id),
        true,
    );
    Ok(Json(json!({ "rid": rid, "base_id": base.id })))
}

pub async fn verify_now(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
) -> AppResult<Response> {
    store(&state)?;
    let row = get_backup(&state, id).await?.ok_or(AppError::NotFound)?;
    user.require(can_node(&user, &row.node) && user.project(&row.project).control)?;
    let st = state.clone();
    tokio::spawn(async move {
        let _ = verify_backup(&st, id).await;
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response())
}

pub async fn restore_state(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(rid): Path<u64>,
) -> AppResult<Json<RestoreState>> {
    let backups = store(&state)?;
    user.require(user.is_admin())?;
    let r = backups.restores.lock().unwrap().get(&rid).cloned();
    r.map(Json).ok_or(AppError::NotFound)
}

pub async fn delete_backup(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let backups = store(&state)?;
    user.require(user.is_admin())?;
    let row = get_backup(&state, id).await?.ok_or(AppError::NotFound)?;
    if !row.path.is_empty() {
        let _ = tokio::fs::remove_file(backups.root.join(&row.path)).await;
    }
    state.db.call(move |c| c.execute("DELETE FROM backups WHERE id = ?1", [id])).await?;
    audit(
        &state.db,
        &client_ip(&headers, &peer),
        &user.username,
        "backup.delete",
        &format!("{} {}/{} #{id}", row.node, row.project, row.service),
        true,
    );
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_roll_over_log_files() {
        assert_eq!(next_segment("0000000100000000000000FE").unwrap(), "0000000100000000000000FF");
        assert_eq!(next_segment("0000000100000000000000FF").unwrap(), "000000010000000100000000");
    }

    #[test]
    fn chain_gap_finds_first_hole_and_ignores_older_segments() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let start = "000000010000000000000003";
        let whole = l(&["000000010000000000000002", start, "000000010000000000000004", "000000010000000000000005"]);
        assert_eq!(chain_gap(start, &whole), None);
        let holed = l(&[start, "000000010000000000000005"]);
        assert_eq!(chain_gap(start, &holed).as_deref(), Some("000000010000000000000004"));
        // Segments of an older timeline don't count against the chain.
        let other_tli = l(&["000000020000000000000001", start]);
        assert_eq!(chain_gap(start, &other_tli), None);
    }
}
