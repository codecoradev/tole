# Running `tole serve` on a VPS: a container behind a TLS proxy (#1044)

A recipe for running `tole serve` on a VPS so a client on another machine (a
desktop, a phone, a script) can use it with the live event stream. It is a
**deployment recipe, not a new capability**: no route, flag or default of tole
changes. What is here:

| File | What it is |
|---|---|
| [`Dockerfile`](../Dockerfile) | Multi-stage build of the release binary of `tole-cli` (default features), a Debian slim runtime, a non-root user (uid 10001), `tole serve` as the command, a `HEALTHCHECK` on `GET /ready` |
| [`.dockerignore`](../.dockerignore) | The build context is `Cargo.toml`, `Cargo.lock`, `crates/` and `scripts/container/` only |
| [`scripts/container/`](../scripts/container) | `docker-entrypoint.sh` (reads a secret file into `TOLE_API_KEY`), `healthcheck.sh`, `stream_probe.py` (the streaming and resume test client), `smoke.sh` (the image check CI runs before a push), `image-tags.sh` (the tag rules of the workflow) and its self-test |
| [`.github/workflows/container.yml`](../.github/workflows/container.yml) | Builds the image on pull requests (no push) and publishes the `nightly` and the release channel to GHCR |
| [`templates/container/compose.yaml`](../templates/container/compose.yaml) | tole plus one Caddy proxy; only the proxy publishes ports |
| [`templates/container/Caddyfile`](../templates/container/Caddyfile), [`nginx.conf`](../templates/container/nginx.conf) | The proxy settings that keep the stream unbuffered |

