#!/usr/bin/env python3
"""Tier 2 eval runner (issue #73): small live missions, scored.

Requires: freshly built release binary (see CONTRIBUTING binary hygiene),
provider env (TOLE_* / OPENAI_*). NEVER run in CI (costs money).

Usage:
  python3 evals/tier2/run.py --all
  python3 evals/tier2/run.py --mission read_and_report
  python3 evals/tier2/run.py --all --out results.json
"""

import argparse
import json
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path
from typing import Optional

REPO = Path(__file__).resolve().parents[2]
BINARY = REPO / "target/release/tole"
RESULTS_DEFAULT = Path(__file__).parent / "results.json"
TIMEOUT_SECS = 300

# Mission spec: name -> (prompt, judgefn(out: dict) -> (ok, detail))
# `out` fields: text (final answer), stdout, session (durable log parsed)


def judge_read_and_report(out: dict):
    ok = "ALPHA-77" in out["text"]
    return ok, "answer must contain the planted marker"


def judge_echo_chain(out: dict):
    ok = out["tool_calls"] >= 2 and "CHAIN-OK" in out["text"]
    return ok, "must call a tool at least twice and confirm"


def judge_crash_resume(out: dict):
    # The runner resumes after a synthetic failure; success = completed
    # the mission in the SAME session (runner asserts session count == 1).
    ok = out["sessions"] == 1 and "RESUMED-DONE" in out["text"]
    return ok, "must finish in the same durable session after recovery"


MISSIONS = {
    "read_and_report": judge_read_and_report,
    "echo_chain": judge_echo_chain,
    "crash_resume": judge_crash_resume,
}


def run_tole(args: list[str], timeout: int = TIMEOUT_SECS) -> tuple[int, str, str]:
    try:
        proc = subprocess.run(
            [str(BINARY), *args], capture_output=True, text=True, timeout=timeout
        )
    except subprocess.TimeoutExpired:
        # A hung mission is a failed mission, not a crashed runner:
        # record the timeout and let remaining missions finish.
        return -1, "", f"timeout after {timeout}s"
    return proc.returncode, proc.stdout, proc.stderr


def session_file(sessions_dir: Path) -> Optional[Path]:
    files = sorted(sessions_dir.glob("*.jsonl"), key=lambda p: p.stat().st_mtime)
    return files[-1] if files else None


def durable_metrics(log: Path) -> dict:
    """Parse the JSONL session log: token totals + tool-call count."""
    prompt = completion = 0
    tool_calls = 0
    for line in log.read_text().splitlines():
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        records = rec if isinstance(rec, list) else [rec]
        for r in records:
            kind = r.get("kind")
            if kind == "usage":
                u = r.get("usage", {})
                prompt += u.get("prompt_tokens", 0) or 0
                completion += u.get("completion_tokens", 0) or 0
            elif kind == "entry" and r.get("type") == "intent":
                tool_calls += 1
    return {
        "prompt_tokens": prompt,
        "completion_tokens": completion,
        "tool_calls": tool_calls,
    }


def run_mission(name: str, workspace: Path) -> dict:
    sessions = Path(tempfile.mkdtemp(prefix=f"tole-eval-{name}-"))
    started = time.time()

    if name == "read_and_report":
        fixture = workspace / "mission-data.txt"
        fixture.write_text("The secret marker is ALPHA-77.\n")
        prompt = (
            "Use read_file on mission-data.txt, then reply with exactly "
            "the secret marker it contains and nothing else."
        )
        args = ["--workspace", str(workspace), "--sessions-dir", str(sessions),
                "run", "--allow", "read_file", prompt]
        code, stdout, stderr = run_tole(args)

    elif name == "echo_chain":
        prompt = (
            "Use run_command with `echo step-1` and then again with "
            "`echo step-2`. After both succeed reply with exactly: CHAIN-OK"
        )
        args = ["--workspace", str(workspace), "--sessions-dir", str(sessions),
                "run", "--allow", "run_command", prompt]
        code, stdout, stderr = run_tole(args)

    elif name == "crash_resume":
        # Real failure-recovery path: the target file does NOT exist at
        # start (the file that existed pre-run made the failure branch
        # unreachable — CodeCora). The model must observe the read
        # failure, create the file via run_command, read again
        # successfully, and finish in the SAME durable session.
        prompt = (
            "Try read_file on marker.txt (expect an error at first). "
            "After that error, create marker.txt containing "
            "RESUMED-DONE using run_command (printf > marker.txt), "
            "read it again with read_file, then reply with exactly: "
            "RESUMED-DONE"
        )
        args = ["--workspace", str(workspace), "--sessions-dir", str(sessions),
                "run", "--allow", "read_file", "--allow", "run_command", prompt]
        code, stdout, stderr = run_tole(args)
    else:
        raise SystemExit(f"unknown mission {name}")

    log = session_file(sessions)
    metrics = (
        durable_metrics(log)
        if log
        else {"prompt_tokens": 0, "completion_tokens": 0, "tool_calls": 0}
    )
    text = "\n".join(
        line for line in stdout.splitlines() if not line.startswith("session:")
    ).strip()

    out = {
        "mission": name,
        "exit_code": code,
        "text": text[-500:],
        "stderr_tail": stderr[-300:],
        "sessions_dir": str(sessions),
        "duration_secs": round(time.time() - started, 1),
        **metrics,
    }
    sessions_count = len(list(sessions.glob("*.jsonl")))
    ok, detail = MISSIONS[name]({**out, "sessions": sessions_count})
    out["success"] = ok
    out["detail"] = detail
    # Clean the per-mission session dir (CodeCora scan 2026-09-18:
    # mkdtemp dirs were never removed).
    shutil.rmtree(sessions, ignore_errors=True)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mission", choices=sorted(MISSIONS))
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--out", default=str(RESULTS_DEFAULT))
    ns = ap.parse_args()
    if not ns.all and not ns.mission:
        ap.error("pick --all or --mission")

    if not BINARY.exists():
        raise SystemExit(f"release binary missing: {BINARY} — build it first")

    names = sorted(MISSIONS) if ns.all else [ns.mission]
    results = {
        "generated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "missions": [],
    }
    for name in names:
        with tempfile.TemporaryDirectory(prefix="tole-eval-ws-") as ws:
            print(f"=== {name} ===", flush=True)
            results["missions"].append(run_mission(name, Path(ws)))

    out_path = Path(ns.out)
    out_path.write_text(json.dumps(results, indent=1))
    ok = all(m["success"] for m in results["missions"])
    print(json.dumps([{ "mission": m["mission"], "success": m["success"],
                        "prompt_tokens": m["prompt_tokens"],
                        "completion_tokens": m["completion_tokens"],
                        "tool_calls": m["tool_calls"]} for m in results["missions"]], indent=1))
    print(f"results: {out_path}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
