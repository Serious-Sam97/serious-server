use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub allowed_roots: Vec<PathBuf>,
    pub cookie_secure: bool,
    /// Public origin (e.g. "https://serious.example.com") used to validate
    /// the Origin header on WebSocket handshakes. Empty = same-host check only.
    pub public_origin: Option<String>,
    /// systemd units shown on the dashboard.
    pub services: Vec<String>,
}

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let home = env("HOME").unwrap_or_else(|| "/root".into());

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

        let data_dir = env("SS_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("{home}/.local/share/serious-server")));

        let allowed_roots: Vec<PathBuf> = env("SS_ALLOWED_ROOTS")
            .unwrap_or_else(|| format!("{home}/Development"))
            .split(':')
            .map(PathBuf::from)
            .collect();
        let allowed_roots = allowed_roots
            .iter()
            .map(|p| {
                p.canonicalize()
                    .map_err(|e| anyhow::anyhow!("allowed root {}: {e}", p.display()))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(!allowed_roots.is_empty(), "no allowed roots configured");

        Ok(Self {
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
        })
    }
}
