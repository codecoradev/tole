#!/usr/bin/env python3
"""Read-only report: how often do agents reach for shell features in
`run_command` (issue #531)?

`run_command` splits its input to argv and runs it with no shell, so
`a | b`, `cd sub && cmd`, `cmd > file` are literal argv characters. This
script answers, from session JSONL alone (no runtime logging), how many
`run_command` / `job_start` inputs were shell-shaped and what happened
next. It only reads; it never writes into the sessions it scans.

Usage:
  python3 evals/shell_usage.py <session-dir-or-file>...   [--examples N] [--json]

Each path is scanned recursively for `*.jsonl`. A session line is either
one entry object or an array of them (batched appends); both are read.

Categories (an input can match several; `any` counts it once):
  pipe        `|`            chain      `&&` `||` `;`
  redirect    `>` `>>` `<`   subst      `$(` or a backtick (unquoted / double-quoted)
  cd          leading `cd `  wrapper    `sh|bash|zsh -c ...` (a shell smuggled in as argv)
Operators inside quotes are literal and are NOT counted, so
`grep "a|b" f` is not a pipe.

"failed" = the matching tool_result has ok=false. "after_failed" = the call
directly follows a failed run_command in the same session (a retry signal).
"""

import argparse
import json
import shlex
import sys
from collections import Counter
from pathlib import Path

TOOLS = ("run_command", "job_start")
SHELLS = {"sh", "bash", "zsh", "dash", "ksh", "fish"}
CATS = ("pipe", "chain", "redirect", "subst", "cd", "wrapper")


def _has_subst(cmd: str) -> bool:
    """`$(` or a backtick outside single quotes (the shell expands them
    inside double quotes too)."""
    single = False
    i = 0
    while i < len(cmd):
        c = cmd[i]
        if c == "\\":
            i += 2
            continue
        if c == "'":
            single = not single
        elif not single and (c == "`" or (c == "$" and cmd[i + 1 : i + 2] == "(")):
            return True
        i += 1
    return False


def classify(cmd: str) -> set:
    """Shell-feature categories present in one command line."""
    found = _unquoted_ops(cmd)
    try:
        toks = shlex.split(cmd, posix=True)
    except ValueError:
        toks = []
    if toks and toks[0] == "cd":
        found.add("cd")
    if toks and Path(toks[0]).name in SHELLS and "-c" in toks[1:3]:
        found.add("wrapper")
    if _has_subst(cmd):
        found.add("subst")
    return found


def _unquoted_ops(cmd: str) -> set:
    """Categories whose operator character appears outside any quotes."""
    out = set()
    q = None
    i = 0
    while i < len(cmd):
        c = cmd[i]
        if q:
            if c == "\\" and q == '"':
                i += 2
                continue
            if c == q:
                q = None
        elif c == "\\":
            i += 2
            continue
        elif c in "'\"":
            q = c
        elif c == "|":
            out.add("chain" if cmd[i + 1 : i + 2] == "|" else "pipe")
            if cmd[i + 1 : i + 2] == "|":
                i += 1
        elif c in "&;":
            out.add("chain")
        elif c in "<>":
            out.add("redirect")
        i += 1
    return out


def iter_entries(path: Path):
    """Entries of one session file, in order; bad lines are skipped."""
    for line in path.read_text(errors="replace").splitlines():
        line = line.strip().rstrip(",")
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        for e in obj if isinstance(obj, list) else [obj]:
            if isinstance(e, dict) and e.get("kind") == "entry":
                yield e


def scan_session(path: Path):
    """Yield one record per run_command/job_start intent in the session."""
    intents = []  # in order
    results = {}  # intent id -> ok
    for e in iter_entries(path):
        p = e.get("payload") or {}
        if e.get("type") == "intent" and p.get("tool") in TOOLS:
            cmd = (p.get("input") or {}).get("command")
            if isinstance(cmd, str):
                intents.append({"id": e.get("id"), "tool": p["tool"], "command": cmd})
        elif e.get("type") == "tool_result" and e.get("parentId"):
            results[e["parentId"]] = bool(p.get("ok"))
    prev_failed = False
    for it in intents:
        cats = classify(it["command"])
        ok = results.get(it["id"])
        yield {
            "session": str(path),
            "command": it["command"],
            "cats": sorted(cats),
            "failed": ok is False,
            "after_failed": prev_failed,
        }
        prev_failed = ok is False


def collect(paths):
    files = []
    for p in map(Path, paths):
        files += sorted(p.rglob("*.jsonl")) if p.is_dir() else [p]
    sessions, rows = 0, []
    for f in files:
        sessions += 1
        rows += list(scan_session(f))
    return sessions, rows


def summarize(sessions, rows):
    s = {
        "sessions": sessions,
        "calls": len(rows),
        "any": sum(1 for r in rows if r["cats"]),
        "any_failed": sum(1 for r in rows if r["cats"] and r["failed"]),
        "plain_failed": sum(1 for r in rows if not r["cats"] and r["failed"]),
        "after_a_failed_call": sum(
            1 for r in rows if r["after_failed"]
        ),
    }
    for c in CATS:
        s[c] = sum(1 for r in rows if c in r["cats"])
    return s


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--examples", type=int, default=5)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args(argv)
    sessions, rows = collect(a.paths)
    s = summarize(sessions, rows)
    if a.json:
        json.dump(s, sys.stdout, indent=2)
        print()
        return 0
    n = s["calls"] or 1
    print(f"sessions scanned: {s['sessions']}   run_command/job_start calls: {s['calls']}")
    print(f"shell-shaped: {s['any']} ({100 * s['any'] / n:.1f}%)")
    for c in CATS:
        print(f"  {c:9} {s[c]}")
    print(f"failed, shell-shaped: {s['any_failed']}   failed, plain: {s['plain_failed']}")
    print(f"run_command calls that directly follow a failed one: {s['after_a_failed_call']}")
    shown = [r for r in rows if r["cats"]][: a.examples]
    for r in shown:
        print(f"  e.g. [{','.join(r['cats'])}{' FAILED' if r['failed'] else ''}] {r['command'][:100]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