Status: run end to end on one Docker host (Docker 29.9.3) with a stand-in
model provider, **but the image of the committed `Dockerfile` has not been
built there** (the sandbox had no route to the Debian package mirror). "What was
and was not verified" lists exactly what was exercised. A workflow
(`.github/workflows/container.yml`, #1015) builds and publishes the image to
GHCR as `ghcr.io/codecoradev/tole` in two channels, see "Channels and tags"; it
has not run yet, and the package needs the one-time owner steps there before a
VPS can pull it.

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
curl -H "Authorization: Bearer $(sudo cat secrets/tole_serve_token)" \
     https://$TOLE_DOMAIN/info                         # 200 {"features":[...],"protocolVersion":1,"version":"..."}
```

`/health` answers without a credential (it is what the container health check
and a load balancer use); `/info` needs the token, so a `200` there also proves
the token reached serve. Run `scripts/container/stream_probe.py` (below) once
before you rely on the stack.

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
  `--cron` the ticker beats. It answers `503` when one does not (a read-only volume or one with no free inode shows up here; a
  volume that still has inodes but no free blocks is not proven to fail the probe) and while serve drains after SIGTERM. `GET /health` is the
  liveness answer (the process is up) for a probe that must not fail on a full disk;
  a Kubernetes readiness probe uses `/ready`, a liveness probe `/health`. Neither
  takes a token (mobile-control.md, "`GET /ready`"). `/info` (token needed) is for you to probe the
  version and the feature list after a deploy, not for the container check, which
  would otherwise have to hold the secret.
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

## Channels and tags

The image is `ghcr.io/codecoradev/tole`, for `linux/amd64` and `linux/arm64`
(one multi-arch tag; Docker picks the platform). There are **two channels**, and
there is **no `latest` tag**: a bare `docker pull ghcr.io/codecoradev/tole` fails
on purpose, so nothing changes under a deploy that did not choose a channel.

| Channel | Built from | Tags | Moves? |
|---|---|---|---|
| Production | a release tag `vX.Y.Z` that is on `main` | `X.Y.Z` | never, one image per release |
| | | `X.Y` | to the newest final release of that minor (of the tags on `main`) |
| | | `stable` | to the newest final release (of the tags on `main`) |
| | | `sha-<7 hex>` | never |
| Nightly | the head of `develop`, once a day (03:17 UTC; skipped when `develop` has no commit since the last nightly) | `nightly` | every night, to the newest |
| | | `nightly-YYYYMMDD` | never, that day's build |
| | | `sha-<7 hex>` | never |

A prerelease tag (`v1.0.0-rc.1`) gets only `1.0.0-rc.1` and `sha-<7 hex>`: never
`X.Y` or `stable`. A version is `vX.Y.Z` with each part `0` or without a leading
zero and at most six digits, and a prerelease suffix is at most 40 characters;
anything else fails in the first job, before an image is pushed. The tag rules are
`scripts/container/image-tags.sh`, with a self-test.

What the workflow guarantees about the tags, how, and what it does not:

- **`X.Y.Z`, `nightly-YYYYMMDD` and `sha-*` are not re-pointed by the workflow.**
  They are created only when the registry answers "not found" for them. Any other
  answer (a 5xx, a rate limit, an auth error) fails the job; it is never read as
  "absent". `sha-<7 hex>` names the first image built from that commit (a release
  at the commit of an earlier nightly keeps the nightly's `sha-` tag). Not
  guaranteed: someone with write access to the package can still delete or move a
  tag by hand, and a registry that wrongly answers "not found" for a tag that
  exists would let it be re-created (the check cannot see past the registry).
- **`stable` and `X.Y` move forward only.** "Highest" is computed over the final
  release tags that are **on `main`** (`git tag --merged origin/main`), so a stray
  or refused tag (one not on `main`, or malformed, like `v999999999999.0.0`) never
  counts and cannot freeze them. The list is read again in the `tags` job right
  before `publish` makes the tags, so a run that started earlier cannot take
  `stable` back. A re-run of an old release leaves `stable` and `X.Y` where they
  are. A few seconds pass between that read and the tag write; two release runs
  are serialised by the shared group, so this is not a race between releases.
- **`nightly` only moves to the head of `develop`.** A run whose commit is not the
  head of `develop` (a re-run of an old nightly) builds and tests but publishes
  nothing, and says why in its summary. If `develop` moves while a nightly builds,
  that run keeps its exact tags and leaves `nightly` alone. A scheduled nightly
  is skipped when `sha-<7 hex>` of the head exists; if an earlier run died after
  writing it but before `nightly` moved, start a manual run (it does not skip).
- **The tags of one run point at one image, and a re-run completes them.**
  The tags are written by one `imagetools create` call, **anchor first** (`X.Y.Z`
  for a release, `sha-<7 hex>` for a nightly). On a re-run, if the anchor exists,
  the run first checks that its image carries this commit in the
  `org.opencontainers.image.revision` label (pulled and read; a missing label or
  another commit **fails the run**, which is what a release tag deleted and
  pushed again on another commit looks like), and only then points every missing
  tag at the anchor's digest, not at the rebuilt image. To repair a re-pointed
  tag, delete the package version that carries the old `X.Y.Z` (GitHub,
  organisation Packages, `tole`, the version list; its tags go with it) and re-run;
  check `stable` and `X.Y` afterwards. **Not guaranteed:** the order in which
  `imagetools create` writes the tags of one call was not verified. If it dies
  half way and the anchor was not written yet, a re-run finds no anchor and points
  all tags at its own rebuilt image: still one image per tag set, but a different
  digest of the same commit than the half-written tags had for a moment.
- **What is pushed is what was tested.** Each platform is pushed by digest first,
  then that digest is pulled and smoke-tested; tags are made only after both
  platforms passed. A digest that failed the test stays in the registry untagged.
- **Credentials.** The checkouts keep no token in `.git`. The job that pushes
  (`build-push`) drops its registry login (`docker logout`) before it runs the
  checked-out smoke script. The job that writes the tags (`publish`) has no
  checkout and runs no file of the repository; the tag list comes from the
  `tags` job, which has no `packages` permission.
- **Order.** Every release run shares one concurrency group and every nightly run
  another, so two tags pushed together are published one after the other. GitHub
  keeps one pending run per group and **cancels an older pending one**: when you
  push several release tags within minutes, look at the Actions list and re-run
  any release run that shows "Cancelled" (the re-run is safe, see above). There
  is no alert for that, so the `plan` job of every release run lists the earlier
  release tags on `main` that have no `X.Y.Z` image (only tags whose tree has this
  workflow) as a warning and in the job summary, with the re-run instructions; a
  registry error there counts as "could not check", never as "missing".

**Which tag to pull**

| You want | Pull | Why |
|---|---|---|
| A VPS deploy | `ghcr.io/codecoradev/tole:0.12.0` (an exact version) | Changes only when you edit the tag, after reading the CHANGELOG. Recommended. |
| A VPS that follows releases | `ghcr.io/codecoradev/tole:stable` | Moves to each final release on `docker compose pull`; you take every release, including a BREAKING one, unread. Pair it with the checklist's backup and a staging run. |
| To try what is coming | `ghcr.io/codecoradev/tole:nightly` | The head of `develop`: not a release, can break, not for data you cannot lose. |
| An exact build, forever | `ghcr.io/codecoradev/tole@sha256:<digest>` (compose: `TOLE_IMAGE_TAG=0.12.0@sha256:<digest>`) | A digest never changes. It is printed in the summary of the workflow run that published the image (Actions, "Container image", the `publish` job); `docker buildx imagetools inspect ghcr.io/codecoradev/tole:0.12.0` shows it later. |
| A build of one commit | `ghcr.io/codecoradev/tole:sha-0123abc` | The first image built from that commit, nightly or release. |

With the compose file of this recipe:

```bash
cd templates/container
# .env:  TOLE_IMAGE=ghcr.io/codecoradev/tole   TOLE_IMAGE_TAG=0.12.0
docker compose pull tole && docker compose up -d --no-build
```

To follow a channel, set `TOLE_IMAGE_TAG=stable` (or `nightly`) and repeat those
two commands whenever you want the newest; compose recreates the container on the
same volumes (see "Persistence and upgrades"). Check what is running:
`docker compose exec tole tole --version`, or `GET /info` with the token.

**Rolling back.** The ledgers are append-only and older binaries cannot be
assumed to read what a newer one wrote, so read the CHANGELOG of the versions
between before you go back, and back up the two volumes first. Then set
`TOLE_IMAGE_TAG` to the previous exact version (`0.11.0`) and run the same two
commands. To pin an exact build, append its digest: `TOLE_IMAGE_TAG=0.12.0@sha256:<digest>`
(Docker then ignores the tag and checks the digest).
Docker keeps the previous image locally until you prune it, so a rollback of a
deploy you just made needs no download: `docker image ls ghcr.io/codecoradev/tole`.
The digest of what runs now (the index digest, the one the workflow summary prints):
`docker image inspect --format '{{json .RepoDigests}}' "$(docker compose images -q tole)"`.
It lists a digest only for an image that was pulled from a registry, not for one
built locally. (Written from the command line documentation; not run against a
real container here.)

**Where the publishing runs.** `.github/workflows/container.yml`:

- A pull request that touches the image recipe (Dockerfile, `scripts/container/`,
  `templates/container/`, the workflow) builds both platforms and runs
  `scripts/container/smoke.sh` on each (`tole --version`, uid 10001, `tole serve`
  reaches `healthy` through the image's own HEALTHCHECK), and parses the compose
  file. It pushes nothing and never has `packages: write` (the `plan` job has `packages: read`, to ask the registry what exists). A fork's pull request
  and a manual run from a branch other than `develop` do the same.
- The nightly (schedule, or a manual run from `develop`) and a release tag push the
  image. Each platform builds on a native runner (no emulation), is pushed by
  digest and then pulled back and smoke-tested **by that digest**; a last job puts
  the tags on one index of the two digests and checks that both platforms are in
  it. A tag push is refused unless its commit is on `main` (the same rule as
  `release.yml`), and the smoke test requires `tole --version` to report exactly
  `tole <the tag's version>`.
- A nightly publishes only when its commit is the head of `develop` and the `CI`
  workflow run of that push to `develop` succeeded (still running, missing or red:
  the run builds and tests but publishes nothing, and the next night tries again).
  A manual run has the input `skip_ci_gate` for the first package creation or a
  repair. The gate is the whole `CI` workflow, so an unrelated red job holds the
  nightly back; that is on purpose.
- Only the two jobs that push have `packages: write`, and only when the repository
  is `codecoradev/tole`.
- Not published with the image: a signature, an SBOM or a provenance attestation
  (the workflow builds with `provenance: false` and `sbom: false` so the two
  platform images merge into a clean index); the image carries no sandbox
  tooling (#1016).

**One-time steps for the owner** (a workflow cannot do them):

1. Run the workflow once (Actions, "Container image", "Run workflow", branch
   `develop`; tick `skip_ci_gate` if CI of the head is not green). It creates the
   package.
2. Make it public: GitHub, organisation `codecoradev`, **Packages**, `tole`,
   **Package settings**, **Danger Zone**, **Change visibility**, **Public**.
   (If the button is greyed out, an organisation owner has to allow public
   packages first: organisation **Settings**, **Packages**, **Package creation**,
   tick **Public**.) There is no API for this setting. Check it from a machine
   that is not logged in: `docker logout ghcr.io; docker pull
   ghcr.io/codecoradev/tole:nightly`.
3. Check the repository link: the package page shows `codecoradev/tole` under
   "Repository". The image labels and index annotation carry
   `org.opencontainers.image.source`, which links it; if it is missing, use
   **Connect repository** on the package page. Under **Manage Actions access**
   the repository must be listed with **Write** (it is, when the workflow created
   the package; add it by hand if the package was pushed from elsewhere first).
4. Optional, recommended: an Environment on the two pushing jobs. Repository
   **Settings**, **Environments**, **New environment** (name it `container-publish`),
   **Deployment branches and tags**, **Selected branches and tags**, add the branch
   `develop` and the tag pattern `v*`; then add `environment: container-publish`
   to the jobs `build-push` and `publish` of the workflow (two lines). See "Known
   limits" for why.
5. Optional: add the `Container image` build check as a required check of `develop`
   only if every pull request should run it; it is path-filtered, so a required
   check would block pull requests that do not touch the recipe. Leave it advisory.

**Known limits** (left as they are, on purpose):

- **A pull request can change the workflow.** A pull request from a branch of this
  repository runs the workflow text of that branch, and a collaborator who can push
  a branch could edit it. The guarantees for pull request runs (no push, no
  `packages` permission, never a fork) are only as strong as the text of the
  workflow that review approves; branch protection and review of the workflow file
  are what hold them. The Environment above is the technical backstop: a job in
  it can only run for `develop` and `v*` tags.
- **Floating action versions hold the token.** The workflow uses `actions/*` and
  `docker/*` at a floating major, as every workflow of this repository does, and
  those steps run with the `GITHUB_TOKEN` of the pushing jobs, which can write the
  package. Dependabot (`github-actions`, weekly) keeps them current; pinning to a
  full commit is a repository-wide decision, not made here.
- **The nightly's gate is the smoke test and the `CI` run of the commit**, not the
  other workflows (security scans, perf budget).

No secret and no repository variable is needed: the workflow logs in with the
run's `GITHUB_TOKEN`.

## Persistence and upgrades

The ledgers, user config, cron store and the workspace are on the `tole-data`
and `tole-workspace` volumes, so removing or replacing the container keeps
them. An upgrade of a locally built image is `docker compose up --build -d`, of a published one `docker compose pull tole && docker compose up -d --no-build` ("Channels and tags"): compose stops the old
container (drain, see above) and starts the new one on the same volumes; a turn
that was running is detached and resumes with the next prompt
(`POST /sessions {"resume": id}`, mobile-control.md). The ledgers are append-only
and a newer binary reads them as `tole resume` does; read the CHANGELOG for the
version step first. Do not run two servers on the same
`/data/sessions` (each session file has a writer lock; a second writer gets
`409`). To back up, copy the volume while the service is stopped:
`docker run --rm -v tole_tole-data:/data -v "$PWD":/b debian:bookworm-slim tar czf /b/tole-data.tgz -C /data .`
(the volume name is prefixed with the compose project name).

**Disk growth.** Nothing in tole removes or rotates a settled session file or a
spilled image: the ledgers and `/data/codecora/tole/images` only grow, and they share the
`tole-data` volume with the config and the cron store, so a full volume stops all
of it. `GET /ready` fails on a read-only volume (its `state` check creates and
removes an empty file in `/data/sessions` and in the tole data directory under `/data/codecora`), but it is a
write probe and not a space alarm: watch free space from the host (`df -h`, the
orchestrator's volume metric). To see what is using it:
`du -sh /data/sessions /data/codecora/tole/images` and `ls -lS /data/sessions | head`
inside the container. Retention is decided in part and built in part: a whole
settled session file may be moved by an owner command, nothing deletes one, and
only a read-only size report exists so far (`tole sessions` and `tole doctor` print
the number of session files, their bytes, the largest and the oldest); the archive
command is not built. See [session-retention-design.md](session-retention-design.md)
section 12 (#1020), which also says what can be said about sizing the volume.

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
| Do not buffer the response | A buffer holds `text_delta` frames until it fills or the turn ends | `flush_interval -1` (Caddy also flushed `text/event-stream` unbatched without it in the test; keep it) | `proxy_buffering off` (tole's `X-Accel-Buffering: no` alone also works; the setting does not rely on it) |
| Do not compress `text/event-stream` | A compressor waits for a full block | no `encode` block | `gzip off` |
| Read timeout above the heartbeat | An idle stream carries only a ping every 15 s | none by default | `proxy_read_timeout` (the template: 3600 s) |
| No short read timeout on `POST .../prompt` | It answers when the **turn ends**, which can take minutes, with no byte before that | none by default | `proxy_read_timeout`: the template's 3600 s is a choice, a longer turn is cut |
| Send request bodies with a `Content-Length` | tole refuses chunked request bodies: any `Transfer-Encoding` header is a `400` (`serve.rs`, request smuggling guard) | `request_buffers 12MB`: fixes a body of unknown length from an HTTP/2 client (tested, Caddy 2.11.9) but **not** a chunked HTTP/1.1 body, which is still forwarded chunked and answered `400`. `tole-client` sends a `Content-Length`, so it is not affected; use the nginx recipe for a client that sends chunked bodies | the default request buffering does it; **never** `proxy_request_buffering off` |
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
  transcript, start turns and approve Writes on the box. The shared token has no
  scope and no revocation short of changing the secret and restarting; a
  per-device token (#1320, `tole serve token add`, `read-only` or `full`) is
  revoked alone with no restart. Treat it like an SSH key: `openssl rand -hex 32`, a file, not in a
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
  anyone whether serve works (`200` or `503`, no detail; the failing check only with the token). Webhook routes keep
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

The Caddy service gets the same treatment: no capability, `no-new-privileges`,
a read-only root filesystem (`/tmp` is a tmpfs) and a process cap. It still
binds 80 and 443 with `cap_drop: [ALL]` because Docker sets
`net.ipv4.ip_unprivileged_port_start=0` inside a container (run on Docker 29.9.3;
on rootless Podman or a host that overrides the sysctl, add
`cap_add: [NET_BIND_SERVICE]`). With the name `localhost` Caddy logs one error
because it cannot install its local root CA into the container's trust store;
it is harmless, and a public name does not trigger it.

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
  `[sandbox.consumers]` in the user config) and `bwrap` is not in the image;
  `tole doctor` inside the container says `sandbox: backend unavailable
  (bubblewrap (bwrap) is not on an absolute PATH entry ...)` and warns that
  `verify` and `turnend` would run unsandboxed (`doctor` is not `serve`: what a
  serve does with them is the `optional` sentence below, which was not tested
  here). It
  needs unprivileged user namespaces, which a container usually forbids. One
  data point, **not** a bubblewrap test: on Docker 29.9.3 with its default
  seccomp profile, `unshare --user --map-root-user true` as uid 10001 in the
  pinned Debian image fails with `Operation not permitted`, and succeeds with
  `--security-opt seccomp=unconfined`. Whether `bwrap` itself then works, what
  `optional` and `required` do in each case, and which setup the image should
  ship is the open measurement of #1016; this recipe does not claim an answer.
  Enabling user namespaces means relaxing the container (a custom seccomp
  profile, `apparmor=unconfined`, or a `CAP_SYS_ADMIN` grant), which removes
  part of what the container gave you. Note that in `tole serve` an `optional`
  sandbox counts as `required` (nobody can answer a prompt), so configuring a
  sandbox that cannot start makes the consumer refuse to run rather than fall
  back (sandbox-design.md, threat-model.md #601). A host with rootless Podman or
  a VM per tenant is the stronger answer; this recipe does not provide it.
- **Secrets are protected from the host's views, not from the user.** A Docker
  secret keeps the value out of the image, the compose file and `docker inspect`;
  `root` on the VPS and the container's own user can read it.
- **One user.** There is one credential and no identity bound to a session.

## If the token leaks, and rotating it

**What a leaked serve token allows today** (nothing here is mitigated by the
container; the container only bounds where it lands):

- Read every transcript the API serves (the sessions in `/data/sessions`,
  including what the model read from the workspace).
- Start turns on your provider key and your bill, and send prompts.
- **Approve the Writes the model asks for.** Every non-ReadOnly tool waits for an
  approver, and the token holder is that approver. With it, an attacker can have
  the model run commands as uid 10001 in the container: read `/workspace` and
  `/data`, read the secret files (`cat /run/secrets/tole_api_key` gives away the
  provider key), and send anything out over the open network.
- It does not give a shell on the VPS or access outside the container's
  volumes, **unless** a command escapes the container (shared kernel, see above).
- The shared token has no scope and no revocation list; per-device tokens (#1320)
  have both, and this page does not depend on them.

**Rotate** (also after a laptop is lost, a screen was shared, or on any doubt;
tested on the compose stack, old token `401` and new token `200` afterwards):

```bash
cd templates/container
umask 077
openssl rand -hex 32 > secrets/tole_serve_token.new
sudo chown 10001:10001 secrets/tole_serve_token.new && sudo chmod 0400 secrets/tole_serve_token.new
sudo mv -f secrets/tole_serve_token.new secrets/tole_serve_token
docker compose restart tole        # or: docker compose up -d --force-recreate tole
```

The restart is what makes the old token stop working: serve reads the file once
at start, so until then the old token is still accepted. A turn that was running
is detached and resumes with the next prompt; nothing in the ledger is lost.
Then put the new token into every client (the desktop's credential store, a
script) and check `curl -H "Authorization: Bearer <old>" .../info` answers `401`.
If the token could have been used, also read the transcripts since the leak for
commands you did not start, rotate the **provider key** the same way
(`secrets/tole_api_key`, then the restart; revoke the old one at the provider)
and treat anything in `/workspace` and `/data` as seen. The proxy's certificate
is not involved.

## Why not expose plain HTTP

`tole serve` has no TLS of its own, and the token is a bearer credential that
travels in the `Authorization` header of every request, the event stream's
included. On plain HTTP over the internet anyone on the path (a Wi-Fi, an ISP, a
proxy) reads it once and holds approval authority until you rotate; the same
path reads every transcript and can alter an approval answer in flight. That is
why the compose file publishes nothing for `tole` (it listens on `0.0.0.0`
only inside the compose network), the only published ports are the proxy's, and
the client side refuses it: `tole-client` and the desktop (decision D6) require
`https://` for any non-loopback server, except a network the owner marks as
private (WireGuard, an SSH tunnel). If you do not want a public proxy at all, keep the
stack on a private network and publish nothing. Never `-p 7801:7801` on a public
address, and do not put the token in a URL.

## Checklist

Before you run it on a VPS:

- [ ] Domain name points at the VPS, ports 80 and 443 reach the proxy, and the
      provider's firewall allows nothing else inbound (no 7801).
- [ ] `docker compose ps` shows `tole` **healthy** and no `ports:` on it;
      `curl http://<vps-ip>:7801/health` from outside does not connect.
- [ ] `https://<domain>/health` is `200` and `.../info` is `401` without the
      token and `200` with it.
- [ ] `secrets/` is owned by 10001 and `0400`, not in git, not in a chat or a
      ticket; a copy of the token is in your password manager only.
- [ ] `TOLE_IMAGE_TAG` is an exact version (or a digest) of the production
      channel, or a version you built; not `nightly`, and there is no `latest`;
      you read the CHANGELOG step before an upgrade.
- [ ] `scripts/container/stream_probe.py` through the proxy exits 0 (first
      `text_delta` frame within a few hundred milliseconds of the prompt, frames
      spread over the turn, resume with `after=<seq>` equals the log).
- [ ] You know a failed-auth flood on a public name locks you out (`429`, see the
      threat model): restrict the proxy to your addresses, or use a private
      network.
- [ ] The model's commands are approved one by one: no broad `--allow` pattern on
      a serve with a model attached; you know they run as uid 10001 with open
      network and can read the secret files.
- [ ] Backups: the two volumes are copied while the stack is stopped, and you
      have restored one once.
- [ ] A rotation was rehearsed: you know the three commands above and where each
      client keeps the token.

## Decisions on publishing (#1015)

Decided by the owner (2026-11-28) and implemented by `container.yml`:

1. **Registry and visibility.** GHCR, a public package
   (`ghcr.io/codecoradev/tole`), linked to the repository. Making it public is a
   one-time owner step ("Channels and tags").
2. **Tags.** Two channels (nightly from `develop`, production from a release tag),
   exact and floating tags, **no `latest`**. A `latest` would be one more name for
   `stable` that a deploy can follow without choosing; the workflow has none.
3. **When.** The nightly from `develop` (schedule or manual), the production
   channel only from a `vX.Y.Z` tag that is on `main`; a pull request only builds.
   It is a separate workflow on the same trigger as `release.yml` (a tag push),
   not a job of `publish.yml`: the image builds from the checked-out tag with
   `--locked` and does not use crates.io, so it needs no order against the crates
   publish, and `packages: write` stays out of the workflow that holds the
   crates.io token.
4. **Platforms.** `linux/amd64` and `linux/arm64`, each on a native runner
   (`release.yml` already builds on `ubuntu-24.04-arm`). **The Dockerfile was
   never built on arm64 before this workflow**; a failing arm64 build fails the
   run, so the first nightly is where it shows, before any release.
5. **Provenance.** Not done: no signature, SBOM or attestation. Still open for
   the owner. (`packages: write` is limited to the two pushing jobs.)
6. **A build-only job for pull requests.** Done (path-filtered).
7. **Sandbox in the image** (bubblewrap, a seccomp profile): decided by #1016, not
   here.

Still open on #1015: a headless build for the image and the image and binary
sizes in `docs/perf.md` (they need a built image), and a retention rule for the
daily `nightly-YYYYMMDD` tags (they accumulate; a cleanup job is a follow-up).

## What was and was not verified

Run on one Linux host with Docker Engine 29.9.3 and Compose 5.7.1 (2026-11-28).
The sandbox that ran it had no route to the Debian package mirror
(`deb.debian.org` is refused by its network policy), so **the image of the
committed `Dockerfile` was not built**: its `apt-get install ca-certificates git`
step cannot run there. What was done instead, and what that does and does not show:

- **The build stage**, as written: `cargo build --release --locked -p tole-cli` in
  the pinned `rust:1.97.0-slim-bookworm@sha256:6d22...` image, with the host's
  crate cache mounted read-only and `--network none` (`--offline`). It builds with
  the committed `Cargo.lock` in 4 min 55 s at 3 jobs, and the stripped binary is
  24 MB (`tole 0.12.0`). The BuildKit cache mounts of the Dockerfile were not
  exercised.
- **A test image made from the runtime stage of the Dockerfile with only two
  differences**: the `apt-get` step removed (so no `git`, no CA bundle, no
  `/etc/ssl/certs`) and the binary copied from the build above. Everything else
  (user 10001, `/data` and `/workspace`, volumes, `ENV`, the entrypoint and health
  check scripts, `HEALTHCHECK`, `CMD`) is byte for byte the Dockerfile's. That
  image is 148 MB; the real one is larger by what `git` and `ca-certificates`
  add, not measured. It cannot reach an HTTPS provider (no CA bundle), which the
  tests did not need (the provider was a plain-HTTP stand-in).
- **The compose file, unchanged but for the Caddy hardening,** `docker compose up
  --no-build -d` with that test image: `tole` reached `healthy` (the real
  `healthcheck.sh` inside the container; that run used the script as it was then, with
  `GET /health`, and was not repeated after #1318 moved it to `GET /ready`), the proxy started after it, and with
  the secrets owned by root and mode `0600` the stack failed as documented
  (entrypoint exit 64, restart loop) and with owner 10001 and `0400` it started.
  Inside the running container: uid 10001, root filesystem read-only (a write to
  `/usr` fails), no capability on either service, the token on no command line
  and the provider key in no `docker inspect` output (only the `_FILE` name).
- **Through the proxy**, Caddy 2.11.9 (the compose service) and nginx 1.27 (the
  `nginx.conf` template with only the listen port and `proxy_pass` host changed,
  joined to the compose network): `/health` `200` over HTTP/2, `/info` `401`
  without and `200` with the token, HTTP redirected to HTTPS (`308`), no port of
  `tole` published (`127.0.0.1:7801` on the host does not connect).
- **`stream_probe.py` through both proxies** against a real `tole serve` in the
  container with a stand-in OpenAI-style provider (a script that sends 40 chunks
  250 ms apart, no real model): through Caddy the first `text_delta` frame came
  11 to 14 ms after the prompt was sent, through nginx 12 ms, straight at the
  container 4 ms (the proxy's own cost is about 7 to 10 ms); 40 frames over
  9.77 s with a median gap of 250 ms and a maximum of 251 to 254 ms, in all three; the
  resume check passed each time (stream cut after 3 entries, `after=6` delivered
  the one entry the log had after that). The model was a stand-in: the numbers
  show what the proxy does, not a real provider's latency.
- **A buffering control**, with a mock upstream: nginx with `proxy_buffering on`
  and no `X-Accel-Buffering` batched all six frames at the end of the response;
  the template with `proxy_buffering off` delivered them 0.5 s apart, also when the
  upstream sent no `X-Accel-Buffering` header. Caddy delivered them 0.5 s apart with
  `flush_interval -1`, and also without it (it flushes `text/event-stream`
  by itself; the setting stays as a safeguard).
- **Request bodies** with a mock upstream that reports what it received: nginx
  forwards a `Content-Length` for HTTP/1.1 chunked and HTTP/2 bodies; Caddy with
  `request_buffers` does it for HTTP/2 only, and forwards an HTTP/1.1 chunked
  body chunked (see the table above).
- **`caddy validate`** of the Caddyfile and **`nginx -t`** of the nginx
  template (with a throwaway self-signed certificate): both pass. `docker compose
  config` parses the compose file.
- **Persistence.** With sessions written, `docker compose restart tole` and then a
  recreate of the service from a re-tagged image (an upgrade rehearsal, same
  binary) left the two ledger files on the `tole-data` volume (mode `0600`, owner
  10001); `POST /sessions {"resume": id}` reopened one and `GET .../entries`
  returned the whole transcript. (`GET /sessions` lists only the sessions the
  running server hosts, so it is empty after a restart; that is not data loss.) An
  idle stop and start took about half a second.
- **Rotation**, as in the section above: old token `401`, new token `200` after the
  restart, and while the old process was still running the old token was
  still accepted.
- **`tole doctor`** inside the container (without the entrypoint's environment, so
  its "provider is not configured" line is an artefact of `docker exec`): no
  bubblewrap, `verify` and `turnend` warned as unsandboxed.
- **The earlier, no-daemon checks** (build context under `.dockerignore`,
  `cargo check` in a scratch copy, the entrypoint and health scripts against a
  stub) were not repeated; the runs above supersede them where they overlap.

**Not run:** `docker build` of the committed `Dockerfile` (the `apt-get` step; the
BuildKit cache mounts; the `COPY --from=build` of the real build output); the
size of the real image; a build on arm64; the Caddy certificate path with a
**public name** (Let's Encrypt: only `localhost` and its local CA were run, with
`--insecure` in the probe); the nginx recipe on a host rather than in the
compose network; the stack with a real model provider; a stop during a running
turn (graceful drain in a container); `tole serve --transport mcp` or
`--auth basic` in the image; the desktop's remote entry; Podman; any
bubblewrap run (#1016); an external client from another machine; a published
image, and everything in `container.yml` and the scripts it calls that needs a
Docker daemon or GitHub (see "Verified for `container.yml`" below).

## Not in this change (follow-ups)

- **Retention of the nightly tags.** `nightly-YYYYMMDD` and `sha-*` images of
  the nightly channel accumulate in the package; a cleanup rule (for example keep
  the last 30 days) is not built.
- **Signature, SBOM, provenance attestation** of the published image: owner
  decision, not built.
- **A headless build for the image** and the image and binary sizes in
  `docs/perf.md` (#1015).
- **The desktop's remote entry connects.** `tole-client` has an HTTPS transport
  since #1071 (`threat-model.md`, "HTTPS to a remote serve"), so it can connect to
  the proxy of this recipe. What is left is the desktop using it for a remote
  entry (a credential store and the connection page).
- **Acceptance criteria of #1044 still open:** the compose example with a public
  name and a real certificate, the first-token-to-first-frame time measured
  against a provider that logs its first token (the stand-in provider here had
  no such log), a stop during a running turn, and the desktop entry.
- **A way to keep the serve token unreadable by the model's commands.** Start the
  entrypoint as root with only `CAP_SETUID` and `CAP_SETGID`, open a root-owned
  `0400` secret on a descriptor, and drop to uid 10001 with `--token-fd`, so no
  file the user can read holds the token. It trades the "non-root from the first
  instruction" property for that; it is not built.
- **Real-IP handling.** A trusted-proxy setting so tole's failed-auth limiter
  counts the client and not the proxy (a src change).
- **Per-device tokens** (#1320) are in; the rotation and leak sections above still
  describe the shared token, and a lost device's token is revoked with
  `tole serve token revoke <name>` (no restart).

## Verified for `container.yml`

Written without a Docker daemon and without GitHub Actions, so **the workflow has
not run once**. What was checked, by reading and by running what can run here:

- The workflow parses as YAML (PyYAML), every `run:` block passes `bash -n`, and
  every `${{ }}` expression was read against the contexts GitHub allows at that
  place. `actionlint` and `shellcheck` were not available.
- `scripts/container/test-image-tags.sh` passes (29 cases, the anchor first among them: both channels,
  forward-only floating tags, numeric version order, prerelease and its length
  cap, backport, stray malformed or huge tags in the list, leading zeros, no
  `latest`, every refusal). The version match of `smoke.sh` (`1.2.3` does not
  match `1.2.30` or `11.2.3`) was run by hand on its own.
- `docker compose config` parses `templates/container/compose.yaml` with the
  default variables and with `TOLE_IMAGE` and `TOLE_IMAGE_TAG` set to a GHCR
  image.

**Not verified:** any run of the workflow; the action versions (`docker/*@v3`/`@v6`
floating majors, like the other workflows; Dependabot bumps them); `docker build`
of the Dockerfile on either platform; `smoke.sh` against a real image (the
`healthy` wait of up to 120 s with a stand-in provider URL is untested, and a
wrong assumption there fails every nightly until fixed); `docker buildx imagetools
create --annotation` and the `jq` index check; the `ubuntu-24.04-arm` build
time of the Rust stage (the earlier amd64 build took 4 min 55 s at 3 jobs); that
GHCR links the package and takes the visibility change as described; the
`gh api` query of the nightly's CI gate and the concurrency expression (read, run
only through a `jq` stand-in for the gate); `docker pull` of a digest on the
arm64 runner; the re-run paths (anchor reuse, a develop head that moved), which
need a registry; that GitHub keeps a cancelled pending release run re-runnable; the exact text a
registry gives for a tag or a package that does not exist (the "not found" test
looks for `MANIFEST_UNKNOWN`, `NAME_UNKNOWN`, `manifest unknown`, `name unknown`,
`not found` and `404`: if GHCR answers differently for a package that does not
exist yet, the first publish fails closed with "cannot tell whether ... exists"
and the pattern needs one more string); `docker image inspect` reading the revision
label of a pulled index on the runner; the write order of `imagetools create` for
several `-t` flags; the `docker compose images -q` and `RepoDigests` command in
"Rolling back"; the "release tags without an image" warning (it reads trees and
the registry; it has not run).
