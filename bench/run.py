#!/usr/bin/env python3
"""Perf budget harness and gate (#921).

Measures the REAL `tole` binary against a local mock provider (127.0.0.1,
no network, no secrets) and compares the result with bench/budget.toml.

    run.py measure --bin target/release/tole --out report.json   # + gate
    run.py ratchet --base old-budget.toml --head bench/budget.toml
    run.py render  report.json                                   # markdown
    run.py release-report --tag vX.Y.Z reports...                # release table

Metrics (all four are budgeted per platform):
  cold_start_ms   wall time of `tole --version`, median of N spawns
  idle_rss_mb     VmRSS of a `tole serve` daemon that is up and idle
  active_rss_mb   peak RSS (the child's /proc VmHWM) of one `tole run` that makes
                  ROUNDS_LONG provider round-trips, half of them tool calls
  per_turn_ms     slope of `tole run` wall time over provider round-trips:
                  (wall(ROUNDS_LONG) - wall(ROUNDS_SHORT)) / the difference.
                  One round-trip = request build, response parse, one tool
                  call, one durable JSONL commit. The mock answers at once,
                  so provider latency is excluded; its own (small, constant)
                  cost is included.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import resource
import socket
import statistics
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import budget

HERE = Path(__file__).resolve().parent
COLD_RUNS = 15
IDLE_RUNS = 3
WALL_RUNS = 11
ROUNDS_SHORT = 2
ROUNDS_LONG = 30  # a turn is capped at MAX_STEPS = 32 provider steps
SCHEMA = 1


# --- mock provider ---------------------------------------------------------


class _Mock(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.0"
    rounds = 0  # set per server

    def log_message(self, *a):  # silence
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        done = sum(1 for m in body.get("messages", []) if m.get("role") == "tool")
        with self.server.lock:
            self.server.requests += 1
        if done < self.server.rounds:
            msg = {
                "role": "assistant",
                "content": None,
                "tool_calls": [
                    {
                        "id": f"call_{done}",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": json.dumps({"path": "README.txt", "offset": done, "limit": 200})},
                    }
                ],
            }
            finish = "tool_calls"
        else:
            msg = {"role": "assistant", "content": "done"}
            finish = "stop"
        out = json.dumps(
            {
                "id": "mock",
                "object": "chat.completion",
                "model": "bench",
                "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15},
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


class MockProvider:
    """OpenAI-compatible chat endpoint that asks for `rounds` read_file calls,
    then answers. Stateless: the round is derived from the tool results in
    the request, so concurrent or repeated runs need no reset."""

    def __init__(self, rounds: int):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _Mock)
        self.server.rounds = rounds
        self.server.requests = 0
        self.server.lock = threading.Lock()
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}/v1"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def reset(self, rounds: int) -> None:
        with self.server.lock:
            self.server.rounds = rounds
            self.server.requests = 0

    @property
    def requests(self) -> int:
        with self.server.lock:
            return self.server.requests

    def close(self):
        self.server.shutdown()
        self.server.server_close()


# --- measurement -----------------------------------------------------------


class Env:
    def __init__(self, binary: str, root: Path, provider_url: str):
        self.bin = str(Path(binary).resolve())
        self.root = root
        self.work = root / "work"
        self.work.mkdir()
        (self.work / "README.txt").write_text("tole perf bench fixture\n" * 80)
        self.home = root / "home"
        self.home.mkdir()
        self.url = provider_url

    def env(self) -> dict[str, str]:
        keep = {k: v for k, v in os.environ.items() if k in ("PATH", "LANG", "LC_ALL")}
        keep.update(
            HOME=str(self.home),
            XDG_CONFIG_HOME=str(self.home / "cfg"),
            XDG_DATA_HOME=str(self.home / "data"),
            TOLE_BASE_URL=self.url,
            TOLE_MODEL="bench",
            TOLE_API_KEY="sk-bench-not-a-secret",
            TOLE_STREAM="0",
            TOLE_NO_UPDATE_CHECK="1",
        )
        return keep


class HarnessError(RuntimeError):
    """The harness itself did not measure what it claims to measure."""


def _vm_hwm_mb(pid: int) -> float | None:
    """Kernel high-water RSS of a live process, or None once it is gone."""
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith("VmHWM:"):
                return int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return None


def _spawn_wait(cmd: list[str], env: Env, timeout: float = 120.0, track_rss: bool = False) -> tuple[float, float]:
    """Run to exit; return (wall seconds, peak RSS MB). The peak is the child's
    own /proc VmHWM sampled while it runs (every ~1 ms; the mark is monotone,
    so a sample after the peak is exact). It is NOT wait4's ru_maxrss: that
    one inherits the harness (parent) RSS across fork+exec, so it reads the
    parent's size whenever the parent is larger than the child."""
    errf = tempfile.TemporaryFile()
    t0 = time.perf_counter()
    p = subprocess.Popen(
        cmd, cwd=env.work, env=env.env(), stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL, stderr=errf,
    )
    deadline = time.monotonic() + timeout
    peak = 0.0
    while True:
        pid, status, _ru = os.wait4(p.pid, os.WNOHANG)
        if pid:
            break
        if time.monotonic() > deadline:
            p.kill()
            os.wait4(p.pid, 0)
            raise HarnessError(f"timeout: {cmd}")
        if track_rss:
            hwm = _vm_hwm_mb(p.pid)
            if hwm is not None:
                peak = max(peak, hwm)
        time.sleep(0.001)
    wall = time.perf_counter() - t0
    p.returncode = os.waitstatus_to_exitcode(status)
    if p.returncode != 0:
        errf.seek(0)
        raise HarnessError(f"{cmd} exited {p.returncode}: {errf.read().decode(errors='replace')[-500:]}")
    if track_rss and peak <= 0.0:
        raise HarnessError("could not sample the child's VmHWM (/proc unavailable?)")
    return wall, peak


