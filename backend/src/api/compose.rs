use std::net::SocketAddr;
use std::process::Stdio;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use tokio::io::AsyncReadExt;

use crate::audit::audit;
use crate::auth::client_ip;
use crate::auth::perms::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::{AppState, Job, JobStatus};

const MAX_STORED_JOBS: usize = 50;
const MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Env files fed to compose *interpolation*, in increasing precedence.
///
/// Compose only reads `.env` by default, and `env_file:` in a service supplies
/// variables at runtime — never to `${...}` interpolation. A project that keeps
/// its values in `.env.local` therefore gets empty build args, which for
/// build-time-inlined values (Next.js `NEXT_PUBLIC_*`, Vite `VITE_*`) bakes a
/// broken bundle into the image. Passing the files we find keeps the default
/// behaviour when only `.env` exists.
const ENV_FILES: [&str; 2] = [".env", ".env.local"];

#[derive(serde::Deserialize)]
pub struct ComposePath {
    name: String,
    action: String,
}

/// Run a project-level `docker compose <action>` as a background job and
/// return its id immediately; output is polled via GET /api/jobs/{id}.
pub async fn project_action(
    State(state): State<AppState>,
    user: CurrentUser,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(ComposePath { name, action }): Path<ComposePath>,
) -> AppResult<Response> {
    let ip = client_ip(&headers, &peer);
    let args: Vec<&str> = match action.as_str() {
        "up" => vec!["up", "-d"],
        // Rebuild images from source, then (re)create — the "deploy my latest
        // code" action, e.g. after a git pull.
        "up_build" => vec!["up", "-d", "--build"],
        "build" => vec!["build"],
        "down" => vec!["down"],
        "restart" => vec!["restart"],
        // Skip services built from source: their image (e.g. `image:
        // limiar-core` + `build:`) exists only locally, so pulling it fails
        // the whole job. `up --build` / `build` is how those get updated.
        "pull" => vec!["pull", "--ignore-buildable"],
        _ => return Err(AppError::BadRequest("unknown action".into())),
    };

    // Resolves through the scanner, so `name` maps to a directory under the
    // allowed roots — arbitrary paths can't be smuggled in.
    let project = crate::api::projects::find_project(&state, &name).await?;
    user.require(user.project(&project.name).control)?;

    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("compose");
    for f in &project.compose_files {
        cmd.args(["-f", f]);
    }
    // Global flags; must precede the subcommand. `--env-file` replaces the
    // default `.env` rather than adding to it, so pass every one that exists.
    for name in ENV_FILES {
        let path = std::path::Path::new(&project.path).join(name);
        if path.is_file() {
            cmd.arg("--env-file").arg(&path);
        }
    }
    cmd.args(&args)
        .current_dir(&project.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn docker compose: {e}"))?;

    let job_id = {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.next_id += 1;
        let id = jobs.next_id;
        jobs.jobs.insert(
            id,
            Job {
                id,
                action: action.clone(),
                project: name.clone(),
                status: JobStatus::Running,
                output: String::new(),
            },
        );
        // Evict the oldest finished jobs beyond the cap.
        if jobs.jobs.len() > MAX_STORED_JOBS {
            let mut done: Vec<u64> = jobs
                .jobs
                .values()
                .filter(|j| j.status != JobStatus::Running)
                .map(|j| j.id)
                .collect();
            done.sort();
            for id in done
                .into_iter()
                .take(jobs.jobs.len().saturating_sub(MAX_STORED_JOBS))
            {
                jobs.jobs.remove(&id);
            }
        }
        id
    };

    audit(
        &state.db,
        &ip,
        &user.username,
        "compose.action",
        &format!("{name} {action} (job {job_id})"),
        true,
    );

    let jobs_handle = state.jobs.clone();
    tokio::spawn(async move {
        let mut stdout = child.stdout.take().expect("stdout piped");
        let mut stderr = child.stderr.take().expect("stderr piped");
        let mut out_buf = Vec::new();
        let mut err_buf = Vec::new();
        let (_, _, wait) = tokio::join!(
            stdout.read_to_end(&mut out_buf),
            stderr.read_to_end(&mut err_buf),
            child.wait()
        );

        let mut combined = String::from_utf8_lossy(&err_buf).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out_buf));
        if combined.len() > MAX_OUTPUT_BYTES {
            let cut = combined.len() - MAX_OUTPUT_BYTES;
            combined = format!("[... {cut} bytes truncated]\n{}", &combined[cut..]);
        }

        let success = wait.map(|s| s.success()).unwrap_or(false);
        let mut jobs = jobs_handle.lock().unwrap();
        if let Some(job) = jobs.jobs.get_mut(&job_id) {
            job.status = if success {
                JobStatus::Done
            } else {
                JobStatus::Failed
            };
            job.output = combined;
        }
    });

    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": job_id }))).into_response())
}

pub async fn job_status(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<u64>,
) -> AppResult<Json<Job>> {
    let job = {
        let jobs = state.jobs.lock().unwrap();
        jobs.jobs.get(&id).cloned()
    }
    .ok_or(AppError::NotFound)?;
    // Compose output can contain env values and paths. 404 (not 403) so job
    // ids can't even be probed for existence.
    if !user.project(&job.project).control {
        return Err(AppError::NotFound);
    }
    Ok(Json(job))
}
