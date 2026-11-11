# Profiles, agent collaboration and secret isolation — research note

Status: **research + proposal, nothing implemented.** Companion to
[automation-design.md](automation-design.md). Vocabulary (module, interface,
seam, adapter, depth) follows the codebase-design glossary.

## Part A — Multi-profile / multi-agent collaboration

### A1. What tole already has (do not rebuild)

| Capability | Where | Shape |
|---|---|---|
| Child agents, depth 1, ≤ 4 live | `crates/tole-core/src/agents.rs` (#171) | `agent_start`/`agent_poll`; each child is a full durable `tole run` session |
| Per-child **uteke mailbox room** (`agent-<id>`) | `child_env.rs`, memory loop | the child's settle summary is the only result channel back up |
| Typed handoff roles | `handoff.rs` (#383) | role child writes a handoff document |
| Sibling change notices | touch log (#380) | siblings see each other's file touches |
| Per-child git worktree | `--agents-worktree` | merge-back stays human |
| Caller-side orchestration | [orchestration.md](orchestration.md) | N sessions over `tole serve`, each its own approval domain |
| Clean-context review loop | cora `review_prepare`/`review_finalize`, review gate (#534) | a reviewer that never saw the coding context |

### A2. What competitors do

- **Hermes profiles** — a profile is a separate home (config, `.env`, `SOUL.md`,
  memory, sessions, skills, cron). Hermes warns: never point two agent
  processes at the same profile; "two writers on one home compound each
  other's state". Profiles are explicitly *not* a sandbox.
- **Hermes Kanban** — a durable board shared across profiles: tasks with
  one assignee profile, status (`triage → todo → ready → running → blocked →
  review → done`), parent→child links (dependency gating), comments as the
  inter-agent protocol (a respawned worker reads the full thread), scratch /
  dir / worktree workspaces, a dispatcher that reclaims stale claims and
  crashed workers, bounded retries with a consecutive-failure circuit
  breaker, and "a commit alone never completes a task" (completion contracts).
  Hermes' own comparison: `delegate_task` is a function call; Kanban is a work
  queue that survives restarts and admits humans.
- **Hermes Bot Mode** — 2–6 bots in one group chat, up to three serial reply
  rounds per message.
- **Nakama** — profiles (soul, tools, skills, model) bound per automation;
  org memory curated by admins; self-improving skills with optional admin
  approval.

### A3. Is it needed? Evidence

- **Cost.** Anthropic's lead+subagent research system beat a single agent by
  90.2% but used ~15× the tokens; token use explained ~80% of the variance
  (already cited in orchestration.md).
- **Failure modes.** *Why Do Multi-Agent LLM Systems Fail?* (MAST,
  arXiv 2503.13657, NeurIPS 2025): 14 failure modes in three clusters — system
  design, **inter-agent misalignment**, task verification — from 150+
  annotated traces (κ = 0.88).
- **What works in production** (Cognition, *Multi-Agents: What's Actually
  Working*, 2026, revisiting *Don't Build Multi-Agents*): multi-agent works
  "when **writes stay single-threaded** and the additional agents contribute
  intelligence rather than actions". Working patterns: a clean-context
  review loop (~2 bugs/PR, ~58% severe), a "smart friend" larger model the
  primary consults, and map-reduce-and-manage (manager splits, children
  execute, manager synthesizes). Unstructured swarms of negotiating agents
  are "mostly a distraction".

**Verdict:**
- **Profiles: yes.** Cheap, high leverage, and cron jobs need them anyway
  (a job runs *as* a profile, Nakama-style).
- **Durable task board: yes, narrowly.** It is the one thing child agents
  cannot do: survive restarts, gate on dependencies, admit a human, outlive
  the parent session.
- **Group-chat swarms: no.** Matches the evidence and tole's non-goals.

### A4. Where uteke rooms fit (and where they do not)

uteke is **semantic memory**: recall is ranked and fuzzy. Coordination state
needs exact, ordered, at-most-once semantics (claims, dependencies, status).
So:

| Concern | Store |
|---|---|
| Task, claim, status, dependency, comment order | tole's append-only JSONL ledger (same engine as cron, see A5) |
| Shared knowledge between profiles (findings, decisions, briefs) | a uteke **project room** (`repo-<name>` or `board-<slug>`), read by `uteke_recall`, written by `uteke_document` (Write-tier, approved) |
| Child → parent result | the existing per-child mailbox room (unchanged) |
| A profile's private memory | its own room (`profile-<name>`); never shared by default (Hermes' two-writers warning) |

Comments on a task stay in the ledger (ordered, exact); a worker may *also*
publish a durable finding to the project room for later recall.

### A5. Design — two deep modules, no new concepts

**1. `Profile` — resolved through the existing config layer.** Per §6
("adding code is a last resort"), a profile is a named section in the user
config, applied through the existing `config_apply` path:

```toml
[profiles.reviewer]
system_prompt_file = "~/.tole/profiles/reviewer.md"
model = "…"
allow = []                 # read-only reviewer: no Write pre-auth
memory_room = "profile-reviewer"
skills = ["code-review"]
```

`tole --profile reviewer chat`, `tole cron create … --profile reviewer`.
Interface: `Profile::resolve(name) -> SessionSettings` — one call, and every
face (run/chat/mission/serve/cron) already consumes `SessionSettings`.
Secrets are **not** stored in a profile (see Part B); a profile names which
secrets it may use.

**2. `Ledger` — one claim/fold engine, two adapters.** The cron design
(append-only log, `fire` claim before execution, pid+start-time lease,
recovery, never-overwritten terminal records) is exactly what a task board
needs. One module, two adapters is a *real* seam:

```
Ledger (tole-core, pure, injected clock)
  interface: ready(now) -> Vec<Item>     // due jobs | ready tasks
             claim(item) -> Claim        // appends; at-most-once
             settle(claim, Outcome)      // appends; never overwrites
             recover(now, alive) -> Vec<Recovered>
  adapters:  CronJobs   (ready = schedule is due)
             TaskBoard  (ready = status todo ∧ all parents done)
```

Board semantics adopted from Hermes, reduced to what the evidence supports:
- one assignee profile per task; **one writer per workspace** (a coding task
  gets its own worktree; reviewers are read-only profiles);
- statuses `todo · ready · running · blocked · review · done`;
- parent→child links gate `ready`; comments are ordered ledger entries and
  are replayed into a respawned worker's context;
- completion requires an explicit `task_complete` (a commit is not
  completion); `review` routes to a read-only reviewer profile (the clean
  context pattern);
- bounded retries + circuit breaker (N consecutive failures ⇒ `blocked`
  for a human).

Tools exposed to the model (small surface): `task_list`, `task_show`,
`task_comment`, `task_complete`, `task_block`. Creating and assigning tasks
is a human/CLI action (`tole task …`) in v1 — an agent that can create work
for other agents is the swarm we are avoiding.

### A6. Owner direction (2026-11-11) — dormant profiles, one orchestrator

Supersedes the open-ended shape of A5 where they disagree.

- **One always-on profile: the orchestrator** (also the maintainer). Every
  other profile is **dormant**: a config entry and a persona file, no
  process, no tokens, no memory reads. Motivation from real use: of five
  configured Hermes profiles only a few are used, yet each costs a running
  gateway. A dormant profile costs only disk, so the number of profiles can
  grow freely.
- **Activation only through the orchestrator.** A user `@mention` is a hint
  the orchestrator routes, not a direct line, so there is one conversation and
  one approval surface.
- **Depth ≤ 2 below the orchestrator:** orchestrator (0) → profile (1) →
  sub-profile (2). Depth 2 cannot delegate (structural, like today's depth-1
  cap: the tool is not registered there).
- **Concurrency cap** *(interpretation, to confirm)*: at most **2 delegated
  profiles live at once** across the whole tree, and at most **1 executor**,
  meaning one write lease per workspace. Every other live profile runs
  read-only (consult/review).

Invariants the caps need to stay safe (today's depth-1 cap exists to stop
silent budget multiplication, see `agents.rs`):

| Invariant | Rule |
|---|---|
| Live count | one global counter held by the orchestrator, never per parent, so depth 2 cannot fan out |
| Budget | a child's budget is carved from its parent's *remaining* budget, never added |
| Authority | child allowlist = parent allowlist ∩ profile allowlist; `Destructive` unreachable at every depth |
| Writer | the executor lease is a ledger claim (one per workspace); a second `execute` waits or is refused |
| Results | flow up one level only, through the existing mailbox room; the orchestrator is the only one that answers the user |

Deep module: **`Delegation`**. Small interface, all caps enforced inside:

```
delegate(profile, task, mode: consult | execute) -> Handle   // refuses on any cap
poll(handle) -> Status | Result
```

Callers: the orchestrator's tool, `@mention` routing, cron jobs that run as a
profile. `agent_start` becomes the depth/lease-free special case, or is
replaced by it.

### A7. BMAD Method — analysis and what to adopt

Sources read: README (v7 preview, v6.12 stable), issue #1629 (v6 architecture
write-up), issue #2211 (parallel execution request), and the v7 source of
`bmad-party-mode` (SKILL.md, `mode-*.md`, `party-memory.md`,
`customize.toml`) plus the agent persona skills (`bmad-agent-*`).

**How BMAD works.**
- An agent is a **persona file** (name, icon, role, identity, communication
  style, principles, a menu of skills) with a base → team → user
  customization merge. Shipped extra personas "cost nothing until summoned".
  This is the dormant-profile idea, already proven.
- **Party mode** is a discussion the orchestrator hosts, in four modes:
  `session` (default: one model voices every persona inline, no spawn),
  `auto` (voice inline, spawn real agents *only when independent thinking
  changes the outcome*: reviews, critique, divergent conclusions),
  `subagent` (one real agent per persona per round), `agent-team` (standing
  team). Its own docs say one model voicing every persona "tends to make them
  agree"; independent agents cost more tokens and time.
- **One shared room.** In spawned modes every member receives the whole
  exchange each round, not just its slice; otherwise they "drift out of
  sync — separate consultations wearing a party's clothes".
- **Anti-consensus personas** ship as a party: Option Generator, Claim
  Checker (evidence and confidence), Loop Stopper (ends repetition), Consensus
  Challenger. Rule: the room supports the human's judgment, **does not vote
  or declare consensus**.
- **Memory** is per party, append-only (`.memlog.md`), read on entry through a
  reader that **distills it to a few hundred tokens**, written silently only
  for beats that would change a future session; correction = a superseding
  entry.
- **Durable context**: tickets/stories are the single source of truth;
  workflow steps load just-in-time; progress lives in the artifact's
  frontmatter, so work resumes after compaction.
- **Weak spots** (from #1629/#2211): personas are prompt engineering in the
  main context (no tool restriction, no context isolation, "stay in
  character" is only an instruction), and execution is sequential.

**Adopt in tole:**
1. Profile = persona + allowlist + model + memory room, **dormant by
   default** (A6). Unlike BMAD, the allowlist and model are enforced by the
   host, not requested in the prompt.
2. Discussion modes mapped to the caps: `inline` (orchestrator voices dormant
   personas from their files; zero processes, profiles stay dormant) as the
   default; `independent` (spawn a real read-only profile) only for review,
   critique and genuinely divergent questions. This is BMAD's `auto` mode,
   and it fits the "2 live" cap.
3. One shared thread: a spawned participant receives the full discussion log
   from the ledger, not a slice.
4. Ship anti-consensus roles (claim checker, loop stopper) as dormant
   profiles; the orchestrator never reports a vote as a decision, the owner
   decides.
5. Memory: a uteke room per profile (`profile-<name>`) and per discussion
   group, read through a distilled brief at activation, written only for
   decisions and outcomes.
6. Handoff by artifact: the task's handoff document (#383) is the single
   source of truth between levels, matching BMAD tickets.

**Do not adopt:** stay-in-character theatrics, keepsake HTML, menus of
two-letter codes, and an unrestricted persona running in the main context.

## Part B — Secrets leaking to the LLM

### B1. How secrets actually leak (evidence)

- **Into context by reading.** An agent wiring an API reads `.env` to learn
  the variable name; the live value is now in context and drifts into
  configs, fixtures and commits. GitGuardian (*State of Secrets Sprawl
  2026*): ~28.65M new hardcoded secrets on public GitHub in 2025 (+34%);
  AI-assisted code leaks at **~2× the baseline** rate.
- **Borrowed agent.** The s1ngularity npm compromise (Nx, Aug 2025) invoked
  installed AI CLIs with permission-bypass flags to inventory `.env`,
  `id_rsa`, keystores; 2,349 secrets stolen across 1,079 repos (Docker,
  *Coding Agent Horror Stories* #4).
- **Indirect prompt injection** ("dump `env`", "curl this with your token")
  from issues, web pages, MCP output — the *lethal trifecta*: private data +
  untrusted content + egress.
- Industry convergence (Anthropic managed agents, Vercel, Cloudflare,
  Docker Sandboxes, Infisical Agent Vault, agentcage): **credential
  brokering** — the agent holds a placeholder, a proxy outside its reach
  injects the real value on the wire and redacts echoes on the way back.

### B2. What tole does today

| Control | Where | Covers | Gap |
|---|---|---|---|
| ENV-1 name-based scrub | `subprocess::scrub_env_for_child` | removes `*API_KEY*`, `*SECRET*`, `*TOKEN*`, `*PASSWORD*`, `*CREDENTIAL*`, `*PRIVATE_KEY*` from spawned commands | secrets with innocent names (`DATABASE_URL=postgres://u:pass@…`); `GITHUB_TOKEN` kept for every command |
| E11 value redaction | `openai::scrub` in `request_body` | the **provider key** in any outgoing message/arguments | only one value; other secrets in tool results reach the provider |
| MCP env scrub, config schema rejects secret keys | `mcp.rs`, config | | |
| Child agents | `agents.rs` | — | **inherit the full unscrubbed env** (documented trade-off: they need the provider key) |
| Workspace jail | file tools | `~/.ssh`, `~/.aws` outside the workspace | a `.env` *inside* the workspace is readable verbatim |

### B3. Design — one deep module: `SecretSet`

Today secret handling is spread across `scrub_env_for_child`, `openai::scrub`,
MCP `scrub_err` and the child launch. Deletion test: removing a central
module would re-spread that logic over ≥ 5 call sites — it earns its keep.
Replace, don't layer: the existing functions become callers of it.

```
SecretSet (tole-core)
  interface: SecretSet::collect(env, config) -> SecretSet
             redact(&str) -> Cow<str>          // every known value → «NAME»
             env_for(tool: &str) -> Vec<(Name, Value)>   // scoped release
             is_sensitive_path(&Path) -> bool
```

Layered controls, cheapest first (each is useful alone):

1. **Value-based redaction on the wire (generalize E11).** Collect values of
   secret-shaped env vars, explicitly listed secrets, and passwords embedded
   in URL userinfo; redact every occurrence (plus base64 and percent-encoded
   forms) in *everything* `request_body` sends — tool results, user text,
   arguments, compaction summaries — replacing with `«GITHUB_TOKEN»`.
   One seam (the provider request builder) covers every face. Multi-pattern
   matching (Aho-Corasick) keeps it linear in message size. Values shorter
   than 8 chars are not collected (false-positive guard), reported by
   `tole doctor`.
2. **Sensitive-path reads return names, not values.** `read_file` on
   `.env*`, `*.pem`, `id_*`, `.npmrc`, `.netrc`, `*.key` returns
   `KEY=«redacted»` lines (the "keep names in context, values outside"
   pattern); the raw read needs an explicit approval. The model learns the
   variable name it needs without ever holding the value.
3. **Scoped release instead of global inheritance.** Config binds secrets to
   the tools that need them (agentcage's `inject_to`, per tool rather than
   per domain):

   ```toml
   [secrets]
   GITHUB_TOKEN = { tools = ["gh", "git"] }
   ```

   `run_command`/`job_start` then get *no* secret by default — closing the
   current `GITHUB_TOKEN`-for-every-command trade-off — while the builtin
   `gh`/`git` tools still authenticate.
4. **Child agents get the minimum.** A child receives the scrubbed env plus
   exactly the provider variables it needs, not the parent's whole
   environment (closes the `agents.rs` trade-off).
5. **Brokering for high-risk deployments — integration, not code.** For
   unattended/webhook use, run tole inside a credential-brokering sandbox
   (Docker Sandboxes, agentcage, Agent Vault). tole's provider client honours
   `HTTPS_PROXY`, so placeholder injection happens outside tole. Document it
   in the threat model; do not build a proxy into tole.

**Honest limit.** Redaction stops *accidental* disclosure to the provider and
the logs. It does not stop a determined, prompt-injected agent that holds a
secret in its process reach and has network egress (it can split or encode
the value). Only isolation (layers 3–5) changes what the agent can reach.
The threat model should state this split explicitly.

## Part C — Proposed order

1. `SecretSet` + wire redaction (B3.1) and child minimum env (B3.4): small,
   closes documented gaps, no UX change.
2. Sensitive-path reads (B3.2) and scoped release (B3.3).
3. `Profile` via config (A5.1) — needed by cron jobs.
4. `Ledger` core shared with cron (automation-design phase 1), then the
   `TaskBoard` adapter and `tole task` CLI (A5.2).

Each step updates the threat model, README status line and `docs/epics.md`
in the same PR (`AGENTS.md` §7).

## Sources

- Hermes Agent docs: Profiles; Kanban (multi-agent board); Bot Mode.
- Cognition: *Don't Build Multi-Agents* (2025); *Multi-Agents: What's
  Actually Working* (2026).
- Cemri et al., *Why Do Multi-Agent LLM Systems Fail?*, arXiv 2503.13657.
- Anthropic, *How we built our multi-agent research system* (2025).
- Infisical, *Credential Brokering for AI Agents, Explained* (2026).
- agentcage (github.com/agentcage/agentcage) README.
- Docker, *Coding Agent Horror Stories: The 29 Million Secret Problem* (2026),
  citing GitGuardian *State of Secrets Sprawl 2026* and StepSecurity's
  s1ngularity analysis.
- Nakama docs: automations, profiles, self-improving skills.
