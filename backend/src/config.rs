use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// What this process is in the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Single machine, no fleet (the default, and what exists today).
    Standalone,
    /// Home server: the dashboard, and later the hub every agent reports to.
    Master,
    /// Droplet: reports to a master and obeys it; usually headless.
    Agent,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub mode: Mode,
    /// No HTTP listener at all — agents are managed from the master only.
    pub headless: bool,
    /// Lite metrics sampling period.
    pub sample_interval: Duration,
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub cookie_secure: bool,
    /// Public origin (e.g. "https://serious.example.com") used to validate
    /// the Origin header on WebSocket handshakes. Empty = same-host check only.
    pub public_origin: Option<String>,
    /// systemd units shown on the dashboard.
    pub services: Vec<String>,
    /// Master only: where agents connect (reached through its own tunnel hostname).
    pub fleet_bind: SocketAddr,
    /// Agent only: the master's fleet URL (`https://fleet.example.com`, or `ws://…` in tests).
    pub master_url: Option<String>,
    /// Agent only: one-time join token, used on first boot when no credentials exist yet.
    pub join_token: Option<String>,
    /// Agent only: Cloudflare Access service token (client id, client secret).
    pub cf_access: Option<(String, String)>,
    /// Agent only: what the master may do here, e.g. `system,projects,logs,git,files,backups`.
    pub agent_allow: Vec<String>,
    /// Master only: ClickHouse HTTP interface for fleet metrics history.
    pub clickhouse: Option<ClickhouseConfig>,
    /// Where backups are stored (master/standalone) or spooled (agent).
    pub backup_dir: PathBuf,
    /// Master: ntfy topic URL for alerts (e.g. https://ntfy.sh/my-topic).
    pub alert_ntfy_url: Option<String>,
    /// Master: webhook that receives alert JSON.
    pub alert_webhook_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClickhouseConfig {
    /// `http://host:port` (plain HTTP; ClickHouse runs next to the master).
    pub url: String,
    pub user: String,
    pub password: String,
    pub database: String,
}

