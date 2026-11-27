# tole serve in a container, behind a TLS proxy (#1044)

A recipe for running `tole serve` on a VPS so a client on another machine (a
desktop, a phone, a script) can use it with the live event stream. It is a
**deployment recipe, not a new capability**: no route, flag or default of tole
changes. What is here:

| File | What it is |
|---|---|
| [`Dockerfile`](../Dockerfile) | Multi-stage build of the release binary of `tole-cli` (default features), a Debian slim runtime, a non-root user (uid 10001), `tole serve` as the command, a `HEALTHCHECK` on `GET /ready` |
| [`.dockerignore`](../.dockerignore) | The build context is `Cargo.toml`, `Cargo.lock`, `crates/` and `scripts/container/` only |
| [`scripts/container/`](../scripts/container) | `docker-entrypoint.sh` (reads a secret file into `TOLE_API_KEY`), `healthcheck.sh`, `stream_probe.py` (the streaming and resume test client) |
| [`templates/container/compose.yaml`](../templates/container/compose.yaml) | tole plus one Caddy proxy; only the proxy publishes ports |
| [`templates/container/Caddyfile`](../templates/container/Caddyfile), [`nginx.conf`](../templates/container/nginx.conf) | The proxy settings that keep the stream unbuffered |

Status: written and reviewed against the code and the docs, **not yet run
end to end**. The "What was and was not verified" section lists exactly what
was exercised. No image is published yet (the issue asks for publishing from the
release workflow; that is a follow-up, see the end).

## Quick start

```bash
cd templates/container
umask 077
mkdir -p secrets
openssl rand -hex 32 > secrets/tole_serve_token       # the serve credential
printf '%s' "$PROVIDER_KEY" > secrets/tole_api_key     # the model provider key
sudo chown 10001:10001 secrets/* && sudo chmod 0400 secrets/*   # required, see below
cp .env.example .env && $EDITOR .env                   # TOLE_DOMAIN, TOLE_BASE_URL, TOLE_MODEL
docker compose up --build -d
docker compose ps                                      # tole: healthy
curl https://$TOLE_DOMAIN/health                       # {"protocol":1,"status":"ok","version":"..."}
```

