//! Master side of the fleet link: the agent listener (`/fleet/connect` on
//! its own port), the live node registry, the API tunnel that forwards a
//! user's requests to a node, and the fleet admin endpoints.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::clickhouse::{self, Clickhouse, Row};
use super::{now_secs, random_secret, sha256_hex, Actor, AgentMsg, MasterMsg, Summary, ACTOR_HEADER, PROTO};
use crate::api::system::HistoryPoint;
use crate::audit::audit;
use crate::auth::perms::CurrentUser;
use crate::auth::{check_ws_origin, client_ip};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// In-memory metrics kept per node for the overview sparklines.
const RING_SECS: f64 = 30.0 * 60.0;
const EVENTS_KEPT: usize = 50;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// An agent sends metrics every few seconds and pings every 30 s.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const JOIN_TOKEN_TTL: i64 = 15 * 60;
/// Failed agent authentications allowed per IP per minute.
const AUTH_FAILURES_PER_MIN: usize = 10;
/// Headers forwarded from the browser to a node.
const FORWARD_REQ: [&str; 3] = ["content-type", "accept", "x-csrf"];
/// Headers forwarded from a node's response back to the browser.
const FORWARD_RESP: [&str; 3] = ["content-type", "content-disposition", "etag"];

pub struct HttpReply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

enum StreamMsg {
    Opened(u16),
    Data(String, bool),
    Closed,
}

struct Conn {
    id: u64,
    tx: mpsc::Sender<String>,
    /// Transfer chunks: written only when no control message is waiting.
    bulk: mpsc::Sender<Vec<u8>>,
    pending: HashMap<u64, oneshot::Sender<HttpReply>>,
    streams: HashMap<u64, mpsc::Sender<StreamMsg>>,
}

#[derive(Clone, Default, serde::Serialize)]
struct HelloInfo {
    version: String,
    hostname: String,
    interval: u64,
    allow: Vec<String>,
}

#[derive(Default)]
struct NodeLive {
    conn: Option<Conn>,
    hello: Option<HelloInfo>,
    ring: VecDeque<HistoryPoint>,
    summary: Option<Summary>,
    events: VecDeque<serde_json::Value>,
    connected_at: Option<i64>,
    last_seen: Option<i64>,
}

pub struct Fleet {
    nodes: Mutex<HashMap<String, NodeLive>>,
    next_id: AtomicU64,
    clickhouse: Option<Clickhouse>,
    ch_tx: Option<mpsc::Sender<Row>>,
    ch_rx: Mutex<Option<mpsc::Receiver<Row>>>,
    /// Newest point ClickHouse already holds per node (loaded at startup),
    /// so a master restart doesn't make agents replay duplicates.
    ch_last_ts: Mutex<HashMap<String, f64>>,
    auth_failures: Mutex<HashMap<String, Vec<Instant>>>,
    pub alerts: super::alerts::Alerts,
}

