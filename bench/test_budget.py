import unittest

import budget

GOOD = """
[x86_64-linux.cold_start_ms]
ceiling = 25
[x86_64-linux.idle_rss_mb]
ceiling = 30.5
[x86_64-linux.active_rss_mb]
ceiling = 60
[x86_64-linux.per_turn_ms]
baseline = 3
ceiling = 10
"""


def mk(**kw):
    return {"x": {"cold_start_ms": 25.0, "idle_rss_mb": 30.0, "active_rss_mb": 60.0, "per_turn_ms": 10.0, **kw}}


class Parse(unittest.TestCase):
    def test_parses_all_four_metrics(self):
        b = budget.parse_budget(GOOD)
        self.assertEqual(b["x86_64-linux"]["idle_rss_mb"], 30.5)
        self.assertEqual(set(b["x86_64-linux"]), set(budget.METRICS))

    def test_rejects_unknown_missing_and_bad_values(self):
        for bad in (
            GOOD + "[x86_64-linux.bogus]\nceiling = 1\n",
            GOOD.replace("ceiling = 25", "nope = 25"),
            GOOD.replace("ceiling = 25", "ceiling = 0"),
            GOOD.replace("ceiling = 25", 'ceiling = "25"'),
            "[a.cold_start_ms]\nceiling = 1\n",  # metrics missing
            "not toml [",
        ):
            with self.assertRaises(budget.BudgetError, msg=bad):
                budget.parse_budget(bad)

    def test_repo_budget_file_parses(self):
        import pathlib

        text = (pathlib.Path(__file__).with_name("budget.toml")).read_text()
        parsed = budget.parse_budget(text)
        for key in ("x86_64-linux", "aarch64-linux"):
            self.assertEqual(set(parsed[key]), set(budget.METRICS), key)


class Compare(unittest.TestCase):
    def test_at_the_ceiling_passes_above_fails(self):
        ceilings = mk()["x"]
        ok = budget.compare(dict(ceilings), ceilings)
        self.assertTrue(all(v.ok for v in ok))
        over = budget.compare({**ceilings, "idle_rss_mb": 30.01}, ceilings)
        self.assertEqual([v.metric for v in over if not v.ok], ["idle_rss_mb"])

    def test_every_metric_is_judged_independently(self):
        ceilings = mk()["x"]
        measured = {m: c * 2 for m, c in ceilings.items()}
        self.assertEqual(sum(not v.ok for v in budget.compare(measured, ceilings)), 4)

    def test_unmeasured_budgeted_metric_is_an_error(self):
        ceilings = mk()["x"]
        measured = dict(ceilings)
        del measured["per_turn_ms"]
        with self.assertRaises(budget.BudgetError):
            budget.compare(measured, ceilings)

    def test_unusable_measured_values_are_rejected(self):
        ceilings = mk()["x"]
        for bad in (float("nan"), float("inf"), float("-inf"), -0.5, "3", None, True):
            with self.assertRaises(budget.BudgetError, msg=repr(bad)):
                budget.compare({**ceilings, "per_turn_ms": bad}, ceilings)
        # zero is a legal (if suspiciously good) measurement
        budget.compare({**ceilings, "per_turn_ms": 0.0}, ceilings)

    def test_headroom(self):
        v = budget.Verdict("m", 75.0, 100.0)
        self.assertAlmostEqual(v.headroom_pct, 25.0)


class Ratchet(unittest.TestCase):
    def test_lowering_or_keeping_is_fine(self):
        self.assertEqual(budget.ratchet_violations(mk(), mk(cold_start_ms=20.0)), [])
        self.assertEqual(budget.ratchet_violations(mk(), mk()), [])

    def test_raising_a_ceiling_is_a_violation(self):
        out = budget.ratchet_violations(mk(), mk(active_rss_mb=61.0))
        self.assertEqual(len(out), 1)
        self.assertIn("active_rss_mb", out[0])

    def test_removing_a_budget_is_a_violation_adding_is_not(self):
        base = mk()
        self.assertTrue(budget.ratchet_violations(base, {}))
        head = {**base, "aarch64-linux": base["x"]}
        self.assertEqual(budget.ratchet_violations(base, head), [])
        trimmed = {"x": {k: v for k, v in base["x"].items() if k != "per_turn_ms"}}
        self.assertTrue(budget.ratchet_violations(base, trimmed))


if __name__ == "__main__":
    unittest.main()
