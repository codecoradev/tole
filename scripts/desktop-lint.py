#!/usr/bin/env python3
"""Rules of the desktop guardrails (#1028) that need more than shell.

Called by scripts/desktop-check.sh, which documents the rules; run it through
that script.

    desktop-lint.py deps <workspace-root>      enforced
    desktop-lint.py size <workspace-root>      enforced

Output: `error: path:line: message` or `warning: ...` or `info: ...` per
finding. Exit 0 = no error, 1 = at least one `error:` line, 2 = the check
itself could not run (cargo failed, a file was unreadable, no source file was
found, an exception). The caller must treat anything but 0 and 1 as a failure
of the gate: this script never reports "ok" when it did not finish.

`deps` asks cargo itself: `cargo metadata` for what each crate declares and
where each package comes from, `cargo tree` for what a build of the crate
really reaches (resolver features of the checked package only). Both run
`--offline`, so the crates must be downloaded (`cargo fetch`).
Python 3.8+.
"""
import glob
import json
import os
import re
import subprocess
import sys
import traceback

MODEL = "tole-desktop-model"
DESKTOP = "tole-desktop"
# A package whose name starts with one of these is the GUI toolkit (or part of
# its supply chain, as zed-* forks are).
TOOLKIT_PREFIXES = ("gpui", "zed")
# The session host and agent logic. tole-client (#1040) is the one tole crate a
# desktop may use once it exists: add "tole-client" to ALLOWED_FOR_DESKTOP then.
# It is never allowed in the model crate.
SESSION_HOST = {"tole-cli", "tole-core", "tole-tui"}
TOLE_CLIENT = "tole-client"
ALLOWED_FOR_DESKTOP = set()  # OFF: nothing is allowed yet
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
EXACT_REQ = re.compile(r"=\d+\.\d+\.\d+(-[0-9A-Za-z.\-]+)?")
MAX_FILE_LINES = 500
MAX_CRATE_LINES = 2000

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
    return norm(name).startswith(TOOLKIT_PREFIXES)


def crate_dir(root, crate):
    return os.path.join(root, "crates", crate)


# ---- one walker for every check --------------------------------------------


def rust_files(crate, strict=True):
    """Every .rs file of a crate, the one way all checks see the sources.

    Walks the crate directory and prunes ONLY the crate's own top-level
    `target/` (cargo's output directory); a directory named `target` anywhere
    else, or in the path above the crate, is walked. The decision never looks at
    the absolute path. A symlinked .rs file is followed (a dangling one, or one
    that is not a file, is refused); a symlinked directory is refused because
    the walk would not enter it. With strict, a crate without src/ or without
    any .rs file is refused too, so a check can never pass over zero files.
    Raises RuntimeError on any of these.
    """
    if not os.path.isdir(os.path.join(crate, "src")):
        raise RuntimeError(f"{crate}: no src/ directory, nothing to check")
    found = []

    def unreadable(err):
        raise RuntimeError(f"cannot read {err.filename}: {err.strerror}")

    for base, dirs, names in os.walk(crate, onerror=unreadable):
        for d in list(dirs):
            full = os.path.join(base, d)
            if os.path.islink(full):
                raise RuntimeError(f"{full}: a symlinked directory is not allowed in a desktop crate")
        if os.path.normpath(base) == os.path.normpath(crate):
            dirs[:] = [d for d in dirs if d != "target"]
        for name in names:
            if not name.endswith(".rs"):
                continue
            full = os.path.join(base, name)
            if os.path.islink(full) and not os.path.isfile(full):
                raise RuntimeError(f"{full}: a dangling symlink or one that is not a file")
            found.append(full)
    if strict and not found:
        raise RuntimeError(f"{crate}: no .rs files found, nothing to check")
    return sorted(found)


def count_lines(path):
    with open(path, "rb") as fh:
        data = fh.read()
    return data.count(b"\n") + (1 if data and not data.endswith(b"\n") else 0)


# ---- cargo -------------------------------------------------------------------


