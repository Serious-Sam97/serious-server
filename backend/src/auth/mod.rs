pub mod password;
pub mod perms;
pub mod setup;
pub mod totp;

use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use tower_sessions::Session;

use crate::audit::audit;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

use perms::{CurrentUser, Permissions, Role};

const KEY_STATE: &str = "auth_state";
const KEY_USERNAME: &str = "username";
const KEY_LOGIN_TIME: &str = "login_time";
const STATE_AWAITING_TOTP: &str = "awaiting_totp";
const STATE_AWAITING_PASSWORD_CHANGE: &str = "awaiting_password_change";
const STATE_AWAITING_TOTP_ENROLL: &str = "awaiting_totp_enroll";
const STATE_AUTHENTICATED: &str = "authenticated";
/// Absolute session lifetime; idle timeout is enforced by the session layer.
const MAX_SESSION_SECS: i64 = 7 * 24 * 3600;
const MIN_PASSWORD_LEN: usize = 12;

pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs() as i64
}

/// Real client IP. CF-Connecting-IP is trustworthy here because the listener
/// is loopback-only: nothing but cloudflared can set or omit it.
pub fn client_ip(headers: &HeaderMap, peer: &SocketAddr) -> String {
    headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| peer.ip().to_string())
}

#[derive(Debug, Clone)]
pub struct UserRow {
    pub username: String,
    pub password_hash: String,
    pub totp_secret: String,
    pub totp_confirmed: bool,
    pub totp_last_step: i64,
    pub role: String,
    pub must_change_password: bool,
    pub permissions: String,
}

fn row_to_user(r: &rusqlite::Row) -> rusqlite::Result<UserRow> {
    Ok(UserRow {
        username: r.get(0)?,
        password_hash: r.get(1)?,
        totp_secret: r.get(2)?,
        totp_confirmed: r.get(3)?,
        totp_last_step: r.get(4)?,
        role: r.get(5)?,
        must_change_password: r.get(6)?,
        permissions: r.get(7)?,
    })
}

const USER_COLS: &str = "username, password_hash, totp_secret, totp_confirmed, \
                         totp_last_step, role, must_change_password, permissions";

pub async fn fetch_user(db: &crate::db::Db, username: &str) -> anyhow::Result<Option<UserRow>> {
    let username = username.to_string();
    db.call(move |c| {
        c.query_row(
            &format!("SELECT {USER_COLS} FROM users WHERE username = ?1"),
            [&username],
            |r| row_to_user(r),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
    })
    .await
}

impl UserRow {
    pub fn to_current(&self) -> CurrentUser {
        CurrentUser {
            username: self.username.clone(),
            role: Role::from_db(&self.role),
            perms: Permissions::parse_or_deny(&self.permissions, &self.username),
        }
    }
}

/// Where the login flow goes after the current step succeeds. TOTP comes
/// before a forced password change when the user has a confirmed TOTP
/// (admin password-reset case): a stolen temp password alone must never be
/// able to take over an enrolled account.
fn next_after_password(user: &UserRow) -> &'static str {
    if user.totp_confirmed {
        STATE_AWAITING_TOTP
    } else if user.must_change_password {
        STATE_AWAITING_PASSWORD_CHANGE
    } else {
        STATE_AWAITING_TOTP_ENROLL
    }
}

fn state_to_client(state: &str) -> &'static str {
    match state {
        STATE_AWAITING_TOTP => "totp",
        STATE_AWAITING_PASSWORD_CHANGE => "change_password",
        STATE_AWAITING_TOTP_ENROLL => "totp_enroll",
        _ => "done",
    }
}

async fn set_authenticated(session: &Session) -> anyhow::Result<()> {
    // New session ID on every privilege elevation (fixation defense).
    session.cycle_id().await?;
    session.insert(KEY_STATE, STATE_AUTHENTICATED).await?;
    session.insert(KEY_LOGIN_TIME, now_secs()).await?;
    Ok(())
}

async fn session_state(session: &Session) -> anyhow::Result<(Option<String>, Option<String>)> {
    Ok((
        session.get(KEY_STATE).await?,
        session.get(KEY_USERNAME).await?,
    ))
}

#[derive(Deserialize)]
pub struct LoginReq {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
    Json(req): Json<LoginReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let user = fetch_user(&state.db, &req.username).await?;
    let verified = match &user {
        Some(u) => {
            let (pw, hash) = (req.password.clone(), u.password_hash.clone());
            tokio::task::spawn_blocking(move || password::verify(&pw, &hash))
                .await
                .map_err(anyhow::Error::from)?
        }
        None => {
            let pw = req.password.clone();
            tokio::task::spawn_blocking(move || password::dummy_verify(&pw))
                .await
                .map_err(anyhow::Error::from)?;
            false
        }
    };

