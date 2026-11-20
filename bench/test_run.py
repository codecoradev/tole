import unittest

import run

BUDGET = "".join(
    f"[x86_64-linux.{m}]\nceiling = {c}\n"
    for m, c in (("cold_start_ms", 10), ("idle_rss_mb", 20), ("active_rss_mb", 40), ("per_turn_ms", 10))
)


def report(platform="x86_64-linux", **over):
    m = {"cold_start_ms": 3.0, "idle_rss_mb": 15.0, "active_rss_mb": 28.0, "per_turn_ms": 4.0, **over}
    return {
        "platform": platform, "binary": "tole 0", "cpu": "cpu", "cpus": 2, "mem_total_mb": 1024.0,
        "binary_size_mb": 9.0, "metrics": m,
    }


class Gate(unittest.TestCase):
    def test_within_budget_passes(self):
        r = report()
        _, status = run.gate(r, BUDGET)
        self.assertEqual(status, "pass")
        self.assertEqual(r["gate"]["status"], "pass")

    def test_one_metric_over_fails_the_gate(self):
        r = report(idle_rss_mb=20.5)
        _, status = run.gate(r, BUDGET)
        self.assertEqual(status, "fail")
        bad = [v["metric"] for v in r["gate"]["verdicts"] if not v["ok"]]
        self.assertEqual(bad, ["idle_rss_mb"])

    def test_platform_without_budget_is_reported_not_gated(self):
        r = report(platform="aarch64-linux")
        _, status = run.gate(r, BUDGET)
        self.assertEqual(status, "no-budget")

    def test_render_has_the_cpu_ram_table(self):
        r = report(per_turn_ms=11.0)
        run.gate(r, BUDGET)
        md = run.render(r)
        self.assertIn("| Idle RSS (`tole serve`) | 15.00 MB | 20 MB | 25% |", md)
        self.assertIn("**OVER**", md)
        self.assertIn("2 vCPU, 1024 MB RAM", md)

    def test_release_report_fills_every_placeholder(self):
        import pathlib

        tpl = pathlib.Path(__file__).with_name("release-report-template.md").read_text()
        r = report()
        run.gate(r, BUDGET)
        md = run.fill_template(tpl, "v9.9.9", [r])
        self.assertNotIn("{{", md)
        self.assertIn("v9.9.9", md)
        self.assertIn("| Linux x86_64 | 3.0 ms | 15.0 MB | 28.0 MB | 4.0 ms |", md)
        self.assertIn("| Linux aarch64 | n/a ms", md)


class RoundTrips(unittest.TestCase):
    def test_exact_count_passes_any_other_fails(self):
        run.check_round_trips(31, 30)
        run.check_round_trips(3, 2)
        for seen in (0, 2, 30, 32):
            with self.assertRaises(run.HarnessError):
                run.check_round_trips(seen, 30)

    def test_the_mock_counts_requests_and_walks_the_script(self):
        import json
        import urllib.request

        m = run.MockProvider(2)
        try:
            def post(tools_done):
                msgs = [{"role": "user", "content": "x"}] + [{"role": "tool", "content": "r"}] * tools_done
                req = urllib.request.Request(
                    m.url + "/chat/completions", json.dumps({"messages": msgs}).encode(),
                    {"Content-Type": "application/json"},
                )
                with urllib.request.urlopen(req, timeout=5) as r:
                    return json.load(r)["choices"][0]

            self.assertEqual(post(0)["finish_reason"], "tool_calls")
            self.assertEqual(post(1)["finish_reason"], "tool_calls")
            self.assertEqual(post(2)["finish_reason"], "stop")
            self.assertEqual(m.requests, 3)
            m.reset(5)
            self.assertEqual(m.requests, 0)
        finally:
            m.close()


class ChildPeakRss(unittest.TestCase):
    def test_peak_is_the_childs_not_the_parents(self):
        import tempfile
        from pathlib import Path

        big = bytearray(150 * 1024 * 1024)  # make the parent clearly larger than the child
        for i in range(0, len(big), 4096):
            big[i] = 1
        with tempfile.TemporaryDirectory() as d:
            env = run.Env("/bin/sleep", Path(d), "http://127.0.0.1:1/v1")
            _, peak = run._spawn_wait(["/bin/sleep", "0.2"], env, track_rss=True)
        self.assertGreater(peak, 0.0)
        self.assertLess(peak, 50.0, "a 150 MB parent must not leak into the child's peak")


class DummyCredentials(unittest.TestCase):
    def test_credentials_are_random_per_run_and_reach_the_environment(self):
        import tempfile
        from pathlib import Path

        with tempfile.TemporaryDirectory() as d1, tempfile.TemporaryDirectory() as d2:
            a = run.Env("/bin/true", Path(d1), "http://127.0.0.1:1/v1")
            b = run.Env("/bin/true", Path(d2), "http://127.0.0.1:1/v1")
        self.assertNotEqual((a.dummy_a, a.dummy_b), (b.dummy_a, b.dummy_b))
        self.assertNotEqual(a.dummy_a, a.dummy_b)
        self.assertGreaterEqual(len(a.dummy_a), 32)
        self.assertEqual(a.env()["TOLE_API_KEY"], a.dummy_a)


class NoBudgetOnCi(unittest.TestCase):
    def test_gate_reports_no_budget_and_main_fails_it_on_ci(self):
        import json
        import os
        import tempfile
        from unittest import mock

        with tempfile.TemporaryDirectory() as d:
            budget_f = os.path.join(d, "b.toml")
            open(budget_f, "w").write(BUDGET.replace("x86_64-linux", "riscv-linux"))
            rep = report()
            with mock.patch.object(run, "measure", return_value=rep):
                argv = ["measure", "--bin", "x", "--out", os.path.join(d, "r.json"), "--budget", budget_f]
                with mock.patch.dict(os.environ, {"CI": "true"}):
                    self.assertEqual(run.main(argv), 1)
                    self.assertEqual(run.main(argv + ["--report-only"]), 0)
                with mock.patch.dict(os.environ, {"CI": ""}):
                    self.assertEqual(run.main(argv), 0)
            self.assertEqual(json.load(open(os.path.join(d, "r.json")))["gate"]["status"], "no-budget")


if __name__ == "__main__":
    unittest.main()
