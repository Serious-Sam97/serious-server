use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::auth::perms::{CurrentUser, ProjectPerms};
use crate::error::AppResult;
use crate::state::AppState;

const COMPOSE_NAMES: [&str; 4] = [
    "docker-compose.yml",
    "docker-compose.yaml",
    "compose.yml",
    "compose.yaml",
];
const SKIP_DIRS: [&str; 6] = [
    "node_modules",
    ".git",
    "target",
    "vendor",
    "__pycache__",
    ".next",
];
const MAX_DEPTH: usize = 4;

#[derive(Debug, Serialize)]
pub struct Project {
    pub name: String,
    pub path: String,
    pub compose_files: Vec<String>,
    pub status: &'static str,
    pub running: usize,
    pub total: usize,
}

/// Directories under the allowed roots that contain a compose file.
fn scan_compose_dirs(roots: &[PathBuf]) -> Vec<(PathBuf, Vec<PathBuf>)> {
    let mut found = Vec::new();
    for root in roots {
        walk(root, 0, &mut found);
    }
    found.sort();
    found
}

fn walk(dir: &Path, depth: usize, found: &mut Vec<(PathBuf, Vec<PathBuf>)>) {
    if depth > MAX_DEPTH {
        return;
    }
    let compose_files: Vec<PathBuf> = COMPOSE_NAMES
        .iter()
        .map(|n| dir.join(n))
        .filter(|p| p.is_file())
        .collect();
    if !compose_files.is_empty() {
        found.push((dir.to_path_buf(), compose_files));
        return; // nested compose projects under a compose project are unusual; keep the top one
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() && !SKIP_DIRS.contains(&name.as_ref()) && !path.is_symlink() {
            walk(&path, depth + 1, found);
        }
    }
}

/// Container counts per compose project name, from Docker labels.
async fn container_counts(docker: &bollard::Docker) -> HashMap<String, (usize, usize)> {
    let opts = bollard::query_parameters::ListContainersOptionsBuilder::default()
        .all(true)
        .build();
    let mut counts: HashMap<String, (usize, usize)> = HashMap::new();
    if let Ok(containers) = docker.list_containers(Some(opts)).await {
        for c in containers {
            let Some(project) = c
                .labels
                .as_ref()
                .and_then(|l| l.get("com.docker.compose.project"))
            else {
                continue;
            };
            let entry = counts.entry(project.clone()).or_default();
            entry.1 += 1;
            if c.state
                .as_ref()
                .is_some_and(|s| format!("{s:?}").to_lowercase().contains("running"))
            {
                entry.0 += 1;
            }
        }
    }
    counts
}

/// Map compose config-file path -> compose project name, via `docker compose ls`.
async fn compose_ls_names() -> HashMap<PathBuf, String> {
    let mut map = HashMap::new();
    let output = tokio::process::Command::new("docker")
        .args(["compose", "ls", "--all", "--format", "json"])
        .output()
        .await;
    let Ok(output) = output else { return map };
    let Ok(list) = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout) else {
        return map;
    };
    for item in list {
        let (Some(name), Some(files)) = (
            item.get("Name").and_then(|v| v.as_str()),
            item.get("ConfigFiles").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        for file in files.split(',') {
            map.insert(PathBuf::from(file.trim()), name.to_string());
        }
    }
    map
}

/// Unfiltered scan — internal use only; the route handler filters by
/// the caller's view permission.
pub async fn list_all(state: &AppState) -> AppResult<Vec<Project>> {
    let roots = state.config.allowed_roots.clone();
    let scanned = tokio::task::spawn_blocking(move || scan_compose_dirs(&roots))
        .await
        .map_err(anyhow::Error::from)?;
    let (counts, names) = tokio::join!(container_counts(&state.docker), compose_ls_names());

    let projects = scanned
        .into_iter()
        .map(|(dir, files)| {
            let name = files
                .iter()
                .find_map(|f| names.get(f))
                .cloned()
                .unwrap_or_else(|| {
                    dir.file_name()
                        .map(|n| n.to_string_lossy().to_lowercase())
                        .unwrap_or_else(|| "unknown".into())
                });
            let (running, total) = counts.get(&name).copied().unwrap_or((0, 0));
            let status = match (running, total) {
                (_, 0) => "not-created",
                (0, _) => "stopped",
                (r, t) if r == t => "running",
                _ => "partial",
            };
            Project {
                name,
                path: dir.to_string_lossy().into_owned(),
                compose_files: files
                    .iter()
                    .map(|f| f.to_string_lossy().into_owned())
                    .collect(),
                status,
                running,
                total,
            }
        })
        .collect();

    Ok(projects)
}

pub async fn list(
    State(state): State<AppState>,
    user: CurrentUser,
) -> AppResult<Json<Vec<Project>>> {
    let mut projects = list_all(&state).await?;
    projects.retain(|p| user.project(&p.name).view);
    Ok(Json(projects))
}

/// Look up a scanned project by name — actions only run on projects that
/// actually live under the allowed roots.
pub async fn find_project(state: &AppState, name: &str) -> AppResult<Project> {
    list_all(state)
        .await?
        .into_iter()
        .find(|p| p.name == name)
        .ok_or(crate::error::AppError::NotFound)
}

/// Union of scanned project directories on which the user holds the given
/// capability — the jail for the file API and terminal cwd. Admin gets the
/// configured roots. Scanner dirs are canonical (children of canonicalized
/// config roots), so prefix checks against them stay sound.
pub async fn permitted_dirs(
    state: &AppState,
    user: &CurrentUser,
    cap: impl Fn(ProjectPerms) -> bool,
) -> AppResult<Vec<PathBuf>> {
    if user.is_admin() {
        return Ok(state.config.allowed_roots.clone());
    }
    Ok(list_all(state)
        .await?
        .into_iter()
        .filter(|p| cap(user.project(&p.name)))
        .map(|p| PathBuf::from(p.path))
        .collect())
}
