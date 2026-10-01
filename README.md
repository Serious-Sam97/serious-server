# serious-server

Self-hosted server management webapp: a single Rust binary that serves a React
dashboard for the projects on this machine — Docker compose control, live logs,
a real terminal in the browser, an `.env`/config editor, and system monitoring.
Designed to sit behind a Cloudflare Tunnel with its own login + TOTP 2FA.

## Features

- **Projects** — scans `~/Development` for compose projects, shows container
  status, `compose up/down/restart/pull`, per-container start/stop/restart,
  live log streaming.
- **Terminal** — **admin-only** interactive shell (xterm.js + pty). A shell can
  `cd` anywhere and act as the service user, so it's reserved for the master
  admin. Max 5 concurrent sessions, 60 min idle timeout.
- **Git panel** — per-project safe git actions for normal users without a
  shell: status/branch/log, fetch, pull, push, stash/pop, and a full conflict
  flow (accept ours/theirs, edit in the file editor, complete or abort the
  merge). Fixed-argv only, path-validated, prompts disabled.
- **Files** — browse allowed roots, edit configs with CodeMirror, save
  confirmation + conflict detection (409 if the file changed on disk).
- **System** — a monitor page meant to stay open on a spare screen: CPU
  (per-core heatmap, temperature, load), memory + swap, network, disk I/O,
  disk usage, sensors, top processes, services and container health, with
  5/15/30 min charts. Processes and the full sensor list are only sampled
  while this page is open; otherwise the server reads just the cheap
  counters (~0.1 % of one core idle). The server keeps 30 min of history and replays it on
  connect, so a reload never starts from empty. Press `f` for focus mode
  (hides navigation); the tab title shows live CPU%. A red STALE banner
  appears if updates stop.
- **Keyboard-first UI** — `⌘K`/`Ctrl+K` command palette (`restart jellyfin`,
  `term immich`, `go system`), `1`–`6` switch tabs, and a live status line
  (cpu/mem/disk/net/tunnel) along the bottom of every page.
- **Users & permissions** — the master admin registers users and grants a
  per-project capability matrix (view / control / logs / files / git) plus a
  global System-monitor toggle. TOTP is mandatory for everyone.
- **Audit** — every login, container/compose/git action, file write, and
  terminal session is recorded in SQLite with the acting user, visible in the UI.

## Security model

- Binds **127.0.0.1 only** (enforced at startup) — cloudflared is the only way in.
- Username/password (argon2) **plus TOTP** (QR enrollment, replay-protected).
- First-run setup requires a one-time token printed to the server journal.
- Sessions: HttpOnly `__Host-` cookie, SameSite=Strict, 12 h idle / 7 d absolute.
- CSRF: custom `X-CSRF` header required on all mutations; WS handshakes
  validate `Origin`.
- Login rate limiting (5/min per IP) + global lockout (10 consecutive
  failures → 15 min).
- File/terminal access is jailed to `SS_ALLOWED_ROOTS` via canonicalize +
  prefix check (symlink escapes fail); writes into `.git` are refused.
- Recommended extra layer: put Cloudflare Access in front of the hostname.

## Build

```sh
make build        # vite build + cargo build --release
```

Produces `backend/target/release/serious-server` with the frontend embedded —
that one file is the whole deployment.

## First run

```sh
SS_COOKIE_SECURE=false ./backend/target/release/serious-server
```

The log prints `setup token: <hex>`. Open http://127.0.0.1:8420, follow
`/setup`: paste the token, choose credentials, scan the QR with your
authenticator app, confirm a code. Setup then closes permanently (delete
`~/.local/share/serious-server/serious.db` to re-enroll).

Note: `SS_COOKIE_SECURE=false` is only for plain-HTTP testing. Behind the
tunnel (HTTPS) leave it unset.

## Install as a service

```sh
make install
loginctl enable-linger sam   # once, so it survives logout/reboot
journalctl --user -u serious-server -f
```

## Expose via Cloudflare Tunnel

Add to your tunnel config (e.g. `/etc/cloudflared/config.yml`) above the
catch-all rule:

```yaml
ingress:
  - hostname: serious.your-domain.com
    service: http://localhost:8420
  # ...existing rules...
  - service: http_status:404
```

Then set `SS_PUBLIC_ORIGIN=https://serious.your-domain.com` in the systemd
unit (pins WebSocket origins) and restart. WebSockets work through cloudflared
out of the box.

## Configuration (env vars)

| Var | Default | Meaning |
|---|---|---|
| `SS_BIND` | `127.0.0.1:8420` | Listen address (must be loopback) |
| `SS_DATA_DIR` | `~/.local/share/serious-server` | SQLite DB location |
| `SS_ALLOWED_ROOTS` | `~/Development` | `:`-separated roots for files/terminal/projects |
| `SS_PUBLIC_ORIGIN` | _(unset)_ | Public origin for WS origin pinning |
| `SS_SERVICES` | `cloudflared,docker,jellyfin` | systemd units shown on the dashboard |
| `SS_COOKIE_SECURE` | `true` | Set `false` only for plain-HTTP dev |
| `SS_MODE` | `standalone` | `standalone`, `master` or `agent` (fleet role) |
| `SS_AGENT_HEADLESS` | `true` in agent mode | No HTTP listener; the agent is managed from the master |
| `SS_SAMPLE_SECS` | `2` (`5` for agents) | Metrics sampling period, 1–60 s |
| `SS_FLEET_BIND` | `127.0.0.1:8421` | Master: where agents connect (loopback; cloudflared publishes it) |
| `SS_CLICKHOUSE_URL` / `_USER` / `_PASSWORD` / `_DB` | _(unset)_ | Master: fleet metrics history (`http://host:port`) |
| `SS_MASTER_URL` | _(unset)_ | Agent: the master's fleet URL, e.g. `https://fleet.serious-sam.dev` |
| `SS_JOIN_TOKEN` | _(unset)_ | Agent: single-use enrollment token (first boot only) |
| `SS_CF_ACCESS_CLIENT_ID` / `_SECRET` | _(unset)_ | Agent: Cloudflare Access service token |
| `SS_BACKUP_DIR` | `<data>/backups` (`<data>/spool` on agents) | Backup store on the master / spool on agents |
| `SS_ALERT_NTFY_URL` | _(unset)_ | Master: ntfy topic URL for alerts |
| `SS_ALERT_WEBHOOK_URL` | _(unset)_ | Master: webhook receiving alert JSON |
| `SS_AGENT_ALLOW` | `system,projects,logs,git,files,backups` | Agent: what the master may do here (`terminal` is opt-in) |