def run_cargo(args):
    cmd = [os.environ.get("CARGO", "cargo")] + args
    try:
        done = subprocess.run(cmd, capture_output=True, text=True)
    except OSError as err:
        raise RuntimeError(f"cannot run cargo: {err}")
    if done.returncode != 0:
        raise RuntimeError(f"`cargo {args[0]}` failed (run `cargo fetch` first if crates are missing): "
                           + done.stderr.strip()[-600:])
    return done.stdout


def metadata(root, crate):
    """Declarations and package sources, with every feature on (over-approximation is wanted here)."""
    out = run_cargo(["metadata", "--format-version", "1", "--offline", "--all-features",
                     "--manifest-path", os.path.join(crate_dir(root, crate), "Cargo.toml")])
    return json.loads(out)


def tree_names(root, crate, extra):
    """Names of every package a build of `crate` reaches, any kind, any target.

    `cargo tree -p` applies features to the selected package only, so a feature
    of some other workspace member does not show up here.
    """
    out = run_cargo(["tree", "--offline", "--manifest-path", os.path.join(crate_dir(root, crate), "Cargo.toml"),
                     "-p", crate, "-e", "normal,build,dev", "--target", "all", "--prefix", "none",
                     "--format", "{p}"] + extra)
    names = set()
    for line in out.splitlines():
        line = line.strip()
        if line and not line.startswith("["):
            names.add(line.split()[0])
    names.discard(crate)
    return sorted(names)


def member(meta, name):
    found = [p for p in meta["packages"] if norm(p["name"]) == name and p["source"] is None]
    if not found:
        raise RuntimeError(f"package {name} not found in the workspace")
    return found[0]


# ---- size --------------------------------------------------------------------


def check_size(root):
    desktop_crates = [crate_dir(root, MODEL), crate_dir(root, DESKTOP)]
    for crate in desktop_crates:
        total = 0
        for path in rust_files(crate):
            n = count_lines(path)
            total += n
            if n > MAX_FILE_LINES:
                error(path, 1, f"{n} lines (cap {MAX_FILE_LINES} per file)")
        if total > MAX_CRATE_LINES:
            error(crate, 1, f"{total} lines of Rust (cap {MAX_CRATE_LINES} per crate)")
        info(f"size: {crate} has {total} lines of Rust (cap {MAX_CRATE_LINES} per crate, {MAX_FILE_LINES} per file)")

    # The cap is for the desktop alone: report the other crates, never fail on them.
    for other in sorted(glob.glob(os.path.join(root, "crates", "*"))):
        if not os.path.isdir(other) or any(os.path.samefile(other, c) for c in desktop_crates):
            continue
        try:
            files = rust_files(other, strict=False)
            counts = [count_lines(p) for p in files]
        except (RuntimeError, OSError):
            continue
        big = sum(1 for n in counts if n > MAX_FILE_LINES)
        if big or sum(counts) > MAX_CRATE_LINES:
            info_line = (f"{other} is over the desktop caps ({big} files over {MAX_FILE_LINES} lines, "
                         f"{sum(counts)} lines in all); not enforced here")
            findings.append(("warning", info_line))


# ---- deps --------------------------------------------------------------------


def check_target_paths(pkg):
    crate = os.path.realpath(os.path.dirname(pkg["manifest_path"]))
    pruned = os.path.join(crate, "target")
    for target in pkg["targets"]:
        src = os.path.realpath(target["src_path"])
        where = f"{pkg['name']} target {target['name']} ({'/'.join(target['kind'])})"
        if os.path.commonpath([crate, src]) != crate:
            error(pkg["manifest_path"], 1, f"{where}: source {target['src_path']} is outside the crate, "
                                           "where the size and text checks do not look")
        elif src == pruned or src.startswith(pruned + os.sep):
            error(pkg["manifest_path"], 1, f"{where}: source {target['src_path']} is under the crate's "
                                           "target/ directory, which the checks skip")


