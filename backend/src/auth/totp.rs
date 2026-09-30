use std::time::{SystemTime, UNIX_EPOCH};

use totp_rs::{Algorithm, Secret, TOTP};

const STEP_SECONDS: u64 = 30;

pub fn generate_secret() -> String {
    Secret::generate_secret().to_encoded().to_string()
}

pub fn build(secret_b32: &str, account: &str) -> anyhow::Result<TOTP> {
    let secret = Secret::Encoded(secret_b32.to_string())
        .to_bytes()
        .map_err(|e| anyhow::anyhow!("bad totp secret: {e:?}"))?;
    TOTP::new(
        Algorithm::SHA1,
        6,
        1, // accept +/- one step of clock skew
        STEP_SECONDS,
        secret,
        Some("serious-server".to_string()),
        account.to_string(),
    )
    .map_err(|e| anyhow::anyhow!("totp: {e}"))
}

/// Current TOTP timestep — persisted after each successful check so a
/// captured code can't be replayed within its validity window.
pub fn current_step() -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs();
    (now / STEP_SECONDS) as i64
}

pub fn check_now(totp: &TOTP, code: &str) -> bool {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_secs();
    totp.check(code, now)
}
