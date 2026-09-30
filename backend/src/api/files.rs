use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::audit::audit;
use crate::auth::client_ip;
use crate::auth::perms::CurrentUser;
use crate::api::projects::permitted_dirs;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const MAX_EDIT_BYTES: u64 = 2 * 1024 * 1024;
const SKIP_DIRS: [&str; 4] = ["node_modules", ".git", "target", "__pycache__"];

/// The security core of the file API: reject sneaky components up front,
/// canonicalize (which resolves symlinks), then require the result to live
/// under an allowed root.
fn resolve(roots: &[PathBuf], raw: &str, for_write: bool) -> Result<PathBuf, AppError> {
    if raw.contains('\0') {
        return Err(AppError::BadRequest("invalid path".into()));
    }
    let path = Path::new(raw);
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return Err(AppError::BadRequest("path must be absolute and normalized".into()));
    }

    let canonical = if for_write && !path.exists() {
        // New file: canonicalize the parent, then re-append the final name.
        let parent = path.parent().ok_or(AppError::Forbidden)?;
        let name = path.file_name().ok_or(AppError::Forbidden)?;
        parent
            .canonicalize()
            .map_err(|_| AppError::NotFound)?
            .join(name)
    } else {
        path.canonicalize().map_err(|_| AppError::NotFound)?
    };

    if !roots.iter().any(|root| canonical.starts_with(root)) {
        return Err(AppError::Forbidden);
    }
    if for_write
        && canonical
            .components()
            .any(|c| c.as_os_str() == ".git")
    {
        return Err(AppError::Forbidden);
    }
    Ok(canonical)
}

pub async fn roots(
    State(state): State<AppState>,
    user: CurrentUser,
) -> AppResult<Json<serde_json::Value>> {
    let roots = permitted_dirs(&state, &user, |p| p.files).await?;
    Ok(Json(json!(roots
        .iter()
        .map(|r| r.to_string_lossy())
        .collect::<Vec<_>>())))
}

#[derive(Deserialize)]
pub struct TreeQuery {
    path: String,
    #[serde(default)]
    all: bool,
}

pub async fn tree(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<TreeQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let roots = permitted_dirs(&state, &user, |p| p.files).await?;
    let dir = resolve(&roots, &q.path, false)?;
    if !dir.is_dir() {
        return Err(AppError::BadRequest("not a directory".into()));
    }

    let show_all = q.all;
    let entries = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<serde_json::Value>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata()?;
            let is_symlink = entry.path().is_symlink();
            let kind = if meta.is_dir() {
                if !show_all && SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                "dir"
            } else if is_symlink {
                "symlink"
            } else {
                "file"
            };
            out.push(json!({
                "name": name,
                "type": kind,
                "size": meta.len(),
                "mtime_ms": mtime_ms(&meta),
            }));
        }
        out.sort_by(|a, b| {
            let (ad, bd) = (a["type"] == "dir", b["type"] == "dir");
            bd.cmp(&ad)
                .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
        });
        Ok(out)
    })
    .await
    .map_err(anyhow::Error::from)?
    .map_err(|e| anyhow::anyhow!("read dir: {e}"))?;

    Ok(Json(json!(entries)))
}

fn mtime_ms(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[derive(Deserialize)]
pub struct ReadQuery {
    path: String,
}

pub async fn read(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<ReadQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let roots = permitted_dirs(&state, &user, |p| p.files).await?;
    let path = resolve(&roots, &q.path, false)?;
    let meta = std::fs::metadata(&path).map_err(|_| AppError::NotFound)?;
    if !meta.is_file() {
        return Err(AppError::BadRequest("not a file".into()));
    }
    if meta.len() > MAX_EDIT_BYTES {
        return Err(AppError::BadRequest(format!(
            "file too large to edit ({} bytes)",
            meta.len()
        )));
    }

    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| anyhow::anyhow!("read: {e}"))?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(AppError::BadRequest("binary file".into()));
    }

    Ok(Json(json!({
        "content": String::from_utf8_lossy(&bytes),
        "size": meta.len(),
        "mtime_ms": mtime_ms(&meta),
    })))
}

#[derive(Deserialize)]
pub struct WriteReq {
    path: String,
    content: String,
    /// mtime the client last saw; None allowed only for new files.
    expected_mtime_ms: Option<i64>,
}

pub async fn write(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<WriteReq>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    let roots = permitted_dirs(&state, &user, |p| p.files).await?;
    let path = resolve(&roots, &req.path, true)?;

    if let Ok(meta) = std::fs::metadata(&path) {
        let current = mtime_ms(&meta);
        if req.expected_mtime_ms != Some(current) {
            return Err(AppError::Conflict(
                "file changed on disk since it was loaded".into(),
            ));
        }
    } else if req.expected_mtime_ms.is_some() {
        return Err(AppError::Conflict("file no longer exists".into()));
    }

    // Write to a temp file in the same directory, then rename over the
    // target so a crash can't leave a half-written .env behind.
    let parent = path.parent().ok_or(AppError::Forbidden)?.to_path_buf();
    let tmp = parent.join(format!(
        ".{}.serious-tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    tokio::fs::write(&tmp, req.content.as_bytes())
        .await
        .map_err(|e| anyhow::anyhow!("write temp: {e}"))?;
    if let Err(e) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(anyhow::anyhow!("rename: {e}").into());
    }

    let meta = std::fs::metadata(&path).map_err(|e| anyhow::anyhow!("stat: {e}"))?;
    audit(
        &state.db,
        &ip,
        &user.username,
        "file.write",
        &format!("{} ({} bytes)", path.display(), req.content.len()),
        true,
    );
    Ok((
        StatusCode::OK,
        Json(json!({ "mtime_ms": mtime_ms(&meta) })),
    )
        .into_response())
}
