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
    def check(self, stdout="", code=0, mode="kinds", files=None, extra=()):
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "ws")
            os.mkdir(root)
            for rel, content in (files or {}).items():
                path = os.path.join(root, rel)
                os.makedirs(os.path.dirname(path), exist_ok=True)
                with open(path, "wb") as f:
                    f.write(content if isinstance(content, bytes) else content.encode())
            cargo = stub_cargo(d, stdout.replace("@ROOT@", root), code)
            env = dict(os.environ, CARGO=cargo)
            done = subprocess.run(["bash", os.path.join(HERE, "arch-check.sh"), mode, root, *extra],
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

    def test_all_runs_kinds_size_and_unsafe(self):
        rc, out = self.check(metadata([pkg("a", "core")]), mode="all", files={**SMALL, **UNSAFE_OK, LIST: "# empty\n"})
        self.assertEqual(rc, 0, out)
        self.assertIn("kinds:", out)
        self.assertIn("size:", out)
        self.assertIn("unsafe:", out)

LIST = "scripts/arch-size-allow.txt"
SMALL = {"crates/a/src/lib.rs": "fn a() {}\n"}
ROOT_TOML = '[workspace]\nmembers = ["crates/a"]\n\n[workspace.lints.rust]\nunsafe_code = "deny"\n'
CRATE_TOML = '[package]\nname = "a"\n\n[lints]\nworkspace = true\n'


def agents(*paths, start=arch.UNSAFE_START, end=arch.UNSAFE_END):
    """An AGENTS.md whose opt-in list holds `paths` (one bullet each)."""
    return "# Agents\n\nSome prose.\n\n" + start + "\n" + "".join(f"- `{p}`: why\n" for p in paths) + end + "\n\nMore prose.\n"


UNSAFE_OK = {"Cargo.toml": ROOT_TOML, "crates/a/Cargo.toml": CRATE_TOML, "AGENTS.md": agents()}


def lines(n):
    return "".join(f"// {i}\n" for i in range(n))


def size(files, crates, allow_text):
    found = []
    allow = arch.parse_allow(allow_text, report=lambda p, l, m: found.append((p, l, m)))
    arch.check_size(files, crates, allow, report=lambda p, l, m: found.append((p, l, m)))
    return found


class SizeRules(unittest.TestCase):
    def test_everything_within_the_caps_passes_with_an_empty_list(self):
        self.assertEqual(size({"crates/a/src/lib.rs": 500}, {"a": 50000}, "# nothing listed\n"), [])

    def test_a_new_file_over_the_cap_fails_with_the_file_and_the_numbers(self):
        found = size({"crates/a/src/new.rs": 501}, {"a": 501}, "")
        self.assertEqual(len(found), 1)
        path, _, msg = found[0]
        self.assertEqual(path, "crates/a/src/new.rs")
        self.assertIn("501 lines", msg)
        self.assertIn("500-line cap", msg)
        self.assertIn("new entries are not added", msg)

    def test_a_listed_file_that_grows_by_one_line_fails(self):
        found = size({"crates/a/src/big.rs": 801}, {"a": 801}, "file crates/a/src/big.rs 800\n")
        self.assertEqual(len(found), 1)
        self.assertIn("grew past its entry (801 lines, entry 800)", found[0][2])
        self.assertEqual(found[0][0], LIST)
        self.assertEqual(found[0][1], 1)

    def test_a_listed_file_that_shrank_has_a_stale_entry(self):
        found = size({"crates/a/src/big.rs": 700}, {"a": 700}, "file crates/a/src/big.rs 800\n")
        self.assertEqual(len(found), 1)
        self.assertIn("lower it to 700", found[0][2])

    def test_an_entry_left_behind_after_a_split_fails(self):
        found = size({"crates/a/src/big.rs": 400}, {"a": 400}, "file crates/a/src/big.rs 800\n")
        self.assertEqual(len(found), 1)
        self.assertIn("remove this entry", found[0][2])
        self.assertIn("within the 500-line cap", found[0][2])

    def test_an_entry_for_a_file_that_is_gone_fails(self):
        found = size({"crates/a/src/lib.rs": 10}, {"a": 10}, "file crates/a/src/gone.rs 800\n")
        self.assertEqual(len(found), 1)
        self.assertIn("not measured any more", found[0][2])

    def test_an_entry_never_over_its_cap_is_refused(self):
        # An entry is written only for a violation: a number within the cap cannot be a valid entry.
        found = size({"crates/a/src/big.rs": 600}, {"a": 600}, "file crates/a/src/big.rs 400\n")
        self.assertTrue(any("not over the 500-line cap" in m or "grew past" in m for _, _, m in found), found)

    def test_the_crate_cap_has_the_same_rules(self):
        self.assertIn("not in", size({}, {"a": 50001}, "")[0][2])
        self.assertIn("grew past", size({}, {"a": 50002}, "crate a 50001\n")[0][2])
        self.assertIn("lower it to 50001", size({}, {"a": 50001}, "crate a 60000\n")[0][2])
        self.assertIn("remove this entry", size({}, {"a": 40000}, "crate a 60000\n")[0][2])
        self.assertEqual(size({}, {"a": 60000}, "crate a 60000\n"), [])

    def test_a_listed_violation_that_did_not_change_passes(self):
        self.assertEqual(size({"crates/a/src/big.rs": 800}, {"a": 800}, "file crates/a/src/big.rs 800  # split later\n"), [])

    def test_a_list_that_cannot_be_read_is_reported_not_taken_as_empty(self):
        found = size({"crates/a/src/lib.rs": 10}, {"a": 10}, "oops\nfile x notanumber\nfile y 0\nfile z 9\nfile z 9\n")
        msgs = " | ".join(m for _, _, m in found)
        self.assertEqual(msgs.count("not understood"), 3, msgs)
        self.assertIn("duplicate entry", msgs)

    def test_the_committed_list_has_only_real_violations(self):
        # AC3: every entry was over its cap when written, and the list is sorted (crates, then files).
        path = os.path.join(HERE, "arch-size-allow.txt")
        with open(path, encoding="utf-8") as f:
            text = f.read()
        found = []
        allow = arch.parse_allow(text, report=lambda p, l, m: found.append((p, l, m)))
        self.assertEqual(found, [])
        self.assertTrue(allow, "the committed list is empty")
        for (kind, name), (n, _) in allow.items():
            cap = arch.MAX_FILE_LINES if kind == "file" else arch.MAX_CRATE_LINES
            self.assertGreater(n, cap, f"{kind} {name}: entry {n} is within the cap")

    def test_the_baseline_lists_only_violations_sorted(self):
        text = arch.baseline_text({"b.rs": 600, "a.rs": 700, "c.rs": 500}, {"z": 50001, "y": 10})
        entries = [l for l in text.splitlines() if not l.startswith("#")]
        self.assertEqual(entries, ["crate z 50001", "file a.rs 700", "file b.rs 600"])


def workspace(directory, files):
    for rel, content in files.items():
        path = os.path.join(directory, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(content if isinstance(content, bytes) else content.encode())
    return [{"name": "a", "manifest": "crates/a/Cargo.toml", "dir": os.path.join(directory, "crates/a"), "kind": "core", "deps": []}]


class SizeMeasure(unittest.TestCase):
    def test_lines_are_counted_like_wc_plus_an_unterminated_last_line(self):
        self.assertEqual(arch.count_lines(b""), 0)
        self.assertEqual(arch.count_lines(b"a\n"), 1)
        self.assertEqual(arch.count_lines(b"a\nb"), 2)
        self.assertEqual(arch.count_lines(b"\n\n"), 2)

    def test_sources_tests_examples_benches_and_the_build_script_are_measured(self):
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {
                "crates/a/src/lib.rs": lines(3), "crates/a/src/deep/x.rs": lines(4), "crates/a/src/notes.txt": lines(99),
                "crates/a/tests/t.rs": lines(5), "crates/a/examples/e.rs": lines(6), "crates/a/benches/b.rs": lines(7),
                "crates/a/build.rs": lines(8), "crates/a/other/skip.rs": lines(9),
            })
            files, crates = arch.measure(pk, d)
        self.assertEqual(files, {"crates/a/src/lib.rs": 3, "crates/a/src/deep/x.rs": 4, "crates/a/tests/t.rs": 5,
                                 "crates/a/examples/e.rs": 6, "crates/a/benches/b.rs": 7, "crates/a/build.rs": 8})
        self.assertEqual(crates, {"a": 7})  # only src/ counts towards the crate cap

    def test_a_crate_with_no_source_fails_closed(self):
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {"crates/a/tests/t.rs": lines(1)})
            with self.assertRaises(RuntimeError):
                arch.measure(pk, d)

    def test_a_file_that_is_not_utf8_fails_closed(self):
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {"crates/a/src/lib.rs": b"fn a() {}\n\xff\xfe\n"})
            with self.assertRaises(UnicodeDecodeError):
                arch.measure(pk, d)

    def test_a_dangling_symlink_fails_closed(self):
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {"crates/a/src/lib.rs": lines(1)})
            os.symlink(os.path.join(d, "nowhere.rs"), os.path.join(d, "crates/a/src/link.rs"))
            with self.assertRaises(RuntimeError):
                arch.measure(pk, d)

    def test_an_unreadable_file_fails_closed(self):
        if os.geteuid() == 0:
            self.skipTest("root reads everything")
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {"crates/a/src/lib.rs": lines(1), "crates/a/src/secret.rs": lines(1)})
            os.chmod(os.path.join(d, "crates/a/src/secret.rs"), 0)
            with self.assertRaises(PermissionError):
                arch.measure(pk, d)

    def test_a_symlinked_directory_is_not_followed(self):
        with tempfile.TemporaryDirectory() as d:
            pk = workspace(d, {"crates/a/src/lib.rs": lines(1), "elsewhere/big.rs": lines(900)})
            os.symlink(os.path.join(d, "elsewhere"), os.path.join(d, "crates/a/src/linked"))
            files, _ = arch.measure(pk, d)
        self.assertEqual(files, {"crates/a/src/lib.rs": 1})


