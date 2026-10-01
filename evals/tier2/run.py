#!/usr/bin/env python3
"""Tier 2 eval runner (issue #73): small live missions, scored.

Requires: freshly built release binary (see CONTRIBUTING binary hygiene),
provider env (TOLE_* / OPENAI_*). NEVER run in CI (costs money).

Usage:
  python3 evals/tier2/run.py --all
  python3 evals/tier2/run.py --mission read_and_report
  python3 evals/tier2/run.py --all --out results.json
  python3 evals/tier2/run.py --all --archive          # + trace archive
  TOLE_EVAL_BINARY=/path/to/tole python3 evals/tier2/run.py --all

With --archive (issue #107), a REDACTED copy of each mission's session
JSONL is kept under evals/traces/<model>/<mission>/ (secret-shaped
tokens and host session paths scrubbed) for offline replay scoring.
The scorer bin is feature-gated (issue #118) — build it with:
  cargo build -p tole-cli --features replay --bin tole-replay
then:
  target/debug/tole-replay --traces evals/traces \
    --revised evals/replay/prompts/<candidate>.txt

The incumbent (shipped default prompt) is always evaluated in the same
run — extracted from the CLI source, never from a copy that can drift.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Optional

REPO = Path(__file__).resolve().parents[2]
# Binary override for eval sessions on shared hosts (issue #107): point
# TOLE_EVAL_BINARY at the freshly built release binary instead of
# whichever stale target/release happens to exist.
BINARY = Path(os.environ.get("TOLE_EVAL_BINARY", str(REPO / "target/release/tole")))
RESULTS_DEFAULT = Path(__file__).parent / "results.json"
# Trace archive root for --archive (Tier 2.5 replay, issue #107).
ARCHIVE_DEFAULT = REPO / "evals" / "traces"
TIMEOUT_SECS = 300

# Secret-shaped tokens must never reach the archive. Per-family shapes,
# length-guarded so prose ("sketching", "risk-management") cannot match:
# provider/API keys (sk-…, rk-…), GitHub token families
# (ghp_/gho_/ghs_/ghu_/ghr_, github_pat_), GitLab (glpat-), Slack
# (xoxb-/xoxp-/xoxa-), Google (AIza…), AWS access key IDs (AKIA…).
_SECRET_SHAPED = re.compile(
    r"\b(?:sk|rk)-[A-Za-z0-9_\-]{12,}"
    r"|gh[poshur]_[A-Za-z0-9]{20,}"
    r"|github_pat_[A-Za-z0-9_]{20,}"
    r"|glpat-[A-Za-z0-9_\-]{20,}"
    r"|xox[abpr]-[A-Za-z0-9\-]{10,}"
    r"|AIza[0-9A-Za-z_\-]{20,}"
    r"|AKIA[0-9A-Z]{16}"
)


def resolved_model() -> Optional[str]:
    """The model this run uses (same resolution order as tole: TOLE_* then
    OPENAI_*). Recorded into results.json so Tier-3 diffs are honest
    about what they compare (diff.py documents the operator-trust gap)."""
    for prefix in ("TOLE", "OPENAI"):
        v = os.environ.get(f"{prefix}_MODEL", "").strip()
        if v:
            return v
    return None


def redact_line(line: str, host_paths: list[str]) -> str:
    # Keep only the token FAMILY (sk-, ghp_, github_pat_, …) so the
    # archive is still greppable by shape — the secret material itself is
    # fully dropped, never partially kept.
    def stub(m: "re.Match[str]") -> str:
        tok = m.group(0)
        fam = re.match(
            r"sk-|rk-|gh[poshur]_|github_pat_|glpat-|xox[abpr]-|AIza|AKIA", tok
        )
        return (fam.group(0) if fam else "") + "***REDACTED***"

    out = _SECRET_SHAPED.sub(stub, line)
    for p in host_paths:
        if p:
            out = out.replace(p, "<HOSTPATH>")
    return out


def archive_trace(
    sessions_dir: Path, mission: str, archive_root: Path, model: Optional[str]
) -> Optional[Path]:
    """Copy the (redacted) session JSONL into
    archive_root/<model>/<mission>/ + write meta.json. Returns the copy
    path, or None when the run produced no session file."""
    log = session_file(sessions_dir)
    if log is None:
        return None
    dest_dir = archive_root / (model or "unknown-model") / mission
    dest_dir.mkdir(parents=True, exist_ok=True)
    host_paths = [str(sessions_dir)]
    dest = dest_dir / log.name
    lines = []
    for raw in log.read_text().splitlines():
        lines.append(redact_line(raw, host_paths) if raw.strip() else raw)
    dest.write_text("\n".join(lines) + ("\n" if lines else ""))
    meta = {
        "mission": mission,
        "model": model,
        "session": log.name,
        "recorded_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
        "source": "evals/tier2/run.py --archive (issue #107)",
    }
    (dest_dir / f"{log.stem}.meta.json").write_text(json.dumps(meta, indent=1) + "\n")
    return dest

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


def run_mission(
    name: str,
    workspace: Path,
    archive_root: Optional[Path] = None,
    model: Optional[str] = None,
) -> dict:
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
    out["model"] = model
    # Tier 2.5 (issue #107): before deleting the session dir, archive a
    # REDACTED copy for offline trace replay (evals/replay/).
    if archive_root is not None:
        archived = archive_trace(sessions, name, archive_root, model)
        out["archived_trace"] = str(archived) if archived else None
    # Clean the per-mission session dir (CodeCora scan 2026-09-18:
    # mkdtemp dirs were never removed).
    shutil.rmtree(sessions, ignore_errors=True)
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--mission", choices=sorted(MISSIONS))
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--out", default=str(RESULTS_DEFAULT))
    ap.add_argument(
        "--archive",
        nargs="?",
        const=str(ARCHIVE_DEFAULT),
        default=None,
        metavar="DIR",
        help="archive a redacted copy of each mission trace to "
        "DIR/<model>/<mission>/ for offline replay (default: evals/traces)",
    )
    ns = ap.parse_args()
    if not ns.all and not ns.mission:
        ap.error("pick --all or --mission")

    if not BINARY.exists():
        raise SystemExit(f"release binary missing: {BINARY} — build it first")

    model = resolved_model()
    archive_root = Path(ns.archive) if ns.archive else None
    names = sorted(MISSIONS) if ns.all else [ns.mission]
    results = {
        "generated_at": datetime.now().astimezone().isoformat(timespec="seconds"),
        "model": model,
        "missions": [],
    }
    for name in names:
        with tempfile.TemporaryDirectory(prefix="tole-eval-ws-") as ws:
            print(f"=== {name} ===", flush=True)
            results["missions"].append(
                run_mission(name, Path(ws), archive_root, model)
            )

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
