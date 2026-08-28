# E8 MVP Gate — Run Retro (2026-08-27)

Issue #33 → PR #34 (merged `74d778a`). The gate is passed: the agent
fixed a real issue end-to-end with one logged session and one logged
manual intervention.

## The run

| Step | Who | Result |
|---|---|---|
| Issue triage + branch | maintainer | issue #33 created, branch `docs/e8-readme-status` |
| Mission prompt | maintainer | issue content embedded (agent cannot read issues) |
| read_file | agent | hash `2fa04d023a98c53b` quoted correctly |
| edit_file | agent | hash-anchored, old_text exact, single line changed |
| read_file verify | agent | final hash `8943e9ff28371ed5` confirmed |
| gh issue comment | agent | **denied** — sandbox token scope (intervention #1) |
| Comment posted | maintainer | agent's comment verbatim on #33 |
| commit + push + PR | maintainer | agent has no git tool (by design this phase) |
| CI + merge | bots | 12/12 green, CodeCora clean, squash-merged |

Durable log: `.tole/e8-sessions/s-1a0419d5d57-13aaf4.jsonl` (replayable).

## What worked

- **Hash anchoring did its job** — the model quoted the read hash without prompting, edit was refused-free, no stale-context failure mode appeared.
- **Risk gates worked** — both Write ops prompted; the Destructive tier stayed untouched (nothing destructive in scope).
- **Durable log + resume** — the run survived a denial (stdin closed in background), resumed cleanly, and completed without losing context.
- **Scope discipline** — only README.md touched; the agent stopped after step 5 exactly as instructed.

## What was lacking → Phase 3 backlog

1. **Agent cannot read issues/PRs** (`gh` tool is write-only: comment/create/pr_create). The mission prompt had to embed the issue body. → add `issue_view` / `pr_view` (RO) ops.
2. **No git tool** — branch/commit/push/PR-open are all maintainer work. → add a gated `git` tool (status/diff/commit; push stays manual initially).
3. **Token scope leak** — the sandbox `GITHUB_TOKEN` env override silently downgrades `gh` writes to 403. → document the `env -u GITHUB_TOKEN` requirement in the runbook, and consider unsetting it in the `gh` tool subprocess for non-CI hosts.
4. **Background stdin kills approvals** — running headless (CI) turns every Write approval into an abort. → add `--allow write:edit_file` style scoped pre-authorization for non-interactive runs, keeping Destructive always-interactive.
5. **No browser/web read tool** — issues referencing external context require manual embedding.

## Verdict

Gate passed on the first real run. Phase 3 (self-sufficiency: read ops,
git, scoped pre-auth) is justified — see `epics.md` E9/E13+.