    if !verified {
        state.login_guard.record_failure();
        audit(&state.db, &ip, &req.username, "auth.login", "", false);
        return Err(AppError::Unauthorized);
    }
    let user = user.expect("verified implies row");

    let next = next_after_password(&user);
    session
        .insert(KEY_STATE, next)
        .await
        .map_err(anyhow::Error::from)?;
    session
        .insert(KEY_USERNAME, user.username.clone())
        .await
        .map_err(anyhow::Error::from)?;
    audit(&state.db, &ip, &user.username, "auth.login", "", true);

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "next": state_to_client(next) })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct TotpReq {
    code: String,
}

pub async fn verify_totp(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
    Json(req): Json<TotpReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let (auth_state, username) = session_state(&session).await?;
    let (Some(auth_state), Some(username)) = (auth_state, username) else {
        return Err(AppError::Unauthorized);
    };
    if auth_state != STATE_AWAITING_TOTP {
        return Err(AppError::BadRequest("not awaiting totp".into()));
    }

    let user = fetch_user(&state.db, &username)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let totp = totp::build(&user.totp_secret, &user.username)?;
    let step = totp::current_step();
    let ok = totp::check_now(&totp, req.code.trim()) && step > user.totp_last_step;

    if !ok {
        state.login_guard.record_failure();
        audit(&state.db, &ip, &username, "auth.totp", "", false);
        return Err(AppError::Unauthorized);
    }

    let uname = username.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET totp_last_step = ?1 WHERE username = ?2",
                rusqlite::params![step, uname],
            )
        })
        .await?;

    state.login_guard.record_success();
    audit(&state.db, &ip, &username, "auth.totp", "", true);

    if user.must_change_password {
        session
            .insert(KEY_STATE, STATE_AWAITING_PASSWORD_CHANGE)
            .await
            .map_err(anyhow::Error::from)?;
        return Ok(Json(json!({ "next": "change_password" })).into_response());
    }
    set_authenticated(&session).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Deserialize)]
pub struct ChangePasswordReq {
    new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
    Json(req): Json<ChangePasswordReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let (auth_state, username) = session_state(&session).await?;
    let (Some(auth_state), Some(username)) = (auth_state, username) else {
        return Err(AppError::Unauthorized);
    };
    if auth_state != STATE_AWAITING_PASSWORD_CHANGE {
        return Err(AppError::BadRequest("not awaiting password change".into()));
    }
    if req.new_password.len() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }

    let user = fetch_user(&state.db, &username)
        .await?
        .ok_or(AppError::Unauthorized)?;

    let hash = {
        let pw = req.new_password.clone();
        tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(anyhow::Error::from)??
    };
    let uname = username.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET password_hash = ?1, must_change_password = 0 WHERE username = ?2",
                rusqlite::params![hash, uname],
            )
        })
        .await?;
    audit(&state.db, &ip, &username, "auth.change_password", "", true);

    if user.totp_confirmed {
        set_authenticated(&session).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    session
        .insert(KEY_STATE, STATE_AWAITING_TOTP_ENROLL)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(Json(json!({ "next": "totp_enroll" })).into_response())
}

/// Mid-login TOTP enrollment for admin-created users (and TOTP resets).
/// Unlike /setup this operates on the existing session row and never
/// creates accounts. Each call regenerates the secret — retry friendly,
/// and it means a TOTP reset invalidates the old secret for good.
pub async fn enroll_begin(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
) -> AppResult<Json<serde_json::Value>> {
    let ip = client_ip(&headers, &peer);
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let (auth_state, username) = session_state(&session).await?;
    let (Some(auth_state), Some(username)) = (auth_state, username) else {
        return Err(AppError::Unauthorized);
    };
    if auth_state != STATE_AWAITING_TOTP_ENROLL {
        return Err(AppError::BadRequest("not awaiting totp enrollment".into()));
    }

    let secret = totp::generate_secret();
    let uri = totp::build(&secret, &username)?.get_url();
    let uname = username.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET totp_secret = ?1, totp_confirmed = 0 WHERE username = ?2",
                rusqlite::params![secret, uname],
            )
        })
        .await?;
    audit(&state.db, &ip, &username, "auth.enroll", "begin", true);
    Ok(Json(json!({ "otpauth_uri": uri })))
}

