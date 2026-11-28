# Running a mission unattended

`tole mission` chains turns toward a goal (see [Mission mode](../README.md#mission-mode)).
Started in the background, from CI or by a script, nobody is there to press
`y`, and two things decide whether it finishes: **which tools you pre-approve**
and **what `--verify` checks**. The first real coding mission (#1223: one function, an
index migration and tests, on 0.12.0, in the background) hit the `todo_write`
and `job_*` stalls, the goal required on `--resume`, two budget ceilings and a
verify gate that failed twice before it passed. The rest of this page is read
from the code of the tree at `develop` of 2026-11-26 (references are to it), not
observed in that run. What #1223 saw: the `todo_write` stall, the `job_*` stall,
the goal required on `--resume`, two ceilings and the gate failing twice.
Everything else here, the end-of-file behaviour included, is derived from the
code.

## 1. Approvals when nobody can answer

A mission asks for a Write the way `tole run` does: the allow list first, then
a `[y/N]` prompt on stdout with the answer read from stdin
(`InteractiveApprover::stdio` in `crates/tole-cli/src/approver.rs`; the prompt
and the read are `ask` in `crates/tole-cli/src/stdio_prompt.rs`, which denies on
end of file, on a read error and on any answer but `y` or `yes`). Two ways it
goes wrong headless:

- **stdin stays open and nobody types**: the mission waits at the prompt. It
  looks like a hang at step 1.
- **stdin is at end of file** (a closed pipe, `< /dev/null`): the read gives
  no answer and the call is denied (fail closed). A denied call ends the turn
  as `ApprovalRequired`, and the mission settles `stopped` and exits nonzero.
  stderr names the tool: `mission: turn settled: ApprovalRequired { name: "..." }`.

Start an unattended mission with `< /dev/null` so a missing pre-approval stops
it at once with a name, instead of waiting for ever. A `stopped` session stays
resumable (section 3).

`--yes` would end the question for every Write (`--yes` auto-allows each Write
tool, `run_command` and `git` included), so most unattended runs use a narrow
allow list instead. `Destructive` tools (`delete_file`) are never covered by
`--yes`, `--allow` or `--trust`: they always prompt, so with stdin at end of
file they are denied and stop the mission, and with stdin open they wait at the
prompt.

### What `--trust workspace` does not cover

`--trust workspace` is `read_only` plus `write_file` and `edit_file` inside
the `--workspace` jail (and not for dot-paths, build files or scripts, which
keep prompting; `crates/tole-cli/src/config/trust.rs`). It needs an explicit
`--workspace <dir>` that is not your home or `/`. Everything else a mission
normally does still asks:

| Tool | Risk | Covered by `--trust workspace`? | Add |
|---|---|---|---|
| `todo_write` | Write | no | `--allow todo_write` |
| `job_start` | Write | no (`job_poll`, ReadOnly, is) | `--allow 'job_*'` |
| `run_command` | Write | no | `--allow run_command`, or one command: `--allow 'run_command:cargo test *'` |
| `git` | Write | no | `--allow git` |
| `delete_file` | Destructive | never | nothing; it stops the mission |

`todo_write` matters most: the mission prompt tells the model to keep its plan
with it, so a mission without that allow stops at its first step.
`--trust internal` covers `todo_write` and `job_*` too, but also `uteke_*`,
`mcp_cora_*`, `agent_poll` and `tole_session_*`, and none of the file writers;
presets add up (`--trust internal --trust workspace`).

The tool is called `git`. `--allow 'git_*'` is a glob over tool names and
matches no tool of this tree. Patterns are globs over tool names, with two
special forms, `run_command:<argv>` (matches the command by argument) and
`<tool>@workspace` (a resolved-path guard; `--trust workspace` is built on it),
and `!` to deny (README, "Useful flags"); a tool you did not name keeps asking.

### The recipe

```bash
cd /path/to/worktree
tole mission --workspace . --sessions-dir /path/to/sessions \
  --trust workspace \
  --allow todo_write --allow 'job_*' --allow run_command --allow git \
  --verify /usr/local/bin/mission-gate.sh \
  "<goal>" < /dev/null
```

- The gate runs as `sh -c <command>`, so it must be executable (`chmod +x`).
  Under the default sandbox on a bubblewrap host it sees only the system
  directories (`/usr`, `/bin`, `/lib*`, `/sbin`), the workspace and a fresh `/tmp`
  and `$HOME`; the gate's own directory is not bound. A gate in some other directory
  (`/home/x/gates`) is not there and ends in exit 127 on every run. Keep a
  tamper-proof gate in a system directory such as `/usr/local/bin`, or in the
  workspace's `.tole/` (bound read-only, but only while sandboxed), or set
  `verify = "off"` ([sandbox-design.md](sandbox-design.md) §9.4).
- `--workspace` is the jail of the file tools. `run_command`, `git` and the
  `--verify` command run in the process's working directory, so start tole in
  the checkout you want changed. There is no `--worktree` flag.
- If `[sandbox.consumers]` in your user config turns a command consumer on
  (`run_command`, `job_start`, ...), the `git` tool refuses a linked git
  worktree and any layout other than a plain `.git` directory at the root
  ([sandbox-design.md](sandbox-design.md) §9.3). An implicit default `verify`
  does not count as a consumer written in config. Use a clone in that case.
- With cora (>= 0.20.0) on the PATH, attached automatically unless `--no-auto-mcp`, the
  review gate is on by default (`block`): a `git commit` (also through
  `run_command` or `job_start`) is refused until the staged diff has been
  reviewed. That is a tool refusal, not an approval stop, so the mission goes
  on and the model sees the refusal; if it repeats the identical commit three
  times in a row the loop guard settles the mission `stopped`. The gate fails
  open on tooling trouble. Review before the commit, or pass `--review-gate off`
  (or `warn`) for the run (README, "Review gate before commit").
- A mission refuses `--on-pretool`, `--on-posttool`, `--on-turnend` and
  `--memory`.
- A project `.tole/config.toml` must be trusted before a headless run:
  without a terminal tole refuses an untrusted config and runs nothing
  (`tole config trust`; README, "Configuration (project and user files)").

## 2. The verify gate

`--verify <cmd>` (or `[mission] verify` in the config) is the only ground
truth a mission has. How the loop uses it (`crates/tole-cli/src/mission.rs`):

- It runs (`sh -c`) after every turn that ends in a final answer, not only
  when the model says it is done.
- The mission completes only when the command **exits 0 and** the same turn's
  final message has a line that is `MISSION_COMPLETE` on its own (trimmed). A
  pass without the marker keeps the mission going; the marker without a pass
  does too.
- On a failure the model gets `verify FAILED (exit N):` and the last 800
  characters of the command's output (stdout, then stderr), which it uses to
  fix the cause and try again. Three failures in one run end it as
  `verify_failed`; they add up and a pass does not reset the count. A run that
  does not finish in `--verify-timeout` seconds (default 300), or that cannot
  be spawned unsandboxed, counts as a failure and the model gets the error text
  instead of an output tail. A sandbox that exists but cannot run the gate is
  not a failure of the gate: see `verify_refused`.
- Without `--verify`, the marker alone completes the mission.

So: **always pass a gate whose only success path is exit 0.** A gate that
always exits 0 (`true`, `echo ok`, a script that swallows the test runner's
status) lets the model's own `MISSION_COMPLETE` through: a false completion
with a green exit code. tole does not check for this. It does not warn when no
gate is set (tole has no such warning today), and it does not look inside the
command.

A gate worth passing:

- runs the real checks, such as `cargo test`, the linter and a build, and
  returns their status (`set -euo pipefail` in a script; no `| tee` that
  replaces it);
- fails when it cannot run what it checks (a missing test binary is a failure,
  not a skip);
- prints enough on failure to act on, since the tail of that output is all the
  model sees;
- lives outside the workspace when the model must not be able to change it. A
  script inside the writable workspace can be rewritten by the agent, and the
  sandbox does not protect it ([sandbox-design.md](sandbox-design.md) §9.4).

The gate has no secrets unless a `[secrets]` entry grants `verify` one. By
default it also runs sandboxed where it can (`[sandbox.consumers] verify` is
`optional`: sandboxed where bubblewrap works, unsandboxed with a warning on a
host with no sandbox at all): a `cargo` gate cannot see `~/.cargo` there, so
set `verify = "off"` in the user config or make the gate self-contained (README,
"Sandbox" in the status list at the top; [sandbox-design.md](sandbox-design.md)
§9.4). When a backend exists but cannot run the gate (a refused workspace
layout or grant, a launcher that fails), the mission stops as `verify_refused`,
never as a pass; only the default, on a host with no backend at all, falls back
to unsandboxed. A preflight checks the sandbox plan (backend, layout, grant)
before the first turn; it never checks that the gate path exists, and a
launcher that fails at run time stops the mission as `verify_refused` after a
turn.

## 3. Budgets, statuses and resuming

How a mission ended is its `status` (printed at the end, and in the
`fact/mission` register that `tole status` renders). Only `complete` exits 0.

| Status | Meaning | Next step |
|---|---|---|
| `complete` | marker declared and, with `--verify`, the gate passed | review the diff |
| `exhausted_steps`, `exhausted_tokens`, `exhausted_time` | `--max-steps`, `--max-tokens` or `--max-minutes` reached | resume with a larger `--max-steps` or `--max-tokens`; `--max-minutes` restarts per invocation anyway |
| `exhausted_turn_cap` | three turns in a row used the per-turn step cap (32 steps) without finishing | look at the transcript, then resume |
| `verify_failed` | the gate failed three times | read the last failure, fix the gate or the goal |
| `verify_refused` | the owner's sandbox could not run the gate | fix the sandbox setting |
| `stopped` | a turn settled for a reason the mission does not continue from: a denied approval, the loop guard, a provider failure, an unknown tool, a blocked stop gate, a cancel, or the 120-poll budget | read the `mission: turn settled:` line on stderr |
| `error` | a storage failure or a loop error | the session file is the record |

A turn that hits the per-turn step cap does not end the mission: it gets a
"produce the deliverable now" nudge and the mission goes on; only the third such
turn in a row ends it as `exhausted_turn_cap`. A turn that ends in a final
answer resets that count (`cap_trips = 0` in `mission.rs`); the count also
starts again on `--resume`.
Every status reached inside the loop (`error` included) writes a summary to the
session; a failure before the loop (provider config, creating or opening the
session) returns an error and writes none. A stopped or exhausted mission can
be resumed.

### Resuming

```bash
tole mission --resume <session-id> --sessions-dir /path/to/sessions \
  --max-steps 320 --max-tokens 8000000 \
  --workspace . --trust workspace \
  --allow todo_write --allow 'job_*' --allow run_command --allow git \
  --verify /usr/local/bin/mission-gate.sh \
  "<goal>" < /dev/null
```

- **The goal is still required** (the positional argument; there is no
  `--goal-file`). On a resume it is not sent to the model again, since the
  original is pinned in the session's system prompt, but it is recorded in the
  new summary. Pass the same text. The session keeps its pinned system prompt,
  which names the original `--verify` command: resume with the same gate.
  The session is looked up in `--sessions-dir` (the same one as the first run).
- **Approvals are not stored in the session.** Repeat `--trust` and `--allow`.
  If the mission stopped on a denied call, add the pattern it names.
- **Steps and tokens count the whole session**, from its durable usage
  ledger. Resuming an `exhausted_steps` or `exhausted_tokens` mission with the
  same ceilings stops again before the first turn: pass larger
  `--max-steps` / `--max-tokens`. `--max-minutes` restarts with each
  invocation, and so does the count of verify failures.
- Defaults are 160 steps, 60 minutes and 4,000,000 tokens
  (README, "Mission mode").

### Keeping a long mission affordable

Each step sends the history again, so cost per step grows with the
transcript. tole compacts between turns once a prompt reaches `--compact-at`
(default 0.8) of the model's context window, which it learns from
`TOLE_CONTEXT_WINDOW` or the provider's `/models`. A gateway whose `/models`
reports no window never compacts on its own: set `TOLE_CONTEXT_WINDOW`
(README, `TOLE_CONTEXT_WINDOW`). Compaction follows the context window, not the
budget; nothing compacts because a ceiling is near.

## See also

- [approving-delegated-work.md](approving-delegated-work.md): a child that
  `delegate` starts is a headless run too, and can ask the owner instead of
  being denied when `[delegation] approvals` is on.
- [crash-resume.md](crash-resume.md): what a kill leaves in a mission session
  and what recovers it (`tole resume <id>` first, then `tole mission --resume`).

## What is not here

Open asks of #1223 that need code, not a page: the recipe in `tole mission
--help`, a goal that is optional with `--resume` (or `--goal-file`), a
warning at start when no gate is set or the gate is trivially `exit 0`, and
compaction or a warning as a budget ceiling nears. Until those land, this
page and the flags above are the whole answer.
