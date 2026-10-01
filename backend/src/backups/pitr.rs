//! Point-in-time recovery plumbing shared by the agent (restore on a
//! droplet) and the master (verification at home): bundle a base backup
//! with its WAL, and materialize a data directory from a bundle inside a
//! throwaway helper container.

use std::path::{Path, PathBuf};

use bollard::models::{ContainerCreateBody, HostConfig, Mount, MountType};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{exec_sh, exec_with_stdin};

// ---------------------------------------------------------------------------
// Minimal ustar writer (a bundle holds only regular files)
// ---------------------------------------------------------------------------

fn octal(field: &mut [u8], value: u64) {
    let s = format!("{:0width$o}\0", value, width = field.len() - 1);
    field.copy_from_slice(s.as_bytes());
}

fn tar_header(name: &str, size: u64, mtime: u64) -> anyhow::Result<[u8; 512]> {
    anyhow::ensure!(name.len() < 100, "tar name too long: {name}");
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut h[100..108], 0o644);
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], size);
    octal(&mut h[136..148], mtime);
    h[156] = b'0';
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    Ok(h)
}

/// Write `files` (name in archive, source path) as one tar at `dest`.
/// Returns (size, sha256).
pub async fn write_bundle(dest: &Path, files: &[(String, PathBuf)]) -> anyhow::Result<(u64, String)> {
    let mut out = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut put = async |bytes: &[u8], out: &mut tokio::fs::File| -> anyhow::Result<()> {
        hasher.update(bytes);
        total += bytes.len() as u64;
        out.write_all(bytes).await?;
        Ok(())
    };
    let mtime = crate::fleet::now_secs() as u64;
    let mut buf = vec![0u8; 256 * 1024];
    for (name, src) in files {
        let size = tokio::fs::metadata(src).await?.len();
        put(&tar_header(name, size, mtime)?, &mut out).await?;
        let mut f = tokio::fs::File::open(src).await?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            put(&buf[..n], &mut out).await?;
        }
        let pad = (512 - (size % 512) as usize) % 512;
        put(&vec![0u8; pad], &mut out).await?;
    }
    put(&[0u8; 1024], &mut out).await?;
    out.sync_all().await?;
    let sha = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    Ok((total, sha))
}

// ---------------------------------------------------------------------------
// Data directory layout and helper containers
// ---------------------------------------------------------------------------

/// Where a Postgres container keeps its data.
#[derive(Debug, Clone)]
pub struct Layout {
    /// PGDATA inside the database container.
    pub pgdata: String,
    /// The mount holding PGDATA: a volume name or a host path.
    pub source: String,
    pub bind: bool,
    /// PGDATA relative to the mount root ("" when mounted at PGDATA itself).
    pub rel: String,
}

pub async fn layout(docker: &bollard::Docker, container_id: &str) -> anyhow::Result<Layout> {
    let info = docker
        .inspect_container(container_id, None::<bollard::query_parameters::InspectContainerOptions>)
        .await?;
    let pgdata = info
        .config
        .as_ref()
        .and_then(|c| c.env.as_ref())
        .and_then(|env| env.iter().find_map(|kv| kv.strip_prefix("PGDATA=").map(String::from)))
        .unwrap_or_else(|| "/var/lib/postgresql/data".into());
    let mount = info
        .mounts
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            m.destination
                .as_deref()
                .is_some_and(|d| pgdata == d || pgdata.starts_with(&format!("{}/", d.trim_end_matches('/'))))
        })
        .max_by_key(|m| m.destination.as_ref().map(|d| d.len()).unwrap_or(0))
        .ok_or_else(|| anyhow::anyhow!("PGDATA {pgdata} is not on a volume or bind mount — refusing to restore into the container layer"))?;
    let dest = mount.destination.unwrap_or_default();
    let bind = mount.typ.as_ref().map(|t| t.to_string()).as_deref() == Some("bind");
    let source = if bind { mount.source.unwrap_or_default() } else { mount.name.unwrap_or_default() };
    anyhow::ensure!(!source.is_empty(), "cannot tell which volume holds PGDATA");
    let rel = pgdata[dest.trim_end_matches('/').len()..].trim_start_matches('/').to_string();
    Ok(Layout { pgdata, source, bind, rel })
}

