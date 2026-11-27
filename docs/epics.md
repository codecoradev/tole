# tole — Epics & Roadmap (v1.1, chat-first replan)

> Ground truth: Phase 1a/1b/2 complete (E1–E8, E10–E12, E4.5, gh read ops,
> TOLE_* env). Replan 2026-08-27: chat-first direction per PRD v1.1 §0
> Identity.
> Status sync 2026-09-18 (v0.3.0): Track A (A1–A2) and Track B (B1–B4)
> **complete**; MCP client shipped ahead of schedule (#74).
> Status sync 2026-10-05 (v0.5.0 on crates.io): Tracks C and D
> **complete** (E9 closed 2026-09-28; D1–D3 all shipped — D1 in v0.4.0,
> D2 2026-09-18, D3 2026-09-28). Post-D3 work is shipped too (see the
> Post-D3 section). 298/298 workspace tests, CI green. Code is
> authoritative over this doc.
> Status sync 2026-10-06 (pre-0.6.0): the 0.6.0 train (#168–#185) is
> fully merged to develop — see the Post-D3 section.
> Status sync 2026-11-20 (0.12.0 release prep): 0.12.0 is the release
> cut from develop on 2026-11-20 (see "0.12.0"); 0.11.0 was released on
> 2026-11-14 (#808 sync of 2026-11-15). Every open item
> points at an open issue; an idea without an issue is not planned (file
> the issue first). Shipped detail lives in `CHANGELOG.md`; the code is
> authoritative over this page.

## Identity recap (PRD v1.1)

**tole = a durable conversational harness** — a personal-assistant chatbot
that can call tools, with persistent resumable sessions. Light coding via
tools (files + git + gh). Optional first-class integrations: uteke (memory,
documents), cora-code (code intel). Dynamic tools via a generic
approval-gated `run_command` — no config DSL.

## Completed (historical, do not re-plan)

E1 storage · E2 state machine · E3 mock provider · E4 cora search + real
provider · E4.5 native tool calling · E5 crash-resume · E6 approval gates ·
E7 tool suite · E8 MVP gate (real issue fixed, retro in `docs/e8-retro.md`)
· E10 loop guard · E11 secret redaction · E12 file CRUD (hash-anchored
edit) · gh read ops · TOLE_* env canonical.

---

## Track A — Coding-lite completion (small, closes old promises) — ✅ DONE

### A1 — Scoped pre-authorization — ✅ DONE (v0.3.0)
- **What:** `--allow <risk>:<tool>` (repeatable), e.g. `--allow write:edit_file`.
  Pre-authorizes specific Write tools for headless/CI runs.
- **Hard rule:** Destructive is **never** allowlistable — structural
  enforcement stays (registry + AllowlistApprover deny on sight).
- **Acceptance:** headless run completes with edit_file pre-authorized
  while other Write tools still deny; Destructive attempt with allow flags
  still denied. Unit tests for the flag parser + approver wiring.
- **Est:** S

### A2 — Git tool (light) — ✅ DONE (v0.3.0, `git.rs` registered in the CLI host)
- **What:** `git` builtin: `status`, `diff`, `add`, `commit` ops, argv
  whitelist discipline identical to gh.rs (validated per-op args, no
  free-form flags). Push stays manual.
- **Why:** closes the last maintainer intervention in the coding-lite loop
  (branch/commit). Also the natural companion for the markdown→uteke
  document workflow (write_file → git commit → uteke_document).
- **Acceptance:** agent commits a change end-to-end in a sandbox repo;
  injection attempts (`--upload-pack`, leading-dash messages) rejected;
  fake-git e2e test (same pattern as fake-gh).
- **Est:** M

---

## Track B — Chat-first (the product direction) — ✅ DONE

### B1 — Chat mode (`tole chat`) — keystone — ✅ DONE (v0.3.0)
- **What:** multi-turn REPL: prompt → agentic turn (durable, exactly like
  `run`) → answer → next prompt. `--resume <id>` / `--last` to continue.
  Explicit resume (no implicit auto-continue) — deliberate state
  transitions. Clean EOF/`/exit`, Ctrl-C safe (resume-able mid-turn).
- **Acceptance:** multi-turn conversation in one session; SIGKILL mid-chat
  → `tole chat --resume <id>` continues with full context; `/exit` leaves a
  cleanly closed session.
- **Est:** M

### B2 — System prompt & agent identity — ✅ DONE (v0.3.0)
- **What:** resolution `--system` flag > `TOLE_SYSTEM_PROMPT` env >
  default. Persisted in the session header (once, not per-turn — token
  efficient, replay-accurate). Analysis note: persona must stay stable
  across resume; sessions recorded pre-B2 (no system prompt) must remain
  replayable (backward-compat test).
- **Acceptance:** persona consistent across turns and resume; old session
  replay still green.
- **Est:** S–M

### B3 — Session UX — ✅ DONE (v0.3.0, as `tole sessions` + `tole status <id>`)
- **What:** `tole sessions list` (id, age, turn count, last-message
  preview, usage) and `tole sessions show <id>` (timeline view). Branching
  shipped later as forks (#210).
- **Acceptance:** list + inspect without opening JSONL by hand.
- **Est:** S

### B4 — Dynamic tools: generic `run_command` + startup probing + uteke first-class — ✅ DONE (v0.3.0)
- **What:**
  1. **`run_command` builtin**: input `{ "command": "..." }` → shlex-parsed
     to argv (no shell), cwd-jail, subprocess timeout + output cap. Risk
     ceiling = Write (Destructive unreachable). Approval prompt shows the
     exact argv; headless via A1 pre-auth.
  2. **Startup probing**: builtins whose binary is missing (gh/uteke/cora)
     are skipped with one warning line — no phantom tools burning turns.
  3. **uteke first-class**: `uteke_document` (save markdown into a room),
     plus existing recall. Renames `uteke_search` → `uteke_recall`
     (name/behavior consistency).
- **Why:** no TOML DSL (rejected — poor UX); any host CLI becomes usable
  with zero recompilation; uteke is the primary assistant integration.
- **Acceptance:** agent uses run_command to invoke a non-builtin binary
  (e.g. `figlet`) approval-gated; missing-binary builtin skipped with
  warning; uteke_document saves a markdown doc into a room (validated
  against the uteke CLI); rename with spec + tests updated.
- **Est:** M–L

---

## Track C — ✅ DONE (E9, closed 2026-09-28)

E9 hardening/OSS-prep (issue #9) complete: cross-build CI (#82),
timeout/truncation fuzz-lite + panic-free corpus (perf PR), perf
thresholds + docs/perf.md, LICENSE/README/SECURITY public-ready (#79,
#91). MCP client shipped early (#74). Session branching stays deferred.

---

## Track D — Integration surfaces (approved 2026-09-18)

tole-core gains three hosts beyond the CLI, in build order (each reuses the
previous layer):

### D1 — `tole mcp`: MCP server over stdio (issue #94) — ✅ DONE (v0.4.0)
Expose the registry's hardened tools via rmcp's server side. ReadOnly tools
always callable; Write needs `--allow`; Destructive structurally absent.
Est: S–M.

### D2 — `tole acp`: Agent Client Protocol host (issue #95) — ✅ DONE (2026-09-18)
Editor integration (Zed et al.): ACP sessions ↔ JSONL sessions, ACP
permission requests ↔ the approval gate, streaming turn events. Est: M.

### D3 — `tole-serve`: server mode (issues #96, #137) — ✅ DONE (2026-09-28)
Long-running daemon (uteke-serve pattern): thin REST (v1, #96) +
**multi-session MCP over Streamable HTTP** (`--transport mcp`, #137) —
one authenticated connection, N durable sessions, per-session jail
routing via `session_id`. Est: M–L.

---

## Post-D3 — shipped on develop (synced 2026-11-15)

Everything below is merged; CHANGELOG entries carry the detail.

- **v0.4.0:** harness memory loop (`--memory uteke` / `TOLE_MEMORY`),
  cora MCP auto-preset (`--no-auto-mcp` opt-out), write_file wire-schema
  fix, 2026-09-18 CodeCora scan triage fixes, serve hardening Wave 2
  (#136: connection cap, auth rate-limit, IO timeouts).
- **v0.5.0 (2026-10-02):** `verify_package` (#144, the slopsquatting
  defense), turn-end stop gates `--on-turnend` (#145), memory-loop
  decision typing (#143), multi-session MCP-over-HTTP (#137/#138),
  ACP scan minors (#156).
- **Post-0.5.0:** `--trust` presets (#160), SKILL.md support
  (`--skill` + discovery + `load_skill`, #161/#162).
- **2026-10-05 sweep fixes (#163/#164):** loud refusals for
  client-session-only flags on the server faces; `--sessions-dir`
  honored by serve/acp; `--workspace` honored by `serve --transport
  mcp`; `--on-turnend` wired into serve/acp; the #138 ambiguity
  refusal implemented; skills discovery defaults to cwd.
- **2026-10-05 activation fix (#165/#166):** `TOLE_TRUST` env actually
  read (was documented but unimplemented).
- **0.6.0 train (merged to develop 2026-10-05/06):**
  `mcp-http` becomes a default `tole-cli` feature (#168 — every
  documented face works on a plain install); the `gitea` tool (#169);
  the v0.5.0 Tier-2 baseline + HOME-scrubbing eval archiver (#170);
  `systemone_decide` typed decisions (#172/#173); depth-1 child
  agents `agent_start`/`agent_poll` (#171/#174); the no-mcp build
  gate fix (#175/#180); ACP native model picker + per-session
  approval controls (#176/#177/#179); ACP `session/cancel` (#178/#184);
  project-sync board-sync fixes (#181/#182, #183); REST
  `POST /sessions/{id}/cancel` + the `tole_session_cancel` MCP tool
  (#178/#185) — cooperative cancellation now exists on all three
  server faces (ACP, REST, MCP), spec-conformant (the receiver may
  ignore un-cancellable work; single-tool calls are not interrupted).
- **0.7.0 Mission MVP (merged to develop 2026-10-06; identity
  extended owner-approved — chat-first personal assistant WITH a
  mission mode for autonomous work):** durable task-list tools
  `todo_write`/`todo_read` (#198/#205); `tole mission` budgeted
  turn-chaining with `--max-steps/--max-minutes/--max-tokens/--verify`
  and durable mission summaries (#199/#206); budget tiers + cost
  report (#201/#207); ACP intra-turn visibility + true text streaming
  (#196/#203/#204); ACP model picker auto-advertised from the
  provider `/models` endpoint (#195/#197); remote approvals via the
  serve face — queue, decisions, expiry, CLI consumer (#200/#214);
  run ergonomics `--prompt-file/--name/--timeout` (#216/#219);
  read-only `web_fetch`/`web_search` SSRF-guarded (#215/#221,
  three CodeCora review rounds); startup update notification +
  `tole upgrade` (#220/#222, fleet-parity with uteke/cora); the
  mobile-control guide (#202/#218) defines the 0.9.0 surface.
- **0.7.1 Hardening patch (released 2026-10-08):**
  pre-tag rescan MAJORs #226–#235 and rescan-2 #276–#287 fixed (env
  scrubbing, MCP approval-prompt sanitizing, redirect/NUL/argv hardening,
  mailbox consume-once, perf-gate and tmpdir test fixes); follow-ups
  #293 (no hardcoded `gh` repo), #294 (plan mode in the MCP server
  registry), #295 (one session-id rule), #299 (ACP session cap), #300
  (`agent_poll` is Write); one crate-internal tool-call authorization
  gate used by `drive`, `resume_turn` and the MCP server path (#303,
  PRs #304/#309/#312). Behavior changes: `AllowlistApprover::new(_, Deny|Ask)`
  ignores patterns (#283), `agent_poll` is Write (#300), embedder
  registries with pre-hooks are enforced on the MCP path (#303).
- **0.7.2 Maintenance patch (released 2026-10-08):**
  scan backlog closed — systemone error bodies surfaced (#328), `write_file`
  and `git status/diff` limitations documented accurately (#329), the bare
  `--no-default-features` tole-cli profile compiles and is CI-checked (#330);
  Dependabot PRs no longer blocked by naming/description/CLA checks (#322 plus
  a ruleset context change); dependency bumps (rmcp 3.5.0, tokio 1.53.2,
  libc 0.2.190, uuid 1.27.0). No behavior changes.
- **0.8.0 Project config `.tole/config.toml` (#208) (released 2026-10-09):** merged to develop 2026-10-09 (parts #342/#343/#345/#346). Strict TOML schema,
  content-bound trust store outside the repo, fail-closed gate on the protocol
  faces, `tole config check|trust|untrust`, precedence flag > env > config >
  default for every key (lists replaced wholesale, booleans only turned on by
  flags), and refusal of config-sourced hooks/skills/MCP servers on the faces
  that refuse the flags.
- **0.8.1 Token-efficiency patch (#211) (released 2026-10-09):** usage and per-step request-size report in
  `tole status`/serve status (#356), `read_file` 20,000-char cap with `offset`/`limit` continuation (#357),
  wire-size reference missions with a CI bound (#358), two scan-found fixes (#360); scan backlog in #361.
  Deferred: lazy tool registration, history trimming, non-cached budget accounting.
- **0.8.2 Hardening patch (never tagged; shipped in 0.9.0):** `tole serve` HTTP framing,
  slow-client limits and a constant-time token check (#361/#364), cancel during replay
  approval settles `cancelled` (#365), scan follow-ups (#366/#369), rustls advisories
  (#376), MCP transport parity with REST — failed-auth limiter, body ceiling, resilient
  accept (#400).
- **0.9.0 (2026-10-28)** — everything below, plus the 0.8.2 patch above.
  Feature set (each issue stays open until its live E2E against a real provider has run):
  - `tole doctor` — offline/catalog/full tiers and a coverage ledger (#378: #388/#390/#392,
    follow-ups #401).
  - Per-user-turn token stats in `tole status` and the status JSON (#379: #389).
  - Soft interrupts — steer a running turn at its safe point, serve/MCP entry points
    (#377: #391/#393).
  - Memory injection hygiene — bounded, deduped, all-or-nothing recall (#384: #387).
  - Sibling change notices between child agents (#380: #394/#395, #401).
  - Typed child-agent handoff, roles and the verify gate (#383: #396/#397/#398;
    design in `docs/child-agents-handoff.md`).
  - Architecture audit refactors (#418, #420, #428, #441, #443), uteke/cora alignment
    (#422), provider failover chain (#209), `TOLE_SKILL` (#434), quiet startup (#435).
  - User config layer `$CODECORA_HOME/tole/config.toml` below the project config (#433);
    `web_search` method, auth headers and field mapping (#436); `--trust workspace`
    (#437); command-level allow/deny patterns for `run_command` (#438); global session
    index and `sessions --all`, `resume` names the owning project's command (#439);
    context compaction for `chat`, `resume` and `mission` (#440), its overflow recovery
    and the `doctor` missing-window warning (#481); session forks, `tole branch` and
    `sessions --tree` (#210); image input via `--image` (#212); Claude Code transcript
    import, `tole import-claude` (#381, import only).
- **0.10.0 (released 2026-11-04; 0.10.1 the same day):** `tole-tui`, a pi-style terminal UI — bare `tole` opens it; model,
  reasoning effort, context size and tokens in the footer (epic #464, issues #457–#463). Shipped: skeleton (#459) and the MVP (#460: streaming transcript, editor, footer, inline approvals, Esc cancel), the model picker, thinking level and slash-command menu (#461), and steering, the follow-up queue, collapsible output, `@file` and markdown/diff styling (#462), and #463's `/export` `/copy`, `/fork` `/tree` and the color themes (`default` / `light` / `mono`, `NO_COLOR`, user-config color overrides). The keybinding config (`[tui.keys]`, user config only) is the last #463 part, so #463 is done.
- **0.11.0 (released 2026-11-14):** the security and automation release; every BREAKING entry has a migration step (`docs/migrating-0.11.md`).
  - Secrets: `SecretSet` redaction at every sink, widened to more encodings (#595, #681, #682), live tool events redacted (#677), the redacted session log (#596), the shared secret-shape detector (#597), scoped release with no secrets for `run_command`, `job_start` and cron scripts by default (#598), more files count as sensitive (#683), non-dumpable processes (#675) and owner-only records refused under a session (#684).
  - Plugins: the plugin lock and rug-pull guard for MCP servers, bound to source, binary and secrets, with refused servers never started (#599, #679, #680), and `tole plugin list|diff|approve|revoke`.
  - Automation (epic #577): `tole cron` with time zones, parked and agent-created jobs, signed webhooks with `tole hook`, and outbound signed webhooks (#911, `[webhook_out]`, `tole webhook test`).
  - Collaboration (epics #583, #589): profiles, delegation with budget carve and `tole audit`, the advisor, model routing, skills (approved-hash loading, `skill_propose`, the curator) and multi-round discussion with `tole discuss` and the `discuss` tools. `agent_start` and `agent_poll` are deprecated aliases of `delegate` and `delegate_poll`.
  - Also: built-in `web_search` and `web_fetch` with the nginjen backend (#625), the Streamable HTTP MCP client (#530), the review gate before `git commit` (#534), `tole setup cora` (#535), the serve face for graphical clients (#524, #558), images on every turn (#559, #560) and the TUI polish of #463.
- **0.12.0 (released 2026-11-20; everything since 0.11.0):**
  - Epic #941 (one assembly for every face, still open for #949 only): steps 1 to 6 done (#942 to #948): server sessions get the default prompt, approved skills and routing (#945) and the owner's MCP client servers behind the plugin lock (#946), and the face-parity table and test are in (`docs/faces.md`).
  - Gateway (epic #910): the `turn_finished` outbound webhook event (#962, opt-in) joins the signed delivery of #911; the first decision of an approval is the only one that acts (BREAKING, `POST /approvals/{id}/decision`).
  - BREAKING: `verify` and `turnend` are sandboxed by default (#748), and the uteke home moves under `CODECORA_HOME`; each has an opt-out in `CHANGELOG.md` under `0.12.0`.
  - Plugins, #600 slice 1 (parts 1 to 6, merged): `plugin list` and `diff` show source, tier, secrets and sandbox state and start nothing the lock would refuse; `revoke` is owner-only and a hand-edited lock cannot revive a revoke; the description scanner; the one-shot probe for unapproved servers; `tole plugin add` with trust tiers (`first-party`, `third-party-verified`, `third-party-unverified`); `tole doctor` warnings for unpinned launchers, unsandboxed third-party servers and broad grants.
  - Plugins, #600 slice 2 (#811): bundle manifests (`tole-plugin.toml`, `tole plugin add --bundle`) and per-plugin secret grants approved one by one, withdrawn one by one (`tole plugin revoke --secret`), pinned to the manifest hash and recorded in the ledger.
  - Sandbox (#601, #748): opt-in bubblewrap sandbox for third-party stdio MCP servers (`[sandbox]`), and for `run_command` and `job_start` (`[sandbox.consumers]`, #748 parts 1 and 2: the built-in `command` profile and rlimits; the mission `--verify` command and the `on_turnend` stop gates as `verify` and `turnend`), with named profiles and per-server bindings (#810: `[sandbox.profiles]`, `[sandbox.servers]`, `tole sandbox list|show|set`, user layer only). Linux only; `tole doctor` says so elsewhere. A CI job runs the real-bubblewrap tests on every pull request (#751). Design: `docs/sandbox-design.md`.
  - Delegation approvals (#714, closed, six parts): the durable `approvals.jsonl` queue, a delegated child that parks at an approval and is denied on timeout, `tole delegation approvals`, `/approvals` decisions on `tole serve`, `delegate_pending` and `delegate_decide` for agent levels, and the guide `docs/approving-delegated-work.md`. Off by default (`[delegation] approvals = "off"`; `owner` and `chain` opt in).
  - Discussion: configurable provider timeout (#786), one retry after a timeout (#756), a timed-out call is charged its reserved estimate (#804).
  - Module restructuring (#752, closed, parts 0 to 20): see "Layering and published crates" below.
  - Tests and CI: `TestDir` and `CARGO_TARGET_TMPDIR` stop the suites leaking temp directories (#781, parts 1 to 9 merged, the issue is still open), tests wait for conditions with the shared `wait_until` helper instead of fixed sleeps (#812, part 1; the CPU-limited nightly job is part 2), the `verify_package` mock server binds port 0 (#801), CI builds and tests develop after every merge (#762) and compiles Windows and macOS on every pull request (#729).
- **UI epic #523 (started 2026-11-10):** core seams first, then clients. #524 (serve face for graphical clients) ships in three PRs: the paginated transcript read `GET /sessions/{id}/entries` (done), `POST /sessions {"resume": "<id>"}` (done), and the SSE event stream `GET /sessions/{id}/events` with its own stream cap of 8 (done; #524 is complete). Then #525 (one session-host module, done as a module inside `tole-cli` rather than a new crate until a second consumer exists, sync API; part 1 moves tool registration out of `session_host.rs` (done), part 2 consolidates `TuiSession` with the server host as shared primitives, not a literal merge: session create + index registration (done), one turn runner (done), the register settings (done)), #526 (`tole-tui` split: UI-neutral input seam done, `TuiHost` collapsed to `footer`/`run_turn`/`apply` done, approval-as-state done; rendering split into a `render` module done), #527 GPUI desktop, #528 Flutter client, #529 web client. Images (#559, done): any chat/TUI-host turn can carry images (`/image` in `chat`, pending images on `TuiSession`), ACP accepts `image` content blocks and advertises `promptCapabilities.image`; no model-callable image tool by decision. Image input for graphical clients on the serve face (#558: upload, `images` on a prompt, recorded-image read, quotas) is done. The TUI's own paste and `/image` (#560, done): bracketed paste inserts a block as one edit, `/image` and a pasted image path/data URL attach to the next message; no OS clipboard images by decision. #463 (later TUI polish, in four parts; done): part 1 `/export [path]` (HTML/JSONL) and `/copy` (OSC 52, no native clipboard) is done; part 2 `/fork` and `/tree` (a UI over the #210 session forks; in-file branching stays open) is done; part 3 themes (`default` / `light` / `mono`, `NO_COLOR`, color overrides) is done; part 4 the keybinding config (`[tui.keys]` in the user config: ten rebindable convenience actions, the approval keys, Esc, Ctrl+C, Enter and typing are not rebindable, an invalid keymap falls back to the default with one note) is done. #463 is complete (#567 fork/tree, #568 export/copy, #569 themes, keybindings). Status 2026-11-15: #524, #525 and #526 are closed. The epic #523 stays open for #527 (GPUI desktop), #528 (Flutter), #529 (web, low priority, framework undecided), #561 (the shared UI/UX plan) and the serve items a graphical client needs: #572 (list past sessions), #573 (model, effort and context in the status), #574 (list and set models) and #575 (rename and compact). Local transport for the desktop (#1043, 2026-11-20): `tole serve --socket <path>` (Unix socket in a `0700` directory, macOS and Linux; the Windows named pipe is a follow-up), `--token-fd`, and `protocol`/`version` in `GET /health` are in; attaching to a running serve from a second client is the client side. Graceful shutdown (#1014): `tole serve` (REST and MCP) handles SIGTERM and SIGINT, detaches running turns, stops the cron ticker and webhook senders, releases the socket and its lock and exits 0 within `--shutdown-grace` (default 20 s); not yet confirmed in a real container. The desktop was un-parked on 2026-11-20 (issues #1028 to #1044, milestone 0.10.0); mobile and web stay parked behind it, see "Parked and owner decisions". Of that plan, #1041 (the plain-language action summary in the shared approval row, needed by #1032 and #528) is built: `tole_core::action_summary`, in `GET /approvals`, the `approval_required` stop and the TUI prompt, see [docs/mobile-control.md](mobile-control.md) (Approval summary); the plain-terminal stdio prompt shows it too (`does:` line); the ACP title, the delegated-children queue and the webhook events still wait, see [docs/faces.md](faces.md).
- **Open backlog:** nothing from the 0.10 era is left: #210 (forks), #213 (MCP prompts and resources), #217 (detach) and #381 (Claude Code import) are closed. Ideas that never got an issue (in-file session branching, attaching to a *running* Claude Code process, the multi-client daemon of `docs/daemon-design.md`, #382 closed as a note) are not planned; each needs an issue with a user story first. The open work is in Track E and in "Parked and owner decisions".

## Track E — Agentic automation (approved 2026-11-11)

Owner discussion 2026-11-11 (competitor audit of OpenClaw, Hermes, Grok
Bot and Nakama; BMAD for collaboration). Design notes:
`docs/automation-design.md`, `docs/profiles-collaboration-secrets.md`,
`docs/skills-registry-design.md`, `docs/secrets-integrations-design.md`,
`docs/third-party-plugins-design.md`. 1 issue = 1 branch = 1 PR.

- **E1 — Automation, epic #577 (closed; #578 to #582 all done):** ledger core (#578), `tole cron` (#579),
  time zones (#580), unattended approvals (#581), webhooks on `tole serve`
  (#582). The scheduler lives inside tole. Status: the ledger core is in
  `tole-core` (`cron`, `ledger`: cron/`@every` parser, append-only
  claim/fold/recovery, catch-up with `missed` records, crash-injection and
  property tests; per-job time zones since #580). #579 is done: the file adapter and
  `tole cron create|list|pause|resume|remove|runs|tick` with script mode
  landed, then agent mode (session per run, model pin, step/minute
  budgets), the `tole serve --cron` ticker and approvals parking, then
  delivery targets (`log`, `session`, `file:`, trust-gated `command:`,
  `GET /cron/runs`), `run`/`cancel`/`edit`/`status`, the token budget and
  `cron.daily_token_cap`, and the `doctor` ticker check; the closing PR adds
  `--skill` and `--profile` on `create`/`edit` and `cron.max_concurrent`.
  #581: part 1 (an `/approvals` allow resumes the parked run, #638) is
  done; part 2 adds agent-created jobs. An agent creates a job only
  through the `cron_create` tool: the job is saved paused with
  `created_by` and `creator_allow`, within the creator's allow list.
  `tole cron resume` is the human enable. A job not saved through the CLI
  runs only enabled, within its creator's grant and never as a script, and
  an agent run whose workdir holds the cron dir is blocked.
  #580 is done in two parts: the `tz` zone layer (#641), then per-job time
  zones. `tole cron create|edit --timezone`, `cron.timezone`, and
  `cron.tz_source` (`system` zoneinfo at run time, or `offset-only`). DST
  gaps and overlaps fire once, `list` shows local time and UTC, and jobs
  without a zone are unchanged. The `bundled` source is not built.
  #582 is done in three parts: the signature and replay gate for
  `POST /hooks/<route>` on `tole serve` (v2 signatures since #678), filters
  and templates that run before any model call, then a delivery that fires
  its route's cron job, and `tole hook add|list|remove|test`.
- **E2 — Profiles, delegation, discussion, routing, epic #583:**
  - profiles (#584; config, `--profile`, tiers, room and persona done in
    the PR for #584; `tole cron create --profile` in the #579 closing PR);
  - delegation with budget carve, depth 2 (max 3), write lease and a
    mandatory report (#585; the `tole_core::delegation` tree with all the
    caps is in, the `[delegation]` limits, the delegated-child launch
    contract and the `delegate` tool with profiles, carved mission budgets,
    leases, grants and measured usage, and the completion report
    (`delegate_report`), and `tole audit`; done);
  - model routing with RuleRouter plus LayaRouter, on/off by the
    `SYSTEMONE_*` env (#586; the `Router` seam and the RuleRouter
    baseline, `[router]` in the user config, `tole route` and the doctor
    lines are in, and new runs, missions, delegated children and cron runs
    are routed once when `[router] enabled = true`. LayaRouter waits for
    the eval gate);
  - advisor (#587; the core seam, `advisor_max_uses`, the doctor line and
    `consult_advisor` in missions, delegated children and cron runs are in;
    done);
  - multi-round discussion with a sanitizer (#588; part 1a is the pure core
    in `tole-core`: contract, sanitizer, digest validator, checker verdicts,
    gate and the fixed prompts; part 1b is the append-only resumable
    ledger; part 2 adds budget hard stops and caps to the ledger, the
    built-in scribe and claim-checker roles and the host driver as a
    library, with independent read-only participants through `delegate`,
    uteke round documents (wired on every entry point by #717: the goal
    and one digest per round are written when uteke is available, and one
    note says they are off when it is not) and `tole audit --discussions`;
    part 3a is the
    owner-only `tole discuss` command; part 3b is the model-facing
    `discuss`, `discuss_status` and `discuss_stop` under the rules of
    `delegate` (the owner chose both entry points, see `automation-design.md`
    "Entry point"); independent child participants for agents (#704) are in
    as read-only reviewers that stay read-only (#1039 lets the owner give them named connected tools, one approval per tool per discussion, with `tole discuss start --tool`); the owner's approval surface
    for delegated children is #714, built in six parts behind
    `[delegation] approvals` (default `off`), see
    `delegation-approvals-design.md` and `approving-delegated-work.md`);
    #714 is done and closed, so every phase of the epic is done; the epic
    issue #583 is still open with its #714 box unchecked and needs closing;
  - `uteke_document` parent (#576; done).
- **E3 — Skills, epic #589 (closed; #590 to #593 all done):** `SkillIndex` + uteke doc adapter, K = 5 with
  pinned core (#590), approved-hash loading (#591), propose-only growth with
  meaning dedup (#592), curator on by default (#593). Status: #590's seam,
  adapters, selection, `skill_search` and `tole skills` are in, opt-in by
  `TOLE_SKILL_INDEX=uteke`; config and profile keys follow #584. #592 in
  progress: part 1 is the `skill_draft` gate and draft store in `tole-core`
  (lint, secret and injection scan, taint, provenance, dedup bands); part 2
  is the `skill_propose` tool (host-stamped taint, session, model and dedup)
  and `tole skills drafts list|show|reject|verdict`; part 3 is `promote`
  (exact bytes, the owner's confirmation, tainted and near-duplicate
  drafts gated) and `revoke` (what grew from a poisoned source). #592 is
  done. #593 is done: part 1 is the `skill_curator` core in `tole-core`
  (usage ledger, derived lifecycle, underperformers, package-aware archive
  and restore with a mutation ledger); part 2 is `tole curator`
  (run/status/pause/restore/pin/lock/adopt) and the recall and load
  recording; part 3a is the `[curator]` config table, `tole curator
  schedule` (a weekly script job) and a doctor line; part 3b records run
  outcomes (success or failure of `run` and `mission`, per loaded skill and
  as the skill-less baseline). LLM-judged merge proposals and chat, serve
  and cron outcomes are not planned (no open issue).
- **E4 — Secrets and third-party plugins, epic #594:**
  - `SecretSet` at every sink (#595; module, provider body, child minimum
    env, MCP arguments, web, `systemone`, memory loop and `uteke_document`
    done, widened in #681 and #682; live tool events in #677; export,
    handoff, digests and skill drafts reuse it as they land);
  - redacted session log (#596; done);
  - shared ruleset with uteke (#597; done: `uteke-privacy` 0.24.1 wired into `SecretSet::redact` after the known-value pass, same sinks, `[REDACTED:<kind>]`, `redaction_allowlist` for false positives; the uteke-side issue codecoradev/uteke#1732 is closed);
  - scoped release, with no secrets for `run_command` (#598, breaking; done,
    with sensitive files and `read_file_raw`, #676 for verify and stop gates,
    #683 for the wider file list);
  - plugin lock and rug-pull guard (#599; done, with `tole plugin
    list|diff|approve|revoke`, the source and binary binding of #679 and the
    unstarted refused servers of #680);
  - owner-only records and the tole binary refused under a session (#684;
    done) and the non-dumpable process guard (#675; done);
  - `tole plugin` commands, scanner, trust tiers and grants (#600; open). Slice 1
    is done (list and diff, owner-only revoke, the description scanner, the
    one-shot probe, `tole plugin add`, trust tiers, doctor warnings). Slice 2
    (#811, bundle manifests and per-plugin secret grants) is done. Slice 3a
    is done: the skills pin (stated source, append-only skill ledger, refusal of
    a drifted or ledger-revoked skill) and, in #879, the ledger checkpoint in the approvals
    file with `tole skills adopt|recheckpoint`; `tole plugin audit` (slice 3b) is done too. MCP
    Registry namespace verification is skipped (owner decision); fetching skills
    from the uteke registry is a later slice with no issue yet;
  - external sandbox wrapper and lazy start (#601; open; design and owner
    decisions in `docs/sandbox-design.md`; first slice done: opt-in
    bubblewrap sandbox for third-party stdio MCP servers on Linux, with a
    doctor line; opt-in `[sandbox.consumers]` for `run_command` and
    `job_start` with rlimits, #748 first slice done; `verify` and `turnend`,
    second slice done; cron scripts and `command:` deliveries, third slice
    done; the default flip of `verify` and `turnend` to `optional`, owner
    decision 2026-11-16, is on `develop` for the next minor, `docs/sandbox-design.md`
    §9.4). Open: the `git` tool hardening
    (#873); the #748 cgroup limits are evaluated and deferred (`docs/sandbox-design.md`
    §12.1), and #750 for the operability
    pieces: the doctor listing of sensitive grants and startup logging (#809),
    per-server and named profiles with the CLI (#810, done), the profile
    name and hash in the plugin lock entry with re-approval on change (#750,
    done), the profile hash in the startup line, the `sandbox_fallback`
    ledger line of an `optional` fallback, `TOLE_VERBOSE=1` launch logging and
    the wider credential list of the doctor listing (#750, done), and lazy start
    with tool retrieval by meaning (no slice issue yet));

Order that was followed: E4 #595 first (other epics reuse it), then E1,
E2 and E3. Remaining order: #809 and #810 (under #750),
and the rest of #748 (#597 is done).
Cross-repo in uteke OSS: document similarity (codecoradev/uteke#1730), personal
skill library (codecoradev/uteke#1731), shared secret-detector crate
(codecoradev/uteke#1732, closed).

## Open work by area (2026-11-15)

| Area | Open issues |
|------|-------------|
| Secrets and plugins (epic #594) | #600 plugin commands (slice 2: #811, slice 3a skills pin and 3b audit: done), #601 sandbox (#748 commands, #750 operability: #809, #810) |
| Security follow-ups | #813 (decisions left from the #714 and #600 reviews) |
| Collaboration (epic #583) | none; the epic issue only needs closing |
| Serve for graphical clients | none open: #572, #573, #574 and #575 are done (#571 moved to the gateway track) |
| Gateway (epic #910) | G0a #571 (`approval_required` stop reason, serve protocol only; done), G0b outbound signed webhook delivery (#911, done), `turn_finished` event (#962, done: merged in 0.12.0, the owner confirmed D1-D10 as the defaults on 2026-11-19), G1 to G5 in the epic |
| UI stack (epic #523) | **Renderer decision (owner, 2026-11-25): the desktop views are re-done in React on GPUI (GPUIX) with Tailwind and the Rust core stays; epic #1241, validation gates #1240, children #1242 to #1248; the Rust GPUI renderer is kept until the M2 parity gate and M3 (#1247); none of it is built yet, so what follows describes the Rust renderer.** #527 desktop (un-parked 2026-11-20, planned as #1028 to #1044, milestone 0.10.0, see [docs/desktop-design.md](desktop-design.md) section 0; #1028 part 1 is in: the `crates/tole-desktop-model` (view state, tokens, string table; no toolkit) and `crates/tole-desktop` (ui, binary) crates (the app shell window behind the off-by-default `gpui` feature, the `scripts/desktop-check.sh` guardrails and the `Desktop` workflow); the offscreen render harness is in too: `scripts/desktop-render.sh` writes one PNG per screen and state from a temporary copy that adds GPUI's `test-support`, so the committed graph does not have it, and Tab and Shift+Tab move focus through the shell; still open on #1028: the check of the window on Wayland, macOS and Windows; #1029 part 1 is in: the supervisor of the local serve in `tole-desktop` and the connections model in `tole-desktop-model`, with no page or setting using them yet; the approvals queue model is in (#1032 part 1, `crates/tole-desktop-model/src/approvals`: the Waiting and Decided lists, the 900 s countdown, the sidebar badge that reads Unknown and never a zero when the list is not known, and the message-box rule; its recovery items are #1116: refusals and fetches expire on `Tick`, a reply that is not the current fetch adds no waiting request while a fetch runs; the Approvals page is built on it (#1032 part 2, `crates/tole-desktop/src/ui/approvals`: the card as a reusable component, the Waiting and Decided tabs with their empty and unknown states, and an answer sent once through the approvals driver; the card inside the task view and the link back to the task are not built)); the adapter from the typed client to the approvals, skills, tools and automations models is in too (`crates/tole-desktop/src/adapter`: pure functions, no I/O and no toolkit), and so are the first two drivers (`crates/tole-desktop/src/driver`: the `Api` trait over the typed client and a `Clock`; the approvals driver, a worker with its stream, fetch and decision threads that keeps the approvals queue true to a serve, retries with a backoff and answers once; and the task driver, which reads one session's history, follows its event stream, sends a prompt, a steer note or a Stop and answers that task's approvals, and polls the status after an allow, a 409 or a Stop so a task is never left busy; the task view of #1031 uses the task driver and the Approvals page uses the approvals driver), and the skills driver (`driver/skills`: the Skills and Connected tools pages loaded from `GET /skills` and `GET /tools` on a worker with one loader per route, refreshed on a slow timer and on demand, retried with a backoff, and rows taken back when a load fails, is partial, is older than twice the refresh or the machine slept; read-only; the Skills and Connected tools pages (`ui/skills`, #1034) draw it: the Skills tab with its review, approved, not used and archived groups, the Connected tools tab with the trust state of each server, the not-read, reading, failed and partial states, and Refresh; no button approves or trusts anything), the automations driver (`driver/automations`: the Automations page, #1033, loaded from `GET /cron/jobs` and, for the opened automation, `GET /cron/runs` (with the pending approval of a run that stopped to ask), on a worker with a loader per route, refreshed and retried like the skills driver, rows marked failed when stale; `Handle::set_enabled` sends one pause or resume per press, the row changes only when the serve answers, a refused switch is a notice, and a list read that raced the switch is dropped; the Automations page (`ui/automations`, window section) uses it: the table with its on/off switch, the count in the top bar only for a list that was read, the notices and the scheduler note, and a press on a row, a switch or the dismiss button sent by id; still open on #1033: the history of the selected automation, the link to Approvals, and the render scenes; the notifications card has no serve route yet), and the backend (`crates/tole-desktop/src/backend`: the link to the supervisor's serve as a trait, and a worker that starts the approvals, skills and automations drivers over one `Api` while the serve is ready, stops them when it stops being ready or is replaced (the view reads Unknown at once, never a zero), starts new ones over a new `Api` when it is ready again, with at most one attach a second and a backoff after a failed one, keeps the tasks the window opens (at most three, by session id, kept while the serve is down and started again after a restart; the approvals driver follows exactly those sessions) with one task driver each, passes the task commands by session id, and gives the window one plain-data `BackendView`, with a pure function (`backend::shell::apply`) that applies it to the model's shell state: the connection dot from the serve state, the approvals badge and the skills count as they are (Unknown stays Unknown: the shell's `skills_new` is a `Badge`, written as Unknown unless both the skills and the tools are known); the app module (`crates/tole-desktop/src/app`, no toolkit) boots it: the supervisor's `Config` for the folder the app was started in and the runtime directory of the user, the launch on a thread of its own with the backend following it from the first moment (a launch that fails or panics is a `Failed` state with a retry, not a panic), one change signal for the window, the buttons off the window's thread, and a quit that bounds every wait (a launch still running is let go); the window uses it: `ui::run` takes the initial shell state and the app, applies `backend::shell::apply` to its shell state on every change the app signals (looked for every 50 ms on the window's thread), shows the unknown mark for a count that is not known (the skills and automations counts included), and shows a line with the serve's state and the button the failure calls for while the serve is not ready; `tole-desktop` boots it in the folder it is run from (`cargo run -p tole-desktop --features gpui`), and with no display it says so and exits 1 without starting a serve; it also refuses `/` and the home folder, which a file manager or launcher starts an app in, with a notice and no serve; known limits: the window shows the folder it was started in while an already running serve of another folder is reused, because attaching does not compare workspaces (D8 is open), and the ordered stop of the backend on close (`Rc::try_unwrap` in `main.rs`) is skipped silently when the toolkit still holds its handle to the app, so the process just ends and the serve keeps running); #1031 part 1 is in: the task view of #1031 is the first page to use a driver: the "New task" button of the home page makes a session in the selected folder (`Backend::new_task`, one call on a short-lived thread, refused at once, asking the serve nothing, when three tasks are open, when the serve is not ready or while the last one is still being started (one call at a time, so two clicks make one session), and each of those, and a serve that could not make the session, is told on the home page in its own words) and opens it as a task, up to three, as tabs; the main area of an open task shows the transcript from the driver's window of rows (the owner's words as a bubble, Tole's answer as Markdown that never opens a link, a "Working" group that folds with its technical details one click further, a plan, a thinking and a writing mark; a long task builds only the rows near the screen, the list reports the heights it measured back, keeps a running task at its end until the owner scrolls up and keeps the text being read in place, with "Jump to latest"), the status row (the sentence and its dot from the model, the "Look only" chip (always off for a task this window starts, which is created without plan mode; reading it from a session opened elsewhere is not built), Stop while a turn can be stopped) and the composer (a box that grows; Enter sends, Shift+Enter breaks the line and Enter while an input method composes is the input method's, by the model's rules; a note while a turn runs, shown as the owner's words without the wrapper core adds for the model; the words come back into the box when the driver refuses the command or fails the send, never after the transcript showed the message, and stay in the box with a note when no driver can take them; closing a tab drops its unsent words); a section of the sidebar takes the main area back and the tasks stay open as tabs; still open on the task view: the right panel (Files, Changes, History, Usage), image attachments, the approval card in the task, opening a past task from the sidebar (the sessions grouped by folder are #1030), the scroll rate of a 2,000-entry session on a real laptop (the render harness recorded a median of 11 ms and a 95th percentile of 23 ms per frame for 2,000 rows in a debug build with software rendering), and the hand check of an input method on macOS and Linux; #1030 part 1 is in: the sidebar counts (approvals, automations on, discussions that need you, new skills and tools, each shown above zero), the header (sidebar toggle, breadcrumb, connection indicator) and the home page with the folder list and "New task", as model state first and then the view; still open on #1030: the sessions grouped by folder, the composer, "Pick up where you left off", "Coming up" and the starter cards, which need the typed client, and the New task shortcut; the pages and the approvals page being #1030 to #1032), #528 and #529 (mobile and web, parked behind the desktop), #561 (the shared UI/UX plan; the desktop draft in docs/design/desktop is its first output); #1044 (container image and TLS proxy for a remote serve): a Dockerfile, a compose example with a Caddy proxy, an nginx example and a streaming probe are in `docs/container.md` and `templates/container/`, written but not yet run end to end (no image is published, and the desktop's TLS client is not built) |
| Tests and CI | #781 (leaked temp dirs), #812 (wait_until helper and a 1-CPU job; covers flaky #702, #712, #738), #806 (develop red after a series of PRs), #705 and #670 (Cora Review in CI), #566 (iOS cross-compile job), #669 (cora code intelligence accuracy) |
| Docs and process | #807 (local Cora review), #808 (this page) |
| One assembly for every face (epic #941) | #942 to #944 done (prompt, skills, roster via `FacePolicy`, routing step, every turn through `turn_runner`); #947 done (fork, tree, compact, name, plan mode and mission over `serve`); #948 the parity table [docs/faces.md](faces.md) and its test (done); #945 done (default prompt, approved skills and model routing for server sessions; #971 routes a new session's model once, at its first prompt); #946 done (the owner's MCP client servers on `serve` and `acp` behind the plugin lock, so the review guidance is active when the cora review tools are configured); the epic stays open for step 7 only: #949 (desktop over the same serve API) has a design note, [docs/desktop-design.md](desktop-design.md), whose decisions D1 to D4 and D6 the owner answered on 2026-11-20 (section 0: the desktop is a client of a local `tole serve` over a socket; the UI stack is un-parked for the desktop, issues #1028 to #1044), D5 and D7 to D11 stay open |

## Layering and published crates

Published crates (`publish.yml` publishes them in this order, all at the workspace version, 0.12.0 today): `tole-core`, then `tole-tui`, then `tole-cli`. `tole-core` is the platform-agnostic library (embedders build it with `--no-default-features`); `tole-tui` is the inline terminal UI over `tole-core`; `tole-cli` is the `tole` binary and depends on both (`tui` is a default feature). The owner decision of 2026-11-14 (#752) is that the set does not change: no new crates, and `publish.yml`, `release.yml` and the crates.io names stay as they are. Real crate splits stay a future option if compile time or embedder size asks for it. Amended 2026-11-20 by the owner decision on #1040 (every graphical client talks to `tole serve`): a fourth workspace crate, `tole-client`, is the typed blocking client of the serve protocol (HTTP + SSE, no async runtime, no TLS yet). It has a method for every serve route the desktop uses (#1069: the first set, then interrupt, detach, fork, tree, compact, name, model, effort, images and missions, next to the #1042 discussion, cron, profile, skill and tool routes), an incremental SSE reader that resumes with `after=<seq>`, a contract test against a real serve, and a test (`crates/tole-cli/tests/client_routes.rs`) that fails on a serve route with no client method, or a client method with no serve route, unless the route is on its allowlist; it is not in `publish.yml` yet (the publish order is still open). Its wire and event types have a committed, CI-checked JSON Schema (`crates/tole-client/schema/tole-client.schema.json`, #1068), generated by a unit test with `schemars` as a dev-dependency only. A second transport, `UnixTransport` (macOS and Linux, for `tole serve --socket`), is in (#1067); the Windows named pipe is #1051.

Layering rules (#752, closed; details in `docs/architecture.md`):

- Each of `tole-core` and `tole-cli` groups its top-level modules into three levels: kernel (session model, storage, secrets, provider seam, ledger, approval, turn loop in the core; config and trust, profiles, routing and hooks in the CLI), engines (delegation, discussion, router, advisor, skills, web, MCP client, children and the shell and vcs tools in the core; delegation, discuss, skill, cron, plugin and the session host in the CLI) and faces (`mcp_server`, `update_check`; the diagnostics and hook commands, and the binary-only `acp`, `mcp_http`, `mission`, `serve`, `setup` and `upgrade`).
- An import may point down or sideways, never up. `crates/tole-core/tests/layering.rs` and `crates/tole-cli/tests/layering.rs` scan `crate::<module>` paths and fail on a lower level importing a higher one (`cargo test -p tole-core --test layering`, `cargo test -p tole-cli --test layering`).
- Known violations sit in a `BASELINE` list with a reason, and the list only shrinks: `tole-core` has four entries, `tole-cli` none. New violations are not allowed; fix the dependency instead of extending the baseline.
- A new top-level module must be added to one of the level lists. A moved module stays reachable at its old path through a `pub use` alias, so public paths do not change.
- Moves are pure (`git mv` plus `pub use`), one subsystem per pull request, and never touch a file an open PR is changing.

Architecture standard (epic #1249, CodeCora modular and thin-UI standard; the checks run in the `Check` job, the `Features alone` jobs of `ci.yml`, the `Desktop` workflow and the `Security` workflow):

- In: crate kinds and the crate dependency matrix (#1250), the size guard with an allow-list that only shrinks (#1251, rule D1), the `unsafe_code` deny with a named opt-in list (#1252, rule D4), the licence gate with `cargo-deny` (#1253, rule D2), and each Cargo feature built alone plus a `wasm32` check of the desktop model (#1254, rules M3 and L5). Details and the rules are in `docs/architecture.md`.
- Ratchets (files taken off the allow-list by pure moves): `crates/tole-cli/src/config/mod.rs` is done; `crates/tole-tui/src/lib.rs` is in review; `crates/tole-cli/src/main.rs` went from 4,999 to 2,196 lines in this change (13 modules, a third owner-approved one-time raise of +176 lines of glue in the `tole-cli` crate total); what remains is the 541-line `Command` enum and the 1,313-line `dispatch` fn, which need a non-pure follow-up (flatten the per-subcommand arg structs into `Command`, split the `dispatch` arms into functions), so the file stays on the allow-list; `crates/tole-cli/src/serve.rs` went from 3,367 to 1,112 lines in this change (16 new modules, a fourth owner-approved one-time raise of +132 lines of glue in the `tole-cli` crate total); what remains is the 685-line `route` match and the `client_routes` scan that reads it in that file, which need a non-pure follow-up, so the file stays on the allow-list; the large `tole-core` files are open and held on one owner decision (below).
- Owner decision open: a split adds module glue (docs, imports, `mod` and `pub use` lines) to a crate that is already over the 50,000 line cap, so it raises that crate's total in the allow-list (the first split added 84 lines for about 3,100 moved lines, about 3 percent). The owner approved two one-time raises on 2026-11-26, three crate-total changes in all: the `unsafe` opt-in lines (+16 in `tole-core`, +11 in `tole-cli`) and the `config/mod.rs` split (+84 in `tole-cli`), then a third one-time raise for the `main.rs` split (+176 in `tole-cli`); how later splits count is not decided.

## Parked and owner decisions

Parked means: an issue exists and is open, and nobody works on it until the owner decides.

| Item | Why it is parked |
|------|------------------|
| #741 `import-hermes` (session import and profile pack) | `[PLAN]`, same direction. Open question: does the import redact or keep text verbatim |
| #742 serve auth providers: OIDC (and scopes) | Basic is done (owner decisions of 2026-11-17: Basic first, single user with many devices, no session bound to an identity, TLS at a reverse proxy; `tole serve --auth basic`, `tole serve-hash`, `docs/mobile-control.md`). OIDC and the read-only / full scopes remain, parked until the owner picks the next slice |
| #528 Flutter, #529 web | Mobile and web wait behind the desktop (#527 was un-parked on 2026-11-20); the core seams #524 to #526 and #558 to #560 and the serve items #572 to #575 are done; #571 moved to the gateway track, epic #910). #529 is low priority; the desktop decision of 2026-11-25 (React on GPUIX, epic #1241) does not choose the web framework, and rendering the same React tree in a browser through GPUIX wasm is a parked option (M6) |
| #382 multi-client daemon | Closed as a design note (`docs/daemon-design.md`); no build without a user story |

Owner decisions still open:

- #813: decided 2026-11-15: `[delegation] approvals` stays `off` with a `tole doctor` hint when delegation is in use and the key is unset; `tole discuss start|resume` refuse under a session; an unreadable plugin ledger blocks `plugin add` and `plugin approve`; the user-config floor of the discussion timeout is 10 s (the env hook still goes down to 1 s).
- #748: decided 2026-11-16 and 2026-11-17: `verify` and `turnend` are sandboxed by default as `optional` in the next minor (never `required` by default: a sandboxed verify cannot see `~/.cargo`); the implicit default runs unsandboxed with a loud, ledgered warning only on a host with no sandbox backend, an explicit `optional` or `required` never does. Landed on `develop`; the release is the owner's. Whether a later release moves the default to `required` where a backend exists is not decided.
- #806: the rule that stops a series of merged pull requests from turning develop red. The CONTRIBUTING rule is documented; requiring up-to-date branches or a merge queue is a repository ruleset setting that only the owner can change.
- #705: Cora Review failing or running 12 minutes and more on larger PRs: cora-code 0.21 chunks the diff and adds `review.exclude` (cora-code#720); the exclude is configured in `.cora.yaml` (closed with that change).
- #741 (the open question in the table above) and #742 (Basic built, decided 2026-11-17: Basic first, single user with many devices, sessions not bound to an identity, TLS stays at a reverse proxy; OIDC and scopes remain). #740 was decided on 2026-11-16 (profile fixed per session process, request wins on resume and is recorded, `[profiles.*]` budget as is, no `memory_room` from a request, profile model else server default) and built as `profile` on `POST /sessions` (create and resume, `docs/mobile-control.md`); `tole sessions` shows each session's profile and `tole doctor` lists the serve profiles by name, so #740 is fully done.

---

## Execution order

```
A1 → A2 → B1 → B2 → B3 → B4 → Track C → D1 → D2 → D3 → Post-D3
                         ↑ all tracks done; see the Post-D3 section
```

A-track first: small, closes the coding-lite promises and unblocks headless
testing patterns reused by B4's run_command. B1 is the keystone; B2 touches
the session header (do it before B3/B4 so the format migrates once).