def cold_start_ms(env: Env) -> tuple[float, list[float]]:
    _spawn_wait([env.bin, "--version"], env)  # warm the page cache: "cold" = new process, not cold disk
    xs = [_spawn_wait([env.bin, "--version"], env)[0] * 1000 for _ in range(COLD_RUNS)]
    return statistics.median(xs), xs


def _free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _vm_rss_mb(pid: int) -> float:
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 1024.0
    raise RuntimeError("no VmRSS")


def idle_rss_mb(env: Env) -> tuple[float, list[float]]:
    xs = []
    for _ in range(IDLE_RUNS):
        port = _free_port()
        e = env.env()
        e["TOLE_SERVE_TOKEN"] = "bench-token-not-a-secret"
        p = subprocess.Popen(
            [env.bin, "--sessions-dir", str(env.root / "serve-sessions"), "serve", "--port", str(port)],
            cwd=env.work, env=e, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        try:
            deadline = time.monotonic() + 20
            while True:
                if p.poll() is not None:
                    raise RuntimeError(f"tole serve exited {p.returncode}")
                try:
                    with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1) as r:
                        if r.status == 200:
                            break
                except OSError:
                    pass
                if time.monotonic() > deadline:
                    raise RuntimeError("tole serve never answered /health")
                time.sleep(0.02)
            time.sleep(1.0)  # let start-up allocations settle: this is the idle figure
            xs.append(_vm_rss_mb(p.pid))
        finally:
            p.terminate()
            try:
                p.wait(5)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
    return statistics.median(xs), xs


def check_round_trips(requests: int, rounds: int) -> None:
    """A run asked for `rounds` tool calls must reach the provider exactly
    rounds + 1 times (one per tool call, one for the final answer). Anything
    else means the loop stopped early or the tool-result shape changed, and
    the timing and RSS would describe a different workload."""
    if requests != rounds + 1:
        raise HarnessError(
            f"expected {rounds + 1} provider requests for {rounds} tool round-trips, saw {requests}"
        )


def _run_rounds(env: Env, provider: MockProvider, rounds: int) -> tuple[float, float]:
    provider.reset(rounds)
    sess = env.root / f"sessions-{rounds}-{time.monotonic_ns()}"
    out = _spawn_wait(
        [env.bin, "--sessions-dir", str(sess), "run", "--yes", "read the file until told to stop"],
        env,
        track_rss=True,
    )
    check_round_trips(provider.requests, rounds)
    return out