fn mount(source: &str, bind: bool, target: &str) -> Mount {
    Mount {
        target: Some(target.into()),
        source: Some(source.into()),
        typ: Some(if bind { MountType::BIND } else { MountType::VOLUME }),
        ..Default::default()
    }
}

/// Start a throwaway container from `image` (root, sleeping) with `mounts`.
pub async fn helper(docker: &bollard::Docker, image: &str, mounts: Vec<Mount>) -> anyhow::Result<String> {
    let name = format!("serious-helper-{:x}", super::agent::random_id());
    let body = ContainerCreateBody {
        image: Some(image.into()),
        user: Some("0:0".into()),
        entrypoint: Some(vec!["sleep".into()]),
        cmd: Some(vec!["infinity".into()]),
        labels: Some([("serious.helper".to_string(), "true".to_string())].into()),
        host_config: Some(HostConfig {
            mounts: Some(mounts),
            network_mode: Some("none".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let opts = bollard::query_parameters::CreateContainerOptionsBuilder::default().name(&name).build();
    let created = docker.create_container(Some(opts), body).await?;
    docker
        .start_container(&created.id, None::<bollard::query_parameters::StartContainerOptions>)
        .await?;
    Ok(created.id)
}

pub async fn remove(docker: &bollard::Docker, id: &str) {
    let opts = bollard::query_parameters::RemoveContainerOptionsBuilder::default().force(true).build();
    let _ = docker.remove_container(id, Some(opts)).await;
}

/// Copy everything in a data mount into a fresh volume — the one-click
/// rollback kept before a restore overwrites anything.
pub async fn snapshot_to_volume(
    docker: &bollard::Docker,
    image: &str,
    layout: &Layout,
    volume: &str,
) -> anyhow::Result<()> {
    docker
        .create_volume(bollard::models::VolumeCreateRequest {
            name: Some(volume.into()),
            labels: Some([("serious.rollback".to_string(), "true".to_string())].into()),
            ..Default::default()
        })
        .await?;
    let h = helper(docker, image, vec![mount(&layout.source, layout.bind, "/src"), mount(volume, false, "/dst")]).await?;
    let r = exec_sh(docker, &h, "set -e; cp -a /src/. /dst/").await;
    remove(docker, &h).await;
    r.map(|_| ())
}

/// Copy a rollback volume back over the data mount.
pub async fn rollback_from_volume(
    docker: &bollard::Docker,
    image: &str,
    layout: &Layout,
    volume: &str,
) -> anyhow::Result<()> {
    let h = helper(docker, image, vec![mount(&layout.source, layout.bind, "/dst"), mount(volume, false, "/src")]).await?;
    let r = exec_sh(
        docker,
        &h,
        "set -e; find /dst -mindepth 1 -maxdepth 1 -exec rm -rf {} +; cp -a /src/. /dst/",
    )
    .await;
    remove(docker, &h).await;
    r.map(|_| ())
}

/// Replace the data directory in `layout`'s mount with the cluster in
/// `bundle` (base.tar.gz + wal/*.gz), set up to replay the WAL through
/// `restore_command` and promote — at `target_time` if given, else at the
/// end of the archive.
pub async fn materialize(
    docker: &bollard::Docker,
    image: &str,
    layout: &Layout,
    bundle: &Path,
    target_time: Option<i64>,
) -> anyhow::Result<()> {
    let h = helper(docker, image, vec![mount(&layout.source, layout.bind, "/restore")]).await?;
    let d = if layout.rel.is_empty() { "/restore".to_string() } else { format!("/restore/{}", layout.rel) };
    let target = match target_time {
        Some(t) => {
            let ts = time::OffsetDateTime::from_unix_timestamp(t)?
                .format(&time::macros::format_description!("[year]-[month]-[day] [hour]:[minute]:[second]+00"))?;
            format!("recovery_target_time = '{ts}'\nrecovery_target_inclusive = true\n")
        }
        None => String::new(),
    };
    let script = format!(
        r#"set -e
D="{d}"; S=/restore/.serious-stage
rm -rf "$S"; mkdir -p "$S"; tar xf - -C "$S"
mkdir -p "$D"
find "$D" -mindepth 1 -maxdepth 1 ! -name .serious-stage -exec rm -rf {{}} +
tar xzf "$S/base.tar.gz" -C "$D"
mkdir -p "$D/serious_wal"
if [ -d "$S/wal" ]; then for f in "$S"/wal/*; do [ -e "$f" ] && mv "$f" "$D/serious_wal/"; done; fi
rm -rf "$S"
cat >> "$D/postgresql.auto.conf" <<'EOF'
restore_command = 'gunzip -c {pgdata}/serious_wal/%f.gz > "%p"'
recovery_target_action = 'promote'
recovery_target_timeline = 'latest'
{target}EOF
touch "$D/recovery.signal"
chown -R postgres:postgres "$D"
chmod 700 "$D"
"#,
        pgdata = layout.pgdata,
    );
    let r = exec_with_stdin(docker, &h, &script, Some(bundle)).await;
    remove(docker, &h).await;
    r.map(|_| ())
}

/// After promotion: drop the recovery settings and the shipped WAL, so they
/// don't linger in future base backups.
pub async fn cleanup_after_recovery(docker: &bollard::Docker, container_id: &str, pgdata: &str) -> anyhow::Result<()> {
    for setting in ["restore_command", "recovery_target_action", "recovery_target_timeline", "recovery_target_time", "recovery_target_inclusive"] {
        super::psql(docker, container_id, &format!("ALTER SYSTEM RESET {setting}")).await?;
    }
    super::psql(docker, container_id, "SELECT pg_reload_conf()").await?;
    exec_sh(docker, container_id, &format!("rm -rf '{pgdata}/serious_wal'")).await?;
    Ok(())
}

async fn log_tail(docker: &bollard::Docker, container_id: &str) -> String {
    use futures_util::StreamExt;
    let opts = bollard::query_parameters::LogsOptionsBuilder::default()
        .stdout(true)
        .stderr(true)
        .tail("6")
        .build();
    let mut out = String::new();
    let mut logs = docker.logs(container_id, Some(opts));
    while let Some(Ok(chunk)) = logs.next().await {
        out.push_str(&String::from_utf8_lossy(&chunk.into_bytes()));
    }
    out.lines().filter(|l| l.contains("FATAL") || l.contains("PANIC") || l.contains("ERROR")).collect::<Vec<_>>().join(" | ")
}

/// Wait until the server accepts queries and has left recovery.
pub async fn wait_promoted(docker: &bollard::Docker, container_id: &str, timeout: std::time::Duration) -> anyhow::Result<()> {
    let end = std::time::Instant::now() + timeout;
    let mut last = String::new();
    while std::time::Instant::now() < end {
        // A startup failure (e.g. missing WAL) stops the container: say so now,
        // don't wait out the timeout with the database down.
        let state = docker
            .inspect_container(container_id, None::<bollard::query_parameters::InspectContainerOptions>)
            .await?
            .state;
        if state.as_ref().and_then(|s| s.running) == Some(false) {
            let code = state.and_then(|s| s.exit_code).unwrap_or(-1);
            anyhow::bail!("database stopped during recovery (exit {code}): {}", log_tail(docker, container_id).await);
        }
        match super::psql(docker, container_id, "SELECT pg_is_in_recovery()").await {
            Ok(v) if v == "f" => return Ok(()),
            Ok(v) => last = format!("still in recovery ({v})"),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    anyhow::bail!("database did not finish recovery in time: {last}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tar_header_checksum_and_size() {
        let h = tar_header("wal/000000010000000000000003.gz", 1234, 0).unwrap();
        assert_eq!(&h[124..136], b"00000002322\0");
        let stored = u32::from_str_radix(std::str::from_utf8(&h[148..154]).unwrap(), 8).unwrap();
        let mut copy = h;
        copy[148..156].copy_from_slice(b"        ");
        assert_eq!(stored, copy.iter().map(|b| *b as u32).sum::<u32>());
    }
}
