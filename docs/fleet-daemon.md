# Running tole 24/7 on a minimal host (#1231)

The fleet host is a minimal container: no `systemctl`, no `crontab`, no `sudo`,
no `bwrap`. The scripts are **Linux only** (`/proc`, `flock`, `setsid`). The daemon is kept alive by three portable bash scripts in
[`scripts/fleet/`](../scripts/fleet) and an external scheduler that calls them.
Drop-in systemd units for hosts that do have systemd are in
[`templates/systemd/`](../templates/systemd); they are **not installed on the
fleet host**.

## Deploy

1. Install tole: `cargo install tole-cli` (the binary lands in `~/.cargo/bin/tole`).
   `cargo install` does not ship the scripts: clone this repository (or copy
   `scripts/fleet/`) and run them from there.
2. Start the daemon (idempotent; a second call is a no-op):

   ```bash
   TOLE_BIN=$HOME/.cargo/bin/tole scripts/fleet/tole-serve-launcher start
   scripts/fleet/tole-serve-launcher status     # exit 0 running, 3 not running
   scripts/fleet/tole-serve-launcher stop
   ```

3. Have a scheduler call `scripts/fleet/tole-watchdog` (restart on death) and
   `scripts/fleet/tole-cron-tick` (one cron pass) every minute.

Extra `tole serve` flags go after `--`, e.g. `tole-serve-launcher start -- --cron --transport mcp`.
They are saved to `$TOLE_FLEET_DIR/serve.args`: a later `start` without `--` (and so
the watchdog's restart) reuses them, and `start --` clears them.

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
| `serve.args` | the extra `tole serve` flags of the last `start -- ...`, one per line; reused by the watchdog |
| `disabled` | written by `stop`, cleared by `start`; while it exists the watchdog does not restart the daemon |
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
logs `alive`. Exit 0 = alive, restarted, or left down because of `stop`
(`disabled by stop` in the log), 1 = restart failed. A restart reuses the saved
`serve.args`, so `--cron` and `--transport mcp` survive a crash.

The scripts do not rotate their logs (`serve.log`, `watchdog.log`,
`cron-tick.log` only grow): rotate them with `logrotate` (`copytruncate`) or a
scheduled job.

**With systemd**: use the templates instead of the watchdog (`Restart=on-failure`
does the restart).

```bash
cp templates/systemd/tole-* ~/.config/systemd/user/   # adjust the ExecStart paths
systemctl --user daemon-reload
systemctl --user enable --now tole-serve.service tole-cron-tick.timer
loginctl enable-linger "$USER"   # else user units stop when the last login session ends
```

`tole-serve.service` creates the sessions dir and the token (mode 0600) in
`ExecStartPre` and sets a `PATH`. `tole-cron-tick.service` assumes this repository
is checked out at `~/tole`; after `cargo install` copy
`scripts/fleet/tole-cron-tick` somewhere and edit its `ExecStart`.

## Adding a `tole cron` job

Script jobs run an argv (no shell) and spend zero tokens. Jobs live in
`$TOLE_CRON_DIR` (default `<codecora home>/tole/cron`), not in a sessions dir:
`tole cron` does not use `--sessions-dir`. The session of an agent job is kept
under that job's `--workdir`.

```bash
tole cron create '@every 30m' --id backup --script '/usr/local/bin/backup.sh' --workdir /srv/backup
tole cron list
tole cron runs
```

`--workdir` is the directory a job runs in (and, with the cron sandbox on, the
directory it may write). A job without one runs in the tick wrapper's cwd, which
is `$TOLE_FLEET_DIR/cron-work`, an empty directory: `$TOLE_FLEET_DIR` itself
holds the token. Use absolute paths in the script. Agent jobs that need Writes
are refused by `tole cron tick`: parked until hosted by `tole serve --cron`
(`tole-serve-launcher start -- --cron`).

## Sandbox decision

There is no `bwrap` on this host, so tole prints an "unsandboxed verify"
warning. Decision: set `sandbox.consumers.verify = "off"` in the user config to
silence it. Rationale: the warning is accurate and cannot be acted on here, and
the container itself is the isolation boundary. Installing bwrap (and removing
that override) is **deferred** until the host image can provide it; the
sandbox work is tracked in [#748](https://github.com/codecoradev/tole/issues/748).

What this does and does not cover:

- The override is for the `verify` consumer only. The others (`run_command`,
  `job_start`, `turnend`, `cron`) have their own `[sandbox.consumers]` entries.
- Without a sandbox, a `verify` command, a `cron` script and anything else tole
  spawns run as the daemon's user and can read `$TOLE_FLEET_DIR/token` (and the
  rest of that user's files). The token is protected from other users, not from
  the commands the daemon runs. The environment of those commands is scrubbed,
  the files are not.
- `bwrap` needs unprivileged user namespaces (`kernel.unprivileged_userns_clone`
  or `user.max_user_namespaces` > 0, and no AppArmor restriction); a container
  that forbids them cannot run it even when the binary is installed.

## Security note

- The daemon binds `127.0.0.1` only; do not set `TOLE_FLEET_BIND` to a public
  address without a TLS-terminating proxy in front.
- Every request needs the bearer token. The launcher passes it with
  `--token-file` (not `--token`, which would show in `ps` and `/proc/<pid>/cmdline`)
  and creates the file with mode 0600.
- The token is the only credential: keep `$TOLE_FLEET_DIR` private and never
  commit it.

Do not put the token on a command line: `curl -H "Authorization: Bearer $(cat ...)"`
shows it in `ps` and `/proc/<pid>/cmdline` while curl runs. Feed it through a
curl config on stdin instead (`printf` is a shell builtin, so it is not in `ps`):

```bash
printf 'header = "Authorization: Bearer %s"\n' "$(cat "$TOLE_FLEET_DIR/token")" |
  curl -sS -K - http://127.0.0.1:7801/sessions
```

A header file (`curl -H @file`, mode 0600) works too.
