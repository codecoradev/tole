# Sandbox for third-party servers and model commands — design note

Status: **design with owner decisions of 2026-11-14 (§16); #601, epic
#594.** The first slice is implemented: the core module
`tole_core::sandbox` and its wiring into the stdio MCP launch (opt-in
`[sandbox]` in the user config: `mode` off, optional or required,
profiles `server` and `strict`, network `none` or `host`, connections
keyed by server name and workspace) and a `tole doctor` line; the rest of
§15 is not. Builds on [third-party-plugins-design.md](third-party-plugins-design.md)
§4.2–4.3 and [secrets-integrations-design.md](secrets-integrations-design.md),
and on the #594 audit (#675–#684). The feasibility results in §3 were
measured on 2026-11-13 in a Linux container (kernel 6.18, Ubuntu 24.04).

## 1. Summary

- On Linux, tole runs a third-party stdio MCP server inside **bubblewrap**
  (`bwrap`), found on `PATH` like cora and uteke. tole builds the `bwrap`
  argument list itself from a profile: no other tool, no new crate.
- Inside: a private `/proc` (tole and every other process are invisible),
  the workspace read-only, a private writable scratch dir, the tole data
  dir and the rest of `$HOME` absent, and no network unless the profile
  names hosts.
- `run_command`, `job_start`, cron scripts, the mission `--verify` command
  and the stop gates can opt in with the same profiles.
- No sandbox available: a server that needs one is not started. Only an
  owner-written `sandbox = "optional"` lets it run without one, and every
  such start is warned about and logged. Nothing falls back silently.
- macOS and Windows get no sandbox in the first phases (§13).

## 2. Threat model

The attacker is code tole starts but does not trust: a third-party MCP
server (malicious from the start, or after an update), or a command, test
or build script written by a model that read a prompt injection. It runs as
the owner's user.

### 2.1 What the sandbox must stop