class SizeWrapper(unittest.TestCase):
    meta = metadata([pkg("a", "core")])

    def run_size(self, files, extra=(), mode="size"):
        return Wrapper().check(self.meta, mode=mode, files=files, extra=extra)

    def test_a_new_501_line_file_exits_1_and_names_it(self):
        rc, out = self.run_size({**SMALL, "crates/a/src/big.rs": lines(501), LIST: "# empty\n"})
        self.assertEqual(rc, 1, out)
        self.assertIn("crates/a/src/big.rs", out)
        self.assertIn("501 lines", out)

    def test_a_listed_file_that_grows_exits_1_and_a_split_leaves_a_stale_entry(self):
        base = {**SMALL, "crates/a/src/big.rs": lines(601)}
        rc, out = self.run_size({**base, LIST: "file crates/a/src/big.rs 600\n"})
        self.assertEqual(rc, 1, out)
        self.assertIn("grew past its entry", out)
        rc, out = self.run_size({**SMALL, "crates/a/src/big.rs": lines(300), LIST: "file crates/a/src/big.rs 600\n"})
        self.assertEqual(rc, 1, out)
        self.assertIn("remove this entry", out)

    def test_a_listed_violation_unchanged_passes(self):
        rc, out = self.run_size({**SMALL, "crates/a/src/big.rs": lines(600), LIST: "file crates/a/src/big.rs 600\n"})
        self.assertEqual(rc, 0, out)
        self.assertIn("arch-check (size): ok", out)

    def test_a_missing_list_fails_closed(self):
        rc, out = self.run_size(SMALL)
        self.assertEqual(rc, 2, out)
        self.assertIn("NOT passed", out)

    def test_a_non_utf8_source_fails_closed(self):
        rc, out = self.run_size({"crates/a/src/lib.rs": b"\xff\xfe", LIST: "# empty\n"})
        self.assertEqual(rc, 2, out)
        self.assertNotIn(": ok", out)

    def test_init_writes_the_first_list_and_refuses_to_run_again(self):
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "ws")
            workspace(root, {**SMALL, "crates/a/src/big.rs": lines(777)})
            os.makedirs(os.path.join(root, "scripts"))
            env = dict(os.environ, CARGO=stub_cargo(d, self.meta.replace("@ROOT@", root)))
            cmd = ["bash", os.path.join(HERE, "arch-check.sh"), "size", root, "--init"]
            first = subprocess.run(cmd, capture_output=True, text=True, env=env)
            self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
            with open(os.path.join(root, LIST), encoding="utf-8") as f:
                self.assertIn("file crates/a/src/big.rs 777", f.read())
            again = subprocess.run(cmd, capture_output=True, text=True, env=env)
            self.assertEqual(again.returncode, 2, again.stdout + again.stderr)
            self.assertIn("exists", again.stdout + again.stderr)
            plain = subprocess.run(cmd[:-1], capture_output=True, text=True, env=env)
            self.assertEqual(plain.returncode, 0, plain.stdout + plain.stderr)

    def test_init_is_for_the_size_mode_only(self):
        rc, _ = self.run_size(SMALL, extra=("--init",), mode="kinds")
        self.assertEqual(rc, 2)


