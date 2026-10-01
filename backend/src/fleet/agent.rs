//! Agent side of the fleet link: dial the master, enroll on first boot,
//! stream metrics and docker events, and execute tunneled API requests with
//! the same handlers the master runs locally.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use rand::RngExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tower::ServiceExt;

use super::{Actor, AgentMsg, MasterMsg, Summary, ACTOR_HEADER, PROTO};
use crate::auth::perms::{CurrentUser, Role};
use crate::backups::agent::{BackupAgent, LinkTx};
use crate::state::AppState;

const PING_EVERY: Duration = Duration::from_secs(30);
/// No frame at all from the master for this long = dead link (pongs count).
const SILENCE_LIMIT: Duration = Duration::from_secs(90);
const MAX_BACKOFF_SECS: u64 = 60;
/// Metrics points per message when replaying after a reconnect.
const REPLAY_CHUNK: usize = 200;

#[derive(Serialize, Deserialize, Clone)]
struct Creds {
    node: String,
    secret: String,
}

fn creds_path(state: &AppState) -> PathBuf {
    state.config.data_dir.join("node.json")
}

fn load_creds(state: &AppState) -> Option<Creds> {
    let text = std::fs::read_to_string(creds_path(state)).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_creds(state: &AppState, creds: &Creds) -> anyhow::Result<()> {
    let path = creds_path(state);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(creds)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Keep the link up forever: reconnect with exponential backoff + jitter.
pub fn spawn(state: AppState) {
    let Some(master) = state.config.master_url.clone() else {
        tracing::warn!("agent mode without SS_MASTER_URL — not connecting to any master");
        return;
    };
    let url = match ws_url(&master) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("SS_MASTER_URL: {e}");
            return;
        }
    };
    let router = tunnel_router(state.clone());
    let backups = crate::backups::agent::BackupAgent::spawn(state.clone());
    tokio::spawn(async move {
        let mut backoff = 1u64;
        loop {
            let started = Instant::now();
            match session(&state, &router, &backups, &url).await {
                Ok(()) => tracing::info!("fleet: link closed by master"),
                Err(e) => tracing::warn!("fleet: {e:#}"),
            }
            if started.elapsed() > Duration::from_secs(60) {
                backoff = 1; // it was a healthy link; retry fast
            }
            let jitter = rand::rng().random_range(0..1000);
            tokio::time::sleep(Duration::from_millis(backoff * 1000 + jitter)).await;
            backoff = (backoff * 2).min(MAX_BACKOFF_SECS);
        }
    });
}

/// `https://fleet.example.com` → `wss://fleet.example.com/fleet/connect`.
fn ws_url(master: &str) -> anyhow::Result<String> {
    let master = master.trim_end_matches('/');
    let url = if let Some(rest) = master.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = master.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if master.starts_with("wss://") || master.starts_with("ws://") {
        master.to_string()
    } else {
        anyhow::bail!("must start with https:// (or ws:// for local tests), got {master}");
    };
    Ok(format!("{url}/fleet/connect"))
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

type Link = tokio_tungstenite::WebSocketStream<Box<dyn Io>>;

async fn dial(url: &str, headers: &[(&str, String)]) -> anyhow::Result<Link> {
    let mut req = url.into_client_request()?;
    for (k, v) in headers {
        req.headers_mut().insert(
            tungstenite::http::HeaderName::from_bytes(k.as_bytes())?,
            HeaderValue::from_str(v)?,
        );
    }
    let uri = req.uri().clone();
    let tls = uri.scheme_str() == Some("wss");
    let host = uri.host().ok_or_else(|| anyhow::anyhow!("no host in {url}"))?.to_string();
    let port = uri.port_u16().unwrap_or(if tls { 443 } else { 80 });

    let tcp = tokio::time::timeout(Duration::from_secs(15), TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| anyhow::anyhow!("connect to {host}:{port} timed out"))??;
    tcp.set_nodelay(true)?;
    let io: Box<dyn Io> = if tls {
        Box::new(super::net::tls(&host, tcp).await?)
    } else {
        Box::new(tcp)
    };
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(32 << 20);
    match tokio_tungstenite::client_async_with_config(req, io, Some(config)).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(resp)) if resp.status() == StatusCode::UNAUTHORIZED => {
            anyhow::bail!("master rejected our credentials (node revoked, or join token used/expired)")
        }
        Err(tungstenite::Error::Http(resp)) => anyhow::bail!("master answered {}", resp.status()),
        Err(e) => Err(e.into()),
    }
}

fn text(msg: &AgentMsg) -> Message {
    Message::Text(serde_json::to_string(msg).expect("serialize").into())
}

fn hostname() -> String {
    sysinfo::System::host_name().unwrap_or_else(|| "unknown".into())
}

