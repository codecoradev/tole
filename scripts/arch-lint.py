#!/usr/bin/env python3
"""Architecture guardrails of the CodeCora modular + thin-UI standard (#1249).

Called by scripts/arch-check.sh, which documents the rules; run it through
that script.

    arch-lint.py kinds <workspace-root>    enforced (#1250)

Output: `error: path:line: message`, `warning: ...` or `info: ...` per finding.
Exit 0 = no error, 1 = at least one `error:` line, 2 = the check itself could
not run (cargo failed, bad metadata, an exception). The caller must treat
anything but 0 and 1 as a failure of the gate: this script never reports "ok"
when it did not finish.

The rules are pure functions over package data (`check_kinds`), so they are
unit-tested without cargo (scripts/test_arch_lint.py). Only `load_packages`
touches cargo: `cargo metadata --no-deps --offline`, so nothing is downloaded.
Python 3.8+, standard library only.
"""
import json
import os
import subprocess
import sys
import traceback

# The kinds of the standard (rule L1). tole uses core, integration, ui, app and
# bin today; the others are accepted so a future crate can declare them.
KINDS = ("core", "engine", "integration", "os", "feature", "app", "ffi", "ui", "feature-ui", "bin", "tool")

# Rule L2: the kinds a crate of a kind may depend on (normal dependencies
# between workspace crates). `bin` and `tool` may depend on everything.
MATRIX = {
    "core": {"core"},
    "engine": {"core", "engine"},
    "integration": {"core", "engine"},
    "os": {"core"},
    "feature": {"core", "engine", "integration", "os", "feature"},
    "app": {"core", "engine", "integration", "os", "feature"},
    "ffi": {"core", "app"},
    "ui": {"core", "app", "ui"},
    "feature-ui": {"core", "app", "ui"},
    "bin": set(KINDS),
    "tool": set(KINDS),
}

# Rule L3: a UI toolkit is a normal dependency of these kinds only.
TOOLKIT_KINDS = {"ui", "feature-ui", "bin"}
TOOLKIT_NAMES = {"crossterm", "ratatui"}
# A package whose name starts with one of these is the GUI toolkit (or part of
# its supply chain, as zed-* forks are). Same list as desktop-lint.py.
TOOLKIT_PREFIXES = ("gpui", "zed")

findings = []  # (severity, text)


def error(path, line, msg):
    findings.append(("error", f"{path}:{line}: {msg}"))


def warning(path, line, msg):
    findings.append(("warning", f"{path}:{line}: {msg}"))


def info(msg):
    findings.append(("info", msg))


def norm(name):
    return name.lower().replace("_", "-")


def is_toolkit(name):
    n = norm(name)
    return n in TOOLKIT_NAMES or n.startswith(TOOLKIT_PREFIXES)


# ---- rules (pure) -------------------------------------------------------------


def check_kinds(packages, report=error):
    """Rules L1 to L3 over the workspace crates.

    `packages` is a list of dicts: `name`, `manifest` (path shown in messages),
    `kind` (the declared string or None) and `deps` (a list of dicts with
    `name` and `kind`: "normal", "build" or "dev"). Returns the kind of each
    crate that declared a valid one, by name.

    Only normal dependencies are checked: build and dev dependencies are
    test and build wiring, not part of what the crate ships.
    """
    kinds = {}
    for pkg in packages:
        kind = pkg.get("kind")
        manifest = pkg["manifest"]
        if kind is None:
            report(manifest, 1, f"{pkg['name']}: no kind (add [package.metadata.tole] kind = \"...\"; one of {', '.join(KINDS)})")
        elif not isinstance(kind, str) or kind not in KINDS:
            report(manifest, 1, f"{pkg['name']}: unknown kind {kind!r} (one of {', '.join(KINDS)})")
        else:
            kinds[pkg["name"]] = kind
    workspace = {pkg["name"] for pkg in packages}
    for pkg in packages:
        kind = kinds.get(pkg["name"])
        if kind is None:
            continue  # already reported
        manifest = pkg["manifest"]
        seen = set()
        for dep in pkg.get("deps", []):
            if dep.get("kind", "normal") not in ("normal", None):
                continue
            name = dep["name"]
            if name in seen:
                continue
            seen.add(name)
            if name in workspace:
                dep_kind = kinds.get(name)
                if dep_kind is not None and dep_kind not in MATRIX[kind]:
                    report(manifest, 1,
                           f"{pkg['name']} ({kind}) depends on {name} ({dep_kind}): a {kind} crate may depend only on "
                           f"{', '.join(sorted(MATRIX[kind]))}")
            elif is_toolkit(name) and kind not in TOOLKIT_KINDS:
                report(manifest, 1,
                       f"{pkg['name']} ({kind}) depends on the UI toolkit package {name}: only "
                       f"{', '.join(sorted(TOOLKIT_KINDS))} crates may")
    return kinds


# ---- cargo --------------------------------------------------------------------


def run_cargo(args, cwd):
    cmd = [os.environ.get("CARGO", "cargo")] + args
    try:
        done = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd)
    except OSError as err:
        raise RuntimeError(f"cannot run cargo: {err}")
    if done.returncode != 0:
        raise RuntimeError(f"`cargo {args[0]}` failed: " + done.stderr.strip()[-600:])
    return done.stdout


def load_packages(root):
    """The workspace members with their declared kind and direct dependencies."""
    out = run_cargo(["metadata", "--format-version", "1", "--no-deps", "--offline"], root)
    meta = json.loads(out)
    members = set(meta.get("workspace_members", []))
    packages = []
    for pkg in meta["packages"]:
        if members and pkg["id"] not in members:
            continue
        tole = (pkg.get("metadata") or {}).get("tole") or {}
        manifest = os.path.relpath(pkg["manifest_path"], root)
        packages.append({
            "name": pkg["name"],
            "manifest": manifest,
            "kind": tole.get("kind") if isinstance(tole, dict) else None,
            "deps": [{"name": d["name"], "kind": d.get("kind") or "normal"} for d in pkg.get("dependencies", [])],
        })
    if not packages:
        raise RuntimeError("cargo metadata reported no workspace crate (is this a Cargo workspace root?)")
    return packages


def run_kinds(root):
    packages = load_packages(root)
    kinds = check_kinds(packages)
    info(f"kinds: {len(packages)} crates checked ({', '.join(f'{n} {k}' for n, k in sorted(kinds.items()))})")


# ---- main ---------------------------------------------------------------------


def main(argv):
    modes = {"kinds": run_kinds}
    if len(argv) != 3 or argv[1] not in modes:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        modes[argv[1]](argv[2])
    except Exception as exc:  # fail closed: never print "ok" for a run that did not finish
        print(f"arch-lint {argv[1]}: could not finish: {exc}", file=sys.stderr)
        traceback.print_exc(file=sys.stderr)
        return 2
    for severity, text in findings:
        print(f"{severity}: {text}")
    return 1 if any(s == "error" for s, _ in findings) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