pub async fn enroll_confirm(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
    Json(req): Json<TotpReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    state
        .login_guard
        .check(&ip)
        .map_err(|_| AppError::TooManyRequests)?;

    let (auth_state, username) = session_state(&session).await?;
    let (Some(auth_state), Some(username)) = (auth_state, username) else {
        return Err(AppError::Unauthorized);
    };
    if auth_state != STATE_AWAITING_TOTP_ENROLL {
        return Err(AppError::BadRequest("not awaiting totp enrollment".into()));
    }

    let user = fetch_user(&state.db, &username)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let totp = totp::build(&user.totp_secret, &user.username)?;
    if !totp::check_now(&totp, req.code.trim()) {
        state.login_guard.record_failure();
        audit(&state.db, &ip, &username, "auth.enroll", "confirm", false);
        return Err(AppError::Unauthorized);
    }

    let step = totp::current_step();
    let uname = username.clone();
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET totp_confirmed = 1, totp_last_step = ?1 WHERE username = ?2",
                rusqlite::params![step, uname],
            )
        })
        .await?;

    state.login_guard.record_success();
    audit(&state.db, &ip, &username, "auth.enroll", "confirm", true);
    set_authenticated(&session).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn logout(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    session: Session,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    let username: Option<String> = session
        .get(KEY_USERNAME)
        .await
        .map_err(anyhow::Error::from)?;
    session.flush().await.map_err(anyhow::Error::from)?;
    let actor = username.as_deref().unwrap_or("?");
    audit(&state.db, &ip, actor, "auth.logout", "", true);
    Ok(StatusCode::NO_CONTENT.into_response())
}

pub async fn session_info(
    State(state): State<AppState>,
    session: Session,
) -> AppResult<Json<serde_json::Value>> {
    match load_authenticated_user(&state, &session).await? {
        Some(user) => {
            let current = user.to_current();
            Ok(Json(json!({
                "authenticated": true,
                "username": current.username,
                "role": if current.is_admin() { "admin" } else { "user" },
                "permissions": current.perms,
            })))
        }
        None => Ok(Json(json!({
            "authenticated": false,
            "username": null,
            "role": null,
            "permissions": Permissions::default(),
        }))),
    }
}

/// The full auth check: session must be authenticated AND the user row must
/// still exist in good standing. Loaded from the DB on every request — this
/// is the revocation mechanism (deletes, password/TOTP resets, and
/// permission edits all bite on the target's next request).
async fn load_authenticated_user(
    state: &AppState,
    session: &Session,
) -> anyhow::Result<Option<UserRow>> {
    let (auth_state, username) = session_state(session).await?;
    if auth_state.as_deref() != Some(STATE_AUTHENTICATED) {
        return Ok(None);
    }
    let login_time: Option<i64> = session.get(KEY_LOGIN_TIME).await?;
    if !matches!(login_time, Some(t) if now_secs() - t < MAX_SESSION_SECS) {
        return Ok(None);
    }
    let Some(username) = username else {
        return Ok(None);
    };
    let user = fetch_user(&state.db, &username).await?;
    match user {
        // A pending password change or unconfirmed TOTP means the current
        // credential is suspect (admin reset) — kill the session.
        Some(u) if u.totp_confirmed && !u.must_change_password => Ok(Some(u)),
        _ => {
            session.flush().await?;
            Ok(None)
        }
    }
}

/// Middleware for everything under /api except auth/setup routes.
pub async fn require_auth(
    State(state): State<AppState>,
    session: Session,
    mut req: Request,
    next: Next,
) -> Response {
    match load_authenticated_user(&state, &session).await {
        Ok(Some(user)) => {
            req.extensions_mut().insert(user.to_current());
            next.run(req).await
        }
        Ok(None) => AppError::Unauthorized.into_response(),
        Err(e) => AppError::Internal(e).into_response(),
    }
}

/// Layered inside require_auth on the admin sub-router.
pub async fn require_admin(user: CurrentUser, req: Request, next: Next) -> Response {
    if !user.is_admin() {
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

/// Cross-origin requests can't set custom headers without a CORS preflight,
/// and no CORS layer is configured — so requiring this header on every
/// mutation blocks CSRF even if SameSite fails us.
pub async fn csrf_guard(req: Request, next: Next) -> Response {
    let method = req.method();
    let safe = matches!(
        *method,
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    if !safe && req.headers().get("x-csrf").is_none() {
        return AppError::Forbidden.into_response();
    }
    next.run(req).await
}

/// Validate the Origin header on WebSocket handshakes. Session cookies ride
/// the handshake and SameSite is not reliably applied to WS, so this is the
/// real cross-site defense for the terminal.
pub fn check_ws_origin(headers: &HeaderMap, state: &AppState) -> Result<(), AppError> {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .ok_or(AppError::Forbidden)?;

    if let Some(expected) = &state.config.public_origin {
        if origin == expected {
            return Ok(());
        }
    }
    // Dev / no configured origin: accept same-host origins (localhost dev server).
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let origin_host = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
        .unwrap_or("");
    let same_host = |h: &str| {
        let hh = h.split(':').next().unwrap_or("");
        hh == origin_host.split(':').next().unwrap_or("-")
    };
    if same_host(host) || origin_host.starts_with("localhost") || origin_host.starts_with("127.0.0.1")
    {
        return Ok(());
    }
    Err(AppError::Forbidden)
}