impl Fleet {
    pub fn new(config: &crate::config::Config) -> Self {
        let clickhouse = config.clickhouse.as_ref().map(Clickhouse::new);
        let (ch_tx, ch_rx) = match clickhouse {
            Some(_) => {
                let (tx, rx) = mpsc::channel(10_000);
                (Some(tx), Some(rx))
            }
            None => (None, None),
        };
        Self {
            nodes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            clickhouse,
            ch_tx,
            ch_rx: Mutex::new(ch_rx),
            ch_last_ts: Mutex::new(HashMap::new()),
            auth_failures: Mutex::new(HashMap::new()),
            alerts: Default::default(),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn record(&self, row: Row) {
        if let Some(tx) = &self.ch_tx {
            // Never block the fleet on ClickHouse; the writer buffers.
            let _ = tx.try_send(row);
        }
    }

    fn event(&self, node: &str, kind: &str, detail: serde_json::Value) {
        let ts = now_secs();
        {
            let mut nodes = self.nodes.lock().unwrap();
            let live = nodes.entry(node.to_string()).or_default();
            if live.events.len() >= EVENTS_KEPT {
                live.events.pop_front();
            }
            live.events
                .push_back(json!({ "ts": ts, "kind": kind, "detail": detail }));
        }
        self.record(Row::Event {
            node: node.to_string(),
            ts,
            kind: kind.to_string(),
            detail: detail.to_string(),
        });
    }

    fn rate_limited(&self, ip: &str) -> bool {
        let mut map = self.auth_failures.lock().unwrap();
        let v = map.entry(ip.to_string()).or_default();
        v.retain(|t| t.elapsed() < Duration::from_secs(60));
        v.len() >= AUTH_FAILURES_PER_MIN
    }

    fn auth_failed(&self, ip: &str) {
        let mut map = self.auth_failures.lock().unwrap();
        map.entry(ip.to_string()).or_default().push(Instant::now());
        if map.len() > 10_000 {
            map.retain(|_, v| v.iter().any(|t| t.elapsed() < Duration::from_secs(60)));
        }
    }

    fn last_ts(&self, node: &str) -> f64 {
        let ring = self
            .nodes
            .lock()
            .unwrap()
            .get(node)
            .and_then(|n| n.ring.back().map(|p| p[0]))
            .unwrap_or(0.0);
        let stored = self.ch_last_ts.lock().unwrap().get(node).copied().unwrap_or(0.0);
        ring.max(stored)
    }

    fn attach(&self, node: &str, tx: mpsc::Sender<String>, bulk: mpsc::Sender<Vec<u8>>, hello: HelloInfo) -> u64 {
        let id = self.next_id();
        let mut nodes = self.nodes.lock().unwrap();
        let live = nodes.entry(node.to_string()).or_default();
        // A reconnect replaces the old link; dropping its sender ends the
        // old writer, which closes the stale socket.
        live.conn = Some(Conn {
            id,
            tx,
            bulk,
            pending: HashMap::new(),
            streams: HashMap::new(),
        });
        live.hello = Some(hello);
        live.connected_at = Some(now_secs());
        live.last_seen = Some(now_secs());
        id
    }

    fn detach(&self, node: &str, conn_id: u64) -> bool {
        let mut nodes = self.nodes.lock().unwrap();
        let Some(live) = nodes.get_mut(node) else {
            return false;
        };
        if live.conn.as_ref().is_some_and(|c| c.id == conn_id) {
            // Dropping pending senders fails waiting requests; dropping
            // stream senders ends their relays.
            live.conn = None;
            live.connected_at = None;
            true
        } else {
            false
        }
    }

    /// Forget a node entirely (revoked): its link is dropped immediately.
    fn kick(&self, node: &str) {
        self.nodes.lock().unwrap().remove(node);
    }

    fn push_metrics(&self, node: &str, points: Vec<HistoryPoint>, summary: Option<Summary>) {
        let mut fresh = Vec::new();
        {
            let mut nodes = self.nodes.lock().unwrap();
            let live = nodes.entry(node.to_string()).or_default();
            let newest = live.ring.back().map(|p| p[0]).unwrap_or(0.0);
            for p in points {
                if p[0] > newest {
                    live.ring.push_back(p);
                    fresh.push(p);
                }
            }
            if let Some(last) = live.ring.back().map(|p| p[0]) {
                while live.ring.front().is_some_and(|p| p[0] < last - RING_SECS) {
                    live.ring.pop_front();
                }
            }
            if summary.is_some() {
                live.summary = summary;
            }
            live.last_seen = Some(now_secs());
        }
        let stored = self.ch_last_ts.lock().unwrap().get(node).copied().unwrap_or(0.0);
        for point in fresh.into_iter().filter(|p| p[0] > stored) {
            self.record(Row::Metric {
                node: node.to_string(),
                point,
            });
        }
    }

    /// Send one control message to a connected node.
    pub async fn send(&self, node: &str, msg: &MasterMsg) -> Result<(), ()> {
        let (tx, _) = self.sender(node).ok_or(())?;
        tx.send(serde_json::to_string(msg).expect("serialize")).await.map_err(|_| ())
    }

    /// Send one binary transfer frame (waits while the window is full).
    pub async fn send_bulk(&self, node: &str, frame: Vec<u8>) -> Result<(), ()> {
        let tx = self
            .nodes
            .lock()
            .unwrap()
            .get(node)
            .and_then(|n| n.conn.as_ref())
            .map(|c| c.bulk.clone())
            .ok_or(())?;
        tx.send(frame).await.map_err(|_| ())
    }

    pub fn backup_event(&self, node: &str, project: &str, service: &str, ok: bool, detail: &str) {
        self.event(
            node,
            if ok { "backup" } else { "backup_failed" },
            json!({ "project": project, "service": service, "detail": detail }),
        );
    }

    /// (online, last seen, latest summary) for the alert evaluator.
    pub fn live_status(&self, node: &str) -> (bool, Option<i64>, Option<Summary>) {
        let nodes = self.nodes.lock().unwrap();
        match nodes.get(node) {
            Some(n) => (n.conn.is_some(), n.last_seen, n.summary.clone()),
            None => (false, None, None),
        }
    }

    fn touch(&self, node: &str) {
        if let Some(live) = self.nodes.lock().unwrap().get_mut(node) {
            live.last_seen = Some(now_secs());
        }
    }

    fn sender(&self, node: &str) -> Option<(mpsc::Sender<String>, u64)> {
        self.nodes
            .lock()
            .unwrap()
            .get(node)
            .and_then(|n| n.conn.as_ref())
            .map(|c| (c.tx.clone(), c.id))
    }

    fn resolve(&self, node: &str, conn_id: u64, id: u64, reply: HttpReply) {
        let tx = {
            let mut nodes = self.nodes.lock().unwrap();
            nodes
                .get_mut(node)
                .and_then(|n| n.conn.as_mut())
                .filter(|c| c.id == conn_id)
                .and_then(|c| c.pending.remove(&id))
        };
        if let Some(tx) = tx {
            let _ = tx.send(reply);
        }
    }

    fn stream_msg(&self, node: &str, conn_id: u64, id: u64, msg: StreamMsg) {
        let closing = matches!(msg, StreamMsg::Closed);
        let tx = {
            let mut nodes = self.nodes.lock().unwrap();
            let conn = nodes
                .get_mut(node)
                .and_then(|n| n.conn.as_mut())
                .filter(|c| c.id == conn_id);
            match conn {
                Some(c) if closing => c.streams.remove(&id),
                Some(c) => c.streams.get(&id).cloned(),
                None => None,
            }
        };
        if let Some(tx) = tx {
            // A slow browser applies backpressure all the way to the agent's
            // reader only for its own stream; others use their own channels.
            let _ = tx.try_send(msg);
        }
    }

    /// Forward one HTTP request to a node and wait for its answer.
    async fn request(&self, node: &str, msg_for: impl FnOnce(u64) -> MasterMsg) -> Result<HttpReply, Response> {
        let id = self.next_id();
        let (reply_tx, reply_rx) = oneshot::channel();
        let tx = {
            let mut nodes = self.nodes.lock().unwrap();
            let conn = nodes.get_mut(node).and_then(|n| n.conn.as_mut());
            match conn {
                Some(c) => {
                    c.pending.insert(id, reply_tx);
                    c.tx.clone()
                }
                None => return Err(node_error(StatusCode::SERVICE_UNAVAILABLE, "node offline")),
            }
        };
        let text = serde_json::to_string(&msg_for(id)).expect("serialize");
        if tx.send(text).await.is_err() {
            return Err(node_error(StatusCode::SERVICE_UNAVAILABLE, "node offline"));
        }
        match tokio::time::timeout(REQUEST_TIMEOUT, reply_rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(node_error(StatusCode::BAD_GATEWAY, "node disconnected")),
            Err(_) => {
                if let Some(c) = self.nodes.lock().unwrap().get_mut(node).and_then(|n| n.conn.as_mut()) {
                    c.pending.remove(&id);
                }
                Err(node_error(StatusCode::GATEWAY_TIMEOUT, "node did not answer in time"))
            }
        }
    }

    fn open_stream(&self, node: &str) -> Option<(u64, mpsc::Sender<String>, mpsc::Receiver<StreamMsg>)> {
        let id = self.next_id();
        let (tx, rx) = mpsc::channel(256);
        let mut nodes = self.nodes.lock().unwrap();
        let conn = nodes.get_mut(node)?.conn.as_mut()?;
        conn.streams.insert(id, tx);
        Some((id, conn.tx.clone(), rx))
    }

    fn close_stream(&self, node: &str, id: u64) {
        let tx = {
            let mut nodes = self.nodes.lock().unwrap();
            nodes
                .get_mut(node)
                .and_then(|n| n.conn.as_mut())
                .and_then(|c| c.streams.remove(&id).map(|_| c.tx.clone()))
        };
        if let Some(tx) = tx {
            let _ = tx.try_send(serde_json::to_string(&MasterMsg::WsClose { id }).expect("serialize"));
        }
    }
}

fn node_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

// ---------------------------------------------------------------------------
// Startup
// ---------------------------------------------------------------------------

/// Start the ClickHouse writer, feed the home node's own samples, and bind
/// the agent listener.
pub async fn spawn(state: AppState) -> anyhow::Result<()> {
    let fleet = state.fleet.clone().expect("master mode");

    if let (Some(ch), Some(rx)) = (fleet.clickhouse.clone(), fleet.ch_rx.lock().unwrap().take()) {
        if let Err(e) = ch.init_schema().await {
            tracing::warn!("clickhouse schema init failed ({e}); the writer retries");
        }
        match ch.last_ts_per_node().await {
            Ok(map) => *fleet.ch_last_ts.lock().unwrap() = map,
            Err(e) => tracing::warn!("clickhouse not reachable yet ({e}); history resumes when it is"),
        }
        clickhouse::spawn_writer(ch, rx);

        // The home server is a node in the history too.
        let fleet_home = fleet.clone();
        let sampler = state.sampler.clone();
        tokio::spawn(async move {
            let mut rx = sampler.rx.clone();
            while rx.changed().await.is_ok() {
                let point = sampler.history.lock().unwrap().back().copied();
                if let Some(point) = point {
                    fleet_home.record(Row::Metric {
                        node: "home".into(),
                        point,
                    });
                }
            }
        });
    }

    super::alerts::spawn(state.clone());
    let listener = tokio::net::TcpListener::bind(state.config.fleet_bind).await?;
    tracing::info!("fleet listener on {}", state.config.fleet_bind);
    let app = Router::new()
        .route("/fleet/connect", get(connect))
        .route("/fleet/health", get(|| async { Json(json!({ "ok": true })) }))
        .with_state(state);
    tokio::spawn(async move {
        if let Err(e) = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        {
            tracing::error!("fleet listener stopped: {e}");
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent connections
// ---------------------------------------------------------------------------

enum Auth {
    Known(String),
    Enrolled { node: String, secret: String },
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> anyhow::Result<Option<Auth>> {
    if let Some(secret) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    {
        let hash = sha256_hex(secret.trim());
        let name: Option<String> = state
            .db
            .call(move |c| {
                c.query_row("SELECT name FROM nodes WHERE secret_hash = ?1", [&hash], |r| r.get(0))
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(other),
                    })
            })
            .await?;
        return Ok(name.map(Auth::Known));
    }
    if let Some(token) = headers.get("x-ss-join").and_then(|v| v.to_str().ok()) {
        let token_hash = sha256_hex(token.trim());
        let secret = random_secret();
        let secret_hash = sha256_hex(&secret);
        let now = now_secs();
        // Single use: the token row is consumed in the same transaction that
        // creates the node.
        let node: Option<String> = state
            .db
            .call(move |c| {
                let tx = c.unchecked_transaction()?;
                let row = tx
                    .query_row(
                        "SELECT name, env, color FROM join_tokens WHERE token_hash = ?1 AND expires_at >= ?2",
                        rusqlite::params![token_hash, now],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)),
                    )
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(other),
                    })?;
                let Some((name, env, color)) = row else {
                    return Ok(None);
                };
                tx.execute("DELETE FROM join_tokens WHERE token_hash = ?1", [&token_hash])?;
                tx.execute(
                    "INSERT INTO nodes (name, env, color, secret_hash) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![name, env, color, secret_hash],
                )?;
                tx.commit()?;
                Ok(Some(name))
            })
            .await?;
        return Ok(node.map(|node| Auth::Enrolled { node, secret }));
    }
    Ok(None)
}

async fn connect(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let fleet = state.fleet.clone().expect("master mode");
    let ip = client_ip(&headers, &peer);
    if fleet.rate_limited(&ip) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    match authenticate(&state, &headers).await {
        Ok(Some(auth)) => ws
            .max_message_size(32 << 20)
            .on_upgrade(move |socket| run_node(state, auth, socket, ip)),
        Ok(None) => {
            fleet.auth_failed(&ip);
            tracing::warn!(ip, "fleet: rejected agent credentials");
            StatusCode::UNAUTHORIZED.into_response()
        }
        Err(e) => {
            tracing::error!("fleet auth: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn run_node(state: AppState, auth: Auth, socket: WebSocket, ip: String) {
    let fleet = state.fleet.clone().expect("master mode");
    let (mut sink, mut stream) = socket.split();
    let (node, new_secret) = match auth {
        Auth::Known(node) => (node, None),
        Auth::Enrolled { node, secret } => (node, Some(secret)),
    };
    let send = |msg: &MasterMsg| Message::Text(serde_json::to_string(msg).expect("serialize").into());

    // The agent speaks first: protocol version and who it is.
    let hello = match tokio::time::timeout(Duration::from_secs(10), stream.next()).await {
        Ok(Some(Ok(Message::Text(t)))) => serde_json::from_str::<AgentMsg>(&t).ok(),
        _ => None,
    };
    let Some(AgentMsg::Hello { proto, version, hostname, interval, allow }) = hello else {
        tracing::warn!(node, "fleet: agent did not send hello");
        return;
    };
    if proto != PROTO {
        let message = format!("protocol mismatch: master speaks {PROTO}, agent {proto} — update the older side");
        let _ = sink.send(send(&MasterMsg::Error { message: message.clone() })).await;
        tracing::warn!(node, "fleet: {message}");
        return;
    }
    if let Some(secret) = new_secret {
        audit(&state.db, &ip, "fleet", "node.enroll", &node, true);
        if sink
            .send(send(&MasterMsg::Enrolled { node: node.clone(), secret }))
            .await
            .is_err()
        {
            return;
        }
    }
    let last_ts = fleet.last_ts(&node);
    if sink
        .send(send(&MasterMsg::Welcome { node: node.clone(), last_ts }))
        .await
        .is_err()
    {
        return;
    }

    let info = HelloInfo { version, hostname, interval, allow };
    let info_json = serde_json::to_string(&info).unwrap_or_default();
    let (tx, mut rx) = mpsc::channel::<String>(1024);
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(4);
    let conn_id = fleet.attach(&node, tx.clone(), bulk_tx, info);
    // The node's backup policies, so its scheduler runs even while we're away.
    if let Ok(policies) = crate::backups::master::node_policies(&state, &node).await {
        let _ = tx.send(serde_json::to_string(&MasterMsg::Policies { policies }).expect("serialize")).await;
    }
    drop(tx);
    tracing::info!(node, ip, "fleet: node connected");
    fleet.event(&node, "online", json!({ "ip": ip }));
    {
        let node = node.clone();
        let now = now_secs();
        let _ = state
            .db
            .call(move |c| {
                c.execute(
                    "UPDATE nodes SET last_seen = ?1, info = ?2 WHERE name = ?3",
                    rusqlite::params![now, info_json, node],
                )
            })
            .await;
    }

    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased; // control and metrics before bulk transfer chunks
                m = rx.recv() => match m {
                    Some(t) => Message::Text(t.into()),
                    None => break,
                },
                m = bulk_rx.recv() => match m {
                    Some(b) => Message::Binary(b.into()),
                    None => break,
                },
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let mut uploads = crate::backups::master::Uploads::new();
    let reply = |msg: MasterMsg| {
        let fleet = fleet.clone();
        let node = node.clone();
        async move {
            let _ = fleet.send(&node, &msg).await;
        }
    };
    loop {
        let msg = match tokio::time::timeout(IDLE_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(msg))) => msg,
            _ => break,
        };
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            Message::Binary(b) => {
                fleet.touch(&node);
                if let Some((super::FRAME_UPLOAD, id, offset, payload)) = super::parse_frame(&b) {
                    if let Some(answer) =
                        crate::backups::master::upload_chunk(&state, &node, &mut uploads, id, offset, payload).await
                    {
                        reply(answer).await;
                    }
                }
                continue;
            }
            _ => {
                fleet.touch(&node);
                continue;
            }
        };
        let Ok(msg) = serde_json::from_str::<AgentMsg>(&text) else {
            continue;
        };
        match msg {
            AgentMsg::Metrics { points, summary } => fleet.push_metrics(&node, points, summary),
            AgentMsg::Event { kind, detail } => fleet.event(&node, &kind, detail),
            AgentMsg::HttpResponse { id, status, headers, body } => {
                let body = B64.decode(body).unwrap_or_default();
                fleet.resolve(&node, conn_id, id, HttpReply { status, headers, body });
            }
            AgentMsg::WsOpened { id, status } => {
                fleet.stream_msg(&node, conn_id, id, StreamMsg::Opened(status))
            }
            AgentMsg::WsData { id, text, binary } => {
                fleet.stream_msg(&node, conn_id, id, StreamMsg::Data(text, binary))
            }
            AgentMsg::WsClosed { id } => fleet.stream_msg(&node, conn_id, id, StreamMsg::Closed),
            AgentMsg::UploadBegin { id, meta, size, sha256 } => {
                let answer =
                    crate::backups::master::upload_begin(&state, &node, &mut uploads, id, meta, size, sha256).await;
                reply(answer).await;
            }
            AgentMsg::BackupFailed { meta, error } => {
                crate::backups::master::record_failure(&state, &node, &meta, &error).await;
            }
            AgentMsg::RestoreAck { rid, offset } => {
                if let Some(b) = &state.backups {
                    b.restore_ack(rid, offset);
                }
            }
            AgentMsg::RestoreStatus { rid, stage, ok, message } => {
                if let Some(b) = &state.backups {
                    b.restore_status(rid, stage, ok, message);
                }
            }
            AgentMsg::Hello { .. } => {}
        }
        // Revoked while connected: `kick` removed the entry.
        if fleet.sender(&node).is_none_or(|(_, id)| id != conn_id) {
            break;
        }
    }

    if fleet.detach(&node, conn_id) {
        tracing::info!(node, "fleet: node disconnected");
        fleet.event(&node, "offline", json!({}));
        let now = now_secs();
        let name = node.clone();
        let _ = state
            .db
            .call(move |c| c.execute("UPDATE nodes SET last_seen = ?1 WHERE name = ?2", rusqlite::params![now, name]))
            .await;
    }
    writer.abort();
}

// ---------------------------------------------------------------------------
// User-facing API (session-authenticated, mounted under /api)
// ---------------------------------------------------------------------------

pub fn api_routes() -> Router<AppState> {
    Router::new()
        .route("/fleet/nodes", get(list_nodes))
        .route("/fleet/nodes/{node}/history", get(node_history))
        .route("/fleet/alerts", get(super::alerts::list))
        .route("/nodes/{node}/ws/{*rest}", get(proxy_ws))
        .route("/nodes/{node}/{*rest}", any(proxy_http))
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/admin/fleet/tokens", post(create_token))
        .route(
            "/admin/fleet/nodes/{name}",
            axum::routing::put(update_node).delete(delete_node),
        )
}

fn fleet_of(state: &AppState) -> Result<Arc<Fleet>, AppError> {
    state.fleet.clone().ok_or(AppError::NotFound)
}

fn actor_headers(user: &CurrentUser, ip: &str) -> Vec<(String, String)> {
    let actor = Actor {
        username: user.username.clone(),
        admin: user.is_admin(),
        perms: user.perms.clone(),
    };
    vec![
        (ACTOR_HEADER.into(), B64.encode(serde_json::to_vec(&actor).expect("serialize"))),
        ("cf-connecting-ip".into(), ip.to_string()),
    ]
}

/// Sparkline: CPU% of the last `n` points.
fn spark(points: impl DoubleEndedIterator<Item = HistoryPoint>, n: usize) -> Vec<f32> {
    let mut v: Vec<f32> = points.rev().take(n).map(|p| p[1] as f32).collect();
    v.reverse();
    v
}

pub async fn list_nodes(
    State(state): State<AppState>,
    user: CurrentUser,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    let mut out = Vec::new();

    // The home server itself, from the local sampler.
    let payload = state.sampler.rx.borrow().clone();
    let home_last = state.sampler.history.lock().unwrap().back().copied();
    let home_spark = spark(state.sampler.history.lock().unwrap().iter().copied(), 90);
    out.push(json!({
        "name": "home",
        "env": "home",
        "color": null,
        "local": true,
        "online": true,
        "hostname": serde_json::from_str::<serde_json::Value>(&payload).ok()
            .and_then(|v| v["host"]["hostname"].as_str().map(String::from)),
        "version": env!("CARGO_PKG_VERSION"),
        "last": home_last,
        "summary": Summary::from_payload(&payload),
        "spark": home_spark,
        "can_system": user.can_system(),
    }));

    let rows: Vec<(String, String, String, Option<i64>, String)> = state
        .db
        .call(|c| {
            c.prepare("SELECT name, env, color, last_seen, info FROM nodes ORDER BY name")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect()
        })
        .await?;
    let fleet = state.fleet.clone();
    for (name, env, color, db_last_seen, info) in rows {
        if !user.can_node(&name) {
            continue;
        }
        let stored: serde_json::Value = serde_json::from_str(&info).unwrap_or(json!({}));
        let mut entry = json!({
            "name": name,
            "env": env,
            "color": color,
            "local": false,
            "online": false,
            "hostname": stored["hostname"],
            "version": stored["version"],
            "allow": stored["allow"],
            "last_seen": db_last_seen,
            "last": null,
            "summary": null,
            "spark": [],
            "events": [],
        });
        if let Some(fleet) = &fleet {
            let nodes = fleet.nodes.lock().unwrap();
            if let Some(live) = nodes.get(&name) {
                entry["online"] = json!(live.conn.is_some());
                entry["connected_at"] = json!(live.connected_at);
                if let Some(ts) = live.last_seen {
                    entry["last_seen"] = json!(ts);
                }
                if let Some(h) = &live.hello {
                    entry["hostname"] = json!(h.hostname);
                    entry["version"] = json!(h.version);
                    entry["allow"] = json!(h.allow);
                    entry["interval"] = json!(h.interval);
                }
                entry["last"] = json!(live.ring.back());
                entry["summary"] = json!(live.summary);
                entry["spark"] = json!(spark(live.ring.iter().copied(), 90));
                entry["events"] = json!(live.events.iter().rev().take(8).collect::<Vec<_>>());
            }
        }
        out.push(entry);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_hours")]
    hours: u32,
}

fn default_hours() -> u32 {
    24
}

pub async fn node_history(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(node): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let allowed = if node == "home" { user.can_system() } else { user.can_node(&node) };
    user.require(allowed)?;
    let fleet = fleet_of(&state)?;
    let ch = fleet.clickhouse.clone().ok_or(AppError::NotFound)?;
    let hours = q.hours.clamp(1, 24 * 90);
    let data = ch
        .history(&node, hours)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("history: {e}")))?;
    Ok(Json(json!({ "hours": hours, "points": data })))
}