## Docker

Run the whole app as a container instead of a systemd service:

```sh
make docker                        # build + up, port published to 127.0.0.1:8420 only
docker compose logs serious-server # first boot prints the setup token here
```

The container mounts the docker socket and `/home/samiraniz/Developer` at the
same path as the host, so compose projects behave identically. It runs as
uid 1000 with the host docker group added. Differences vs the systemd
install: service badges are empty (no systemctl inside the container) and
the browser terminal is a bash shell inside the container (with your
projects mounted), not a host shell.

## Fleet (master + droplet agents)

The home server runs as the **master** (`SS_MODE=master`, see
`docker-compose.yml`); each droplet runs a headless **agent**
(`deploy/agent/docker-compose.yml`). Agents dial out to the master — no
inbound ports on the droplets — and keep one WebSocket open for metrics,
docker events, and the API tunnel.

- **Fleet overview** (`/fleet`, the landing page): every node with live
  CPU/mem/disk/load, 24 h CPU from ClickHouse, recent container events.
- **Environment selector** in the header: `/n/<node>/…` shows the same
  Projects / System / Files / Audit pages for that droplet. The status line
  takes the node's colour, and every state change on a droplet asks for
  confirmation naming the node and its environment.
- **Enrollment**: Fleet page → "add a node" gives a single-use join token
  (15 min). The agent swaps it for its own 256-bit secret on first connect
  (only a SHA-256 is stored on the master). "revoke" drops the link at once.
- **What the master may do** on a droplet is decided *on the droplet*:
  `SS_AGENT_ALLOW` (default `system,projects,logs,git,files,backups`;
  `terminal` is opt-in).
- **Permissions**: admins see every node; other users need the node ticked
  on the Users page, and project permissions then apply by project name.

Cloudflare (home account): route `fleet.serious-sam.dev` →
`http://localhost:8421` and protect it with a Cloudflare Access policy that
only accepts a **service token**; agents send it via
`SS_CF_ACCESS_CLIENT_ID/SECRET`. The droplets' own Cloudflare account is not
involved.

Idle cost measured on this machine: agent ~0.05 % of one core and ~11 MB RSS
with the link up; serious-clickhouse ~0.7 %, ~150 MB.

## Database backups

Postgres containers in compose projects are detected automatically (by
image; `serious.backup=false` opts out, `serious.backup.engine=postgres` opts a
custom image in). Each gets a **Backups** panel on its project page. All
backups are stored on the master (`SS_BACKUP_DIR`, default `<data>/backups`);
a droplet only spools a file until the master confirms a verified copy.

- **Policy per database**, set in the UI: cron schedule (+ UTC offset),
  optional catch-up window, retention (last N + daily/weekly/monthly), dump
  rate cap, and automatic verification.
- **`logical` mode**: `pg_dump -Fc` run inside the database container (same
  version as the server, credentials from the container's own env, `nice 19`),
  streamed to disk with a sha256 on the way. "backup now" + download = export.
- **`continuous` mode** (droplets): `pg_receivewal` streams WAL through a
  replication slot capped at 10 GB (`max_slot_wal_keep_size`, so an outage
  can't fill the droplet's disk); finished segments ship as they close, and a
  segment switch every 5 min when there were writes keeps the data-loss
  window ≤ 5 min. The schedule takes `pg_basebackup` base backups. A lost slot
  is detected and a new chain starts with a fresh base backup.
- **Restore** (admin, type `project/service` to confirm): logical restores
  take a safety dump first; point-in-time restores stop the database, copy
  its current data to a `serious-rollback-…` volume, rebuild from the nearest
  base backup, replay WAL to the chosen moment, and roll back automatically
  if anything fails.
- **Verification**: each scheduled backup is restored into a throwaway
  Postgres (same image, no network) on the master and checked (table count +
  `pg_amcheck`). The result shows next to the backup.
- Transfers ride the fleet WebSocket as 512 KiB binary frames with acks —
  resumable from the last byte the master stored.

## Alerts

The master evaluates once a minute: node offline > 5 min, disk ≥ 90 %,
backup failed, backup overdue (1 h past its schedule), verification failed,
broken WAL chain. Active alerts show on the Fleet page; new and resolved
ones are sent to `SS_ALERT_NTFY_URL` (an ntfy topic) and/or
`SS_ALERT_WEBHOOK_URL` (JSON).

## Development

```sh
make dev          # native: cargo run on :8420 + vite dev on :5173 (proxies /api)
make docker-dev   # dockerized hot reload: cargo-watch backend + vite HMR
```

`make docker-dev` needs no local Rust or Node — edit `backend/src` or
`frontend/src` on the host and the containers rebuild/HMR automatically.
Open http://127.0.0.1:5173. First backend start is slow while cargo-watch
installs into the cached volume. Dev caveats are listed at the top of
`docker-compose.dev.yml`.
