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
  5/15/30 min charts. The server keeps 30 min of history and replays it on
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