def check_deps(root):
    model_meta, desk_meta = metadata(root, MODEL), metadata(root, DESKTOP)
    model, desk = member(model_meta, MODEL), member(desk_meta, DESKTOP)
    model_manifest, desk_manifest = model["manifest_path"], desk["manifest_path"]

    # (a) the model has no toolkit and no tole dependency, whatever its features.
    banned_model = SESSION_HOST | {TOLE_CLIENT}
    for name in tree_names(root, MODEL, ["--all-features"]):
        if is_toolkit(name) or norm(name) in banned_model:
            error(model_manifest, 1, f"{MODEL} depends (possibly through another crate) on {name}; "
                                     "the model has no GUI toolkit and no tole dependency")

    # (b) the desktop never reaches the session host or agent logic, with its own
    # features all on (and only its own: another member's feature is not counted).
    banned_desktop = (SESSION_HOST | {TOLE_CLIENT}) - ALLOWED_FOR_DESKTOP
    for name in tree_names(root, DESKTOP, ["--all-features"]):
        if norm(name) in banned_desktop:
            error(desk_manifest, 1, f"{DESKTOP} depends (possibly through another crate) on {name}; "
                                    "the desktop is a thin client of the serve (use the typed client crate "
                                    "once it is allowed)")

    # (c) every toolkit dependency of the desktop: optional, exact, crates.io, normal kind.
    for dep in desk["dependencies"]:
        rename = dep.get("rename")
        if not (is_toolkit(dep["name"]) or (rename and is_toolkit(rename))):
            continue
        label = f"{dep['name']}" + (f" (renamed {rename})" if rename else "")
        if dep.get("kind") is not None:
            error(desk_manifest, 1, f"{label}: the toolkit is not allowed in dev- or build-dependencies")
        if rename and not norm(rename).startswith("gpui"):
            error(desk_manifest, 1, f"{label}: a toolkit dependency must keep a gpui* name")
        if not dep.get("optional"):
            error(desk_manifest, 1, f"{label}: must be optional (behind the gpui feature)")
        if not EXACT_REQ.fullmatch(dep["req"]):
            error(desk_manifest, 1, f"{label}: must be exact-pinned, =MAJOR.MINOR.PATCH (is {dep['req']})")
        if dep.get("source") != CRATES_IO or dep.get("registry"):
            error(desk_manifest, 1, f"{label}: must come from crates.io (source is {dep.get('source')}, "
                                    f"registry {dep.get('registry')})")

    # (c, continued) wherever a toolkit package is resolved, it came from crates.io:
    # this is what catches a [patch] or [replace] in the root manifest.
    for meta in (desk_meta, model_meta):
        for pkg in meta["packages"]:
            if is_toolkit(pkg["name"]) and pkg["source"] != CRATES_IO:
                error(desk_manifest, 1, f"{pkg['name']} {pkg['version']} resolves from {pkg['source']}, "
                                        "not crates.io (a [patch] or path override?)")

    # (c, continued) a default build reaches no toolkit package.
    for crate, manifest in ((DESKTOP, desk_manifest), (MODEL, model_manifest)):
        for name in tree_names(root, crate, []):
            if is_toolkit(name):
                error(manifest, 1, f"{crate} reaches {name} without the gpui feature "
                                   "(a default feature or a non-optional dependency)")

    # (c, continued) every target of both crates (lib, bin, test, example, bench, build
    # script) has its source inside the crate and outside its `target/` directory, so the
    # walker that the size and text checks use sees it.
    for pkg in (model, desk):
        check_target_paths(pkg)

def main(argv):
    modes = {"deps": check_deps, "size": check_size}
    if len(argv) != 3 or argv[1] not in modes:
        print(__doc__, file=sys.stderr)
        return 2
    try:
        modes[argv[1]](argv[2])
    except Exception as exc:  # fail closed: never print "ok" for a run that did not finish
        print(f"desktop-lint {argv[1]}: could not finish: {exc}", file=sys.stderr)
        traceback.print_exc(file=sys.stderr)
        return 2
    for severity, text in findings:
        print(f"{severity}: {text}")
    return 1 if any(s == "error" for s, _ in findings) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
