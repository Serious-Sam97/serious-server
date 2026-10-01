//! History endpoints backed by ClickHouse: events timeline, node comparison,
//! per-period summary and per-container usage. Queries run only when a page
//! asks; each returns a few hundred pre-aggregated points at most.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::clickhouse::Clickhouse;
use super::now_secs;
use crate::auth::perms::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Who may see a node's history: home → the system monitor permission;
/// a droplet → access to that node.
fn may_see(user: &CurrentUser, node: &str) -> bool {
    if node == "home" { user.can_system() } else { user.can_node(node) }
}

async fn visible_nodes(state: &AppState, user: &CurrentUser) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let rows: Vec<(String, String)> = state
        .db
        .call(|c| c.prepare("SELECT name, color FROM nodes ORDER BY name")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .await?;
    let mut out = Vec::new();
    if user.can_system() {
        out.push(("home".to_string(), None));
    }
    out.extend(rows.into_iter().filter(|(n, _)| user.can_node(n)).map(|(n, c)| (n, Some(c))));
    Ok(out)
}

fn ch(state: &AppState) -> Option<Clickhouse> {
    state.fleet.as_ref().and_then(|f| f.clickhouse())
}

fn internal(e: anyhow::Error) -> AppError {
    AppError::Internal(anyhow::anyhow!("history: {e:#}"))
}

/// A docker event about a project this user may not see stays hidden.
fn event_visible(user: &CurrentUser, kind: &str, detail: &serde_json::Value) -> bool {
    if user.is_admin() {
        return true;
    }
    match detail["project"].as_str() {
        Some(p) if !p.is_empty() => user.project(p).view,
        _ => kind != "docker",
    }
}

#[derive(Deserialize)]
pub struct EventsQuery {
    node: Option<String>,
    #[serde(default = "day")]
    hours: u32,
    kind: Option<String>,
}

fn day() -> u32 {
    24
}