def opt_ins(text):
    return arch.find_opt_ins(text)


class UnsafeSites(unittest.TestCase):
    def test_outer_and_inner_attributes_are_sites_with_their_line(self):
        text = "// SAFETY: a\n#![allow(unsafe_code)]\nfn a() {}\n\n// SAFETY: b\n#[allow(unsafe_code)]\nfn b() {}\n"
        self.assertEqual(opt_ins(text), [(2, True), (6, True)])

    def test_the_safety_comment_may_trail_the_attribute_or_sit_within_three_lines(self):
        self.assertEqual(opt_ins("#[allow(unsafe_code)] // SAFETY: why\nfn a() {}\n"), [(1, True)])
        self.assertEqual(opt_ins("// SAFETY: why\n//\n//\n#[allow(unsafe_code)]\nfn a() {}\n"), [(4, True)])
        self.assertEqual(opt_ins("#[allow(unsafe_code)]\n// SAFETY: why\nfn a() {}\n"), [(1, True)])

    def test_a_safety_comment_four_lines_away_or_a_bare_word_does_not_count(self):
        far = "// SAFETY: why\n//\n//\n//\n#[allow(unsafe_code)]\nfn a() {}\n"
        self.assertEqual(opt_ins(far), [(5, False)])
        self.assertEqual(opt_ins("#[allow(unsafe_code)] // safety, trust me\nfn a() {}\n"), [(1, False)])
        self.assertEqual(opt_ins("#[allow(unsafe_code)]\nfn a() {}\n"), [(1, False)])

    def test_expect_cfg_attr_lists_and_multiline_attributes_are_sites(self):
        for attr in ("#[expect(unsafe_code)]", "#[allow(dead_code, unsafe_code)]", '#[cfg_attr(unix, allow(unsafe_code))]',
                     "#[allow(\n    unsafe_code,\n)]", "#[allow(unsafe_code, reason = \"ffi\")]"):
            with self.subTest(attr=attr):
                found = opt_ins(f"// SAFETY: ok\n{attr}\nfn a() {{}}\n")
                self.assertEqual(found, [(2, True)])

    def test_other_lints_deny_forbid_and_comments_are_not_sites(self):
        for text in ("#[allow(dead_code)]\nfn a() {}\n", "#![deny(unsafe_code)]\n", "#![forbid(unsafe_code)]\n",
                     "// #[allow(unsafe_code)] in a comment\n", "//! Use `#![allow(unsafe_code)]` to opt in.\n",
                     "/// `#[allow(unsafe_code)]`\nfn a() {}\n", "#[allow(not_unsafe_code_at_all)]\nfn a() {}\n"):
            with self.subTest(text=text):
                self.assertEqual(opt_ins(text), [])

    def test_a_multiline_attribute_is_one_site(self):
        self.assertEqual(len(opt_ins("#[allow(\n    unsafe_code,\n    dead_code,\n)]\nfn a() {}\n")), 1)


