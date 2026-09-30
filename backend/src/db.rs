use std::path::Path;
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

/// Thin async wrapper around a single rusqlite connection. Write volume is
/// tiny (auth events, audit rows), so one mutex-guarded connection moved onto
/// the blocking pool is plenty.
#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    username TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    totp_secret TEXT NOT NULL,
    totp_confirmed INTEGER NOT NULL DEFAULT 0,
    totp_last_step INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY,
    ts TEXT NOT NULL DEFAULT (datetime('now')),
    ip TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL,
    ok INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS audit_ts ON audit(ts);
"#;

/// The master admin is unique and immutable, guaranteed by the database
/// itself: at most one row may hold role='admin', and that row can be
/// neither deleted nor demoted. App-level checks exist only for friendlier
/// error messages.
const ADMIN_CONSTRAINTS: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS users_single_admin
  ON users(role) WHERE role = 'admin';
CREATE TRIGGER IF NOT EXISTS users_no_admin_delete
BEFORE DELETE ON users WHEN OLD.role = 'admin' AND OLD.totp_confirmed = 1
BEGIN SELECT RAISE(ABORT, 'admin cannot be deleted'); END;
CREATE TRIGGER IF NOT EXISTS users_no_admin_demote
BEFORE UPDATE OF role ON users
WHEN OLD.role = 'admin' AND OLD.totp_confirmed = 1 AND NEW.role <> 'admin'
BEGIN SELECT RAISE(ABORT, 'admin cannot be demoted'); END;
"#;

fn table_columns(conn: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    conn.prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect()
}

/// Idempotent, framework-free migration. Order matters: add columns, then
/// promote the pre-multi-user admin, then install the admin constraints.
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let cols = table_columns(conn, "users")?;
    let adds: &[(&str, &str)] = &[
        (
            "role",
            "ALTER TABLE users ADD COLUMN role TEXT NOT NULL DEFAULT 'user'",
        ),
        (
            "must_change_password",
            "ALTER TABLE users ADD COLUMN must_change_password INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "permissions",
            "ALTER TABLE users ADD COLUMN permissions TEXT NOT NULL DEFAULT '{}'",
        ),
    ];
    for (name, ddl) in adds {
        if !cols.iter().any(|c| c == name) {
            conn.execute(ddl, [])?;
        }
    }

    // Databases from the single-admin era: the enrolled user becomes admin.
    conn.execute(
        "UPDATE users SET role = 'admin'
         WHERE id = (SELECT id FROM users WHERE totp_confirmed = 1 ORDER BY id LIMIT 1)
           AND NOT EXISTS (SELECT 1 FROM users WHERE role = 'admin')",
        [],
    )?;
    conn.execute_batch(ADMIN_CONSTRAINTS)?;

    let audit_cols = table_columns(conn, "audit")?;
    if !audit_cols.iter().any(|c| c == "actor") {
        conn.execute("ALTER TABLE audit ADD COLUMN actor TEXT NOT NULL DEFAULT ''", [])?;
    }
    Ok(())
}

impl Db {
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let db_path = data_dir.join("serious.db");
        let conn = Connection::open(&db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;

        // The DB holds the password hash and TOTP secret — owner-only.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&db_path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// Run a closure against the connection on the blocking pool.
    pub async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let guard = conn.lock().expect("db mutex poisoned");
            f(&guard).map_err(anyhow::Error::from)
        })
        .await?
    }
}
