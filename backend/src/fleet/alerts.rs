//! Fleet alerts, evaluated on the master once a minute: node offline, disk
//! nearly full, backup failed / overdue / unverifiable, broken WAL chain.
//! New and resolved alerts go to ntfy and/or a webhook when configured.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use serde_json::json;

use super::now_secs;
use crate::auth::perms::CurrentUser;
use crate::backups::{Cron, Policy};
use crate::error::AppResult;
use crate::state::AppState;

const EVERY: Duration = Duration::from_secs(60);
const OFFLINE_AFTER: i64 = 5 * 60;
const DISK_FULL: f64 = 0.90;
/// Grace after a scheduled time before a backup counts as overdue.
const OVERDUE_GRACE: i64 = 3600;

#[derive(Clone, Serialize, Debug)]
pub struct Alert {
    pub key: String,
    /// `critical` or `warning`
    pub severity: &'static str,
    pub node: String,
    pub title: String,
    pub detail: String,
    pub since: i64,
}

#[derive(Default)]
pub struct Alerts {
    active: Mutex<HashMap<String, Alert>>,
}

pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        // Let agents reconnect after a master restart before judging them.
        tokio::time::sleep(Duration::from_secs(90)).await;
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tick.tick().await;
            match evaluate(&state).await {
                Ok(current) => reconcile(&state, current).await,
                Err(e) => tracing::warn!("alert evaluation failed: {e:#}"),
            }
        }
    });
}

async fn reconcile(state: &AppState, current: Vec<Alert>) {
    let Some(fleet) = &state.fleet else { return };
    let (fired, resolved) = {
        let mut active = fleet.alerts.active.lock().unwrap();
        let now_keys: Vec<String> = current.iter().map(|a| a.key.clone()).collect();
        let resolved: Vec<Alert> = active.values().filter(|a| !now_keys.contains(&a.key)).cloned().collect();
        active.retain(|k, _| now_keys.contains(k));
        let mut fired = Vec::new();
        for a in current {
            if !active.contains_key(&a.key) {
                fired.push(a.clone());
                active.insert(a.key.clone(), a);
            }
        }
        (fired, resolved)
    };
    for a in fired {
        tracing::warn!(node = a.node, "ALERT {}: {} — {}", a.severity, a.title, a.detail);
        notify(state, &a, true).await;
    }
    for a in resolved {
        tracing::info!(node = a.node, "resolved: {}", a.title);
        notify(state, &a, false).await;
    }
}

async fn notify(state: &AppState, a: &Alert, firing: bool) {
    let cfg = &state.config;
    if let Some(url) = &cfg.alert_ntfy_url {
        let title = if firing { format!("[{}] {}", a.node, a.title) } else { format!("resolved: [{}] {}", a.node, a.title) };
        let priority = if firing && a.severity == "critical" { "high" } else { "default" };
        let tags = if firing { "warning" } else { "white_check_mark" };
        if let Err(e) =
            super::net::post(url, &[("Title", &title), ("Priority", priority), ("Tags", tags)], a.detail.as_bytes()).await
        {
            tracing::warn!("ntfy notification failed: {e:#}");
        }
    }
    if let Some(url) = &cfg.alert_webhook_url {
        let body = json!({ "status": if firing { "firing" } else { "resolved" }, "alert": a }).to_string();
        if let Err(e) = super::net::post(url, &[("Content-Type", "application/json")], body.as_bytes()).await {
            tracing::warn!("alert webhook failed: {e:#}");
        }
    }
}

fn when(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(&time::macros::format_description!("[year]-[month]-[day] [hour]:[minute] UTC"))
                .ok()
        })
        .unwrap_or_default()
}

