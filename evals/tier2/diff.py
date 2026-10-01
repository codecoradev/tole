#!/usr/bin/env python3
"""Tier 3: diff current Tier-2 results against a stored baseline.

Usage:
  python3 evals/tier2/diff.py evals/baselines/v0.3.0.json evals/tier2/results.json

Exit 0 = PASS (no regression), exit 1 = REGRESS.

Thresholds:
  - success: must not drop (any mission that passed before and fails now
    is a regression; newly-passing is noted, not an error)
  - prompt_tokens / completion_tokens / tool_calls: must not grow more
    than +25% for still-passing missions (model must be pinned for a
    fair comparison — results.json carries no model field; the runner
    trusts the operator).
"""

import json
import sys
from pathlib import Path


def load(path: str) -> dict:
    p = Path(path)
    if not p.exists():
        raise SystemExit(f"missing file: {p}")
    return json.loads(p.read_text())


def main() -> int:
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    baseline = load(sys.argv[1])
    current = load(sys.argv[2])

    base = {m["mission"]: m for m in baseline["missions"]}
    cur = {m["mission"]: m for m in current["missions"]}

    regressions = []
    notes = []
    for name, old in base.items():
        new = cur.get(name)
        if new is None:
            regressions.append(f"{name}: mission missing from current run")
            continue
        if old["success"] and not new["success"]:
            regressions.append(f"{name}: SUCCESS -> FAIL ({new.get('detail')})")
            continue
        if not old["success"] and new["success"]:
            notes.append(f"{name}: FAIL -> SUCCESS (improvement)")
            # An improvement is not subject to the growth check — flagging
            # higher token usage on a newly-passing mission as a harness
            # regression contradicted the documented scope (CodeCora scan
            # 2026-09-18).
            continue
        for metric in ("prompt_tokens", "completion_tokens", "tool_calls"):
            o, n = old.get(metric, 0), new.get(metric, 0)
            if o:
                if n > o * 1.25:
                    regressions.append(
                        f"{name}: {metric} grew {o} -> {n} (>25% harness regression)"
                    )
            elif n:
                notes.append(
                    f"{name}: {metric} baseline is 0 (now {n}) — growth check skipped"
                )
    for name in cur:
        if name not in base:
            notes.append(f"{name}: new mission (no baseline)")

    for n in notes:
        print(f"NOTE: {n}")
    if regressions:
        for r in regressions:
            print(f"REGRESS: {r}")
        return 1
    print("PASS: no harness regression vs baseline")
    return 0


if __name__ == "__main__":
    sys.exit(main())
