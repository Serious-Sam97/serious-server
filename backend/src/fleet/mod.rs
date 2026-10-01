//! Fleet link: one master (the home server) and headless agents on droplets.
//!
//! Agents dial OUT to the master's fleet listener and keep one WebSocket open.
//! Everything rides that socket as JSON text frames: enrollment, metrics,
//! docker events, and the API tunnel (HTTP requests and WebSocket streams the
//! master forwards on behalf of a logged-in user).

pub mod agent;
pub mod alerts;
pub mod clickhouse;
pub mod containers;
pub mod history;
pub mod master;
pub mod net;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::system::HistoryPoint;
use crate::auth::perms::Permissions;
use crate::backups::{BackupMeta, Policy};

/// Bumped on incompatible protocol changes; both sides refuse a mismatch
/// with a clear message instead of misbehaving.
pub const PROTO: u32 = 1;

/// Header carrying the acting user on tunneled requests. Only the master sets
/// it: the agent's tunnel router is reachable solely through the fleet socket
/// (headless agents have no HTTP listener at all).
pub const ACTOR_HEADER: &str = "x-ss-actor";

/// Who a tunneled request acts for. The agent enforces project permissions
/// with these, exactly as the master would for a local request.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Actor {
    pub username: String,
    pub admin: bool,
    pub perms: Permissions,
}

/// Small per-sample context the overview needs beyond a history point.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Summary {
    pub mem_total: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub cores: u64,
    pub uptime: u64,
    pub cpu_temp: Option<f64>,
    #[serde(default)]
    pub swap_total: u64,
    #[serde(default)]
    pub swap_used: u64,
}

impl Summary {
    /// Pull the summary out of a sampler payload (the `/ws/system` JSON).
    pub fn from_payload(payload: &str) -> Option<Summary> {
        let v: serde_json::Value = serde_json::from_str(payload).ok()?;
        let root = v["disks"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["mount"] == "/").or_else(|| d.first()));
        Some(Summary {
            mem_total: v["mem"]["total"].as_u64()?,
            disk_total: root.and_then(|d| d["total"].as_u64()).unwrap_or(0),
            disk_used: root.and_then(|d| d["used"].as_u64()).unwrap_or(0),
            cores: v["cpu"]["per_core"].as_array().map(|c| c.len() as u64).unwrap_or(0),
            uptime: v["uptime"].as_u64().unwrap_or(0),
            cpu_temp: v["cpu"]["temp"].as_f64(),
            swap_total: v["mem"]["swap_total"].as_u64().unwrap_or(0),
            swap_used: v["mem"]["swap_used"].as_u64().unwrap_or(0),
        })
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum AgentMsg {
    Hello {
        proto: u32,
        version: String,
        hostname: String,
        /// Seconds between lite samples.
        interval: u64,
        /// Capabilities granted to the master (SS_AGENT_ALLOW).
        allow: Vec<String>,
    },
    Metrics {
        points: Vec<HistoryPoint>,
        summary: Option<Summary>,
    },
    Event {
        kind: String,
        detail: serde_json::Value,
    },
    /// Per-container usage, averaged over the last minute.
    ContainerStats {
        ts: i64,
        items: Vec<containers::ContainerStat>,
    },
    HttpResponse {
        id: u64,
        status: u16,
        headers: Vec<(String, String)>,
        /// base64
        body: String,
    },
    WsOpened {
        id: u64,
        /// 101 on success, else the HTTP status the handshake was refused with.
        status: u16,
    },
    WsData {
        id: u64,
        text: String,
        /// `text` is base64 of a binary frame (the terminal speaks binary).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        binary: bool,
    },
    WsClosed {
        id: u64,
    },
    /// A finished backup waiting in the agent's spool. Chunks follow as
    /// binary frames ([`FRAME_UPLOAD`]) from the offset the master acks.
    UploadBegin {
        id: u64,
        meta: BackupMeta,
        size: u64,
        sha256: String,
    },
    /// A scheduled/manual backup could not be taken at all.
    BackupFailed { meta: BackupMeta, error: String },
    /// Restore download flow control: bytes received so far.
    RestoreAck { rid: u64, offset: u64 },
    RestoreStatus {
        rid: u64,
        stage: String,
        /// Some(true/false) once finished.
        ok: Option<bool>,
        message: String,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum MasterMsg {
    /// First-connection reply to a join token: the credentials to keep.
    Enrolled { node: String, secret: String },
    /// Accepted. `last_ts` = newest metrics point the master holds, so the
    /// agent can replay what the master missed while the link was down.
    Welcome { node: String, last_ts: f64 },
    /// Refused (bad credentials, protocol mismatch …) — the agent logs it.
    Error { message: String },
    HttpRequest {
        id: u64,
        method: String,
        /// Path + query below `/api`, e.g. `/git/myapp/status`.
        path: String,
        headers: Vec<(String, String)>,
        /// base64
        body: String,
    },
    WsOpen {
        id: u64,
        path: String,
        headers: Vec<(String, String)>,
    },
    WsData {
        id: u64,
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        binary: bool,
    },
    WsClose {
        id: u64,
    },
    /// The complete set of backup policies for this node (replaces the old).
    Policies { policies: Vec<Policy> },
    RunBackup {
        project: String,
        service: String,
        trigger: String,
    },
    /// Upload flow control: bytes stored so far (also the resume point).
    UploadAck { id: u64, offset: u64 },
    UploadDone {
        id: u64,
        ok: bool,
        error: Option<String>,
    },
    /// Restore a backup the master holds; chunks follow as [`FRAME_RESTORE`].
    RestoreStart {
        rid: u64,
        project: String,
        service: String,
        size: u64,
        sha256: String,
        /// `logical` (a pg_dump archive) or `pitr` (base + WAL bundle).
        #[serde(default = "logical")]
        kind: String,
        /// PITR only: stop replaying at this moment (unix secs); None = latest.
        #[serde(default)]
        target_time: Option<i64>,
    },
}

/// Binary frame kinds: `[kind u8][id u64 BE][offset u64 BE][payload]`.
pub const FRAME_UPLOAD: u8 = 1;
pub const FRAME_RESTORE: u8 = 2;
/// Bytes per transfer chunk: far below any proxy's WebSocket message limit.
pub const CHUNK: usize = 512 * 1024;
/// Chunks in flight before waiting for an ack.
pub const WINDOW: u64 = 8;

pub fn frame(kind: u8, id: u64, offset: u64, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(17 + payload.len());
    v.push(kind);
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(&offset.to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn parse_frame(b: &[u8]) -> Option<(u8, u64, u64, &[u8])> {
    if b.len() < 17 {
        return None;
    }
    let id = u64::from_be_bytes(b[1..9].try_into().ok()?);
    let offset = u64::from_be_bytes(b[9..17].try_into().ok()?);
    Some((b[0], id, offset, &b[17..]))
}

fn logical() -> String {
    "logical".into()
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 256 bits of randomness, hex-encoded.
pub fn random_secret() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    format!("{:032x}{:032x}", rng.random::<u128>(), rng.random::<u128>())
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
