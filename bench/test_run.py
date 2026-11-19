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


if __name__ == "__main__":
    unittest.main()