/// Streams opened by the master, keyed by stream id.
type Streams = Arc<Mutex<HashMap<u64, mpsc::Sender<Message>>>>;

async fn session(
    state: &AppState,
    router: &Router,
    backups: &Arc<BackupAgent>,
    url: &str,
) -> anyhow::Result<()> {
    let creds = load_creds(state);
    let mut headers: Vec<(&str, String)> = vec![(
        "user-agent",
        format!("serious-server-agent/{}", env!("CARGO_PKG_VERSION")),
    )];
    match (&creds, &state.config.join_token) {
        (Some(c), _) => headers.push(("authorization", format!("Bearer {}", c.secret))),
        (None, Some(token)) => headers.push(("x-ss-join", token.clone())),
        (None, None) => anyhow::bail!("not enrolled yet: set SS_JOIN_TOKEN (create one on the master's Fleet page)"),
    }
    if let Some((id, secret)) = &state.config.cf_access {
        headers.push(("cf-access-client-id", id.clone()));
        headers.push(("cf-access-client-secret", secret.clone()));
    }

    let link = dial(url, &headers).await?;
    let (mut sink, mut stream) = link.split();
    sink.send(text(&AgentMsg::Hello {
        proto: PROTO,
        version: env!("CARGO_PKG_VERSION").into(),
        hostname: hostname(),
        interval: state.config.sample_interval.as_secs(),
        allow: state.config.agent_allow.clone(),
    }))
    .await?;

    // Handshake: optional credentials (first boot), then welcome.
    let (node, last_ts) = loop {
        let msg = tokio::time::timeout(Duration::from_secs(15), stream.next())
            .await
            .map_err(|_| anyhow::anyhow!("master did not welcome us"))?
            .ok_or_else(|| anyhow::anyhow!("master closed during handshake"))??;
        let Message::Text(t) = msg else { continue };
        match serde_json::from_str::<MasterMsg>(&t)? {
            MasterMsg::Enrolled { node, secret } => {
                save_creds(state, &Creds { node: node.clone(), secret })?;
                tracing::info!(node, "fleet: enrolled — credentials saved, SS_JOIN_TOKEN is no longer needed");
            }
            MasterMsg::Welcome { node, last_ts } => break (node, last_ts),
            MasterMsg::Error { message } => anyhow::bail!("master refused: {message}"),
            _ => {}
        }
    };
    tracing::info!(node, "fleet: connected to master");

    let (tx, mut rx) = mpsc::channel::<Message>(1024);
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(4);
    let writer = tokio::spawn(async move {
        loop {
            let msg = tokio::select! {
                biased; // control and metrics before bulk transfer chunks
                m = rx.recv() => match m {
                    Some(m) => m,
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
    backups.link_up(LinkTx { ctrl: tx.clone(), bulk: bulk_tx.clone() });
    drop(bulk_tx);
    let result = pump(state, router, backups, &mut stream, &tx, last_ts).await;
    backups.link_down();
    drop(tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
    result
}

async fn pump(
    state: &AppState,
    router: &Router,
    backups: &Arc<BackupAgent>,
    stream: &mut futures_util::stream::SplitStream<Link>,
    tx: &mpsc::Sender<Message>,
    last_ts: f64,
) -> anyhow::Result<()> {
    // Replay what the master missed while the link was down.
    let missed: Vec<_> = state
        .sampler
        .history
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p[0] > last_ts)
        .copied()
        .collect();
    for chunk in missed.chunks(REPLAY_CHUNK) {
        tx.send(text(&AgentMsg::Metrics {
            points: chunk.to_vec(),
            summary: None,
        }))
        .await?;
    }

    let streams: Streams = Arc::new(Mutex::new(HashMap::new()));
    let mut samples = state.sampler.rx.clone();
    samples.mark_unchanged();
    let mut events = docker_events(state);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut last_rx = Instant::now();

    loop {
        tokio::select! {
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                };
                last_rx = Instant::now();
                let t = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => return Ok(()),
                    Message::Binary(b) => {
                        if let Some((super::FRAME_RESTORE, rid, offset, payload)) = super::parse_frame(&b) {
                            backups.restore_chunk(rid, offset, payload);
                        }
                        continue;
                    }
                    _ => continue,
                };
                let Ok(msg) = serde_json::from_str::<MasterMsg>(&t) else { continue };
                match msg {
                    MasterMsg::HttpRequest { id, method, path, headers, body } => {
                        let (router, tx) = (router.clone(), tx.clone());
                        tokio::spawn(async move {
                            let reply = handle_http(router, id, method, path, headers, body).await;
                            let _ = tx.send(text(&reply)).await;
                        });
                    }
                    MasterMsg::WsOpen { id, path, headers } => {
                        let (router, tx, streams) = (router.clone(), tx.clone(), streams.clone());
                        tokio::spawn(handle_ws(router, tx, streams, id, path, headers));
                    }
                    MasterMsg::WsData { id, text, binary } => {
                        let target = streams.lock().unwrap().get(&id).cloned();
                        if let Some(target) = target {
                            let frame = if binary {
                                Message::Binary(B64.decode(text).unwrap_or_default().into())
                            } else {
                                Message::Text(text.into())
                            };
                            // Awaiting here would let one slow local stream stall the link.
                            let _ = target.try_send(frame);
                        }
                    }
                    MasterMsg::WsClose { id } => {
                        streams.lock().unwrap().remove(&id);
                    }
                    MasterMsg::Error { message } => anyhow::bail!("master: {message}"),
                    msg @ (MasterMsg::Policies { .. }
                    | MasterMsg::RunBackup { .. }
                    | MasterMsg::UploadAck { .. }
                    | MasterMsg::UploadDone { .. }
                    | MasterMsg::RestoreStart { .. }) => backups.handle(msg).await,
                    MasterMsg::Enrolled { .. } | MasterMsg::Welcome { .. } => {}
                }
            }
            changed = samples.changed() => {
                if changed.is_err() {
                    anyhow::bail!("sampler stopped");
                }
                let payload = samples.borrow_and_update().clone();
                let point = state.sampler.history.lock().unwrap().back().copied();
                if let Some(point) = point {
                    tx.send(text(&AgentMsg::Metrics {
                        points: vec![point],
                        summary: Summary::from_payload(&payload),
                    }))
                    .await?;
                }
            }
            Some(event) = next_event(&mut events) => {
                tx.send(text(&event)).await?;
            }
            _ = ping.tick() => {
                if last_rx.elapsed() > SILENCE_LIMIT {
                    anyhow::bail!("master silent for {}s", SILENCE_LIMIT.as_secs());
                }
                tx.send(Message::Ping(Vec::new().into())).await?;
            }
        }
    }
}

type EventStream = futures_util::stream::BoxStream<'static, Result<bollard::models::EventMessage, bollard::errors::Error>>;

fn docker_events(state: &AppState) -> Option<EventStream> {
    let filters: HashMap<String, Vec<String>> = HashMap::from([
        ("type".to_string(), vec!["container".to_string()]),
        (
            "event".to_string(),
            ["start", "stop", "die", "restart", "oom", "health_status"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
    ]);
    let opts = bollard::query_parameters::EventsOptionsBuilder::default()
        .filters(&filters)
        .build();
    Some(state.docker.events(Some(opts)).boxed())
}

/// Next docker event as a fleet message; a broken event stream goes quiet
/// for the rest of this session instead of spinning.
async fn next_event(events: &mut Option<EventStream>) -> Option<AgentMsg> {
    loop {
        let stream = match events.as_mut() {
            Some(s) => s,
            None => return std::future::pending().await,
        };
        match stream.next().await {
            Some(Ok(ev)) => {
                let attrs = ev.actor.and_then(|a| a.attributes).unwrap_or_default();
                return Some(AgentMsg::Event {
                    kind: "docker".into(),
                    detail: json!({
                        "action": ev.action,
                        "name": attrs.get("name"),
                        "project": attrs.get("com.docker.compose.project"),
                        "service": attrs.get("com.docker.compose.service"),
                    }),
                });
            }
            Some(Err(e)) => {
                tracing::warn!("docker events: {e}");
                *events = None;
            }
            None => *events = None,
        }
    }
}

// ---------------------------------------------------------------------------
// API tunnel
// ---------------------------------------------------------------------------

/// The node-local API as the master's users see it: same handlers, with the
/// acting user taken from the master's actor header and every route gated
/// by this agent's own allowlist (SS_AGENT_ALLOW).
pub fn tunnel_router(state: AppState) -> Router {
    crate::node_api()
        .route("/audit", get(crate::api::audit_log))
        .layer(middleware::from_fn_with_state(state.clone(), tunnel_auth))
        .with_state(state)
}

/// Which SS_AGENT_ALLOW capability a path needs.
fn capability(path: &str) -> Option<&'static str> {
    let p = path;
    if p.starts_with("/system") || p.starts_with("/ws/system") || p.starts_with("/audit") {
        Some("system")
    } else if p.starts_with("/ws/containers/") {
        Some("logs")
    } else if p.starts_with("/projects") || p.starts_with("/compose") || p.starts_with("/jobs") || p.starts_with("/containers") {
        Some("projects")
    } else if p.starts_with("/git") {
        Some("git")
    } else if p.starts_with("/files") {
        Some("files")
    } else if p.starts_with("/ws/terminal") {
        Some("terminal")
    } else if p.starts_with("/backups") {
        Some("backups")
    } else {
        None
    }
}

async fn tunnel_auth(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let actor = req
        .headers()
        .get(ACTOR_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| B64.decode(v).ok())
        .and_then(|b| serde_json::from_slice::<Actor>(&b).ok());
    let Some(actor) = actor else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let path = req.uri().path();
    let Some(cap) = capability(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !state.config.agent_allow.iter().any(|c| c == cap) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "error": format!("'{cap}' is not allowed on this node (SS_AGENT_ALLOW)") })),
        )
            .into_response();
    }
    if path.starts_with("/audit") && !actor.admin {
        return StatusCode::FORBIDDEN.into_response();
    }
    req.extensions_mut().insert(CurrentUser {
        username: actor.username,
        role: if actor.admin { Role::Admin } else { Role::User },
        perms: actor.perms,
    });
    // Handlers log the client IP from cf-connecting-ip (set by the master);
    // the socket address itself is meaningless here.
    req.extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 0))));
    next.run(req).await
}