async fn evaluate(state: &AppState) -> anyhow::Result<Vec<Alert>> {
    let fleet = state.fleet.clone().ok_or_else(|| anyhow::anyhow!("not a master"))?;
    let now = now_secs();
    let mut out = Vec::new();

    // Nodes: offline, disk.
    let nodes: Vec<(String, Option<i64>)> = state
        .db
        .call(|c| c.prepare("SELECT name, last_seen FROM nodes")?.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
        .await?;
    for (name, db_seen) in nodes {
        let (online, seen, summary) = fleet.live_status(&name);
        let seen = seen.or(db_seen);
        if !online && seen.is_none_or(|t| now - t > OFFLINE_AFTER) {
            out.push(Alert {
                key: format!("offline:{name}"),
                severity: "critical",
                node: name.clone(),
                title: format!("{name} is offline"),
                detail: match seen {
                    Some(t) => format!("last seen {}", when(t)),
                    None => "never connected".into(),
                },
                since: seen.unwrap_or(now),
            });
        }
        if let Some(s) = summary.filter(|s| s.disk_total > 0) {
            let frac = s.disk_used as f64 / s.disk_total as f64;
            if frac >= DISK_FULL {
                out.push(Alert {
                    key: format!("disk:{name}"),
                    severity: "warning",
                    node: name.clone(),
                    title: format!("{name} disk {:.0}% full", frac * 100.0),
                    detail: format!("{} of {} GB used", s.disk_used / 1_000_000_000, s.disk_total / 1_000_000_000),
                    since: now,
                });
            }
        }
    }

    // Backups, per policy.
    let policies: Vec<(String, String, i64)> = state
        .db
        .call(|c| {
            c.prepare("SELECT node, policy, updated_at FROM backup_policies")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect()
        })
        .await?;
    for (node, raw, updated_at) in policies {
        let Ok(p) = serde_json::from_str::<Policy>(&raw) else { continue };
        if !p.enabled {
            continue;
        }
        let kind = if p.mode == "continuous" { "base" } else { "logical" };
        let name = format!("{}/{}", p.project, p.service);
        let (n, pr, sv, k) = (node.clone(), p.project.clone(), p.service.clone(), kind.to_string());
        // Latest rows of this kind: (status, started_at, error, verified)
        let rows: Vec<(String, i64, String, String)> = state
            .db
            .call(move |c| {
                c.prepare(
                    "SELECT status, started_at, error, verified FROM backups
                     WHERE node = ?1 AND project = ?2 AND service = ?3 AND kind = ?4
                     ORDER BY started_at DESC LIMIT 50",
                )?
                .query_map([&n, &pr, &sv, &k], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect()
            })
            .await?;
        let last_ok = rows.iter().find(|r| r.0 == "ok");
        // The newest attempt of any kind (a manual dump counts too) failed.
        let (n, pr, sv) = (node.clone(), p.project.clone(), p.service.clone());
        let newest: Option<(String, i64, String)> = state
            .db
            .call(move |c| {
                c.query_row(
                    "SELECT status, started_at, error FROM backups
                     WHERE node = ?1 AND project = ?2 AND service = ?3 AND kind != 'wal'
                     ORDER BY started_at DESC, id DESC LIMIT 1",
                    [&n, &pr, &sv],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
            })
            .await?;
        if let Some(last) = newest.filter(|r| r.0 == "failed") {
            out.push(Alert {
                key: format!("backup_failed:{node}:{name}"),
                severity: "critical",
                node: node.clone(),
                title: format!("backup of {name} failed"),
                detail: last.2.clone(),
                since: last.1,
            });
        }
        if let Ok(cron) = Cron::parse(&p.schedule) {
            let from = last_ok.map(|r| r.1).unwrap_or(updated_at);
            if let Some(due) = cron.next_after(from, p.offset()) {
                if now > due + OVERDUE_GRACE {
                    out.push(Alert {
                        key: format!("overdue:{node}:{name}"),
                        severity: "warning",
                        node: node.clone(),
                        title: format!("no successful {kind} backup of {name} since it was due"),
                        detail: match last_ok {
                            Some(r) => format!("last success {}; was due {}", when(r.1), when(due)),
                            None => format!("never succeeded; first was due {}", when(due)),
                        },
                        since: due,
                    });
                }
            }
        }
        if let Some(v) = rows.iter().find(|r| !r.3.is_empty()).filter(|r| r.3.starts_with("failed")) {
            out.push(Alert {
                key: format!("verify:{node}:{name}"),
                severity: "critical",
                node: node.clone(),
                title: format!("a backup of {name} failed verification"),
                detail: v.3.clone(),
                since: v.1,
            });
        }
    }

    // WAL chains: reported by the backup listing logic.
    for gap in crate::backups::master::wal_gaps(state).await? {
        out.push(Alert {
            key: format!("wal_gap:{}:{}/{}", gap.0, gap.1, gap.2),
            severity: "critical",
            node: gap.0.clone(),
            title: format!("WAL chain of {}/{} is broken", gap.1, gap.2),
            detail: format!("segment {} is missing — the next base backup starts a new chain", gap.3),
            since: now,
        });
    }
    Ok(out)
}

/// Active alerts for the nodes this user can see.
pub async fn list(State(state): State<AppState>, user: CurrentUser) -> AppResult<Json<Vec<Alert>>> {
    let Some(fleet) = &state.fleet else { return Ok(Json(Vec::new())) };
    let mut v: Vec<Alert> = fleet
        .alerts
        .active
        .lock()
        .unwrap()
        .values()
        .filter(|a| a.node == "home" || user.can_node(&a.node))
        .cloned()
        .collect();
    v.sort_by(|a, b| (a.severity != "critical", a.since).cmp(&(b.severity != "critical", b.since)));
    Ok(Json(v))
}
