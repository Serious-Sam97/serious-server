use serde::Serialize;

use crate::db::Db;

#[derive(Debug, Serialize)]
pub struct AuditEntry {
    pub id: i64,
    pub ts: String,
    pub ip: String,
    pub actor: String,
    pub action: String,
    pub detail: String,
    pub ok: bool,
}

/// Fire-and-forget audit write; an audit failure must never take the
/// action itself down, but it should be loud in the logs.
pub fn audit(db: &Db, ip: &str, actor: &str, action: &str, detail: &str, ok: bool) {
    let db = db.clone();
    let (ip, actor, action, detail) = (
        ip.to_string(),
        actor.to_string(),
        action.to_string(),
        detail.to_string(),
    );
    tokio::spawn(async move {
        let res = db
            .call(move |c| {
                c.execute(
                    "INSERT INTO audit (ip, actor, action, detail, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params![ip, actor, action, detail, ok],
                )
            })
            .await;
        if let Err(e) = res {
            tracing::error!(error = %e, "audit write failed");
        }
    });
}

pub async fn recent(db: &Db, limit: u32, before: Option<i64>) -> anyhow::Result<Vec<AuditEntry>> {
    let limit = limit.min(500);
    db.call(move |c| {
        let mut stmt = c.prepare(
            "SELECT id, ts, ip, actor, action, detail, ok FROM audit
             WHERE (?1 IS NULL OR id < ?1)
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![before, limit], |r| {
            Ok(AuditEntry {
                id: r.get(0)?,
                ts: r.get(1)?,
                ip: r.get(2)?,
                actor: r.get(3)?,
                action: r.get(4)?,
                detail: r.get(5)?,
                ok: r.get(6)?,
            })
        })?;
        rows.collect()
    })
    .await
}
