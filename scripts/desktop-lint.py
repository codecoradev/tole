#!/usr/bin/env python3
"""Rules of the desktop guardrails (#1028) that need more than shell.

Called by scripts/desktop-check.sh, which documents the rules; run it through
that script.

    desktop-lint.py deps <workspace-root>      enforced
    desktop-lint.py size <workspace-root>      enforced
    desktop-lint.py literals <workspace-root>  advisory (never exits 1)

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
# The session host and agent logic: the desktop never lists any of these among
# its own dependencies, in any kind or target.
SESSION_HOST = {"tole-cli", "tole-core", "tole-tui"}
# tole-client (#1040) is the one tole crate a desktop may depend on. It is never
# allowed in the model crate.
TOLE_CLIENT = "tole-client"
ALLOWED_FOR_DESKTOP = {TOLE_CLIENT}
# The wire types of tole-client are tole-core's own, so tole-core is in the
# desktop's closure, but ONLY through tole-client (#1029). The session host
# (tole-cli) and the terminal UI (tole-tui) are refused everywhere.
ALLOWED_VIA_CLIENT = {"tole-core"}
CRATES_IO = "registry+https://github.com/rust-lang/crates.io-index"
EXACT_REQ = re.compile(r"=\d+\.\d+\.\d+(-[0-9A-Za-z.\-]+)?")
MAX_FILE_LINES = 500
# Owner rule D1 (2026-11-20): a crate's ideal size is 1,500 to 2,500 lines, the
# hard ceiling 4,000. The 500-line per-file cap is unchanged.
MAX_CRATE_LINES = 4000

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


def read(path):
    with open(path, encoding="utf-8") as fh:  # raises on a bad file: fail closed
        return fh.read()


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


# ---- Rust source scrubbing ---------------------------------------------------

RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')


def scrub(src, keep_strings):
    """Rust source with comments blanked, newlines kept (line numbers hold).

    Handles `//`, nested `/* */`, strings with escapes, raw strings (`r`, `br`,
    `cr`, with any number of #) and char literals, so `"//"` does not hide
    code. With keep_strings False string contents are blanked too (code only).
    """
    out, i, n = [], 0, len(src)

    def blank(text):
        return "".join(c if c == "\n" else " " for c in text)

    while i < n:
        c = src[i]
        m = RAW_STRING.match(src, i) if c in "rbc" and not (i and (src[i - 1].isalnum() or src[i - 1] == "_")) else None
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            out.append(blank(src[i:j]))
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            out.append(blank(src[i:j]))
            i = j
        elif m:
            end = src.find('"' + m.group(1), m.end())
            j = n if end < 0 else end + 1 + len(m.group(1))
            out.append(src[i:j] if keep_strings else blank(src[i:j]))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            out.append(src[i:j] if keep_strings else '"' + blank(src[i + 1:j - 1]) + '"')
            i = j
        elif c == "'":
            cm = re.match(r"'(?:\\.[^']*|[^\\'])'", src[i:i + 12])
            if cm:  # a char literal; otherwise a lifetime
                out.append(cm.group(0) if keep_strings else blank(cm.group(0)))
                i += len(cm.group(0))
            else:
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def scan(emit, path, text, regex, msg):
    for m in re.finditer(regex, text):
        emit(path, text.count("\n", 0, m.start()) + 1, msg)


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

    # (b) the desktop is a thin client: of the tole crates only tole-client may be
    # a dependency, and the session host reaches it at most through tole-client.
    # (b1) what the desktop itself declares, in every kind and target, by the real
    # package name (a rename does not hide it).
    for dep in desk["dependencies"]:
        name = norm(dep["name"])
        if name in SESSION_HOST or (name == TOLE_CLIENT and name not in ALLOWED_FOR_DESKTOP):
            error(desk_manifest, 1, f"{DESKTOP} lists {dep['name']} among its own dependencies; the desktop is "
                                    f"a thin client of the serve and may depend on {TOLE_CLIENT} only")
    # (b2) tole-client must be the workspace crate, not a registry package of that name.
    for pkg in desk_meta["packages"]:
        if norm(pkg["name"]) == TOLE_CLIENT and pkg["source"] is not None:
            error(desk_manifest, 1, f"{pkg['name']} {pkg['version']} resolves from {pkg['source']}; "
                                    "it must be the workspace crate")
    # (b3) the closure, with the desktop's own features all on (and only its own:
    # another member's feature is not counted). tole-tui and tole-cli are refused
    # everywhere, tole-client's own subtree included.
    closure = tree_names(root, DESKTOP, ["--all-features"])
    for name in closure:
        if norm(name) in SESSION_HOST - ALLOWED_VIA_CLIENT:
            error(desk_manifest, 1, f"{DESKTOP} depends (possibly through another crate) on {name}; "
                                    "the desktop never reaches the session host or the terminal UI")
    # (b4) tole-core only through tole-client: prune that subtree and look again.
    if any(norm(n) == TOLE_CLIENT for n in closure):
        outside = tree_names(root, DESKTOP, ["--all-features", "--prune", TOLE_CLIENT])
    else:
        outside = closure
    for name in outside:
        if norm(name) in ALLOWED_VIA_CLIENT:
            error(desk_manifest, 1, f"{DESKTOP} reaches {name} other than through {TOLE_CLIENT}; {name} is "
                                    f"allowed in the closure only as a dependency of {TOLE_CLIENT}")

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

    # (d) secondary, by text: in the desktop crate only ui/adapter.rs names the toolkit.
    # Fails closed: an unreadable file, a dangling link or no source at all ends the run with exit 2.
    desk_dir = crate_dir(root, DESKTOP)
    adapter = os.path.normpath(os.path.join(desk_dir, "src", "ui", "adapter.rs"))
    for path in rust_files(desk_dir):
        code = scrub(read(path), keep_strings=False)
        if os.path.normpath(path) != adapter:
            scan(error, path, code, r"\bgpui\w*",
                 "toolkit named outside src/ui/adapter.rs (re-export it from the adapter)")
        scan(error, path, code, r"#\s*!?\s*\[[^\]]*\bpath\s*=|\binclude(?:_str|_bytes)?\s*!",
             "a `path` attribute (also inside cfg_attr) and include! are not allowed in the desktop crate "
             "(they hide files from the scan)")


# ---- literals (advisory) -----------------------------------------------------

TEXT_METHODS = r"(?:child|label|text|title|description|tooltip|placeholder|aria_label)"
STRING = r'(?:r#*)?"'
LITERAL_RULES = [
    (r"\brgba?\s*\(\s*(?:0x|[0-9])", "numeric colour (use a tokens colour)"),
    (r"\bhsla?\s*\(\s*[0-9.]", "numeric colour (use a tokens colour)"),
    (r"\b(?:Hsla|Rgba)\s*\{\s*[a-z]\w*\s*:(?!:)", "colour built from fields (use a tokens colour)"),
    (r"\b0x(?:[0-9a-fA-F]_?){6,8}\b", "hex colour literal (use a tokens colour)"),
    (r'"#[0-9a-fA-F]{3,8}"', "hex colour string (use a tokens colour)"),
    (r"\b(?:px|rems?|Pixels|len)\s*\(\s*[0-9_]*\.?[0-9_]*[1-9]", "numeric size (use a tokens size)"),
    (rf"\.{TEXT_METHODS}\s*\(\s*(?:format!\s*\(\s*)?{STRING}", "string literal passed to a text element (use a strings entry)"),
    (rf"\.font_family\s*\(\s*{STRING}", "font name literal (use a tokens font family)"),
    (rf"\b(?:Label|SharedString|StyledText|Tooltip|Text|String)\s*::\s*(?:new|from)\s*\(\s*(?:format!\s*\(\s*)?{STRING}",
     "string literal turned into text (use a strings entry)"),
    (r'"(?:[^"\\\n]|\\.)*"\s*\.\s*(?:into|to_string|to_owned)\s*\(\s*\)',
     "string literal turned into text (use a strings entry)"),
]


def check_literals(root):
    desk_dir = crate_dir(root, DESKTOP)
    ui_prefix = os.path.join("src", "ui") + os.sep
    ui_files = [p for p in rust_files(desk_dir) if os.path.relpath(p, desk_dir).startswith(ui_prefix)]
    if not ui_files:
        raise RuntimeError(f"{desk_dir}: no .rs files under src/ui, nothing to check")
    for path in ui_files:
        text = scrub(read(path), keep_strings=True)
        for regex, msg in LITERAL_RULES:
            scan(warning, path, text, regex, msg)


def main(argv):
    modes = {"deps": check_deps, "size": check_size, "literals": check_literals}
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
