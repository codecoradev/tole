# Threat Model — tole

Status: initial version (2026-09-12), mapped against the OWASP Agentic/LLM
Top 10 checklist (issue #72); each shipped feature adds its own row or section,
and the residuals were last reviewed against the code for 0.11.0 (2026-11-14). Every item resolves to **control** (where it
lives) / **gap** (tracked) / **N/A** (with rationale).

## Assets

| Asset | Where | Notes |
|---|---|---|
| Provider API key | `TOLE_/OPENAI_*` env | Never persisted; scrubbed from wire errors/messages |
| Session logs (JSONL) | sessions dir | Transcript incl. tool results, stored in the redacted form since #596 (lines written before stay raw) — local only |
| Job logs | `<workspace>/tole-jobs/` | Child stdout/stderr, attacker-influenceable content |
| Workspace files | `--workspace` dir (default cwd) | Readable AND writable by the model via tools |
| Host env of child processes | inherited by spawned commands | See ENV-1 |
| Subprocess execution authority | run_command / job_start / git / gh | The model chooses argv within argv-split + jail rules |

## Trust boundaries

1. **Provider responses → context.** The model's output becomes intent
   entries; tool args are argv-validated but otherwise untrusted.
2. **Tool results → context.** File contents, job logs, command stdout flow
   back into the transcript and are sent to the provider. A malicious
   workspace file or a compromised job subprocess can inject
   instructions here (OWASP: prompt injection).
3. **Workspace → filesystem.** File tools jail to `--workspace`
   (component-validated walk using `symlink_metadata`, final component
   opened with `O_NOFOLLOW` on unix, #68; a parent-directory swap between
   walk and open remains a residual race — see Deliberate limitations);
   `run_command`/`job_start`
   run with cwd jailed but FULL process authority — the jail bounds *cwd*,
   not capability.
4. **Child env.** Spawned commands get the host environment scrubbed of
   secret-shaped variables (ENV-1) except what the `[secrets]` release grants
   the launching tool (#598).

## OWASP Agentic/LLM Top-10 mapping

| OWASP item | Status | Where / rationale |
|---|---|---|
| Prompt injection (direct/indirect) | **control + documented residual** | Tool results on the wire are wrapped in `TOOL_RESULT_BEGIN/END` fences with inner fence markers neutralized (this fix); secrets scrubbed (E11). Residual: an injected instruction inside data can still steer the model — no framework fully solves this; delimiters + user-visible tool audit lines (`what: …`) limit blast radius. |
| Insecure output handling | **control** | Same fence treatment; result sizes capped (read_file 1 MiB file cap and 20,000-char head cap per call, run_command 4000/2000 chars, job_poll 8 KiB window). |
| Excessive agency / tool misuse | **control (defense-in-depth) + documented residual** | Primary control: the per-call approval gate (every non-ReadOnly spawn needs consent). Secondary: `check_destructive_argv` blocklist refuses headline catastrophic patterns (teardown tools, recursive rm escaping the workspace, wrapper-nested variants) in BOTH `run_command` and `job_start`. The blocklist is best-effort by nature — wrapper/interpreter bypasses (`sudo apt ...`, `timeout 5 dd ...`, `find / -delete`, `python -c rmtree`) are expected and accepted residual risk. A real sandbox is now in scope (#601, owner decision 6 of `docs/sandbox-design.md`): the first slice runs third-party stdio MCP servers inside bubblewrap on Linux, opt-in through `[sandbox] mode` in the user config (off by default; wrapper `tole_core::sandbox`): private `/proc`, `$HOME` and the tole data dir absent, workspace read-only, no network unless the profile says `host`, no capabilities, no nested user namespace, bwrap started with the scrubbed environment and no secret ever in its argv. `required` refuses to start a server when there is no working bwrap, and an `optional` sandbox counts as `required` in unattended runs (`tole serve`, cron); an `optional` server that does start unsandboxed (a human attached, no working bwrap) is announced on stderr and recorded as a `sandbox_fallback` line in the plugin ledger (server, profile name and hash, reason; append-only, once per server and session, #750); `TOLE_VERBOSE=1` prints the redacted bwrap command line of a sandboxed launch to stderr only, never to the session log (a secret tole does not know, under an innocent flag name, is not recognised). Since #748 `run_command` and `job_start` can opt in too (`[sandbox.consumers] run_command` / `job_start` = `off` (default) | `optional` | `required`, user config only): the built-in `command` profile (workspace writable, the top-level `.git` directory and `.tole` read-only, and the effective sessions directory when it is inside the workspace (#866: a moved one, or the default relative to a working directory below `--workspace`; a layout the mount cannot protect, such as one deeper than a direct child, is refused; paths are resolved by the operating system, a `..` after a link is not collapsed as text) (#849: when they existed at launch they cannot be renamed, removed, replaced or written, so a sandboxed command cannot commit; the `.git` files and gitdirs of the `.gitmodules` submodules, of `.tole-worktrees` and of a gitdir file's target inside the workspace are read-only too; a link in that chain is refused; refused, not protected: a linked worktree whose `.git` file is inside the workspace but outside `.tole-worktrees`, an uninitialized `.gitmodules` submodule, nested submodules, a `commondir`/`config.worktree` redirect of an in-workspace gitdir, and an in-workspace gitdir target that does not exist), data dir and `$HOME` absent, no network unless `network = "host"`, the same hardening flags, the ENV-1 scrub and `[secrets]` release unchanged, CPU, address-space, process-count and file-size rlimits, no inherited descriptor above 2, the command passed as argv after `--`); `required` refuses without a working sandbox, `optional` runs unsandboxed with a warning only when a human is attached and refuses in unattended runs; a sandbox never replaces the approval of the tool. The mission `--verify` command and the `on_turnend` stop gates opt in the same way (`[sandbox.consumers] verify` / `turnend`): both are always treated as unattended (`optional` counts as `required`), a refused verify (no sandbox, a workspace layout the profile refuses, or a launcher that fails to start) ends the mission `verify_refused` and never passes, and a refused stop gate (the same causes, a launcher that fails, and a gate killed at its timeout inside the sandbox, #860) is a deny of the turn, never a pass (an unsandboxed gate keeps the non-blocking timeout of #110). Cron script jobs and `command:` deliveries opt in with `[sandbox.consumers] cron` (#748, third slice; the same profile, the job's `workdir` is the workspace, so a workdir below a repository root does not see the repository's `.git`): a cron job is unattended, so `optional` counts as `required` and there is no unsandboxed fallback; a refused job (no backend, a workdir or layout the profile refuses, a workdir that holds or is inside the cron dir (`TOLE_CRON_DIR`; the grant check only knows the default data dir, so the launcher checks the cron dir itself and fails closed when it cannot resolve it), a launcher that fails or cannot be spawned) settles `error` with `usage.sandbox_refused` and the reason in the run log, a refused or failed delivery is a `failed` delivery record, and a launcher failure is never a pass; a delivery that needs the network or `$HOME` fails under the profile (`network = "host"` is shared by every consumer, there is no per-consumer grant); cron agent jobs use `run_command` and `job_start`. Residual: a gate script inside the writable workspace (`./gate.sh`) can be rewritten by the agent (the profile protects `.git` and `.tole`, not the gate; keep it outside the workspace), and `tole serve` / `tole acp` bind the process working directory, so a gate there is refused when it is `$HOME` or `/`; a missing user config is announced where it may be a wrong directory (`CODECORA_HOME`, no resolvable data directory, a session, serve/acp/mcp) and an uninspectable path is a startup error, but a process that controls its environment can still point `CODECORA_HOME` elsewhere, so the boundary is the command sandbox that masks the data directory (#864); `--no-config` and the policy-loosening flags need a terminal (stdin and stderr) when the user config sets `[sandbox]` or `[secrets]`, marker or not (#867; a user config that sets neither is unchanged, a file that cannot be read keeps the strict rule; the marker and the terminal are NOT a security boundary, a deliberate pty or a copied binary gets past them, and the command sandbox that masks the data dir is the boundary; no new marker); under an agent session a flag or `TOLE_TRUST` may only narrow the user config's `allow`, `trust` and hook lists (#865: exact entries, every user deny and hook kept; keyed on the same environment marker as #867, so a copied binary with a cleared environment, a deliberate pty or a forged depth marker is not stopped, and the child tole itself starts, the project config and serve/acp/mcp are exempt, except that a child inherits the marker when its parent is itself marked and is then guarded too (fails safe); `--profile`, `--yes`, MCP server flags and `--workspace` are not covered; the boundary is the command sandbox); a cron agent run gets the owner's `[sandbox]` and consumers like any other run (`tole cron run` and `tick` load the user config, #863), but `verify` and `turnend` are sandboxed BY DEFAULT as `optional` since the flip of #748 (owner decision 2026-11-16; `required` is not the default because a sandboxed verify cannot see `~/.cargo`; opt out with `verify = "off"` / `turnend = "off"`; the implicit default relaxes "no unsandboxed fallback" ONLY on a host with no sandbox backend at all (no `bwrap` on an absolute `PATH` entry, `bwrap` older than 0.8, a `bwrap` that cannot create a namespace because user namespaces are refused, or a non-Linux host; the backend is looked up as an executable regular file in absolute `PATH` entries only, first working one wins, and probed at start-up; a writable ABSOLUTE `PATH` directory is still trusted, as for explicit modes; a failure that may pass, such as EAGAIN or ENOMEM, is retried and not cached, so a persistent one is probed again on each later launch): there the command runs unsandboxed with a stderr warning, a `sandbox_fallback` line in the plugin ledger and a doctor WARNING, because refusing every verify and stop gate on Windows, macOS or a Linux without bubblewrap would break those hosts and push owners to switch the consumers off; a backend that exists but cannot sandbox the run, a refused workspace layout and a failing launcher still refuse, and an explicit `optional` or `required` never falls back; the default does not turn on the `git` tool hardening, which stays tied to a consumer the owner wrote), `cron` is opt-in (no fallback, ever; plan in `docs/sandbox-design.md` §9.4); inside a `command` sandbox only what exists at launch is protected (a `.git` or `.tole` that did not exist at launch, or a `.git` made after a detached job started, is unprotected; a nested repository not named by `.gitmodules` or inside `.tole-worktrees` is unprotected, in particular a gitlink in the index with no `.gitmodules` entry (an accidental embedded repository, also with an empty or missing directory), and a linked worktree of a submodule elsewhere in the workspace (only the top-level worktree registry is read), and a plain `git status`, `git diff` or `git add -A` on the host in the outer repository can fire a `core.fsmonitor` planted in it; a `core.hooksPath`, `include.path` or `core.fsmonitor` in the (read-only) `.git/config` that names scripts inside the workspace, whose scripts stay writable; files outside `.git` that run later on the host (`.envrc`, Makefile, package scripts, CI and hook-manager configs, `.gitattributes`) can be planted; a diff does not show `.git` internals, so treat a workspace a sandboxed command wrote into as untrusted until its git config and hooks have been inspected, or use a fresh clone), the limits are per process and not a cgroup, and `/tmp` is a tmpfs without a size limit; macOS and Windows have no sandbox and `tole doctor` says so; no host-allowlist proxy (network is all or nothing); the kernel is shared, so a kernel escape is not stopped; every grant is resolved and re-checked at each launch and only the resolved path is bound, but a same-user process racing a path component between that resolution and bubblewrap's mount is not stopped by user space alone (that attacker already owns the account; see the design note §5). |
| Sensitive data disclosure (env) | **control (ENV-1, #595)** | Children inherit a scrubbed env: any var whose name contains `API_KEY`, `APIKEY`, `SECRET`, `TOKEN`, `PASSWORD`, `PASSWD`, `PASSPHRASE`, `CREDENTIAL` or `PRIVATE_KEY`, ends in `PASS`, `_KEY`, `_DSN`, `_SK`, `_COOKIE`, `_AUTH`, `_BEARER` or `DATABASE_URL`, or starts with `AUTH_` or `BEARER_` (case-insensitive; `SSH_AUTH_SOCK`, `GPG_KEY`, public keys and `*BYPASS` excepted, #682) is removed, and so is any var whose VALUE is a secret: a URL with a password (`REDIS_URL=redis://:pw@host`; proxy settings `*_PROXY` excepted, as every network command needs them, their password is still redacted) or a value tole holds by role whatever the var is called (webhook keys from `hmac_key_env` or `hmac_key_file`, the serve token, `${NAME}` web header expansions; `secrets::is_secret_var`, #682). Removed except what the `[secrets]` release grants to the launching tool (#598): by default only `GITHUB_TOKEN`, only for the `gh`/`git` tools and the owner's pre/post tool hooks and MCP servers; `run_command`, `job_start`, cron scripts, the mission `--verify` command (consumer `verify`) and the `on_turnend` stop gates (consumer `turnend`) get none (#676: the last two run code the agent wrote, unattended). A project config can only narrow a binding. The name rule is `secrets::is_secret_name`, shared with the redactor below. Child agents get the same scrubbed env plus only their provider keys and base URLs (`TOLE_API_KEY`, `OPENAI_API_KEY`, `TOLE_FALLBACK_API_KEY`, `TOLE_BASE_URL`, `OPENAI_BASE_URL`, `TOLE_FALLBACK_BASE_URL`); before #595 they inherited the full env. |
| Sensitive data disclosure (parent process) | **control (#675)** | The ENV-1 scrub edits only a child's own environment; a same-user child could still read tole's `/proc/<ppid>/environ` (every secret tole was started with) and `/proc/<ppid>/mem`, or ptrace it. On Linux every `tole` process marks itself non-dumpable (`prctl(PR_SET_DUMPABLE, 0)`, `tole_core::process_guard`) first thing in `main`, before any thread or child exists: the kernel gives those files to root and refuses same-user ptrace. `tole doctor` shows the state (`process guard: ✓ non-dumpable`). Side effects: no core dumps, a debugger needs root. tole does not also remove secrets from its own environment: `unsetenv` leaves the exec-time block that `/proc/<pid>/environ` shows, granted secrets reach children by inheritance, and changing the environment once threads may run is unsound. Residual: a process running as root (or with `CAP_SYS_PTRACE`) still reads it; macOS (`KERN_PROCARGS2`) and Windows expose a same-user process's environment by other means; and a secret the owner keeps in a readable file (a config, `~/.netrc`) is readable by any same-user child. Those belong to the sandbox work (#601). |
| Sensitive data disclosure (process arguments) | **control (#682)** | A value in a process's arguments is readable by every local process (`ps`, `/proc/<pid>/cmdline`, which stays readable even when the process is non-dumpable). `tole serve` takes its bearer token from `TOLE_SERVE_TOKEN` or `--token-file` (a regular file, no link or FIFO, at most 4 KiB); `--token <value>` still works but prints a warning naming the alternatives. Residual: `tole approvals --token` is a client and keeps the argument form; prefer `TOLE_SERVE_TOKEN` there too. |
| Sensitive data disclosure (files) | **control (#598 part b)** | `read_file`, the `git` diff and `cora_search` never return the content of a sensitive file: `.env*`, `*.env`, `*.env.*`, `*.pem`, `id_*`, `.npmrc`, `.netrc`, `*.key`, `credentials`, `.git-credentials`, `.pgpass`, `.htpasswd`, `.pypirc`, `.dockercfg`, `config.json` directly under `.docker`, `*.p12`, `*.pfx`, `*.jks`, `*.keystore`, `*.tfvars`, `kubeconfig`, `secrets.y*ml`, or a name matching the `sensitive_files` config list (#683; file name, case-insensitive, requested or resolved real path): values become `«redacted»`. The verbatim read is `read_file_raw`, `Risk::Write`, so it needs approval or an explicit `--allow read_file_raw`. Residual: a command (`run_command`, `job_start`) can still read any file; it is approval-gated and known secret values are redacted on egress; a hard link to a sensitive file under another name is not detected. |
| Sensitive data disclosure (egress) | **control (E11 → #595) + documented residual** | `SecretSet` collects the values of every secret-named env var, the password of every env var whose value is a URL with one (#682), the values web backend headers expand from `${NAME}` (whatever the variable is called), the secrets tole holds by role (every webhook route key, the serve token, whatever carried them, #682), the provider key and its base-URL password, and replaces each value, and its base64 / hex / percent-encoded / JSON-escaped forms, with `«NAME»` (#681: base64 standard and URL-safe alone and embedded at any offset of a larger payload, such as HTTP Basic `user:token`, a docker `auth` or a base64'd `.env`; hex in either case; JSON `\/` and `\uXXXX` escapes; a value split by a zero-width, bidi or soft-hyphen character, found by a second scan of the text without those characters; at most 24 needles per value, none shorter than the 8-character floor) at every egress sink: the provider request body (system prompt, messages, tool arguments, tool results) and provider errors, MCP tool arguments (every string, keys included, structure preserved), the `web_search` query, the `systemone_decide` request, the memory-loop session summary (redacted before the length cap), `uteke_document` markdown, and the live turn events every observer gets (#677: `tool_started` input, the `tool_finished` preview of an output or error, reasoning; redacted in the turn loop with the session's own set before the 200-character preview cut, so serve SSE, ACP `tool_call_update` and the TUI never show what the log hides). Streamed answer text (`text_delta`) is model output and is not redacted live; the committed entry is. A `web_fetch` URL (direct or via the fetch backend) that carries a known secret is refused instead, since a redacted URL would fetch something else. Values under 8 characters are not redacted (false-positive guard); they are reported by name only. Residual: export, handoff documents, round digests and skill drafts are later sinks of the same set; MCP tool RESULTS are not scrubbed (documented residual); secrets tole never knew (pasted, printed by a command) are masked by the shape detector (#597) as `prefix[REDACTED:<kind>]`, after the known-value pass and at the same sinks: the shared `uteke-privacy` ruleset covers AWS, GitHub, npm, Slack and Google keys, `sk-` style keys, bearer headers, JWTs, PEM private keys, URL passwords and `NAME=value` secrets with an entropy check; it is precision-first, so a secret of another shape (a bare random string, a vendor key without a known prefix) is not caught, and a false positive is kept with the user-layer `redaction_allowlist`; a `web_fetch` URL holding such a shape is refused like one holding a known value; a `«NAME»` marker is kept only where it is the whole secret position, so text that carries a marker cannot hide a secret next to it; a JSON field is masked by its name (`{"api_key": ...}`); an entry that only had a shape masked lists `shape:<kind>` in its `"redacted"` field; upstream residuals of the detector: a value with a non-ASCII character is treated as prose and a value containing `[REDACTED` as a placeholder, so a secret glued to either is not caught by the shape pass; a literal `«` or `»` inside a value counts as such a non-ASCII neighbour (`API_KEY=«Zx9$kQ2mPz7Lw4Rt8Vb`, `API_KEY=Zx9$kQ2mPz7Lw4Rt8Vb»`, `API_KEY=«A«B»»<pw>` are not masked), a lone `«`, `«»` or `é` next to a value switches the detector off for that candidate (upstream treats the value as prose); the stand-in of a marker is alphanumeric, so it counts as a word character: `«A»eyJ...` (a JWT is word-boundary anchored), a marker adjacent to or inside a fixed-length vendor token: directly BEFORE it (`«A»ghp_...`, `«A»sk_live_...`, `«A»xoxb-...`, `«A»npm_...`, `«A»AIza...`) or directly AFTER it (`AIza...«A»`, `npm_...«A»`) and a marker inside a keyword (`API_«A»KEY=`, `--p«A»assword=`) are not masked; while a `«NAME»` marker (any name the set holds, or a well-formed one in the text) is swapped for a letters-and-digits stand-in the text cannot contain, so a secret glued to a marker is masked; a known value under an arbitrary transform is not recognised (#681): line-wrapped base64 (`base64` wraps at 76 characters), mixed-case hex, gzip or other compression, encryption, ROT13 and other substitutions, a value cut into pieces or reordered, escapes of invisible characters (`\u200b` written out), or an invisible character other than the ones listed in `secrets::is_invisible`; the shape detector cannot close that either, only keeping the secret out of the agent's reach does (`[secrets]` grants, the sandbox work #601); since #596 the durable log stores the redacted form too: every entry payload and the pinned system prompt are redacted with the same process set before the line is written, the entry names what it hid (`"redacted": ["NAME"]`), and the provider is fed from those entries, so replay is exact. Logs written before #596 keep their raw lines (append-only; nothing is rewritten). Tool-call arguments and tool results in the provider body are redacted structurally (`SecretSet::redact_value`), so a secret that appears as a bare JSON number (`{"pin":12345678}`) becomes the string `"«NAME»"` and the replayed arguments stay valid JSON. `SecretSet::redact` memoizes its result for texts of 1 KiB or more (#908): the memo keys on the exact input text and so RETAINS the raw input in process memory, including text the detector does not recognise as secret, for the lifetime of the set (the process-wide set lives as long as the process), bounded to 64 MiB of keys and values per set and emptied when exceeded; it is never written to disk, and an allowlist install empties it. |
| MCP tool poisoning / rug pull | **control (#599 part 1)** | Each MCP server's tool manifest (names, descriptions, input schemas, canonical and length-prefixed) and version are hashed on every connect and must match the owner's approval in the user plugin lock, or the server's tools are not registered (fail closed: unapproved, changed, revoked and unreadable-lock all refuse). A refused server contributes no other model-visible text either (#680): `register_server_tools` records the admission decision on the session's own `ToolRegistry` (never process-wide, so sessions with different locks in `serve` or the cron loop do not share it), and the host probes extras, offers `mcp_read` and lists or renders `/mcp:<server>:<prompt>` only for servers that registry admitted, and a server with no lock entry, a revoked one, or any server under an unreadable lock is not even started (a stdio server is code). Prompts and resource templates are not part of the manifest hash: they are reachable only from a server whose pinned tools match. Approval shows the full model-visible text, terminal-safe, and happens only in an interactive terminal before the session starts; unattended runs never approve. A project lock can only narrow. Since #679 the lock entry is also bound to the host's own facts, which a server cannot replay: the configured source (`stdio:<command line>` or the url) must equal the approved one, and for a stdio program that resolved to a file at approval, its SHA-256 must equal the recorded `integrity`; otherwise the server is refused before it is started (`source_change` / `binary_change` in the ledger) until it is re-approved, so a program the owner never approved under that name never runs; the same checks run again at connect. A server's stdio process receives only the secrets its approval recorded (`secrets`, the `mcp` grants held then; a project lock can drop names), and only while the `[secrets]` release still binds them to `mcp`. Residual: an approved description is still model-visible text (the description scanner is part 2 of the design); for a launcher source (`npx -y pkg`, `uvx`, `docker run`) the integrity covers the launcher, not the package it fetches, so pin the package version in the command line (#600 resolves and pins packages). Revocation (#600): `tole plugin approve` and `revoke` refuse under an agent session; `list` and `diff` never start a server the lock would refuse and start nothing under a session; the plugin ledger is compared with the lock, so a lock edited by hand (a revoked entry flipped back, a manifest hash re-pinned) is treated as revoked until the owner approves again. Not protected: the lock and the ledger are files of the owner, so a process that can rewrite both (or a copied binary with a cleared environment) revives a server undetected. Owner commands (#599 part 2): `tole plugin list|diff|approve|revoke` run without a session, write only with a terminal yes or a matching `--hash`, print only redacted, terminal-safe text, and never write a lock that does not read safely; `revoke` blocks the next connect. `list` and `diff` report a binary that changed since approval, which is refused at connect (#679). |
| MCP client servers on server faces (`tole serve`, `tole acp`, #946) | **control + documented residual** | **Who names a server:** only the owner, once, at start-up: the `--mcp-server` flag of `tole serve` / `tole acp` or `mcp_server` in the USER config. No request body, header, ACP `session/new` `mcpServers`, session override, cwd file or project config can add, change or name one: the list is installed once at start-up (`session::owner_mcp`, set-once) and read from nowhere else; `SessionOverrides` and every request type carry no MCP field (a test pins it). `tole serve` and `tole acp` refuse an `mcp_server` that comes from the PROJECT config (a repository file) at start-up, as they refuse hooks from it. There is NO PATH-based auto-preset: the `cora` preset the CLI attaches when a `cora` binary is on the PATH is never attached on a server face, and with no configured server a server session behaves as before. **The lock:** every server goes through the same code as the CLI (`tool_roster::build`): approval bound to source, program binary and secrets (#599, #679, #680), `[sandbox]` profile (#750, #810) and the unattended rule: `tole serve` (no human) counts an `optional` sandbox as `required`, so with no usable backend the server is not started. `tole acp` is attended like the CLI (a live editor user): with `optional` and no backend the server starts unsandboxed, with the stderr warning and a `sandbox_fallback` line in the plugin ledger. An unapproved, changed, moved or revoked server is not started and registers no tool, and no server is ever approved on a server face (approval is a terminal act). **The notice:** a server that is not started is reported once per server in the session log as an `error` entry with `notice: mcp_server_not_started`, naming the server and the lock's own reason (never the server's output, never a secret, redacted again with the process secrets), which a client reads with `GET /sessions/{id}/entries`; the session still opens without it. **The gate:** every MCP tool stays `Risk::Write` (server metadata is never trusted for risk) and goes through the one approval gate, so over `serve` it queues in the approvals queue like any Write; the ReadOnly `mcp_read` resource tool is not offered on a server face, so no MCP surface is ReadOnly. **Residual:** (a) an unsandboxed server is one process per name, shared by every session and client of a `serve` (`conn_key_for` keys by the bare name when not sandboxed), while every session still re-runs the lock gate; a sandboxed server is keyed per session workspace. (b) Owner-controlled allow patterns (`--allow`, a profile's `allow`, `allow_patterns`) such as `mcp_*` let MCP tools skip the approval queue; that is the owner's pre-authorization. (c) An `mcp_server` from the project config stops `serve`/`acp` at start-up (fail closed; a value from the environment or a profile is refused the same way). (d) The `mcp_server_not_started` notice is appended once per distinct server and reason per session log (a resume does not repeat it). (e) An approved server's tools run with the owner's account and whatever its sandbox profile grants (an unsandboxed or `network = host` server can reach anything the account can). A server's output is untrusted model input (fenced like every tool result, but it can still mislead the model). A prompt injected through a tool result (or a file, or a web page) can ask the model for further Write calls, including more calls to an approved server; those still queue for approval, so the owner approving without reading them remains the weak point. A server that changes what it does without changing its manifest (a backend behind the same tools) is outside the lock, as on the CLI. `tole mcp` (stdio) refuses `--mcp-server`, and neither it nor `--transport mcp` re-exports a third party's tools through its own tool surface (a separate decision); the sessions `--transport mcp` opens with `tole_session_new` use the owner's list like any server session. `POST /missions` and cron runs attach none. |
| Supply chain (deps) | **control** | Cargo Audit + Trivy FS Scan required by rulesets on every PR and push. |
| Resource exhaustion | **control + fixed here (JOB-2)** | Provider 120 s timeout, subprocess 30 s ceiling, step budget 32, loop guard, read caps; job logs are read via bounded windows on poll (never a whole-log load). Since the 2026-09-18 scan triage, a ReadOnly poll no longer rewrites the log file — runaway log disk growth is bounded by the job's lifetime and clearing a runaway log is an operator action. |
| Session/message tampering | **control** | Append-only JSONL, CAS state transitions, seq monotonicity, torn-line recovery with truncation (#68); compaction never drops entries. |
| Injection via config | **control (MCP shipped)** | `.cora.yaml`/session headers are owner-controlled. MCP servers (#74) add an untrusted surface: server-supplied tool metadata is never trusted for risk tier (everything is Write → approval gate), tool results flow through the same fence/scrub path as native results, and the connection env is scrubbed. Residual: a malicious MCP server controls its own tool descriptions (model-visible) — treat server config as operator trust. |
| Identity & authz of sub-agents | **N/A** | tole v0 is single-agent, single-tenant. MCP servers are external tools, not sub-agents. |
| Human oversight | **control** | Risk-tiered approval gate (every non-ReadOnly call; Destructive never auto-allowed), #68 closed the replay-without-consent hole. |
| Guessing, sniffing or replaying the serve credential (`tole serve --auth basic`, #742) | **control (first slice) + documented residual** | One `Authenticator` decides every serve route (REST, event stream, uploads, `--transport mcp`; webhooks keep their signature gate). Basic checks one user against a salted PBKDF2-HMAC-SHA256 hash (at least 10000 iterations, 600000 by default; `tole serve-hash`) that comes from a file or the environment, never a flag and never plaintext, and is held as a role secret like the token; a wrong user, a wrong password and a malformed header get one identical `401` (hash always computed, constant-time compare), failures are limited per IP (`429` after 10 a minute), a credential over 512 bytes is not hashed, a verified header is cached 5 minutes by SHA-256 only, and no credential reaches a log or error. Plain HTTP is refused off loopback unless `--allow-insecure-basic`. Residual: Basic over plain HTTP on a trusted network is readable on the path (TLS terminates at a reverse proxy, which tole does not serve); one user and one password, no scopes (every credential is full authority, approvals included), no identity bound to a session, so a leaked password is a leaked approval authority until the hash is replaced and the server restarted; behind a proxy all clients share one IP in the limiter; an unauthenticated client can still ask for one PBKDF2 run per request up to the limit; the failure limiter is in memory (a restart resets it). OIDC is not built. |
| Unattended approval resume (cron) | **control (#581)** | A parked cron Write runs only after an `/approvals` allow (serve bearer token): the run resumes once, through a `resume` ledger record the fold accepts only for an `awaiting_approval` run of the same job not resumed before (forged or repeated lines are ignored), and the one-shot approval covers exactly the approved call. Denied, expired or lost approvals never run it. Residual: approvals live in serve's memory, so a restart drops them (the run stays `awaiting_approval`; re-run it). |
| Agent self-grants power through cron (P8) | **control (#581)** | An agent creates a job only through `cron_create`, a Write tool registered for session faces at depth 0 outside plan mode (never in a cron run). The host sets `paused`, `created_by = agent`, `creator_allow` (the session's allow list) and the workdir; the input type refuses any other field by name. `allow` must be within the creator's and keeps its deny patterns; a `command:` delivery is refused. A job not saved through `tole cron create`, hand-written lines included, runs only after `tole cron resume` (`enabled_by = cli`), within `creator_allow`, and never as a script; otherwise it is `blocked`. The ledger fold fixes `created_by` and `creator_allow` to the line that created a live job. An agent run whose workdir holds the cron dir is `blocked`, so its file tools cannot write job lines. No config defines jobs. Residual: the ledger file is the trust root, so a process that can write it directly (the owner, or a command the owner allowlisted) can write a line claiming `created_by = cli`. |
| Forged or replayed webhook (`POST /hooks/<route>`) | **control (#582 part 1)** | Every route needs an HMAC-SHA256 key (`hmac_key_env` or `hmac_key_file`; at least 16 bytes); a route without one stops `tole serve` at startup. Native deliveries (v2, #678) sign `v2\n<timestamp>\n<delivery id>\n<event>\n<body>` (`X-Tole-Signature: v2=<hex>`) and must be within ±300 s, so a capture can neither be replayed under a fresh id nor relabelled as another event; the v1 form (`sha256=` over `<timestamp>.<body>`), which signed neither, is refused. GitHub deliveries sign the body only (`X-Hub-Signature-256`), so their idempotency key is the signed body (`gh:` and its SHA-256), not the unsigned `X-GitHub-Delivery`: a capture re-sent under a new id is `duplicate`. The compare is constant-time, and a failed signature counts toward the source IP's failed-auth limit. Each key is recorded as `hook_seen` in the cron ledger; a repeat is answered `duplicate` and never fires, across restarts too. Unsigned routes (`insecure_loopback`) load only on a loopback bind and answer loopback clients only. **Accepted residual (owner decision 2026-11-12, narrowed by #678):** GitHub signs no timestamp, so a captured GitHub delivery replayed after the 7-day `hook_seen` retention verifies again and is accepted as new, and its unsigned `X-GitHub-Event` header can be changed then: a route's `events` list is not a security boundary for GitHub deliveries (a `filter` on the payload is, since the body is signed). Two distinct GitHub deliveries with byte-identical bodies count as one. Forging a new delivery still needs the route's key. For a route with `allow_writes`, prefer the native scheme (and TLS in front of `tole serve`, which speaks plain HTTP). Native deliveries are not affected: their 300 s window ends long before the retention does. |
| Webhook flooding and oversized bodies | **control (#582 part 1)** | An unknown route (404) or a body over the route's `max_body_bytes` (413, at most 1 MiB) is answered before the body is read. Rate limiting has two parts. The per-route `rate_per_minute` counts accepted deliveries (`hook_seen` records in the ledger), so it survives a restart. Requests with a bad or missing signature are limited per source IP by the existing failed-auth limiter, which is in memory and starts empty after a restart. The fold keeps seen deliveries for 7 days, so memory stays bounded. **Accepted residual (owner decision 2026-11-12):** the ledger file `jobs.jsonl` is append-only and never rotated, so it grows by one line per accepted delivery, at most the route's rate per minute. No rotation or compaction is planned for now. |
| Webhook route defined by a repository | **control (#582 part 1)** | `[hooks]` is user config only: a project config's `hooks` is ignored with a warning, and `config check` marks it IGNORED. The key is named by an environment variable or a file path, never stored in the config. The key file is opened without following a link or waiting on a FIFO, regular files only, at most 4 KiB. The key never appears in errors, logs, responses, `config check` or the ledger. |
| The advisor steers or leaks the run | **control (#587 part 1, core)** | The advisor model comes only from the user config (`profiles.<name>.models.advisor`), and a blank one is refused. Its instructions are fixed; the executor supplies only a capped question. The task contract, the transcript window (the last 12 entries) and the question are each redacted with the process secrets, neutralized (fence markers, control characters) and capped before they are fenced as data. A reply is parsed only for a verdict word (`plan`, `correction`, `stop`) and capped, redacted text; nothing else is read, so an advisor cannot name a tool, grant or budget for the host to act on. A reply without a verdict is a `plan`, never a `stop`. It goes back to the executor fenced as advisory data. Consultations are capped per run (`advisor_max_uses`, default 3, at most 20), and `Budget` makes advisor tokens part of the run's own budget; usage is tagged `role: advisor`. **Part 2 constraints (owner decisions 2026-11-13):** the tool is registered only where the run's meter shares that budget (never an uncounted advisor). The window comes from the host's in-memory view of the transcript, never from a file the model or a child can write. **Part 2 (`consult_advisor`):** registered only in `tole mission` (delegated children included, by their own profile) and cron agent runs. The window is an in-memory copy of what the turn loop sends the run's provider; no file is read. The run's budget is shared: a consultation that cannot fit, or past the cap, is refused before any call. What a call spent is counted even when it fails, and written to the session as `role: advisor` usage, so the run's reported tokens and the cron daily cap include it. The call sends no tools, does not stream and has no fallback to another model. An advisor's own tool call is answered with an error, and nothing runs. Residual: the advisor sees redacted transcript text, so a model provider for the advisor tier receives that text like the run's own provider does. |
| Model routing picks a model or power the owner did not choose | **control (#586 part 1)** | The router returns only a tier (`light`, `standard`, `heavy`); tiers map to models only in the user config (`[router.tiers]`, `profiles.<name>.models`). A project config cannot add tiers or rules or enable routing: it can only set `enabled = false` or narrow `allow_tiers`. A rule's tier that is not declared where it resolves, or an unknown profile, is a load error naming the remedy, never a fallback to another model. No match keeps the configured model. A route has no field for a budget, an allow list or a secret, so routing cannot raise them. The brief reads the task text only for its size and the owner's keywords. Every string a decision records or prints is redacted with the process secrets before it is capped, and `tole doctor` / `config check` show names only. **Part 2:** a new run, mission, delegated child or cron run is routed once, before its first provider call. Only `fact/model` and the redacted `fact/route` record change; budgets, allow lists, plan mode and secrets are built exactly as before (an end-to-end test compares the tools sent with and without a route). An explicit model (`/model`, a cron job's `--model`) is never overridden. A cron run is routed on the job's own prompt, never on a webhook payload, so a delivery cannot steer the model. A broken router stops the run before any provider call. **Server sessions (#945):** a `tole serve` / `tole acp` / MCP session is routed once, at its first prompt, by the same step; the owner's config is read then, a request cannot add tiers, rules or enable routing, a client-set model (`POST /sessions/{id}/model`), a profile's model and a resumed session are never overridden or re-routed, and a config or router error fails that prompt redacted before any provider call. Residual: a rule can route to a costlier declared tier; the run's own budget still caps what it spends. |
| Webhook key or wider power through the `tole hook` CLI | **control (#582 part 3)** | `tole hook add` writes the user config only. It takes the key's source (`--key-env NAME` or an absolute `--key-file`), never the key, so the key stays out of shell history and process listings. No `tole hook` output (list, test, errors) shows a key. `list` does not show its variable or file either; a load error may name them, as serve's does. `allow_writes`, `insecure_loopback` and filters have no flag, so widening a route stays a manual edit (design P8). `tole hook test` is a local dry run whose printed context is redacted and capped like serve's. `--fire` posts only to a loopback `http://` URL, so a signed delivery never leaves the machine through it. |
| A multi-round discussion acts on what a model wrote, drops dissent or overspends | **control (#588 part 2)** | Participants are user-config profiles only, never the built-in scribe or claim-checker; those two run with fixed instructions, no tools and no streaming, and only `[profiles.scribe]` / `[profiles.claim-checker]` in the user config can change their model (an unresolved tier is a load error, never a fallback). Inline participants are provider calls with no tools; a tool call in a reply runs nothing. Every reply passes pass 1 (control characters, then secret redaction, then fence neutralization, then the cap) before the ledger records it, and reaches the next model only fenced as data. A scribe reply is read only through the digest validator (every line cites a real entry of its author, every voice appears); anything else is dropped and the round is passed on as its pass-1 text. The claim-checker's checklist is read only through its parser; an unreadable one meets nothing. The gate is computed by the ledger from its own records, so no digest or contribution can report, write, change the roster or call a tool. Every call is reserved in the ledger before it runs and settled after, and charged to the goal's pot the discussion was carved from; a call that does not fit, or `max_rounds`, ends the discussion with an escalation to the owner. A crashed call stays counted. A participant turn or scribe call that failed with the provider's own timeout (and no other error: not an HTTP status, not a malformed reply) is made once more (#756): the retry is reserved and settled like any call, so the discussion budget and the goal's pot still stop it (a retry that does not fit is the budget escalation), the ledger records it as a reservation carrying `retry_of` (the timed-out call) and refuses a second retry of a call or a retry of a retry, and a second timeout fails the turn as before; the checker is not retried, and the gate, `max_rounds` and the dissent rules are untouched. A call that fails with no reported usage after the request may have left (a transport timeout above all, #804) is charged its reserved estimate in the ledger and the goal's pot, like a crashed call, because the provider may have generated and billed the reply before the client gave up: two timeouts cost two estimates and the budget stops the discussion at the cap with the same escalation, instead of running past it unseen; the settle record carries `charged` (`timeout` or `unreported`) so the audit shows why, and `tole audit --discussion` shows retries and charged calls even for a round that never completed. Only a failure that provably came before the request left (refused connection, DNS, a rejected URL, the TLS handshake) stays at 0; the classifier is a closed list of anchored error prefixes and charges anything it cannot place. A settle is final, so a resume never charges a call twice. Residual: the charge is the estimate, not the real bill (a reply longer than the estimate, or billed at a different rate, is still not seen), and a reported usage is trusted as before. Records (512 KiB) and the log (16 MiB) are capped on write and read; the log and its lock are never opened through a symlink. Independent participants (review and critique only) are `consult` children of the delegation tree: read-only, carved from the caller, within the depth and live-process caps and the machine guard; their contribution is the completion report the parent reads with the report caps, then pass 1. A child is started at most once: its reservation (the intent) is in the ledger before it starts, its task is tagged with that call, and its node id is bound after; a resumed driver polls a bound child, and for an unbound intent looks for the tagged child (only among direct children of a goal, or of the session's own node in a delegated child, so a child cannot plant one) before it starts any. uteke round documents hold digests and the contract only, never a raw contribution, redacted then capped. `Driver::from_config` is the one builder that wires them (#717): slug, parent and room are host-built, a failed write is a note in the outcome (never silent, never a change of gate), and without uteke one note says documents are off. Residual: a participant can still argue for something false inside the fence; the checker's evidence rule and the owner's decision are the controls. The number of participants is advice, not a limit (#856): above 8 voices the CLI prints one line on stderr and the `discuss` tool result carries an `advice` array, and nothing else changes, so what stops a discussion that costs too much is still the budget (every call reserved before it runs, a hard cap), `max_rounds` and the reservation of each independent child. The only ceiling is technical and measured: 16 voices, the most whose worst-case round (every voice at the 8000-character cap in 4-byte characters, the digest refused) fits one 512 KiB ledger record; a test writes that record and checks that one more voice would not fit, and a contract above it is refused naming the ceiling. A model-callable `discuss` therefore cannot buy more than its carved budget with more voices; independent participants take their turns one at a time, so more voices never raise the number of live processes. How a discussion is started is the next row. |
| A discussion is started or steered by something other than the owner | **control (#588 part 3)** | Two entry points, both fixed by the owner (`automation-design.md`, "Entry point"). `tole discuss start\|resume\|list\|show` is owner-only: the contract (question, criteria, participants, budget, max rounds) is what the owner typed, nothing a model wrote; participants are user-config profiles, inline, read-only, at depth 0; the budget is a hard cap reserved call by call in the ledger, and the stop at the cap or at `--max-rounds` is an escalation to the owner. `--dry-run` validates and prices the contract without a provider call or a write. The model-facing tools `discuss`, `discuss_status` and `discuss_stop` are `Risk::Write` (each call approved or allowlisted; the prompt shows the question, participants and budget) and are registered only where `delegate` is, so never at the goal's depth limit. The input has five fields and refuses any other; the id is the host's. The budget is carved from the caller's remaining budget in the delegation tree (never more than is left; a crash is charged the whole carve), at most two run at once per process, and the participants must be user-config profiles whose allow patterns the caller's allow list covers, never a built-in role. `discuss_status` and `discuss_stop` reach only discussions the same session started (a host-only tree node, not a task string), never the owner's; status text is fenced and redacted. A stop is the ledger's own `stopped` escalation, so no participant text can start, prevent or delay it. Residual: a model with `discuss` can spend up to the carve it was given on participants' calls, and a discussion whose process died stays open until a stop or `tole discuss resume`. |
| An agent grows or keeps its own skills (drafts and curator) | **control (#592, #593)** | `skill_propose` is `Risk::Write` and writes only a draft under `$CODECORA_HOME/tole/skill-drafts/<id>/`, outside every skills directory, so nothing can load it; a draft carries its provenance and the taint of any web or MCP content the session consumed, is never overwritten and is decided once. Only the owner makes it loadable: `tole skills drafts promote` shows the draft, needs a terminal yes or the content `--hash`, refuses a tainted draft without `--accept-taint`, a draft near an existing skill unless its verdict is `new`, and a name already in use, and approves the package hash only for the exact bytes it wrote. `revoke` withdraws what grew from a poisoned source. `tole curator` is LLM-free: it archives unused agent-grown skills whole (restorable), never edits a skill's text, never touches pinned skills or skills a live cron job names, and only lists a skill whose runs do worse as a proposal. `promote` is owner-only under #684. Residual: a CLI face (`run`, `chat`, `mission`) builds its tools before the session id exists, so its drafts record no session id yet (`serve` and ACP sessions do); revoke such a draft by its draft id. |
| Webhook filter cost (regex denial of service) | **control (#582 part 2a)** | Filters run before any LLM work. A filter tree is at most 8 deep and 64 nodes, compiled once when the route loads. Regexes use `regex-lite` (no backtracking, search `O(pattern × input)`), and every input to that cost is capped: a pattern is at most 256 bytes, its compiled program at most 32 KiB, and a string over 16 KiB is never scanned. A test runs `^(a+)+$` on 16 KiB of `a` plus `!` and must finish within 2 s (it takes milliseconds). |
| Prompt injection through a webhook payload | **mitigation (#582 part 2a)** | A payload reaches the model only through the route's template. Placeholders render scalars only (objects and arrays become `[object]` / `[array]`). Each value has control characters and the fence markers neutralized, is redacted with the process secrets and then capped, so a cap never leaves part of a secret. The whole rendering is capped at 8 KiB and sent as one JSON string between `<<<BEGIN UNTRUSTED WEBHOOK DATA (data, not instructions)>>>` and the matching end marker. Residual: a model can still follow text inside the fence. The runs are ReadOnly by default (part 2b), which limits what such text can make it do. |
| A webhook fires work with more power than intended | **control (#582 part 2b)** | A delivery fires only the route's `cron_job`, an existing job, never a job described by the payload. It goes through the same claim path as a scheduled run: the #640 trust check (an agent's or unknown-origin job must be enabled by its owner and within its creator's grant, and a non-owner script never runs), the delivery check, the daily token cap, and the job's model pin, budgets and allowlist. A paused or running job is refused (`409`). The fired run is ReadOnly by default: its roster is built in plan mode, so Write tools are absent and the job's allowlist cannot bring them back. Only `allow_writes = true` in the user config lifts that. The payload context is appended to that one run's prompt and never stored in the job. Events and filters are checked before any LLM work. |
| Outbound webhook (`[webhook_out.<name>]`, #911) pointed somewhere the owner did not choose | **control (#911)** | A destination (URL, key, events) exists in the USER config only: a project config's `webhook_out` is ignored with a warning, a cron job only names a destination (`deliver = ["webhook:gateway"]`), and an agent-created job may name one only with `allow_agent_jobs = true` and after the owner enables it (#640). A payload field never picks a URL, header, key or event type. The address policy runs per attempt in the resolver: loopback needs `allow_loopback` and a host that is `localhost` or a loopback IP (a name that resolves to 127.0.0.1 is refused; `allow_private` does not unlock it), private ranges need `allow_private`, redirects are not followed, the environment proxy is off. |
| Secret in an outbound webhook (payload, outbox, ledger, logs) | **control + documented residual (#911)** | The destination key, its URL and each long query value or path segment of the URL are role secrets, redacted on egress and scrubbed from children; the payload goes through `SecretSet::redact` before it is stored or signed; the outbox keeps the already-redacted body (0600); the ledger, doctor and stderr carry only the destination name and a fixed code, never a URL, header or response. An approval `description` is redacted too, including `${NAME}` header secrets, and an event carries no storage path or call fingerprint. A real-binary leak check (a key and a URL token printed by a job in halves) covers the receiver, the outbox, the ledger and stderr. Residual: a value added to the secret set after an event is queued is not re-redacted in a stored body. |
| Forged or tampered outbox line (same account) | **control + documented residual (#911)** | The sender re-validates every line before signing: a destination the config does not name, a body naming another destination, an event type the destination does not list, an id that is not the body's, an unknown schema version, a time in the future or past the deadline, an oversized or malformed body is refused and marked dead. Residual: the same account can edit the user config, so a well-formed event for a configured destination can still be appended. |
| A webhook mistaken for authority | **control (#911)** | A notification only says that a run finished or that an approval is pending. It carries no credential, token or link; the decision arrives only on `POST /approvals/{id}/decision` with serve's own credential. A receiver that applies the documented 300 s timestamp window rejects a replay, and a replay cannot grant anything. The notifier on the approval queue cannot change, delay by lock or fail an approval (called after every lock is released, panics caught, an event announced exactly once). |

## Deliberate limitations (documented, not fixed)

- **A granted secret is readable by its consumer's whole child** — since
  #598 `GITHUB_TOKEN` reaches only the `gh`/`git` tools (and the owner's
  hooks and MCP servers) by default, but a user who grants a secret to
  `run_command` hands it to every command the model runs.
- **Workspace width is the operator's choice** — `--workspace /` is
  technically possible and a terrible idea; the flag validates existence,
  not sanity. Documented in CONTRIBUTING.
- **Symlink swap inside `tole-jobs/`** — requires a local attacker with
  write access to the operator's workspace, at which point host
  compromise is already achieved. Jobs dirs are created 0700 to reduce
  exposure; no further control planned.
- **`write_file` parent-component swap (residual TOCTOU)** — the walk
  checks each component with `symlink_metadata`, then the final `open()`
  uses `O_NOFOLLOW`, which protects only the FINAL component. A leaf
  symlink swap is refused; replacing an already-verified parent directory
  with a symlink between the walk and the `open()` is not caught and can
  escape the jail. Exploiting it requires a concurrent local process with
  write access to the workspace, and the model already has full process
  authority through `run_command`/`job_start` (the jail bounds cwd, not
  capability), so severity is low. A per-component `openat` descent
  (`O_DIRECTORY | O_NOFOLLOW`, e.g. via `cap-std`) could close it as a
  possible future hardening; none is planned or promised. Non-unix hosts
  have no `O_NOFOLLOW` equivalent wired (best-effort `symlink_metadata`
  check only).
- **`edit_file` read-modify-write window** — `edit_file` checks the
  quoted content hash against the file it just read, then writes the edit
  through a temp file and an atomic rename. A write by ANOTHER process
  between that read and the rename is overwritten. The window is the
  duration of one temp-file write; closing it would need a lock every
  other writer honours, which editors and build tools do not. Within one
  session tool calls are sequential, so only concurrent outside writers
  are affected.
- **`git status`/`diff` are repo-wide by design** — they take no
  pathspec and run with cwd = the tool's workdir, so they show the whole
  repository containing it; in a monorepo subdirectory that includes
  modified files of sibling directories. This is not a jail escape: the
  jail (#247) applies to `add` pathspecs (no absolute paths, `..`, or
  pathspec magic), and the whole git tool is `Risk::Write` (it runs on the host, so with a command consumer on no host-side code execution is reachable from state a sandboxed command can write: hooks are off for every op, so the owner's own pre-commit and commit-msg do NOT run for a model commit through the tool (a human's own `git commit` still runs them); `core.fsmonitor` is always off, no system gitconfig, no external diff drivers, status and diff never descend into a nested repository; and before every op git is asked which git directory it uses and the tool REFUSES unless that is a plain `.git` directory (not bare, not a linked worktree or submodule) whose config holds only allowlisted inert keys, so `filter.*`, `include.*`, `alias.*`, `core.hooksPath`, `core.sshCommand`, `gpg.*` and every key that can name a program or a file stop the tool whenever and by whomever the repository was created; `add` is refused while the index holds a submodule entry; residuals: a still-running sandboxed process can write the config between the vet and the spawn and get that same call to execute it (the spawned git re-reads the config after the vet; later calls refuse; narrow window), a sandbox-written `.gitattributes` with `* filter=lfs` triggers the owner's global `filter.lfs.*` command on attacker-influenced input, a detached `git maintenance run --auto` (`gc.autodetach`) can outlive the vet, git gets an allowlist environment with `HOME` from the passwd database (a tole started by a sandboxed command cannot redirect its config through `GIT_CONFIG_*`, `XDG_CONFIG_HOME` or `HOME`; the git binary and its `PATH` are fixed trusted directories, never the inherited `PATH`; a grant that holds the passwd home is refused; the identity variables, `TMPDIR` and extra descriptors are not passed on), an owner global `include.path` that points at a workspace file is not vetted, and `GIT_CONFIG_NOSYSTEM=1` can cause "dubious ownership"; #748), so every call
  — reads included — goes through the approval gate.
- **Injection residual** (above) — fences are a mitigation, not immunity;
  the model must still treat fenced content as data.

## Follow-ups (filed)

- Per-call Destructive classification heuristics (from RC-1) — needs a
  design discussion before code.
- MCP (#74, SHIPPED): stdio client live (and a Streamable HTTP client,
  #530, see "Remote MCP servers" below); trust-model extensions applied
  (metadata never trusted for risk, scrubbed env, fenced results). The
  former "remaining surface" items shipped: HTTP transport + server auth
  are `tole serve --transport mcp` (#96/#137, hardened in #136) — see the
  Server surfaces section below. Resource subscriptions remain unshipped;
  that would re-open this document.
- MCP prompts and resources (#213): a prompt template is
  **server-controlled content** — same trust tier as a tool description,
  but it lands as the next *user* message when the operator types
  `/mcp:<server>:<prompt>`, so tole prints the rendered text before it is
  sent, caps it (256k chars) and keeps only text parts. It adds no
  authority: every tool call it provokes still passes the approver.
  `mcp_read` (`resources/read`) is a ReadOnly tool registered only for
  servers that advertised the `resources` capability; the server list is
  fixed at startup (the model cannot name another), URIs are server-side
  (no jail), results are capped and fenced like any tool result. Residual:
  a malicious server can put prompt-injection text in either surface —
  server config stays operator trust, and `mcp_read` assumes a server's
  `resources/read` has no side effects (a spec expectation, not enforced).

## Server surfaces — `tole serve`, `tole mcp`, `tole acp` (#94–#96, #137)

All three faces share one rule set: non-interactive hosts never gain
interactive powers. Concretely: `Destructive` registration is refused
behind a non-interactive approver (structurally absent from `tole mcp`
and serve; ACP is the exception by design — its approvals are genuine
per-call human decisions routed to the editor, so `delete_file` may
register there). The daemon adds (both transports): mandatory credentials (a bearer token,
or Basic with `--auth basic`, #742; refuses to start without one), per-IP auth-failure rate limiting, a connection
cap (32), IO timeouts (30s: header, auth-header, and request-body
phases on the MCP transport — SSE response streaming is never cut),
and the **jail-of-jails** — a client-supplied session
cwd must canonicalize inside the server workspace root (default the
server cwd, `--workspace` override), or a remote client could jail a
session to `/`. Multi-session MCP routing strips the `session_id` key
before the tool sees its arguments, and a no-id registry call with 2+
open sessions is refused (the #138-documented ambiguity refusal,
implemented 2026-10-05) rather than silently executing against the
server-level registry.

## Skills loading (#161/#162)

SKILL.md files are operator-supplied prompt content, loaded into the
system prompt (`--skill`) or served on demand via the ReadOnly
`load_skill` tool (discovery index only — name + description — until
loaded). The surface is the same as `--system`: whoever controls the
workspace `skills/` dir or `~/.codecora/tole/skills/` controls prompt
content; discovery from a compromised repo is prompt injection by
another name. Frontmatter is validated loudly (name/dir mismatch is a
hard error) and bodies are capped (16 KiB) — but content itself is
trusted by definition, same tier as the system prompt. A SKILL.md over
1 MiB is never read (its length is checked first and the read stops one
byte past the cap): it is left out of every listing with a startup note
and `load_skill` refuses it, so a cloned repo cannot exhaust memory with
one huge skill file. A FIFO or other non-regular SKILL.md is refused
without blocking. The approvals file (#591) is capped at 1 MiB the same
way.

**Server sessions (#945).** `tole serve` and `tole acp` sessions load approved
skills from the session workspace like the CLI. Their explicit skill files
are the owner's process-level choice (`--skill`, `TOLE_SKILL`, a profile, the
user config), checked at startup and again at each session creation (regular
file, size and body caps). A `skill` named by the PROJECT config stays refused
on these faces: the config trust binds the config's bytes, not those of the
file it names inside the served repository, so a later `git pull` would change
what loads into every new session without an approval.

**Skill index (#590).** With `TOLE_SKILL_INDEX=uteke` a session lists a
pinned core plus the skills a `uteke doc` index recalls by meaning. The index
only finds NAMES: every listed description and every loaded body is read
from the local skill files, and a name with no local skill is dropped, so a
tampered or shared index entry cannot put text into the prompt; at worst it
changes which local skills are listed. The recall query is the owner-written
`run` prompt, never tool output, so a fetched page cannot steer which
instructions get listed. Every query (that prompt, a model-written
`skill_search` query, a `tole skills search` query) is redacted against the
process secrets before it reaches the index, because `uteke doc search` may
forward it to a cloud store. `tole skills index` sends only
each skill's name, description, status and content hash to uteke, never the
body. Only entries tagged `skill`, with status `approved` and in scope for
the active `--profile`, are recalled; `tole skills retire` keeps a skill on disk
but stops its recall. Skill approval is required by default, so only
owner-approved content loads (below).

**Registry approval (#878).** `tole skills fetch <id> --pinned-sha <sha256>`
approves a third-party skill by registry id, and downloads nothing: uteke
0.24.1 returns no skill body (`skill show` is a manifest, the package hash and
a ledger), so the bytes are whatever the owner placed in a skills directory
by a channel they chose. The command computes the package hash over that
directory and refuses unless it equals both the owner's pinned hash and the
hash uteke recorded for the id; the uteke answer is read as a name and a hash
only (the adapter drops everything else) and can only make the command refuse,
never approve or supply text. The ledger line and the approval are those of
`tole skills approve`, with the stated source. Owner only: refused under a
session or child agent (#684) and without a terminal on stdin, with no `--hash`
bypass; no model tool exists for it and a project config cannot reach it.
uteke computes the hash it records from the same directory (`uteke skill
index`), so equality with the pin adds almost nothing when the pin was copied
from `uteke skill show`: the pin must come from a source the owner trusts
independently of the directory, and the success message says only that the hash
equals the pin and uteke's record. File names in the listing the owner reads
before answering are sanitized (no escape, CR or newline survives), so a name
cannot forge or hide a line. The owner-only gate is advisory like every other
owner command (#601): a model with a shell can drive a pty (`script -qc 'tole
skills fetch ...'`) and answer y; the approvals are loaded before the prompt
and saved after it, so a revoke or approve made while the prompt is open can be
overwritten (the same as `approve`); the uteke binary is found by a plain
PATH lookup like the existing adapter. Residuals: the pin and uteke's record are not signatures, so a pin the owner
copied from the same hostile page as the bytes proves only that the bytes did
not change after pinning; whoever can write uteke's library and the skill
directory and also drives the owner's terminal is the owner-equivalent
attacker of every approval. Nothing is executed at approval, and an approval
does not vet what a `scripts/` file does later (normal tool approvals apply).

**Approved-hash loading (#591).** Required by default since 2026-11-11 (owner
decision), so the trust is no longer "whatever is on disk": a discovered skill loads only when
its package (SKILL.md plus every other file of its directory, paths and
bytes) hashes to the owner's approval, recorded by `tole skills approve` in
`$CODECORA_HOME/tole/skill-approvals.json`, outside every workspace. A
skill that is new, edited (any file), or added by a cloned repo is listed
by name only, marked unavailable, with no description, and `load_skill`
refuses it; the body that loads is parsed from the exact bytes that were
hashed. A symlink in a package makes it unloadable. A malformed approvals
file fails startup. `approve --hash` pins the hash the owner reviewed, so a
file changed between review and approval is refused. The approvals file is
as strong as the user's home directory: a process that can write it can
approve anything. The append-only `skill-ledger.jsonl` next to it records each
approval and revoke, and the approvals file stores the ledger's length and
SHA-256 (#879), so deleting, truncating or rewriting the ledger alone makes
every approval refused until the owner runs `tole skills recheckpoint`; a
process that can write both files forges a matching checkpoint. Skills the owner names directly (`--skill`, a profile's
`skills`) are not gated. An approval covers the bytes of the package as
they are when a session starts and loads the skill; it does not vet what a
`references/` or `scripts/` file does when a skill later tells the model to
read or run it (those reads and commands go through the normal tool
approvals). `tole skills approve` shows the files, their sizes and the hash
before it asks; without a terminal it requires `--hash`. With `TOLE_SKILL_INDEX=uteke` the same gate covers
the pinned core, the recalled skills and every `skill_search` hit. The only
opt-out is `TOLE_SKILL_APPROVAL=off` in the environment: there is no config
key for it, and the config schema refuses unknown keys, so a project config
(`.tole/config.toml`) in a cloned repository can never loosen the gate. A
session with unapproved skills prints one startup note (the count and the
fix command, never a description); `tole doctor` warns when approval is
required but the CodeCora home is unresolvable, which means no skill can
ever be approved.

## verify_package (#144)

The ReadOnly registry check before any install answers the
slopsquatting class: hallucinated names surface NOT FOUND with registry
candidates, one-character candidates carry a typo-squat warning, and
registry 429/5xx report `rate_limited` honestly (never "not found").
It is advisory to the model — installs still go through the normal
approval gate (`run_command` Write tier).

## Trust presets (#160, env fixed in #166)

`--trust` / `TOLE_TRUST` presets are pure sugar over `--allow` globs —
they widen nothing structurally: Destructive is never auto-allowed,
write-capable native tools keep prompting under `internal`, and unknown
preset names (flag or env) fail loudly. A typo'd env value breaks every
invocation with a clear error — deliberately, per the
typo-silently-narrowing-trust rule.

`workspace` (#437) is the one preset that auto-allows write-capable native
tools: `write_file` and `edit_file`, through the path guard
(`write_file@workspace`). Their reach is the workspace jail (root
canonicalized; `../` and symlinked targets refused), and the guard keeps
prompting for every dot-path (a write to `.git/hooks/*`, `.envrc`,
`.vscode/`, `.tole/` or `.github/workflows/` would turn into code execution
the next time git, direnv, an editor or CI runs), for build and task files
(`build.rs`, `Cargo.toml`, `Makefile`, `package.json`, `pyproject.toml`,
`setup.py`, `Dockerfile`, ...), for instruction files tole or another agent
loads (`SKILL.md`, `AGENTS.md`, `CLAUDE.md`) and for scripts (`.sh`, `.ps1`,
...). The guard reads the target the tool reports through
`Tool::approval_target` — the same jail resolution `execute` uses, so a
symlink already in the workspace (`docs -> .git/hooks`) cannot launder a
hook write — with Windows trailing dots/spaces normalized and 8.3 short
names refused; a tool that reports no target fails the guard (prompts).
The preset needs an explicit workspace (never the implicit cwd, which
is usually a whole repository) and is refused on a home or filesystem-root
workspace. Residual risk, accepted and documented: an
auto-allowed write can still change ordinary files the user later executes
(a `Makefile`, a script, source code); the preset is for a workspace whose
results the user reviews, not a sandbox. `delete_file` (Destructive) and
`run_command` keep prompting, unlike `--yes`. A project config can name the
preset only after its content was trusted, the same bar as
`allow = ["write_file"]`.

## Command patterns (#438)

`run_command:<argv pattern>` allow patterns match the argv the tool
executes — the same splitter, no shell — so `git status; rm -rf /` cannot
chain a second command (it would run `git` with literal arguments). An
ALLOW compares the program literally (`/usr/bin/git` does not match `git`,
`run_command:*` matches nothing), so an allow can only be as wide as
written. Choose allows with care: `run_command:git *` also allows
`git -c core.pager=… log`, `git config alias.x '!…'` and other ways a
program runs code from its arguments.

A DENY (`!…`) wins over every allow and over `--yes`; it compares the
program by file name, case-insensitively and without a Windows executable
extension, so `/bin/RM` and `rm.exe` do not slip past `rm *`. A deny list
cannot be complete — `env rm`, `sh -c 'rm …'`, `find -exec rm`,
`python -c`, `git -c …`, `make` all reach other programs — so the rule
FAILS CLOSED: while any `run_command` deny is set, a command runs
unattended only when an explicit `run_command:` allow names it (a bare
`run_command` allow or `--yes` is not enough), never when its program is a
known wrapper (`sh`, `bash`, `env`, `sudo`, `xargs`, ...), and everything
else asks the human (refused where nobody can be asked). The ALLOWLIST is
the boundary. Denies travel with allow lists (config, ACP, serve), a child
agent always inherits its parent's denies, and a child's command pattern
must be covered argument by argument by a parent pattern: a parent whose
program token is a glob covers nothing, because for an allow the program is
literal.

## Memory loop — decision typing (#143)

The session summary stored by the memory loop keeps its content contract
(first prompt + final answer only — never raw tool output), so the
prompt-injection-into-memory surface is unchanged. What #143 adds is a
TYPE: sessions that executed a Write/Destructive tool are stored with
`--type decision` and a `wrote` tag. The `wrote` bit is derived from the
harness's own tool-risk accounting (durable, session-scoped
`fact.wrote_this_turn` register — set on fresh execution AND
crash-replay, never reset by turn machinery), not from model-asserted
content — the model cannot claim or suppress the decision typing.

## Turn-end stop gates (#145)

`--on-turnend` gates run owner-controlled scripts at the final-message
boundary. Two surfaces are deliberate and bounded: (1) the deny reason is
owner-authored input (the gate script is trusted the same way
`--on-pretool` scripts are — argv-executed, env-scrubbed, timed out; since
#676 a gate is its own `[secrets]` consumer, `turnend`, with no secret by
default, because it usually runs code the agent wrote), and
it enters the transcript as a user-role entry the model will read; (2) the
final-text preview handed to the gate is capped at 8 KiB and never leaves
the host process. Denials are capped at 3 per turn, so a permanently
failing gate cannot livelock the loop — the turn settles durably as
`StopGateBlocked`, visible to replay.

## Cooperative cancellation (#178/#184/#185)

All three server faces expose cooperative turn cancellation: ACP
clients send `session/cancel`, REST callers `POST /sessions/{id}/cancel`,
MCP clients call `tole_session_cancel`. Steering (soft interrupt, #377) goes through the same authenticated faces (`POST /sessions/{id}/interrupt`, `tole_session_interrupt`; detaching, #217, via `POST /sessions/{id}/detach` / `tole_session_detach`, only stops waiting — it never runs or approves anything) and adds no authority: the message is ordinary user-role input to the session, so every tool the steered turn runs still passes the session's approver. Every path sets the same
per-session token; a blocking turn observes it at checkpoints (between
provider calls and tool executions) and settles
`stopReason: "cancelled"` as a normal, durable turn end — the session
resumes as usual. Per the ACP and MCP specs the receiver MAY ignore
cancellation for work that cannot be stopped: a single in-flight tool
call (including `run_command`) is not interrupted; the token is
checked before the next one. Unknown session ids 404; cancelling an
idle session is an idempotent no-op. The `tole_session_*` glob in the
`--trust internal` preset covers the new MCP tool.

## Depth-1 child agents (#171/#174)

`agent_start` spawns a child tole session; `agent_poll` reads its
result. Both are `Risk::Write` (#300): a settled poll consumes the
mailbox (uteke forget + `mailbox_consumed` in meta.json), so the tier
matches the side effect and plan mode drops it (plan mode also drops
`agent_start`, so no child exists to poll). `--trust internal`
allowlists `agent_poll` by exact name to keep the poll loop unattended;
`agent_start` still prompts. The registry enforces a structural depth cap (no
grandchildren), results travel via ephemeral uteke mailboxes, and the
parent-only `--agents-worktree` flag gives each child its own git
worktree. A child's prompt is model/operator-supplied — the same trust
tier as the session system prompt. Child sessions are ordinary durable
sessions: approval gates, risk tiers, and secret redaction apply
unchanged inside them. Since #595 a child does not inherit the parent's
whole environment: it gets the ENV-1-scrubbed env plus only its provider keys
(`child_env::PROVIDER_SECRETS`), so MCP tokens, the serve token and other
secrets of the parent never reach it. Since #598 it also gets its parent's
grant (the secrets the parent's `[secrets]` release binds to some tool,
narrowed by a profile's `grants`), so `GITHUB_TOKEN` reaches it only that way. Consequence: a child cannot use an integration whose secret
lives only in the parent's environment (HTTP MCP servers, `systemone`, web
backend headers).

Typed handoff (#383) and sibling notices (#380) add two child-to-agent
channels, both carrying model-authored text that is treated as untrusted. A
role child gets a Write-tier `handoff_submit` tool that `agent_start`
allowlists by exact name; it can only write one schema-validated, bounded
file at the path the parent chose (the role is fixed by the parent), so it
does not widen what the child may do. Artifact paths and refs are validated
(workspace-relative, no `..`, backslash, colon or leading `-`; plain ref
names), and any handoff or sibling-log text shown to another model is
escaped and bounded. The shared sibling log is a same-user channel between
processes that already share the parent's authority; a forged line can only
add notices, and ids read from it must match the agent-id charset.

The verify gate is cooperative, not adversarial: a child that shares the
parent's workspace can write files under `tole-agents/<id>/`, including
`meta.json` (role, `keep_mailbox`, prompt) and its own handoff, so a
deliberately hostile child can make its gate read `closed`. It gains no
authority from that — it already runs with the parent's approvals and could
report a passing handoff directly. What a forged `meta.json` must never
reach is anything outside the child's own agent dir: the mailbox room is
derived from the validated agent id, never read from the file.

## `systemone_decide` (#172/#173)

ReadOnly tool, active only when `SYSTEMONE_API_KEY` is set
(`SYSTEMONE_BASE_URL` selects the backend — hosted Jev default,
self-hosted compatible). The decision payload sent to the backend is
model-controlled context: treat the System One backend as an external
data flow. The tool executes no writes; results are advisory input to
the session like any other ReadOnly tool.

### Task-list tools (#198)

`todo_write` mutates only the session's task list, which lives in the
write-once session log itself (the tool's result entry is the record; no
side channel, no file). The threat surface equals any Write tool: prompt
injection could rewrite the plan, but the list is data, never executed —
and every revision is auditable in the replay. `todo_read` is ReadOnly.

### Mission mode (#199)

`tole mission` chains normal durable turns autonomously. The risk frame
is unchanged and deliberate: the same approval gates, risk tiers, and
write-once audit apply per chained turn — autonomy does not widen the
boundary. Budgets bound blast radius in time, steps, and tokens
(#201); exhaustion is resumable, never a dead session. The durable cost
report keeps autonomous spend honest and comparable — an unmeasured
mission is an unaudited one. `--verify` gives the operator a
machine-checkable completion condition stronger than the model's own
claim. It runs without approval and usually executes code the agent wrote
(`cargo test`), so it gets the ENV-1 scrub as the `verify` consumer, with
no secret unless a `[secrets]` entry grants one (#676). Destructive tools remain structurally un-auto-allowable in
missions.

### Remote approver trust boundary (#200)

The `/approvals` decision endpoint is the remote trust boundary: it is
behind the same bearer-token auth + rate limiter as every serve route,
and a decision is a one-shot for exactly one queued effect (fingerprint
of tool + canonical input) — never a blanket allow. Decisions expire to
denied so a lost connection cannot strand a mission, and every decision
writes a durable audit register naming the approval id, tool, and
verdict. The phone/CLI holder is therefore a full approver: treat the
token as approval authority and scope it accordingly.

### Event streams (#524)

`GET /sessions/{id}/events` is read-only and behind the same bearer token and per-IP auth
limiter as every route; it adds no authority. What it adds is **long-lived connections**: each
open stream pins a connection thread, so streams are capped at 8 per server (the 9th is `429`)
and count within the 32-connection cap, which keeps `/approvals` and `/prompt` reachable while
streams are open. A stuck client cannot stall a turn: ephemeral frames go through a bounded,
non-blocking per-stream queue (a full queue drops frames and reports `resync`), and writes carry
the existing 30 s write timeout. What flows is what the token holder could already read:
durable frames are the log's entries (the same as `GET …/entries`), and ephemeral frames carry
streamed model text, tool inputs and previews, and the pending-approval rows `GET /approvals`
returns. Approvals are still decided only through `POST /approvals/{id}/decision`; the stream
only reports them, and nothing about it can approve, deny or run a tool.

### Phone as approval surface (#202)

uteke-mobile consumes the #200 queue: the phone becomes a remote
approver. The boundary is the serve token — it carries approval
authority, so device compromise equals write access to the box. Locked
down accordingly: the token is a revocable secret (rotate = serve
restart), decisions are one-shot per (session, effect) with expiry to
denied (a stolen device cannot bank future approvals), every decision
is audited on the session, and there is no push endpoint in tole — the
phone pulls, so the attack surface tole exposes is exactly the
authenticated REST face. Scoped device tokens (per-device, revocable,
read-only vs approver roles) are the recognized follow-up; until then
the deployment guidance is a dedicated OS user + minimal
`--allow`/`--trust` on the serve process.

### Remote MCP servers (#530)

`--mcp-server name=https://…` connects the MCP client to a remote
Streamable HTTP endpoint instead of a child process. The trust model of
the tools is unchanged: every tool is `Write`, server metadata is never
trusted for risk, results are capped (256k chars) and fenced, and
`--allow 'mcp_<name>_*'` is the operator's way to pre-authorize a server
they trust. What the transport adds:

- **Token handling.** The bearer token is read from `TOLE_MCP_<NAME>_TOKEN`
  only (never from a spec, flag or config file, which get committed and
  printed), re-read on every (re)connect, and sent only to that server's
  endpoint. `TOKEN` in the name also makes the child-process environment
  scrub (ENV-1) drop it from the processes tole spawns. Errors and the
  warnings tole prints never carry it: every reactor error is scrubbed of the
  token, the url and its host (transport errors echo them, and a sloppy
  server may echo the `Authorization` header in an error body). The client
  echoes the NORMALIZED url (lowercased host, default port dropped), not the
  string the operator typed, so the url, its path and query, the authority and
  the bare host are each scrubbed case-insensitively (a server `https://Example.COM:443/x`
  must not surface as `example.com`).
- **Transport safety.** `https` is required; plain `http` only for
  `localhost`, `127.0.0.0/8` and `::1` (lookalikes such as
  `127.0.0.1.evil.com` are refused). A url the client's own parser (the `url`
  crate) refuses, one with embedded credentials, whitespace, a percent-encoded
  host or a backslash is refused (parsers read `\` as `/`, so the host that was
  validated could differ from the host contacted). A redirect to another origin is not followed (rmcp's same-origin
  redirect policy), so a server cannot bounce the token to a third host. The
  url is re-validated at connect time because the config type is public.
- **Disclosure.** Tool arguments and results go to the remote operator, like
  any hosted API; use `--allow` only for servers you would send that data to.
- **Residual: the token is bound to the server NAME, not the url.** A config
  that points an existing name at a different url sends that name's token
  there. The place where that can be abused is a cloned repository's
  `.tole/config.toml`; `mcp_server` is already behind the content-bound
  project-config trust gate above (a changed file is untrusted again), so a
  repo cannot add or repoint a server without the operator approving the
  exact bytes. User-level config and flags are operator trust.
- **Residual: the env name folds case.** `Nginjen` and `nginjen` are two
  server names but one variable (`TOLE_MCP_NGINJEN_TOKEN`), so they would
  share a token. Name servers distinctly; a trusted config is operator trust.
- **Residual: system proxy.** The HTTP client honors `HTTPS_PROXY` /
  `HTTP_PROXY`. An `https` token stays end-to-end encrypted through a CONNECT
  proxy, but a loopback `http` url reaches a proxy in clear unless
  `NO_PROXY` lists loopback.
- **Residual: availability.** A remote server can hang or stream slowly; the
  handshake and per-call timeouts and the connection-eviction contract are
  the same as for stdio, and a transport failure drops the connection so
  the next call reconnects (and re-reads the token).
- **Build.** The HTTP client lives behind the existing `mcp` feature, so an
  embedder or mobile build without it links neither `reqwest` nor the HTTP
  stack. TLS is rustls with the `ring` provider already used by `ureq`
  (installed once, process-wide, unless the embedder installed another).

### Review gate before `git commit` (#534)

While the cora review tools are attached, the `git` tool's `commit` op and
`run_command` / `job_start` running `git commit` ask cora's own commit gate
(`cora hook verify`, cora >= 0.20.0) first and are refused when the staged
diff has no validated review (exit 11) or a blocking one (exit 10). cora owns
the record: its marker is bound to the exact change set of the staged diff, so
tole computes and trusts no hash of its own, and a verdict for another diff
(or an edited index, or a marker older than 24 h) does not unlock the commit.

- **Which repository is asked.** `cora hook verify` reads the staged diff of
  the repository it runs in, so the gate runs it where the commit really
  happens: `git -C <dir> commit` is verified in `<dir>` (each `-C` relative to
  the previous one), never in the session repository. A commit that redirects
  git's git dir or work tree (`--git-dir`, `--work-tree`, `GIT_DIR=`,
  `GIT_WORK_TREE=`, `GIT_INDEX_FILE=`, `-c core.worktree=`) has no directory
  cora can be asked about, so it is refused under `block` rather than verified
  against the wrong index.
- **A guardrail, not a boundary.** The approver is the security boundary. The
  gate makes a cooperating agent do the review step; it does not stop one that
  is trying to avoid it: `sh -c "git commit"`, a script written then run, a git
  alias (`git -c alias.x=commit x`), or other commit-creating porcelain
  (`merge`, `cherry-pick`, `am`, `revert`) are not recognized. Each of those
  still goes through the approver like any other Write call (which `--yes`
  pre-approves). A direct `git commit`
  by a human is not gated by tole (cora's own pre-commit hook is the tool for
  that).
- **The review is self-review.** The reviewer is the model that wrote the
  change (cora's agent-as-reviewer has no LLM of its own). The gate guarantees
  the review ran on this exact diff, not that its judgement is sound, and it
  does not replace the independent CI review.
- **Fails open on tooling trouble, on purpose.** A cora without `hook verify`
  (usage error), a missing or broken binary, or a 15 s timeout produces ONE
  warning and the commit proceeds; locking an agent out because a tool broke
  would only teach operators to turn the gate off. `tole doctor` shows whether
  the gate can work.
- **Only when a review is possible.** The gate activates after both review
  tools are registered. With `--no-auto-mcp`, no cora, or any face without MCP
  servers (`serve`, `acp`, `mcp`), there is no gate.
- **Operator control.** `TOLE_REVIEW_GATE=off|warn|block` or `--review-gate`
  (the flag wins), or the config key `review_gate`. A project's
  `.tole/config.toml` is untrusted input, so the key is merged differently from
  every other one: the STRICTER of the project and user values applies
  (`block` > `warn` > `off`), so a cloned repo can keep or tighten the gate but
  never switch off what the user chose. With only the project layer set, the
  value is used as given, behind the same content-bound trust gate as the other
  security-sensitive keys (a trusted file that says `off` is the user's own
  decision). Flag and env still win over config. The model cannot
  change it (the environment is not model-writable and `cora hook verify` runs
  with the child-env scrub). Child agents inherit the non-secret environment, so they are
  gated the same way.

### `tole setup cora` (#535)

`tole setup cora` runs the fixed command `cargo install cora-code --locked`
(the one constant `tole doctor` also prints; never assembled from user input)
so the review flow can be set up without hunting for the install line. Trust
is that of `tole upgrade`: crates.io is the source, cargo verifies the registry
checksum, tole downloads and executes nothing itself, and `cargo install`
compiles the crate and its dependencies (their build scripts run on the
operator's machine, as for any `cargo install`).

- **Never unprompted.** It needs a `y` at a terminal or an explicit `--yes`;
  without a terminal and without `--yes` it refuses (exit 2). It is a CLI
  subcommand only: no tool exposes it, so a model cannot trigger an install,
  and it is skipped by the config gate like `upgrade`.
- **Idempotent and bounded.** A supported cora is left alone; `--check` only
  reports. The result is verified at `$CARGO_HOME/bin/cora` (not on PATH
  yet, or an older copy may come first), and a result that is still too old
  or does not answer `--version` is an error, not a success.
- **Residual.** Whatever `cora` resolves first on PATH is what tole runs,
  including the auto-attached `cora mcp`; setup warns when PATH still resolves
  a missing or too-old copy but does not rewrite PATH. A prebuilt-archive
  download path is not implemented: `cora-code`'s release layout was not
  verified, and cargo already gives checksum-verified installs.

### Web tools (#215)

`web_fetch`/`web_search` are ReadOnly: results enter context as model
content and are never executed — the exfiltration framing is identical
to any tool output. Fetch is text-only (no JS, no browser), size-capped
(512 KB), content-type allowlisted; search used to require an explicit
`TOLE_WEB_SEARCH_URL` backend (no keyless scraping); since #625 the default
build has built-in keyless search (below), and a build without `web-builtin`
keeps the old rule. A
crafted page CAN steer a mission via prompt injection; the mitigation
is the same as every other untrusted input: tool-result fencing,
approval gates on anything that matters, and mission budgets bounding
the blast radius.

Built-in web tools (#625, the default `web-builtin` feature of tole-cli):
without a `TOLE_WEB_*` backend, `web_search` uses nginjen-lite's keyless
engines (DuckDuckGo Lite, then Bing, then Brave; a non-2xx answer moves to
the next) and `web_fetch` uses its HTML converters, both on tole's own HTTP
client (one client, one TLS stack). The guards stay in tole:
- **Every hop is vetted.** The client follows redirects one hop at a time
  (`get_following_redirects`); each hop goes through `web::vetted_get`,
  which checks the host up front and again in the connection's own
  resolver (pinned, so a DNS rebinding between the two fails closed). An
  https request never hops to plain http, and a hop whose URL carries a
  known secret is refused before it is sent.
- **Fetch:** http(s) only, a URL carrying a known secret is refused, a
  private or loopback host is refused before anything is sent
  (`TOLE_WEB_ALLOW_PRIVATE=1` opts out), robots.txt is honoured (a robots
  failure is fail-open, as in lite), the content type must be text, JSON or
  XML, and the body is capped at 512 KiB (truncated and flagged) and the
  text at 20,000 characters.
- **Search:** the query is redacted with the process secrets and capped
  (512 characters) before it leaves; engine answers are capped at 2 MiB,
  results at 8.
- **Politeness:** `web_fetch` and its robots.txt requests send
  `tole/<version> (+https://github.com/codecoradev/tole)`. The search
  engines get nginjen-lite's own browser-like User-Agent, because they
  block other agents. A host gets at most one request per second.
- **Memory:** the robots cache (64 origins) and the per-host pacer
  (64 hosts) are bounded; no threads.
- **Residual:** the engines see the (redacted) query, and their results
  and every fetched page are untrusted text (prompt injection as above).
  There is no domain allowlist (owner decision): unattended runs use the
  same tools.

nginjen backend (#625 part 2): with `NGINJEN_BASE_URL` and
`NGINJEN_API_KEY` both set, both tools go to that service. The search query
(redacted) and the fetch target are disclosed to its operator, as with the
generic backends.
- **The key** is a bearer token sent only to that backend. No redirect is
  followed, so it never reaches another host. It is a secret-named
  variable, so the process `SecretSet` redacts it everywhere; no error,
  spec or doctor line carries it.
- **The fetch target** is vetted before anything leaves: http(s), no
  credentials, no known secret, and a public host
  (`TOLE_WEB_ALLOW_PRIVATE=1` opts out). The backend resolves the host
  again itself, so as for the generic backend this keeps internal
  addresses from being asked about; it is not a defense against DNS
  rebinding at the backend.
- **Jina:** a result marked `engine_used: "jina"` is discarded, so text
  from a third party tole did not choose never reaches the model.

The order per tool is a `TOLE_WEB_*` backend, then nginjen, then the
built-in tools. `tole doctor` names the lane, never the URL or the key.

Fetch backend (#532): with `TOLE_WEB_FETCH_URL` set, `web_fetch` sends the
target to that backend instead of fetching directly (backend only, no
fallback, so behavior is predictable). It stays ReadOnly with the same
model-content-never-executed trust as search. What changes: the **target
URL is disclosed to the backend operator**, and the backend, not tole,
makes the request. tole therefore vets the target first and sends nothing
if it fails: https only, no credentials in the URL, and a host that
resolves to public addresses (`TOLE_WEB_ALLOW_PRIVATE=1` opts out as for
direct fetch). The backend resolves the host again itself, so this keeps
internal addresses from being *asked about*; it is not a defense against
DNS rebinding at the backend, which is the operator's to enforce. Headers
(`TOLE_WEB_FETCH_HEADERS`, same rules as the search headers) go only to the
backend, redirects from the backend are not followed, and errors never
carry a value. The backend's JSON envelope is bounded (1 MiB) and its text
is truncated to the direct-fetch limits. Not covered: what the backend
returns is arbitrary text from the internet (prompt injection as above).

Search backend credentials (#436): `TOLE_WEB_SEARCH_HEADERS` values (with
`${VAR}` references expanded from the environment) are sent only to the
`TOLE_WEB_SEARCH_URL` backend: the search agent follows no redirects, so a
backend cannot bounce the credential to another host. Header names are
RFC 9110 tokens, transport headers (`Host`, `Content-Length`,
`Transfer-Encoding`, `Connection`, `Content-Type`) are refused, a value
with a line break is refused (no header injection), and errors name the
header or variable, never the value.

### Project config trust (#208)

`<cwd>/.tole/config.toml` is untrusted input from a cloned repo: it may
carry `allow`, `trust`, hooks, `skill` and `mcp_server`. Part 2 added the
trust machinery, part 3a wired the startup gate and the low-risk keys, part 3b
applies the security-sensitive ones (`trust`, `allow`, `mcp_server`,
`on_pretool`/`on_posttool`/`on_turnend`, `skill`, `plan_mode`, `no_auto_mcp`,
`no_skills`, `[mission]` `verify`/`verify_timeout`):

- **Content-bound, outside the repo.** The approval is a snapshot of the exact
  file bytes in `$CODECORA_HOME/tole/trusted-configs.json` (default
  `~/.codecora`), keyed by canonical project directory. Any byte change
  (including whitespace or CRLF) makes it untrusted again; the repo cannot
  pre-trust itself. There is no home-less fallback: with no `CODECORA_HOME`
  or `HOME` tole errors instead of writing a store next to the project. The
  home is resolved by ONE shared function (`tole_core::paths`, #344) also used
  for user-global skills and the update-check cache: empty = unset, a relative
  value is refused, and skills / the update check are skipped (never read from
  or written to the cwd) when it is unresolvable.
- **Terminal-safe prompt.** The path, the full content and the diff against the
  previously trusted version are attacker-controlled text shown to a human at
  the decision. Control characters (ESC, CR, NUL, DEL, C1), bidi
  overrides/isolates, line/paragraph separators and zero-width characters are
  printed as visible `\u{hex}` escapes, so the prompt cannot be rewritten.
- **Fail closed.** Without a terminal there is no question: the answer is an
  error naming the file and `tole config trust`. A corrupt, unreadable or
  unsupported-version store is a hard error and is never overwritten.
  `tole config trust` refuses a file that does not validate.
- **Explicit path = intent.** A file named on the command line (`--config`) is
  user intent and does not consult the store. `--no-config` skips discovery,
  the gate and any output.
- **Startup gating.** Every command except `config`, `upgrade`, `setup` and `approvals`
  runs the gate before anything else (before the update banner, session
  creation or provider access). Only `run`/`chat`/`resume`/`mission` with a
  terminal on stdin AND stderr (and not `--prompt-file -`) may ask; `sessions`,
  `status` and the protocol faces `serve`/`acp`/`mcp` NEVER prompt — their
  stdin/stdout are protocol channels — and fail closed with the exact
  `tole config trust` instruction while the command does not run.
- **Vetted bytes only.** The gate reads the file once; that exact content is
  what gets parsed into the settings. The file is never re-read after vetting,
  so a swap between check and use cannot smuggle in other content.
- **No config, no change.** Without a config file nothing is read, printed or
  looked up (not even the trust store location).
- **Secrets stay out.** The API key and serve token are env-only; the schema
  rejects secret-like keys, and `config check` prints only model/base_url
  values.
- **Sensitive keys apply only after trust.** `allow`, `trust`, the hooks,
  `mcp_server` and `skill` take effect through the SAME single startup path as
  every other key: nothing from the file is read into the settings before the
  gate passed, and `--no-config` drops all of them. A config `allow` / `trust`
  list feeds the very same allowlist machinery as the flags.
- **Destructive is never allowlistable via the config.** `allow = ["*"]`,
  `allow = ["delete_file"]` or a trust preset cannot skip the Destructive
  prompt on the interactive CLI (the approver checks `Destructive` before any
  pattern), and on the non-interactive faces the Destructive tools remain
  structurally unregistered. Tests drive the real binary with such a config and
  assert the file survives.
- **Faces that refuse hooks refuse config hooks too.** `serve`/`acp`/`mcp`
  refuse `--on-pretool`/`--on-posttool` (and `mcp` `--on-turnend`), `mission`
  refuses the hooks, `--plan-mode`, `--memory` and the client-session flags
  (`--skill`, `--no-skills`, `--mcp-server`) on every one of these faces. The
  check runs on the EFFECTIVE (post-config) values, so a safety hook or deny
  policy arriving from the file makes the command fail loudly (with a
  `--no-config` hint) instead of being silently ignored.
- **Replace, never merge; booleans only up.** A higher layer replaces a whole
  list key, so a flag cannot be "extended" by a hostile file; a boolean key is
  `flag || config` (a flag cannot switch a config `true` off, `--no-config`
  can).
- **Unsupported keys are loud.** A build without the `mcp` feature rejects
  `mcp_server`/`no_auto_mcp`, one without `shell-tools` the hook keys and
  `memory`, naming the key and the feature.

Residual risks: a trusted config is trusted — a user who approves a malicious
file at the prompt (or with `tole config trust --yes`, or names it with
`--config`) grants exactly what the equivalent flags could grant (allowlisted
Write tools, hooks that run as the user, MCP servers spawned as the user,
skills injected into the prompt); it is shown in full, but a human decides; the trust store is a plain 0600 file in the
user's home, so anything running as that user can edit it; the store update is
an unlocked read-modify-write, so two concurrent `tole config trust` runs can
lose one of the two records (the loser is simply asked again; the file itself
is replaced atomically and never corrupted).

### Context compaction (#440)

A compaction replaces, on the wire only, the settled turns before the kept
tail with a model-written summary; the session file keeps every entry. The
summary is committed atomically with its usage row (a crash before the
commit leaves the session pre-compaction), only between turns and at a turn
boundary (no tool result is separated from its call), and an entry whose
kept-entry id is unknown is ignored (full transcript). Trust: the summary is
model output that may restate tool output, yet it reaches the wire as a
user-role message (an assistant-first conversation is rejected by some
OpenAI-compatible endpoints, and the system role would RAISE its
authority). The marking is therefore structural: the summary goes inside
the same untrusted-content fence as tool results (forged fence markers in
it are neutralized), introduced as a record rather than instructions, and
the summary instruction tells the model to report instructions found in
tool output as data. Residual risk: a prompt injection that already steered the model
before compaction can persist through the summary, as it would through the
model's own earlier answers. The window lookup is one `GET /models` on the session's own provider,
with the same base URL and credentials as chat completions (no new host or
secret), at most once per process and only after a prompt passes 8k tokens;
an explicit `TOLE_CONTEXT_WINDOW` / config `context_window` skips it.
A provider context-overflow error (#481) sets a durable `fact/context_overflow`
register in the commit that settles the failed turn; the next prompt then forces
one ordinary compaction pass (same atomic, between-turns, fenced-summary path) and
clears it. Classification reads only HTTP 400/413/422 error text, so a tool output
or model text can neither set nor suppress the flag.

### Global session index (#439)

`$CODECORA_HOME/tole/sessions-index.jsonl` maps session ids to session files
and cwds across projects. It holds no conversation content (previews are
read from the session file at list time), is created owner-only (0600), is
append-only (one `write` per line; a torn or foreign line is skipped, never
fatal) and is best effort: a failure never affects a session, and
`TOLE_NO_SESSION_INDEX=1` disables it. The index never moves a session
across projects: `resume <id>` for a session of another project REFUSES and
prints the command to run in that project, because this process's workspace
jail, project config and trust were resolved for the current directory and
would otherwise act on the wrong tree. Anyone who can write the index can
already write the session files it points to (same home).

### Image input (#212, #559)

Images are **user-supplied input, never a tool**. Input paths: `--image` on
`run|chat|mission`, `/image` in `chat` and the TUI host's pending images (any
turn, #559), and ACP `image` content blocks (#559). Every path ends in the
same resolver: a file path must canonicalize inside the workspace jail
(symlink and `..` escapes refused), inline bytes (a `data:` URL or an ACP
block's base64 `data`) are decoded, capped at 10 MiB and spilled
content-addressed to `$CODECORA_HOME/tole/images/`, and the type is sniffed
from the bytes (PNG/JPEG/GIF/WebP only). An ACP client's declared `mimeType`
is not trusted and its optional `uri` is never read, so a remote client
cannot name a host path; one bad image refuses the whole prompt instead of
being dropped silently. The log records only the reference (path, mime,
size, FNV-1a hash); every request re-reads the file and re-checks jail and
hash, so a swapped file is reported, not sent.

The image **content** is untrusted: text rendered inside an image can carry
instructions exactly like a fetched web page or a tool result, and the same
mitigations apply (approval gates on anything that matters, budgets). The
spill dir grows with each distinct inline image and is not pruned (same
home as the session logs; an operator cleanup).

**No model-callable image tool (decision, #559).** Tool results are text in
the OpenAI-compatible format, so a `read_image` tool would need an
attributed user message committed by the harness at a safe point, plus its
own threat-model entry (a model-chosen path, an injection channel the user
did not pick). Until a follow-up issue makes that case, the model cannot
pull images on its own.

### TUI `/export` and `/copy` (#463)

`/export` writes the session transcript to a file. Transcripts hold tool
output and can therefore hold secrets, so the file is created owner-only
(`0600`). Since #596 the entries it renders are the log's redacted form, so a
known secret value never reaches an export of a session started after it; a
session logged before #596 keeps its raw lines and exports them as they are. The target is
workspace-jailed (canonicalized parent, symlink-safe, `..` and outside paths
refused), an existing file or symlink is never overwritten or followed
(`create_new`), and the only directory it creates is `.tole/exports/`, level
by level with a jail check each time. The HTML page escapes every piece of log
text, carries no script and no external resource, and sets a
`default-src 'none'` CSP, so a hostile tool result or model answer is inert
text when the page is opened. `/copy` writes one OSC 52 sequence whose payload
is only base64 of the last answer (capped at 64 KiB), so answer text cannot
inject terminal escapes; what the terminal does with the clipboard write is
outside tole (some terminals ask first or ignore it).

### TUI themes (#463)

A theme decides how the approval prompt and error notes look, so it is an
input an attacker would like to control (a prompt drawn invisible, or styled
like a harmless line, gets a Write approved by accident). Three rules close
that: (1) the theme is read from the **user** config and the environment only;
`config::layered` drops a project config's `[tui]` table (which still parses,
so the file loads), startup prints one warning and `tole config check` marks
the keys `IGNORED`. (2) The theme vocabulary cannot hide text: no background
colors at all, no hidden / reversed / blink modifier, so the worst a user-set
theme does is choose a foreground color. (3) The approval and note roles are
critical: never dim and bold in every custom style. A foreground color close to
the user's own background cannot be detected and is not rejected; that choice
is the user's. Theme values are plain config strings: no code runs, and the
values shown in notes pass through the TUI sanitizer. An invalid value falls
back to the built-in default instead of aborting.

### TUI keybindings (#463)

A key map decides what a key does, so it is an input an attacker would like to
control: rebinding `y`, `n`, Esc or Ctrl+C would intercept an approval answer
or the cancel key, and a project config that rebinds keys could make a
familiar keystroke do something else. Four rules close that: (1) the keys are
read from the **user** config only (no environment variable): `config::layered`
drops a project config's `[tui]` table, startup prints one warning and `tole
config check` marks `tui.keys` `IGNORED`. (2) The set of actions is closed and
convenience only (tool-output and reasoning toggles, pickers, cycling,
newline, queue, history); sending, cancelling, approval answers and typing are
not actions. A binding must carry Ctrl or Alt (or be a function key), and
`y`, `n`, Esc, Ctrl+C, Enter, Backspace, Tab and the keys a terminal cannot
tell apart are refused. (3) At an approval prompt (and in a picker) the keymap
is not consulted: only `y`, `n` (plain, no Ctrl/Alt), Esc and Ctrl+C act, so no
table can intercept or swallow them; the dispatcher also refuses to map those
keys, bare characters, Tab and bare Enter whatever the table holds. (4) An
invalid or clashing table falls back to the whole default keymap with one
note; it never aborts and never leaves a key half-bound. Key values are plain
config strings; they pass through the TUI sanitizer when shown in a note.

### Profiles (#584)

A profile (`[profiles.<name>]`) names a persona, an allow list, skills, a
memory room and models, so it is an input an attacker would like to control: a
cloned repository that defines `profiles.coder.allow = ["*"]` would pre-approve
every Write for anyone who runs `--profile coder` there. Profiles are therefore
read from the **user** config only: `config::layered` drops a project
config's `[profiles]` table, startup prints one warning and `tole config check`
marks it `IGNORED`. A profile cannot widen what the existing rules allow:
`Destructive` stays unallowlistable, plan mode and the face refusals apply as
for the flags, and its values sit below the per-key flags. Secrets are never
stored in a profile: `grants` holds environment variable NAMES (a value-shaped
entry is a config error that does not echo it), and the secret-like-key guard
applies inside profile tables too. Dormancy is structural: resolving a profile
reads that profile's persona file (bounded to 64 KiB) and nothing else; no
process, provider call or memory read happens for any profile. The persona
becomes the system prompt and reaches the provider like any other system
prompt (secret redaction applies, #595).

### Delegation tree (#585)

`tole_core::delegation` holds the caps that replace the depth-1 rule for the
CLI faces (`run`, `chat`, `mission`): there the agent tools are `delegate` /
`delegate_poll`, with `agent_start` / `agent_poll` kept as deprecated aliases
for one release. `tole serve` keeps the classic depth-1 `agent_start` for now,
and cron jobs register no agent tools, so an unattended run cannot delegate at
all. All the caps sit in one module so no caller can skip one:
- **No budget multiplication.** A child's tokens and steps are carved from its
  parent's remaining budget and its deadline is never later than the parent's;
  a running child's whole carve stays reserved, so any number of children
  share one root pot. A spawn with nothing left is refused. Unknown usage is
  charged in full, so a crash cannot refund a budget it may have burned: a
  child whose process is gone is charged its whole carve unless the host
  measured less, a budget stop is charged its whole carve, a settle never
  goes below what the node already reported, and a node's reported own
  spend can only grow. Only a child that never started refunds everything.
- **Depth** is 2 by default and can never exceed 3 (`max_depth` outside 1–3 is
  refused when the goal opens); a node at the limit cannot delegate.
- **Authority only narrows.** The child allowlist is the profile's patterns
  that a parent pattern covers, plus every deny of both; uncovered patterns are
  dropped and recorded. A `consult` node cannot spawn an `execute` node or take
  a write lease.
- **One writer per path.** An `execute` node takes a write lease before
  writing; a lease conflicts with any live lease on the same path, an ancestor
  or a descendant, and the first lease in log order wins a race. A lease path
  must be absolute and lexically normal (`/a/b`: no empty, `.` or `..`
  component, no trailing slash); any other spelling is refused, never
  rewritten, so `/w/src/../src/a.rs` cannot dodge a lease on `/w/src/a.rs`.
  The module does no filesystem access: the host must canonicalize paths
  (symlinks, `..`, case folding) before asking for a lease.
- **Mandatory report.** A run settles `done` only with a report (summary;
  changed files need a diff ref and the checks run) and no unsettled children;
  otherwise `incomplete`, with the reasons in the log. A terminal record is
  never overwritten.
- **Audit.** The tree is an append-only log (spawns, budgets, allowlists,
  workspaces, sessions, leases, reports); dead processes settle `interrupted`
  and queued children of a settled parent `cancelled` on recovery.
- **The fold re-checks every spawn.** The host launches what the log records,
  so the fold itself ignores a spawn record that breaks a cap: an unknown or
  settled parent, an id that is not the parent's next, a depth past the goal's
  limit, `execute` under a `consult` parent, an allow pattern no parent
  pattern covers or a parent deny missing, an empty budget, or a budget or
  deadline past what the parent can still carve at that point of the log. A
  settle is never charged below the node's reported own spend, and a budget
  stop always its whole carve.

The report and the summary are model-authored text: they are recorded and
shown, never executed or trusted as instructions.

Launch contract (#585 part 2): a delegated child carries the log path and its
node id (`TOLE_DELEGATION_LOG`, `TOLE_DELEGATION_NODE`; the log must be
absolute, the id charset-checked, with no leading, trailing or doubled dot) and
its real depth. An agent dir is only ever one plain component under the agents
dir (`agents::agent_dir` refuses empty, `.`, `..`, leading-dot and
separator-bearing ids). It takes allow patterns and trust presets ONLY from the flags its
parent passed (parent ∩ profile): its profile's `allow`, the config's
`allow`/`trust` and `TOLE_TRUST` are ignored, since any of them could be wider
than what the parent granted. `[delegation]` limits come from the user config
only.

The delegate tool (#585 part 3, `crates/tole-cli/src/delegate.rs`) adds:
- **Structural depth.** The tool is registered only where the tree says the
  session's node may delegate; a classic `agent_start` child and a delegated
  child at the goal's depth get no agent tools.
- **Authority.** The child runs as `tole mission` with `--allow` = parent ∩
  profile (and, per the launch contract, nothing else).
- **Secrets.** The child keeps its provider keys plus the parent grant ∩ the
  profile's `grants`; the parent grant is the set of secret-named variables
  the parent process holds. Every other secret-named variable is scrubbed.
- **Budget.** The carve becomes the child's `--max-steps`, `--max-minutes`
  and `--max-tokens`. A settled or recovered child is charged what its session
  usage records say, and its whole carve when they cannot be read. The records
  live in the child's agent dir, which the child can write: a hostile child
  could understate its usage there, within its carve (the same cooperative
  model as the handoff file).
- **Known limit: the root session's own usage is outside the goal budget.**
  The tool cannot read the root session's usage records, so the root reports
  zero own spend when it delegates; only its children's usage is carved and
  charged. A child's own spend is measured.
- **Writers.** An `execute` child's write lease is taken on the canonicalized
  directory it works in (its worktree, else the workspace), checked before it
  spawns. In place, the pre-check refuses a writer when any lease overlaps the
  workspace (a second writer in the same workspace is refused). With
  `--agents-worktree` each writer gets a fresh worktree under
  `.tole-worktrees`, so the pre-check refuses it only when that directory or an
  ancestor of it is leased (someone writing the whole workspace); sibling
  worktrees run in parallel.
- **Machine guard.** Past `[delegation] max_live_processes` a spawn is recorded
  `queued` and started by a later delegate or poll, strictly in log order: a
  new spawn joins the queue behind older ones, and a queued writer whose
  workspace lease is taken holds the queue until it frees. An exited but
  unreaped child process (a zombie) counts as gone, so its slot frees on the
  next delegate without a poll. The file kept next to the log for a queued
  spawn holds only what carries no authority (role, artifacts to read, mailbox
  choice): allow list, budget, depth, mode, task, workspace and profile are
  read back from the node the tree recorded and the secrets are recomputed, so
  a queued sibling cannot be widened through that file.
- **One canonical root.** The host canonicalizes the session root once; the
  log key, the agents dir, the agent dirs recorded in the log and every poll
  lookup use that one value, so a symlinked or `..` workspace path still
  finds and settles its children.
- **Completion report (#585 part 4).** A delegated child gets the
  harness-owned `delegate_report` tool (its parent adds exactly that name to
  its allow list, like `handoff_submit`). It writes ONE report, at most 64 KiB,
  to `<agent dir>/report.json` (`TOLE_DELEGATION_REPORT`): created new, never
  through a symlink, a second submission refused. Every field is capped and
  control or invisible characters are escaped. The PARENT reads the file when
  it settles the child (on poll, or on the next delegate for a child whose
  process is gone): only through a plain agent dir at its canonical path,
  without following a symlink, with the size cap applied before parsing, and
  cleaned again. It records the report in the log itself; the child never
  writes the log. A child that ends without a complete report settles
  `incomplete` with the reasons; a missing, oversized, symlinked or invalid
  report counts as none, and a child whose agent dir is not plain is charged
  its whole carve. The report reaches the parent model only between fence
  markers it cannot contain, labeled as data, not instructions.
  Every file the parent reads from a child's agent dir (the report, its
  session files for usage, and `agent_poll`'s pid, meta, log and handoff) is
  opened with `O_NOFOLLOW | O_NONBLOCK` and checked to be a regular file on the
  open descriptor, with a size cap before reading: a FIFO or symlink planted
  there is refused at once, so a child cannot block its parent (which settles
  children under the delegation log's exclusive lock) or point it at another
  file.
- **`tole audit` (#585 part 5).** Read-only: it opens only the delegation log
  in the data dir (never child-writable), without the host lock and without
  repairing a torn line, writes nothing, reads nothing from the children's
  agent dirs and makes no network call. Its output is bounded and redacted:
  the task is one capped line (never the full prompt), report summaries and
  reasons are capped, control characters are dropped, and every string passes
  through the process `SecretSet`.
- **Where the files live.** The log, its lock and the queue files live in the
  tole data dir, `<codecora home>/tole/delegation/<hash of the canonical
  workspace root>/`, outside every workspace, so a child's jailed file tools
  cannot reach them; children get the log's absolute path in
  `TOLE_DELEGATION_LOG`. What a child can still write is its own agent dir
  under `<workspace>/tole-agents/<id>/` (its meta, log and session records:
  usage understatement within its carve, see Budget) and, like any session,
  whatever its allow list and approvals let it touch. A child with an approved
  or allowlisted `run_command` could write the data dir like any other path
  outside the jail; that is the same authority it already holds, and the fold
  still refuses any spawn wider than its parent.

### Delegation resource guard

`tole_core::resource_guard` (owner requirement: delegation must never
overload or crash the server) decides whether a delegated child may start on
this machine. The `delegate` tool calls it before every launch, in the drain
that `delegate` and `delegate_poll` share, so both spawn levels (depth 0 and
1) get it. A child waits in the queue, never fails, while: the
machine-wide count of live delegated children (every goal and session) is at
`machine_max_live`; the last start was less than `spawn_interval_secs` ago;
free memory is under `min_free_memory_mb` plus `child_memory_mb` for the new
child and each one still ramping up (a cgroup v2 `memory.max` counts, so a
container's limit wins over the host's free memory; its use is the working
set, `memory.current` minus `inactive_file` of `memory.stat`, so page cache
left by builds does not starve the queue, and all of `memory.current` with
a startup note when `memory.stat` is unreadable); the one-minute load per
CPU is over `max_load_per_cpu`; less than `min_free_pids_percent` of the
process-id limit (cgroup `pids.max`, else `pid_max` against the threads in
`/proc`) is free; or the data
dir, workspace or worktree filesystem has less than `min_free_disk_mb` free
for an unprivileged user. A probe that cannot be read is skipped with one
startup note, so on another OS the guard degrades to the count and interval
rules instead of blocking forever. Delegated children run at niceness
`child_nice` (never lowered below the parent's). The thresholds are
`[delegation]` keys of the user config only, bounded (zero, negative or
absurd values are config errors), so a cloned repository cannot loosen them.

The census comes from one registry per machine,
`<data dir>/delegation/machine.jsonl` (`crates/tole-cli/src/machine_guard.rs`):
every delegated child registers its `{pid, start_time}` lease when it starts,
under `machine.lock`, which is only taken while a delegation log's lock is
held (one order, so no deadlock) and held for the census, the probes and the
spawn. A lease that is dead (including an unreaped zombie) no longer counts,
so a child that died without being settled frees its slot; "ramping" means
started less than 30 seconds ago. The registry is opened without following
symlinks or blocking and read up to 256 KiB. Like the log, it is outside every
workspace; a child allowed to run commands could forge entries, which can only
make spawns wait. A wait on the head of the queue holds the queue (strict
FIFO). The `delegate` and `delegate_poll` replies carry each queued spawn's
`queued_reason` and `waited_secs`, and `tole doctor` prints the headroom
against every threshold and warns about a spawn queued longer than 10 minutes.

### User config (#433)

`$CODECORA_HOME/tole/config.toml` (default `~/.codecora/tole/config.toml`) has
the project config's schema and sits below it in precedence. It is **not**
trust-gated: it lives in the same directory as the trust store, so anyone
who can write it can already write trust records (or set `CODECORA_HOME`).
It is still bounded and validated like a project file (64 KiB, strict
schema, secret-like keys refused), `--no-config` ignores it, and every face
that refuses a hook / skill / MCP server from the command line refuses it
from the user config too, naming the user config as the origin.

### Scheduled jobs (#579)

Cron jobs live in the user's data dir (`$TOLE_CRON_DIR`, default
`<codecora home>/tole/cron`), never in a repository. Unattended agent runs
pre-authorize Writes only through the job's `allow` list; anything else is
parked in `/approvals` (`tole serve --cron`) or refused (`tole cron tick`),
never auto-approved. Run output is redacted with the process's secret set
(env secrets and web-header secrets, #595) before it is written or
delivered. Delivery targets: a `file:` target is a relative path that must
stay inside the job's workdir (no `..`, no absolute path, no symlinked
target, the parent re-checked after canonicalizing); a `command:` target is
argv (no shell, destructive argv refused, scrubbed env, 60 s timeout) and is
trust-gated: only a job whose record says `created_by = cli` (saved by a
human with `tole cron create`/`edit`) may run one, any other job is
`blocked` before it runs. `cron.daily_token_cap`, `cron.max_concurrent`,
`cron.timezone` and `cron.tz_source` are read from the user config only, so
a cloned repository cannot change them. A job's zone name is validated
before any file is read and resolves only inside the zoneinfo directory
(alias links included, nothing outside, no FIFO, at most 512 KiB). A job's skills (`--skill`) and profile (`--profile`) are validated on
save and again when a run is claimed: a skill that is gone or broken, or a
profile the user config no longer defines, records the run `blocked` before
any token is spent. Saving a job also pins each skill's content (a hash of
the skill package for a name, of the file for a SKILL.md path, and of each
profile skill): an unattended run never follows instructions that changed
after the owner chose them (a `git pull` in the workdir, a cloned repo's
`skills/`). A changed skill blocks the run (`skill_changed`) until the
owner re-pins it with `tole cron edit --skill` (or `--profile`); editing any
other field keeps the pins. Under skill approval (required by default) a skill
named by name must also be an owner-approved package (#591), else the run is
`blocked` (`skill_not_approved`). A profiled run auto-approves a Write only when both the
job's `allow` and the profile's `allow` cover it (never wider than either),
and gets the secrets `Profile::granted(job grants)`: a job grants none, so
a profile's `grants` give a cron run nothing. `GET /cron/runs` sits
behind the `tole serve` bearer token like every other route.

An `allow` on a parked cron call resumes the run (#581) through a
`resume` ledger record. The ledger fold accepts it only for a run that
settled `awaiting_approval` for the same job and was not resumed before, so
a hand-written `resume` line cannot revive a finished run or run a parked
one twice. The resumed turn gets no new prompt and the approval is a
one-shot for exactly the approved call; without an allow (denied, expired,
lost with a restart) the call parks again and never runs. An allow does
not run the call by itself: the call runs only if the resumed model makes
that same call again, otherwise it never runs. The continuation runs
under the job's model pin, step budget, token budget and time limit, each
counted for the continuation alone. The session file sits in the job's
workdir, which the job can write. `JsonlStorage::open` therefore opens it
once and uses that handle for the replay, the torn-tail repair and every
append, never re-opening the path. On unix that open never follows a
symlink at the last path component and never waits on a FIFO, and on
every platform the handle must be a regular file. The `.lock` sidecar and
compaction's temp file and reopen are opened the same way. The resume uses
`open_bounded` (64 MiB cap on the handle), so a file swapped for a symlink
or a FIFO after an earlier check is refused at the open. A directory on
the path (`.tole` or `.tole/sessions`) swapped for a symlink is covered on
unix too (#639): the storage resolves its directory once, walks the
canonical path from `/` with `O_NOFOLLOW | O_DIRECTORY` at every
component (a component swapped for a link in between is an error) and holds
that directory handle. The session file, the `.lock` sidecar, compaction's
temp file, the rename over the live file and the directory sync are all
relative to the handle, so a swap after the open redirects nothing. Still
not covered: a path that already goes through a link when it is opened (it
is resolved once, as the path says; the host cannot tell a legitimate link
such as `/tmp` from a planted one), and non-unix targets, which keep the
path-based behavior.

### Owner-only records (#684)

Some records mean "the owner did this": a cron job saved with
`created_by = cli` or enabled with `enabled_by = cli`, a plugin approval,
a skill approval, a project-config trust, the user config. They are
written by the official CLI (`tole cron create|resume|edit`, `tole plugin
approve`, `tole skills approve`, `tole skills drafts promote`, `tole
config trust`) and sit in the tole data dir (`$CODECORA_HOME/tole`).
The rules that depend on them (an agent job runs only after a human
enabled it, only the owner approves) hold only if a session cannot write
them. Two layers close the direct routes; neither is a boundary.

- **No tole binary from a session.** In every session kind (a root
  session, a cron agent run, a child agent), `run_command` and `job_start`
  refuse an argv whose program is the tole binary: by name (`tole`,
  `tole-cli`, `.exe` forms), or by resolving to the running executable
  (links included), also behind `env`, `nohup`, `timeout`, `exec` and the
  like and inside `sh -c` text. Any argument that names the marker below
  is refused too. A human running `tole` from a terminal is not affected:
  the check lives in the two tools, not in the CLI.
- **A marker for what still runs.** Every process those two tools start
  carries `TOLE_UNDER_SESSION=1` (a child agent also has
  `TOLE_AGENT_DEPTH >= 1`). The owner-only subcommands above refuse to
  write when either is set (`owner_only`), before anything is read or
  saved. The scrub keeps it (it is not secret-shaped) and the model's
  arguments cannot remove it: `env -u`, `unset` and `NAME=` forms name the
  marker and are refused.
- **No file writers in a jail that holds the data dir.** When the
  workspace contains the tole data dir (tole started in `~`) or sits
  inside it, every session kind gets no `write_file`, `edit_file` or
  `delete_file`, and tole warns at startup. Reads stay. Cron agent runs
  already refused such a workdir (`ledger_in_workdir`).

Residual, deliberately: a renamed or copied tole binary started with a
cleared environment (`env -i`, a wrapper that drops variables) is
indistinguishable from a program the owner runs; an interpreter
(`python`, `sh -c 'echo ... >> file'`, `git` hooks) can write the files
directly; the jail check compares paths, so a bind mount or a hard link
into the data dir is not seen. A session with `run_command` has the
owner's file permissions. These are defense in depth against the plain
route (the official CLI, the file tools). The real boundary is a sandbox
that cannot write the data dir (#601).

### Delegation approvals (#714)

A delegated child has no terminal, so a Write its allow list does not cover
used to be denied at once and nobody saw the question. With `[delegation]
approvals = "owner"` or `"chain"` the child asks through a durable,
append-only log (`<data dir>/delegation/<hash>/approvals.jsonl`, outside every
workspace, mode 0600) and waits; the owner answers with `tole delegation
approvals` or `/approvals` in `tole serve`; in `chain` mode a parent that holds
a `delegate_decide` grant answers within its own authority first. No answer is
a denial. The default is `off`: the child is denied at once, as before.
Design: [delegation-approvals-design.md](delegation-approvals-design.md).

What a decision is: one allow or deny of one call, bound to a SHA-256
fingerprint over node, session, tool, canonical raw input, target and
workspace; consumed (written) before the call runs. It never carries a
pattern, a budget, a secret or a profile change, and no model-written text.
The fold re-validates every record, so a forged line does no more than a legal
one could.

| Threat | Mitigation | Residual |
|---|---|---|
| **Injection through a request text** (a child writes a path, a command or an argument crafted to read as an instruction or a pre-approval to the decider) | The decider sees host-built `display` (tool, resolved target, `describe`), fenced and marked as data, never a model-written reason; a request carries no free-text field at all; the fingerprint, not the text, is what is approved; truncated display makes the request owner-only | A decider model may still be influenced by the plausible-looking content of a legitimate request (a file body). Bounded by 2.2 (only what its allow list already allows, inside the workspace guard) |
| **A child asks for more than its parent holds** | Rule 2.2.3: a level decides only if its own list allows the call; otherwise it cannot decide and the request moves up; at the root the owner decides. A deny pattern of the child or a parent is final and never escalated. Widening (patterns, profile, budget, secrets) is not an approval and is refused or dropped, never escalated | None beyond the owner's own choice to allow |
| **A parent approves on injected instruction** (its context was poisoned) | The parent decides only within its own allow list and the workspace guard; `delegate_decide` is `Risk::Write`, so at the root the owner's approver sees each decision; at depth >= 1 the tool exists only by explicit grant; `deny` and `escalate` are always available; no always-owner class is decidable | A poisoned level can approve what its list already allows, for its own subtree: the same power it has to do it itself or tell the child to. The grant is the owner's to withhold |
| **Replay** (an allow reused for another call, node, session, workspace, or twice) | One-shot: `Consume` is written before Allow is returned; the fingerprint is SHA-256 over node, session, tool, canonical raw input, target and workspace; a changed input is a new request; the fold ignores a second `Consume` and a `Decide` after `expired` | An allow is valid for one call by the one waiting process; there is no later reuse path |
| **Confused deputy** (a level approves in its authority something that acts on the child's workspace, or the owner approves something that reads differently than it runs) | The target is resolved against the **child's** canonical root and must pass the guard; the fingerprint includes the workspace; `display` and fingerprint come from the same host code path as the call; level eligibility is read from the immutable `Node.allow` of the log, not from a request field | The owner confirming a long approval without reading it. Mitigated by the 2 KiB display cap, `show` and `--fingerprint` |
| **Race between levels** (parent decides as the window closes; two levels decide; owner and parent at once) | One lock; first valid `Decide` in log order wins; validity checked at append time against the derived current level and re-checked by the fold; deny is final; the loser gets `race_lost` | None for correctness. Clock skew is single-host, and the check is by log order and the `at` the lock writer set |
| **Orphaned requests** (child or level dies; parent settles; process killed while parked; owner away) | Pending requests of a settled node fold to `expired(node_settled)`; dead levels are skipped; a request always expires (`approval_secs`, capped by the node deadline) to deny; recover settles the node; the log is bounded | A request may stay `pending` in the file until the next fold marks it; nothing can use it. An unattended machine just denies |
| **Flood** (a looping child fills the queue) | At most 1 open request per node and 32 per goal; the log is capped at 8 MiB for new requests and not read past 9 MiB, each line at 32 KiB; each denied request ends that child's turn (`ApprovalRequired`) | A cap hit denies everything new in that goal until the owner clears the file. Rotation of the log is later work |
| **Forged record** (an interpreter appends a `Decide`) | Fold re-validation; owner-only subcommands refuse under a session; #684 residual | Same as #684: the real boundary is #601 |

Always the owner, never an agent level: secrets, trust and approval records,
plugin, skill and MCP effects, a Write outside the workspace or onto a file
another tool executes, an oversized or truncated request. `Destructive` stays
unreachable (a delegated child is not interactive, so those tools are not
registered). A discussion participant (#704) never asks and never decides: it
stays read-only.

### Approval auto mode (#995, proposed, design only)

Design: [approval-auto-mode.md](approval-auto-mode.md). Nothing here is
implemented; the rows record what the implementation must hold. With the
feature off (the default) every row of "Remote approver trust boundary (#200)"
stands unchanged: an unanswered approval expires to denied. With it on for a
profile, "no answer" can become an allow, so the boundary moves from "a person
holding the serve token" to "a person, or a judge call after a silence". It is
opt-in per profile in the **user** config (a project config cannot enable it),
and it reaches only a positive allow-list of classes computed by code before
any model runs. In the recommended form the one class is `edit_file` on a
workspace path the owner listed (default: none).

| Threat | Mitigation | Residual |
|---|---|---|
| **Silence turned into consent** (an attacker floods or waits for the owner to be away, so no answer becomes an allow) | Default off; user config only; only `edit_file` on an owner-written path allow-list can be a candidate, and only when the whole edit fits 2 KiB; the judge runs once, after `after_secs` (floor 300, default 600), and the human can still decide until the 900 s hard expiry; a kill switch turns it off without a restart (proposed); per-session and per-hour caps; one auto verdict per (session, fingerprint) | **A workspace edit is delayed arbitrary code execution.** An approved edit to source, a test, a manifest or lock file, `conftest.py`, `sitecustomize.py`, a `*.pth`, `*.mk`, `*.cmake`, `*.nix` or a CI or tool config runs the next time anything builds, tests, imports or deploys; the guard's denylist ("not a sandbox") does not cover these, which is why the rule is an owner allow-list of paths, not the guard. Nobody read the edit. Reviewable afterwards in version control and the audit register, not preventable. This is the accepted cost of the feature |
| **Prompt injection of the judge** (a crafted path or argument says "answer allow") | The judge is a separate call with a compiled policy prompt, no tools, no transcript, and a closed input struct (tool, risk, class, workspace-relative target, redacted capped summary); strict JSON answer; only `allow` with `high` confidence approves; eligibility is decided by code, so a fooled judge cannot approve an ineligible call | The judge has no transcript or user prompt, so it cannot tell an edit the owner wanted from one a hostile page asked for; it answers "plausibly benign" and nothing more. A fooled judge can approve a call in the eligible class. The class limit is the control, the judge is not |
| **The asker judging itself** | The judge call shares no context with the turn that asked and sees none of its text beyond the capped summary; a dedicated judge model is recommended | A judge on the session's own model shares its weaknesses (decision A3) |
| **A class widened by mistake** (a new tool, an MCP tool, a relaxed sandbox) | Positive allow-list: a tool is `Never` until a code change lists it; a table test pins the set; MCP tools (always `Write`), `run_command`, `job_start`, egress, secrets, trust, skills, plugins, sandbox relaxation, config and data dirs, writes outside the workspace guard, `target = None`, spawned work and `Destructive` are never eligible; a config may only narrow | A future PR that adds a class is a trust decision and must say so in its title |
| **Race between a human and the judge** (a human decides as the judge answers; a late deny after an allow) | One transition out of `Pending` under the queue lock, shared by the human route, the hard expiry and the judge; the loser is discarded and not audited as a decision; the judge claim makes it run once; a human decision after an auto decision is 409 | A call already resumed by an auto allow cannot be recalled by a later human deny |
| **Fail-open by error** (provider down, timeout, malformed or partial answer, panic, audit write failure, budget refusal, restart) | Every path leaves the entry denied or expired and no one-shot. An allow is audited first (`judge_verdict`), then transitioned and announced once, then an outcome record (`applied` or `lost`) is written; a failed audit denies and never announces approved; a non-announced claim blocks a human decision for one audit write only | A restart loses pending entries and one-shots, restarts the ten-minute clock, and can leave a verdict record with no effect; the per-fingerprint rule is per process unless persisted. Availability of auto mode is best effort |
| **A decision indistinguishable from a human one** | `decidedBy` / `source` (`human`, `auto`, `timeout`) in the session register, `GET /approvals`, the event stream and `approval_resolved`; the register holds the model name, class, confidence, redacted reason and an input hash | A receiver that ignores the field treats it like a human decision (documented duty: information, not authority) |
| **Leak through the judge or its audit** (a secret in an argument reaches the provider, the register or a webhook) | Input and reason go through the same process secret redaction as every egress, are capped, and the reason text never leaves the session register (events and listings carry a reason code only); the judge sees the full edit built from the raw input (not `describe`, which shows 80-character previews), is pinned to the session's provider by default, a deterministic secret-shape check makes an edit containing a token or key `Never`, and `tole doctor` names the judge's provider and model | The redaction set holds only process secrets (environment, webhook and serve-token secrets, `web_fetch` header secrets), so a secret pasted into `new_text` that the shape check misses reaches the provider. A judge on a different provider is a new egress party (decision A15) |
| **Cost and loop abuse** (a looping agent re-asks until the judge says allow; token spend) | One verdict per fingerprint, per-session and hourly caps, small `max_tokens`, usage recorded on the session, a budget refusal is a deny | The exact budget counter is open (design decision A10) |
| **Replay after a symlink swap** (the file or path changes between approval and replay) | `edit_file` refuses a stale `old_hash` (content anchor); the candidate stores its resolved target and the replay re-checks it before the one-shot is used | The fingerprint binds the raw `path` string, not the resolved target, so the target re-check is required, not optional |
| **Unattended callers** (parked cron runs, missions) | Not eligible in v1 (`unattended_scope`) | Revisit after use (decision A6) |

Never auto-approved, and why: anything that executes attacker-influenced code
(`run_command`, `job_start`), leaves the host (egress), changes what is trusted
or remembered (skills, plugins, MCP, config, `uteke_document`), handles a
secret, spawns work (`consult_advisor`, agents), or cannot be judged from the
full text the judge sees. Any tool not listed is never eligible. An edit to a
file the owner listed is the only class left, because it can be reviewed and
reverted afterwards, and even that is delayed code execution unless the listed
paths are inert (documentation, for example).