pub async fn proxy_http(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((node, rest)): Path<(String, String)>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !user.can_node(&node) {
        return AppError::Forbidden.into_response();
    }
    let Ok(fleet) = fleet_of(&state) else {
        return AppError::NotFound.into_response();
    };
    let ip = client_ip(&headers, &peer);
    let path = match uri.query() {
        Some(q) => format!("/{rest}?{q}"),
        None => format!("/{rest}"),
    };
    let mut fwd: Vec<(String, String)> = FORWARD_REQ
        .iter()
        .filter_map(|h| headers.get(*h).and_then(|v| v.to_str().ok()).map(|v| (h.to_string(), v.to_string())))
        .collect();
    fwd.extend(actor_headers(&user, &ip));

    let mutation = method != Method::GET && method != Method::HEAD;
    let method_s = method.to_string();
    let path_s = path.clone();
    let result = fleet
        .request(&node, |id| MasterMsg::HttpRequest {
            id,
            method: method_s,
            path: path_s,
            headers: fwd,
            body: B64.encode(&body),
        })
        .await;
    let reply = match result {
        Ok(r) => r,
        Err(resp) => {
            if mutation {
                audit(&state.db, &ip, &user.username, "node.request", &format!("{node} {method} {path}"), false);
            }
            return resp;
        }
    };
    if mutation {
        audit(
            &state.db,
            &ip,
            &user.username,
            "node.request",
            &format!("{node} {method} {path} → {}", reply.status),
            reply.status < 400,
        );
    }
    let mut resp = Response::new(axum::body::Body::from(reply.body));
    *resp.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::BAD_GATEWAY);
    for (k, v) in reply.headers {
        if FORWARD_RESP.contains(&k.as_str()) {
            if let (Ok(k), Ok(v)) = (HeaderName::try_from(k), HeaderValue::try_from(v)) {
                resp.headers_mut().insert(k, v);
            }
        }
    }
    resp
}

