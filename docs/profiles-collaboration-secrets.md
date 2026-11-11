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