class UnsafeList(unittest.TestCase):
    def parse(self, text):
        found = []
        listed = arch.parse_unsafe_list(text, report=lambda p, l, m: found.append((p, l, m)))
        return listed, found

    def test_bullets_between_the_markers_are_the_list_with_their_lines(self):
        listed, found = self.parse(agents("crates/a/src/x.rs", "crates/b/tests/y.rs"))
        self.assertEqual(found, [])
        self.assertEqual(listed, {"crates/a/src/x.rs": 6, "crates/b/tests/y.rs": 7})

    def test_an_empty_list_is_valid(self):
        self.assertEqual(self.parse(agents()), ({}, []))

    def test_the_text_after_the_path_is_a_description(self):
        listed, found = self.parse(f"{arch.UNSAFE_START}\n* `crates/a/src/x.rs`: a, b (c)\n{arch.UNSAFE_END}\n")
        self.assertEqual((list(listed), found), (["crates/a/src/x.rs"], []))

    def test_a_line_that_is_not_a_bullet_with_a_path_is_reported_not_ignored(self):
        for bad in ("crates/a/src/x.rs", "- crates/a/src/x.rs", "- `x.rs`", "- `crates/a/src/x.toml`", "prose here"):
            with self.subTest(bad=bad):
                listed, found = self.parse(f"{arch.UNSAFE_START}\n{bad}\n{arch.UNSAFE_END}\n")
                self.assertEqual(listed, {})
                self.assertEqual(len(found), 1, found)
                self.assertIn("not understood", found[0][2])
                self.assertEqual(found[0][1], 2)

    def test_a_duplicate_is_reported(self):
        _, found = self.parse(agents("crates/a/src/x.rs", "crates/a/src/x.rs"))
        self.assertEqual(len(found), 1)
        self.assertIn("duplicate entry", found[0][2])

    def test_markers_that_are_missing_repeated_or_reversed_fail_closed(self):
        s, e = arch.UNSAFE_START, arch.UNSAFE_END
        for text in ("", f"{s}\n", f"{e}\n", f"{e}\n{s}\n", f"{s}\n{s}\n{e}\n", f"{s}\n{e}\n{e}\n"):
            with self.subTest(text=text):
                with self.assertRaises(RuntimeError):
                    self.parse(text)

    def test_a_marker_named_in_prose_is_not_a_marker(self):
        listed, found = self.parse(f"Write `{arch.UNSAFE_START}` and `{arch.UNSAFE_END}` around it.\n\n" + agents("crates/a/src/x.rs"))
        self.assertEqual((list(listed), found), (["crates/a/src/x.rs"], []))


