"""Regression tests for the Tier-2 judges (issue #229).

The committed v0.5.0 baseline recorded crash_resume success=true for a
run that exited 2 with the approval denied: the approval banner echoes
the tool INPUT JSON, and the judge's substring match over raw stdout
was satisfied by that echo. These tests pin the fix: banners are
stripped before matching, and every judge requires exit_code == 0.

Run: python3 test_judges.py   (from evals/tier2/)
"""

import unittest

from run import (
    judge_crash_resume,
    judge_echo_chain,
    judge_read_and_report,
    strip_approval_banners,
)

BANNER = (
    "── approval required ──────────────────────────\n"
    "tool:  write_file [Write]\n"
    "what:  write file marker.txt\n"
    'input: {"content":"RESUMED-DONE","path":"marker.txt"}\n'
    "allow? [y/N] "
)

# The v0.5.0 baseline record for crash_resume (abridged): exit 2,
# approval denied, "text" is nothing but the banner — judged success.
BASELINE_FALSE_PASS = {
    "exit_code": 2,
    "text": BANNER,
    "sessions": 1,
    "tool_calls": 7,
}


class StripApprovalBannersTest(unittest.TestCase):
    def test_banner_block_is_removed(self):
        raw = "before\n" + BANNER + "\nafter"
        self.assertEqual(strip_approval_banners(raw), "before\nafter")

    def test_truncated_banner_is_removed_to_eof(self):
        raw = "before\n── approval required ──\ntool:  write_file [Write]"
        self.assertEqual(strip_approval_banners(raw), "before")

    def test_plain_output_is_untouched(self):
        self.assertEqual(strip_approval_banners("RESUMED-DONE"), "RESUMED-DONE")


class JudgesRequireExitZeroTest(unittest.TestCase):
    def test_crash_resume_baseline_false_pass_now_fails(self):
        ok, _ = judge_crash_resume(dict(BASELINE_FALSE_PASS))
        self.assertFalse(ok)

    def test_crash_resume_real_pass(self):
        ok, _ = judge_crash_resume(
            {"exit_code": 0, "text": "RESUMED-DONE", "sessions": 1, "tool_calls": 5}
        )
        self.assertTrue(ok)

    def test_read_and_report_requires_exit_zero(self):
        self.assertFalse(judge_read_and_report({"exit_code": 2, "text": "ALPHA-77"})[0])
        self.assertTrue(judge_read_and_report({"exit_code": 0, "text": "x ALPHA-77 x"})[0])

    def test_echo_chain_requires_exit_zero(self):
        self.assertFalse(
            judge_echo_chain({"exit_code": 1, "text": "CHAIN-OK", "tool_calls": 3})[0]
        )
        self.assertTrue(
            judge_echo_chain({"exit_code": 0, "text": "CHAIN-OK", "tool_calls": 2})[0]
        )


if __name__ == "__main__":
    unittest.main()