async fn handle_http(
    router: Router,
    id: u64,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
) -> AgentMsg {
    let fail = |status: u16, msg: &str| AgentMsg::HttpResponse {
        id,
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: B64.encode(json!({ "error": msg }).to_string()),
    };
    let mut builder = axum::http::Request::builder().method(method.as_str()).uri(&path);
    for (k, v) in &headers {
        builder = builder.header(k, v);
    }
    let body = B64.decode(body).unwrap_or_default();
    let Ok(req) = builder.body(Body::from(body)) else {
        return fail(400, "malformed tunneled request");
    };
    let resp = match router.oneshot(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
        .collect();
    match resp.into_body().collect().await {
        Ok(b) => AgentMsg::HttpResponse {
            id,
            status,
            headers,
            body: B64.encode(b.to_bytes()),
        },
        Err(_) => fail(502, "response body failed"),
    }
}

/// Run a tunneled WebSocket against the local router: serve the router on
/// one end of an in-memory pipe (hyper, with upgrades) and do a normal
/// client handshake on the other, so the existing WebSocket handlers run
/// unchanged.
async fn handle_ws(
    router: Router,
    tx: mpsc::Sender<Message>,
    streams: Streams,
    id: u64,
    path: String,
    headers: Vec<(String, String)>,
) {
    let (client_io, server_io) = tokio::io::duplex(64 * 1024);
    let service = hyper_util::service::TowerToHyperService::new(router);
    tokio::spawn(async move {
        let _ = hyper::server::conn::http1::Builder::new()
            .serve_connection(hyper_util::rt::TokioIo::new(server_io), service)
            .with_upgrades()
            .await;
    });

    let opened = async {
        let mut req = format!("ws://agent{path}").into_client_request()?;
        for (k, v) in &headers {
            req.headers_mut().insert(
                tungstenite::http::HeaderName::try_from(k.as_str())?,
                HeaderValue::from_str(v)?,
            );
        }
        // Same-host origin: passes check_ws_origin like a local browser would.
        req.headers_mut().insert("origin", HeaderValue::from_static("http://agent"));
        anyhow::Ok(tokio_tungstenite::client_async(req, client_io).await)
    }
    .await;

    let ws = match opened {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let _ = tx.send(text(&AgentMsg::WsOpened { id, status: resp.status().as_u16() })).await;
            let _ = tx.send(text(&AgentMsg::WsClosed { id })).await;
            return;
        }
        _ => {
            let _ = tx.send(text(&AgentMsg::WsOpened { id, status: 502 })).await;
            let _ = tx.send(text(&AgentMsg::WsClosed { id })).await;
            return;
        }
    };
    let (in_tx, mut in_rx) = mpsc::channel::<Message>(256);
    streams.lock().unwrap().insert(id, in_tx);
    if tx.send(text(&AgentMsg::WsOpened { id, status: 101 })).await.is_err() {
        streams.lock().unwrap().remove(&id);
        return;
    }

    let (mut local_sink, mut local_stream) = ws.split();
    loop {
        tokio::select! {
            msg = local_stream.next() => {
                let out = match msg {
                    Some(Ok(Message::Text(t))) => AgentMsg::WsData { id, text: t.to_string(), binary: false },
                    Some(Ok(Message::Binary(b))) => AgentMsg::WsData { id, text: B64.encode(&b), binary: true },
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                if tx.send(text(&out)).await.is_err() {
                    break;
                }
            }
            msg = in_rx.recv() => match msg {
                Some(frame) => {
                    if local_sink.send(frame).await.is_err() {
                        break;
                    }
                }
                None => break, // master closed the stream (or the link dropped)
            },
        }
    }
    let _ = local_sink.close().await;
    streams.lock().unwrap().remove(&id);
    let _ = tx.send(text(&AgentMsg::WsClosed { id })).await;
}