def unsafe_check(sites, listed):
    found = []
    arch.check_unsafe(sites, listed, report=lambda p, l, m: found.append((p, l, m)))
    return found


class UnsafeRules(unittest.TestCase):
    def test_a_site_with_safety_in_a_listed_file_passes(self):
        self.assertEqual(unsafe_check({"crates/a/src/x.rs": [(3, True), (9, True)]}, {"crates/a/src/x.rs": 5}), [])

    def test_no_opt_ins_and_an_empty_list_pass(self):
        self.assertEqual(unsafe_check({}, {}), [])

    def test_a_file_with_an_opt_in_that_is_not_listed_fails_naming_file_and_line(self):
        found = unsafe_check({"crates/a/src/new.rs": [(7, True)]}, {})
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0][:2], ("crates/a/src/new.rs", 7))
        self.assertIn("not in the opt-in list", found[0][2])

    def test_a_listed_file_without_an_opt_in_fails_naming_the_list_line(self):
        found = unsafe_check({}, {"crates/a/src/old.rs": 12})
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0][:2], (arch.AGENTS_FILE, 12))
        self.assertIn("holds no #[allow(unsafe_code)]", found[0][2])

    def test_a_site_without_a_safety_comment_fails(self):
        found = unsafe_check({"crates/a/src/x.rs": [(3, True), (9, False)]}, {"crates/a/src/x.rs": 5})
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0][:2], ("crates/a/src/x.rs", 9))
        self.assertIn("SAFETY", found[0][2])

    def test_the_lint_wiring_is_checked(self):
        def lints(root, crates):
            found = []
            arch.check_lints(root, crates, report=lambda p, l, m: found.append((p, l, m)))
            return found
        self.assertEqual(lints(ROOT_TOML, {"crates/a/Cargo.toml": CRATE_TOML}), [])
        ok_inline = '[package]\nname = "a"\n[lints] # inherit\nworkspace = true # D4\n[features]\n'
        self.assertEqual(lints(ROOT_TOML + '[profile.release]\nopt-level = 3\n', {"c": ok_inline}), [])
        for bad_root in ("[workspace]\n", '[workspace.lints.rust]\nunsafe_code = "warn"\n',
                         '[workspace.lints.rust]\nfoo = 1\n[other]\nunsafe_code = "deny"\n', '[workspace.lints.clippy]\nunsafe_code = "deny"\n'):
            with self.subTest(root=bad_root):
                self.assertEqual(len(lints(bad_root, {})), 1)
        for bad_crate in ('[package]\nname = "a"\n', '[lints]\nrust = {}\n', '[lints.rust]\nunsafe_code = "allow"\n',
                          '[lints]\nworkspace = false\n', '[package]\nname="a"\n[dependencies]\nworkspace = true\n'):
            with self.subTest(crate=bad_crate):
                found = lints(ROOT_TOML, {"crates/a/Cargo.toml": bad_crate})
                self.assertEqual(len(found), 1, found)
                self.assertEqual(found[0][0], "crates/a/Cargo.toml")


