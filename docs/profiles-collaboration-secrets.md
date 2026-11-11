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

### A8. Owner decisions round 2 (2026-11-11) and deeper analysis

Decisions (supersede A6 where they differ):
1. **No fixed count** of live profiles under the orchestrator; the owner's
   prompt sets how many.
2. **Depth: default 2, configurable up to 3** below the orchestrator.
   **Every level may execute (write)**, but every action must be reported
   and auditable; control returns to the owner and the orchestrator.
3. **Discussion** runs as many rounds as the goal needs, chosen
   dynamically. Each round passes through a **sanitizer**; profiles see only
   the sanitized result of the previous round, never the raw exchange. The
   orchestrator decides after each round: report to the owner, or continue
   automatically. Round results go into a **hierarchy of uteke documents**.

#### A8.1 What replaces the count limit

Without a count cap something still has to bound cost and machine load, or
"no limit" turns into the 15× token bill silently:
- **Budget is the limiter.** Every delegation carves its budget (tokens,
  minutes, steps) from the parent's remaining budget; the owner's prompt
  (or the orchestrator's default) sets the root budget. N profiles share
  one pot; they cannot multiply it.
- **A machine guard**, not a policy limit: `delegation.max_live_processes`
  (generous default, owner-configurable) only queues extra spawns so a
  laptop is not overrun. Queued work starts when a slot frees, and the
  queueing is visible.

#### A8.2 Every level can write — made safe by isolation and reports, not by read-only

The risk of several writers is real (Cognition: parallel writers make
conflicting implicit decisions; MAST FM-2.6 reasoning–action mismatch).
The controls:

| Control | Rule |
|---|---|
| Physical isolation | parallel writers each get their **own git worktree** (the existing `--agents-worktree` mechanism); writers in the **same** workspace are serialized by a write lease (ledger claim) |
| Merge back | only the orchestrator (with owner approval) merges a worktree; children never merge into each other |
| Approvals | a child's non-allowlisted Write bubbles up to the orchestrator's single approval surface; allowlist = parent ∩ profile; `Destructive` unreachable |
| Mandatory report | a delegated run cannot settle `done` without a **completion report** (what changed, files, diff ref, checks run, open risks); missing report ⇒ `incomplete`, never silently done (Hermes: "a commit alone never completes a task") |
| Audit trail | every delegated run is an ordinary durable session (JSONL); the ledger records the tree (who spawned whom, budget, allowlist, worktree). `tole audit <goal>` prints it top-down with drill-down into any session |
| Depth | `delegation.max_depth` default 2, hard ceiling 3; the delegate tool is registered only where `depth < max_depth` (keeps today's structural enforcement, moves the number) |

Depth 3 is allowed but not recommended: each hop passes a lossy summary, so
context loss compounds (MAST FM-1.4 loss of history, FM-2.4 information
withholding).

#### A8.3 Multi-round discussion with a per-round sanitizer

Evidence:
- MAST (150+ traces): premature termination 7.82%, no or incomplete
  verification 6.82%, incorrect verification 6.66%, proceeding on wrong
  assumptions 11.65%, plus step repetition and "unaware of termination
  conditions" as named modes. A round loop needs explicit stop rules and
  verification that is not just the orchestrator grading itself.
- MAD survey (arXiv 2607.26212): 86% of systems pass messages verbatim, ~11%
  summarize; summarizing "can substantially cut communication overhead while
  remaining competitive" but "risks information loss when critical details
  are omitted". Hybrids (verbatim for local reasoning, summary for
  coordination) exist.
- Minority Sentinel (arXiv 2606.29270): models share training data, so
  errors correlate; in ~1 of 4 divergent cases the **minority was right**,
  and majority voting suppressed it.
- Sparse-topology debate (Li et al., EMNLP Findings 2024): not everyone needs
  to see everyone; comparable or better quality at lower cost.

Loop (all state in the ledger, so a crash resumes mid-discussion):

```
Goal contract   question, acceptance criteria, participants (from the
                owner's prompt), budget, max rounds
Round n         each participant gets: contract + digest(n-1) + its OWN
                previous contribution  → contribution(n) [raw, ledger]
Sanitizer       deterministic pass, then digest pass → digest(n)
Gate            orchestrator: report | continue | escalate
```

**The sanitizer has two passes:**
1. *Deterministic, always on, cheap:* secret redaction (`SecretSet`, Part
   B), control/bidi character neutralization (the existing `sanitize.rs`
   rules), tool-result fence neutralization, size caps. This is a security
   step: raw contributions can carry text from web pages and tool output, and
   the digest must not turn an injected instruction into an authoritative
   line.
2. *Digest, cheap model:* a fixed schema, not free prose:
   - decisions made, each with the round and participant that made it;
   - claims, each attributed and **linked to its raw ledger entry**
     (provenance, so an auditor can drill down);
   - **open disagreements, minority positions kept** (agreement may be
     compressed, dissent may not be dropped);
   - open questions, actions taken (writes, with diff/worktree refs);
   - **delta**: what is new versus the previous digest.

   Small rounds skip it: below a threshold, the raw round is passed through
   verbatim (the hybrid the survey describes). The digest is written by a
   dedicated cheap "scribe" profile, not by the orchestrator, so the party
   that decides is not also the one that summarizes.

**The gate, deterministic checks first, judgment second:**

| Outcome | When |
|---|---|
| **Report to owner** | acceptance criteria met, checked as a checklist by a claim-checker role (not the orchestrator alone) |
| **Escalate to owner** | budget or max rounds reached; no new information in the delta for 2 rounds (stalled); a disagreement that only the owner can decide; a pending Write approval |
| **Continue automatically** | otherwise; the orchestrator may change the roster for the next round (dynamic), within the budget |

Token effect (input tokens spent re-reading history, N participants, R rounds,
c tokens per contribution, d tokens per digest):
- verbatim, everyone sees everything: N²·c·R(R−1)/2
- with digests: N·R·(d + c) + R·N·c (scribe)
- example N=3, c=800, d=600: R=4 → 43.2k vs 26.4k; R=8 → 201.6k vs 52.8k.
  The saving grows quadratically with the number of rounds.

#### A8.4 Hierarchical uteke documents

uteke documents support a `parent` (max depth 10), so the hierarchy fits:

```
goal-<id>                      contract + final report (linked to the project room)
├── goal-<id>-r1               round digest
│   └── goal-<id>-r1-<profile> per-profile digest (only when useful)
├── goal-<id>-r2
└── …
```

- Raw contributions stay in tole's JSONL ledger, **not** in uteke: indexing
  raw debate would cost embedding work and pollute semantic recall with
  superseded positions. uteke holds the digests and the final report; every
  digest line links back to the raw entry.
- Only the final report and recorded decisions are linked into the shared
  project room; round documents stay under their parent.
- Gap: tole's `uteke_document` tool takes `slug`, `markdown` and `room`, but
  no `parent`. It needs one (verify that the uteke CLI exposes it; the uteke
  cloud API does).

Decided (2026-11-11): default max rounds **4**; the scribe runs on a
**cheap model**. The `parent` gap is tracked in codecoradev/tole#576.

### A9. Model options per profile, task-weight routing, laya as the selector

Owner direction: the orchestrator and every profile carry **several model
options** and switch between them; the choice can be made by laya (or
hosted Jev), based on how heavy the task is. laya/Jev stay **optional**.
Dormant profiles free memory that a local laya can use.

#### A9.1 What laya is (read from `codecoradev/laya-service`)

- Sidecar over the Laya decision engine: a **non-autoregressive System-1
  model** answering typed questions (`choice` 2–20 options, `score` 2–10
  ordered levels, `noul` 0–1) over a state, with **calibrated confidence**;
  the service's suggested policy is auto-act at ≥ 0.85, escalate below.
- Stateless and policy-free; **deterministic** (identical payloads gave
  identical answers; a result cache is valid, PERFORMANCE.md).
- Cost: ~0.2–0.6 s per short decision on CPU; ~3.3 s p50 for a ~1k-token
  state; tole's client notes ~17 s on ~2.5k tokens (1024-token context).
  Throughput is CPU-bound and flat beyond ~2 concurrent; admission control
  answers `503` + `Retry-After` beyond the queue.
- Memory: Python service measured 2.0–2.8 GB RSS (`mem_limit: 4g`); the
  Rust/ONNX port is a 31 MB binary plus a ~1.29 GB fp32 checkpoint.
- Jev-wire compatible, so tole's existing `systemone_decide` client
  (`SYSTEMONE_BASE_URL`) already talks to it; hosted Jev is the same wire.
- Limit stated in tole's client: **not adversarial-resistant**; pass
  filtered state, never raw tool results.

**Memory reading of "dormant profiles free memory":** a dormant profile
costs nothing, so the box's RAM goes to the orchestrator, the live children
and **one** laya process shared by every profile (it is stateless, so it is
not per profile). Budget ~1.3 GB (Rust) to ~2.8 GB (Python) for it; on a
small VPS, hosted Jev or the rule router (below) is the fallback.

#### A9.2 Evidence on routing by task weight

| Source | Finding |
|---|---|
| FrugalGPT (Chen, Zaharia, Zou, 2023) | cascades matched the best single model with up to 98% lower cost on their benchmarks |
| Hybrid LLM (Ding et al., ICLR 2024) | a router on predicted query difficulty cut large-model calls by up to 40% with no quality drop |
| RouteLLM (Ong et al., 2024, arXiv 2406.18665) | routers trained on preference data cut cost by over 2× in some settings without hurting quality |
| Anthropic advisor strategy (2026) | Sonnet executor + Opus advisor: +2.7 pp on SWE-bench Multilingual at 11.9% lower cost per task; Haiku + Opus advisor on BrowseComp 41.2% vs 19.7% solo, 85% cheaper than Sonnet solo (but 29% below its score) |
| Cognition, *Multi-Agents: What's Actually Working* (2026) | a weaker primary often does not know when to escalate; across frontier models the router becomes a capability router, not only a difficulty escalator |
| **LLMRouterBench** (ACL Findings 2026; 400K instances, 33 models) | many routers perform alike; several, including commercial routers, **fail to reliably beat a simple baseline**; bigger model pools give diminishing returns versus careful curation |
| **Routing collapse** (NeurIPS 2026, arXiv 2602.03478) | under loose budgets routers **over-select the strongest, most expensive model** even when a cheaper one is equivalent |

Conclusion: routing by weight pays off, **if** the pool is small and
curated, a simple rule baseline exists and is beaten before a learned
router is trusted, ties go to the cheaper model, and a failure can
escalate (cascade).

#### A9.3 Design

**Model options as tiers, not names.** Each profile (and the orchestrator)
declares tiers in its config; the router picks a tier, which keeps laya's
`choice` at 3–4 options (far under its 20 limit) and lets the owner change
models without touching routing:

```toml
[profiles.coder.models]
light    = "…"     # summaries, scribe, simple edits
standard = "…"     # default
heavy    = "…"     # multi-file design, hard debugging
advisor  = "…"     # optional: consulted, never executes (advisor pattern)
default  = "standard"
```

**Precedence** (first that applies wins): owner's explicit choice in the
prompt or flag → a role pin (the scribe is always `light`) → the router →
the profile's `default`.

**Deep module `ModelRouter`**, two adapters (a real seam):

```
route(task_brief, profile, budget_left) -> Route { tier, source, confidence, reason }
  RuleRouter   always available: role, risk tier of the tools the task
               needs, files expected to change, owner keywords; the baseline
  LayaRouter   systemone backend (local laya or hosted Jev); used only when
               its confidence ≥ 0.85, otherwise RuleRouter decides
```

LayaRouter asks a fixed, literal question set over a **short, owner-side
task brief** (the goal contract and profile role, never raw tool output or
web text: laya is not adversarial-resistant, and short states keep it
sub-second):
- `score` weight: light / standard / heavy, each level described literally;
- `noul` "changes more than one file or a public interface";
- `noul` "needs a plan before acting".

Rules around it:
- **Tie or doubt → cheaper tier** (the routing-collapse lesson); the budget
  caps the tier the router may pick.
- **Route per delegation and per discussion participant, not per turn.**
  A model switch mid-session discards the cached prompt prefix and changes
  behaviour; tole's `usage_report` already measures the cache-hit rate to
  verify this.
- **Cascade, bounded:** a run that fails its checks, hits its step budget
  or reports `blocked` may retry once, one tier up; never an automatic
  downgrade mid-task.
- **Advisor instead of escalation** where it fits: an executor on
  `standard` may consult `advisor` a capped number of times, mirroring the
  advisor strategy.
- **Audit:** every routing decision is a ledger entry (brief hash,
  answers, confidence, tier, source); laya is deterministic, so decisions
  are cached by that hash and replayable.
- **Fail-safe:** laya down or `503` ⇒ RuleRouter, never a blocked task.
  Hosted Jev sends the brief off-host (a threat-model data flow); a local
  laya keeps it on the box.

**Adoption gate:** before LayaRouter becomes a default, the eval harness
(`evals/`, Tier 2 missions) compares always-`heavy`, RuleRouter and
LayaRouter on success rate and cost. LayaRouter ships as the default only if
it beats the rule baseline (LLMRouterBench shows many routers do not).

#### A9.4 Other cheap decisions laya can take (later, same seam)

All on sanitized or owner-written text only, never raw payloads:
- which dormant profile fits a task (`choice`; above 20 profiles, choose a
  group first);
- the discussion gate's "is this round's delta new information" (`noul`),
  feeding stall detection; the claim-checker stays an LLM role;
- relevance of an incoming webhook event before any LLM run (automation
  design §6 filters).

### A10. Decisions round 3 (2026-11-11) and validation

**Decided:**
1. **laya runs as a separately hosted service**, and its presence is the
   feature's on/off switch. Deployment: the Docker Hub image
   `codecoradev/laya-service-rust`.
2. **The advisor tier is enabled.**

**Validation (checked 2026-11-11):**

| Claim | Result |
|---|---|
| Rust image exists | yes: Docker Hub `codecoradev/laya-service-rust`, tags `latest`, `2026-10-28c`, `rust`, amd64 + arm64 |
| Image contents | **binary only** (~23 MB compressed, 31 MB binary, distroless, non-root). The ONNX checkpoint is **not** baked in: mount it at `/model` (fused export, ~1.29 GB fp32, exported once with upstream `tools/export_onnx.py`). The Python image `codecoradev/laya-service` downloads weights itself (~360 MB image) |
| Contract | same four endpoints as the Python service; 15/15 exact parity verified by the laya project; model load 3–6 s from disk |
| Auth | `BEARER_TOKEN` required, fail-closed; `/healthz` open; listens on `0.0.0.0:8080` |
| tole client fits | yes, no new client: `systemone_decide` is active only when `SYSTEMONE_API_KEY` is set (owner decision 2026-10-05: on by key presence, silent off otherwise); `SYSTEMONE_BASE_URL` is the full POST URL (`https://<host>/v1/systemone`); any non-hosted backend gets the conservative caps (≤ 20 options, 4 KiB state), which a routing brief fits |
| Advisor via Anthropic's advisor tool | **not directly usable**: it is a Claude Platform server-side beta tool (`advisor_20260301`), while tole's provider speaks OpenAI-compatible `/chat/completions` |
| uteke CLI `--parent` | yes: `uteke doc create <slug> --parent <slug>` plus children / descendants / move / breadcrumbs, max depth 10 (uteke `docs/cli-reference.md`, CHANGELOG). #576 is unblocked |

**Resulting design:**
- **On/off = configuration only.** `SYSTEMONE_API_KEY` (= the laya
  `BEARER_TOKEN`) + `SYSTEMONE_BASE_URL` set ⇒ `LayaRouter` and
  `systemone_decide` are active; unset ⇒ `RuleRouter` only, nothing else
  changes. No new flag, no new env name. `tole doctor` reports the state and
  calls `/healthz`.
- `LayaRouter` reuses the `systemone` client internally; it is not the
  model-facing tool, so the model cannot steer its own routing.
- Deployment note: put the service behind TLS on a private network; the
  routing brief leaves the tole process (threat-model data flow, kept inside
  the owner's infrastructure).
- **Advisor, host-side and provider-agnostic:** when a profile declares an
  `advisor` tier, tole registers a ReadOnly `consult_advisor` tool. tole
  makes a separate provider call on the advisor model with a curated context
  (the task contract plus the recent transcript window, after secret
  redaction) and returns a short plan, correction or stop signal. The
  advisor never calls tools and never talks to the owner. Uses are capped
  per run (`advisor_max_uses`, default 3), its tokens are recorded
  separately in the usage ledger and carved from the run's budget, and each
  consultation is a ledger entry.

**Conflicts with earlier recorded context, resolved by the owner
(2026-11-11):**
1. The 2026-10-06 competitor research marked "multi-agent without a cap"
   as anti-DNA and kept depth 1. **Superseded:** A8 stands (any number of
   profiles, depth 2 by default, max 3), with the budget carve (A8.1) as
   the cap that replaces the count cap.
2. The 2026-11-09 proposal put the scheduler and gateway in a separate
   repository, with tole as a narrow kernel. **Decided: the scheduler stays
   inside tole** (the ledger core in `tole-core`, the ticker in `tole serve`,
   and `tole cron tick`), as in [automation-design.md](automation-design.md).
   The gateway (chat channels) is out of scope for now; where it lives
   stays open.

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
