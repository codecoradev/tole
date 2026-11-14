# Delegation approvals: escalation up the chain — design note

Status: design for #714 (parent #583); the owner's answers to the open
questions are in section 12. PR 1 (records and fold) implements section 3,
PR 2 (request builder, caps, the parked approver) section 4.1, PR 3 (the
owner's command and the audit section) section 5.2 and 6, PR 4 (serve) the
second bullet of 5.2.
Owner decision of 2026-11-14 (in the body of #714): a delegated child that cannot
decide an approval escalates it up the delegation chain, level by level
(child, parent, grandparent, ..., the owner), until a level can decide it.
The owner is the last level. No answer means **denied**, never allowed.

Related: `profiles-collaboration-secrets.md` A8.2 ("a child's non-allowlisted
Write bubbles up to the orchestrator's single approval surface"),
`automation-design.md` ("Entry point", item 3, #704), `threat-model.md`
("Remote approver trust boundary (#200)", "Delegation tree (#585)",
"Owner-only records (#684)").

## 1. Current state (what exists, what is missing)

References are to the tree at `develop` of 2026-11-14.

| Piece | Where | Behavior today |
|---|---|---|
| Delegated child | `crates/tole-cli/src/delegate.rs` `launch` | A detached `tole mission` (`--max-steps/--max-minutes/--max-tokens`, `--profile`, `--plan-mode` for a discussion participant). Authority: `--allow` = parent ∩ profile (`tole_core::delegation::child_allow`), nothing from its own config. |
| Child approver | `crates/tole-cli/src/main.rs` `build_approver` (`InteractiveApprover::stdio`, `approver.rs`) | The child has no TTY, so a prompt reads EOF and **denies** (fail closed). `interactive()` is `true`, so Destructive tools are still registered and then denied. |
| What the child sees | `crates/tole-core/src/turn.rs` `drive` (the `Denied::Approver` arm) | Durable turn error `approval required` plus a denial note; the turn ends `ApprovalRequired`. `mission.rs` ("every path settles ... the mission stops and reports") ends the mission `stopped`. The parent sees a finished child, never a question. |
| Queue | `crates/tole-cli/src/approvals.rs` `ApprovalQueue`, `QueueApprover` | **In memory**, in the `tole serve` process only (and cron runs through it). Entries expire after `EXPIRY_SECS = 900`. An ALLOW stores a one-shot keyed `(session, fingerprint)`. |
| Fingerprint | `turn.rs` `call_fingerprint` (tool + `canonical_json(input)`, `DefaultHasher`) | 64 bits, unkeyed, not stable across Rust versions. Fine for an in-memory queue; not for a durable one (section 3). |
| Resume | `session_host.rs` `resume_session_turn`, `turn.rs` `resume_turn_with_cancel` | After an ALLOW, serve resumes the session. The denial left `pc=Planning` (no `settle_final` on that arm), so `drive` replans, the model re-issues the identical call, the approver finds the one-shot, and the call runs. Approval-by-reissue: the fingerprint is what ties the allow to the call. |
| Decision route | `serve.rs` `/approvals`, `/approvals/{id}/decision`, `write_approval_audit` | Bearer-token holder is a full approver (#200). Writes a `fact/approval_<id>` register on the session before resuming. |
| Delegation tree | `crates/tole-core/src/delegation.rs` (`Rec`, `child_allow`, `recover`), host `delegate.rs` | Append-only JSONL `<data dir>/delegation/<hash of workspace>/delegation.jsonl` outside every workspace, exclusive host lock (`ledger_file::FileStore`). Depth 2 (max 3), budget carve, leases, machine guard, mandatory report, `tole audit`. **No approval records.** |
| Owner-only records | `owner_only.rs`, `session_guard.rs`, `child_env::under_session` (#684, #700) | `run_command` and `job_start` refuse the tole binary; every process they start carries `TOLE_UNDER_SESSION=1`; owner-only subcommands refuse when it or `TOLE_AGENT_DEPTH >= 1` is set. |
| Reviewer participants | #704 / PR #715 (`discuss_tool`, `Delegator::consult_participant`) | `--plan-mode` (no Write tool), empty allow list, `TOLE_DISCUSSION_PARTICIPANT=1`; only `delegate_report` writes. "A Write it wants is denied until #714." |
| `Mode::Consult` | `delegation.rs` | Skips the write lease only; it is **not** read-only by construction (#714, "Why"). Read-only comes from the participant launch flags. |

The gap: nothing durable carries a child's question to anyone, nothing
tells a parent or the owner a child is waiting, and the one-shot machinery
lives in the wrong process.

## 2. The level chain and who can decide what

### 2.1 The chain

A request from node `N` of goal `G` has the chain
`[parent(N), parent(parent(N)), ..., root(G)]` read from the delegation tree
(`Node.parent`), followed by the **owner** as the terminal level. The root
node is the orchestrator session the owner started; it is an agent level
like any other: its authority is its own `--allow` / `--trust` flags.
"Owner" means the human, reached only through a host surface (section 5.2).

Levels are numbered 1 (the child's parent) to `depth` (the root); the owner
is `depth + 1`. The chain is recorded in the request (section 3) when it is
created and never changes.

### 2.2 What a level may decide

A level `L` (an agent node) can decide request `R` only if **all** hold:

1. `L` is in `R.chain`, is `Running`, and is the request's **current level**
   (section 4.3). A level never decides a request of a node outside its
   subtree, never its own request, and never one of another goal.
2. `R.risk` is `Write`. `Destructive` is unreachable (section 2.4).
3. `L`'s own **effective allow list** would allow `R` on its own: the
   `pattern_verdict` of `L.allow` (the `Node.allow` recorded in the log, deny
   patterns winning) over the same tool, input and resolved target is
   `Allow`. A level decides only within its own allow list and risk tier, so
   it never grants more than it holds. This is the delegate rule ("never
   more power than the creator holds", `child_allow`) applied to approvals.
4. `R` is not on the always-owner list (2.3).
5. `R` carries a `target` that passes the workspace guard
   (`approval::workspace_guard_ok`, resolved against the child's canonical
   jail root), **or** it is a command that a command pattern of `L.allow`
   allows (`run_command` has no path target).
6. `R.display` was not truncated (the decider saw exactly what the
   fingerprint covers); otherwise the request is owner-only.

A child's own list is consulted first and unchanged: a **deny** pattern of
the child (its own or one inherited from a parent by `child_allow`) is a
final denial that is never escalated, so no level can override a deny that
lies lower in the chain. An `Ask` verdict (a command wrapper under a command
deny) is owner-only.

Because `child_allow` narrows only downward, anything a child is asked about
that a parent's list allows is exactly what the profile narrowing removed
from the child. That is the case escalation exists for.

### 2.3 Always the owner, never an agent

A request in any of these classes skips every agent level (each recorded as
`skipped: owner_only` in the chain) and goes to the owner:

| Class | How it is recognized (fail closed: unknown means owner-only) |
|---|---|
| Secrets | the tool is bound in the session's `[secrets]` release (#598) or its input names a secret-named variable; the child's `keep_secrets` is never widened by an approval |
| Trust and approval records | a target (resolved, symlinks followed) inside the tole data dir or on a project config / trust file; any tool that writes an owner-only record of #684 (`cron`, `plugin approve`, `skills approve`, `config trust`) |
| New allow patterns | **there is no allow-always verdict**: every decision is one call. A request to widen a list is not an approval at all: `delegate` drops patterns the parent does not cover (`dropped`, recorded) and that is never escalated |
| Profile or budget changes | likewise not approvals: a `delegate` budget above what is left is refused by the carve (`DelegationError`), never escalated |
| Plugin and skill approvals, external effects | tools supplied by a plugin, an MCP server or a skill (their effect is unknowable to a level); `skill_propose` stays a draft write, promotion stays `tole skills drafts promote` |
| Write outside the workspace | a **path-writing tool** (one that reports an `approval_target`) whose `target` is absent, outside the canonical root, or failing the `@workspace` guard (executed-by-another-tool files such as `.git/hooks`, `build.rs`, `Makefile`, CI files, `.envrc`), including any `..` or symlink escape. A command tool (`run_command`) has no path target and is not in this class: it is decidable by a level only through a matching command pattern of that level's list (2.2.5), otherwise it goes up |
| Oversized | display truncated, or input over the record cap (16 KiB) |

The "external effects" and "oversized" rows go slightly beyond the list in
#714; they apply the same rule to what a level cannot inspect (owner decision 3).

### 2.4 Destructive and depth

`Destructive` stays structurally unreachable at every depth: the new
delegated approver reports `interactive() == false`, so the registry does
not register Destructive tools (`gate.rs`, `tool.rs`). Today's stdio
approver says `true` and merely denies at EOF; the new approver is stricter
(a behavior change to note in the CHANGELOG). Depth and the budget carve
are untouched (section 8).

## 3. Data model: a durable, append-only, fingerprint-bound queue

One file next to the delegation log, same directory, same host lock and
`FileStore`: `<data dir>/delegation/<hash>/approvals.jsonl` (mode 0600,
outside every workspace, so no child file tool reaches it, #684). A new
core module `tole_core::delegation_approvals` folds it, exactly like
`delegation.rs`: pure, no clock, no spawn, no lock, `now` and liveness passed
in; the host holds the exclusive lock around every append; whole-line
appends; torn tail repaired under the lock.

Records (`#[serde(tag = "kind")]`):

| Record | Fields | Written by |
|---|---|---|
| `Request` | `req` (`dap-<time hex>-<random>`), `goal`, `node`, `session` (the child's agent dir), `tool`, `risk`, `target`, `input` (redacted as in session logs, #596, capped), `display` (host-built from `Tool::describe` + `target`, redacted, capped 2 KiB, `truncated` flag), `fingerprint`, `workspace` (canonical), `origin` (built-in, MCP, plugin, skill), `secrets` and `protected` (host-resolved flags, set when the tool is bound in `[secrets]`, the input names a secret or redaction changed it), `chain` (levels: node, depth, and a `skipped` reason when not eligible), `level_secs`, `expires_at`, `at` | the child's host process, never a model string |
| `Escalate` | `req`, `from`, `to` (absent: the owner), `reason` (`timeout` `skipped` `explicit` `dead`; only `explicit` and `dead` move the request, the others record what the clock and the chain already did), `at` | a level (explicit) or the waiting child (timeout, observed) |
| `Decide` | `req`, `by` (`{agent: {node, level}}` or `owner`), `verdict` (`allow` or `deny`), `via` (`tool` for an agent, `cli` or `serve` for the owner), `fingerprint` (of the request, so a decision names the call it covers), `at` | the deciding host, under the lock, after the checks of 2.2 |
| `Consume` | `req`, `node`, `fingerprint` (of the call about to run), `at` | the child's approver, **before** it returns Allow (write-ahead) |
| `Expire` | `req`, `reason` (`timeout` `orphaned` `node_settled`), `at` | the waiting child, the owner CLI or serve (lazily); also derived at fold |

Fold state per request: `pending(level)`, `allowed`, `denied`, `consumed`,
`expired`. Rules the fold enforces (it re-validates, so a forged line from
outside the host cannot do more than the rules allow):

- The **first valid** `Decide` in log order wins; later ones are ignored and
  reported as `race_lost`. `deny` from a level is final; the owner may decide
  while the request is pending, at any level (the owner holds all authority).
- A `Decide` by an agent is valid only if its node is the current level, is
  in the chain, was `eligible`, and `Node.allow` (read from the delegation
  log, immutable once spawned) still allows the recorded request.
- `Consume` is valid once, only for `req.node` and only on `allowed`.
- A request whose node is terminal in the delegation log folds to
  `expired(node_settled)`; its decision is moot.

**Fingerprint.** `fp = SHA-256("tole-approval-v1" 0x00 node 0x00 session
0x00 tool 0x00 canonical_json(raw input) 0x00 target 0x00 workspace)`,
hex. It binds one call of one node, in one session and workspace. It
replaces the 64-bit `DefaultHasher` value for durable use (`sha2` is already
a `tole-core` dependency). `canonical_json` (private in `turn.rs`) becomes
`pub(crate)`-shared. It is computed over the **raw** input by the child;
`input` in the record is the redacted copy (secrets never reach the log) and
the fingerprint is what binds the decision, not the copy.

Bounds: at most 1 pending request per node (Write calls run one at a time,
`docs/tool-batches.md`), at most 32 pending per goal, `approvals.jsonl`
capped at 8 MiB (over: new requests are denied, fail closed, and `doctor`
says so). Rotation is later work (section 11).

## 4. How a headless child parks and is resumed

### 4.1 Park in process

The delegated child gets a new `DelegatedApprover` (host code in `tole-cli`,
next to `QueueApprover`) instead of `InteractiveApprover::stdio`. Its
`decide`:

1. Allow list first: identical to today (`pattern_verdict`). A `Deny`
   pattern is a final Deny.
2. `ReadOnly` allows (as `QueueApprover`).
3. Otherwise build the `Request` (fingerprint, redacted copy, chain with
   eligibility computed from the ancestors' recorded `allow`), append it,
   and **block inside `decide`** polling the log by offset (the
   `LedgerStore::read_from` the delegation log already uses), about once a
   second, until the request is `allowed`, `denied` or `expired`, the node's
   deadline passes, or the session's `CancelToken` fires.
4. On `allowed`: append `Consume`, then return `Verdict::Allow`. The exact
   call being decided runs, once. On anything else: `Verdict::Deny`, which
   takes the existing path (`drive` records the durable turn error and denial
   note, the turn ends `ApprovalRequired`, the mission stops `stopped` and
   hands in its report as today).

Blocking in `decide` has precedent: `InteractiveApprover` blocks on stdin.
While parked a child spends no tokens and no steps, but its wall clock runs:
the wait is capped by `min(approval_secs, time to the node's deadline)`, and
the node's deadline is never extended.

### 4.2 Why not exit and resume

The alternative is serve's shape: deny, settle `ApprovalRequired`, exit, and
let the host resume the session after the decision (`resume_turn`). For a
delegated child it needs more and gives less:

- the approval is by **reissue**: `resume_turn` replans and the model must
  re-propose the identical call, which costs a model call and is not
  guaranteed (a changed input is a new request);
- the node would need to rejoin the tree: today a node that ended is settled
  by its parent (`settle_finished`), terminal records are never overwritten,
  and `recover` marks a dead running node `interrupted`;
- a resume process must be started by someone, which is a new authority.

Kept from serve: the one-shot fingerprint, deny means settle and stop,
the 900 s expiry, a durable audit register. Changed: the queue is durable
and cross-process (not an in-memory `Mutex<Vec>`); the key is the request
and node, not `(session, u64)`; the hash is SHA-256; the approver blocks
instead of denying first. `turn.rs` and `resume_turn` need **no change**:
serve and cron keep their current deny-then-resume path unchanged.
(Parking for hours, which would need exit-and-resume, is out of scope: owner decision 1.)

### 4.3 Timeouts and the current level

Each agent level gets `level_secs` (default 120) to decide, then the request
moves up one level; the owner gets the rest. The total is `approval_secs`
(default 900, matching `EXPIRY_SECS`; configurable up to 3600), capped by the
node's deadline. The current level is **derived**, not stored:
`first eligible level L_i` such that `created + i * level_secs` has not
passed and no `Escalate` skipped it; levels recorded `skipped` are passed
immediately. A decide is checked against the current level under the lock at
append time, and the fold re-derives from the record's `at`. When time runs
out the request is `expired`, and a late decision is refused: **no answer
means denied**.

A level that is not running (settled, interrupted) is `dead` and skipped at
once. The owner can decide at any time while the request is pending.

### 4.4 Crash and resume

| Event | Result |
|---|---|
| Child dies while parked | `Delegation::recover` settles it `interrupted` (whole carve charged, unchanged); its pending requests fold to `expired(node_settled)`; no one-shot survives. The owner resumes the session interactively (`tole mission --resume`), where the owner's own approver decides live. |
| Decider dies after `Decide` | The record is durable; the child's next poll sees it. |
| Child dies after `Consume`, before the tool ran | The call did not run (write-ahead consume), the request is consumed and cannot be reused. The owner resumes or a new delegation re-asks. Fails closed. |
| Torn tail on `approvals.jsonl` | Repaired under the lock like the other ledgers; a half line is never a decision. |
| Parent settles while a child waits | The parent cannot settle `done` with unsettled children; if it ends anyway, the child's requests expire to deny and the child ends `stopped`. |

## 5. The deciding surfaces

### 5.1 An agent level: two tools

Registered only for a node that can have children (`may_delegate`), is not
a discussion participant (no `TOLE_DISCUSSION_PARTICIPANT`), and whose
allow list holds a `delegate_decide` pattern (the owner or the parent put
it there; `child_allow` passes it down only if the parent covers it). A node
without the pattern is `skipped: no_decide_tool` in every chain.

- `delegate_pending` (`Risk::ReadOnly`): requests whose **current level** is
  this node. Output is data: fenced, redacted, capped (the `discuss_status`
  treatment), with `req`, child node and profile, tool, target, `display`,
  the fingerprint prefix and the time left. `delegate_poll` also shows an
  `awaiting_approval` count per child so a polling parent notices.
- `delegate_decide` (`Risk::Write`): input `{request: string, decision:
  "allow" | "deny" | "escalate"}`. **Both fields are closed**: `request` must
  match the id charset and exist; `decision` is an enum; any other field is
  refused (no node id, no level, no free text, no pattern, no "reason"). Its
  authority is the host's, not the model's: the deciding node comes from the
  host's own `TOLE_DELEGATION_NODE`, and `execute` re-runs every check of
  2.2 under the lock. A refusal says why and records nothing.

Because `delegate_decide` is itself `Risk::Write`, it passes through the
deciding level's own approver. At the root that is the owner's approver
(prompt, or `--allow`), so by default an orchestrator cannot approve a
child's Write without the owner seeing the decision; at depth >= 1 the tool
exists only where an allow pattern grants it. There is no free-text note
from a level to the child: a decision carries no model-written string, so a
decider cannot inject into the child's context.

### 5.2 The owner

Two hosts, both owner-only:

- `tole delegation approvals list|show|allow|deny` (new subcommand group).
  `allow` and `deny` call `owner_only::refuse_under_session` first (#684,
  #700): they refuse when `TOLE_UNDER_SESSION` or `TOLE_AGENT_DEPTH >= 1` is
  set, before anything is read or written. `show` prints the full request
  (chain, display, redacted input, fingerprint). `allow` needs a terminal
  "yes" after `show`, or `--fingerprint <12+ hex chars>` for scripts (the
  content-bound pattern of `plugin approve --hash`); a prefix that is
  ambiguous is refused.
- `/approvals` in `tole serve` lists the pending delegation requests next to
  its own (`source: "delegation"`; scanning `<data dir>/delegation/*/approvals.jsonl`,
  at most 64 logs, none when the data dir is unresolved or `approvals` is off),
  and `POST /approvals/{id}/decision` routes `dap-*` ids to the delegation
  queue, with the CLI's rules because it is the same code (`approvals_owner`):
  the route refuses (403) when the serve process itself runs under a session
  (the same marker check), before it looks at the id or the body; the id must
  be a full request id; the body is `{decision, confirm?, fingerprint?}` and
  nothing else; `allow` needs `confirm` equal to the id (no "allow all"), `deny`
  does not; only a pending request is decided (409 with the reason otherwise,
  404 unknown). It writes the `Decide` record with `via: serve`, bound to the
  fingerprint, and keeps the #200 trust statement: the bearer token is approval
  authority. The listing carries the redacted, capped display and the CLI's
  fenced `text`. No resume thread: the child is already waiting, and serve's
  in-memory queue, its ids (`apr-*`) and its deny-then-resume are untouched.
  The SSE event frame of the first draft is not built: a client polls.

A session never reaches either: `run_command` and `job_start` refuse the
tole binary and mark every process they start (#700). The residual of #684
applies unchanged (a copied binary with a cleared environment, an
interpreter writing the file): defense in depth, not a boundary; the real
boundary is #601. The fold's re-validation (2.2) means such a write still
cannot do more than a legal decision could, and cannot decide for a level
that was not current or not eligible.

## 6. Audit

Every record is the audit: request (node, tool, target, display, chain),
each escalation with its reason, the decision with `by` (agent node and level
number, or owner and surface), consumption, expiry. `tole audit <goal>`
gains an approvals section under the tree ("approval dap-...: write_file
src/a.rs, asked by n3 (depth 2), escalated 2->1 (timeout), decided allow by
n1 (level 2, tool), consumed"). The child writes a `fact/approval_<id>`
register on its **own** session when it sees the outcome (the child owns
that file; serve's route writes it from the other side because it hosts the
session), carrying the decider and level. The ledger is authoritative if the
child died first. `tole audit --json` includes the same fields.

## 7. Config and rollout switch

`[delegation] approvals = "off" | "owner" | "chain"` (user config only, like
the other `[delegation]` keys: a cloned repository must not choose how
approvals work), plus `approval_secs` and `approval_level_secs`. `off` is
today's behavior exactly (stdio approver, Writes denied). The switch lets
each PR land without enabling the feature (section 10).

## 8. Depth and budget caps are unchanged

- No new delegation: escalation moves a question, not a child. Depth stays
  structural (`may_delegate`); `delegate_decide` adds no spawn path.
- A parked child holds its live-process slot (so it counts toward
  `max_live_processes`) and its carve stays reserved; a wait cannot extend
  the node's deadline or its token or step budget. Deciders are ancestors,
  which are already running, so a parked child cannot starve its own decider
  in the machine-guard queue.
- A decision never carries budget, allow patterns, secrets or profile
  changes (2.3): it is one allow or deny of one fingerprinted call.

## 9. Threat model rows

To be added to `docs/threat-model.md` ("Delegation approvals (#714)") with
the implementation, one row per line below.

| Threat | Mitigation | Residual |
|---|---|---|
| **Injection through a request text** (a child writes a path, a command or an argument crafted to read as an instruction or a pre-approval to the decider) | The decider sees host-built `display` (tool, resolved target, `describe`), fenced and marked as data, never a model-written reason; a request carries no free-text field at all; the fingerprint, not the text, is what is approved; truncated display makes the request owner-only | A decider model may still be influenced by the plausible-looking content of a legitimate request (a file body). Bounded by 2.2 (only what its allow list already allows, inside the workspace guard) |
| **A child asks for more than its parent holds** | Rule 2.2.3: a level decides only if its own list allows the call; otherwise it cannot decide and the request moves up; at the root the owner decides. A deny pattern of the child or a parent is final and never escalated. Widening (patterns, profile, budget, secrets) is not an approval and is refused or dropped, never escalated | None beyond the owner's own choice to allow |
| **A parent approves on injected instruction** (its context was poisoned) | The parent decides only within its own allow list and the workspace guard; `delegate_decide` is `Risk::Write`, so at the root the owner's approver sees each decision; at depth >= 1 the tool exists only by explicit grant; `deny` and `escalate` are always available; no always-owner class is decidable | A poisoned level can approve what its list already allows, for its own subtree: the same power it has to do it itself or tell the child to. The grant is the owner's to withhold |
| **Replay** (an allow reused for another call, node, session, workspace, or twice) | One-shot: `Consume` is written before Allow is returned; the fingerprint is SHA-256 over node, session, tool, canonical raw input, target and workspace; a changed input is a new request; the fold ignores a second `Consume` and a `Decide` after `expired` | An allow is valid for one call by the one waiting process; there is no later reuse path |
| **Confused deputy** (a level approves in its authority something that acts on the child's workspace, or the owner approves something that reads differently than it runs) | The target is resolved against the **child's** canonical root and must pass the guard; the fingerprint includes the workspace; `display` and fingerprint come from the same host code path as the call; level eligibility is read from the immutable `Node.allow` of the log, not from a request field | The owner confirming a long approval without reading it. Mitigated by the 2 KiB display cap, `show` and `--fingerprint` |
| **Race between levels** (parent decides as the window closes; two levels decide; owner and parent at once) | One lock; first valid `Decide` in log order wins; validity checked at append time against the derived current level and re-checked by the fold; deny is final; the loser gets `race_lost` | None for correctness. Clock skew is single-host, and the check is by log order and the `at` the lock writer set |
| **Orphaned requests** (child or level dies; parent settles; process killed while parked; owner away) | Pending requests of a settled node fold to `expired(node_settled)`; dead levels are skipped; a request always expires (`approval_secs`, capped by the node deadline) to deny; recover settles the node; the log is bounded | A request may stay `pending` in the file until the next fold marks it; nothing can use it. An unattended machine just denies |
| **Flood** (a looping child fills the queue) | At most 1 pending per node and 32 per goal; file cap; each denied request ends that child's turn (`ApprovalRequired`) | A cap hit denies everything new in that goal until the owner clears the file |
| **Forged record** (an interpreter appends a `Decide`) | Fold re-validation; owner-only subcommands refuse under a session; #684 residual | Same as #684: the real boundary is #601 |

## 10. Tests and mutation plan

Same discipline as #715: every rule gets a test, and every test is
mutation-checked (apply the mutation, the named test must fail).

Core (`delegation_approvals_tests.rs`, pure, deterministic, no clock):

| Rule | Test | Mutation that must fail it |
|---|---|---|
| first valid decision wins; later ignored | two `Decide`s, both orders | take the last instead of the first |
| a decide by a non-current level is rejected | parent decides after escalation to grandparent | drop the current-level check |
| a level never decides beyond its own list | child asks `write_file` the parent does not allow; table over allow lists, deny patterns, `@workspace`, command patterns | widen `pattern_verdict` to the child's list; ignore deny |
| always-owner table | secrets, data-dir target, config file, `..` and symlink escape, plugin and MCP tool, truncated display, oversized input | remove any one row of the classifier |
| fingerprint binding | change each of node, session, tool, input key order (equal), input value, target, workspace | drop a field; hash the key order |
| one-shot | `Consume` twice; consume by another node; consume before allow | skip the consumed check |
| expiry denies | now past `expires_at`, late `Decide` | make expiry allow, accept a late decide |
| escalation timing | `level_secs` boundaries, skipped and dead levels | off-by-one, ignore `skipped` |
| orphan | node terminal in the delegation log | ignore node status |
| torn tail, append-only, id uniqueness | crash-injection as in the cron/delegation ledgers | none (property) |
| closed schemas | `delegate_decide` refuses every extra field and bad enum; `delegate_pending` output is fenced and capped | remove the field refusal |

Host (`tole-cli`): the owner subcommand refuses under `TOLE_UNDER_SESSION`
and `TOLE_AGENT_DEPTH` (extends `owner_only_e2e`); `--fingerprint` prefix
rules; serve route refuses under session and routes `dap-` ids; the approver
returns Deny at the deadline, on cancel, and on cap; `interactive()` is
false and Destructive is unregistered for a delegated child; the config
switch `off` equals today's behavior.

End to end (a fake provider and a real child process, as the `delegate`
tests do): child parks, owner allows, the exact call runs once, a second
identical call asks again; owner denies; timeout denies and the mission
stops `stopped`; parent decides within its list; parent cannot decide
beyond it and the request reaches the owner; grandparent chain; kill the
child while parked; kill between `Consume` and execution; a reviewer
participant still has no Write tool.

Gates for each PR: `cargo fmt`, `clippy -D warnings`, `cargo test
--workspace`, `--no-default-features` builds, and the pre-commit `cora
review --staged`.

## 11. Split into small PRs

Each is under about 600 changed lines of code, tests in their own files. All
stay behind `approvals = "off"` until PR 6 flips the default, so nothing
changes for a user before the last step. What a PR does **not** enable stays
denied exactly as today.

| # | PR | Delivers | Still denied after it |
|---|---|---|---|
| 1 | `feat(delegation): approval records and fold in tole-core` | `tole_core::delegation_approvals`: records, fold, chain building from the tree, authority predicate, always-owner classifier, SHA-256 fingerprint, shared `canonical_json`, the validating `append` with the record and log caps, and the `[delegation] approvals` switch (`off`) with `approval_secs` and `approval_level_secs`. Pure and fully unit-tested. | Everything (no host wiring) |
| 2 | `feat(delegate): park a delegated child at an approval (off by default)` | the request builder (display and input caps, one pending request per node and 32 per goal), approvals store in the host, `DelegatedApprover` (park, poll, timeout, cancel, caps, `interactive() == false`), write-ahead `Consume`, orphaning in `recover` | Every request expires to deny; no surface yet decides |
| 3 | `feat(delegate): owner decides delegated approvals with tole` | `tole delegation approvals list\|show\|allow\|deny`, `owner_only` guard, `--fingerprint`, child's `fact/approval_*` register, `tole audit` approvals section; `approvals = "owner"` becomes usable | Agent levels cannot decide; all agent levels are `skipped: no_decide_tool`. Owner-only classes are final here |
| 4 | `feat(serve): /approvals lists and decides delegation requests` | serve GET merge, `dap-` routing, under-session refusal, event frame | Same as 3 |
| 5 | `feat(delegate): agent levels decide within their own authority` | `delegate_pending`, `delegate_decide`, registration rule, level windows and escalation, `delegate_poll` count, `approvals = "chain"` usable | Anything outside a level's list, always-owner classes, Destructive; the default is still `off` |
| 6 | `docs,feat: delegation approvals on by default, threat model, mutation sweep` | default `owner` (or `chain`, see question 2), threat-model section and rows, `automation-design.md` and `epics.md` text, CHANGELOG, a full mutation sweep and the end-to-end tests of section 10 | Reviewer participants stay read-only (question 4); Destructive; every owner-only class |

PR 1 is the riskiest to review (the classifier and the predicate), so it
carries the heaviest tests; PRs 3 and 4 are independent after 2 and can land
in either order.

## 12. Owner decisions

Decided by the owner on 2026-11-14; they replace the open questions of the
first draft of this note.

1. **The child parks in the process, not exit-and-resume.** A child waits at
   most `approval_secs` (default 900, cap 3600 s). Revisit only if a real
   unattended case appears.
2. **`chain` ships available, the default stays `owner`**, also after PR 6:
   agents decide only where the owner granted `delegate_decide`.
3. **External-effect tools (MCP, plugin, skill) are always-owner**, stricter
   than the list in #714; the owner can still allow one call.
4. **Reviewer participants (#704) stay read-only permanently** and never ask
   for a Write; #714 changes `delegate` and `execute` children only.
5. **`delegate_decide` may be allowlisted only by an explicit `--allow
   delegate_decide` typed by the owner** (it is `Risk::Write`, so it prompts
   by default). A profile may list it but, like every pattern, it takes
   effect only where the parent's list covers it.