pub async fn events(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<Vec<serde_json::Value>>> {
    let hours = q.hours.clamp(1, 24 * 90);
    let nodes: Vec<String> = match q.node.as_deref().filter(|n| !n.is_empty() && *n != "all") {
        Some(n) => {
            user.require(may_see(&user, n))?;
            vec![n.to_string()]
        }
        None => visible_nodes(&state, &user).await?.into_iter().map(|(n, _)| n).collect(),
    };
    if nodes.is_empty() {
        return Ok(Json(Vec::new()));
    }
    let kind = q.kind.as_deref().filter(|k| !k.is_empty());
    let mut out = Vec::new();
    if let Some(ch) = ch(&state) {
        for r in ch.events(&nodes, hours, kind).await.map_err(internal)? {
            let detail: serde_json::Value =
                serde_json::from_str(r[3].as_str().unwrap_or("{}")).unwrap_or(json!({}));
            let kind = r[2].as_str().unwrap_or("");
            if event_visible(&user, kind, &detail) {
                out.push(json!({ "node": r[0], "ts": r[1], "kind": kind, "detail": detail }));
            }
        }
    } else if let Some(fleet) = &state.fleet {
        let since = now_secs() - hours as i64 * 3600;
        for n in &nodes {
            for e in fleet.recent_events(n) {
                let k = e["kind"].as_str().unwrap_or("");
                if e["ts"].as_i64().unwrap_or(0) >= since
                    && kind.is_none_or(|want| want == k)
                    && event_visible(&user, k, &e["detail"])
                {
                    out.push(json!({ "node": n, "ts": e["ts"], "kind": k, "detail": e["detail"] }));
                }
            }
        }
        out.sort_by_key(|e| std::cmp::Reverse(e["ts"].as_i64().unwrap_or(0)));
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct CompareQuery {
    #[serde(default = "day")]
    hours: u32,
    #[serde(default = "cpu")]
    metric: String,
}

fn cpu() -> String {
    "cpu".into()
}

/// One metric for every visible node on a shared time axis (gaps = null).
pub async fn compare(
    State(state): State<AppState>,
    user: CurrentUser,
    Query(q): Query<CompareQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let ch = ch(&state).ok_or(AppError::NotFound)?;
    let hours = q.hours.clamp(1, 24 * 90);
    let metric = if q.metric == "mem" { "mem" } else { "cpu" };
    let nodes = visible_nodes(&state, &user).await?;
    let names: Vec<String> = nodes.iter().map(|(n, _)| n.clone()).collect();
    let rows = ch.compare(&names, hours, metric).await.map_err(internal)?;
    let step = Clickhouse::step(hours) as i64;
    let end = now_secs();
    let start = (end - hours as i64 * 3600) / step * step;
    let times: Vec<i64> = (0..).map(|i| start + i * step).take_while(|t| *t <= end).collect();
    let series: Vec<serde_json::Value> = nodes
        .iter()
        .map(|(name, color)| {
            let mut values = vec![serde_json::Value::Null; times.len()];
            for r in rows.iter().filter(|r| r[0] == name.as_str()) {
                let x = r[1].as_i64().unwrap_or(0);
                if x >= start {
                    if let Some(slot) = values.get_mut(((x - start) / step) as usize) {
                        *slot = r[2].clone();
                    }
                }
            }
            json!({ "node": name, "color": color, "values": values })
        })
        .collect();
    // Buckets nobody has reached yet (the store is written every 30 s) would
    // read as "no data" at the live edge: drop the trailing all-empty ones.
    let filled = (0..times.len())
        .rev()
        .find(|i| series.iter().any(|s| !s["values"][*i].is_null()))
        .map_or(0, |i| i + 1);
    let times = &times[..filled];
    let series: Vec<serde_json::Value> = series
        .into_iter()
        .map(|mut s| {
            if let Some(v) = s["values"].as_array_mut() {
                v.truncate(filled);
            }
            s
        })
        .collect();
    Ok(Json(json!({ "metric": metric, "hours": hours, "times": times, "series": series })))
}

#[derive(Deserialize)]
pub struct SummaryQuery {
    #[serde(default = "week")]
    days: u32,
}

fn week() -> u32 {
    7
}

/// Reporting uptime, container restarts and backup outcomes over `days`.
pub async fn summary(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(node): Path<String>,
    Query(q): Query<SummaryQuery>,
) -> AppResult<Json<serde_json::Value>> {
    user.require(may_see(&user, &node))?;
    let days = q.days.clamp(1, 90);
    let since = now_secs() - days as i64 * 86_400;
    let (n, s) = (node.clone(), since);
    let backups: Vec<(String, i64)> = state
        .db
        .call(move |c| {
            c.prepare(
                "SELECT status, COUNT(*) FROM backups WHERE node = ?1 AND kind != 'wal' AND started_at >= ?2 GROUP BY status",
            )?
            .query_map(rusqlite::params![n, s], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
        })
        .await?;
    let count = |status: &str| backups.iter().find(|b| b.0 == status).map(|b| b.1).unwrap_or(0);
    let (mut uptime, mut restarts) = (serde_json::Value::Null, Vec::new());
    if let Some(ch) = ch(&state) {
        let (minutes, first) = ch.coverage(&node, days).await.map_err(internal)?;
        if minutes > 0 {
            let from = first.max(since);
            let expected = ((now_secs() - from) / 60).max(1) as f64;
            uptime = json!(((minutes as f64 / expected) * 100.0).min(100.0));
        }
        restarts = ch
            .container_events(&node, days)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| json!({ "container": r[0], "restarts": r[1], "dies": r[2], "ooms": r[3] }))
            .collect();
    }
    Ok(Json(json!({
        "days": days,
        "uptime_pct": uptime,
        "containers": restarts,
        "backups": { "ok": count("ok"), "failed": count("failed") },
    })))
}

#[derive(Deserialize)]
pub struct ContainersQuery {
    #[serde(default = "day")]
    hours: u32,
}

/// Per-container usage: the latest minute plus a series over `hours`.
pub async fn containers(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(node): Path<String>,
    Query(q): Query<ContainersQuery>,
) -> AppResult<Json<serde_json::Value>> {
    user.require(may_see(&user, &node))?;
    let fleet = state.fleet.clone().ok_or(AppError::NotFound)?;
    let hours = q.hours.clamp(1, 24 * 90);
    let visible = |project: &str| user.is_admin() || (!project.is_empty() && user.project(project).view);
    let (ts, latest) = fleet.latest_containers(&node);
    let mut by_name: std::collections::BTreeMap<String, serde_json::Value> = std::collections::BTreeMap::new();
    for c in latest.into_iter().filter(|c| visible(&c.project)) {
        by_name.insert(
            c.container.clone(),
            json!({ "project": c.project, "service": c.service, "container": c.container,
                    "latest": { "cpu": c.cpu, "mem": c.mem, "mem_limit": c.mem_limit, "rx": c.rx, "tx": c.tx },
                    "series": [] }),
        );
    }
    if let Some(ch) = ch(&state) {
        for r in ch.containers(&node, hours).await.map_err(internal)? {
            let project = r[0].as_str().unwrap_or("");
            if !visible(project) {
                continue;
            }
            let name = r[2].as_str().unwrap_or("").to_string();
            let entry = by_name.entry(name.clone()).or_insert_with(|| {
                json!({ "project": project, "service": r[1], "container": name, "latest": null, "series": [] })
            });
            if let Some(series) = entry["series"].as_array_mut() {
                series.push(json!([r[3], r[4], r[5]]));
            }
        }
    }
    Ok(Json(json!({ "ts": ts, "hours": hours, "containers": by_name.into_values().collect::<Vec<_>>() })))
}