| # | Attack | Today | With the sandbox |
|---|---|---|---|
| S1 | Read tole's environment or memory (`/proc/<ppid>/environ`, `mem`, ptrace) | closed on Linux by non-dumpable (#675) | also closed by a separate mechanism: tole is not in the sandbox's process namespace |
| S2 | Read secrets kept in files: `~/.netrc`, `~/.ssh`, `~/.aws`, `~/.config/gh`, `.env` files outside the workspace | open | `$HOME` is not mounted |
| S3 | Write owner-only records: cron ledger, plugin lock, skill approvals, trust store, user config (`~/.codecora`), or run `tole cron create` / `tole plugin approve` (#684) | open | the data dir does not exist inside; the tole CLI there finds no lock, no ledger and nothing to approve |
| S4 | Plant code that later runs **outside** the sandbox: `.git/hooks`, `.git/config` (`core.hooksPath`, `core.fsmonitor`), `.tole/` config, shell rc files | open | those paths are read-only inside, `$HOME` is absent |
| S5 | Send data anywhere (exfiltration, a callback to a control server) | open | no network by default; an allowlist of named hosts through a filtering proxy (§8) |
| S6 | See or signal other processes (other agents, other servers, the owner's shell) | open | private PID namespace, new session (no `TIOCSTI` on the terminal) |
| S7 | Escape the view by creating a user or mount namespace inside | n/a | refused (`--disable-userns`, probed in §3) |

### 2.2 What it cannot stop

- **Kernel bugs and sandbox escapes.** A process sandbox shares the kernel;
  escapes in other agents' sandboxes are reported regularly
  (third-party-plugins design §2). For high-risk code, use a VM, a
  container on another host, or a remote HTTP MCP server.
- **What the sandboxed code is allowed to do.** A server granted
  `GITHUB_TOKEN` and `api.github.com` can misuse that token on that host.
  A command with a writable workspace can corrupt the workspace. The
  sandbox bounds the damage to the grants; it does not judge them.
- **Model-visible text.** Poisoned tool descriptions, resources and prompts
  (#680) reach the model whatever the server's process can touch. That is
  the plugin lock's job (#599, #680).
- **Identity of the server.** Which binary runs under a name is the lock's
  job (#679); the sandbox only bounds what that binary can do.
- **Anything tole does in-process:** file tools, `web_fetch`, the `gh`
  and `git` tools, HTTP MCP servers. They are governed by the approver,
  the workspace jail and the redactor, not by this sandbox.
- **Resource exhaustion** beyond simple rlimits (§12): no cgroup limits in
  the first phases.
- **Other platforms** (§13).

## 3. Feasibility (probed in this container, 2026-11-13)

Commands and raw output are in the PR's Testing section. "nobody" means
the probe ran as uid 65534 through `setpriv`, to test the unprivileged
case even though the container runs as root.

| Question | Result |
|---|---|
| `bwrap` installed? | No. `apt-get install bubblewrap` (as root) installed 0.9.0, 130 KiB, depends only on libc, libcap, libselinux. Removed after the probes. |
| Unprivileged user namespaces | Work: `max_user_namespaces` = 64250, no AppArmor userns restriction on this kernel; `bwrap --unshare-all` runs as nobody. |
| Private `/proc`, PID namespace | Works: the sandboxed shell is PID 2 and sees 4 to 6 PIDs, all its own. |
| Read-only workspace, writable scratch, absent data dir | Works: a write to the workspace fails with "Read-only file system", the scratch write succeeds, `~/.codecora/tole/plugins.lock` does not exist inside. |
| No network | Works: the only interface is `lo`; a TCP connect to 1.1.1.1:443 fails with "Network is unreachable". |
| Allowlist channel | Works: a Unix socket bound into the sandbox reaches a host-side listener (standing in for the proxy of §8) while direct TCP stays unreachable. |
| Nested user namespace (S7) | Without `--disable-userns`, `unshare -r -m` inside the sandbox **succeeds**. With `--unshare-user --disable-userns` it fails ("No space left on device"). Required flag. |
| Root caller | With root as the caller the sandboxed process keeps a full capability set unless `--cap-drop ALL` is passed; with it, `CapEff` is 0 and `NoNewPrivs` is 1. Required flag. |
| Environment of bwrap's own PID 1 | **Finding:** `--clearenv` clears only the child's environment. bwrap's PID-1 reaper keeps the environment bwrap was started with, and the sandboxed child can read it from `/proc/1/environ`. tole must apply the ENV-1 scrub to the `bwrap` process itself (it does today for the server's own `Command`). With the scrubbed env the canary was gone. |
| Landlock | Built in (`CONFIG_LSM` lists `landlock` first); `landlock_create_ruleset(.., LANDLOCK_CREATE_RULESET_VERSION)` returned **ABI 7**. `/sys/kernel/security` is not mounted here, so the LSM list came from `/proc/config.gz`. A 60-line ctypes probe denied a read outside the allowlist, and denied the read of the parent's `/proc/<ppid>/environ` even with `/proc` allowed and the parent dumpable (Landlock's ptrace scoping). |
| seccomp | `CONFIG_SECCOMP_FILTER=y`; the container imposes no filter (`Seccomp: 0`); a strict-mode filter installed by a child killed it on `getpid` as expected. |
| cgroups | v1 hierarchy, no systemd: no per-sandbox memory or PID limit without extra setup. |
| Spawn latency (100 runs, mean) | plain `/bin/true` 3.1 ms; bwrap with a mount namespace 7.0 ms; bwrap `--unshare-all` 7.6 ms; with `--share-net` 7.0 ms; `unshare -r -n -p -f --mount-proc` 5.5 ms. **About +4.5 ms per spawn.** |
| Memory | bwrap outer process RSS 2.1 MiB (PSS 0.36 MiB), its PID-1 reaper RSS 1.5 MiB (PSS 0.24 MiB): **about 0.6 MiB PSS per sandbox.** |
| firejail | Installable (0.9.72, setuid root, 1.6 MiB). As nobody it refuses ("the user is not allowed to use Firejail") until the user is added to `/etc/firejail/firejail.users`. Removed after the probe. |
| nsjail | Not packaged for Ubuntu 24.04; build from source only. Not probed. |
| systemd-run | Present but unusable: no systemd as PID 1, no user bus. |
| Docker | CLI present, no daemon. |
| `srt` | Not installed. Registry metadata (npm, read only, not installed): `@anthropic-ai/sandbox-runtime` 0.0.83, Apache-2.0, needs Node ≥ 22.12, plus `bubblewrap`, `socat` and `ripgrep` on Linux; README marks it "Beta Research Preview". |

## 4. Mechanism: options and choice

| Option | Filesystem view | `/proc` hidden | Network | Limits | Needs | Maturity, licence |
|---|---|---|---|---|---|---|
| **bubblewrap** | bind mounts, tmpfs, read-only binds | yes (PID ns + fresh `/proc`) | none or full; allowlist via a Unix-socket proxy | none (rlimits from the caller) | `bwrap` ≥ 0.8 (for `--disable-userns`), unprivileged user namespaces | Flatpak's sandbox since 2016; in Ubuntu main; LGPL-2.0+ (run as a separate program, no linking) |
| `srt` (Anthropic sandbox-runtime) | deny/allow read and write lists | via bubblewrap | **host allowlist built in** (HTTP and SOCKS proxies) | none | Node ≥ 22.12, npm package, `bwrap`, `socat`, `ripgrep` | 0.0.x research preview, config format still changing; Apache-2.0; also macOS (Seatbelt) and Windows (alpha, admin install) |
| Landlock (helper in tole) | allowlist of path trees; cannot hide or remount | no, but blocks reading other processes' `environ`/`mem` (probed) | TCP bind/connect by **port** only (ABI ≥ 4); UDP and DNS not covered | none | kernel ≥ 5.13 (ABI 4+ for network: 6.7); ~300 lines of syscall code in tole (`libc`, already a dependency) | in mainline since 2021; kernel ABI |
| firejail | profiles, many options | yes | none, or a netns needing root setup | rlimits, cgroups | setuid-root binary, `firejail.users` allowlist | mature, but a large setuid attack surface with a record of local privilege escalations; GPL-2.0+ |
| nsjail | mounts, PID ns | yes | none or macvlan (root) | rlimits, cgroups | build from source (protobuf) | mature (Google); Apache-2.0; not packaged here |
| `systemd-run` | `ProtectHome`, `ReadOnlyPaths`… | partly (`ProtectProc`) | `IPAddressAllow` | cgroups | systemd as PID 1 and a user manager | fine on desktops; absent in containers, CI and here |
| `unshare` (util-linux) | none by itself (tole would script mounts) | yes | none | none | util-linux | it is the primitive bwrap wraps; using it means rebuilding bwrap |
| Containers (Docker, Podman) | full | yes | yes | yes | a daemon or rootless setup, images | heavy (hundreds of ms per start); right for high-risk code, wrong as the default |
| macOS `sandbox-exec` | SBPL profile: deny reads and writes by path | no `/proc`; see §13 | allow only a localhost port | none | built in | marked deprecated by Apple yet still shipped and used by Chrome, Codex and `srt`; not testable here |
| Windows | — | — | — | Job objects only | AppContainer or restricted tokens need ACL changes; `srt`'s Windows mode needs an admin install | no unprivileged equivalent |

**Choice: bubblewrap, with the argument list built by tole.**

- It is the external wrapper the owner chose (third-party design §7,
  decision 2); detection on `PATH` matches cora and uteke.
- It covers S1–S4, S6 and S7 by construction and is in every major
  distribution's main repository.
- tole stays one Rust binary: no Node runtime and no npm package on the
  path of every server start.
- The profile is ~50 lines of argv building, pure and unit-testable.

**Rejected:**

- `srt` as the required backend: it needs Node ≥ 22.12, `socat` and
  `ripgrep` beside `bwrap`. It is a 0.0.x preview whose configuration
  still changes, and on Linux the isolation is bubblewrap anyway. Its one
  extra, the host allowlist, is small enough to provide in tole (§8).
  Kept as a candidate backend for macOS (decision 4).
- Landlock alone: no mount view, no private `/proc`, and network control
  by port only, with UDP (and so DNS tunnelling) uncovered. It also means
  sandbox code inside tole, against the owner's "external wrapper"
  decision. Worth keeping as an optional second layer (decision 3).
- firejail: setuid root and a large attack surface; refuses
  unprivileged users by default.
- nsjail: not packaged; same isolation as bwrap at a higher install cost.
- systemd-run: absent where unattended runs happen (containers, CI).
- Containers: too heavy as the default; the owner can still run tole or
  a server in one.

## 5. The sandbox view

For each sandboxed process tole runs `bwrap <args> -- <program> <args>`:

| Path inside | Source | Mode |
|---|---|---|
| `/usr`, `/bin`, `/lib*`, `/sbin` | host | read-only |
| `/etc`: `ld.so.cache`, `passwd`, `group`, `nsswitch.conf`, `localtime`, `ssl/`, `ca-certificates/`, `alternatives/` | host | read-only |
| `/etc/resolv.conf`, `/etc/hosts` | host | read-only, only when the profile has network |
| the program's install prefix (e.g. `/opt/node22`, a venv) | host | read-only |
| workspace | host | read-only, or read-write per profile; `.git/hooks`, `.git/config` and `.tole/` always re-bound read-only on top |
| scratch (`$TMPDIR`, `$HOME`) | a fresh `0700` dir under the system temp, owned by tole | read-write |
| extra `read` / `write` paths of the profile | host | as named |
| `/proc` | new, for the sandbox's PID namespace | — |
| `/dev` | minimal (`null`, `zero`, `random`, `urandom`, `tty`) | — |
| `/tmp` | tmpfs | read-write |
| everything else (`$HOME`, the data dir, `/root`, `/run`, `/var`, other users) | absent | — |

Fixed flags: `--unshare-all --unshare-user --disable-userns --cap-drop ALL
--new-session --die-with-parent` (not for `job_start`, whose jobs outlive
the call by design), `--unshare-net` unless the profile has full network,
and `--chdir` to the workspace.

**Environment.** tole applies today's ENV-1 scrub (with the consumer's
`[secrets]` release) to the `bwrap` process itself, and bwrap passes that
environment through. tole never puts a value in bwrap's argument list:
`--setenv` arguments are readable by every local user in
`/proc/<pid>/cmdline`. Only `HOME`, `TMPDIR` and, with network, the
proxy variables are overridden with `--setenv` (no secrets there). A
granted secret therefore reaches the sandboxed process exactly as it does
today, and nothing else does.

**Paths a profile may not grant.** A profile entry that is, contains or
sits inside the tole data dir (`$CODECORA_HOME`, default `~/.codecora`) or
the session directory, or that is `$HOME` itself, `/` or `/proc`, is a
config error that names the entry. Granting `~/.ssh` and similar is
allowed (an owner's decision) but `tole doctor` lists it.

## 6. Configuration

Profiles and modes live in the **user config only**
(`~/.codecora/tole/config.toml`), like `[delegation]` and `cron.*`.

```toml
[sandbox]
backend = "bwrap"            # "bwrap" | "none"; default: detect bwrap on PATH

[sandbox.profiles.server]    # built in; may be redefined
workspace = "read"           # "none" | "read" | "write"
network = "none"             # "none" | "allowlist" | "full"
hosts = []                   # with network = "allowlist": "api.github.com", "*.npmjs.org:443"
read = []                    # extra read-only paths
write = []                   # extra writable paths

[sandbox.profiles.github]
workspace = "read"
network = "allowlist"
hosts = ["api.github.com"]

[sandbox.servers.github]     # per stdio MCP server, keyed by server name
profile = "github"
mode = "required"            # "required" | "optional" | "off"

[sandbox.consumers]          # opt-in for model-driven commands (§9.2)
run_command = "command"      # profile name, or absent = not sandboxed
job_start = "command"
cron = "command"
verify = "command"
turnend = "command"
```

Rules:

- **User config only.** A `[sandbox]` table in a project config
  (`.tole/config.toml`) can only **narrow**: name a stricter built-in
  profile for a server or consumer, drop hosts, turn `write` into `read`,
  or turn `optional` into `required`. Anything wider is ignored with a
  startup warning naming the user config as the remedy, as `[secrets]`
  does today. A cloned repository must never loosen its own sandbox.
- **Bound to the lock entry.** The profile name and its hash are recorded
  in the server's plugin lock entry at approval (the "sandbox profile"
  field of third-party design §4.1). A changed profile is shown by
  `tole plugin diff` and needs re-approval, like a changed manifest.
- **Owner-only commands never run inside a sandbox, and never run from
  one.** Hooks (`--on-pretool` / `--on-posttool`), first-party helper
  processes (uteke, cora, the memory loop, skills), the `gh` and `git`
  tools, and owner CLI commands (`tole cron create`, `tole plugin
  approve`, `tole skills approve`, `tole config trust`) run as today. Run
  from inside a sandbox, the same commands find no data dir and change
  nothing (S3).
- **Flags.** `--sandbox <profile>` / `--no-sandbox` on `run`, `mission`
  and `chat` may narrow per session; `--no-sandbox` cannot turn off a
  `required` server (the server is then not started).

## 7. Default profiles

| Profile | Workspace | Network | For |
|---|---|---|---|
| `server` | read | none | third-party stdio MCP servers |
| `command` | write (with `.git/hooks`, `.git/config`, `.tole/` read-only) | none | `run_command`, `job_start`, cron scripts, `verify`, `turnend` |
| `strict` | none | none | servers that need nothing from the machine |

All three: scratch dir writable, data dir and `$HOME` absent, private
`/proc`, no new namespaces, no capabilities. Toolchain caches a command
needs (`~/.cargo/registry`, `~/.npm`, `~/.cache/uv`) are not in the
defaults; a profile adds them under `read` and, if it fetches, adds the
registry hosts. Package downloads for a server happen at install time,
outside the sandbox (`tole plugin add`, #600), so a server profile needs
no registry access.

## 8. Network

- `none` (default): `--unshare-net`; only the sandbox's own loopback.
- `full`: `--share-net`. Allowed only in the user config; `tole doctor`
  and the startup line show it.
- `allowlist`: `--unshare-net` plus one channel out. tole runs one small
  filtering proxy per process (HTTP `CONNECT` and plain HTTP, tokio, which
  tole already uses) on a Unix socket in the scratch dir, bound into the
  sandbox. Inside, a forwarder listens on `127.0.0.1:<port>` in the
  sandbox's own loopback and relays to the socket. The forwarder is the
  tole binary itself, bound read-only, run as a hidden subcommand. `HTTPS_PROXY`,
  `HTTP_PROXY` and `ALL_PROXY` point to it. The proxy:
  - allows a request only when the host (and port, when given) matches
    the profile's `hosts`, checked before any DNS lookup, so a refused
    name never reaches a resolver;
  - resolves an allowed name itself and refuses loopback, link-local,
    private and cloud-metadata addresses unless an IP literal is listed
    (reusing the SSRF vetting being added for the web clients in #625);
  - never adds credentials (it is not a credential broker, which the epic
    keeps out of scope);
  - logs each refusal (host name only).

  A client that ignores the proxy variables simply has no network (fails
  closed). There is no DNS inside, so DNS tunnelling is closed too.

## 9. Consumers

### 9.1 Third-party stdio MCP servers (required)

- **Which servers.** The tier comes from the lock entry (#600). First-party
  servers (cora, uteke, nginjen, laya) default to `mode = "off"`;
  third-party ones to `required`; unverified ones are always `required`,
  and `optional` is refused for them (owner decision 3). HTTP servers are
  not processes on this machine and are not affected.
- **Lazy start.** Today every configured server starts when the session
  starts. With the lock storing the approved tool snapshot (#599 part 2),
  tole registers a locked server's tools from the snapshot and starts
  nothing. The first call to one of its tools starts it (inside the
  sandbox) and checks its live manifest against the lock: a mismatch
  stops the server, unregisters its tools for the session and returns the
  "changed since approval" error. A server idle for `idle_secs` (default
  600) is stopped and restarted on the next call. A dormant profile starts
  nothing. A server with no snapshot yet (approval pending) is probed at
  approval time, inside the same sandbox.
- **Tool retrieval by meaning.** Third-party tool definitions are not all
  put in the model's context. The model gets one read-only search tool;
  definitions whose description matches the request by meaning are added
  for the rest of the session, through the skills index seam. This bounds
  shadowing (a poisoned description is only seen when it is relevant) and
  context cost. It does not depend on the sandbox and can land on its own.
- **Unattended runs** (cron, webhook, `serve`): only locked, approved,
  non-unverified servers, and `optional` counts as `required` (open
  question 2).

### 9.2 Model-driven commands (opt-in)

`run_command`, `job_start`, cron scripts (and `command:` deliveries), the
mission `--verify` command and the `on_turnend` stop gates each map to a
profile in `[sandbox.consumers]`. Absent means "not sandboxed", as today:
these commands often need the owner's toolchain, and making them sandboxed
by default would break existing setups. `tole doctor` recommends `command`
for each one, and a later phase can make it the default (decision 1).

Notes per consumer:

- `job_start`: no `--die-with-parent` (jobs are detached); the log file is
  opened by tole outside and passed as stdout, so it works with a
  read-only workspace; `job_poll` watches the `bwrap` PID, which lives as
  long as the job.
- `verify` and `turnend`: they run code the agent just wrote, unattended,
  without approval (#676). They are the strongest case for opting in.
- Child agents (`agent_start`, `delegate`) are tole processes with their own
  tools; they are not wrapped. Their own model commands follow their own
  `[sandbox.consumers]`.

## 10. Failure behaviour

At startup tole resolves the backend once: `bwrap` on `PATH`, version
≥ 0.8, then a self-test spawn (`bwrap … -- /bin/true`, ~8 ms) to catch
kernels or containers that refuse user namespaces (Docker's default
seccomp profile, Ubuntu's AppArmor userns restriction without bwrap's
profile). The result is cached for the process.

| Mode | Sandbox works | Sandbox unavailable |
|---|---|---|
| `required` | runs inside | **not started**: a startup line names the server, the reason and the remedy (install bubblewrap, or set `mode = "optional"` in the user config); its tools are not registered; a call to one returns the same message |
| `optional` | runs inside | runs **unsandboxed**, with a warning at every start and a ledger entry; refused for unverified servers and, by owner decision 2, in unattended runs |
| `off` | runs unsandboxed | runs unsandboxed |

A consumer in `[sandbox.consumers]` behaves as `required`: the command is
refused with the reason, never run unsandboxed. A sandbox that starts but
whose program fails (missing path, denied network) is an ordinary error
of that program, with tole's note naming the profile.

## 11. What is logged

Names only, never values or argument contents:

- At startup (stderr, and `tole doctor`): backend and version, or why
  none is available; each stdio server with its mode and profile name.
- Per start (session log entry): server or consumer name, profile name,
  profile hash, sandboxed or not and why.
- Per proxy refusal: profile name and host name.
- Per `optional` fallback: a ledger entry (server name, reason).
- Never: environment values, granted secret values, file contents, the
  full bwrap argument list (it holds local paths; available with
  `--verbose` only).

## 12. Cost

- Latency: about +4.5 ms per spawn (§3). For an MCP server this is once
  per start, against a handshake that takes 100 ms to several seconds. For
  `run_command` it is per call, against a 30 s ceiling; negligible.
- Memory: about 0.6 MiB PSS per running sandbox (two small bwrap
  processes); the proxy is one tokio task in the tole process.
- Disk: the scratch dirs, removed when the session ends; `bwrap` itself
  is 130 KiB.
- Resource limits: tole sets rlimits before exec (`RLIMIT_FSIZE`,
  `RLIMIT_CPU` for commands, `RLIMIT_AS` optional) and sizes `/tmp` with
  `--size`. cgroup memory and PID limits need systemd or a delegated
  cgroup and are not in the first phases.

## 13. Platforms

- **Linux:** the design above. Requires unprivileged user namespaces
  (default on most distributions). Ubuntu ≥ 23.10 restricts them through
  AppArmor (`kernel.apparmor_restrict_unprivileged_userns`); whether
  `bwrap` works there depends on the AppArmor profile in place (`srt`'s
  README asks owners to lift the restriction). The probe kernel has no
  such restriction, so this is untested here; the self-test of §10
  detects it either way. Inside default Docker containers user namespaces
  are usually refused: the self-test fails and `required` servers do not
  start there.
- **macOS:** no namespaces and no `/proc`. `sandbox-exec` with a Seatbelt
  profile can deny reads and writes by path and allow network only to a
  localhost proxy port, which covers S2–S5. Not testable here; whether
  Seatbelt can also stop a same-user process from reading tole's
  environment through `sysctl(KERN_PROCARGS2)` (S1 on macOS, the residual
  of #675) is unverified and must be tested on a Mac. Proposed: a later
  phase, either tole-generated profiles or `srt` as the backend (open
  question 4). Until then, `required` servers do not start on macOS.
- **Windows:** no unprivileged equivalent. AppContainer and restricted
  tokens need ACL changes on the files a process may touch; Job objects
  only limit resources; `srt`'s Windows mode needs an administrator
  install. Windows gets no sandbox: third-party stdio servers need
  `mode = "optional"` or an HTTP server, and unverified ones are
  unavailable.

## 14. Audit findings and what the sandbox closes

| Finding | What the sandbox closes | What remains |
|---|---|---|
| #675 `/proc/<ppid>/environ` (fixed by non-dumpable) | A second, independent wall for sandboxed processes: tole is outside their PID namespace, so a later regression of the dumpable flag (an exec path that forgets it, a setuid transition) does not reopen it for them. Also closes reading secrets from the owner's files (S2), the residual #675 named. | Unsandboxed children (hooks, first-party helpers, commands that did not opt in) rely on non-dumpable alone. Root and `CAP_SYS_PTRACE`. macOS and Windows. Secrets granted to a consumer are visible to it by design. |
| #676 `--verify` and stop gates with the full environment (fixed by the scrub) | With `verify` / `turnend` opted in: the code the agent wrote can no longer read `$HOME`, write the data dir or `.git/hooks`, or reach the network. | Opt-in, not default. A secret granted to `verify` is still readable by the test code. The verify result itself still decides mission completion. |
| #679 lock bound to the name only | Bounds the damage of a re-pointed or swapped server: whatever binary runs under the name gets only the profile (no data dir, no `$HOME`, listed hosts only). The profile hash in the lock entry makes a widened profile need re-approval. | The identity check itself (source and integrity in `admit`) must be fixed in #679; a swapped binary with a broad profile still gets that profile's grants and arguments. |
| #680 refused server still serves resources and prompts | Nothing directly: the problem is model-visible text, not process authority. Lazy start helps: a server that is not admitted is never started, so it cannot serve anything. | The gate on resources, prompts and the `/mcp:` listing (#680's own fix); prompts and resources outside the manifest hash. |
| #684 owner-only records writable by the agent | For sandboxed consumers: the data dir is absent, so neither the tole CLI nor a script nor a copied binary can write the cron ledger, plugin lock, skill approvals, trust store or user config; `.tole/` and `.git/hooks` are read-only. | In-process file tools when the workspace contains the data dir (needs #684's jail refusal); consumers that did not opt in; the `tole`-binary refusal and the session marker #684 proposes stay useful as defence in depth. |

## 15. Phased plan

Each phase is one PR of about 900 changed lines or fewer, with its tests,
docs (threat model, this note, README where user-visible) and CHANGELOG
entry.

1. **Core and doctor** (~600): `tole_core::sandbox`: profile types,
   parsing of `[sandbox]` with the user-only and narrow-only rules,
   forbidden-path checks, the pure bwrap argv builder (unit tests per
   flag and per path rule), backend detection and the self-test, a
   `tole doctor` section. Nothing wired yet.
2. **Third-party stdio servers** (~700): `mcp.rs` `start_server` wraps a
   server whose mode says so; failure policy of §10; profile name and hash
   in the lock entry and in `plugin diff`; logging of §11. Integration
   tests with `tests/fixtures/mock_mcp.py` that check a canary in tole's
   env, a file in a fake data dir and a TCP connect all fail inside; they
   skip with a note when `bwrap` is absent, and CI installs `bubblewrap`.
3. **Network allowlist** (deferred by owner decision 5; ~800): the proxy, the in-sandbox forwarder,
   address vetting, refusal logging; tests with a local upstream.
4. **Model-driven commands** (~600): `[sandbox.consumers]` for
   `run_command`, `job_start`, cron scripts and deliveries, `verify`,
   `turnend`; rlimits; per-consumer tests; migration note.
5. **Lazy start and idle stop** (~700): register from the locked snapshot,
   start on first call, manifest check at start, idle stop.
6. **Tool retrieval by meaning** (~900, may split): the search tool over
   third-party definitions through the skills index seam.
7. **Not planned (owner decisions 3 and 4):** a macOS backend and an
   inner Landlock layer. Cgroup limits remain later work.

## 16. Owner decisions

Decided by the owner on 2026-11-14, replacing the six open questions of
the proposal.

1. **Default for model commands.** `verify` (and `turnend`) will run
   sandboxed by default in a later phase. The first sandbox PR is opt-in
   only: `[sandbox]` is off by default and nothing changes for existing
   setups.
2. **`optional` in unattended runs.** An `optional` sandbox is treated as
   `required` for unattended runs (cron, webhook, `serve` with no human).
   If the sandbox is unavailable, an unattended run refuses; it never runs
   unsandboxed.
3. **Landlock.** No tole-owned Landlock layer now. Bubblewrap first.
4. **macOS.** No macOS sandbox now. macOS stays unsandboxed and
   `tole doctor` says so.
5. **Allowlist proxy.** No host-allowlist proxy yet. Network is all or
   nothing per sandbox profile (`none` or `host`); phase 3 is deferred.
6. **RC-1 wording.** The threat-model RC-1 row changes with the first
   sandbox PR: opt-in sandboxing of third-party servers is in scope.

Rules that stay in force for every implementation phase:

- bwrap's own environment is scrubbed: it is spawned with a cleared
  environment, never tole's.
- Secrets are never passed with `--setenv` (they would show in the `bwrap`
  argv and in `/proc/<pid>/cmdline`); a granted secret reaches the program
  only through the scrubbed child environment mechanism.
- `--disable-userns` is always passed (no nested user namespaces).
- `--cap-drop ALL` is always passed, in particular when tole runs as root.
