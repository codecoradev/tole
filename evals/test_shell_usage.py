"""Tests for evals/shell_usage.py (issue #531). Run: python3 evals/test_shell_usage.py"""

import json
import tempfile
import unittest
from pathlib import Path

from shell_usage import classify, collect, summarize


class Classify(unittest.TestCase):
    def test_features(self):
        cases = {
            "ls | wc -l": {"pipe"},
            "a && b": {"chain"},
            "a || b": {"chain"},
            "a; b": {"chain"},
            "echo x > f": {"redirect"},
            "echo x >> f": {"redirect"},
            "wc -l < f": {"redirect"},
            "echo $(date)": {"subst"},
            "echo `date`": {"subst"},
            "cd sub && ls": {"cd", "chain"},
            'bash -c "ls | wc"': {"wrapper"},
        }
        for cmd, want in cases.items():
            self.assertEqual(classify(cmd), want, cmd)

    def test_quoted_operators_are_literal(self):
        for cmd in ['grep "a|b" f', "grep 'a;b' f", 'echo "x > y"', "echo '$(date)'"]:
            self.assertEqual(classify(cmd), set(), cmd)

    def test_plain_and_unparsable(self):
        self.assertEqual(classify("cargo test -p tole-core"), set())
        self.assertEqual(classify('echo "unterminated'), set())


def intent(i, cmd, tool="run_command"):
    return {"kind": "entry", "id": f"i{i}", "type": "intent",
            "payload": {"tool": tool, "input": {"command": cmd}}}


def result(i, ok):
    return {"kind": "entry", "id": f"r{i}", "parentId": f"i{i}",
            "type": "tool_result", "payload": {"ok": ok}}


class Scan(unittest.TestCase):
    def test_counts_failures_retries_and_batched_lines(self):
        with tempfile.TemporaryDirectory() as d:
            f = Path(d) / "s.jsonl"
            lines = [
                json.dumps({"kind": "header"}),
                json.dumps([intent(1, "ls | wc"), result(1, False)]),  # batched
                json.dumps(intent(2, "ls")),
                json.dumps(result(2, True)),
                json.dumps(intent(3, "read_file", tool="read_file")),  # ignored tool
                "not json",
            ]
            f.write_text("\n".join(lines))
            sessions, rows = collect([d])
            s = summarize(sessions, rows)
            self.assertEqual((s["sessions"], s["calls"], s["any"]), (1, 2, 1))
            self.assertEqual(s["pipe"], 1)
            self.assertEqual(s["any_failed"], 1)
            self.assertEqual(s["after_a_failed_call"], 1)
            self.assertEqual(s["plain_failed"], 0)


if __name__ == "__main__":
    unittest.main()
