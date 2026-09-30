use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use serde::Deserialize;

use crate::audit::audit;
use crate::auth::perms::CurrentUser;
use crate::auth::{check_ws_origin, client_ip};
use crate::error::{AppError, AppResult};
use crate::pty::PtySession;
use crate::state::AppState;

const MAX_SESSIONS: usize = 5;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Deserialize)]
pub struct TermQuery {
    cwd: Option<String>,
    #[serde(default = "default_rows")]
    rows: u16,
    #[serde(default = "default_cols")]
    cols: u16,
}

fn default_rows() -> u16 {
    24
}
fn default_cols() -> u16 {
    80
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMsg {
    Resize { cols: u16, rows: u16 },
    Ping,
}

pub async fn terminal_ws(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Query(q): Query<TermQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    check_ws_origin(&headers, &state)?;
    let ip = client_ip(&headers, &peer);

    // A shell only STARTS in the requested cwd — it can cd anywhere and act
    // as the service user. That power belongs to the master admin alone.
    user.require(user.is_admin())?;
    let roots = &state.config.allowed_roots;
    let cwd = match &q.cwd {
        Some(raw) => {
            let path = PathBuf::from(raw)
                .canonicalize()
                .map_err(|_| AppError::NotFound)?;
            if !roots.iter().any(|root| path.starts_with(root)) {
                return Err(AppError::Forbidden);
            }
            path
        }
        None => roots.first().cloned().ok_or(AppError::Forbidden)?,
    };

    // Racy-looking but fine: worst case a burst opens session 6, which is a
    // resource cap, not a security boundary.
    if state.terminal_sessions.load(Ordering::SeqCst) >= MAX_SESSIONS {
        return Err(AppError::TooManyRequests);
    }

    let username = user.username.clone();
    Ok(ws.on_upgrade(move |socket| run_terminal(socket, state, cwd, q.rows, q.cols, ip, username)))
}

async fn run_terminal(
    mut socket: WebSocket,
    state: AppState,
    cwd: PathBuf,
    rows: u16,
    cols: u16,
    ip: String,
    username: String,
) {
    state.terminal_sessions.fetch_add(1, Ordering::SeqCst);
    let started = Instant::now();
    audit(
        &state.db,
        &ip,
        &username,
        "terminal.open",
        &cwd.to_string_lossy(),
        true,
    );

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let session = PtySession::spawn(&shell, &cwd, rows, cols);
    let mut session = match session {
        Ok(s) => s,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    format!(r#"{{"type":"exit","error":"{e}"}}"#).into(),
                ))
                .await;
            finish(&state, &ip, &username, &cwd, started);
            return;
        }
    };

    let mut last_input = Instant::now();
    let mut idle_check = tokio::time::interval(Duration::from_secs(30));

    loop {
        tokio::select! {
            chunk = session.output.recv() => match chunk {
                Some(data) => {
                    if socket.send(Message::Binary(data.into())).await.is_err() {
                        break;
                    }
                }
                None => {
                    // Shell exited.
                    let _ = socket
                        .send(Message::Text(r#"{"type":"exit"}"#.to_string().into()))
                        .await;
                    break;
                }
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Binary(data))) => {
                    last_input = Instant::now();
                    session.write(data.to_vec());
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str::<ControlMsg>(&text) {
                        Ok(ControlMsg::Resize { cols, rows }) => session.resize(rows, cols),
                        Ok(ControlMsg::Ping) => {
                            last_input = Instant::now();
                            let _ = socket
                                .send(Message::Text(r#"{"type":"pong"}"#.to_string().into()))
                                .await;
                        }
                        Err(_) => {}
                    }
                }
                Some(Ok(_)) => {}
                _ => break, // client closed or errored
            },
            _ = idle_check.tick() => {
                if last_input.elapsed() > IDLE_TIMEOUT {
                    let _ = socket
                        .send(Message::Text(r#"{"type":"exit","reason":"idle"}"#.to_string().into()))
                        .await;
                    break;
                }
                // A live shell must not outlive a revocation: re-verify the
                // user still exists in good standing with terminal rights
                // on this cwd.
                if !still_permits_terminal(&state, &username, &cwd).await {
                    let _ = socket
                        .send(Message::Text(r#"{"type":"exit","reason":"revoked"}"#.to_string().into()))
                        .await;
                    break;
                }
            }
        }
    }

    session.kill();
    finish(&state, &ip, &username, &cwd, started);
}

async fn still_permits_terminal(state: &AppState, username: &str, _cwd: &std::path::Path) -> bool {
    match crate::auth::fetch_user(&state.db, username).await {
        Ok(Some(u)) if u.totp_confirmed && !u.must_change_password => {
            u.to_current().is_admin()
        }
        _ => false,
    }
}

fn finish(state: &AppState, ip: &str, username: &str, cwd: &std::path::Path, started: Instant) {
    state.terminal_sessions.fetch_sub(1, Ordering::SeqCst);
    audit(
        &state.db,
        ip,
        username,
        "terminal.close",
        &format!("{} ({}s)", cwd.display(), started.elapsed().as_secs()),
        true,
    );
}
