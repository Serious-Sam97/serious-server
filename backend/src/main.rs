mod api;
mod audit;
mod auth;
mod backups;
mod config;
mod db;
mod embed;
mod error;
mod fleet;
mod pty;
mod security;
mod state;

use std::net::SocketAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use axum::extract::Request;
use axum::http::{header, HeaderValue};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use rand::RngExt;
use serde_json::json;
use tower_http::trace::TraceLayer;
use tower_sessions::cookie::SameSite;
use tower_sessions::{Expiry, MemoryStore, SessionManagerLayer};

use crate::state::AppState;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "serious_server=info,tower_http=warn".into()),
        )
        .init();

    let config = Arc::new(config::Config::load()?);
    // Agents sit next to production apps on small droplets: one runtime
    // thread and a small blocking pool instead of a worker per core.
    let mut runtime = if config.mode == config::Mode::Agent {
        let mut b = tokio::runtime::Builder::new_current_thread();
        b.max_blocking_threads(8);
        b
    } else {
        tokio::runtime::Builder::new_multi_thread()
    };
    let runtime = runtime.enable_all().build()?;
    let result = runtime.block_on(run(config));
    // Blocking-pool work (a pty read, a git command) can't be cancelled;
    // don't let it hold the exit hostage.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// The node-local API: everything a user does on ONE machine. Served
/// directly on the home server, and through the fleet tunnel on agents.
pub fn node_api() -> Router<AppState> {
    Router::new()
        .route("/projects", get(api::projects::list))
        .route(
            "/projects/{name}/containers",
            get(api::docker::list_containers),
        )
        .route("/compose/{name}/{action}", post(api::compose::project_action))
        .route("/jobs/{id}", get(api::compose::job_status))
        .route(
            "/containers/{id}/{action}",
            post(api::docker::container_action),
        )
        .route("/ws/containers/{id}/logs", get(api::docker::logs_ws))
        .route("/ws/terminal", get(api::terminal::terminal_ws))
        .route("/ws/system", get(api::system::stats_ws))
        .route("/git/{name}/status", get(api::git::status))
        .route("/git/{name}/resolve", post(api::git::resolve))
        .route("/git/{name}/mark_resolved", post(api::git::mark_resolved))
        .route("/git/{name}/{action}", post(api::git::action))
        .route("/files/roots", get(api::files::roots))
        .route("/files/tree", get(api::files::tree))
        .route("/files/read", get(api::files::read))
        .route("/files/write", put(api::files::write))
        .route("/system/stats", get(api::system::stats))
        .route("/system/services", get(api::system::services))
        .route("/backups/targets", get(backups::targets))
}

async fn run(config: Arc<config::Config>) -> anyhow::Result<()> {
    let db = db::Db::open(&config.data_dir)?;
    let docker = bollard::Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("docker connect: {e}"))?;
    let sampler = api::system::spawn_sampler(config.sample_interval);

    // First boot (no enrolled admin): mint a setup token and print it, so
    // only someone who can read this machine's journal can enroll. A
    // headless agent has no users — the master's users act on it.
    let setup_token = if config.headless {
        None
    } else {
        let confirmed: i64 = db
            .call(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM users WHERE role = 'admin' AND totp_confirmed = 1",
                    [],
                    |r| r.get(0),
                )
            })
            .await?;
        if confirmed == 0 {
            let token = format!("{:032x}", rand::rng().random::<u128>());
            tracing::info!("no admin enrolled — open /setup and use setup token: {token}");
            Some(token)
        } else {
            None
        }
    };

    let fleet = (config.mode == config::Mode::Master)
        .then(|| Arc::new(fleet::master::Fleet::new(&config)));

    let backups = (config.mode != config::Mode::Agent)
        .then(|| Arc::new(backups::master::Backups::new(config.backup_dir.clone())));

    let state = AppState {
        config: config.clone(),
        db,
        docker,
        setup_token: Arc::new(Mutex::new(setup_token)),
        login_guard: Arc::new(security::LoginGuard::new()),
        sampler,
        jobs: Arc::new(Mutex::new(state::Jobs::default())),
        terminal_sessions: Arc::new(AtomicUsize::new(0)),
        fleet,
        backups,
    };

    if config.mode == config::Mode::Agent {
        fleet::agent::spawn(state.clone());
    }
    if state.fleet.is_some() {
        fleet::master::spawn(state.clone()).await?;
    }
    if state.backups.is_some() {
        backups::master::spawn_local(state.clone()).await?;
    }

    if config.headless {
        tracing::info!(
            "serious-server agent running headless (sampling every {}s)",
            config.sample_interval.as_secs()
        );
        shutdown_signal().await;
        return Ok(());
    }

    let session_store = MemoryStore::default();
    let cookie_name = if config.cookie_secure {
        "__Host-ss_session"
    } else {
        "ss_session"
    };
    let session_layer = SessionManagerLayer::new(session_store)
        .with_name(cookie_name)
        .with_secure(config.cookie_secure)
        .with_same_site(SameSite::Strict)
        .with_http_only(true)
        .with_expiry(Expiry::OnInactivity(time::Duration::hours(12)));

    let public_api = Router::new()
        .route("/health", get(|| async { Json(json!({ "ok": true })) }))
        .route("/setup/status", get(auth::setup::status))
        .route("/setup", post(auth::setup::begin))
        .route("/setup/confirm", post(auth::setup::confirm))
        .route("/auth/login", post(auth::login))
        .route("/auth/totp", post(auth::verify_totp))
        .route("/auth/change_password", post(auth::change_password))
        .route("/auth/enroll/begin", post(auth::enroll_begin))
        .route("/auth/enroll/confirm", post(auth::enroll_confirm))
        .route("/auth/session", get(auth::session_info));

    let admin_api = Router::new()
        .route(
            "/admin/users",
            get(api::users::list).post(api::users::create),
        )
        .route("/admin/users/{id}", axum::routing::delete(api::users::delete_user))
        .route(
            "/admin/users/{id}/reset_password",
            post(api::users::reset_password),
        )
        .route("/admin/users/{id}/reset_totp", post(api::users::reset_totp))
        .route(
            "/admin/users/{id}/permissions",
            put(api::users::set_permissions),
        )
        .route("/audit", get(api::audit_log))
        .merge(fleet::master::admin_routes())
        .layer(middleware::from_fn(auth::require_admin));

    let protected_api = node_api()
        .route("/auth/logout", post(auth::logout))
        .merge(fleet::master::api_routes())
        .merge(backups::master::routes())
        .merge(admin_api)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ));

    let app = Router::new()
        .nest(
            "/api",
            public_api
                .merge(protected_api)
                .layer(middleware::from_fn(auth::csrf_guard)),
        )
        .fallback(embed::spa_handler)
        .layer(session_layer)
        .layer(middleware::from_fn(security_headers))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(
        "serious-server ({:?}) listening on http://{}",
        config.mode,
        config.bind
    );
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

/// Ctrl-C or SIGTERM. As PID 1 in a container the process ignores SIGTERM
/// unless it handles it, so `docker stop` would otherwise wait out its 10 s
/// timeout and SIGKILL.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}

async fn security_headers(req: Request, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
             connect-src 'self' wss: ws:; frame-ancestors 'none'",
        ),
    );
    if is_api {
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    res
}
