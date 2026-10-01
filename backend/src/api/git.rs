use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::process::Stdio;
use std::time::Duration;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::projects::{find_project, Project};
use crate::audit::audit;
use crate::auth::client_ip;
use crate::auth::perms::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Debug, Serialize)]
pub struct GitOutput {
    pub ok: bool,
    pub output: String,
}

/// Every git invocation goes through here: fixed argv (never a shell),
/// prompts disabled so missing credentials fail fast instead of hanging,
/// hard timeout, merged output.
async fn run_git(dir: &FsPath, args: &[&str], identity: Option<&str>) -> AppResult<GitOutput> {
    let mut cmd = tokio::process::Command::new("git");
    if let Some(username) = identity {
        cmd.args([
            "-c",
            &format!("user.name={username}"),
            "-c",
            &format!("user.email={username}@serious-server.local"),
        ]);
    }
    cmd.args(["-c", "color.ui=false"])
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env(
            "GIT_SSH_COMMAND",
            // ControlMaster off: the user's ~/.ssh/config may enable
            // connection multiplexing, whose control socket can't be created
            // when .ssh is mounted read-only. Automated git needs no sharing.
            "ssh -oBatchMode=yes -oStrictHostKeyChecking=accept-new \
             -oControlMaster=no -oControlPath=none",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let result = tokio::time::timeout(GIT_TIMEOUT, cmd.output())
        .await
        .map_err(|_| AppError::BadRequest("git command timed out".into()))?
        .map_err(|e| anyhow::anyhow!("spawn git: {e}"))?;

    let mut output = String::from_utf8_lossy(&result.stderr).into_owned();
    output.push_str(&String::from_utf8_lossy(&result.stdout));
    if output.len() > MAX_OUTPUT {
        let cut = output.len() - MAX_OUTPUT;
        output = format!("[... {cut} bytes truncated]\n{}", &output[cut..]);
    }
    Ok(GitOutput {
        ok: result.status.success(),
        output,
    })
}

/// Resolve project + git permission + repo check in one step.
async fn git_project(state: &AppState, user: &CurrentUser, name: &str) -> AppResult<Project> {
    let project = find_project(state, name).await?;
    if project.external {
        return Err(crate::error::AppError::BadRequest(
            "this project's files are outside the allowed roots — git is unavailable".into(),
        ));
    }
    user.require(user.project(&project.name).git)?;
    if !FsPath::new(&project.path).join(".git").exists() {
        return Err(AppError::BadRequest("not a git repository".into()));
    }
    Ok(project)
}

async fn unmerged_files(dir: &FsPath) -> AppResult<Vec<String>> {
    let out = run_git(dir, &["diff", "--name-only", "--diff-filter=U"], None).await?;
    Ok(out.output.lines().map(|l| l.to_string()).filter(|l| !l.is_empty()).collect())
}

// ---- status ----

pub async fn status(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(name): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let project = find_project(&state, &name).await?;
    if project.external {
        return Err(crate::error::AppError::BadRequest(
            "this project's files are outside the allowed roots — git is unavailable".into(),
        ));
    }
    user.require(user.project(&project.name).git)?;
    let dir = FsPath::new(&project.path);
    if !dir.join(".git").exists() {
        return Ok(Json(json!({ "is_repo": false })));
    }

    let branch = run_git(dir, &["rev-parse", "--abbrev-ref", "HEAD"], None).await?;
    let counts = run_git(
        dir,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
        None,
    )
    .await?;
    let (ahead, behind) = if counts.ok {
        let mut it = counts.output.split_whitespace();
        (
            it.next().and_then(|v| v.parse::<u32>().ok()),
            it.next().and_then(|v| v.parse::<u32>().ok()),
        )
    } else {
        (None, None) // no upstream configured
    };

    let porcelain = run_git(dir, &["status", "--porcelain"], None).await?;
    let files: Vec<_> = porcelain
        .output
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let code = &l[..2];
            let path = l[3..].to_string();
            json!({
                "path": path,
                "status": code.trim(),
                "conflicted": matches!(code, "UU" | "AA" | "DD" | "AU" | "UA" | "DU" | "UD"),
            })
        })
        .collect();

    let log = run_git(
        dir,
        &["log", "-n", "15", "--pretty=format:%h%x09%an%x09%ar%x09%s"],
        None,
    )
    .await?;
    let commits: Vec<_> = log
        .output
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(4, '\t');
            Some(json!({
                "hash": parts.next()?,
                "author": parts.next()?,
                "when": parts.next()?,
                "subject": parts.next().unwrap_or(""),
            }))
        })
        .collect();

    let stash = run_git(dir, &["stash", "list"], None).await?;

    Ok(Json(json!({
        "is_repo": true,
        "branch": branch.output.trim(),
        "ahead": ahead,
        "behind": behind,
        "in_merge": dir.join(".git/MERGE_HEAD").exists(),
        "files": files,
        "log": commits,
        "stash_count": stash.output.lines().filter(|l| !l.is_empty()).count(),
    })))
}

