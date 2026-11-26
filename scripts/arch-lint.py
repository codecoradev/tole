#!/usr/bin/env python3
"""Architecture guardrails of the CodeCora modular + thin-UI standard (#1249).

Called by scripts/arch-check.sh, which documents the rules; run it through
that script.

    arch-lint.py kinds <workspace-root>            enforced (#1250)
    arch-lint.py size <workspace-root> [--init]    enforced (#1251)

Output: `error: path:line: message`, `warning: ...` or `info: ...` per finding.
Exit 0 = no error, 1 = at least one `error:` line, 2 = the check itself could
not run (cargo failed, bad metadata, an exception). The caller must treat
anything but 0 and 1 as a failure of the gate: this script never reports "ok"
when it did not finish.

The rules are pure functions over package data (`check_kinds`), so they are
unit-tested without cargo (scripts/test_arch_lint.py). Only `load_packages`
touches cargo: `cargo metadata --no-deps --offline`, so nothing is downloaded.
Python 3.8+, standard library only.

size (rule D1): a source file stays at 500 lines or fewer and a crate's `src/`
at 50,000 or fewer. Today's violations are in scripts/arch-size-allow.txt and
that list can only go down: a file or crate over its cap and not in the list
fails, an entry that no longer matches the measured size fails (grown or
stale), an entry that is within its cap fails, and `--init` refuses to run when
the list exists. There is no mode that adds an entry.
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

# Rule D1 (standard v1.3, owner rule): the caps and what is measured.
MAX_FILE_LINES = 500
MAX_CRATE_LINES = 50000
SOURCE_DIRS = ("src", "tests", "examples", "benches")
ALLOW_LIST = "scripts/arch-size-allow.txt"

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


# ---- size rules (pure) ----------------------------------------------------------


def parse_allow(text, report=error, path=ALLOW_LIST):
    """The allow-list: `file <path> <lines>` or `crate <name> <lines>`, `#` comments.

    Returns {(kind, name): (lines, line number)}. A line that is not understood,
    a duplicate or a number that is not a positive integer is reported: a list
    that cannot be read must not pass as an empty one.
    """
    entries = {}
    for number, raw in enumerate(text.splitlines(), 1):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split()
        if len(parts) != 3 or parts[0] not in ("file", "crate") or not parts[2].isdigit() or int(parts[2]) < 1:
            report(path, number, f"not understood: {raw.strip()!r} (use `file <path> <lines>` or `crate <name> <lines>`)")
            continue
        key = (parts[0], parts[1])
        if key in entries:
            report(path, number, f"duplicate entry for {parts[0]} {parts[1]}")
            continue
        entries[key] = (int(parts[2]), number)
    return entries


def check_size(files, crates, allow, report=error, path=ALLOW_LIST):
    """Rule D1 against the allow-list that only shrinks.

    `files`: {relative path: lines} for every measured source file; `crates`:
    {crate name: lines of its src/}; `allow`: the parsed list.
    """
    for kind, measured, cap, unit in (("file", files, MAX_FILE_LINES, "a file"), ("crate", crates, MAX_CRATE_LINES, "a crate's src/")):
        for name, lines in sorted(measured.items()):
            if lines <= cap:
                continue
            entry = allow.get((kind, name))
            if entry is None:
                report(name if kind == "file" else path, 1,
                       f"{kind} {name}: {lines} lines, over the {cap}-line cap for {unit} and not in {path}: "
                       f"split it (new entries are not added)")
            elif lines > entry[0]:
                report(path, entry[1], f"{kind} {name}: grew past its entry ({lines} lines, entry {entry[0]}): split it, never raise the entry")
            elif lines < entry[0]:
                report(path, entry[1], f"{kind} {name}: stale entry, lower it to {lines} (entry {entry[0]})")
        for (k, name), (lines, number) in sorted(allow.items()):
            if k != kind:
                continue
            if name not in measured:
                report(path, number, f"{kind} {name}: remove this entry, it is not measured any more (renamed, moved or deleted)")
            elif measured[name] <= cap:
                report(path, number, f"{kind} {name}: remove this entry, it is within the {cap}-line cap now ({measured[name]} lines)")
            elif lines <= cap:
                report(path, number, f"{kind} {name}: entry {lines} is not over the {cap}-line cap: an entry is written only for a violation")


def baseline_text(files, crates):
    """The first allow-list: every file and crate over its cap, sorted."""
    out = [
        "# Size guard allow-list (#1251, standard rule D1). Written once by",
        "# `scripts/arch-check.sh size --init`; it can only go down:",
        "#   - a file or crate over its cap and not listed fails the check;",
        "#   - an entry that is smaller or larger than the measured size fails;",
        "#   - an entry within its cap fails (remove it).",
        "# Never add an entry: split the file or the crate. Cap: 500 lines per file,",
        "# 50,000 per crate src/. Format: `file <path> <lines>` or `crate <name> <lines>`.",
    ]
    out += [f"crate {n} {v}" for n, v in sorted(crates.items()) if v > MAX_CRATE_LINES]
    out += [f"file {n} {v}" for n, v in sorted(files.items()) if v > MAX_FILE_LINES]
    return "\n".join(out) + "\n"


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
            "dir": os.path.dirname(pkg["manifest_path"]),
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


def count_lines(data):
    """Lines of a file as `wc -l` counts them, plus an unterminated last line."""
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


def read_lines(path):
    """The lines of one source file. Anything unreadable or not UTF-8 raises:
    a file that cannot be measured must not pass as a small one."""
    if os.path.islink(path) and not os.path.exists(path):
        raise RuntimeError(f"{path}: a symbolic link to nothing")
    with open(path, "rb") as fh:
        data = fh.read()
    data.decode("utf-8")
    return count_lines(data)


def measure(packages, root):
    """({relative path: lines} of every .rs file of the crates' source
    directories and build.rs, {crate: lines of src/}). Symlinked directories are
    not followed; a crate with no source under src/ raises."""
    files, crates = {}, {}
    for pkg in packages:
        crate_dir = pkg["dir"]
        src_total, src_files = 0, 0
        for sub in SOURCE_DIRS:
            top = os.path.join(crate_dir, sub)
            if not os.path.isdir(top):
                continue
            for here, dirs, names in os.walk(top, followlinks=False, onerror=_walk_error):
                dirs.sort()
                for name in sorted(names):
                    if not name.endswith(".rs"):
                        continue
                    full = os.path.join(here, name)
                    lines = read_lines(full)
                    files[os.path.relpath(full, root).replace(os.sep, "/")] = lines
                    if sub == "src":
                        src_total += lines
                        src_files += 1
        build = os.path.join(crate_dir, "build.rs")
        if os.path.lexists(build):
            files[os.path.relpath(build, root).replace(os.sep, "/")] = read_lines(build)
        if src_files == 0:
            raise RuntimeError(f"{pkg['name']}: no .rs file under {os.path.relpath(os.path.join(crate_dir, 'src'), root)}; the size of this crate cannot be measured")
        crates[pkg["name"]] = src_total
    return files, crates


def _walk_error(err):
    raise RuntimeError(f"cannot read {err.filename}: {err.strerror}")


def run_size(root, init=False):
    packages = load_packages(root)
    files, crates = measure(packages, root)
    allow_path = os.path.join(root, ALLOW_LIST)
    if init:
        if os.path.lexists(allow_path):
            raise RuntimeError(f"{ALLOW_LIST} exists: --init writes the first baseline only; the list can only go down, there is no mode that adds entries")
        with open(allow_path, "w", encoding="utf-8") as fh:
            fh.write(baseline_text(files, crates))
        info(f"size: wrote {ALLOW_LIST} ({sum(1 for v in files.values() if v > MAX_FILE_LINES)} files, "
             f"{sum(1 for v in crates.values() if v > MAX_CRATE_LINES)} crates over their caps)")
        return
    if not os.path.isfile(allow_path):
        raise RuntimeError(f"{ALLOW_LIST} is missing: without the list every file over the cap would be a new violation (write it once with --init)")
    with open(allow_path, encoding="utf-8") as fh:
        text = fh.read()
    allow = parse_allow(text)
    check_size(files, crates, allow)
    over_files = sum(1 for v in files.values() if v > MAX_FILE_LINES)
    over_crates = sum(1 for v in crates.values() if v > MAX_CRATE_LINES)
    info(f"size: {len(files)} source files in {len(crates)} crates measured; {over_files} files and {over_crates} crates over "
         f"their caps, all listed ({len(allow)} entries)")


# ---- main ---------------------------------------------------------------------


def main(argv):
    modes = {"kinds": run_kinds, "size": run_size}
    flags = argv[3:]
    if len(argv) < 3 or argv[1] not in modes or any(f != "--init" for f in flags) or (flags and argv[1] != "size"):
        print(__doc__, file=sys.stderr)
        return 2
    try:
        if argv[1] == "size":
            run_size(argv[2], init=bool(flags))
        else:
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
