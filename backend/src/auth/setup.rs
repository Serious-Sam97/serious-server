use std::net::SocketAddr;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::audit;
use crate::auth::{client_ip, password, totp};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Setup is "done" when an ADMIN is enrolled — the presence of confirmed
/// normal users must never re-open (or be confused with) first-run setup.
async fn admin_exists(state: &AppState) -> anyhow::Result<bool> {
    let n: i64 = state
        .db
        .call(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND totp_confirmed = 1",
                [],
                |r| r.get(0),
            )
        })
        .await?;
    Ok(n > 0)
}

pub async fn status(State(state): State<AppState>) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "needs_setup": !admin_exists(&state).await? })))
}

#[derive(Deserialize)]
pub struct SetupReq {
    token: String,
    username: String,
    password: String,
}

/// Create the admin account. Requires the one-time token printed to the
/// journal at first boot, so a stranger who finds the URL during the setup
/// window still can't enroll.
pub async fn begin(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<SetupReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    if admin_exists(&state).await? {
        return Err(AppError::Gone);
    }
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let token_ok = state
        .setup_token
        .lock()
        .unwrap()
        .as_deref()
        .is_some_and(|t| t == req.token.trim());
    if !token_ok {
        state.login_guard.record_failure();
        audit(&state.db, &ip, "?", "setup.begin", "bad token", false);
        return Err(AppError::Forbidden);
    }

    let username = req.username.trim().to_string();
    if username.is_empty() || req.password.len() < 12 {
        return Err(AppError::BadRequest(
            "username required and password must be at least 12 characters".into(),
        ));
    }

    let hash = {
        let pw = req.password.clone();
        tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(anyhow::Error::from)??
    };
    let secret = totp::generate_secret();
    let otpauth_uri = totp::build(&secret, &username)?.get_url();

    let (u, h, s) = (username.clone(), hash, secret);
    state
        .db
        .call(move |c| {
            // Restarting setup replaces any half-enrolled account.
            c.execute("DELETE FROM users WHERE totp_confirmed = 0 AND role = 'admin'", [])?;
            c.execute(
                "INSERT INTO users (username, password_hash, totp_secret, role) VALUES (?1, ?2, ?3, 'admin')",
                rusqlite::params![u, h, s],
            )
        })
        .await?;

    audit(&state.db, &ip, &username, "setup.begin", "", true);
    Ok(Json(json!({ "otpauth_uri": otpauth_uri })).into_response())
}

#[derive(Deserialize)]
pub struct ConfirmReq {
    code: String,
}

/// Activate the account only once the user proves their authenticator app
/// actually produces valid codes.
pub async fn confirm(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<ConfirmReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    if admin_exists(&state).await? {
        return Err(AppError::Gone);
    }

    let pending = state
        .db
        .call(|c| {
            c.query_row(
                "SELECT username, totp_secret FROM users WHERE totp_confirmed = 0 AND role = 'admin'",
                [],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await?;
    let Some((username, secret)) = pending else {
        return Err(AppError::BadRequest("setup not started".into()));
    };

    let totp = totp::build(&secret, &username)?;
    if !totp::check_now(&totp, req.code.trim()) {
        audit(&state.db, &ip, &username, "setup.confirm", "", false);
        return Err(AppError::Unauthorized);
    }

    let step = totp::current_step();
    let u = username.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET totp_confirmed = 1, totp_last_step = ?1 WHERE username = ?2",
                rusqlite::params![step, u],
            )
        })
        .await?;
    state.setup_token.lock().unwrap().take();

    audit(&state.db, &ip, &username, "setup.confirm", "", true);
    tracing::info!("admin account '{username}' enrolled; setup closed");
    Ok(StatusCode::NO_CONTENT.into_response())
}