def session_metrics(env: Env, provider: MockProvider) -> dict:
    _run_rounds(env, provider, ROUNDS_SHORT)  # warm-up
    short = [_run_rounds(env, provider, ROUNDS_SHORT) for _ in range(WALL_RUNS)]
    long_ = [_run_rounds(env, provider, ROUNDS_LONG) for _ in range(WALL_RUNS)]
    w_short = statistics.median(w for w, _ in short)
    w_long = statistics.median(w for w, _ in long_)
    # Noise can make the slope dip below zero on a very fast machine; a
    # negative cost is meaningless, so it is floored at 0 (the comparator
    # rejects a negative measured value outright).
    per_turn = max(0.0, (w_long - w_short) * 1000 / (ROUNDS_LONG - ROUNDS_SHORT))
    return {
        "per_turn_ms": per_turn,
        "active_rss_mb": max(r for _, r in long_),
        "samples": {
            "wall_short_s": [w for w, _ in short],
            "wall_long_s": [w for w, _ in long_],
            "peak_rss_long_mb": [r for _, r in long_],
        },
    }


def measure(binary: str) -> dict:
    provider = MockProvider(ROUNDS_SHORT)
    with tempfile.TemporaryDirectory(prefix="tole-bench-") as d:
        env = Env(binary, Path(d), provider.url)
        try:
            cold, cold_x = cold_start_ms(env)
            idle, idle_x = idle_rss_mb(env)
            sess = session_metrics(env, provider)
        finally:
            provider.close()
    version = subprocess.run([env.bin, "--version"], capture_output=True, text=True).stdout.strip()
    metrics = {
        "cold_start_ms": cold,
        "idle_rss_mb": idle,
        "active_rss_mb": sess["active_rss_mb"],
        "per_turn_ms": sess["per_turn_ms"],
    }
    return {
        "schema": SCHEMA,
        "platform": f"{platform.machine()}-{platform.system().lower()}",
        "cpu": _cpu_model(),
        "cpus": os.cpu_count(),
        "mem_total_mb": _mem_total_mb(),
        "binary": version,
        "binary_size_mb": Path(env.bin).stat().st_size / 1048576,
        "commit": os.environ.get("GITHUB_SHA", ""),
        "rounds": {"short": ROUNDS_SHORT, "long": ROUNDS_LONG},
        "metrics": metrics,
        "samples": {"cold_start_ms": cold_x, "idle_rss_mb": idle_x, **sess["samples"]},
    }


def _cpu_model() -> str:
    try:
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.lower().startswith(("model name", "hardware", "cpu model")):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def _mem_total_mb() -> float:
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) / 1024.0
    except OSError:
        pass
    return 0.0


# --- gate / report ---------------------------------------------------------


def gate(report: dict, budget_text: str) -> tuple[list[budget.Verdict], str]:
    """Attach verdicts to the report. Returns (verdicts, status) where status is
    pass | fail | no-budget. A platform with no budget is reported, not gated."""
    budgets = budget.parse_budget(budget_text)
    ceilings = budgets.get(report["platform"])
    if ceilings is None:
        report["gate"] = {"status": "no-budget", "verdicts": []}
        return [], "no-budget"
    verdicts = budget.compare(report["metrics"], ceilings)
    status = "pass" if all(v.ok for v in verdicts) else "fail"
    report["gate"] = {
        "status": status,
        "verdicts": [
            {"metric": v.metric, "measured": v.measured, "ceiling": v.ceiling, "ok": v.ok,
             "headroom_pct": v.headroom_pct}
            for v in verdicts
        ],
    }
    return verdicts, status


UNITS = {"cold_start_ms": "ms", "idle_rss_mb": "MB", "active_rss_mb": "MB", "per_turn_ms": "ms"}
LABELS = {
    "cold_start_ms": "Cold start (`tole --version`)",
    "idle_rss_mb": "Idle RSS (`tole serve`)",
    "active_rss_mb": "Active-session RSS (peak)",
    "per_turn_ms": "Per-turn overhead",
}