/// Capabilities an agent grants the master unless SS_AGENT_ALLOW says otherwise.
/// `terminal` is deliberately absent: a shell on every droplet reachable from
/// one place is the highest-value target if the master were compromised.
pub const DEFAULT_AGENT_ALLOW: &str = "system,projects,logs,git,files,backups";
pub const AGENT_CAPS: [&str; 7] = ["system", "projects", "logs", "git", "files", "backups", "terminal"];

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let home = env("HOME").unwrap_or_else(|| "/root".into());

        let mode = match env("SS_MODE").as_deref() {
            None | Some("standalone") => Mode::Standalone,
            Some("master") => Mode::Master,
            Some("agent") => Mode::Agent,
            Some(other) => anyhow::bail!("SS_MODE must be standalone, master or agent, got {other:?}"),
        };
        let headless = match env("SS_AGENT_HEADLESS").as_deref() {
            None => mode == Mode::Agent,
            Some("true") => true,
            Some("false") => false,
            Some(other) => anyhow::bail!("SS_AGENT_HEADLESS must be true or false, got {other:?}"),
        };
        anyhow::ensure!(
            !headless || mode == Mode::Agent,
            "SS_AGENT_HEADLESS=true only makes sense with SS_MODE=agent"
        );
        // Agents default to a slower tick: nobody watches them live most of
        // the time, and every sample is shipped over the network.
        let default_secs = if mode == Mode::Agent { 5 } else { 2 };
        let sample_secs: u64 = match env("SS_SAMPLE_SECS") {
            Some(v) => v
                .parse()
                .map_err(|_| anyhow::anyhow!("SS_SAMPLE_SECS must be a whole number, got {v:?}"))?,
            None => default_secs,
        };
        anyhow::ensure!(
            (1..=60).contains(&sample_secs),
            "SS_SAMPLE_SECS must be between 1 and 60, got {sample_secs}"
        );

        let bind: SocketAddr = env("SS_BIND")
            .unwrap_or_else(|| "127.0.0.1:8420".into())
            .parse()?;
        // Hard requirement: never expose the raw HTTP port. cloudflared talks
        // to localhost; anything else is a config mistake. The only sanctioned
        // exception is inside a container, where the bind must be 0.0.0.0 and
        // the compose file publishes it to host loopback instead.
        let allow_public = env("SS_ALLOW_PUBLIC_BIND").is_some_and(|v| v == "true");
        anyhow::ensure!(
            bind.ip().is_loopback() || allow_public,
            "SS_BIND must be a loopback address, got {bind} \
             (set SS_ALLOW_PUBLIC_BIND=true only inside a container whose port \
             is published to 127.0.0.1)"
        );

        let fleet_bind: SocketAddr = env("SS_FLEET_BIND")
            .unwrap_or_else(|| "127.0.0.1:8421".into())
            .parse()?;
        anyhow::ensure!(
            fleet_bind.ip().is_loopback() || allow_public,
            "SS_FLEET_BIND must be a loopback address, got {fleet_bind} \
             (cloudflared publishes it; SS_ALLOW_PUBLIC_BIND=true only inside a container)"
        );

        let agent_allow: Vec<String> = env("SS_AGENT_ALLOW")
            .unwrap_or_else(|| DEFAULT_AGENT_ALLOW.into())
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if let Some(bad) = agent_allow.iter().find(|c| !AGENT_CAPS.contains(&c.as_str())) {
            anyhow::bail!("SS_AGENT_ALLOW: unknown capability {bad:?} (known: {})", AGENT_CAPS.join(","));
        }
        let cf_access = match (env("SS_CF_ACCESS_CLIENT_ID"), env("SS_CF_ACCESS_CLIENT_SECRET")) {
            (Some(id), Some(secret)) => Some((id, secret)),
            (None, None) => None,
            _ => anyhow::bail!("set both SS_CF_ACCESS_CLIENT_ID and SS_CF_ACCESS_CLIENT_SECRET, or neither"),
        };
        let clickhouse = env("SS_CLICKHOUSE_URL").map(|url| ClickhouseConfig {
            url: url.trim_end_matches('/').to_string(),
            user: env("SS_CLICKHOUSE_USER").unwrap_or_else(|| "default".into()),
            password: env("SS_CLICKHOUSE_PASSWORD").unwrap_or_default(),
            database: env("SS_CLICKHOUSE_DB").unwrap_or_else(|| "serious".into()),
        });
        if let Some(ch) = &clickhouse {
            anyhow::ensure!(
                ch.url.starts_with("http://"),
                "SS_CLICKHOUSE_URL must be plain http://host:port, got {}",
                ch.url
            );
        }

        let data_dir = env("SS_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{home}/.local/share/serious-server")));

        let allowed_roots: Vec<PathBuf> = env("SS_ALLOWED_ROOTS")
            .unwrap_or_else(|| format!("{home}/Development"))
            .split(':')
            .map(PathBuf::from)
            .collect();
        // A typo in one root must not crash-loop the server (an agent would
        // vanish from the fleet): skip it loudly, fail only if none is left.
        let allowed_roots: Vec<PathBuf> = allowed_roots
            .iter()
            .filter_map(|p| match p.canonicalize() {
                Ok(c) => Some(c),
                Err(e) => {
                    tracing::warn!("allowed root {} skipped: {e}", p.display());
                    None
                }
            })
            .collect();
        anyhow::ensure!(!allowed_roots.is_empty(), "none of the allowed roots (SS_ALLOWED_ROOTS) exist");
        let backup_dir = env("SS_BACKUP_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join(if mode == Mode::Agent { "spool" } else { "backups" }));

        Ok(Self {
            mode,
            headless,
            sample_interval: Duration::from_secs(sample_secs),
            bind,
            data_dir,
            allowed_roots,
            cookie_secure: env("SS_COOKIE_SECURE").map(|v| v != "false").unwrap_or(true),
            public_origin: env("SS_PUBLIC_ORIGIN").map(|o| o.trim_end_matches('/').to_string()),
            // Explicitly empty ("SS_SERVICES=") means no service badges — used
            // in containers where systemctl isn't reachable.
            services: std::env::var("SS_SERVICES")
                .unwrap_or_else(|_| "cloudflared,docker,jellyfin".into())
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            fleet_bind,
            master_url: env("SS_MASTER_URL"),
            join_token: env("SS_JOIN_TOKEN"),
            cf_access,
            agent_allow,
            clickhouse,
            backup_dir,
            alert_ntfy_url: env("SS_ALERT_NTFY_URL"),
            alert_webhook_url: env("SS_ALERT_WEBHOOK_URL"),
        })
    }
}
