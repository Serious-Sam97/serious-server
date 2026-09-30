use std::collections::HashMap;
use std::net::SocketAddr;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::audit::audit;
use crate::auth::perms::CurrentUser;
use crate::auth::{check_ws_origin, client_ip};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

#[derive(Debug, Serialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub service: String,
    pub image: String,
    pub state: String,
    pub status: String,
}

fn project_filter(project: &str) -> HashMap<String, Vec<String>> {
    HashMap::from([(
        "label".to_string(),
        vec![format!("com.docker.compose.project={project}")],
    )])
}

pub async fn list_containers(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(project): Path<String>,
) -> AppResult<Json<Vec<ContainerInfo>>> {
    // The name goes straight into a Docker label filter, which would let an
    // unchecked user enumerate ANY compose project on the host.
    user.require(user.project(&project).view)?;
    let opts = bollard::query_parameters::ListContainersOptionsBuilder::default()
        .all(true)
        .filters(&project_filter(&project))
        .build();
    let containers = state
        .docker
        .list_containers(Some(opts))
        .await
        .map_err(|e| anyhow::anyhow!("docker: {e}"))?;

    let out = containers
        .into_iter()
        .map(|c| ContainerInfo {
            id: c.id.unwrap_or_default(),
            name: c
                .names
                .unwrap_or_default()
                .first()
                .map(|n| n.trim_start_matches('/').to_string())
                .unwrap_or_default(),
            service: c
                .labels
                .as_ref()
                .and_then(|l| l.get("com.docker.compose.service"))
                .cloned()
                .unwrap_or_default(),
            image: c.image.unwrap_or_default(),
            state: c
                .state
                .map(|s| format!("{s:?}").to_lowercase())
                .unwrap_or_default(),
            status: c.status.unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

/// A container may only be acted on if it belongs to a compose project —
/// keeps the API scoped to what the dashboard shows.
async fn container_project(state: &AppState, id: &str) -> AppResult<String> {
    let inspect = state
        .docker
        .inspect_container(
            id,
            None::<bollard::query_parameters::InspectContainerOptions>,
        )
        .await
        .map_err(|_| AppError::NotFound)?;
    inspect
        .config
        .and_then(|c| c.labels)
        .and_then(|l| l.get("com.docker.compose.project").cloned())
        .ok_or(AppError::Forbidden)
}

#[derive(Deserialize)]
pub struct ActionPath {
    id: String,
    action: String,
}

pub async fn container_action(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(ActionPath { id, action }): Path<ActionPath>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    // Permission is checked on the RESOLVED project label, never a
    // client-sent name.
    let project = container_project(&state, &id).await?;
    user.require(user.project(&project).control)?;

    let result = match action.as_str() {
        "start" => state
            .docker
            .start_container(&id, None::<bollard::query_parameters::StartContainerOptions>)
            .await,
        "stop" => state
            .docker
            .stop_container(&id, None::<bollard::query_parameters::StopContainerOptions>)
            .await,
        "restart" => {
            state
                .docker
                .restart_container(
                    &id,
                    None::<bollard::query_parameters::RestartContainerOptions>,
                )
                .await
        }
        _ => return Err(AppError::BadRequest("unknown action".into())),
    };

    let detail = format!("{project}/{id} {action}");
    match result {
        Ok(()) => {
            audit(&state.db, &ip, &user.username, "container.action", &detail, true);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(e) => {
            audit(&state.db, &ip, &user.username, "container.action", &detail, false);
            Err(anyhow::anyhow!("docker {action}: {e}").into())
        }
    }
}

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_tail")]
    tail: u32,
}

fn default_tail() -> u32 {
    200
}

pub async fn logs_ws(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<LogsQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    check_ws_origin(&headers, &state)?;
    let project = container_project(&state, &id).await?;
    user.require(user.project(&project).logs)?;
    Ok(ws.on_upgrade(move |socket| stream_logs(socket, state, id, project, user.username, q.tail)))
}

async fn stream_logs(
    mut socket: WebSocket,
    state: AppState,
    id: String,
    project: String,
    username: String,
    tail: u32,
) {
    let opts = bollard::query_parameters::LogsOptionsBuilder::default()
        .follow(true)
        .stdout(true)
        .stderr(true)
        .timestamps(true)
        .tail(&tail.to_string())
        .build();
    let mut stream = state.docker.logs(&id, Some(opts));
    // Long-lived socket: re-check the permission periodically so a
    // revoked/deleted user doesn't keep an open stream.
    let mut recheck = tokio::time::interval(std::time::Duration::from_secs(30));
    recheck.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(log)) => {
                    let bytes = log.into_bytes();
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        return;
                    }
                }
                Some(Err(e)) => {
                    let _ = socket
                        .send(Message::Text(format!("\n[log stream error: {e}]\n").into()))
                        .await;
                    return;
                }
                None => {
                    let _ = socket
                        .send(Message::Text("\n[log stream ended]\n".to_string().into()))
                        .await;
                    return;
                }
            },
            msg = socket.recv() => {
                // Any close/error from the client tears the stream down.
                if !matches!(msg, Some(Ok(_))) {
                    return;
                }
            },
            _ = recheck.tick() => {
                if !still_permits_logs(&state, &username, &project).await {
                    let _ = socket
                        .send(Message::Text("\n[access revoked]\n".to_string().into()))
                        .await;
                    return;
                }
            }
        }
    }
}

async fn still_permits_logs(state: &AppState, username: &str, project: &str) -> bool {
    match crate::auth::fetch_user(&state.db, username).await {
        Ok(Some(u)) if u.totp_confirmed && !u.must_change_password => {
            u.to_current().project(project).logs
        }
        _ => false,
    }
}
