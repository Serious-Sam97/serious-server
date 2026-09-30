use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::audit::audit;
use crate::auth::perms::{CurrentUser, Permissions};
use crate::auth::{client_ip, password, totp};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const MIN_PASSWORD_LEN: usize = 12;

#[derive(Debug, Serialize)]
pub struct UserSummary {
    pub id: i64,
    pub username: String,
    pub role: String,
    pub totp_confirmed: bool,
    pub must_change_password: bool,
    pub created_at: String,
    pub permissions: Permissions,
}

pub async fn list(State(state): State<AppState>) -> AppResult<Json<Vec<UserSummary>>> {
    let rows = state
        .db
        .call(|c| {
            let mut stmt = c.prepare(
                "SELECT id, username, role, totp_confirmed, must_change_password,
                        created_at, permissions
                 FROM users ORDER BY role DESC, username",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await?;

    Ok(Json(
        rows.into_iter()
            .map(
                |(id, username, role, totp_confirmed, must_change_password, created_at, perms)| {
                    let permissions = Permissions::parse_or_deny(&perms, &username);
                    UserSummary {
                        id,
                        username,
                        role,
                        totp_confirmed,
                        must_change_password,
                        created_at,
                        permissions,
                    }
                },
            )
            .collect(),
    ))
}

/// Fetch a target user and refuse to touch the admin. The DB triggers are
/// the backstop; this exists for friendly errors.
async fn non_admin_target(state: &AppState, id: i64) -> AppResult<String> {
    let row = state
        .db
        .call(move |c| {
            c.query_row(
                "SELECT username, role FROM users WHERE id = ?1",
                [id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other),
            })
        })
        .await?;
    match row {
        None => Err(AppError::NotFound),
        Some((_, role)) if role == "admin" => Err(AppError::Forbidden),
        Some((username, _)) => Ok(username),
    }
}

#[derive(Deserialize)]
pub struct CreateReq {
    username: String,
    temp_password: String,
    #[serde(default)]
    permissions: Permissions,
}

pub async fn create(
    State(state): State<AppState>,
    admin: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<CreateReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    let username = req.username.trim().to_string();
    if username.is_empty() {
        return Err(AppError::BadRequest("username required".into()));
    }
    if req.temp_password.len() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "temporary password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }

    let hash = {
        let pw = req.temp_password.clone();
        tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(anyhow::Error::from)??
    };
    // Placeholder secret; regenerated when the user enrolls at first login.
    let secret = totp::generate_secret();
    let perms_json = serde_json::to_string(&req.permissions).map_err(anyhow::Error::from)?;

    let (u, h, s, p) = (username.clone(), hash, secret, perms_json);
    let result = state
        .db
        .call(move |c| {
            c.execute(
                "INSERT INTO users (username, password_hash, totp_secret, role,
                                    must_change_password, permissions)
                 VALUES (?1, ?2, ?3, 'user', 1, ?4)",
                rusqlite::params![u, h, s, p],
            )?;
            Ok(c.last_insert_rowid())
        })
        .await;

    match result {
        Ok(id) => {
            audit(&state.db, &ip, &admin.username, "user.create", &username, true);
            Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
        }
        Err(e) if e.to_string().contains("UNIQUE") => {
            Err(AppError::Conflict("username already exists".into()))
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_user(
    State(state): State<AppState>,
    admin: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let ip = client_ip(&headers, &peer);
    let username = non_admin_target(&state, id).await?;
    state
        .db
        .call(move |c| c.execute("DELETE FROM users WHERE id = ?1", [id]))
        .await?;
    audit(&state.db, &ip, &admin.username, "user.delete", &username, true);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct ResetPasswordReq {
    temp_password: String,
}

/// Back to temp-password state; the user's live sessions die on their next
/// request (require_auth re-checks must_change_password) and TOTP is
/// demanded before the change screen on re-login.
pub async fn reset_password(
    State(state): State<AppState>,
    admin: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<ResetPasswordReq>,
) -> AppResult<StatusCode> {
    let ip = client_ip(&headers, &peer);
    let username = non_admin_target(&state, id).await?;
    if req.temp_password.len() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "temporary password must be at least {MIN_PASSWORD_LEN} characters"
        )));
    }
    let hash = {
        let pw = req.temp_password.clone();
        tokio::task::spawn_blocking(move || password::hash(&pw))
            .await
            .map_err(anyhow::Error::from)??
    };
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET password_hash = ?1, must_change_password = 1 WHERE id = ?2",
                rusqlite::params![hash, id],
            )
        })
        .await?;
    audit(
        &state.db,
        &ip,
        &admin.username,
        "user.reset_password",
        &username,
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Force re-enrollment: unconfirm now (locks out sessions), secret is
/// regenerated at the next enroll/begin so the old one is dead on arrival.
pub async fn reset_totp(
    State(state): State<AppState>,
    admin: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    let ip = client_ip(&headers, &peer);
    let username = non_admin_target(&state, id).await?;
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET totp_confirmed = 0, totp_last_step = 0 WHERE id = ?1",
                [id],
            )
        })
        .await?;
    audit(
        &state.db,
        &ip,
        &admin.username,
        "user.reset_totp",
        &username,
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct SetPermissionsReq {
    permissions: Permissions,
}

pub async fn set_permissions(
    State(state): State<AppState>,
    admin: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(req): Json<SetPermissionsReq>,
) -> AppResult<StatusCode> {
    let ip = client_ip(&headers, &peer);
    let username = non_admin_target(&state, id).await?;
    let perms_json = serde_json::to_string(&req.permissions).map_err(anyhow::Error::from)?;
    let detail = format!("{username}: {perms_json}");
    state
        .db
        .call(move |c| {
            c.execute(
                "UPDATE users SET permissions = ?1 WHERE id = ?2",
                rusqlite::params![perms_json, id],
            )
        })
        .await?;
    audit(
        &state.db,
        &ip,
        &admin.username,
        "user.permissions",
        &detail,
        true,
    );
    Ok(StatusCode::NO_CONTENT)
}
