# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Behavior change
- **Skill approval is required by default** (owner decision 2026-11-11, #591). Without `TOLE_SKILL_APPROVAL` set, a session loads a discovered skill (`<workspace>/skills/`, `~/.codecora/tole/skills/`) only when its whole directory hashes to the owner's approval; every other skill is listed by name only, as unavailable, and `load_skill` refuses it. A session with unapproved skills prints one startup note (`N skill(s) unavailable until approved (tole skills list; tole skills approve <name> --hash <h>)`), and `tole doctor` reports the same count, or warns when the CodeCora home is unresolvable (then nothing can ever be approved). Cron jobs that name an unapproved skill are `blocked` (`skill_not_approved`) with the exact `tole skills approve <name> --hash <h>` in the recorded detail. `--skill <file>` and a profile's `skills` stay ungated (the owner named the file). **Migration:** run `tole skills list`, review each skill, then `tole skills approve <name> --hash <hash>` (scripts must pass `--hash`); or set `TOLE_SKILL_APPROVAL=off` in the environment to restore the old behaviour. Only the environment can opt out; no config file can.

### Security
- Known-value secret redaction, part 1 of #595 (epic #594). New `tole-core` module `secrets` with `SecretSet`: it collects the values of every secret-named env var (the ENV-1 name rule, now `secrets::is_secret_name`, shared with the child-env scrub) plus explicitly named values (a provider key, URL userinfo passwords), and `redact` replaces each value and its base64 (standard and URL-safe), percent-encoded and JSON-escaped forms with `«NAME»`; overlapping matches merge, unmatched text is returned without a copy, and an empty set costs nothing. Values under 8 characters are not redacted and are reported by name only. The provider request body (system prompt, messages, tool arguments, tool results) and provider errors now redact every known secret of the process instead of only the provider key (E11): the marker changes from `<redacted>` to `«NAME»`, and a provider key shorter than 8 characters is no longer redacted. `tole-core` API: new `secrets` module, `OpenAiProvider::with_secrets`, `subprocess::scrub_env_for_child_keeping`, `child_env::PROVIDER_SECRETS`.
- **Behavior change:** child agents (`agent_start`) no longer inherit the parent's whole environment. They get the ENV-1-scrubbed environment plus only their provider keys (`TOLE_API_KEY`, `OPENAI_API_KEY`, `TOLE_FALLBACK_API_KEY`); non-secret settings (`TOLE_BASE_URL`, `TOLE_MODEL`, `TOLE_MEMORY*`, ...) and `GITHUB_TOKEN` stay. A child therefore can no longer use an integration whose secret lives only in the parent's environment (`TOLE_MCP_<NAME>_TOKEN`, `SYSTEMONE_API_KEY`, web backend header keys). The remaining egress sinks (MCP tool arguments, `web_search` / `web_fetch`, `systemone`, memory loop, `uteke_document`) follow in part 2.
- Known-value secret redaction, part 2 of #595: the same `SecretSet` now covers every egress sink tole has today. MCP tool arguments (every string and key, JSON structure preserved), the `web_search` query, the `systemone_decide` request and `uteke_document` markdown are redacted before they leave the process; the memory-loop session summary is redacted before its length cap (so a cap can never cut a secret into an unrecognizable prefix). A `web_fetch` URL that carries a known secret, raw or percent-encoded, is refused (direct fetch and fetch backend) instead of being sent. Values a `TOLE_WEB_SEARCH_HEADERS` / `TOLE_WEB_FETCH_HEADERS` header expands from `${NAME}` count as secrets whatever the variable is called, for every sink including the provider. `tole-core` API changes: `mcp::register_server_tools` and `memory::remember_session` take a `&SecretSet`; `web::WebFetchTool` is no longer a unit struct (use `WebFetchTool::default()`); new `with_secrets` on `McpTool`, `WebFetchTool`, `WebSearchTool`, `WebBackendFetchTool`, `SystemOneTool` and `UtekeDocumentTool`; new `SecretSet::redact_value` and `web::header_secrets`. The provider now redacts tool-call arguments and tool results structurally with `redact_value` (part 1 redacted their serialized text, which turned a secret that is a bare JSON number into invalid JSON; it now becomes the marker string).

### Fixed
- Skill review notes (#616, #617): `tole skills approve` now shows the package (files, sizes, hash) before it approves and asks for confirmation on a terminal; **without a terminal it refuses unless `--hash` is given, so scripts must pass `--hash <sha256:…>`** (the hash `tole skills list` shows). `tole skills index` redacts known secret values from each description before it reaches uteke (the content hash stays the real file's). A skill package walk counts directories too (at most 1024 entries), so a skill directory of countless empty subdirectories is refused quickly.
- A huge SKILL.md can no longer exhaust memory at session start: every SKILL.md read (`load_skill` in every mode, the discovery listing, `skill_search`, the approved-hash gate, `tole skills index`) goes through one capped reader (1 MiB; the length is checked before reading and the read stops one byte past the cap). An oversized skill is left out of the listings with a startup note and refused by `load_skill`. A FIFO named SKILL.md is refused instead of blocking. The skill approvals file is capped at 1 MiB too. Found while fixing the same pattern in #617.
- Reopening a session right after dropping its writer no longer fails with a spurious `Locked` naming our own pid (#552): an `flock` belongs to the open file description, so a child forked by another thread of the same process (a command, job or hook) briefly holds a duplicate of the lock descriptor until it execs, and a reopen in that window was refused. It showed as `409 ... open for writing by pid <our pid>` on `POST /sessions {"resume"}`, in the TUI `/resume` and `/new`, and as two flaky `tole-core` tests. `lock_writer` now retries for up to 150 ms, only when the recorded holder is our own pid; a holder with another pid still fails at once, and a genuine second open in this process (#475) still fails after the bounded wait. Regression test: 6000 create/drop/reopen rounds against three threads spawning children (8 to 12 failures before, 0 after).
- Missions now learn about cora's agent-as-reviewer flow (#536): `tole mission` builds its registry like `run` (including the auto-attached `mcp_cora_*` tools) but its pinned system prompt never named the review tools, so the unattended command was the one that did not know the flow existed. When both `mcp_cora_cora_review_prepare` and `mcp_cora_cora_review_finalize` are registered, a fresh mission's prompt now carries the same review section as `run` / `chat` / the TUI plus one sentence asking for a review (and its verdict in the final message) before the mission ends; without the tools nothing is added. Guidance only: a gate before `git commit` is tracked separately. `serve`, `acp` and `mcp` sessions are unchanged on purpose: they attach no external MCP servers, so they have no review tools. Resumed missions keep their pinned prompt.

### Added
- Delegation, part 4 of #585: the completion report. A delegated child gets the harness-owned `delegate_report` tool (added to its allow list like `handoff_submit`) and hands in ONE report (summary, files changed, diff ref, checks run, open risks; each field capped, control characters escaped, at most 64 KiB) to its agent dir, created new and never through a symlink. The parent reads it when it settles the child, on `delegate_poll` or, for a child whose process is gone, on the next delegate (no longer `interrupted`): through a plain agent dir only, size-capped before parsing, cleaned again, and records it in the delegation log itself. A complete report makes the run `done`; none, or an incomplete, oversized, symlinked or invalid one, leaves it `incomplete` with the reasons. `delegate_poll` shows the report fenced as untrusted data. Every file the parent reads from a child's agent dir (report, session files, and `agent_poll`'s pid, meta, log and handoff) is opened non-blocking without following a symlink and must be a regular file, so a FIFO planted there cannot freeze the parent.
- Delegation, part 3 of #585: the CLI faces (`run`, `chat`, `mission`) get the `delegate` / `delegate_poll` tools over `tole_core::delegation`. `delegate {task, profile?, mode?, budget?, allow?, role?, consumes?}` carves the child's budget from the session's node and starts it as `tole mission` with that carve as `--max-steps`/`--max-minutes`/`--max-tokens`, optionally `--profile`, with `--allow` = parent ∩ profile; the child keeps its provider keys plus the parent grant ∩ the profile's `grants`. An `execute` child takes a write lease on the canonicalized directory it works in, checked before it spawns. A child gets the tool again only while above the goal's depth. `delegate_poll` charges a settled child its measured session usage (its whole carve when unreadable); until the completion report lands, an `ok` child settles `incomplete`. Past `max_live_processes` spawns queue and start on a later delegate or poll, strictly in log order (a queued writer waits for its workspace lease; an exited, unreaped child frees its slot). The delegation log and its queue live in the tole data dir (`<codecora home>/tole/delegation/<workspace hash>/`), outside every workspace, and the fold ignores any spawn record that breaks a cap (parent, depth, allow, budget, consult rule). `agents::agent_dir` now refuses ids that are not one plain path component, and `child_env::delegation` refuses node ids with a leading, trailing or doubled dot. Known limit: the root session's own usage is not measured, so it sits outside the goal budget.
- Delegation, part 2 of #585: the launch contract and limits the `delegate` tool (next PR) builds on, inert until then. New user-config table `[delegation]`: `max_depth` (1–3, default 2), `max_live_processes` (default 8), `goal_budget`, `goal_steps`, `goal_minutes` (user layer only; a project config's is `IGNORED`, out-of-range values are config errors). The child launch contract carries a delegated child's real depth and its log and node id (`TOLE_DELEGATION_LOG`, `TOLE_DELEGATION_NODE`), and `agent_start` takes launch options (subcommand, depth, secret names to keep) for a delegating host. A delegated child takes its allow patterns and trust presets ONLY from its flags (the parent's parent ∩ profile); its profile's `allow`, the config's `allow`/`trust` and `TOLE_TRUST` are ignored. The ledger file store moved from `cron::store` to `ledger_file` (still reachable as `cron::store`) so non-cron hosts can use it.
- `tole cron` closing part (#579): agent jobs take `--skill` (repeatable; a skill name found from the workdir or a SKILL.md file) and `--profile NAME` on `create` and `edit`, both validated on save and again before every run (a skill that is gone or a profile no longer in the user config records the run `blocked`, no tokens spent). Saving pins each skill's content hash (job and profile skills); a run whose skill changed since is `blocked` (`skill_changed`) until `cron edit --skill`/`--profile` re-pins it, and under `TOLE_SKILL_APPROVAL=required` a skill named by name must be owner-approved (`skill_not_approved`). A profiled run gets the profile's persona and skills in its system prompt, its model (below the job's `--model` pin), its memory room (when `TOLE_MEMORY=uteke`), Writes auto-approved only when both the job's and the profile's allow lists cover them, and no secret grants. New user-config key `cron.max_concurrent` (1 to 64, default 4) for `tole cron tick` and the `tole serve --cron` ticker; `tole cron status` shows it.
- Delegation core, part 1 of #585 (epic #583): new `tole-core` module `delegation`, an append-only, auditable tree of one goal (the orchestrator at depth 0, delegated profiles below). A child's tokens and steps are carved from its parent's remaining budget (a running child keeps its whole carve reserved; a settled one is charged what its subtree spent) and its deadline never passes the parent's, so any number of children share one pot. Depth defaults to 2 and can never exceed 3. The child allowlist is parent ∩ profile plus every deny; a `consult` node cannot spawn an executor or write. An `execute` node takes a write lease per path (same path, ancestor or descendant conflict; the first in log order wins). `max_live_processes` only queues. An `ok` run settles `done` only with a completion report (summary; changed files need a diff ref and the checks run) and no unsettled children, else `incomplete`. Unknown usage is charged in full: a dead child is charged its whole carve unless the host measured less, a budget stop its whole carve, and a node's reported own spend can only grow; only a child that never started refunds everything. Recovery settles dead processes `interrupted` and queued orphans `cancelled`; `render_tree` prints the audit view. Not wired yet: the delegate tool, `tole audit` and the profile wiring follow.
- Approved-hash skill loading (#591, epic #589): with `TOLE_SKILL_APPROVAL=required` a discovered skill loads only when its package (SKILL.md plus every other file of its directory) hashes to the owner's approval in `$CODECORA_HOME/tole/skill-approvals.json`; any other skill is listed by name only as unavailable, `load_skill` and `skill_search` never return its text, and the loaded body is parsed from the hashed bytes. New `tole skills approve <name> [--hash]`, `revoke` and `list` (status, hash, approval time). New `tole-core` module `skill_approval` (`Gate`, `Approvals`, `read_package`); `skill_index::select` and `SkillSearchTool` take a `Gate`. The default `off` keeps today's behavior; `--skill` and profile skills are not gated.
- Skill index seam, part 1 of #590 (epic #589): new `tole-core` module `skill_index`. The `SkillIndex` trait (`search(query, filter, k)`, `upsert`, `retire`) has two adapters: `LocalIndex` (the skills directories, lexical match; the fallback without uteke) and `UtekeDocIndex` (`uteke doc`, slug `skill-<name>`, tag `skill`, metadata name / description / profiles / status / `sha256:` content hash; semantic search, so the similarity floor is a cosine; only `approved` entries in scope for the profile are returned). `select` builds a task's list: a pinned core of at most 5 plus the top K (default 5, at most 10) above the floor; fewer or zero is fine, and an index failure is a note, not an error. `RecallQuery` is built from the owner-written brief plus the profile role only. The ReadOnly `skill_search` tool searches on demand. The index only finds names: descriptions and bodies always come from the local skill files, and a name with no local skill is dropped. Also `skills::skill_file`, and a `sha2` dependency (already in the tree via rmcp). Nothing uses the seam yet; the session wiring and `tole skills` follow in part 2.
- Skill recall in sessions, part 2 of #590: opt-in with `TOLE_SKILL_INDEX=uteke` (`TOLE_SKILLS_PINNED`, `TOLE_SKILL_RECALL_K`, `TOLE_SKILL_FLOOR`; an invalid value fails startup). `run` lists the pinned core plus the skills recalled for its prompt; `chat`, `resume` and the TUI list the pinned core; all of them get `skill_search`. The default `local` keeps today's full listing unchanged, and `uteke` without the binary falls back to it with a note. New `tole skills index|retire|search` maintains and inspects the uteke index (`index` sends name, description, status and content hash, never the body). Not included: config / profile keys for these settings (after #584), one parent document per category (#576), recall per delegation and discussion participant (#585, #588), the mid-task note for new skills (#592), and the token-matched eval.
- Dormant profiles (#584, epic #583): `[profiles.<name>]` tables in the **user** config (a project config's `[profiles]` is ignored with a warning and marked `IGNORED` in `tole config check`) carry a persona file (`system_prompt_file`), an `allow` list, `skills`, a `memory_room` (default `profile-<name>`), `grants` (secret NAMES only; a value-shaped entry is a config error) and `[profiles.<name>.models]` tiers `light` / `standard` / `heavy` / `advisor` with a `default` (`standard` unless set; it must name a set executing tier). The global `--profile <name>` selects one for any session command (run, chat, resume, mission, serve, acp, mcp, tui): its values form one layer between the per-key flags and the environment (per-key flag > profile > env > config > user config), the default tier's model replaces the provider triple's model (it beats `TOLE_MODEL`), the persona replaces the system prompt (for `mission`, the default base prompt), and the memory loop uses the profile's room unless a child agent's mailbox room is set. Dormant by construction: resolving a profile reads only its own persona file (64 KiB cap) and starts nothing. An unknown name fails startup and lists the defined ones. `tole-cli` API: new `profile` module (`Profile::resolve`, `Profile::granted`), `config::{ProfileConfig, ModelTiers}`, `config_apply::Source::Profile`, `Flags::profile`, `Settings::memory_room`, `Settings::fill_with_profile`, `config_apply::profile_system_prompt`; `mission::MissionConfig` gains `persona`. Not included: `tole cron create --profile` (the cron CLI landed separately), `tole config check --profile`, and the use of tiers other than `default` (routing #586, advisor #587) and of `grants` (#585, #598).
- TUI keybinding config (#463, part 4 and last: closes the issue). `[tui.keys]` in the **user** config rebinds ten convenience actions (`toggle_tools`, `toggle_reasoning`, `model_picker`, `cycle_model`, `cycle_thinking`, `newline`, `queue_follow_up`, `pop_queue`, `history_prev`, `history_next`), each to one key spec or a list (`ctrl+o`, `alt+enter`, `shift+tab`, `f5`, case-insensitive); an action that is not listed keeps its default, and the default keymap is exactly the keys the TUI always had. Never rebindable: typing (every bare character), `y` / `n` at an approval prompt, Esc, Ctrl+C, Enter, Backspace and Tab; a binding needs Ctrl or Alt (or is a function key), and keys a terminal cannot tell apart (Ctrl+I/M/H/[, Ctrl+digit) or reserves (Ctrl+S/Q/Z/\) are refused. At an approval prompt and in a picker the keymap is not consulted, and the dispatcher also refuses to map the protected keys whatever the table says. An unknown action, bad or protected key, or one key on two actions makes the TUI use the whole default keymap with one startup note (never an abort); `tole config check` shows the result or `INVALID: ...` (`not validated: this build has no TUI` without the `tui` feature). A project config cannot set keys (same single startup warning and `IGNORED` mark as the theme), and there is no env var. The footer hint and the `[+n lines]` marker show the effective binding. Behavior changes: at an approval prompt only plain `y` / `n` answer (before, `Alt+Y` / `Ctrl+N` did too); Shift+Enter and Alt+Enter never send even when unbound; Ctrl+Alt+<letter> no longer acts as Ctrl+<letter>; the `/model` and `/thinking` menu descriptions no longer name a key. `tole-tui` API: new `Keymap`, `KeyAction`, `KeySpec`, `App::with_keymap`, defaulted `TuiHost::keymap`; `Key` gains `Left`, `Right` and `F(u8)` (they were `Key::Other`). `tole-cli`: `Settings::{tui_keys, keymap}`, `TuiSession::with_keymap`.
- TUI `/fork` and `/tree` (#463, part 2 of the TUI polish; the issue stays open for export/copy, themes and keybindings), a UI over the session branching of #210. `/fork [entry-id]` forks the open session at a settled entry (default: the last one, like `tole branch`) into a new session and switches to it in place, as `/new` and `/resume` do; the new session is indexed (`sessions --all`, face `branch`) and its header says `forkedFrom`. The source is only replayed read-only (never locked or written), so forking while the TUI holds its writer lock is safe; an unknown or unsettled entry, or a session with nothing settled yet, is refused with the core error and leaves the session and the sessions directory as they were. `/tree` opens a picker over the family of the open session (its parent chain up to the root and every fork below it, siblings and grandchildren included), indented by depth with the open session marked and preselected; picking a row switches to it. It is built from the same `forkedFrom` data as `sessions --tree`. Like `/new` and `/resume`, both are ignored while a turn runs, and neither replays the old transcript on screen. `tole-tui` API: new `HostCmd::{Fork(Option<String>), PickTree}` (answered with `HostReply::Session` and `HostReply::Sessions`) and `PickKind::Tree`; a host that does not implement them refuses them like the other commands. `tole-cli`: `session_index::{tree_nodes, tree_order, family}` (`sessions --tree` now shares the scan). Not included: in-file branching (several branches inside one session file), transcript replay after a switch.
- TUI `/export [path]` and `/copy` (#463, part 1; the issue stays open for fork/tree, themes and keybindings). `/copy` sends the last assistant answer to the terminal clipboard with an OSC 52 sequence written by the TUI process (`ESC ] 52 ; c ; <base64> BEL`): no native clipboard dependency, the payload is only base64 so answer text cannot inject escape bytes, answers over 64 KiB are refused with a pointer to `/export`, and since a terminal never confirms OSC 52 the note says what was sent (`needs OSC 52 support; tmux needs set -g set-clipboard on`), never that it worked. `/export` writes the session transcript as `.html` (self-contained: every text escaped, no script, no external resource, a `default-src 'none'` CSP meta, light and dark) or `.jsonl` (the entries exactly as stored); no argument writes `<workspace>/.tole/exports/<session id>.html`. The path is workspace-jailed (canonicalized, symlink-safe), an existing file or symlink is never overwritten or followed, the file is created owner-only on Unix, and an unknown extension is refused. Transcripts contain tool output and may contain secrets. Like the other commands, both are ignored while a turn runs. `tole-tui` API: new `HostCmd::{LastAnswer, Export}` and `HostReply::{Answer, Path}`; `TuiSession::with_workspace`, `last_answer` and `export`.
- TUI paste and image attach (#560; epic #523): the TUI enables bracketed paste (and turns it off on exit and on panic), so a pasted block is inserted into the editor as one edit, with CRLF/CR normalized to LF and control characters other than newline and tab dropped, and is never sent before `Enter`. Before, a multi-line paste arrived as keys: the first line was sent at the first line break and the next one steered that turn. `/image <path|data-url>` queues an image for the next message (`/image` lists, `/image clear` empties; same jail, cap and sniffing as `--image`), shown as an `image: …` row above the editor; a paste that is exactly one image path or `data:image/…` URL is attached instead of inserted (inserted as text, with the host's reason, when it cannot be attached). Decision: no OS clipboard images in the TUI (GUI clients own them; no native dependency). `tole-tui` API: `Input` is now an enum, `Input::Key(KeyPress)` | `Input::Paste(String)` (the `Input::new/ctrl/alt/shift` constructors are unchanged; code reading `input.key` / `input.mods` matches `Input::Key` first); new `HostCmd::{AttachImage, ClearImages, Images}`, `HostReply::Images`, `ImageItem`, `Action::PasteImage` and `term_input::input_from_event`.
- Images on any turn and ACP image blocks (#559; epic #523): `tole chat` takes `/image <path|data-url>` to queue an image for the next message (`/image` lists the queue, `/image clear` empties it), so images are no longer limited to the first message; a message that could not be recorded keeps its images queued. The TUI host (`TuiSession`) gets the same queue for #560 to drive: `attach_image(spec)`, `attach_image_ref`, `pending_images`, `clear_pending_images`, taken by the next `run_turn`. `tole acp` now advertises `promptCapabilities.image` (when a spill dir exists) and maps ACP `image` content blocks onto the existing spill-and-reference path of CLI `data:` URLs: the declared `mimeType` is not trusted (bytes are sniffed), the optional `uri` is never read, a bad image refuses the prompt, an image-only prompt now runs instead of being refused as "missing prompt", and audio / embedded-resource blocks stay ignored. An image-only user message no longer sends an empty text part (`tole-core`). New shared-runner entry points `turn_runner::{run_turn_on_with_images, run_turn_with_images, resolve_image, resolve_inline_image, inline_images_supported}` and `session_host::run_session_turn_with_images`. Decision recorded: no model-callable image tool for now; images stay user-supplied (threat model).
- Image input on `tole serve` (#558): `POST /sessions/{id}/images` takes one image as the raw request body, sniffs its type from the bytes (PNG, JPEG, GIF, WebP; the request's `Content-Type` is ignored), resolves it through the same core path as `--image` (jail, 10 MiB cap, FNV-1a 64) and stores it content-addressed under `~/.codecora/tole/images/serve/<session id>/`; the answer is `{id, mime, bytes, fnv1a64}` (the id is the hash, no server path). `POST /sessions/{id}/prompt` accepts `"images": [<id>]` next to `text`; only that session's uploads resolve and the turn records the references exactly like `--image` (no blob in the log), so they are re-sent after a restart and `resume`. `GET /sessions/{id}/images/{fnv1a64}` serves only an image recorded in that session's log, re-checking the jail, size and hash of the bytes served, with the recorded `Content-Type` and `X-Content-Type-Options: nosniff`; a mismatch is a `409` (a recorded image outside the jail a `403`), never a silent serve. The 10 MiB upload ceiling applies only once the headers carry the right token (otherwise the 1 MiB body cap stays; a declared body over the cap is authenticated first, so a wrong token gets `401` through the per-IP limiter and only the right token gets `413`), and uploads count within the 32-connection cap. Quotas in bytes: `TOLE_SERVE_IMAGE_SESSION_QUOTA` (default 64 MiB) and `TOLE_SERVE_IMAGE_TOTAL_QUOTA` (default 512 MiB), `507` when exceeded; `415` for a non-image, `413` over the cap. No `tole-core` API change. Not included: garbage collection of unused uploads (they count against the quotas until removed by hand), inline `data:` URLs in the prompt JSON.
- `review_gate` config key (#534 follow-up): `off | warn | block` in `.tole/config.toml` or the user config, with precedence flag > `TOLE_REVIEW_GATE` > config > user config > default `block`; an invalid value is a config error and `tole config check` shows the effective value and its source. A project file can only keep or tighten the gate: when both layers set it the STRICTER wins, so a cloned repository cannot loosen the gate a user chose (the one key that is not simply project-over-user). A project-only value is used as given, behind the project-config trust gate. No effect without the cora review tools.
- Event stream on `tole serve` (#524, third and last part; epic #523): `GET /sessions/{id}/events?after=<seq>` is a Server-Sent Events stream for graphical clients. **Durable** `entry` frames carry the log's entries with their `seq` as the SSE id and replay losslessly after a dropped connection (`after`, or `Last-Event-ID`, which `EventSource` sends itself); `ready` marks the end of the replay. **Ephemeral**, best-effort frames carry streamed text and reasoning, tool start/finish and `approval_requested` / `approval_resolved`; they are never stored or replayed, and a slow client gets a `resync` marker instead of slowing the turn (bounded, non-blocking per-stream queue). A turn only gets live text/tool events when it starts while a stream is open on that session (with no stream open it runs exactly as before); the turn that resumes after an approval streams durable entries only. Heartbeat comment after 15 s without a write (`TOLE_SERVE_HEARTBEAT_SECS`, 1 to 3600, changes it); `closed` and a clean end when the session leaves the live map. At most 8 open streams per server (`429` beyond that), counted within the 32-connection cap so they cannot starve `/approvals` and `/prompt`. Additive: no existing route or response changes. New `events` module (SSE framing, bus, limiter, log tail, approval diff). With this, #524 is complete; the next UI-epic items are #525 and #526. Not included: a stream token for browser `EventSource` (the web client, #529), a log index for very large sessions, an observer for the approvals-resume turn (a `tole-core` change).
- Resume-by-id on `tole serve` (#524, second of three parts; epic #523): `POST /sessions {"resume": "<id>"}` makes an existing session live again, e.g. after a serve restart, so a UI client can reopen it (history via `GET /sessions/{id}/entries`). The id is validated and joined into the session directory only (no client-supplied path); `cwd` goes through the same jail check as a new session and a session is found only under the workspace it was created in (`404` otherwise). An already-live session is returned as is (`alreadyLive`, with its `busy` flag), never reopened, so a running turn is not disturbed. `409` when another process holds the session's writer lock, `503` at the live-session cap with every session busy; a resumed session queues non-preauthorized Writes like a new one. `POST /sessions` for a new session now shares its open-and-admit code with resume, behavior unchanged. Next: the SSE event stream.
- `GET /sessions/{id}/entries?after=<seq>&limit=<n>` on `tole serve` (#524, first of three parts; epic #523): a paginated transcript read for graphical clients. Returns the session log's entries with `seq > after` exactly as stored (`nextAfter`, `hasMore`, `lastSeq` for paging); `limit` defaults to 100, is clamped to 500 and a page stops near 1 MiB. It replays the session file read-only instead of taking the storage lock, so it works while a turn is running. New `protocol` module for the wire types (serde structs; schema export deferred until a client generator needs it). Additive: no existing route changes. Live sessions only for now; resume-by-id and the SSE event stream are the next two parts.
- Optional fetch backend for `web_fetch` (#532): with `TOLE_WEB_FETCH_URL` set, `web_fetch` asks that backend (a scraping service with browser and reader fallbacks, say) for pages that need JavaScript or block direct fetches, and stays a `ReadOnly` tool with the same trust model as `web_search`. Default contract `GET {url}?url=<target>` → `{"content": "…"}`; `TOLE_WEB_FETCH_METHOD=POST` sends `{"url"}`; `TOLE_WEB_FETCH_HEADERS` (`Name=${VAR}` pairs, sent only to the backend) and `TOLE_WEB_FETCH_FIELDS='content=<field>'` work like the search settings; a malformed value fails startup. The target is vetted before anything is sent (https only, no credentials in the URL, public host; `TOLE_WEB_ALLOW_PRIVATE=1` opts out). Backend only, no direct fallback. Unset: `web_fetch` is the unchanged direct fetcher. The target URL is disclosed to the backend operator (threat model). The response shape of a specific real backend is not verified here: the field mapping exists for that. Not included: a fallback order, a format hint to the backend.
- `evals/shell_usage.py` (#531, step 1): a read-only report over session JSONL of how often `run_command` / `job_start` inputs use shell features (pipe, chain, redirect, substitution, leading `cd`, `bash -c` wrapper), how many failed and how many follow a failed call. No runtime change and no new logging; `run_command` behavior is unchanged. The repository's own traces are too small and too scripted to decide anything (9 calls); the decision waits for real sessions.
- Review gate before `git commit` (#534): while both cora review tools are attached (`mcp_cora_cora_review_prepare` / `_finalize`), a `git commit` is refused until the staged diff has a validated review, and the refusal tells the model exactly how to get one. tole does not hash or record anything itself: it asks cora >= 0.20.0's own commit gate (`cora hook verify`: exit 0 validated or nothing to review, 10 reviewed but blocking, 11 not validated), whose marker is bound to the exact change set, so tole and cora cannot disagree about "this diff". It covers the `git` tool's `commit` op and `run_command` / `job_start` running `git commit` (behind `env`, `VAR=x`, a path to git and git's global options). `git -C <dir> commit` is verified in `<dir>`, the repository the commit really happens in; a commit that redirects git's git dir or work tree (`--git-dir`, `--work-tree`, `GIT_DIR=` / `GIT_WORK_TREE=` / `GIT_INDEX_FILE=`, `-c core.worktree=`) cannot be checked and is refused under `block` (noted under `warn`). `TOLE_REVIEW_GATE=off|warn|block` / `--review-gate` (default `block`; the flag wins; an invalid value fails startup; no config key yet, so a cloned project's config cannot weaken it); with no review tools attached there is no gate; a cora without `hook verify`, or one that cannot run, fails open with one warning; `tole doctor` reports the state. Child agents inherit the policy through the environment. Not included: `sh -c`, scripts, git aliases and `merge` / `cherry-pick` / `am` (see the threat model); a config key.
- `tole setup cora` (#535): installs or upgrades the cora binary the review flow, `mcp_cora_*` and `cora_search` need. It runs `cargo install cora-code --locked` (the same constant `tole doctor` prints) after a `[y/N]` at a terminal or `--yes`; with neither it refuses (exit 2), a supported cora is left alone, `--check` only reports, no cargo prints the manual command (exit 1). The result is verified at `$CARGO_HOME/bin/cora` and a still-too-old result is an error; a note says when PATH resolves a different copy first. `tole doctor` now ends its cora lines with "(or run `tole setup cora`)". Same trust as `tole upgrade` (cargo verifies the registry checksum, tole downloads nothing). Not included: a prebuilt-archive download path (cora-code's release layout is unverified), embedding cora (a CLI-only crate), and `uteke` (the table is ready for one more variant).
- Streamable HTTP MCP client (#530): `--mcp-server name=https://host/path` (and the `mcp_server` config key) connects to a remote MCP server next to the existing stdio form, inside the existing `mcp` feature of `tole-core`. The bearer token is read from `TOLE_MCP_<NAME>_TOKEN` only (name upper-cased, `-` becomes `_`), re-read on every reconnect and sent only to that endpoint; it and the url are scrubbed from every error. `https` is required (plain `http` only for loopback), a url with credentials is refused, and a redirect to another origin is not followed. Tools register as `mcp_<name>_<tool>` and stay `Risk::Write` (pre-authorize with `--allow 'mcp_<name>_*'`); results stay capped and fenced. Build without the `mcp` feature links no HTTP client. **API change in `tole-core`:** `McpServerConfig` now holds `transport: McpTransport` (`Stdio { command, args }` | `Http { url }`) instead of `command` / `args`; new `mcp::{McpTransport, token_env_name}`. Not included (deferred): server-initiated notifications and SSE resumption, OAuth.

### Deprecated
- `agent_start` / `agent_poll` (#585): in the CLI faces they are now aliases of `delegate` / `delegate_poll` (same tree, same caps; `prompt` is accepted for `task`) and will be removed in the next release. `tole serve` keeps the classic depth-1 tools for now.

### Changed
- `tole-tui` rendering split out of `lib.rs` (#526, fourth part): `App::draw` and its styling helpers (`tail`, `styled`, `diff_style`, `inline_spans`, `wrap`) now live in a new private `render` module that only reads `App`, so a second front end can replace or reuse drawing without touching the state machine. A pure move: no behavior change, no public API change.
- `tole-tui` approvals as source-agnostic UI state (#526, third part): a pending approval is an `ApprovalRequest` (one sanitized line plus the `Risk` tier, with `can_allow_always()` encoding that a `Destructive` call is never allowed always) and a `Pending` whose reply is a one-shot closure instead of a `Sender`. `Prompter::ask(request)` blocks for an in-process turn as before; the new `Prompter::post(request, reply)` does not block, so a remote source (the `/approvals` queue of a server) can feed the same UI and receive the verdict. The verdict is delivered exactly once, and dropping a `Pending` unanswered still denies (fail closed), now through its `Drop`, so a cancelled turn, a replaced request or a vanished UI all deny. No behavior change for users. Breaking for code outside this repository that uses the removed API: `Prompter::ask(text, destructive)` is now `ask(ApprovalRequest)` and `Pending`'s fields are private; new public items `tole_tui::ApprovalRequest` and `Prompter::post`.
- `tole-tui` host seam (#526, second part): `TuiHost` shrinks from eleven methods to three (`footer`, `run_turn`, `apply`). `apply(HostCmd) -> Result<HostReply, String>` replaces `models`, `set_model`, `set_effort`, `request_compaction`, `new_session`, `sessions`, `switch_session`, `status_lines` and `set_name`; no slash command is lost. `HostReply` (`Done`, `Session(id)`, `Models`, `Sessions`, `Lines`) carries data only, and the UI keeps the wording and the pickers, so a remote adapter has a small interface to implement. The default `apply` supports `Status` (no lines) and refuses the other commands as before. A reply of the wrong kind from a host is reported in the transcript instead of panicking. `TuiSession` keeps its session operations as plain methods and `apply` dispatches to them. No behavior change for users. Breaking for anyone implementing `TuiHost` outside this repository (override `apply` instead of the removed methods); new public item `tole_tui::HostReply`. Next: model a pending approval as source-agnostic UI state.
- `tole-tui` input seam (#526, first part): `App::on_input(Input)` takes a UI-neutral `Input` (`Key::{Char, Enter, Esc, Tab, BackTab, Backspace, Up, Down, Other}` plus ctrl/alt/shift) so the key logic no longer depends on terminal types. The crossterm mapping is an adapter (`term_input::input_from`, and `App::on_key(KeyEvent)` as the terminal entry point, which drops key releases); keys the logic does not distinguish map to `Key::Other`, which still disarms a pending Ctrl+C exactly as before. No behavior change: the key table in `docs/tui.md` is unchanged and all existing tests pass without edits. New public items: `tole_tui::{Input, Key, Mods}`. Next: collapse `TuiHost` into one `apply(HostCmd)` path and model a pending approval as source-agnostic UI state.
- Internal refactor, no behavior change (#525, first part): tool registration moved out of `session_host.rs`. `UpdatePlanTool`, `PlanEmitter`, `register_gitea`, `detect_github_repo` and `github_repo_from_remote_url` now live in `tole_cli::tool_roster` next to the rest of the roster; `session_host` keeps only session hosting (open, turn, resume, cancel, interrupt, detach, list). Library paths change accordingly (`tole_cli::session_host::UpdatePlanTool` is now `tole_cli::tool_roster::UpdatePlanTool`, and likewise for the other four); there is no re-export. Part 2a: new sessions are created through one helper, `tole_cli::session_host::create_session`, which creates the session file and registers it in the global session index in the same call. `run`, `chat`, `tui`, `mission`, the server faces and the TUI's `/new` all use it (they each created and indexed by hand before), so a face can no longer create a session and forget to index it. Error messages and index contents are unchanged. Part 2b: one turn runner. `tole_cli::turn_runner::{TurnPlan, run_turn_on, run_turn_with, resume_turn_on}` describe how a turn is run (build the session's provider, optionally settle a turn parked mid-flight, optionally compact, run), and `run_session_turn` / `resume_session_turn` (serve, ACP, MCP) and `TuiSession::run_turn` all go through it. The differences between faces are now explicit plan fields (`resolve_parked`, `compaction`: the TUI sets both, the server faces neither) instead of two copies of the sequence. Behavior is unchanged, including each face's error text (the TUI keeps its `storage error:` prefix); on a parked session the server faces still get the core's refusal ("resolve the session first"). Part 2c: a session's durable settings live in one module, `tole_cli::session_settings`: the register names (`fact/model`, `fact/reasoning_effort`, `fact/session_name`), the label validation, and the writers (`set_model`, `set_effort`, `set_name`, `request_compaction`, plus `write_model` for ACP, which validates against its own model list first). The TUI's `/model`, `/effort`, `/name` and `/compact` and ACP's model picker write through it, and the readers (`session_model`, `session_effort`, `status`) use the same constants instead of repeated string literals. Behavior and error text are unchanged. With this, #525's consolidation is done as shared primitives (create + index, one turn runner, settings); what is left is listed on the issue.

## [0.10.1] — 2026-11-04

### Fixed
- Cora-scan minors on the TUI surface (#514): the unknown-command hint no longer byte-slices the typed command (empty or multibyte input could panic), the `@file` walk also skips names containing a newline, and the status snapshot drops a discarded float division and a duplicate `impl` block. Waived with reasons: holding the session lock across a turn (exclusive `&mut` storage by design), counting compactions via `compaction::history` (one-shot command, no storage counter), and un-cleaned temp dirs in `status` tests (OS temp, no tempfile dependency).

## [0.10.0] — 2026-11-04

### Added
- TUI steering, follow-up queue, collapsible output, `@file`, markdown and diff styling (#462): `Enter` while a turn runs pushes the draft on the existing soft-interrupt queue (#377), so the turn commits it as an attributed user message at its next safe point and continues (nothing is cancelled; a steer the turn never reached runs next, or returns to the editor after a cancel); `Alt+Enter` queues a follow-up shown as `queued: …` rows above the editor, the head runs after the turn ends with an answer (one at a time), `Alt+Up` pops the last back into the editor, and `Esc` / `Ctrl+C` cancel and return the whole queue to the editor. `Ctrl+O` expands/collapses tool output (multi-line results are collapsed to the first line plus `[+n lines · Ctrl+O]`), `Ctrl+T` hides/shows reasoning; both are UI state only. Typing `@` opens a file panel (depth ≤ 3, ≤ 2000 files listed, 50 shown, `Tab`/`Enter` insert the path); answers get minimal markdown styling (headings, `**bold**`, inline code, fenced code) and `+`/`-`/`@@` rows of tool output are green/red/dim, all sanitized before styling. Not included (deferred): tables, nested lists, italics and links in markdown; retroactive collapse of already-printed scrollback; diffs beyond the 200-character tool preview; `@` insertion at an in-buffer cursor.
- TUI model picker, thinking level and slash-command menu (#461): `/model` and `Ctrl+L` open a filter-as-you-type picker over the provider's `/models`, `/model <id>` sets one directly, `Ctrl+P` cycles the models used in this run; `/thinking [level]` and `Shift+Tab` cycle `minimal → low → medium → high → unset` and the editor border shows the level; typing `/` opens a command menu (`Tab` completes, `Up`/`Down` select) for `/compact`, `/new`, `/resume` (picker over the global session index), `/session` (the status lines), `/name` and `/quit`; unknown commands are refused with a prefix hint. Model, effort and name are session registers (`fact/model`, `fact/reasoning_effort`, `fact/session_name`), so the next provider call uses them and they survive `tole resume`; `/name` is what `sessions --all` and `resume <name>` show. `/compact` requests the forced pass before the next prompt (no mid-turn compaction; its optional instructions are not supported yet and are reported as ignored). Not included (deferred): `/resume` paging beyond the newest 50, replaying the old transcript after `/new` / `/resume`.
- Terminal UI MVP (#460): bare `tole` on a TTY now talks to the agent. Inline viewport with a streaming transcript (answer text as it arrives, reasoning dimmed, tool calls and results as compact lines), a multi-line editor (`Enter` sends, `Shift+Enter` / `Alt+Enter` / `Ctrl+J` newline, `Up`/`Down` history), a footer (folder, session, model, effort, `context used / window (%)`, session tokens from the `status` snapshot), inline `y`/`n` approval for Write and Destructive tools (the normal `--allow` / `--yes` rules still apply), and `Esc` to cancel the running turn. Sessions are ordinary tole sessions (`tole resume`, `sessions --all`). Hidden `tole tui` subcommand (what bare `tole` runs) goes through the normal config gate. Not included (deferred): in-line cursor movement, slash commands beyond `/quit`, memory loop, MCP prompt commands, sibling notices, soft interrupts (#377/#383).
- Reasoning effort (#457): `--effort <minimal|low|medium|high>` / `TOLE_REASONING_EFFORT` / config `reasoning_effort` (flag > env > config > user config > unset, validated; shown by `tole config check`) is sent as `reasoning_effort` on OpenAI-compatible chat completions only when set, so an unset request body is byte-identical. A session's durable `fact/reasoning_effort` register overrides it and survives resume (an empty or invalid value falls back to the default). A provider that answers HTTP 400 naming `reasoning_effort` gets ONE degraded retry without it: the rejection is remembered for that provider and recorded under the namespaced `tole_degraded` key of the step's usage row, never counted as a provider step. New core API: `openai::{parse_reasoning_effort, REASONING_EFFORTS}`, `OpenAiProvider::with_reasoning_effort`, `Provider::last_degraded` (default None). Not included (deferred): a setter for the register (the TUI `/thinking` command, #464) and the ACP config option.
- Multi-client daemon design note (#382): `docs/daemon-design.md` records how jcode's daemon works (single server, Unix socket, client registry, 5-minute idle timeout, `exec()` hot-reload, client auto-reconnect), a clearly speculative mapping onto tole, the user stories that would un-park it, and an explicit anti-goal. PARKED (tier B): docs only, no code, nothing to build without a user story and owner approval.
- Claude Code session import (#381): `tole import-claude <transcript.jsonl>` imports a Claude Code session (the `~/.claude/projects` JSONL or `claude -p --output-format stream-json` output) as a NEW tole session `cc-<claude-session-id>`; continue it with `tole resume cc-<id> "<prompt>"`. Prompts and assistant text become messages, `tool_use` / `tool_result` blocks become intent / result entries (Claude Code tool names are kept as history only, never executed), model, version, branch and timestamps are recorded in the `fact/cc_import` register, and the header takes the transcript's cwd. Additive: existing sessions are untouched and importing the same session twice is refused. Malformed lines, thinking blocks and sidechain (subagent) lines are skipped (malformed ones with a warning); a dangling prompt or open tool call at the end is dropped and reported so the session rests at `Final`. New core API: `ccimport::{import_file, import_str, CcImport}`, `JsonlStorage::cwd`. Not included (deferred): attaching to a *running* `claude` process (and its double-attach guard), image blocks, a live provider round trip (unverified).
- Detach a running turn (#217): stop waiting without aborting anything. `POST /sessions/{id}/detach` (serve; `202`, `409` when no turn runs) and the MCP tool `tole_session_detach` set a new `CancelToken::detach()` flag. The running tool settles and its result is committed; before the next provider call the turn writes a durable `detached` entry and settles `pc=Final` as the new `TurnOutcome::Detached` (stop reason `detached`; `tole run` exits 9). The session stays prompt-resumable, and a cancel always outranks a detach. A background job started with `job_start` in a serve/MCP/ACP session now nudges that session when it exits: a notice (`[tole] background job <id> finished …`) is queued on the soft-interrupt queue and delivered at the next safe point, which is the next turn's first when the session is idle. New core API: `CancelToken::{detach, is_detached}`, `TurnOutcome::Detached`, `JobStartTool::with_completion_notice`. Not included (deferred): the Ctrl-B key and a `tole detach` command for `chat`/`run` (they have no concurrent input while a turn runs; this lands with the terminal UI, #464), moving an in-flight `run_command` into a job (tools are not interruptible mid-call), nudges that survive a host restart (the queue is in memory; `job_poll` still has the result), and the ACP face.
- MCP prompts and resources (#213): the MCP client now probes each server's `prompts`/`resources` capabilities (probe-first: a server that advertises neither is never asked and adds nothing). In `tole chat`, a server's prompts are listed at startup and invoked as `/mcp:<server>:<prompt> [key=value ...]`; the server renders the prompt, tole prints it and sends it as that turn's user message (text parts only, capped at 256k chars). Servers that advertise `resources` add one ReadOnly `mcp_read {server, uri}` tool (`resources/read`; text contents only). Server prompt text is server-controlled content (threat-model note added); tool calls it provokes still pass the approver. New core API: `mcp::{probe_server_extras, get_server_prompt, parse_prompt_command, McpReadTool, McpExtras, McpPromptInfo}`. Not included (deferred): `tole prompt run` outside chat, quoted/spaced argument values, resource templates, `resources/list` surfacing, and subscriptions.
- Session forks (#210): `tole branch <id> [--at <entry-id>]` starts a new session holding the source's history up to a settled entry (a final assistant answer; default: the last one), so approach B can be tried without losing approach A; continue it with `tole resume <new-id> "<prompt>"`. `tole sessions --tree` nests forks under the session they were cut from. A fork is a separate append-only file whose header records `forkedFrom: {session, entry}` (additive; older files replay unchanged), so replay and todo hydration are correct per branch and the source is never touched. Usage rows and registers are not copied. New core API: `JsonlStorage::fork` / `forked_from`, `storage::ForkOrigin`. `--at` and the default cut only accept a final assistant message with no tool calls hanging off it. Not included (deferred): several leaves inside one file with lineage-following readers, resume by branch head, and a `--fork` flag on `resume`.
- Image input (#212): `tole run|chat|mission --image <path|data:…;base64,…>` (repeatable) attaches images to the first message, and `tole_core::turn::run_turn_with_images` is the embedder entry point. Paths must resolve inside the workspace jail (`..` and symlink escapes refused); inline images are spilled to `<codecora home>/tole/images/`; png/jpeg/gif/webp sniffed from bytes, 10 MiB cap. The OpenAI wire gets `text` + `image_url` content parts; the write-once log stores only a reference (`payload.images`: `path`, `mime`, `bytes`, `fnv1a64`), and each request re-loads the file re-checking jail and hash, so replay/resume keeps working and a changed or missing file reaches the model as an `[image unavailable: …]` note. A non-vision model is a loud `model-capability` error: `TOLE_VISION=0` refuses before any request, and a 400/415/422 naming images becomes the same error. New core API: `image::{resolve, data_url, ImageRef}`, `OpenAiProvider::{with_image_roots, with_vision}`. Image tokens appear in the request size (`tole_wire.history_chars`) and the provider-reported usage. Not included: images referenced inside prompt text (only `--image`), images on ACP/serve/MCP faces, a real vision-model round trip (unverified).
- Context-overflow recovery and a missing-window warning (#481). A provider HTTP 400/413/422 whose error text says the request exceeded the context window (`context_length_exceeded`, "maximum context length", "prompt is too long", ...) now ends the turn with the provider's message plus `(compaction will run on the next resume)` and records a durable `fact/context_overflow` register in the commit that settles the turn; the next `resume <id> "<prompt>"` / `chat` message forces one compaction pass first (independent of window detection and threshold), clears the flag, then runs the prompt. Owner decision 2026-11-03: compaction stays between turns — no compact-and-retry in place. `--no-compaction` also disables the recovery pass; a failed summary call keeps the flag. `tole doctor --tier catalog` now prints a non-fatal `!` warning (exit code and ledger unchanged) naming `TOLE_CONTEXT_WINDOW=<tokens>` when neither the setting nor `/models` gives a window, and reports the window it found otherwise. New core API: `compaction::{is_context_overflow, mark_overflow, overflow_pending, clear_overflow}`; `doctor::run_cli` takes the configured window. Not included (deferred): a built-in window table and a 24 h disk cache.
- Bounded provider retry (#478): a turn now retries 429, any 5xx, connection/timeout errors, stream read failures and empty completions up to 3 times (was one retry for timeout/429 only), waiting 1/2/4 s with jitter or the `Retry-After` seconds from the error text (`(retry-after: N)`, #474), capped at 30 s. The wait is cancel-aware (1 s slices on the `CancelToken`) and each retry is a durable `provider transient failure, retrying` entry written before the wait. Other 4xx stay fail-loud. An empty completion that outlasts its retries still does not fail over to the reserve.
- Interruptible provider calls (#484): a cancel (Esc, `--timeout`, ACP `session/cancel`, serve cancel) now interrupts a provider call already in flight instead of waiting for the provider to answer or the 120 s HTTP timeout. New additive `Provider::complete_with_cancel(&mut self, transcript, &CancelToken)` (default delegates to `complete`, so embedders and mocks are unchanged); `OpenAiProvider` runs the request on a helper thread and re-checks the token every 100 ms, `FailoverProvider` and `Box<P>` forward it, and the turn loop calls it. On cancel the thread is abandoned, nothing is committed for the call, and the turn settles as cancelled as before (resumable). A streaming call also stops reading and stays silent on the live sinks. The abandoned request may still be billed by the provider. Measured: `tole run --timeout 1` against a provider silent for 8 s returned after 8.25 s, now about 1 s, in stream and non-stream modes.

### Changed
- Storage syncs once per durable commit point instead of once per commit (#483): a tool step issued about 4.8 `fdatasync` calls (5.7 with a Write tool); it now issues 2 (intent before the effect, `finish` after it) and a final answer 1, measured by the new `fsync_budget` test through the real turn loop. New `Commit::defer_sync()` writes a line without syncing; the next durable commit's `fdatasync` persists it. The turn loop defers only the usage row, the `ToolCall` hop, `wrote_this_turn` and the settlement, each always followed by a durable commit. File format, replay, torn-tail repair and poison semantics are unchanged.
- Sequential read-only tool-call batches (#480): when one model response carries several tool calls and ALL of them are registered ReadOnly tools, the turn loop now runs them in model order, each as its own effect sandwich, inside ONE model step, instead of running the first and dropping the rest (the model re-asked each, costing a round trip apiece; measured 11 of 22 calls re-asked on glm-5.1). Any non-ReadOnly or unknown tool in the response keeps the old first-call-only behaviour. This is NOT concurrent execution. A failed call stops the batch and the remaining calls get a `not executed` error result. New additive `Provider::last_extra_tool_calls` (default empty; `ProviderOutput` is unchanged), `machine::begin_batch_member`, and a `"batch": true` marker on the 2nd..Nth intents that the OpenAI wire flattening merges into one assistant `tool_calls` array. Design note: `docs/tool-batches.md`. The PRD non-goal now reads "no concurrent execution; sequential batches supported".
- Release builds use thin LTO, a single codegen unit and `strip = true` (#482): the `tole` binary shrinks from 16.7 MB to about 9 MB. The profile is also set in `crates/tole-cli/Cargo.toml` so `cargo install tole-cli` gets it (the workspace-root profile is not packaged). `panic = "abort"` is deliberately not set: Mutex poison recovery and the serve/ACP/MCP daemons rely on unwinding.

### Fixed
- `--on-pretool`/`--on-posttool`/`--on-turnend` on a build without the `shell-tools` feature (`--no-default-features`) are now refused with a clear error instead of being silently ignored (the hook never ran and a Write tool executed under `--yes`).
- Pre/post hook timeout raised from 5 s to 30 s, matching the turn-end gates (#493): hooks may be verification gates (lint/tests), and a loaded host could exceed 5 s, so the timeout (a non-blocking failure by design) silently skipped the hook and turned a deny into an allow. This was the root cause of the #493 CI flake.
- A denied approval or `--on-pretool` exit 2 no longer strands a headless caller (#476, owner decision 2026-11-03). The turn still parks in Planning, so a bare `tole resume <id>` is unchanged, but `tole resume <id> "<prompt>"` now settles the parked turn and runs the new prompt (it was refused with `run_turn requires pc Idle or Final`), which makes the `--on-pretool` help text true. The denial is now visible to the model's next request through a synthetic user-role `denial_note` entry (marked like the stop-gate feedback, never opens a turn); it was an orphan error entry the wire dropped, so a policy deny was asked for again and again. A denied replay of a crash-interrupted effect now says `the outcome is UNKNOWN: the effect may have run before the interruption; check state before retrying` instead of reading like a user refusal. Denied effects still never execute; the loop guard and step budget are unchanged.
- A non-stream completion with empty content and no tool calls now fails with `malformed response: empty stream (no content, no tool calls)`, like stream mode, instead of returning an empty `Final` with exit code 0 (#478).
- A provider HTTP error now carries its body: `http status: 400 Bad Request — <scrubbed 200-char snippet>`, plus `(retry-after: N)` when the header is present (#474). The chat-completions client no longer lets ureq drop the body of a 4xx/5xx (`http_status_as_error(false)`, that request only); the `/models` probes keep their `Err` on non-2xx. The `http status: N` prefix is unchanged and a snippet never changes retry or failover classification.
- Streaming provider requests reuse one connection (#479): `complete_streaming` stops at `data: [DONE]` and left the chunked terminator unread, so every streamed request opened a new TCP+TLS connection (26 requests, 26 connections; non-stream mode used 1). After a clean end of stream the rest of the body is now read to EOF on a detached `tole-sse-drain` thread, so the turn never waits for it. At most 2 drain threads live per provider (a server that never ends the body parks a thread and socket until the agent timeout); past that, and after an error, the reader is dropped and the connection closes as before. No change to what `complete_streaming` returns.
- A second process can no longer write to a session another process has open (#475): a writable `JsonlStorage` holds an exclusive advisory lock on a sidecar `<id>.jsonl.lock` (survives `compact()`, released by the OS on crash), taken before the torn-tail repair, and `resume`/`chat --resume` against a held session fails with `session is open for writing by pid N` instead of appending stale records (duplicated seq, unreadable session). Each append also checks the file length against the last known good length (new `StorageError::StaleHandle`); `StorageError::Locked` is new. Read-only commands (`status`, `sessions`) are unaffected.
- Approval prompts now also escape the Arabic letter mark (U+061C), the Mongolian vowel separator (U+180E), the word joiner and invisible math operators (U+2060–U+2064) and the deprecated format characters (U+206A–U+206F); they could visually reorder or hide text (pre-tag scan of 0.9.0).
- The API key is redacted from replayed tool-call arguments on the wire, as it already was from message text and tool results (E11).
- Compaction with `keep_turns` of 2 or more no longer cuts before the previous summary, which made the summary call replay the raw transcript and drop that summary. Only embedders could hit it; the CLI keeps one turn.
- `tole sessions`, `sessions --all`, `tole status` and session-name lookup replay session files read-only (`JsonlStorage::open_read_only`): they no longer truncate a torn last line that another process may still be appending.
- `agent_poll` and the concurrent-agent cap no longer treat a recycled pid as the agent: a live pid whose command line does not name the agent (every child is launched with `--sessions-dir <agents>/<id>/sessions`) is another process, so a dead child whose number was reused no longer holds a cap slot or reads as "running" forever (pre-tag scan of 0.9.0). Identity, not clocks or executable names: a wall-clock age check would read a live agent as dead after a clock jump, and a launcher changes the process name. An unreadable command line keeps the plain liveness check.

## [0.9.0] — 2026-10-28

Includes the 0.8.2 hardening patch below, which was never tagged or published on its own.

### Added
- Context compaction, wired (#440, 2/2): `chat`, `resume` and `mission` compact between turns once a prompt reaches `--compact-at` (default 0.8; config `compact_at`) of the model's context window. The window is `TOLE_CONTEXT_WINDOW` / config `context_window`, else what the provider's `/models` reports for the session's model (`context_length`, `context_window`, `max_context_length`, `max_model_len`, `max_input_tokens`, `top_provider.context_length`) — looked up lazily, only once a prompt passes 8k tokens, at most once per process; with neither, compaction stays off and says so once. A failed summary call is reported and the session continues uncompacted. `--no-compaction` / config `no_compaction` turns it off; `tole status` lists each compaction; `tole config check` shows the effective values and their sources.
- Context compaction, core (#440, 1/2): `tole_core::compaction`. `compact()` summarizes the settled turns before the kept tail with the session's provider and commits ONE `compaction` entry (summary, first kept entry, compacted count, prompt size before) together with the summary call's usage row — atomic, append-only, the transcript is never rewritten. `view()` is what the provider sees: the transcript as is, or the latest summary (inside the untrusted-content fence used for tool results, introduced as a record rather than instructions) followed by the kept tail; the turn loop sends `view()`. `due()` compares the last call made AFTER the last compaction against `Policy { window, at, keep_turns }`, so the summary call never re-triggers it. Inactive until the CLI wiring (window resolution, `--compact-at`, `--no-compaction`, mission/chat/resume hooks, `tole status`) lands.
- Global session index (#439): every face that starts a session (`run`, `chat`, `mission`, serve/ACP/MCP) appends one line to `$CODECORA_HOME/tole/sessions-index.jsonl` (default `~/.codecora/tole/`, owner-only, append-only, a bad line is skipped): id, absolute session file, cwd, face, start time — never prompt text. `tole sessions --all` lists sessions from every project newest first, with their cwd and first-prompt preview read from the session file (gone files are hidden); `--grep <text>` filters on prompt, name or cwd. `tole resume <id>` for a session of another project refuses with the exact command to run there (`cd <project> && tole resume <id>`): resuming from here would apply this directory's workspace jail, config and trust to that session. `TOLE_NO_SESSION_INDEX=1` turns recording off.
- Command-level allow/deny patterns (#438): `run_command:<argv pattern>` in `--allow` / config `allow` matches `run_command` calls by the argv the tool executes (same splitter, no shell, so `;` never chains a second command): one glob per argument, a final lone `*` = any further arguments, the program compared literally for an allow (`run_command:*` allows nothing). A leading `!` makes any pattern a deny that wins over every allow and over `--yes`; a deny compares the program by file name, case-insensitively, without `.exe`/`.cmd`/`.bat`/`.com` (`/bin/RM`, `rm.exe` vs `rm *`). Fail closed: while a `run_command` deny is configured, a command runs unattended only when an explicit `run_command:` allow names it (a bare `run_command` allow or `--yes` is not enough) and never when its program is a known wrapper (`sh`, `bash`, `env`, `sudo`, `xargs`, ...); everything else asks the human (refused where nobody can be asked). Denies travel with allow lists, a child agent always inherits its parent's denies, and a child's command pattern must be covered argument by argument by a parent pattern (`list_covers`: under a parent command deny only a parent `run_command:` allow covers it; a child wildcard only by the same glob or a lone `*`). In `AllowlistApprover::deny_only` (no allows), a `run_command:` deny refuses every `run_command` call. `tole_core::approval::pattern_verdict` is the one evaluation both approvers use.
- `--trust workspace` (#437): `read_only` plus `write_file` / `edit_file` through a path guard. The tools are jailed to the workspace root (canonicalized, so `../` and symlinks cannot escape), and the guard keeps files another tool may later execute prompting: any dot-component (`.git/hooks`, `.envrc`, `.vscode/`, `.tole/`, `.github/`), build and task files (`build.rs`, `Cargo.toml`, `Makefile`, `package.json`, `pyproject.toml`, `SKILL.md`, `AGENTS.md`, ...) and scripts (`.sh`, `.ps1`, ...). `delete_file` (Destructive) and `run_command` keep prompting; the preset needs an explicit `--workspace` (or config `workspace`) and refuses a home or filesystem root. Not a sandbox: ordinary source files stay writable. The guard checks the RESOLVED target the tool will write (relative to the canonical jail root, symlinks followed; Windows trailing dots/spaces normalized, 8.3 short names refused), never the raw path text. It is a pattern suffix (`write_file@workspace`) matched by `tole_core::approval::pattern_allows`, so `--allow` and child agents use it too. **API:** `Tool::approval_target` (default `None`; decorators must forward it) and `ToolRequest::target` are new.
- `web_search` backends beyond the fleet contract (#436): `TOLE_WEB_SEARCH_METHOD` (`GET` default, or `POST` with the JSON body `{"query", "limit"}`), `TOLE_WEB_SEARCH_HEADERS` (`Name=Value` pairs, `${VAR}` expanded from the environment, sent only to the backend; transport headers and line breaks refused; errors never echo a value) and `TOLE_WEB_SEARCH_FIELDS` (`wire=backend`, e.g. `snippet=description,results=items`). A malformed value fails startup instead of dropping the tool. **API:** `tole_core::web::WebSearchTool::from_env` now returns `Result<Option<Self>, String>`; `WebSearchTool::new(url)` and `from_lookup` are new.
- User config (#433): `$CODECORA_HOME/tole/config.toml` (default `~/.codecora/tole/config.toml`) holds global defaults with the project config's keys and validation. Precedence is flag > env > project config > user config > default, key by key; it needs no trust record, `--no-config` ignores it too, and `tole config check` shows both files and the winning layer per key (`(user config)`).
- `TOLE_SKILL` (#434): preload skill files from the environment (`SKILL.md` paths, separated like `PATH`) — the env layer of the `skill` key, so `--skill` still replaces it and it replaces the config's `skill` list. Server faces refuse it like `--skill`, and the refusal names the variable.
- Code review with cora (#422, 3/3): when `cora mcp` (cora ≥ 0.19) is attached, the system prompt of `tole run` / `tole chat` names the agent-as-reviewer flow — `mcp_cora_cora_review_prepare` with the diff, the model's own judgement, then `mcp_cora_cora_review_finalize` with the same diff and its findings — so "review this change" uses cora's deterministic checks and verdict without an LLM key in cora. Absent in plan mode and without the review tools.
- Supported sibling versions (#422): cora ≥ 0.19.0 and uteke ≥ 0.21.1, kept in one place (`tole_cli::ecosystem`). `tole doctor` prints an `ecosystem:` section reporting each binary as supported, too old (with its upgrade command) or not installed; it never blocks, since both are optional.
- Provider failover chain (#209): `failover::FailoverProvider` retries a step on the next provider of an ordered chain when the active one is unavailable (connection failure, timeout, dropped stream, HTTP 5xx or 429); request errors (401/403/404/400, malformed responses) fail loud and never fail over. The answering provider stays active and the primary is retried after a cooldown. Each step records the route under `tole_route` in its usage row (durable, auditable); rows holding only tole's own keys are not counted as provider steps. Wired into every session face through `TOLE_FALLBACK_BASE_URL` / `TOLE_FALLBACK_API_KEY` / `TOLE_FALLBACK_MODEL` (a reserve without its own key is refused; the primary's key is never sent to another host). A stream that breaks after part of the answer was already shown live does not fail over (no duplicated output); a reserve that fails before the cooldown ends wraps back to the primary; and after a failover only the last provider's error decides the turn loop's transient retry, so an earlier timeout no longer makes a reserve's 401 look retryable.
- Child-agent handoff schema and verify gate (#383): `tole_core::handoff` defines the typed handoff document (role `explore` / `implement` / `verify` / `fix`, artifact references by path and rev, verification evidence), strict validation of the model-authored input (workspace-relative paths, option-safe refs, bounded fields), the close gate (an implement/fix child without passing verification is `Unverified` / `VerifyFailed`, never closed silently) and the prompt for the follow-up `fix` child. `agent_start` takes `role` and `consumes` (artifact references the child reads itself), a role child gets a `handoff_submit` tool, and `agent_poll` reports the gate and a ready-made `fix_prompt` (the parent decides whether to start the fix child; nothing is spawned automatically). Design and anti-goals (no DAG engine) in `docs/child-agents-handoff.md`.
- Sibling change notices, core (#380): `sibling_watch` tracks which files each child agent reads and writes in one shared append-only log (owner-only, bounded) and tells a child, once per stale path, when a sibling changed a file it had read or written; the `Tracked` tool decorator records `read_file` / `write_file` / `edit_file` / `delete_file` on success. Interrupts gain a `source`: a notice is delivered at the next safe point as an attributed entry (`source: notice`) and never withdraws output or supersedes a final answer. Wiring into `tole run` children follows separately.
- Sibling change notices (#380): `sibling_watch` tracks which files each child agent reads and writes in one shared append-only log (owner-only, bounded) and tells a child, once per stale path, when a sibling changed a file it had read or written; the `Tracked` tool decorator records `read_file` / `write_file` / `edit_file` / `delete_file` on success. Interrupts gain a `source`: a notice is delivered at the next safe point as an attributed entry (`source: notice`) and never withdraws output or supersedes a final answer. Children spawned by `agent_start` get `TOLE_AGENT_ID` and a shared `TOLE_AGENT_TOUCH_LOG`; in a child, `tole run` wraps its file tools and runs the watcher, so a sibling's change reaches the model at its next safe point.
- `tole doctor` coverage ledger (#378): every run is recorded per provider + model (level reached, first blocker, fix command, timestamp) in an owner-only `doctor-ledger.json` (atomic write, a corrupt file is an error, never replaced); `--summary` prints `READY n-of-m` + the first blocker and its fix, `--no-ledger` skips recording.
- Soft interrupts (#377): a bounded queue on the session's `CancelToken` that the turn loop drains at the safe point between a settled tool and the next provider call, committing each message as an attributed user-role entry (`interrupt: true`). An `urgent` interrupt (or any interrupt, for a final answer) withdraws an output that has not taken effect, so no tool call is left without its result, and records the drop durably. Host entry points: `POST /sessions/{id}/interrupt` (`tole serve`) and the `tole_session_interrupt` MCP tool (`409`/error when no turn is running).
- `tole doctor` (#378): tiered provider diagnostics — `offline` (wiring), `catalog` (live `/models`, model must be served) and `full` (chat, streaming and a real tool-call loop) — each blocker carries its fix command, and keys / URL credentials are never printed. Exits non-zero when the requested tier is not reached; `--tier full` is the release-validation gate.
- Per-user-turn token stats (#379): `tole status` lists input / output / cache-read / cache-write / reasoning tokens per turn and the status JSON (`GET /sessions/{id}/status`, `tole_session_status`) gains an additive `usage_report.turns` array. Derived from the durable usage ledger, so old sessions read the same way and nothing is stored twice; a value no step reported is `n/a` / `null`, never 0, and no elapsed time is invented.

### Fixed
- A child agent running `tole chat` or `tole resume` now receives sibling change notices (#380) like `tole run` does: both faces run their turns on a session cancel token with the child's watcher attached (#428).
- `tole resume`, `tole chat --resume` and resumed missions now use the session's own model (the durable `fact/model` register set by ACP's model picker) instead of falling back to `TOLE_MODEL`; only the server faces honored it before (#428). The missing-provider-config error is one clean line on every face (the server faces printed a run of spaces inside it).
- `tole status` and `tole sessions` no longer count the stop gate's own feedback message as a user turn (#418).
- `tole doctor`: concurrent runs no longer overwrite each other's ledger entry (load/record/save now happen under an exclusive lock on `doctor-ledger.json.lock`); the catalog tier's `GET /models` gets its own 20 s timeout instead of the ACP picker's 3 s convenience timeout, so a slow but healthy gateway is not reported as a blocker; and the offline tier now requires a host in the base URL (`https://?x` or `https://u:p@/v1` no longer pass).
- Sibling change notices: a child's watcher starts at the end of the shared touch log instead of folding every earlier child's history on start-up.
- The optional `web_search` tool is registered with `if let` instead of an `is_some()` check followed by `unwrap()`.
- `tole serve --transport mcp` is hardened to parity with the REST transport: the bearer token is compared in constant time and its scheme is case-insensitive; failed authentication is rate-limited per source IP (10 per minute, then `429`, same policy as REST); request bodies over 1 MiB are refused with `413` (declared or streamed, never buffered whole); a transient `accept()` error is logged and no longer ends the server and every session; and the no-op `timeout()` around the auth check is gone. The token comparison, header matching and limiter now live in one shared `auth` module used by both transports.

### Changed
- Quiet startup (#435): the probe misses `uteke binary not found`, `cora binary not found` and `no GitHub origin remote detected` are no longer printed on every run; set `TOLE_VERBOSE=1` to see them (`tole doctor` reports cora and uteke regardless). A configured MCP server that registers no tools is still reported.
- Child-launch contract (#443): `tole_core::child_env` is the one definition of the environment `agent_start` gives a child (`TOLE_AGENT_DEPTH`, `TOLE_AGENT_ID`, `TOLE_AGENT_TOUCH_LOG`, `TOLE_AGENT_ROLE`, `TOLE_AGENT_HANDOFF`, the mailbox room and memory backend): the parent writes it with `ChildLaunch`, and the child reads it back with `depth` / `sibling` / `role` (sibling tracking, the handoff tool, the depth cap). No behaviour change.
- Session registry (#441): `session_host::Sessions` owns the live session map — capacity and eviction, the admission rule (`admit`: replace an idle id, refuse a busy one, evict for a new id) and the per-turn busy claim (`claim_turn`). `tole serve`, `tole acp` and the MCP session tools use it instead of each manipulating the map; the map itself is private (tole-cli API change). No behaviour change.
- Session turn runner, part 3 (#428): `turn_runner::cli_turn_token` gives every CLI face (run, resume, chat) the same cancel token plus child sibling watcher; resume and chat now run their turns with that token.
- Session turn runner, part 2 (#428): the memory loop's pre-turn recall and session remember live in `turn_runner` and every face uses them. Server-face sessions now report a failed or successful remember on stderr instead of dropping the result silently.
- Session turn runner, part 1 (#428): one `turn_runner` module builds the provider for a session turn (env config, the session's model, tool specs, pinned system prompt, live-token sinks) for run, resume, chat, missions and the server faces, replacing six copies.
- Tool roster (#420, 3/3): the task-list tools are registered by one `tool_roster::register_todo` (hydrated from the transcript; `todo_write` absent in plan mode) instead of five copies in run, resume, chat, mission and the session host.
- Tool roster (#420, 2/3): server sessions (`tole serve`, `tole acp`, the MCP session tools) build their tools through the same `tool_roster` as the CLI, replacing the third copied builder. They gain `verify_package`, the strict binary probe (executable files only), and sibling tracking plus the handoff tool when running as a child agent; probe misses are reported on stderr. MCP servers on server faces remain a separate follow-up.
- Memory loop on uteke rooms (#422, 2/3): `--memory uteke` stores session summaries in the room `repo-<dir>` (`uteke remember --room`) and recalls with `uteke room recall` instead of the namespaces uteke 0.21 deprecates; while the room is empty, recall falls back to the legacy namespace of the same name, so memory from earlier versions is still found. Child-agent mailboxes are rooms too (`agent-<id>`); consuming one forgets its memories, deletes the room and cleans any legacy namespace. `MemoryConfig.namespace` is renamed `room` (tole-core API).
- `tole_core::transcript` (#418): one read-side view of the transcript conventions the turn loop writes — which user messages open a turn (not stop-gate feedback, not interrupts), which usage rows are provider steps (not rows holding only `tole_*` keys) and the token total. The turn loop, `tole status`, `tole sessions`, usage reports and mission budgets all use it, and embedders can count turns and steps without copying the rules.
- Tool roster (#420, 1/3): the CLI faces (`run`, `resume`, `chat`, `mission`) and the MCP server faces (`tole mcp`, MCP-over-HTTP) build their tools through one `tool_roster` module instead of two copied builders. Plan mode is applied in one place, and there is one binary probe (`binary_available`, executable files only) and one GitHub-origin probe. The MCP server faces now report a missing `uteke` / `cora` binary or GitHub origin on stderr like the CLI does.
- Memory loop injection hygiene (#384): the recalled batch is hard-capped at 24 items / 64 KiB and deduplicated; a malformed recall payload (non-array, or a hit without content) now fails the whole batch visibly instead of silently injecting fewer items; `tole serve`/MCP sessions report recall failures on stderr like the CLI does.

## [0.8.2] — 2026-10-12 (never tagged; shipped in 0.9.0)

### Fixed
- `tole serve` HTTP hardening (#361): header names are matched case-insensitively (a lowercase `authorization:` was refused), `Expect: 100-continue` is answered (curl no longer stalls ~1s on POST bodies over 1 KiB), reading a request has a 15s total deadline (a client trickling bytes can no longer pin a connection slot), an oversized body gets `413`, duplicate/invalid `Content-Length` and `Transfer-Encoding` get `400`, the bearer token is compared in constant time, a poisoned auth-failure lock no longer disables authentication, and `202`/`429`/`503` carry their real reason phrases.
- A client cancel that lands while the fresh approval of a crash-interrupted guarded intent is pending now settles the resumed turn as `Cancelled` instead of `ApprovalRequired`, like the fresh path (#361).
- `tole mission`'s verify step reports the real cause when the verify command cannot be run (spawn or wait failure) instead of calling every error a timeout (#361).
- MCP `tole_session_new` roots the session at the canonical path its workspace-jail check validated (and returns it as `workspace`), and checks session capacity before creating the session file, so a full server no longer leaves an orphaned session on disk (#361).
- `tole serve` `POST /sessions` checks capacity before creating the session file, so a `503` no longer leaves an orphaned session on disk, and the request line now counts against the same 32 KiB ceiling as the headers (#368).
- `tole mission --resume` accepts only a plain session id (letters, digits, `-`, `_`) instead of letting a path escape the sessions directory, and an absurd `--max-minutes` is capped at 10 years instead of wrapping the deadline (#368).

## [0.8.1] — 2026-10-09

### Fixed
- The usage report no longer renders an absent per-step `messages` count as a measured `0`; it is `null` in the status JSON when unknown (found by the pre-tag scan).
- `read_file` rejects an empty `path` with a clear message instead of a confusing "is a directory" error.

### Added
- Per-user-turn token stats (#379): `tole status` lists input / output / cache-read / cache-write / reasoning tokens per turn and the status JSON (`GET /sessions/{id}/status`, `tole_session_status`) gains an additive `usage_report.turns` array. Derived from the durable usage ledger, so old sessions read the same way and nothing is stored twice; a value no step reported is `n/a` / `null`, never 0, and no elapsed time is invented.
- `tole status` and the serve status JSON now report token usage, cache-hit rate and a per-step request-size split (system+tools vs history, stored per step under `tole_wire` in the usage record); `fact/mission` gains `cached_tokens`. Instrumentation only: the wire, budgets and durable entry shapes are unchanged (#211, part 1 of 3).

### Changed
- `read_file` now returns at most 20,000 characters per call and accepts `offset`/`limit` (characters); the result carries `truncated`, `total_chars` and `next_offset` so large files are read in chunks instead of being re-sent whole on every later step (#211, part 3 of 3).

## [0.8.0] — 2026-10-09

### Added
- Project config `.tole/config.toml` (#208): per-directory defaults for `model`, `base_url`,
  `system_prompt`, `memory`, `sessions_dir`, `workspace`, `trust`, `allow`, `mcp_server`, the
  hooks (`on_pretool`/`on_posttool`/`on_turnend`), `skill`, `plan_mode`, `no_auto_mcp`,
  `no_skills` and the `[mission]` table (`max_steps`, `max_minutes`, `max_tokens`, `verify`,
  `verify_timeout`). Precedence is flag > env > config > default; list keys are replaced
  wholesale by the higher layer and booleans can only be turned on by flags. The file is
  untrusted input from the repo: it applies only after you trust it (`tole config trust`;
  content-bound, stored outside the repo in `$CODECORA_HOME/tole/trusted-configs.json`).
  Interactive commands ask `[y/N]` showing the full content and a diff; `serve`/`acp`/`mcp` and
  non-interactive runs fail closed. `--config <path>` names a file explicitly (trusted) and
  `--no-config` ignores it. Secrets (`api_key`, tokens, ...) are rejected, `Destructive` tools
  can never be allowlisted through the config, and faces that refuse hooks, skills or MCP
  servers refuse them from the config too.
- `tole config check|trust|untrust`: validate the file and show every effective value with its
  source (flag/env/config/default); trust or untrust the current content.

### Changed
- CI: the bare `--no-default-features` profile of `tole-cli` is now tested (not only checked), and the ACP/serve e2e tests are gated on `shell-tools` (#349).
- `--skill` / `--no-skills` on `tole serve`/`acp`/`mcp`/`mission` are now refused in builds without the `mcp` feature too (they were silently ignored there), and the refusal also covers values coming from the project config (#346).

### Fixed
- The `gh` tool is only targeted at a remote whose host is exactly `github.com`: the string no longer matches inside a longer hostname (`github.com.evil.io`), the userinfo or another forge's path, and `host:port` URLs parse correctly (#352).
- `tole approvals` now honors `TOLE_SERVE_URL` (precedence: --url > env > default http://127.0.0.1:7801) as its help text always claimed (#339).
- CodeCora home is resolved in one place: skills and the update-check cache no longer fall back to the current directory when HOME is unset, and CODECORA_HOME now means the same thing everywhere (`$CODECORA_HOME/tole/...`; the update-check cache moves from `$CODECORA_HOME/.codecora/tole` to `$CODECORA_HOME/tole`) (#344).

## [0.7.2] — 2026-10-08

### Fixed
- systemone_decide surfaces the provider's 4xx/5xx error body (bounded) instead of "no error body" (#328).
- tole-cli: the bare `--no-default-features` profile now compiles (#330); CI now
  checks that profile and `tole-core --no-default-features --tests` so neither can regress.

### Changed
- Dependency updates: `rmcp` 3.4.1 → 3.5.0, `tokio` 1.53.1 → 1.53.2,
  `libc` 0.2.189 → 0.2.190, `uuid` 1.26.1 → 1.27.0 (lockfile only).
- Docs: the `write_file` parent-component symlink race is now documented as a
  residual limitation and `git status`/`diff` are documented as repo-wide by
  design; the previous "TOCTOU-safe" wording overclaimed (#329).
- CI: Dependabot/Renovate PRs are exempt from the Branch Naming and PR
  Description checks (#322).

## [0.7.1] — 2026-10-08

### Changed
- Approval prompts now show control characters, bidi overrides and line separators in tool descriptions as visible escapes (shared sanitizer in ToolRegistry::decide) (#327).
- Unknown-tool aborts (fresh and guarded replay) now settle the turn to `Final` like every other abort (#316). A bare `tole resume <id>` after one reports "nothing to resume"; send a new prompt instead. The Tier 1 abort-path contract moved from "parks at Planning, resume continues" to "settles to Final, next prompt continues".
- `serve` approval-resume now honors `/cancel`, and `BudgetExhausted` maps to `max_tokens` there too (#318).
- Tool specs sent to the provider now use a static `Tool::summary()` (defaulted method) instead of `describe(Null)`; git and systemone were being advertised with validation error strings (#320).
- The MCP server path (`RegistryServer::execute_checked`) now authorizes tool
  calls through the same crate-internal gate as `drive`/`resume_turn`.
  Behavior change for `RegistryServer::new` embedders: pre-hooks configured on
  an embedder-supplied registry are now enforced there (a configured deny-hook
  must not be bypassable on one path). No in-repo face is affected, since
  none can attach pre-hooks. Destructive refusal and error strings are
  unchanged (#303, part 3 of 3).
- Internal refactor, no behavior change: `drive` and `resume_turn` now share
  one crate-internal tool-call authorization gate (`gate.rs`) with a typed
  denial and a `Permit` required to execute non-ReadOnly tools; tool risk is
  read once per call. Durable entry shapes and error strings are unchanged
  (#303, part 2 of 3).
- **Behavior change:** `agent_poll` is now `Risk::Write` (a successful poll
  consumes the mailbox: it writes `meta.json` and forgets mailbox memories),
  matching the rule that a ReadOnly poll never mutates. `--trust internal`
  allowlists the exact name `agent_poll`, so poll loops stay prompt-free and
  `agent_start` still prompts; users on the `read_only` preset who approved
  `agent_start` are now prompted on every `agent_poll`; plan mode drops it
  together with `agent_start` (#300).

### Fixed
Rescan-2 MAJORs (#276–#287):
- Security: server-supplied text is sanitized before it reaches MCP approval
  prompts (#286); the subprocess env scrubber matches SECRET/TOKEN/
  PASSWORD/API_KEY-shaped names by substring (#284, `TOKENIZERS_PARALLELISM`
  kept); memory `recall` passes the query after `--` (#280); a failed or
  timed-out request-body read on the MCP HTTP face is a 400, not an empty
  body (#277); MCP registration sync waits longer than the serial reactor's
  worst-case queueing (#285).
- Redirects: `web_fetch` resolves relative `Location`s per RFC 3986 against
  the current hop and refuses non-http(s) hops (#281).
- `git` rejects NUL bytes in argv with a clear error (#282).
- **Behavior change:** `AllowlistApprover::new(patterns, Deny|Ask)` now
  ignores `patterns` — the default verdict is final (fail closed). Embedders
  that used `new(p, Deny)` as an allowlist must use `allow_only(p)` (#283).
- **Behavior change:** the `web_fetch` SSRF guard now also refuses 100.64.0.0/10
  (CGNAT / Tailscale-style overlay networks), 0.0.0.0/8, 192.0.0.0/24,
  198.18.0.0/15, multicast (224.0.0.0/4, ff00::/8), reserved 240.0.0.0/4,
  site-local fec0::/10, and 6to4/Teredo addresses that embed a private IPv4.
  Fetching a host on a tailnet now requires `TOLE_WEB_ALLOW_PRIVATE=1` (#317).
- `tole acp`: a hanging `/models` endpoint no longer re-stalls every
  `session/new` (probe wait 60 s → 5 s, failures cached for 30 s, probe runs
  before the sessions lock is taken) (#319).
- `chat` registers and hydrates `todo_read`/`todo_write` (#276); an agent
  mailbox is marked consumed only after a successful recall (#287).
- Tests: the perf `resume_replay` gate actually replays 25 tool calls and
  asserts it (#278); `turn_loop` temp dirs are unique per call (#279).

Architecture-review follow-ups (#293–#299):
- The `gh` tool is registered only when a GitHub origin is detected; the
  hardcoded `codecoradev/tole` fallback on the run/chat/resume/mission and
  `tole mcp` faces is gone (#293).
- `tole serve --transport mcp` honors `--plan-mode` in its server-level
  registry (only ReadOnly tools remain) (#294).
- One session-id rule everywhere: `[A-Za-z0-9_-]`, at most 64 bytes
  (`tole_core::storage::is_valid_session_id`); an id accepted by serve/ACP
  is no longer rejected by `tole chat --resume` / `tole status` (#295).
- `tole acp` caps live sessions at 256 with non-busy eviction (shared helper
  with serve and the MCP session tools) and prunes the approval state of
  evicted sessions (#299).

## [0.7.0] — 2026-10-06

### Added
- `tole upgrade` + startup update notification (issue #220): a
  cache-backed (24 h) banner on startup when a newer crates.io
  release exists (`/releases/latest` redirect primary, API fallback,
  network failures silent, `TOLE_NO_UPDATE_CHECK=1` opts out), and
  `tole upgrade [--check] [--yes]` which resolves the latest version
  from crates.io and re-runs `cargo install tole-cli`, verifying the
  binary afterwards. Non-cargo binaries get an explicit note.
- `web_fetch` / `web_search` (#215): read-only internet access — fetch
  is direct HTTPS (512 KB cap, content-type allowlist, HTML→text), search
  probe-gated on `TOLE_WEB_SEARCH_URL` (fleet backend contract). No
  backend, no tool.
- Run ergonomics (#216): `run --prompt-file <path|->`, `--name <alias>`
  (header-pinned; `tole sessions` shows it, `resume` accepts it), and
  `--timeout <secs>` wall-clock cap (checkpoint cancel — settles
  resumably, never dead).
- Mobile-control guide (#202): docs/mobile-control.md defines the REST
  surface uteke-mobile consumes (sessions, approvals, cancel, cost
  report) with the auth model and cross-repo acceptance; threat model
  gains the phone-as-approval-surface section.
- Remote approvals (#200): serve-face Write approvals become a queue —
  `GET /approvals` + `POST /approvals/{id}/decision` (allow = one-shot
  fingerprint + approvals-only resume; deny = recorded verdict), expiry
  to denied (15 min), durable audit registers, and a CLI consumer
  (`tole approvals list/allow/deny`). MCP/ACP parity follow-up.
- Mission budget tiers + cost report (#201): `--max-tokens` ceiling joins
  `--max-steps`/`--max-minutes`; conservative defaults with `--trust
  internal` headroom (explicit flags win); a durable cost report (steps,
  turns, tokens, wall time, tool-call counts by risk tier) lands on the
  session at every settle path and `tole status` renders it.
- `tole mission` (#199): budgeted autonomous turn-chaining toward a goal —
  `--max-steps` / `--max-minutes` budgets (exhaustion settles resumably),
  optional `--verify <cmd>` gate (exit 0 = completion; failures return to
  the model with output, 3 strikes settle `verify_failed`), durable
  mission summary on the session, `--resume <id>` continuation.
- `todo_write` / `todo_read` (#198): durable mission task list persisted as
  ordinary session entries — `todo_write` echoes the full list as its result
  (the durable record), state re-hydrates from the transcript on
  open/resume, at most one task `in_progress`; `todo_write` is Write
  (covered by `--trust internal`), `todo_read` is ReadOnly.
### Fixed
Pre-tag full-codebase scan gate (all MAJORs triaged valid and fixed):
- Per-session `TodoState` — todo list no longer leaks across concurrent
  serve/ACP sessions (#226).
- MCP server: session-registry-first routing, unknown session id fails
  closed, ambiguity refusal no longer bypassable (#227).
- Child agents: argv depth guard closes quoting/`env -i`/`exec -c`/
  substitution escapes (#228); spawn cap flock-serialized, mailbox
  consumed flag persisted and consume lock held end-to-end (#234, #260).
- Evals tier 2: judges require exit 0 and strip approval-banner echo (#229).
- `git` tool: stdout capped at 20k chars with a truncated flag (#230);
  colon pathspec magic refused in the add jail (#247).
- Turn loop: poll steps exempt from `MAX_STEPS`, trait-driven `is_poll`
  (#231); replay approval gate keyed to current tool risk (#249).
- OpenAI streaming: usage/reasoning reset per response (#232).
- `gitea` tool surfaces 4xx/5xx error bodies (#233).
- `verify_package`: length-guarded edit-distance, bare-name typo
  comparison (#235).
- Hardening: storage session-id charset enforced at the boundary (#248);
  web redirect cap is a hard error (#250); MCP result cap counts
  separators (#251); `gh` tool probe-gated, no hardcoded repo (#254);
  session list uses `try_lock` busy flags (#256); systemone caps keyed on
  exact URL authority; approval URLs refuse plaintext `https://`
  downgrade (#246); monotonic counter in approval entry ids; bounded
  detached ACP `/models` probe; `tole mission` exits nonzero on non-done
  statuses and refuses unsupported hook/memory flags.

## [0.6.0] — 2026-10-06

### Added
- `gitea` tool — the Gitea counterpart of `gh`, over the instance's
  REST API (`TOLE_GITEA_TOKEN` / `GITEA_TOKEN` + a Gitea `origin`
  remote; probe-gated: absent token or non-Gitea remote degrades to one
  warning, no phantom tool). Same six ops as `gh` (issue_view /
  issue_list / pr_view read-only; issue_comment / issue_create /
  pr_create writes), Risk::Write through the approval gate, routes
  whitelisted in one auditable function, per-call `repo` override
  validated (`owner/name`, traversal/dash refused), token never shown
  in approval lines. Self-hosted instances with explicit ports are
  parsed from the remote (`http://host:3000/owner/repo`); GitHub
  remotes never register it (that's `gh`).
- `gh` hardening: `number`/`limit` accept JSON integers (models send
  counts as numbers); optional per-call `repo` override (validated,
  shown in the approval line) replaces the registration-time lock —
  closing the README "per-repo wiring is a known gap" note.
- `mcp-http` is now a **default feature** of `tole-cli`: a plain
  `cargo install tole-cli` / `cargo build -p tole-cli` exposes all four
  documented faces, including `tole serve --transport mcp` (previously
  opt-in — a registry install errored on that transport). Embedders are
  unaffected: `tole-core` defaults do not change, and the mobile
  cross-compile targets only build `tole-core`.
- `--trust` presets (#160): one-word trust for the fleet's ecosystem
  tools — `internal` (uteke_*/cora_search/mcp_cora_*/verify_package/
  job_*/tole_session_*), `read_only` (every safe read), `none`
  (default). Pure sugar over `--allow` globs; Destructive is never
  auto-allowed; unknown presets fail loudly; flag wins over the
  `TOLE_TRUST` env. (Landed on develop 2026-10-02, backfilled here.)
- SKILL.md support (#161/#162): `--skill <path>` loads a skill file
  into the system prompt (fenced, 16 KiB cap, loud frontmatter errors);
  discovery over `<workspace>/skills/` + `~/.codecora/tole/skills/`
  registers the ReadOnly `load_skill` tool with a one-line index;
  `--no-skills` disables everything. (Landed on develop 2026-10-02,
  backfilled here.)

- `systemone_decide` (#172/#173) — typed decisions (choice / score /
  noul + confidence) from any System One backend, active when
  `SYSTEMONE_API_KEY` is set; `SYSTEMONE_BASE_URL` picks the backend
  (hosted Jev default, self-hosted compatible). ReadOnly, approval-free.
- Depth-1 child agents (#171/#174): `agent_start` / `agent_poll` spawn
  and harvest durable child sessions from within one session —
  structurally no grandchildren (registry depth cap), results via
  ephemeral per-child uteke mailboxes, optional per-child git worktrees
  behind the parent-only `--agents-worktree` flag.
- ACP native model picker + per-session approval controls (#176/#177/#179):
  `TOLE_MODELS` (comma-separated ids) advertises a model switch via ACP
  session config options, persisted durably per session across resumes;
  an approval selector (`ask` / `auto`, session-scoped) plus an
  `allow_always` option on Write permission requests let the editor
  relax the gate without weakening the Destructive tier.
- Cooperative cancellation on all three server faces (#178/#184/#185):
  ACP `session/cancel`, REST `POST /sessions/{id}/cancel`, and the
  `tole_session_cancel` multi-session MCP tool. All paths set the same
  per-session token; the blocking turn observes it at checkpoints and
  settles `stopReason: "cancelled"` as a normal durable turn end.
  Spec-conformant: single in-flight tool calls are not interrupted,
  unknown sessions 404, cancelling an idle session is idempotent;
  `--trust internal` covers the new tool via the `tole_session_*` glob.

### Fixed
- **`TOLE_TRUST` env was documented but never read** (found by
  activation testing 2026-10-05): `--trust` help says "flag wins over
  the TOLE_TRUST env", yet no code read the variable — an env-only
  setup silently kept prompting for fleet tools. The env now applies
  whenever no `--trust` flag is passed (comma/whitespace separated for
  multiple presets; an explicit flag still wins wholesale; unknown
  presets in the env fail loudly, same as the flag).
- Full-feature sweep findings (2026-10-05):
  - **Silent no-op flags on the server faces** now fail loudly at
    startup (the scan-3 #9 rule, previously enforced only for
    pretool/posttool hooks): `--skill`/`--no-skills`/`--mcp-server` on
    `tole mcp`/`serve`/`acp`, plus `--on-pretool`/`--on-posttool` on
    `tole serve` (missed by the earlier bail) and `--on-turnend` on
    `tole mcp`. The cora AUTO-PRESET does not trip the `--mcp-server`
    refusal — only explicit flags do.
  - `tole serve` (both transports) and `tole acp` now honor an explicit
    `--sessions-dir` (default stays the per-session-cwd layout);
    `tole serve --transport mcp` now honors `--workspace` as the
    jail-of-jails root (default stays the server cwd); `--on-turnend`
    stop gates are wired into serve/acp session turns (the same
    registry-level gates the run host uses).
  - **Multi-session MCP ambiguity refusal** (the #138-documented
    behavior, previously unimplemented): a registry-tool call WITHOUT
    `session_id` while 2+ sessions are open is refused with a clear
    error instead of silently executing against the server-level
    registry (server-cwd jail).
  - Skills discovery now defaults to the CURRENT DIRECTORY when
    `--workspace` is absent — the same default the file-tools jail
    uses (a project with `<cwd>/skills/` used to silently get no
    discovery).
  - `--on-pretool` help text corrected: an exit-2 deny parks the turn
    resumably at the denial (mirroring an approval denial); it does
    not auto-replan in-flight.
- **MCP-over-HTTP accept-loop hardening (#190):** the `tole serve
  --transport mcp` face now matches the REST face's #136 Wave-2
  controls — a 32-connection semaphore cap (refused at capacity),
  a 30s header-read timeout on the HTTP/1 connection, and a 30s
  bound on the auth-header and request-body read phases. The
  connection stays on hyper's raw HTTP/1 builder (H1-only — no h2c
  sniffing) with hyper_util's TokioTimer wired via the builder's
  own `.timer()`: hyper panics per connection when
  `header_read_timeout` is set without a timer (caught by the
  pre-tag live smoke test, invisible to CI). SSE response
  streaming is intentionally untouched: long
  `tole_session_prompt` turns still stream incrementally; only
  connection establishment and request intake are time-bounded.
  Live-verified: 401 without a token, initialize over SSE, the
  h2c preface refused, the 33rd idle connection reset at the cap,
  recovery after release. Closes the slowloris
  task/fd-pinning class the full-codebase scan
  flagged as MAJOR (scan finding #17, pre-0.6.0-tag gate).
- Build/CI fixes riding the same train: the no-`mcp` `tole-cli`
  profile compiles again (`mcp_server_command` is now gated behind the
  `mcp` feature, #175/#180); project-sync board lookup resolves the
  runtime Done-option id instead of a hardcoded value, and the
  `set_done` mutation takes `optionId` as `String!` (the previous `ID!`
  call failed against the live Projects API) — failures are now loud,
  not silent (#181/#182, #183).

## [0.5.0] — 2026-10-02

### Added
- `verify_package` tool (#144, ReadOnly): checks a package name against
  the crates.io / npm registry before any install — the slopsquatting
  defense (hallucinated names get pre-registered with malware; 43% of
  hallucinated names recur, 13% are one character from a real package).
  Not-found answers include registry candidates; one-character
  candidates surface a typo-squat warning; 429/5xx honestly report
  `rate_limited` (never "not found"). The default system prompt tells
  the model to verify before installing.
- Turn-end stop gates (#145, `--on-turnend`, default OFF): deterministic
  script gates that fire when the model produces its final message,
  BEFORE it commits. Deny (exit 2) forces continuation — the reason
  becomes the model's next input and the loop re-runs (the Claude Code
  "stop hook / gate forces the model to fix it" pattern). Bounded: 3
  denials per turn then the turn settles durably as `StopGateBlocked`
  (prompt-resumable). GATE semantics: exit 0 = pass, ANY non-zero exit
  = deny with stdout as the reason (a verification gate's exit code is
  its verdict — cargo check exits 101, tests exit 1); only true
  infrastructure failures (spawn error, timeout) stay non-blocking;
  30s per-hook timeout. Payload carries the final-text preview (8 KiB)
  and the per-turn tool/risk summary.
- Memory-loop decision typing (#143): a session that executed any
  Write/Destructive tool is stored to uteke with `--type decision` and
  a `wrote` tag (plain context sessions unchanged). The flag is durable
  and session-scoped — set on fresh execution AND crash-replay of a
  Write, never reset by turn machinery — and derived from tool risk,
  not model claims. Summary content contract is unchanged: first
  prompt + final answer only.

## [0.4.0] — 2026-10-01

### Added
- Harness memory loop (`--memory uteke` / `TOLE_MEMORY`): on the first
  turn of a fresh session, memories relevant to the prompt are recalled
  from the owner's uteke store (namespace `repo-<dir>`,
  `TOLE_MEMORY_NAMESPACE` to override) and injected into the user
  message inside a clearly marked fenced block — the durable log stores
  exactly what the provider saw. When the session settles with a final
  answer, a compact summary is stored back (`--type context`, tagged
  `tole,session`). Host-initiated on both ends (the model cannot
  trigger or suppress it); every failure degrades to stderr and the
  turn proceeds without memory.
- cora auto-preset: when the `cora` binary is on PATH, tole attaches the
  local `cora mcp` server automatically — the full code-intel surface
  (brain search, callers, impact, affected tests, dead-code, review;
  registry names `mcp_cora_*`). Opt out per run with `--no-auto-mcp`;
  an explicit `--mcp-server cora=...` replaces the preset for that name.
  MCP tools keep the never-trusted trust model: `Risk::Write`, approval
  gate always applies.
- The `tole-cli` default feature set now includes the `mcp` client
  (`default = ["shell-tools", "mcp"]`): the auto-preset and
  `--mcp-server` work on a plain `cargo build -p tole-cli`. Embedder
  profiles are unaffected — `tole-core` defaults do not change.

### Added
- `tole serve --transport mcp` — **multi-session MCP over Streamable
  HTTP** (issue #137): one authenticated MCP connection addresses N
  durable tole sessions. Session tools (`tole_session_new`,
  `tole_session_prompt`, `tole_session_status`, `tole_session_list`)
  ride alongside the registry tools; a `session_id` argument routes a
  tool call to THAT session's registry (workspace jail + approver), and
  an ambiguous no-id call with 2+ open sessions is refused. Served with
  hyper directly (no axum) behind the same bearer-token auth as REST;
  Destructive stays structurally absent (non-interactive approvers).
- `tole serve`: tole as a **token-authenticated HTTP daemon** (issue
  #96, v1) — REST endpoints for the session host: create/list sessions,
  run turns, poll status. Turn execution is serialized per session
  (concurrent prompts get 409), the session map + durable JSONL live
  server-side, and the allowlist approver keeps Destructive tools
  structurally absent (a server has no human to ask). Zero new
  dependencies (hand-rolled HTTP/1.1). Per-connection read/write
  timeouts (30s). MCP-over-HTTP is the follow-up.
- `tole acp`: tole as an **Agent Client Protocol agent** over stdio
  (issue #95) — editors (Zed et al.) drive durable tole sessions:
  `session/new`/`session/load` map to the JSONL session store, prompts run
  full tole turns, the final answer streams as an `agent_message_chunk`,
  and Write/Destructive tool calls surface as
  `session/request_permission` requests — the editor human approves, with
  Destructive consent being a genuine per-call decision. Provider config
  is only required when a prompt actually runs. The session map survives
  across prompts (a first-run regression where fresh state replaced the
  map after one turn — caught by CodeCora review — is fixed, along with
  session-id path-traversal and mutex-poisoning hardening). The
  session-map lock is held only briefly — a running turn keeps its OWN
  storage lock, so the reader loop stays live for permission routing
  (the first implementation deadlocked protocol routing for up to the
  permission timeout whenever a client opened a session while a
  permission request was pending — caught by CodeCora review). Sessions
  reject concurrent turns (busy) and panic-safe un-busy via Drop.
- `tole mcp`: tole as an **MCP server** over stdio (issue #94) — the
  registry's hardened tools (jailed file ops, argv-validated git, detached
  jobs, memory loop, cora/uteke integrations) become callable by any MCP
  client. ReadOnly tools always callable; Write tools pre-authorized via
  `--allow` patterns (Destructive structurally absent — registration
  behind a non-interactive approver is refused). Verified live: 11 tools
  listed, read_file round-trip, write without `--allow` denied with an
  actionable message.

### Fixed
- **write_file had no wire schema**: it was the only registered tool
  without a `spec()` override, so providers received a property-less
  schema and could legally answer `arguments: {}` — every write failed
  with "missing 'path'" and identical retries tripped the loop guard
  (found live, GLM via bifrost). Spec declares path+content required;
  regression test pins it.
- CodeCora scan triage (2026-09-18, 54 files): 8 of the 10 MAJOR findings
  fixed — derived `Debug` on `OpenAiConfig` leaked `api_key` via `{:?}`
  (manual redacting impl); the uteke recall query could inject CLI flags
  (leading-dash guard); the uteke room-link spawn skipped env scrubbing;
  `job_poll` (ReadOnly) truncated/rewrote the job log (bounded read only —
  behavior change: runaway logs are no longer trimmed on poll; clear them
  as an operator); the MCP result cap was applied after a full block copy
  (incremental, single-oversized-block safe); subprocess capture is capped
  at 32 MiB per stream with a marked truncation suffix, and the drain no
  longer blocks indefinitely on a grandchild holding the pipe (2s grace —
  unterminated output is dropped); the `sh -c` payload scan now recurses
  and strips shell quotes (`sh -c "/bin/rm -rf /"` and nested-shell
  payloads are refused); `gh pr_create` actually passes the advertised
  `head` field (validated like `base`). Remaining MAJOR/MINOR findings are
  tracked on the scan-triage issue.
- docs: architecture.md no longer says the Destructive tier may be
  allowlisted (contradicted the never-allowlistable invariant).

### Fixed
- Remaining CodeCora scan findings (2026-09-18 sweep): `scrub()` no
  longer mangles text when the secret is empty; non-string tool-call
  arguments are serialized instead of silently replaced with `{}`;
  `OpenAiProvider` reuses one ureq agent (connection reuse) instead of
  building one per request; failed startups no longer leave a stray
  empty session file (registry/provider are built before the session is
  created); `resume <id> "<prompt>"` now stores the memory summary like
  `run`; chat states explicitly when a typed message was dropped after
  exhausted mid-flight retries; session ids use `strip_suffix` (a
  `x.jsonl.jsonl` file no longer yields an unusable id);
  `binary_available` honors the executable bit (unix) / `.exe`
  (windows); edit_file's approval line shows the actual old→new change;
  edit_file temp files are unique per attempt and legacy stale temps are
  swept; delete_file on a symlinked path removes the LINK, not the
  referent; job_start kills the spawned job when the pid file cannot be
  written; MCP tool-request descriptions truncate without materializing
  the whole payload, and the transport-error class shares one constant;
  tole-cli is now lib+bin so integration tests drive the REAL jailed
  tools and approver instead of drifting re-implementations; evals
  Tier 2 runner is Python 3.9-compatible, cleans its mkdtemp session
  dirs, uses a portable timestamp, and the baseline diff no longer
  flags newly-passing missions as regressions or skips zero baselines
  silently; threat-model ENV/JOB-2 rows updated to match the code.

### Changed
- `cora_search` follows the same startup-probing contract as the uteke
  tools: a missing `cora` binary degrades to a one-line warning instead
  of registering a phantom tool, and the native single-tool fallback is
  skipped when the cora MCP surface is attached.

## [0.3.0] — 2026-09-12

Reliability & long-running work: the post-soak optimization batch driven by
live E2E findings (2026-09-11 vetio missions + 2026-09-12 optimization pass).

### Added
- MCP client (`mcp` feature): connect external MCP servers over stdio
  via `--mcp-server name=command [args...]` and use their tools from
  the registry. Trust model: every MCP tool is Risk::Write (approval
  gate always fires — server metadata is never trusted), results are
  fenced like native tool output, server env is scrubbed (#74, #77).
- `--workspace <dir>` global flag — the file tools' jail root
  (read_file/write_file/edit_file/delete_file) is now configurable;
  defaults to the process cwd. Canonicalized and strictly validated; the
  TOCTOU-safe jail walk is unchanged (#57, #63).
- `job_start` / `job_poll` tools — detached long-running jobs (own process
  group, stdin null, stdout+stderr to `tole-jobs/<id>/log` inside the
  workspace) with ReadOnly liveness+log-tail polling; zombie states count
  as finished and reads are bounded to an 8 KiB window (#59, #56, #64).
- `tole resume <id> "<prompt>"` — continue a settled session with new
  instructions; headless multi-mission flows keep one durable session.
  Bare `resume` keeps the approvals-only recovery semantics (#55, #65).
- Usage ledger wiring: `Provider::last_usage` (default `None`) +
  `OpenAiProvider` response-usage capture; one durable `UsageRecord` per
  provider step anchored to the last committed entry — `tole status` now
  reports real token totals instead of 0/0 (#66).
- Live-mission binary hygiene rules in CONTRIBUTING.md (#60, #61).
- Agentic eval harness (issue #73, #76): Tier 1 deterministic contracts
  wired into a dedicated Evals CI job (wire-shape stability = prefix-
  cache contract, resume equivalence, abort-path resumability, approval
  matrix); Tier 2 live BYOK missions scored on success/steps/usage-
  ledger tokens; Tier 3 per-release baselines with a >25%-regression
  diff gate (`evals/`).

### Changed
- Retry classification: 429/rate-limit responses join timeouts as
  transient and get the one automatic per-turn retry; the durable audit
  entry is now "provider transient failure, retrying" (#58, #62, #66).
- The turn loop no longer clones the full transcript for every provider
  step; `complete()` borrows the storage slice (#66).

### Fixed
- `AllowlistApprover` constructor semantics (#70): documented the
  decision pipeline (pattern match → Allow; `default` applies only to
  NON-matches, so `new(vec!["x"], Decision::Deny)` ALLOWS "x") with a
  regression test, and added unambiguous `allow_only`/`deny_only`
  constructors — the misleading shape that let the guarded-replay
  livelock test look correct for months.
- Replay safety derives from tool risk (#68): a crash mid-sandwich no
  longer blindly re-executes a Write/Destructive tool on resume —
  Guarded intents require fresh approver consent, and a denied/absent
  approver settles the intent instead of livelocking the session.
- File-tool jails validate by path components (#68): Windows
  drive-prefixed/rooted relatives can no longer escape via `join`, and
  benign names containing ".." are no longer falsely rejected.
- JSONL torn-line recovery (#68): a newline-terminated corrupt final
  line is truncated on open instead of surviving and permanently
  bricking the session on the next append.

## [0.2.0] — 2026-08-27

Chat-first replan (v1.1): tole becomes a durable conversational harness —
the chatbot core for the uteke-mobile integration (all-Rust, runs inside
Flutter via flutter_rust_bridge, LLM BYOK).

### Added
- `tole chat` — durable multi-turn REPL: one session file, many turns;
  `/exit`, `/status` commands; `--resume <id>` / `--last`; state-aware
  dispatch that resolves interrupted turns (bounded retries) so a failed
  turn never wedges the conversation (#42).
- System prompt (B2): `--system` flag > `TOLE_SYSTEM_PROMPT` env > none;
  pinned in the session header, replayed on every resume — persona
  survives process restarts (#43).
- `tole sessions` — directory listing (id, pc, seq, turns, mtime) and
  enriched `tole status` (turns, pinned-prompt indicator, usage totals) (#44).
- `run_command` tool — generic dynamic commands: shlex-style argv split
  (no shell), cwd-jailed, hard timeout, `Risk::Write` ceiling; approval
  prompt shows the exact argv (#45).
- uteke first-class tools: `uteke_recall` (room-scoped; renamed from
  `uteke_search` so the name matches the verb — the old spec advertised
  a `room` parameter that was never wired) and `uteke_document`
  (markdown → document → room link, content via stdin) (#45).
- Startup probing: CLI-backed tools register only when their binary
  exists — no phantom tools (#45).
- `shell-tools` feature gate: subprocess-backed tools compile out for
  embedders (`--no-default-features`) — mobile/FFI profile (#47).
- `git` tool: status/diff/add/commit with path-jail validation and a
  dedicated 120s commit timeout (pre-commit hooks) (#41).
- `gh` read ops: `issue_view`, `issue_list`, `pr_view` (#39).

### Fixed
- Chat mid-flight resolve no longer drops the user's freshly typed
  message when the interrupted turn fails to clear (#42 follow-up).
- `uteke_document`/`uteke_recall`: pipe-deadlock on large documents and
  missing subprocess timeout (new shared `run_with_timeout_stdin`
  helper); room/slug argv-flag-injection validation (#45 follow-up).
- `git add` path jail: absolute paths and `..` traversal rejected (#41 follow-up).

### Changed
- Environment contract: `TOLE_*` canonical, `OPENAI_*` fallback;
  `CORAGENT_*` removed entirely (pre-release cleanup) (#36–#38).
- Removed unused `tokio` and `rusqlite` dependencies from tole-core —
  the core is synchronous by design and storage is JSONL (#46).

## [0.1.0] — 2026-08-26

Initial release: durable agent harness foundation.

### Added
- Write-once JSONL session storage with crash-safe replay (torn-tail
  truncation), state machine, effect sandwich (intent → effect →
  settlement), approval gates with risk tiers (ReadOnly / Write /
  Destructive), scoped pre-auth allowlists (`--allow`, E6/A1),
  OpenAI-compatible provider with tool calling, crash-resume,
  step-budget and loop guards, secret redaction on the wire,
  file tools (`read_file`, `write_file`, `edit_file` with hashline
  anchoring, `delete_file`), `cora_search`, E8 MVP gate passed.

