#!/usr/bin/env python3
"""Self-test of scripts/arch-lint.py and scripts/arch-check.sh (#1249).

Run: python3 scripts/test_arch_lint.py

The rules are pure functions, so most tests feed package data to them. The
last group drives the real shell wrapper with a stub `cargo` (CARGO=...), to
prove the gate fails closed: a cargo failure, unreadable metadata or an empty
workspace is exit 2, never a pass.
"""
import importlib.util
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("arch_lint", os.path.join(HERE, "arch-lint.py"))
arch = importlib.util.module_from_spec(spec)
spec.loader.exec_module(arch)


def pkg(name, kind, deps=()):
    """deps: names (normal) or (name, "dev"|"build"|"normal") pairs."""
    out = []
    for d in deps:
        n, k = (d, "normal") if isinstance(d, str) else d
        out.append({"name": n, "kind": k})
    return {"name": name, "manifest": f"crates/{name}/Cargo.toml", "kind": kind, "deps": out}


def run(packages):
    found = []
    kinds = arch.check_kinds(packages, report=lambda path, line, msg: found.append((path, line, msg)))
    return kinds, found


class KindsRules(unittest.TestCase):
    def test_tole_shape_passes(self):
        kinds, found = run([
            pkg("tole-core", "core"),
            pkg("tole-client", "integration", ["tole-core", "serde"]),
            pkg("tole-tui", "ui", ["tole-core", "crossterm", "ratatui"]),
            pkg("tole-desktop-model", "app"),
            pkg("tole-desktop", "bin", ["tole-client", "tole-desktop-model", "gpui-pre", "gpui-component"]),
            pkg("tole-cli", "bin", ["tole-core", "tole-tui", ("tole-desktop", "dev")]),
        ])
        self.assertEqual(found, [])
        self.assertEqual(kinds["tole-tui"], "ui")

    def test_missing_kind_is_an_error_naming_the_manifest(self):
        _, found = run([pkg("a", None)])
        self.assertEqual(len(found), 1)
        self.assertIn("crates/a/Cargo.toml", found[0][0])
        self.assertIn("no kind", found[0][2])

    def test_unknown_and_non_string_kinds_are_errors(self):
        _, found = run([pkg("a", "kernel"), pkg("b", 7)])
        self.assertEqual(len(found), 2)
        self.assertTrue(all("unknown kind" in m for _, _, m in found))

    def test_core_must_not_depend_upward(self):
        for upper in ("integration", "ui", "app", "bin", "engine"):
            with self.subTest(upper=upper):
                _, found = run([pkg("c", "core", ["u"]), pkg("u", upper)])
                self.assertEqual(len(found), 1, found)
                self.assertIn("c (core) depends on u", found[0][2])

    def test_each_row_of_the_matrix(self):
        allowed = {
            "engine": {"core", "engine"},
            "integration": {"core", "engine"},
            "os": {"core"},
            "ffi": {"core", "app"},
            "ui": {"core", "app", "ui"},
            "feature-ui": {"core", "app", "ui"},
        }
        for kind, ok in allowed.items():
            for other in arch.KINDS:
                with self.subTest(kind=kind, other=other):
                    _, found = run([pkg("x", kind, ["y"]), pkg("y", other)])
                    self.assertEqual(len(found), 0 if other in ok else 1, (kind, other, found))

    def test_bin_and_tool_may_depend_on_everything(self):
        for kind in ("bin", "tool"):
            deps = [f"k{i}" for i in range(len(arch.KINDS))]
            others = [pkg(f"k{i}", k) for i, k in enumerate(arch.KINDS)]
            _, found = run([pkg("top", kind, deps)] + others)
            self.assertEqual(found, [], kind)

    def test_toolkit_only_in_ui_feature_ui_and_bin(self):
        for kind in ("core", "engine", "integration", "os", "feature", "app", "ffi"):
            with self.subTest(kind=kind):
                _, found = run([pkg("x", kind, ["crossterm"])])
                self.assertEqual(len(found), 1, found)
                self.assertIn("UI toolkit", found[0][2])
        for kind in ("ui", "feature-ui", "bin"):
            with self.subTest(kind=kind):
                self.assertEqual(run([pkg("x", kind, ["crossterm", "ratatui", "gpui-pre"])])[1], [])

    def test_toolkit_names_are_normalised_and_prefixed(self):
        for name in ("ratatui", "gpui-pre-platform", "gpui_component", "zed-sum-tree", "Crossterm"):
            with self.subTest(name=name):
                _, found = run([pkg("x", "core", [name])])
                self.assertEqual(len(found), 1, found)

    def test_dev_and_build_dependencies_are_not_checked(self):
        _, found = run([
            pkg("c", "core", [("u", "dev"), ("u", "build"), ("crossterm", "dev")]),
            pkg("u", "bin"),
        ])
        self.assertEqual(found, [])

    def test_the_same_dependency_is_reported_once(self):
        _, found = run([pkg("c", "core", ["u", "u"]), pkg("u", "ui")])
        self.assertEqual(len(found), 1)

    def test_no_cascade_from_a_crate_with_a_bad_kind(self):
        _, found = run([pkg("a", "ui", ["b"]), pkg("b", None)])
        self.assertEqual(len(found), 1)  # only b's missing kind

    def test_non_workspace_crates_are_not_matrix_checked(self):
        _, found = run([pkg("c", "core", ["serde", "tokio"])])
        self.assertEqual(found, [])


