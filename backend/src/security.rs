use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Brute-force protection for the auth endpoints, replacing a generic
/// rate-limit layer with exactly the two policies we need:
///  - per-IP: max 5 attempts per minute (sliding window)
///  - global: 10 consecutive failures locks login for 15 minutes,
///    so a botnet rotating IPs still hits a wall.
pub struct LoginGuard {
    inner: Mutex<Inner>,
}

struct Inner {
    per_ip: HashMap<String, Vec<Instant>>,
    consecutive_failures: u32,
    locked_until: Option<Instant>,
}

const PER_IP_MAX: usize = 5;
const PER_IP_WINDOW: Duration = Duration::from_secs(60);
const GLOBAL_MAX_FAILURES: u32 = 10;
const GLOBAL_LOCKOUT: Duration = Duration::from_secs(15 * 60);

impl LoginGuard {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                per_ip: HashMap::new(),
                consecutive_failures: 0,
                locked_until: None,
            }),
        }
    }

    /// Call before processing an auth attempt. Err(()) = reject with 429.
    pub fn check(&self, ip: &str) -> Result<(), ()> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();

        if let Some(until) = inner.locked_until {
            if now < until {
                return Err(());
            }
            inner.locked_until = None;
            inner.consecutive_failures = 0;
        }

        // Drop stale windows so the map can't grow unbounded.
        inner
            .per_ip
            .retain(|_, hits| hits.iter().any(|t| now.duration_since(*t) < PER_IP_WINDOW));

        let hits = inner.per_ip.entry(ip.to_string()).or_default();
        hits.retain(|t| now.duration_since(*t) < PER_IP_WINDOW);
        if hits.len() >= PER_IP_MAX {
            return Err(());
        }
        hits.push(now);
        Ok(())
    }

    pub fn record_failure(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.consecutive_failures += 1;
        if inner.consecutive_failures >= GLOBAL_MAX_FAILURES {
            inner.locked_until = Some(Instant::now() + GLOBAL_LOCKOUT);
            tracing::warn!("login globally locked for 15 minutes after repeated failures");
        }
    }

    pub fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.consecutive_failures = 0;
        inner.locked_until = None;
    }
}