class UnsafeWrapper(unittest.TestCase):
    meta = metadata([pkg("a", "core")])

    def run_unsafe(self, files):
        return Wrapper().check(self.meta, mode="unsafe", files=files)

    def test_the_list_equal_to_the_code_passes(self):
        files = {**SMALL, **UNSAFE_OK, "AGENTS.md": agents("crates/a/src/ffi.rs"),
                 "crates/a/src/ffi.rs": "// SAFETY: ok\n#![allow(unsafe_code)]\n"}
        rc, out = self.run_unsafe(files)
        self.assertEqual(rc, 0, out)
        self.assertIn("arch-check (unsafe): ok", out)
        self.assertIn("1 opt-ins in 1 files", out)

    def test_an_opt_in_that_the_list_does_not_name_fails_with_exit_1(self):
        rc, out = self.run_unsafe({**SMALL, **UNSAFE_OK, "crates/a/src/ffi.rs": "// SAFETY: ok\n#![allow(unsafe_code)]\n"})
        self.assertEqual(rc, 1, out)
        self.assertIn("crates/a/src/ffi.rs:2", out)
        self.assertIn("not in the opt-in list", out)

    def test_a_listed_file_that_lost_its_opt_in_fails_with_exit_1(self):
        rc, out = self.run_unsafe({**SMALL, **UNSAFE_OK, "AGENTS.md": agents("crates/a/src/ffi.rs"), "crates/a/src/ffi.rs": "fn a() {}\n"})
        self.assertEqual(rc, 1, out)
        self.assertIn("AGENTS.md:6", out)
        self.assertIn("remove the entry", out)

    def test_an_opt_in_in_tests_examples_and_a_build_script_is_found(self):
        site = "// SAFETY: ok\n#![allow(unsafe_code)]\n"
        files = {**SMALL, **UNSAFE_OK, "crates/a/tests/t.rs": site, "crates/a/examples/e.rs": site, "crates/a/build.rs": site}
        rc, out = self.run_unsafe(files)
        self.assertEqual(rc, 1, out)
        for name in ("tests/t.rs", "examples/e.rs", "build.rs"):
            self.assertIn(f"crates/a/{name}:2", out)

    def test_a_target_directory_is_not_scanned(self):
        rc, out = self.run_unsafe({**SMALL, **UNSAFE_OK, "crates/a/target/debug/build/x.rs": "#![allow(unsafe_code)]\n"})
        self.assertEqual(rc, 0, out)

    def test_a_site_without_safety_exits_1(self):
        files = {**SMALL, **UNSAFE_OK, "AGENTS.md": agents("crates/a/src/ffi.rs"), "crates/a/src/ffi.rs": "#![allow(unsafe_code)]\n"}
        rc, out = self.run_unsafe(files)
        self.assertEqual(rc, 1, out)
        self.assertIn("without a `SAFETY:` comment", out)

    def test_a_crate_without_the_lints_table_exits_1(self):
        rc, out = self.run_unsafe({**SMALL, **UNSAFE_OK, "crates/a/Cargo.toml": '[package]\nname = "a"\n'})
        self.assertEqual(rc, 1, out)
        self.assertIn("crates/a/Cargo.toml", out)
        self.assertIn("workspace = true", out)

    def test_a_missing_agents_file_or_broken_markers_fail_closed(self):
        files = {**SMALL, **UNSAFE_OK}
        del files["AGENTS.md"]
        rc, out = self.run_unsafe(files)
        self.assertEqual(rc, 2, out)
        self.assertIn("NOT passed", out)
        rc, out = self.run_unsafe({**SMALL, **UNSAFE_OK, "AGENTS.md": "# no markers\n"})
        self.assertEqual(rc, 2, out)
        self.assertNotIn(": ok", out)

    def test_a_missing_root_manifest_or_a_non_utf8_source_fails_closed(self):
        files = {**SMALL, **UNSAFE_OK}
        del files["Cargo.toml"]
        self.assertEqual(self.run_unsafe(files)[0], 2)
        self.assertEqual(self.run_unsafe({**SMALL, **UNSAFE_OK, "crates/a/src/bad.rs": b"\xff\xfe"})[0], 2)

    def test_init_is_for_the_size_mode_only(self):
        rc, _ = Wrapper().check(self.meta, mode="unsafe", files={**SMALL, **UNSAFE_OK}, extra=("--init",))
        self.assertEqual(rc, 2)


class RepositoryList(unittest.TestCase):
    def test_the_committed_agents_list_parses_and_names_real_files(self):
        root = os.path.dirname(HERE)
        with open(os.path.join(root, arch.AGENTS_FILE), encoding="utf-8") as f:
            found = []
            listed = arch.parse_unsafe_list(f.read(), report=lambda p, l, m: found.append((p, l, m)))
        self.assertEqual(found, [])
        self.assertTrue(listed, "the committed opt-in list is empty")
        for name in listed:
            self.assertTrue(os.path.isfile(os.path.join(root, name)), name)


if __name__ == "__main__":
    unittest.main(verbosity=1)
