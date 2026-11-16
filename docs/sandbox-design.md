# Sandbox for third-party servers and model commands — design note

Status: **design with owner decisions of 2026-11-14 (§16); #601, epic
#594.** The first slice is implemented: the core module
`tole_core::sandbox` and its wiring into the stdio MCP launch (opt-in
`[sandbox]` in the user config: `mode` off, optional or required,
profiles `server` and `strict`, network `none` or `host`, connections
keyed by server name and workspace) and a `tole doctor` line, plus the first
slice of phase 4 (#748, §9.3): opt-in `[sandbox.consumers]` for `run_command`
and `job_start` with the built-in `command` profile and rlimits; and the
per-server profiles of #810 (§6.1: named profiles, `[sandbox.servers]`,
`tole sandbox list|show|set`); the rest of §15 is not. Builds on [third-party-plugins-design.md](third-party-plugins-design.md)
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
| S4 | Plant code that later runs **outside** the sandbox: `.git/hooks`, `.git/config` (`core.hooksPath`, `core.fsmonitor`), `.tole/` config, shell rc files | open | `.git`, `.tole/` and the gitdirs listed in §9.3 are read-only inside, `$HOME` is absent; `.git/config` is read-only, but a `core.hooksPath`, `include.path` or `core.fsmonitor` in it that names scripts inside the workspace (husky style) stays writable: see the residuals of §9.3 |
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
| workspace | host | read-only, or read-write per profile; the whole `.git` and `.tole/` always re-bound read-only on top (§9.3, #849) |
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

Every grant (workspace, `read`, `write`, the install dir of the server) is
resolved (symlinks, `..`, trailing slashes) at launch, right before the
spawn, and the **resolved** path, never the spelling in the config, is both
the source and the destination of the bind, so a link swapped in later is
either followed to its new, re-checked target on the next launch or
refused (`$HOME`, its ancestors, the data dir, `/`). Residual: a same-user
process that swaps a path component between the final resolution and
bubblewrap's mount is not stopped by user space alone; that attacker
already owns the owner's account, and the sandbox is for third-party
servers, which start inside the sandbox and cannot race a bind that
precedes them.

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

[sandbox.consumers]          # opt-in for model-driven commands (§9.2); as built, the
                             # keys are modes, not profile names: see §9.3
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
- **Flags.** `--sandbox <profile>` / `--no-sandbox`, as built in §6.2.

### 6.1 Named profiles and per-server bindings, as built (#810)

The design block above is the target shape; what is built is smaller and
user-layer only:

```toml
[sandbox]
mode = "required"

[sandbox.profiles.net]       # your own name; `server` and `strict` are built in and cannot be redefined
workspace = "read"           # none | read (default) | write
network = "host"             # none (default) | host
read = ["/opt/data"]         # extra read-only paths
write = []                   # extra writable paths

[sandbox.servers.github]     # the MCP server NAME (not `name@workspace`)
profile = "net"              # server | strict | a profile defined above
```

- A server with a `[sandbox.servers.<name>]` entry runs in that profile;
  every other server runs in the default (`[sandbox] profile`, which may
  also name a defined profile, else `server`). The grants of the named
  profile are checked at each launch exactly like the default's
  (`check_grant`: no `/`, `$HOME`, data dir, `/proc`, `/sys`, `/dev`).
  `[sandbox] skip` still wins: a skipped server is not sandboxed.
- No `hosts` and no `allowlist` network (owner decision 5), and no
  per-server `mode` yet: the mode is the global one. Consumers
  (`[sandbox.consumers]`) keep the built-in `command` profile; naming a
  profile for a consumer is not built.
- **A project config can never loosen a profile.** The whole `[sandbox]`
  table is user-layer only: layering copies the user's table and nothing
  of the project's, so a repository cannot define a profile, bind a server
  or narrow one either. `tole config check` marks such project lines
  `IGNORED` and prints, from the user layer, `sandbox effective:` lines
  with the profile of the default and of each bound server and its source.
- `tole sandbox list` shows the mode, the default profile, every profile
  and the bound servers; `tole sandbox show <server>` shows one server's
  profile, source and grants; `tole sandbox set <server> --profile <p>`
  writes `[sandbox.servers.<server>]` in the user config (the rest of the
  file is kept, the result is re-parsed and must differ only by that
  binding) and refuses under an agent session (#684). `--profile` is the
  global flag of `tole`; for `tole sandbox` it names the sandbox profile,
  not a persona. Profiles are defined by editing the file.
- **The profile in the plugin lock entry (#750, as built).** An approval
  records the profile name (`sandbox`) and `sandbox_hash` in the lock entry
  (lock format version 4): SHA-256 over the workspace mode, the network, and
  the sorted, de-duplicated extra `read` and `write` paths after resolution
  (symlinks followed as far as the path exists, so a link retargeted after
  approval is another grant); `protect` only narrows and is not hashed. At
  every start the profile the server would run in now (the per-server binding
  or the default, from the process's user-layer policy) is compared with the
  record, before the server is started and again after it lists its tools:
  another name, another hash, or no sandbox at all (mode `off`, a `skip`
  entry) is `Admission::ProfileChanged` and the server is not registered
  until the owner re-approves. A tighter profile counts too (it is the
  profile that was not approved), which keeps the rule exact and the
  `tole plugin diff` output simple. `tole plugin list` shows the profile and
  hash (`profile ...`, `changed (sandbox profile)`), `diff` prints approved
  against now. Re-approval of a changed profile is interactive only: a
  script's `--hash` consents to a tool list, never to a wider sandbox, and
  agent sessions cannot approve at all. The ledger is the authority: every
  approval is followed by a `sandbox_profile` line (name and hash, or null
  for an unsandboxed approval), and a lock whose record differs from it in
  either direction (hash edited, record dropped, record added) is treated as
  revoked, so editing the lock alone can never loosen or hide a profile
  change. A project lock can pin the same record, never replace it: a
  differing pin makes the server unavailable, a blank one changes nothing.
  *Migration:* an entry approved before has no hash and no ledger line; it is
  reported (`tole plugin list`, `tole doctor`: "approved before approvals
  recorded their sandbox profile"), not blocked, and its profile is not
  compared until its next `tole plugin approve` records it.
  *Who decides "moved":* the ledger, never the lock file alone
  (`plugin_lock::profile_moved`): a lock record that differs from the
  ledger's last `sandbox_profile` line, or a lock hash the ledger never
  recorded, is itself a move, so a script's `tole plugin approve --hash`
  cannot approve past a profile whose lock hash was edited to match a widened
  config (and so cannot rewrite the ledger's record); only a terminal can.
  *Backend:* "the profile it would run under" is `Policy::applied_profile`,
  the one helper approval, admission, `list` and `diff` share: the
  configured profile only when a working backend exists. With bwrap missing
  (or user namespaces refused) in mode `optional` the server would run
  unwrapped, so no profile applies: an approval records no sandbox, and a
  server approved with one is `ProfileChanged` (not started) until the
  owner re-approves or the sandbox works again.
  *Flags:* the gate reads the SESSION view of the policy
  (`sandbox::session_policy`, the run's `--sandbox <p>` / `--no-sandbox`
  applied), because that is what the server will actually run under: a
  `--sandbox` profile other than the approved one, or `--no-sandbox` for a
  server approved with a sandbox, is `ProfileChanged` and the server is not
  registered for that run. Approvals (`tole plugin add|approve`, which
  refuse both flags) and the interactive preflight record the CONFIGURED
  profile, never a flag's, so a one-off override cannot become the approved
  profile. The preflight asks the lock before it starts anything (a revoked,
  moved, rebuilt or profile-changed server is not started, #848). It then
  lists the never-approved servers and the approved ones that start eagerly
  (the listing is how a changed tool list is offered for re-approval, at the
  price of one extra start of each at every interactive session start; an
  approved server that starts lazily is not started), through the one-shot
  probe: no secret, ended before the session starts, sandbox required for a
  third-party server nobody verified. The probe runs under the CONFIGURED
  profile, not a `--sandbox <p>` flag's (`--no-sandbox` does not unwrap it).
  Remote servers and first-party or `skip` servers record no profile.

### 6.2 Flags, as built (#750)

`--sandbox <profile>` and `--no-sandbox` are global flags of `tole`, per
run, and only the owner at a terminal may pass them. They apply to stdio MCP
servers; `[sandbox.consumers]` is not touched by either.

| Case | `--no-sandbox` | `--sandbox <p>` |
|---|---|---|
| under an agent session (`TOLE_UNDER_SESSION`, child agent) | refused | refused |
| `tole cron`, `tole hook`, `tole sandbox`, `tole plugin` (no session server starts, or servers are approved) | refused | refused |
| `tole serve`, `tole acp`, or stdin/stderr not a terminal, mode `required` or `optional` | refused (optional counts as required without a human) | refused (#846: a profile can be wider than the configured one) |
| mode `off` | no-op | refused: it selects, it does not turn the sandbox on |
| attended | honoured for the run | profile must be built in or in the USER config |

- `--sandbox <p>` replaces the profile of every stdio server for the run,
  per-server bindings included; the mode, `skip`, caps and the grant checks
  at each launch are unchanged. It never defines a profile: a project
  config cannot define one (the user layer is the only source), and an
  unknown name stops startup. It can pick a user-defined profile wider than
  the default (that is the owner's own choice, at a terminal, never from a
  session or a script). The two flags conflict.
- "At a terminal" means stdin and stderr are both a TTY. A process outside
  an agent session that fakes a pty (`script`, `unbuffer`) still passes
  that check, for `--sandbox` and `--no-sandbox` alike; agent sessions and
  their children are refused through the session marker (#684) whatever
  they fake, the profile must be built in or defined in the owner's user
  config, and a plugin lock entry that recorded a profile is still compared
  at launch.
- Two views, so a flag cannot widen its own blast radius: the configured
  policy (`tole_core::sandbox::policy()`) is the owner's config and is never
  changed by a flag; it is what the first-party names (`skip`), the sandbox
  of a `tole plugin add` probe and the command consumers rely on. Only
  `sandbox::session_policy()` (connection keys, idle stop, the cap, the
  startup line, `launch_session`) applies the override, and it ignores
  both flags once `mark_unattended()` ran, so a flag that slipped past
  the host check still cannot unsandbox a cron job or a serve session.
  `tole plugin` refuses both flags.
- `--no-config` (#855) drops the whole user config, the sandbox mode, the
  consumers and the secret release with it, so it follows the same rule as
  the two flags: refused under an agent session (`TOLE_UNDER_SESSION`, child
  agent) and for `tole serve`, `tole acp` and `tole mcp`; honoured for the
  owner at a terminal (`no_config_guard`). The marker is an environment
  variable, so this covers processes tole started, not a script that strips
  it; a different `CODECORA_HOME` is the owner's own choice and is not
  refused.
- A sandbox never replaces an approval (#684, #700, #714): the flags change
  where a server runs, not whether it is approved; the plugin lock, ledger
  and `Admission` checks run as before.
- Logged: one startup line and a `tole doctor` line say that a flag
  overrode the config (mode or profile it replaced), and doctor lists the
  grants of the effective policy. Lazy start is section 9.1.2; not built:
  the profile name and hash in the lock entry.

## 7. Default profiles

| Profile | Workspace | Network | For |
|---|---|---|---|
| `server` | read | none | third-party stdio MCP servers |
| `command` | write (with `.git` and `.tole/` read-only) | none | `run_command`, `job_start`, cron scripts, `verify`, `turnend` |
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
  without approval (#676). They are the strongest case for opting in. Both
  are consumers since #748's second slice (§9.4).
- Child agents (`agent_start`, `delegate`) are tole processes with their own
  tools; they are not wrapped. Their own model commands follow their own
  `[sandbox.consumers]`.

### 9.3 Model commands as implemented (#748, first slice)

The first slice made `run_command` and `job_start` consumers; the second
(§9.4) added `verify` and `turnend`. Cron scripts and deliveries run as
before (third slice). Every consumer is off by default; the owner decided
`verify` and `turnend` become sandboxed by default later (§9.4, migration),
and nothing here changes their defaults.

The owner's `[sandbox]`, `[sandbox.consumers]`, `[secrets]` and
`[sensitive_files]` apply to a cron AGENT run (its MCP servers, `run_command`,
`job_start`, verify and stop gates) the same as to `tole run`, since #863:
`tole cron run` and `tole cron tick` execute jobs in the process, so they load
the user config even though `cron` is on the config skip list (never the
project config; a user config that does not parse stops them, it does not run
the job with defaults). The other `cron` subcommands, which only edit files,
still skip it. A cron SCRIPT job is not sandboxed yet (third slice).

```toml
[sandbox.consumers]            # user config only; every key optional
run_command = "required"       # "off" (default) | "optional" | "required"
job_start   = "optional"
verify      = "required"       # the mission --verify command (§9.4)
turnend     = "required"       # the on_turnend stop gates (§9.4)
network     = "none"           # "none" (default) | "host" (explicit opt-in)
cpu_secs     = 3600            # 1 to 86400
memory_mb    = 16384           # 256 to 1048576, address space per process
processes    = 4096            # 16 to 65536, processes and threads
file_size_mb = 4096            # 1 to 1048576, largest file a process writes
```

- **Profile.** The built-in `command` profile (`tole_core::sandbox::command`):
  the workspace writable, `.git` (the whole directory) and `.tole` read-only on
  top of it (next item), the data dir and `$HOME` absent, a private `/proc`, writable
  `/tmp` and scratch (tmpfs, gone with the sandbox), no network unless
  `network = "host"`, and the fixed flags of §5 (`--unshare-all
  --unshare-user --disable-userns --cap-drop ALL --new-session`,
  `--die-with-parent` for `run_command`, not for the detached `job_start`).
  There are no per-consumer profile names yet: `read` / `write` extras and the
  `[sandbox.profiles]` table are later work.
- **Workspace.** The session's own directory, passed with every call
  (`run_command`: its working directory; `job_start`: the workspace root), never
  kept in process state. It is resolved and refused like every grant (`$HOME`,
  an ancestor of it, the data dir, `/`), and then the resolved path is the bind.
  The protected paths are inspected at launch too (see the next item).
- **Protected paths (#849).** The paths that run code later on the owner's
  host are bound read-only: the WHOLE `.git` directory, and `.tole`. Locking a
  few leaves of a writable `.git` does not work, and both attempts were
  reproduced and rejected: (1) with only `.git/hooks` and `.git/config` read-only,
  `mv .git .g && cp -a .g .git` gave the command a plain writable copy (a mount
  point cannot be renamed, but its ancestor can); (2) with `.git` made a
  writable mount point, git still honours `commondir` (and `gitdir`,
  `config.worktree`, `info/`, `modules/*/{hooks,config}`, `worktrees/*`,
  `objects/info/alternates`) INSIDE the gitdir, so
  `echo ../evil > .git/commondir` pointed a later `git commit` or `git status`
  on the host at a planted gitdir whose `hooks` and `core.fsmonitor` it ran, and
  `rm -rf .git` still destroyed HEAD, objects and refs, and a leaf that did not
  exist (a repository made with `git init --template=`) could be created. A
  read-only `.git` is one mount point: it cannot be renamed, removed, replaced
  or written, nothing can be created in it, a hard link to its files made
  from inside fails (`EXDEV`, the kernel refuses links across mounts), and its
  tree is byte for byte unchanged from inside. The same holds for `.tole`.

  **Cost.** Inside a sandboxed `run_command` or `job_start` (the other
  consumers get the same profile as they land), git commands that write fail: `add`, `commit`, `checkout`, `branch`, `stash`,
  `rebase`, `gc`, `config`, `remote add` (the error is git's own, "Read-only file
  system", `git stash` exits 1 with no message). Reads work: `log`, `diff`,
  `show`, `status` (without refreshing the index, no hang), `rev-parse`,
  `branch --list`, `ls-files`. A model cannot commit from inside a sandboxed
  command; it does so through a tool outside the sandbox, or the owner does.
  There is no setting to make `.git` writable.

  **The sessions directory (#866).** Sessions are append-only JSONL, and a
  command that could rewrite them would break resume, the audit and the cost
  report. The default `.tole/sessions` is under `.tole`, so it is read-only
  already. When the owner moved it (`-s`, the `sessions_dir` key) into the
  workspace, the host tells the sandbox (`command::set_sessions_dir`, an
  absolute path) and the SAME plan (`protect_plan`, re-checked at every launch by
  `recheck_plan`) binds it read-only as one more protected path: a command can
  read the entries but not write, create, rename, remove or replace anything in
  it, while tole itself, a host process outside the sandbox, keeps writing
  them. By the rules of `.tole`: it must be a real directory directly under the
  workspace root (a deeper one could be moved away through its parent, which a
  mount cannot prevent, and replaced by a planted copy tole then writes into:
  refused, with the remedy `.tole/sessions`, `./sessions` or a directory outside
  the workspace); a symbolic link, a file, the workspace itself, or a path that
  reaches the workspace through a link is refused too; one outside the
  workspace is not in the sandbox's view; one that does not exist yet is not
  protected (like `.tole`; the session is created before the first command runs,
  so in practice it exists). The refusal follows the consumer's policy like
  every other refusal of the plan.

  **Nested gitdirs.** The owner's git also runs a nested repository's config
  and hooks when it is run in the OUTER repository (a planted
  `core.fsmonitor` in a submodule's config fires on a plain `git status`,
  `git diff` or `git add -A` there, not only inside the submodule). So a `.git`
  gitdir FILE (top level, submodule, worktree) is bound read-only itself, and
  so is the directory it points to when that is inside the workspace (outside
  the workspace it is not in the sandbox's view at all). The nested gitdirs
  that can be named cheaply and deterministically are protected without
  scanning the workspace: every `path` of `.gitmodules` (at most 64, each of
  which must be an initialized submodule with a `.git`) gets its
  `.git` bound the same way, `.gitmodules` itself is bound read-only (it is the
  input of the plan: a command that could rewrite it would take a submodule out
  of the next launch's plan), and the whole `.tole-worktrees` directory
  (child-agent worktrees) is read-only, so no entry can be removed or replaced
  and every worktree `.git` file in it is covered. Cost: a sandboxed command
  cannot change files inside a child agent's worktree.

  The layouts listed here are REFUSED, not half protected (the launch
  fails with the cause; `optional` follows its fallback policy): a linked
  worktree (`git worktree add`) whose `.git` file lies inside the workspace but
  not under `.tole-worktrees` (found through the registry
  `<gitdir>/worktrees/<id>/gitdir` of the gitdir in effect, the top-level `.git`
  directory or the target of a gitdir file such as a separate git dir; a
  worktree outside the workspace is not in the sandbox's view, and a stale
  pointer whose path is gone is fine; a `worktrees` that is a link or not a
  directory, a back-pointer that is a link or not a regular file, more than 64
  entries or an oversized pointer are refused too, never skipped; remove the
  worktree with `git worktree remove` or keep it outside the workspace); a submodule
  listed in `.gitmodules` that is not initialized (a missing path, an empty
  directory as after a clone without `--recursive`, a directory without `.git`,
  a regular file: there is no `.git` to bind, and a command could `git init` one
  with a planted `core.fsmonitor` that the owner's `git status` in the outer
  repository runs; initialize it with `git submodule update --init` or remove
  it); a registered
  submodule that has a `.gitmodules` of its own (nested submodules: only the
  top-level `.gitmodules` is read, so the inner gitdirs would be unprotected;
  any depth, each listed submodule directory is checked without following
  links); a gitdir directory that is not covered by an already read-only
  path and has a `commondir` or `config.worktree` file (git would be sent to
  another directory for its config and hooks, writable when it lies inside the
  workspace), including the top-level `.git` directory itself; a gitdir file
  whose target inside the workspace does not exist (a command could create
  it); a `.gitmodules` with a second hard link. A gitdir file whose target is
  outside the workspace is not in the sandbox's view and stays accepted. A
  worktree's `commondir` inside the read-only `.git` (`.git/worktrees/<id>`) is
  normal and not refused.
  Git reads `.gitmodules` as a config file and the file is writable from the
  sandbox, so only a plain subset is accepted and anything git could read
  differently refuses the launch: `[submodule "name"]` sections with `key =
  value` lines, blank lines and whole-line `#`/`;` comments; a comment after a
  value, a quote, an escape or a continuation, another section or a key outside
  a submodule section is refused (`path = sub # note` is `sub` to git). A
  SYMBOLIC LINK anywhere in that chain (`.git`, `.git/hooks`, `.git/config`,
  `.tole`, `.gitmodules`, a submodule path, `.tole-worktrees`, a gitdir target), a path that leaves the workspace, an unreadable
  `.gitmodules` or gitdir file larger than 64 KiB or 4 KiB (never read in part), or more than 64 entries refuses the launch (for
  `optional`, the fallback policy applies) with the cause in the reason:
  a mount cannot protect a link (the command removes it and makes a real
  directory, or the link leads to a writable place). A read-only mount stops
  writes by path, not by inode, so what the host's git runs inside a gitdir
  directory (the top-level `.git`, a nested one, the target of a gitdir file) is
  also checked: `hooks` and `config` are not links, `config` and the files of
  `hooks` have no second hard link (a name made earlier would stay writable),
  no entry of `hooks` (at most 256) is a link, and a gitdir file itself has one
  name. A FIFO or device named `.git`, `.gitmodules` or a submodule's `.git` is
  never opened (it would block the launch): `.gitmodules` that is not a regular
  file refuses the launch, a `.git` that is neither directory nor file is bound
  without being read. Links and hard links elsewhere inside `.git` (not `hooks`
  or `config`) are not inspected.

  **Plan and launch.** The protected paths are planned, the argument list is
  built, and the plan is made again right before the command line is handed
  out: a `.git` that appeared in between, or a directory swapped for a link,
  refuses the launch. Only the window between that check and the mount in
  `bwrap` is left, which a same-user process racing the path could use (§5).

- **Program and argv.** The command line is split to argv by the tool as
  before and passed to `bwrap` after `--`; the program is found by bare name
  on the absolute entries of tole's `PATH` (a relative entry would mean
  something else inside) and run by its resolved path. A program outside the
  system dirs gets its own directory bound read-only (resolved, refused when it
  is `$HOME` or holds it).
- **Environment.** Unchanged: the ENV-1 scrub with the `[secrets]` release for
  the tool is applied to the `bwrap` process, so a granted secret reaches the
  command as it does today and nothing else does; `TOLE_UNDER_SESSION` is
  still set. No value goes through `--setenv` except `HOME` and `TMPDIR`
  (`/scratch`). File descriptors above 2 are marked close-on-exec before `bwrap`
  starts, so nothing tole's parent leaked into tole reaches the sandbox.
- **Limits.** `RLIMIT_CPU`, `RLIMIT_AS`, `RLIMIT_NPROC` and `RLIMIT_FSIZE` are
  set (soft and hard, never above what tole already has) between fork and exec
  of `bwrap`, so everything inside inherits them and cannot raise them. A failure
  to set one stops the spawn. The kernel counts `RLIMIT_NPROC` per user
  namespace, so only the sandbox's own processes are counted. Linux only. The
  `/tmp` tmpfs is not size-limited (`--size` needs a newer bubblewrap than the
  0.8 minimum); a command can fill memory with it.
- **Policy.** `off` is today's behaviour. `required` refuses when there is no
  working sandbox or the workspace cannot be granted. `optional` runs
  unsandboxed with a warning (printed once per process and tool) when a human
  is attached, and refuses in an unattended run (`tole serve`, cron). On macOS
  and Windows there is no sandbox: `required` refuses with that reason and
  `optional` runs unsandboxed with the warning. A program that is not on `PATH`
  is refused in every mode. A sandbox never replaces an approval: the tool's
  risk, the approver, the destructive-argv and session-guard checks run before
  the launch, exactly as before, and the result (status, stdout, stderr, caps,
  the 30 s timeout) is the same shape.
- **Lifecycle.** A timeout or a cancel kills the `bwrap` process; with
  `--die-with-parent` its init dies with it, and the PID namespace takes every
  descendant along. A job has no `--die-with-parent`, so a job that was spawned
  but cannot be tracked (pid file not written) is stopped through its group and
  the direct children of its launcher (`jobs::kill_detached`).
- **`tole doctor`** prints the consumers' modes, the network and the limits, says
  cron scripts and deliveries are not sandboxed yet, and fails
  when a `required` consumer has no working sandbox (a warning for `optional`;
  an error for `optional` `verify`, which a mission treats as `required`).
  `tole config check` lists the `sandbox.consumers.*` keys.
  For each sandboxed stdio server (from `mcp_server` in the user config) and
  each consumer that is on, it also prints the mode, the workspace access, the
  network, every extra `read` / `write` path, and the decision (sandboxed,
  unsandboxed because there is no backend and the mode is `optional`, or
  refused). A grant that touches `$HOME` (marked `REFUSED`, it is never bound)
  or a credential directory (`.ssh`, `.gnupg`, `.aws`, `.azure`, `.kube`,
  `.docker`, `.netrc`, `.config/gcloud`; marked `SENSITIVE`) is flagged on its line.
- **Residual.** What is protected is what existed at launch, and only what
  is listed above:
  - A `.git` or `.tole` that did not exist at launch is not protected (an
    empty read-only mount over a missing `.git` would create a directory in
    the real workspace and break repository discovery; a launch creates
    nothing there). A `git init` run inside the sandbox, or a `.git` made after
    a detached `job_start` began, leaves a repository whose hooks and config
    the host runs later. "Nothing can be created in `.git` or `.tole`" holds
    only when they existed at launch.
  - `.git/config` is read-only, but the settings in it are not checked: a
    `core.hooksPath`, `include.path`, `core.fsmonitor` or similar that already
    points to scripts or files inside the workspace (husky-style hook
    managers) leaves those scripts writable, and the host runs them.
  - A repository whose `commondir` or gitdir lives outside the workspace is
    outside the sandbox's view and not this plan's concern.
  - A nested repository that is not named by `.gitmodules` (an initialized
    entry) or inside `.tole-worktrees` (a vendored clone, any `x/.git`) is part
    of the writable workspace and is NOT protected: its config and hooks can be
    written, and the owner's `git status` in the outer repository can fire a
    planted `core.fsmonitor`.
  - A gitlink recorded in the index with no `.gitmodules` entry (the usual
    accidental embedded repository), including a directory that is empty or
    missing: nothing names it, so a sandboxed command can `git init` there and
    plant `core.fsmonitor` or a hook, and the host's plain `git status` in the
    outer repository runs it. This is not the same as the missing top-level
    `.git`/`.tole` above.
  - Only the registry of the top-level gitdir is read for linked worktrees: a
    worktree of a SUBMODULE (registered in `.git/modules/<name>/worktrees`) that
    lies elsewhere in the workspace is not refused and not protected.
  - A gitdir file or worktree back-pointer that names a path lexically outside
    the workspace but reaching it through an external symlink alias is classed
    as outside (git writes real paths, so this needs a hand-written pointer).
  - Files outside `.git` that run on the owner's host later cannot be
    protected by a mount: `.envrc`, Makefile, package scripts, `build.rs`, CI
    configs, `.husky`, `.pre-commit-config.yaml`, lefthook,
    `.vscode/tasks.json`, `.gitattributes` and the like.
  - Hard links and links made before the paths were protected: only `config`,
    the files of `hooks` and the gitdir files are checked, so another name for
    some other file inside `.git` (`commondir`, `info/*`, `config.worktree`)
    that was made while `.git` was unprotected is not detected.
  - A diff does not show `.git` internals or a nested repository's config. Treat
    a workspace a sandboxed command wrote into as untrusted until the owner has
    inspected what git will run (`git config --list --show-origin` in the outer
    repository and in each nested one, the hooks directories, the gitdir files),
    or work from a fresh clone.
  - A same-user process racing a
  path component between resolution and the mount is not stopped (§5). The
  limits are per process, not a cgroup. Toolchain caches (`~/.cargo`, `~/.npm`)
  are not visible; the profile has no `read` extras yet.

### 9.4 `verify` and `turnend` as implemented (#748, second slice)

```toml
[sandbox.consumers]
verify  = "required"           # the mission --verify command
turnend = "required"           # the on_turnend stop gates
```

Both are opt-in, exactly like the first slice: default `off`, `optional` or
`required` in the user config only, the same `command` profile, limits and
network setting. Nothing about them is sandboxed by default in this slice.

- **Call sites.** `verify`: `run_verify` in `tole-cli`'s `mission.rs`, the
  only place the `--verify` string is run (`sh -c <string>`, the string stays
  one argument). `turnend`: `ProcessHook::run_raw` in `tole-core`'s
  `hooks.rs`, the only place a stop gate is spawned (called by
  `ToolRegistry::turnend_denial` from the turn loop, so every host that wires
  `on_turnend` goes through it). Pre- and post-tool hooks are a different
  consumer (`hook`) and are not wrapped. Both call
  `sandbox::command::command_in_cwd`, which is `command_for` with the process's
  working directory as the workspace.
- **Same semantics.** Off, the spawned command is byte for byte the old one.
  On, exit status, stdout and stderr capture, the timeouts (`--verify-timeout`,
  the 30 s gate budget), the working directory (the process's own, which is what
  both inherited; `--workspace` does not change it), the stdin payload of a
  gate, and the ENV-1 scrub with the `[secrets]` release for `verify` and
  `turnend` are the same. The scrub is applied to the `bwrap` process, whose
  environment its child inherits.
- **What a gate's own files are.** A gate script inside the writable workspace
  (`./gate.sh`) can be rewritten by the agent: the profile protects `.git`
  and `.tole`, not the gate. The sandbox limits what the gate can do (no
  network, no `$HOME`, nothing outside the workspace written), not whether the
  agent can change what the gate checks. Keep the gate outside the workspace
  (an absolute path in a directory the sandbox binds read-only), or inside
  `.tole`, when the check must be tamper-proof. `tole serve` and `tole acp`
  bind the process's working directory as the workspace, so a gate there is
  refused when that directory is `$HOME` or `/`.
- **Programs.** `verify` runs `sh` by bare name from `PATH`. A stop gate is the
  owner's command line split on whitespace, so its program may be a path
  (`./gate.sh`, `/opt/bin/check`): a path is resolved (only its directory, so the
  program keeps its name), must be an executable file, and its directory is bound
  read-only unless it is inside the workspace or a system dir; a gate that lives
  in `$HOME` itself is refused by the usual grant check. The two tools keep
  taking bare names only.
- **Unattended.** A mission is an autonomous run with nobody to answer, so for
  `verify` `optional` always counts as `required` (the consumer says so itself;
  the process flag is not needed). A stop gate is treated the same way: it
  runs while the model is finishing, with nobody answering a prompt, so for
  `turnend` `optional` also always counts as `required` (an attended chat
  gets no unsandboxed fallback for a gate; `run_command` and `job_start` keep
  theirs).
- **A refusal is never a pass.** With `required` (or `optional`, which is the
  same here) and no working sandbox, or a workspace layout the `command`
  profile refuses (§9.3: uninitialized or nested submodule, linked worktree
  inside the workspace, unprotectable `.git` link chain; the refusal carries the
  profile's reason), never an unsandboxed run:
  - `verify`: the result is `Refused`, not `Failed`. A start-up check runs before
    the first turn (the same planning a launch does: backend, workspace grant,
    protected paths), so the mission ends before any model turn is spent, with
    status `verify_refused`, a non-zero exit and `MISSION_COMPLETE` not
    honoured: the model cannot fix a missing sandbox, so the refusal is not fed
    back or counted against the three-strike cap (`verify_failures` stays 0).
    The session stays resumable (`tole mission --resume ID` after installing
    bubblewrap or relaxing the setting). The skill curator records no outcome
    (it is the owner's setting, not the work).
  - `turnend`: the gate is DENIED, with the refusal as its reason (exit code
    126 from `run_raw`). The final message does not commit, the model sees
    `stop gate: denied by turnend gate: turnend: not run, sandbox required ...`,
    and the denial cap (3) settles the turn `StopGateBlocked` (`tole run` exit
    6). A refusal is a deny and never a pass; it is not reported as the gate's
    own verdict (the gate did not run). An infrastructure failure of an
    UNSANDBOXED gate (spawn error, timeout) stays non-blocking as before
    (#110). For a SANDBOXED gate (it went through the launcher) a launcher that
    cannot even start is a deny too, and so is a timeout (#860): the gate is
    killed, the reason says "the stop gate timed out after Ns inside the sandbox
    and was killed; a timeout is not a pass", and it counts toward the
    denial cap like any denial; a gate that is lost for another reason while
    running is a deny as well. Before #860 a sandboxed timeout was an `Err` the
    caller logged, and the gate passed.
  - A failing command inside the sandbox is an ordinary failure, same as plain:
    `verify_failed` after three strikes, a non-zero gate exit denies.
- **The launcher failing is not the command failing.** A sandbox that passed
  the check can still fail to start the command: `bwrap` gone after the
  self-test, a namespace refused, a mount that cannot be made, a resource limit
  `bwrap` cannot live under. That is a `Refused` for verify (a spawn error of
  the launcher, or a non-zero exit with no command exit code from `bwrap`),
  reported after the turn that ran, and the model is not told to fix it; for a
  stop gate it is a deny with a reason that says the sandbox failed, not that the
  gate failed. The distinction is made with `bwrap --json-status-fd`: `bwrap`
  writes `{"exit-code": N}` only when the command itself ran and ended, and
  nothing of the kind for a setup or exec failure (verified: both a missing bind
  source and a missing program print `bwrap: ...`, exit 1 and report no exit
  code); `child-pid` is written before the mounts and proves nothing. A
  stderr-prefix heuristic was rejected: the command could print `bwrap:`
  itself. The report is written by `bwrap` itself, last, when the command has
  ended. Whether the command can reach the status descriptor depends on the
  `bwrap` version: with bubblewrap 0.9.0 the descriptor is closed inside the
  sandbox (`echo >&3` fails with `EBADF`), and a version that leaves it open
  would let the command write a line of its own, but the report is parsed
  as a whole and `bwrap`'s own `exit-code` line comes last and wins. A report
  that cannot be parsed, or has no `exit-code`, is read as "the launcher
  failed": the result is `Refused` (verify) or a deny (gate), never a pass.
  The exit codes 125, 126 and 127 of the command itself are its own failure
  (`Failed`), not a launcher failure: only a missing `exit-code` in the report
  means the launcher failed. A command killed by a signal (no exit code) is
  still counted as its own failure. Only exit 0 passes.
- **Tests.** Argv builder tests per flag for both consumers
  (`command_tests.rs`), policy without bubblewrap (`command_sandbox_none.rs`,
  `verify_sandbox.rs`) and with it (`stopgate_sandbox.rs`, `verify_sandbox.rs`,
  `command_sandbox.rs`; CI's `Sandbox (bubblewrap)` job runs them with
  `TOLE_REQUIRE_BWRAP=1`).
- **A sandboxed verify or gate can fail every time for environmental
  reasons.** The model is told "verify FAILED" (or the gate's output) and the
  three strikes or denials apply as for any failure, but none of these is the
  model's to fix: a toolchain under `~/.cargo` or `~/.rustup` is not visible (no
  `$HOME`, no `read` extras yet), so `cargo` is not found (exit 127); the
  network is `none` unless the owner wrote `network = "host"`, so a fetch or a
  registry login fails; and `RLIMIT_AS` (`memory_mb`, 16 GiB by default) counts
  address space, which address-hungry tooling can exceed. The semantics are
  unchanged on purpose. `tole doctor` says so when `verify` or `turnend` is on,
  and warns by name when `~/.cargo/bin` or `~/.rustup` exists. Until the `read`
  extras land, a `--verify` or stop gate that needs such a toolchain should
  stay `off`, or the toolchain should live under a system directory.
- **Residual.** As §9.3. `--workspace` is not the sandbox workspace, the
  process's working directory is (the one both ran in before).

**Migration plan (not in this slice).** The owner decided (§16.1) that `verify`
and `turnend` become sandboxed by default in a later phase. The plan, for that
phase's own PR: (1) land the `read` extras so a toolchain can be granted; (2)
the default turns from `off` to `optional` for both, with a release note and a
`tole doctor` line saying so, so an attended run on a machine without
bubblewrap keeps working with a warning while a mission already refuses (it is
unattended); (3) a later release moves it to `required` where a backend
exists, `off` stays the explicit opt-out in the user config. Until that PR the
default stays `off`.

### 9.1.1 Life of a sandboxed server

A wrapped stdio server is keyed by server name and resolved workspace, and
belongs to the sessions that registered it: each session's tool registry
holds a claim, and the server is stopped (process killed, config dropped)
when the last claim is released, so dropping one session never stops a
server another session with the same key still uses. A server unused for
`[sandbox] idle_secs` (default 900, 1 to 86400) is stopped too and starts
again on its session's next call. `[sandbox] max_servers` (default 16, 1 to
64) caps the wrapped servers a process runs at once: a launch past it is
refused with a message naming the limit, never by evicting another
session's server, and a released slot is reusable. Servers that are not
wrapped (sandbox off, HTTP, skipped) keep the old process-lifetime
behaviour.

### 9.1.2 Lazy start, as built (#750)

Opt-in: `[sandbox] lazy_start = true` in the user config (default off; a
project config cannot set it). It applies to a stdio server that is wrapped
by the sandbox (not `skip`, not `--no-sandbox`) and whose lock entry stored
a snapshot that is usable as is; every other server starts at session start
as before.

- **Eligible snapshot** (`tole_core::mcp::lazy_tools`): non-empty; every
  tool name is a plain name (`A-Za-z0-9_-.`, at most 64); every stored
  description and schema still hashes to its recorded hash (so none is cut,
  not stored, redacted or cleaned); and the manifest rebuilt from it hashes
  to the entry's `manifest_hash` (a stale or hand-edited snapshot fails).
  Otherwise the server starts eagerly.
- **Registration** adds the tools from the snapshot and starts nothing. The
  host still runs the lock's pre-start decision (`refuse_unseen`) at
  registration, so an unapproved, revoked or moved server registers nothing,
  exactly as before.
- **First call** (after the usual per-call approval, which is never
  skipped) runs, in order, the same steps an eager start runs: the lock's
  pre-start decision again (a binary swapped since registration is never
  started), the start through the normal launch path (sandbox plan, the
  startup line logged once, `max_servers`), and the lock's check of the live
  manifest and version. A refusal returns the note, stops the server,
  records the ledger line the eager check records, and refuses every later
  call of the session without starting it again; no call ever reaches the
  server. A start that merely fails (cap, program error) is retried by the
  next call. Idle stop and restart behave as for an eager server.
- **Not offered while dormant:** resources, prompts and the `/mcp:` listing
  of a lazy server (the registry marks a server admitted, #680, only after a
  live check). They are not offered after the first call either in this
  slice; start the server eagerly (leave `lazy_start` off) to use them.
- `tole doctor` says when lazy start is on.

## 10. Failure behaviour

At startup tole resolves the backend once: `bwrap` on `PATH`, version
≥ 0.8, then a self-test spawn (`bwrap … -- /bin/true`, ~8 ms) to catch
kernels or containers that refuse user namespaces (Docker's default
seccomp profile, Ubuntu's AppArmor userns restriction without bwrap's
profile). The result is cached for the process.

| Mode | Sandbox works | Sandbox unavailable |
|---|---|---|
| `required` | runs inside | **not started**: a startup line names the server, the reason and the remedy (install bubblewrap, or set `mode = "optional"` in the user config); its tools are not registered; a call to one returns the same message |
| `optional` | runs inside | runs **unsandboxed**, with a warning at every start and a ledger entry; refused for unverified servers and, by owner decision 2, in unattended runs. Refused too when its approval recorded a sandbox profile and the wrapper cannot be built now (a granted path gone, #848): `ProfileChanged` |
| `off` | runs unsandboxed | runs unsandboxed |

A consumer in `[sandbox.consumers]` behaves as `required`: the command is
refused with the reason, never run unsandboxed. A sandbox that starts but
whose program fails (missing path, denied network) is an ordinary error
of that program, with tole's note naming the profile.

## 11. What is logged

Names only, never values or argument contents:

- At startup (stderr, and `tole doctor`): backend and version, or why
  none is available; each stdio server with its mode and profile name. The
  server line is logged once per server and process, the first time it
  starts: `sandboxed`, `unsandboxed` (no backend, `optional`, a human
  attached), or refused (`required`, or `optional` in an unattended run).
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
| #676 `--verify` and stop gates with the full environment (fixed by the scrub) | With `verify` / `turnend` opted in (§9.4): the code the agent wrote can no longer read `$HOME`, write the data dir or `.git/hooks`, or reach the network. | Opt-in, not default. A secret granted to `verify` is still readable by the test code. The verify result itself still decides mission completion. |
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
   `turnend`; rlimits; per-consumer tests; migration note. First slice done
   (#748, §9.3): `run_command`, `job_start`, rlimits. Second slice done
   (§9.4): `verify` and `turnend`, with the migration plan. Cron scripts and
   deliveries follow.
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
