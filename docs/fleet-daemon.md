# Running tole 24/7 on a minimal host (#1231)

The fleet host is a minimal container: no `systemctl`, no `crontab`, no `sudo`,
no `bwrap`. So the daemon is kept alive by three portable bash scripts in
[`scripts/fleet/`](../scripts/fleet) and an external scheduler that calls them.
Drop-in systemd units for hosts that do have systemd are in
[`templates/systemd/`](../templates/systemd); they are **not installed on the
fleet host**.

## Deploy

1. Install tole: `cargo install tole-cli` (the binary lands in `~/.cargo/bin/tole`).
2. Start the daemon (idempotent; a second call is a no-op):

   ```bash
   TOLE_BIN=$HOME/.cargo/bin/tole scripts/fleet/tole-serve-launcher start
   scripts/fleet/tole-serve-launcher status     # exit 0 running, 3 not running
   scripts/fleet/tole-serve-launcher stop
   ```

3. Have a scheduler call `scripts/fleet/tole-watchdog` (restart on death) and
   `scripts/fleet/tole-cron-tick` (one cron pass) every minute.

Extra `tole serve` flags go after `--`, e.g. `tole-serve-launcher start -- --transport mcp`.

| Variable | Default |
|---|---|
| `TOLE_BIN` | `tole` on `PATH` |
| `TOLE_FLEET_DIR` | `~/.local/state/tole-fleet` |
| `TOLE_FLEET_PORT` | `7801` |
| `TOLE_FLEET_BIND` | `127.0.0.1` |
| `TOLE_FLEET_SESSIONS` | `$TOLE_FLEET_DIR/sessions` |
| `TOLE_FLEET_TOKEN_FILE` | `$TOLE_FLEET_DIR/token` (created, mode 0600, if absent) |

## PID and log locations (all under `$TOLE_FLEET_DIR`)

| File | Content |
|---|---|
| `serve.pid` | daemon pid; liveness checks the pid *and* that its cmdline is `tole … serve`, so a recycled pid is not mistaken for the daemon |
| `serve.log` | daemon stdout/stderr |
| `serve.lock` | `flock` guard: `start` and `stop` run one at a time, so two simultaneous starts cannot orphan a daemon |
| `watchdog.log` | one line per watchdog run (`alive` / `restarted` / `FAILED`) |
| `cron-tick.log` | tick start/done lines plus the tick output |
| `cron-tick.lock` | `flock` guard; an overlapping tick exits 0 and logs `skipped`; without `flock(1)` the wrapper exits 127 instead of skipping |
| `token` | bearer token (forced to mode 0600 at every `start`, even if it already existed with looser rights; `$TOLE_FLEET_DIR` is mode 0700) |

The daemon is started with `setsid nohup`, so it survives the parent shell exiting.
`stop` sends SIGTERM (the daemon drains in-flight requests for up to 20 s,
`--shutdown-grace`), waits up to 30 s, then sends SIGKILL; the pid file is removed
only once the process is gone.

## Watchdog integration

**Without systemd** (the fleet host): schedule the two scripts from any external
scheduler, every minute. Hermes cron, cron(8) where present, or a plain loop:

```bash
* * * * * /path/to/scripts/fleet/tole-watchdog
* * * * * /path/to/scripts/fleet/tole-cron-tick
```

`tole-watchdog` is safe to call repeatedly: when the daemon is alive it only
logs `alive`. Exit 0 = alive or restarted, 1 = restart failed.

**With systemd**: use the templates instead of the watchdog (`Restart=on-failure`
does the restart).

```bash
cp templates/systemd/tole-* ~/.config/systemd/user/   # adjust the ExecStart paths
systemctl --user daemon-reload
systemctl --user enable --now tole-serve.service tole-cron-tick.timer
```

## Adding a `tole cron` job

Script jobs run an argv (no shell) and spend zero tokens. Use the same
sessions dir the wrapper uses:

```bash
S=${TOLE_FLEET_DIR:-$HOME/.local/state/tole-fleet}/sessions
tole cron --sessions-dir "$S" create '@every 30m' --id backup --script '/usr/local/bin/backup.sh'
tole cron --sessions-dir "$S" list
tole cron --sessions-dir "$S" runs
```

Script jobs run in the tick wrapper's cwd (`$TOLE_FLEET_DIR`), so use absolute paths and pass `--workspace` for the directory they may touch.
Jobs live in `$TOLE_CRON_DIR` (default `<codecora home>/tole/cron`). Agent jobs
that need Writes are refused by `tole cron tick`: parked until hosted by
`tole serve --cron` (`tole-serve-launcher start -- --cron`).

## Sandbox decision

There is no `bwrap` on this host, so tole prints an "unsandboxed verify"
warning. Decision: set `sandbox.consumers.verify = "off"` in the user config to
silence it. Rationale: the warning is accurate and cannot be acted on here, and
the container itself is the isolation boundary. Installing bwrap (and removing
that override) is **deferred** until the host image can provide it.

## Security note

- The daemon binds `127.0.0.1` only; do not set `TOLE_FLEET_BIND` to a public
  address without a TLS-terminating proxy in front.
- Every request needs the bearer token. The launcher passes it with
  `--token-file` (not `--token`, which would show in `ps` and `/proc/<pid>/cmdline`)
  and creates the file with mode 0600.
- The token is the only credential: keep `$TOLE_FLEET_DIR` private and never
  commit it.

```bash
curl -H "Authorization: Bearer $(cat $TOLE_FLEET_DIR/token)" http://127.0.0.1:7801/sessions
```