pub async fn proxy_ws(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path((node, rest)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    check_ws_origin(&headers, &state)?;
    user.require(user.can_node(&node))?;
    let fleet = fleet_of(&state)?;
    let ip = client_ip(&headers, &peer);
    let path = match uri.query() {
        Some(q) => format!("/ws/{rest}?{q}"),
        None => format!("/ws/{rest}"),
    };
    let fwd = actor_headers(&user, &ip);
    let terminal = rest == "terminal";
    Ok(ws.on_upgrade(move |socket| relay_ws(state, fleet, node, path, fwd, user, terminal, socket)))
}

#[allow(clippy::too_many_arguments)]
async fn relay_ws(
    state: AppState,
    fleet: Arc<Fleet>,
    node: String,
    path: String,
    headers: Vec<(String, String)>,
    user: CurrentUser,
    terminal: bool,
    mut socket: WebSocket,
) {
    let close = |reason: &str| {
        Message::Close(Some(CloseFrame {
            code: 1011,
            reason: reason.to_string().into(),
        }))
    };
    let Some((id, to_agent, mut from_agent)) = fleet.open_stream(&node) else {
        let _ = socket.send(close("node offline")).await;
        return;
    };
    let open = serde_json::to_string(&MasterMsg::WsOpen { id, path, headers }).expect("serialize");
    if to_agent.send(open).await.is_err() {
        let _ = socket.send(close("node offline")).await;
        return;
    }
    match tokio::time::timeout(STREAM_OPEN_TIMEOUT, from_agent.recv()).await {
        Ok(Some(StreamMsg::Opened(101))) => {}
        Ok(Some(StreamMsg::Opened(status))) => {
            let _ = socket.send(close(&format!("refused by node ({status})"))).await;
            fleet.close_stream(&node, id);
            return;
        }
        _ => {
            let _ = socket.send(close("node did not open the stream")).await;
            fleet.close_stream(&node, id);
            return;
        }
    }

    // The agent trusts the master's word on who the user is, so the master
    // re-checks long-lived streams: a deleted or demoted user loses them.
    let mut recheck = tokio::time::interval(Duration::from_secs(30));
    recheck.tick().await;
    loop {
        tokio::select! {
            msg = from_agent.recv() => match msg {
                Some(StreamMsg::Data(text, false)) => {
                    if socket.send(Message::Text(text.into())).await.is_err() { break; }
                }
                Some(StreamMsg::Data(text, true)) => {
                    let data = B64.decode(text).unwrap_or_default();
                    if socket.send(Message::Binary(data.into())).await.is_err() { break; }
                }
                Some(StreamMsg::Opened(_)) => {}
                Some(StreamMsg::Closed) | None => {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            },
            msg = socket.recv() => {
                let out = match msg {
                    Some(Ok(Message::Text(t))) => MasterMsg::WsData { id, text: t.to_string(), binary: false },
                    Some(Ok(Message::Binary(b))) => MasterMsg::WsData { id, text: B64.encode(&b), binary: true },
                    Some(Ok(_)) => continue,
                    _ => break,
                };
                if to_agent.send(serde_json::to_string(&out).expect("serialize")).await.is_err() {
                    break;
                }
            },
            _ = recheck.tick() => {
                if !still_allowed(&state, &user.username, &node, terminal).await {
                    let _ = socket.send(close("access revoked")).await;
                    break;
                }
            }
        }
    }
    fleet.close_stream(&node, id);
}

async fn still_allowed(state: &AppState, username: &str, node: &str, terminal: bool) -> bool {
    match crate::auth::fetch_user(&state.db, username).await {
        Ok(Some(u)) if u.totp_confirmed && !u.must_change_password => {
            let cu = u.to_current();
            cu.can_node(node) && (!terminal || cu.is_admin())
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Admin: enrollment tokens and node management
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct TokenReq {
    name: String,
    env: String,
    color: String,
}

fn valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name != "home"
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
}

fn valid_env(env: &str) -> bool {
    !env.is_empty() && env.len() <= 16 && env.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_color(color: &str) -> bool {
    color.len() == 7 && color.starts_with('#') && color[1..].chars().all(|c| c.is_ascii_hexdigit())
}

pub async fn create_token(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<TokenReq>,
) -> AppResult<Json<serde_json::Value>> {
    fleet_of(&state)?;
    if !valid_node_name(&req.name) {
        return Err(AppError::BadRequest(
            "name: lowercase letters, digits and '-', up to 32 characters (not \"home\")".into(),
        ));
    }
    if !valid_env(&req.env) || !valid_color(&req.color) {
        return Err(AppError::BadRequest("env must be a short word and color #rrggbb".into()));
    }
    let token = random_secret();
    let token_hash = sha256_hex(&token);
    let expires_at = now_secs() + JOIN_TOKEN_TTL;
    let (name, env, color) = (req.name.clone(), req.env.clone(), req.color.clone());
    let now = now_secs();
    let taken: bool = state
        .db
        .call(move |c| {
            c.execute("DELETE FROM join_tokens WHERE expires_at < ?1", [now])?;
            let taken: i64 = c.query_row("SELECT COUNT(*) FROM nodes WHERE name = ?1", [&name], |r| r.get(0))?;
            if taken > 0 {
                return Ok(true);
            }
            // A node that hasn't connected yet: a new token replaces the old one.
            c.execute("DELETE FROM join_tokens WHERE name = ?1", [&name])?;
            c.execute(
                "INSERT INTO join_tokens (token_hash, name, env, color, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![token_hash, name, env, color, expires_at],
            )?;
            Ok(false)
        })
        .await?;
    if taken {
        return Err(AppError::Conflict("a node with that name is already enrolled (revoke it first)".into()));
    }
    let ip = client_ip(&headers, &peer);
    audit(&state.db, &ip, &user.username, "node.token", &req.name, true);
    Ok(Json(json!({ "token": token, "name": req.name, "expires_at": expires_at })))
}

#[derive(Deserialize)]
pub struct UpdateNodeReq {
    env: String,
    color: String,
}

pub async fn update_node(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<UpdateNodeReq>,
) -> AppResult<Json<serde_json::Value>> {
    if !valid_env(&req.env) || !valid_color(&req.color) {
        return Err(AppError::BadRequest("env must be a short word and color #rrggbb".into()));
    }
    let n = state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE nodes SET env = ?1, color = ?2 WHERE name = ?3",
                rusqlite::params![req.env, req.color, name],
            )
        })
        .await?;
    if n == 0 {
        return Err(AppError::NotFound);
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn delete_node(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let fleet = fleet_of(&state)?;
    let n2 = name.clone();
    let n = state
        .db
        .call(move |c| {
            c.execute("DELETE FROM join_tokens WHERE name = ?1", [&n2])?;
            c.execute("DELETE FROM nodes WHERE name = ?1", [&n2])
        })
        .await?;
    // Revocation is immediate: the live link goes with the credentials.
    fleet.kick(&name);
    let ip = client_ip(&headers, &peer);
    audit(&state.db, &ip, &user.username, "node.revoke", &name, n > 0);
    Ok(Json(json!({ "ok": n > 0 })))
}
