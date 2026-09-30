pub mod compose;
pub mod docker;
pub mod files;
pub mod git;
pub mod projects;
pub mod system;
pub mod terminal;
pub mod users;

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;

use crate::audit::AuditEntry;
use crate::error::AppResult;
use crate::state::AppState;

#[derive(Deserialize)]
pub struct AuditQuery {
    #[serde(default = "default_limit")]
    limit: u32,
    before: Option<i64>,
}

fn default_limit() -> u32 {
    100
}

pub async fn audit_log(
    State(state): State<AppState>,
    Query(q): Query<AuditQuery>,
) -> AppResult<Json<Vec<AuditEntry>>> {
    Ok(Json(crate::audit::recent(&state.db, q.limit, q.before).await?))
}