def stub_cargo(directory, stdout, code=0):
    """A `cargo` that prints `stdout` (or fails with `code`)."""
    path = os.path.join(directory, "cargo")
    with open(path, "w") as f:
        f.write("#!/bin/sh\n")
        if code:
            f.write("echo 'stub cargo failure' >&2\n")
            f.write(f"exit {code}\n")
        else:
            f.write("cat <<'JSON'\n" + stdout + "\nJSON\n")
    os.chmod(path, os.stat(path).st_mode | stat.S_IEXEC)
    return path


def metadata(packages):
    members = [f"id-{p['name']}" for p in packages]
    return json.dumps({
        "workspace_members": members,
        "packages": [
            {
                "id": f"id-{p['name']}",
                "name": p["name"],
                "manifest_path": f"@ROOT@/crates/{p['name']}/Cargo.toml",
                "metadata": {"tole": {"kind": p["kind"]}} if p["kind"] else None,
                "dependencies": [{"name": d["name"], "kind": None if d["kind"] == "normal" else d["kind"]} for d in p["deps"]],
            }
            for p in packages
        ],
    })


class Wrapper(unittest.TestCase):
    def check(self, stdout="", code=0, mode="kinds"):
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "ws")
            os.mkdir(root)
            cargo = stub_cargo(d, stdout.replace("@ROOT@", root), code)
            env = dict(os.environ, CARGO=cargo)
            done = subprocess.run(["bash", os.path.join(HERE, "arch-check.sh"), mode, root],
                                  capture_output=True, text=True, env=env)
        return done.returncode, done.stdout + done.stderr

    def test_a_clean_workspace_passes(self):
        rc, out = self.check(metadata([pkg("a", "core"), pkg("b", "ui", ["a"])]))
        self.assertEqual(rc, 0, out)
        self.assertIn("arch-check (kinds): ok", out)

    def test_a_violation_exits_1_and_names_the_crate(self):
        rc, out = self.check(metadata([pkg("a", "core", ["b"]), pkg("b", "ui")]))
        self.assertEqual(rc, 1, out)
        self.assertIn("a (core) depends on b (ui)", out)
        self.assertIn("crates/a/Cargo.toml", out)

    def test_cargo_failure_fails_closed(self):
        rc, out = self.check(code=101)
        self.assertEqual(rc, 2, out)
        self.assertIn("NOT passed", out)
        self.assertNotIn(": ok", out)

    def test_unreadable_metadata_fails_closed(self):
        rc, out = self.check("this is not json")
        self.assertEqual(rc, 2, out)
        self.assertIn("NOT passed", out)

    def test_an_empty_workspace_fails_closed(self):
        rc, out = self.check(json.dumps({"workspace_members": [], "packages": []}))
        self.assertEqual(rc, 2, out)

    def test_usage_error_is_exit_2(self):
        rc, _ = self.check(mode="bogus")
        self.assertEqual(rc, 2)

    def test_all_runs_kinds(self):
        rc, out = self.check(metadata([pkg("a", "core")]), mode="all")
        self.assertEqual(rc, 0, out)


if __name__ == "__main__":
    unittest.main(verbosity=1)