// ---- simple actions ----

#[derive(Deserialize)]
pub struct ActionPath {
    name: String,
    action: String,
}

pub async fn action(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(ActionPath { name, action }): Path<ActionPath>,
) -> AppResult<Json<GitOutput>> {
    let ip = client_ip(&headers, &peer);
    let project = git_project(&state, &user, &name).await?;
    let dir = FsPath::new(&project.path);

    let stash_msg = format!("serious-server: {}", user.username);
    let (args, identity): (Vec<&str>, Option<&str>) = match action.as_str() {
        "fetch" => (vec!["fetch", "--all", "--prune"], None),
        "pull" => (vec!["pull", "--no-rebase"], Some(&user.username)),
        "push" => (vec!["push"], None),
        "stash" => (
            vec!["stash", "push", "-u", "-m", &stash_msg],
            Some(&user.username),
        ),
        "stash_pop" => (vec!["stash", "pop"], Some(&user.username)),
        "merge_abort" => (vec!["merge", "--abort"], None),
        "merge_continue" => {
            if !dir.join(".git/MERGE_HEAD").exists() {
                return Err(AppError::BadRequest("no merge in progress".into()));
            }
            if !unmerged_files(dir).await?.is_empty() {
                return Err(AppError::BadRequest(
                    "conflicts remain — resolve all files first".into(),
                ));
            }
            (vec!["commit", "--no-edit"], Some(&user.username))
        }
        _ => return Err(AppError::BadRequest("unknown git action".into())),
    };

    let result = run_git(dir, &args, identity).await?;
    audit(
        &state.db,
        &ip,
        &user.username,
        &format!("git.{action}"),
        &name,
        result.ok,
    );
    Ok(Json(result))
}

// ---- conflict resolution ----

#[derive(Deserialize)]
pub struct ResolveReq {
    path: String,
    /// "ours" | "theirs" — omitted for mark_resolved
    side: Option<String>,
}

async fn validated_conflict_path(dir: &FsPath, requested: &str) -> AppResult<String> {
    // The ONLY user-supplied value that ever reaches a git argv: accepted
    // solely when it exactly matches a path git itself reports as unmerged.
    let unmerged = unmerged_files(dir).await?;
    unmerged
        .into_iter()
        .find(|p| p == requested)
        .ok_or_else(|| AppError::BadRequest("path is not a conflicted file".into()))
}

pub async fn resolve(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<ResolveReq>,
) -> AppResult<Json<GitOutput>> {
    let ip = client_ip(&headers, &peer);
    let project = git_project(&state, &user, &name).await?;
    let dir = FsPath::new(&project.path);
    let path = validated_conflict_path(dir, &req.path).await?;

    let side_flag = match req.side.as_deref() {
        Some("ours") => "--ours",
        Some("theirs") => "--theirs",
        _ => return Err(AppError::BadRequest("side must be ours or theirs".into())),
    };

    let checkout = run_git(dir, &["checkout", side_flag, "--", &path], None).await?;
    if !checkout.ok {
        return Ok(Json(checkout));
    }
    let add = run_git(dir, &["add", "--", &path], None).await?;
    audit(
        &state.db,
        &ip,
        &user.username,
        "git.resolve",
        &format!("{name}: {path} <- {side_flag}"),
        add.ok,
    );
    Ok(Json(add))
}

pub async fn mark_resolved(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(req): Json<ResolveReq>,
) -> AppResult<Json<GitOutput>> {
    let ip = client_ip(&headers, &peer);
    let project = git_project(&state, &user, &name).await?;
    let dir = FsPath::new(&project.path);
    let path = validated_conflict_path(dir, &req.path).await?;

    let add = run_git(dir, &["add", "--", &path], None).await?;
    audit(
        &state.db,
        &ip,
        &user.username,
        "git.mark_resolved",
        &format!("{name}: {path}"),
        add.ok,
    );
    Ok(Json(add))
}