**The `chown` is required.** Compose (without swarm) bind-mounts a file secret
exactly as it is on the host and ignores the `uid`, `gid` and `mode` of the
compose file, so a `0600` file owned by you is unreadable by the container user
(uid 10001) and serve stops at start with `--token-file ...: Permission
denied`. The host files must be owned by 10001 (mode `0400`) or be readable by
others; owner 10001 with `0400` is the narrow choice. You then read the token on
the host with `sudo`; keep a copy of it where your clients need it (a password
manager, the desktop's credential store), not next to the stack.

`TOLE_BASE_URL`, `TOLE_MODEL` and the key are used together: tole has no
built-in provider default, and with a blank `TOLE_BASE_URL` or `TOLE_MODEL` the
provider has to come from the user config in `/data/codecora/tole/config.toml`.

`TOLE_DOMAIN` is a public name that points at the VPS (Caddy fetches and renews
its certificate; ports 80 and 443 must reach it). With `localhost` Caddy uses
its own local CA, which your client does not trust until you install that root
or pass it (`stream_probe.py --cafile`, `curl --cacert`).

Keep `secrets/` and `.env` out of version control (the repository `.gitignore`
lists both).

## The image

- **Builder.** BuildKit is required (`RUN --mount=type=cache` and
  `COPY --chmod`): the default of Docker 23 and later and of `docker buildx`;
  older engines need `DOCKER_BUILDKIT=1`.
- **Binary.** `cargo build --release --locked -p tole-cli`, stripped, from the
  committed `Cargo.lock`. Base images are pinned by tag **and** digest (the
  multi-arch index digest; bump both together). Build on the architecture you
  run.
- **User.** uid and gid 10001, no login shell, `USER` set. The compose file also
  drops all capabilities, sets `no-new-privileges`, a read-only root filesystem
  (`/tmp` is a tmpfs) and a process cap.
- **Listener.** `tole serve --bind 0.0.0.0 --port 7801`, plain HTTP, REST
  transport. `0.0.0.0` is only inside the container; the compose file publishes
  nothing for this service. `--socket` is not used: it needs a `0700` directory
  owned by the serving user, which a proxy in another container cannot enter.
  `--transport mcp` is not the default; override the command to use it.
- **Token.** `--token-file /run/secrets/tole_serve_token`: a Docker secret, never an argument
  (visible in `ps` and `docker inspect`). The token is trimmed of surrounding
  whitespace; an empty file stops serve. `tole serve` refuses to start without a
  credential.
- **Provider key.** tole reads it from the environment only. The entrypoint
  copies the file named by `TOLE_API_KEY_FILE` (and `TOLE_FALLBACK_API_KEY_FILE`)
  into `TOLE_API_KEY` and then execs tole. The value is in neither the compose
  file, the image nor `docker inspect`. tole marks itself non-dumpable at start
  and scrubs secret-shaped variables from every command it spawns
  (`docs/threat-model.md`, ENV-1 and #675).
- **State.** Two volumes; nothing is written into the image layer.

  | Path | Holds |
  |---|---|
  | `/data/codecora` (`CODECORA_HOME`) | user config `tole/config.toml` (profiles, persona files, `[sandbox]`, webhooks), cron store, discussion logs, trust records, skills |
  | `/data/sessions` (`--sessions-dir`) | the session ledgers (append-only JSONL and their lock files) |
  | `/data/home` (`HOME`) | the account home (for example a `~/.gitconfig`) |
  | `/workspace` (`--workspace`) | the folder serve is jailed to and the tools work in; a client's `cwd` must be inside it |

  The ledgers are kept outside the workspace so the model's file tools cannot
  reach them (they are jailed to `/workspace`). Commands the model runs are not
  confined that way, see "What the container does not add". Files are created
  `0600` and directories `0700`: the entrypoint sets `umask 077` and tole
  inherits it, so files the model's tools create in `/workspace` get the same
  modes. That matters on a bind-mounted workspace a person on the host also
  edits; use a named volume or change the `umask` line in the entrypoint.
- **Health.** `HEALTHCHECK` runs `healthcheck.sh` (bash `/dev/tcp`, no curl in
  the image): `GET /ready` on the container's loopback must answer `200`: the
  state directories (`/data/codecora`, `/data/sessions`) take a write, and with
  `--cron` the ticker beats. It answers `503` when one does not (a read-only or full
  volume shows up here) and while serve drains after SIGTERM. `GET /health` is the
  liveness answer (the process is up) for a probe that must not fail on a full disk;
  a Kubernetes readiness probe uses `/ready`, a liveness probe `/health`. Neither
  takes a token (mobile-control.md, "`GET /ready`").
- **Stop.** SIGTERM drains for `--shutdown-grace` (default 20 s; running turns
  are detached and stay resumable, see mobile-control.md "Shutdown"). The
  compose file sets `stop_grace_period: 30s`, above the grace; Docker's own
  default of 10 s would kill a drain in progress.
- **Cron.** Not on by default. To host the scheduler in the container, override
  the command with the same arguments plus `--cron`. A bind-mounted workspace
  must be writable by uid 10001 (or run the service with `user:` set to the
  owner's uid and gid).

Extra tools a workspace needs (a language toolchain, `gh`) are not in the image:
build your own `FROM` it and `apt-get install` what you need.

## Persistence and upgrades

The ledgers, user config, cron store and the workspace are on the `tole-data`
and `tole-workspace` volumes, so removing or replacing the container keeps
them. An upgrade is `docker compose up --build -d`: compose stops the old
container (drain, see above) and starts the new one on the same volumes; a turn
that was running is detached and resumes with the next prompt
(`POST /sessions {"resume": id}`, mobile-control.md). The ledgers are append-only
and a newer binary reads them as `tole resume` does; read the CHANGELOG for the
version step first. Do not run two servers on the same
`/data/sessions` (each session file has a writer lock; a second writer gets
`409`). To back up, copy the volume while the service is stopped:
`docker run --rm -v tole_tole-data:/data -v "$PWD":/b debian:bookworm-slim tar czf /b/tole-data.tgz -C /data .`
(the volume name is prefixed with the compose project name).

## TLS and the live event stream

tole serves plain HTTP; TLS ends at the proxy the owner runs (decision D6 of
[desktop-design.md](desktop-design.md) requires `https://` for a non-loopback
server; D5, that reach is the owner's own means and tole ships none, is still
awaiting the owner's confirmation and is what this recipe assumes), or the owner uses a private network
(WireGuard, an SSH tunnel) and no public proxy at all.

The stream is `GET /sessions/{id}/events?after=<seq>`: Server-Sent Events
(`text/event-stream`, HTTP/1.1, the response is ended by closing the connection,
not by a length). tole sends `Cache-Control: no-cache` and
`X-Accel-Buffering: no`, and a `: ping` comment after 15 s without a write
(`TOLE_SERVE_HEARTBEAT_SECS`, 1 to 3600). There is no WebSocket. What the proxy
must do:

| Setting | Why | Caddy | nginx |
|---|---|---|---|
| Do not buffer the response | A buffer holds `text_delta` frames until it fills or the turn ends | `flush_interval -1` | `proxy_buffering off` |
| Do not compress `text/event-stream` | A compressor waits for a full block | no `encode` block | `gzip off` |
| Read timeout above the heartbeat | An idle stream carries only a ping every 15 s | none by default | `proxy_read_timeout` (the template: 3600 s) |
| No short read timeout on `POST .../prompt` | It answers when the **turn ends**, which can take minutes, with no byte before that | none by default | `proxy_read_timeout`: the template's 3600 s is a choice, a longer turn is cut |
| Send request bodies with a `Content-Length` | tole refuses chunked request bodies: any `Transfer-Encoding` header is a `400` (`serve.rs`, request smuggling guard) | `request_buffers 12MB` (reads the body, then sends it with a length; from Caddy's documentation, not run here) | the default request buffering does it; **never** `proxy_request_buffering off` |
| HTTP/1.1 to the upstream | tole speaks HTTP/1.1 | `transport http { versions 1.1 }` | `proxy_http_version 1.1` + `Connection ""` |
| Body cap above 10 MiB | An authenticated image upload is accepted up to 10 MiB | `request_body max_size 12MB` | `client_max_body_size 12m` |

A proxy with an idle timeout shorter than 15 s needs a lower
`TOLE_SERVE_HEARTBEAT_SECS` (the compose file passes it through).

A dropped connection loses nothing durable: reconnect with
`?after=<last seq seen>` (or the `Last-Event-ID` header) and the missed `entry`
frames are replayed, once each, then `ready`. Ephemeral frames (`text_delta`,
`reasoning`, tool start and finish) are not replayed; the final answer is always
an `entry`. After a client-side break of `POST .../prompt`, read `GET /entries`
and `busy` before sending again (mobile-control.md and desktop-design.md 3.4).
At most 8 streams are open per server, shared by all clients.

### Checking it: `stream_probe.py`

```bash
python3 scripts/container/stream_probe.py https://$TOLE_DOMAIN \
    --token-file templates/container/secrets/tole_serve_token \
    --cwd /workspace [--cafile caddy-root.pem | --insecure]
```

It starts a session, opens the stream, sends a prompt, and prints the time from
sending the prompt to the first `text_delta` frame, the number of frames, and
the gap between them (a buffering proxy shows one burst at the end of the turn,
and the probe says so and fails). Then it cuts the stream in the middle of a
second turn, reopens it with `after=<last seq>` and compares what arrives with
`GET /entries`. A stream silent for more than `--stream-timeout` (default 120 s) counts as broken:
raise it if you set `TOLE_SERVE_HEARTBEAT_SECS` above 120. It exits 0 when both
checks pass, 1 when one fails (including a prompt that did not succeed) and 2
when it could not decide (the turn ended before the cut, or the log had nothing
to resume). It spends one prompt per phase on a real provider. Run it
against serve directly and through the proxy and compare the first-frame time;
to measure first token against first frame exactly, point `TOLE_BASE_URL` at a
mock provider that logs when it emitted its first token.

## Threat model for a remote serve

Read [threat-model.md](threat-model.md) ("Remote approver trust boundary
(#200)", "Event streams (#524)", "Phone as approval surface (#202)", "Remote
serve in a container (#1044)") and [desktop-design.md](desktop-design.md) 3.2 and 3.3 first. In short:

- **The serve token is approval authority.** Whoever holds it can read every
  transcript, start turns and approve Writes on the box. There are no scopes, no
  per-device tokens and no revocation short of changing the secret and
  restarting. Treat it like an SSH key: `openssl rand -hex 32`, a file, not in a
  chat or a shell history, rotate on any doubt.
- **Public exposure invites a lock-out.** The failed-auth limiter (10 a minute)
  counts per source address, and behind the proxy that address is the proxy's:
  tole does not read `X-Forwarded-For`. Anyone on the internet who sends ten
  wrong tokens in a minute makes every request, the owner's included, answer
  `429` until the window passes. A 256-bit token makes guessing hopeless but not
  the lock-out. Prefer a private network (WireGuard, Tailscale-like, an SSH
  tunnel) or restrict the proxy to the addresses you use; the nginx template
  adds a request-rate limit, which only slows a flood. Do this at the proxy; tole
  has nothing to restrict it by.
- **`/health`, `/ready` and the webhook routes need no token.** `/health` tells anyone the exact tole version; `/ready` tells
  anyone whether the state directory, the ticker and the sender work (three booleans, no path). Webhook routes keep
  their own HMAC gate; a route configured `insecure_loopback` is refused by
  serve on a non-loopback bind, which `0.0.0.0` is.
- **HTTP between the proxy and tole.** The traffic on the compose network is
  unencrypted, and any other container on that network can reach `tole:7801`
  with a token it learned. Keep the network to these two services.
- **`--auth basic`** works in the image (override the command) but a non-loopback
  bind needs `--allow-insecure-basic`, which says only that TLS ends in front;
  it does not check that it does.
- **Rate limits, request caps, the jail.** Unchanged and in force: 32
  connections, 8 streams, a 1 MiB body before authentication, a client `cwd`
  must resolve inside `--workspace`.

### What the container adds

The tole process and everything it starts run as an unprivileged user in a
container whose writable parts are two volumes, with no published port, no
capabilities, no privilege gain and a process cap. A bug in a tool or a command
the model runs reaches the volumes and the container network, not the VPS's
files or its other services.

The Caddy service is **not** hardened like this (no `cap_drop`, read-only
filesystem or `no-new-privileges`): the official image gives its binary a file
capability, so dropping all capabilities needs a matching `cap_add`, and none of
it was run here. Add them once you have checked them on your host.

### What the container does not add

The container is **not** the command sandbox of
[sandbox-design.md](sandbox-design.md) and does not replace the approval gate:

- **Commands run with the full authority of uid 10001 in the container.** They
  can read the secret files (`/run/secrets/*` are readable by that user, and
  tole's non-dumpable flag and environment scrub protect only the process and its
  environment, not files), the config and ledgers in `/data`, and write the
  workspace. An approved `cat /run/secrets/tole_serve_token` hands the model the
  approval authority. So approve commands as you would on a bare host, and do
  not pass broad `--allow` patterns to a serve that has a model attached.
- **The network is open.** The container has outbound access (it must reach the
  provider), so a command can send the workspace or a secret anywhere. There is
  no egress filter in this recipe.
- **Docker's boundary is a shared kernel.** It is weaker than a virtual machine;
  do not put a second tenant's data on the same host.
- **The command sandbox is off and is not installed.** tole's bubblewrap sandbox
  for `run_command`, `job_start` and MCP servers is opt-in (`[sandbox]` and
  `[sandbox.consumers]` in the user config) and `bwrap` is not in the image. It
  needs unprivileged user namespaces, which a container usually forbids:
  Docker's default seccomp profile denies creating a user namespace without
  `CAP_SYS_ADMIN`, and a default AppArmor profile can refuse the mounts (not tested
  here). Enabling them means relaxing the container (a custom seccomp profile,
  `apparmor=unconfined`, or a `CAP_SYS_ADMIN` grant), which removes part of what
  the container gave you. Note that in `tole serve` an `optional` sandbox counts
  as `required` (nobody can answer a prompt), so configuring a sandbox that cannot
  start makes the consumer refuse to run rather than fall back (sandbox-design.md,
  threat-model.md #601). A host with rootless Podman or a VM per tenant is the
  stronger answer; this recipe does not provide it.
- **Secrets are protected from the host's views, not from the user.** A Docker
  secret keeps the value out of the image, the compose file and `docker inspect`;
  `root` on the VPS and the container's own user can read it.
- **One user.** There is one credential and no identity bound to a session.

## What was and was not verified

Run in the lane that wrote this:

- **The build context.** `.dockerignore` was simulated with Docker's matching rules (a
  small script over `git ls-files`): the context holds 869 of 1002 tracked files,
  every file under `crates/`, `Cargo.toml`, `Cargo.lock` and `scripts/container/`,
  and nothing else; the excluded ones are docs, CI, `evals`, `bench`, the other
  scripts and `templates`. The context was copied to a scratch directory and
  `cargo metadata --locked --offline` and `cargo check -p tole-cli --locked
  --offline` pass there. The same check with the first version of the file (which
  had `**/secrets/`) fails with `file not found for module secrets`: that pattern
  dropped `crates/tole-core/src/secrets/`, and it is gone.
- `sh -n` and `bash -n` of the entrypoint and the health check; the entrypoint run
  with a secret file (value read, `_FILE` variable unset, a missing file exits
  64) and the health check run against a local stub (200 gives 0, nothing
  listening gives 1).
- `docker compose config` parses `compose.yaml`.
- `stream_probe.py` run against a real `tole serve` (a debug build) with a mock
  streaming provider, **directly, without a proxy**: 30 `text_delta` frames
  spread over the turn, the stream reopened with `after=<seq>` delivered exactly
  the log after that seq (exit 0); with the provider down it fails with exit 1.

**Not run:** `docker build` (no Docker daemon was available, so the image was not
built and the Dockerfile has only been read, not executed), `docker compose up`,
the health check inside a container, Caddy and nginx (no proxy was run, so the
settings in the templates, including `request_buffers`, are written from their
documentation and not tested), the stream probe through a proxy, a restart or an
image upgrade with data on the volumes, `stream_probe.py` against a real
provider, a build on arm64, the secret files with owner 10001 and mode `0400`.

## Not in this change (follow-ups)

- **Publishing the image** from the release workflow, tag-pinned (the issue's
  first bullet). A workflow change; it needs an independent security review.
- **A CI job that builds the image** (without pushing), so a broken Dockerfile
  is caught before a release.
- **The desktop's remote entry connects.** `tole-client` has an HTTPS transport
  since #1071 (`threat-model.md`, "HTTPS to a remote serve"), so it can connect to
  the proxy of this recipe. What is left is the desktop using it for a remote
  entry (a credential store and the connection page).
- **Acceptance criteria that need a running Docker host:** `docker compose up`
  with `/health` green, the timing of the stream through the proxy recorded,
  restart and upgrade with the data kept.
- **A way to keep the serve token unreadable by the model's commands.** Start the
  entrypoint as root with only `CAP_SETUID` and `CAP_SETGID`, open a root-owned
  `0400` secret on a descriptor, and drop to uid 10001 with `--token-fd`, so no
  file the user can read holds the token. It trades the "non-root from the first
  instruction" property for that; it is not built.
- **Real-IP handling.** A trusted-proxy setting so tole's failed-auth limiter
  counts the client and not the proxy (a src change).