def render(report: dict) -> str:
    """Markdown CPU/RAM table for a release report or a job summary."""
    ver = {v["metric"]: v for v in report.get("gate", {}).get("verdicts", [])}
    lines = [
        f"### Performance: {report['platform']}",
        "",
        f"{report['binary']}, {report['cpu']} ({report['cpus']} vCPU, {report['mem_total_mb']:.0f} MB RAM), "
        f"binary {report['binary_size_mb']:.1f} MB. Mock provider, no network.",
        "",
        "| Metric | Measured | Budget | Headroom |",
        "|---|---:|---:|---:|",
    ]
    for m in budget.METRICS:
        v = ver.get(m)
        val = report["metrics"][m]
        cell = f"{v['ceiling']:g} {UNITS[m]}" if v else "none"
        head = f"{v['headroom_pct']:.0f}%" if v else "n/a"
        mark = "" if not v or v["ok"] else " **OVER**"
        lines.append(f"| {LABELS[m]} | {val:.2f} {UNITS[m]} | {cell} | {head}{mark} |")
    return "\n".join(lines) + "\n"


def fill_template(template: str, tag: str, reports: list[dict]) -> str:
    """Fill release-report-template.md. A platform with no report prints n/a."""
    out = template.replace("{{TAG}}", tag)
    by = {r["platform"].split("-")[0]: r for r in reports}
    for arch in ("x86_64", "aarch64"):
        r = by.get(arch)
        vals = {
            "cold_start_ms": f"{r['metrics']['cold_start_ms']:.1f}" if r else "n/a",
            "idle_rss_mb": f"{r['metrics']['idle_rss_mb']:.1f}" if r else "n/a",
            "active_rss_mb": f"{r['metrics']['active_rss_mb']:.1f}" if r else "n/a",
            "per_turn_ms": f"{r['metrics']['per_turn_ms']:.1f}" if r else "n/a",
            "cpu": r["cpu"] if r else "n/a",
            "cpus": str(r["cpus"]) if r else "n/a",
            "mem_total_mb": f"{r['mem_total_mb']:.0f}" if r else "n/a",
        }
        for k, v in vals.items():
            out = out.replace("{{%s.%s}}" % (arch, k), v)
    return out.replace("{{RENDERED_TABLES}}", "\n".join(render(r) for r in reports))


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    m = sub.add_parser("measure", help="measure the binary, write the report, gate on the budget")
    m.add_argument("--bin", required=True)
    m.add_argument("--out", default="perf-report.json")
    m.add_argument("--budget", default=str(HERE / "budget.toml"))
    m.add_argument("--report-only", action="store_true", help="never fail (still writes the report)")
    r = sub.add_parser("ratchet", help="fail when a ceiling is higher in --head than in --base")
    r.add_argument("--base", required=True)
    r.add_argument("--head", default=str(HERE / "budget.toml"))
    t = sub.add_parser("render", help="print the markdown table for a report")
    t.add_argument("report")
    f = sub.add_parser("release-report", help="fill the release report template from perf reports")
    f.add_argument("--tag", required=True)
    f.add_argument("--template", default=str(HERE / "release-report-template.md"))
    f.add_argument("reports", nargs="+")
    a = ap.parse_args(argv)

    if a.cmd == "release-report":
        reports = [json.loads(Path(p).read_text()) for p in a.reports]
        print(fill_template(Path(a.template).read_text(), a.tag, reports), end="")
        return 0

    if a.cmd == "render":
        print(render(json.loads(Path(a.report).read_text())), end="")
        return 0
    if a.cmd == "ratchet":
        problems = budget.ratchet_violations(
            budget.parse_budget(Path(a.base).read_text()), budget.parse_budget(Path(a.head).read_text())
        )
        for p in problems:
            print(f"::error::perf budget is ratchet-only-downward: {p}")
        return 1 if problems else 0

    report = measure(a.bin)
    _, status = gate(report, Path(a.budget).read_text())
    Path(a.out).write_text(json.dumps(report, indent=2) + "\n")
    print(render(report))
    if status == "no-budget":
        if os.environ.get("CI") == "true" and not a.report_only:
            print(f"::error::no budget for platform {report['platform']} in bench/budget.toml: add one (a gate that skips is no gate)")
            return 1
        print(f"::warning::no budget for platform {report['platform']}: measured, not gated")
    if status == "fail":
        for v in report["gate"]["verdicts"]:
            if not v["ok"]:
                print(f"::error::perf budget exceeded: {v['metric']} {v['measured']:.2f} > {v['ceiling']:g}")
        return 0 if a.report_only else 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
