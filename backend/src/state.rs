use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::db::Db;
use crate::security::LoginGuard;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Job {
    pub id: u64,
    pub action: String,
    pub project: String,
    pub status: JobStatus,
    pub output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    Done,
    Failed,
}

#[derive(Default)]
pub struct Jobs {
    pub next_id: u64,
    pub jobs: HashMap<u64, Job>,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Db,
    pub docker: bollard::Docker,
    /// One-time token printed to the journal on first boot; consumed by setup.
    pub setup_token: Arc<Mutex<Option<String>>>,
    pub login_guard: Arc<LoginGuard>,
    /// Latest system stats + rolling history, fed by the sampler thread.
    pub sampler: crate::api::system::Sampler,
    /// docker compose job states (up/down/pull output).
    pub jobs: Arc<Mutex<Jobs>>,
    pub terminal_sessions: Arc<AtomicUsize>,
    /// Master mode only: connected agents and the API tunnel.
    pub fleet: Option<Arc<crate::fleet::master::Fleet>>,
    /// Master/standalone: the backup store and catalog.
    pub backups: Option<Arc<crate::backups::master::Backups>>,
}
