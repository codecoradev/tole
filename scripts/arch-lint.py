#!/usr/bin/env python3
"""Architecture guardrails of the CodeCora modular + thin-UI standard (#1249).

Called by scripts/arch-check.sh, which documents the rules; run it through
that script.

    arch-lint.py kinds <workspace-root>            enforced (#1250)
    arch-lint.py size <workspace-root> [--init]    enforced (#1251)
    arch-lint.py unsafe <workspace-root>           enforced (#1252)
    arch-lint.py cora <workspace-root>             enforced (#1249)

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

unsafe (rule D4): `unsafe_code = "deny"` is set for the whole workspace
(`[workspace.lints.rust]` in the root Cargo.toml, `[lints] workspace = true` in
every crate), so the compiler rejects an `unsafe` block anywhere that has not
opted in with `#[allow(unsafe_code)]` (or `#![allow(unsafe_code)]`). This mode
keeps the opt-ins honest: every site has a `SAFETY:` comment on the attribute
or within 3 lines of it, and the files that hold a site are exactly the files
listed in AGENTS.md between the unsafe-opt-ins marker comments. Nothing is
tolerated silently: a site, a list entry or a manifest that does not match is
an error, and a list that cannot be read is a failed run (exit 2).

cora: a .cora.yaml `ignore.rules` entry that silences `index-breaking-change`
lists exact `crates/<name>/src/*.rs` files that exist, are reached from a bin
target through `mod` declarations and are NOT reached from the crate's lib
target. Strict line parser (no YAML library): the rule name may appear nowhere else
in the file, and an unknown form is an error.
"""
import json
import os
import re
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

# Rule D4 (#1252): the opt-in list in AGENTS.md and what counts as a site.
AGENTS_FILE = "AGENTS.md"
UNSAFE_START = "<!-- unsafe-opt-ins:start -->"
UNSAFE_END = "<!-- unsafe-opt-ins:end -->"
SAFETY_WINDOW = 3  # lines above and below the attribute searched for `SAFETY:`
_ATTRIBUTE = re.compile(r"#!?\[[^\]]*\]")
_LINT_LIST = re.compile(r"\b(?:allow|expect)\s*\(([^()]*)\)")
_UNSAFE_CODE = re.compile(r"(?<![\w:])unsafe_code\b")
_LIST_ENTRY = re.compile(r"^\s*[-*]\s+`([^`]+)`")
_WORKSPACE_LINT = re.compile(r'^\[workspace\.lints\.rust\][^\[]*?^\s*unsafe_code\s*=\s*"deny"', re.M | re.S)
_CRATE_LINT = re.compile(r"^\[lints\][^\[]*?^\s*workspace\s*=\s*true\s*(?:#.*)?$", re.M | re.S)

# Cora scope guard: the one rule that may be silenced, and only for binary-only files.
CORA_CONFIG = ".cora.yaml"
CORA_RULE = "index-breaking-change"
CORA_HINT = "read CONTRIBUTING.md, section 'Cora Workflow', before changing this entry"
_CORA_HEAD = re.compile(r"""^    - rule: ?(?:"([\w-]+)"|([\w-]+))$""")
_CORA_PLAIN = re.compile(r'^    - "[^"\\#]*"$')
_CORA_KEY = re.compile(r"^      (files|reason):(.*)$")
_CORA_FILE = re.compile(r"""^        - "(crates/[\w.-]+/src/[\w./-]+\.rs)"$""")
_CORA_IGNORE_KEY = re.compile(r"""^["']?ignore["']?\s*:""")
_RS_TOKEN = re.compile(r'#\[\s*path\s*=\s*"([^"]+)"\s*\]|\bmod\s+(?:r#)?(\w+)\s*;')
_RS_UNKNOWN = re.compile(r"\binclude!\s*[(\[{]|cfg_attr\s*\([^\]]*\bpath\b")

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


# ---- unsafe rules (pure) ------------------------------------------------------


def find_opt_ins(text):
    """The `unsafe_code` opt-ins of one source file: [(line, has_safety)].

    A site is an attribute (`#[allow(unsafe_code)]`, `#![allow(unsafe_code)]`,
    the same with `expect`, or either inside `cfg_attr`) that names
    `unsafe_code`; an attribute that spans lines counts once, at its first
    line, and a line that is a `//` comment is never a site. `has_safety` is
    true when `SAFETY:` appears on the attribute or within SAFETY_WINDOW lines
    above or below it.
    """
    lines = text.split("\n")
    sites = []
    for match in _ATTRIBUTE.finditer(text):
        first = text.count("\n", 0, match.start())
        if lines[first].lstrip().startswith("//"):
            continue
        attribute = match.group(0)
        if not any(_UNSAFE_CODE.search(group) for group in _LINT_LIST.findall(attribute)):
            continue
        last = first + attribute.count("\n")
        window = lines[max(0, first - SAFETY_WINDOW):last + 1 + SAFETY_WINDOW]
        sites.append((first + 1, any("SAFETY:" in line for line in window)))
    return sites


def parse_unsafe_list(text, report=error, path=AGENTS_FILE):
    """The opt-in files listed in AGENTS.md: {file: line number}.

    The list is the bullets between a line holding only UNSAFE_START and a line
    holding only UNSAFE_END; each bullet starts with the file in backticks (text
    after it is a description). Missing, repeated or misordered markers raise
    (the list cannot be read); a line that is not a bullet, a bullet without a
    path and a duplicate are reported.
    """
    lines = text.split("\n")
    starts = [i for i, line in enumerate(lines) if line.strip() == UNSAFE_START]
    ends = [i for i, line in enumerate(lines) if line.strip() == UNSAFE_END]
    if len(starts) != 1 or len(ends) != 1 or starts[0] > ends[0]:
        raise RuntimeError(f"{path} must hold exactly one `{UNSAFE_START}` line followed by one `{UNSAFE_END}` line "
                           f"(found {len(starts)} and {len(ends)}): the opt-in list cannot be read")
    listed = {}
    for index in range(starts[0] + 1, ends[0]):
        raw = lines[index]
        if not raw.strip():
            continue
        entry = _LIST_ENTRY.match(raw)
        name = entry.group(1).strip() if entry else None
        if not name or not name.startswith("crates/") or not name.endswith(".rs"):
            report(path, index + 1, f"not understood: {raw.strip()!r} (a bullet that starts with the file in backticks, e.g. - `crates/x/src/y.rs`)")
        elif name in listed:
            report(path, index + 1, f"duplicate entry for {name}")
        else:
            listed[name] = index + 1
    return listed


def check_unsafe(sites, listed, report=error, list_path=AGENTS_FILE):
    """Rule D4 over the opt-ins found in the code.

    `sites`: {file: [(line, has_safety)]} from find_opt_ins; `listed`: {file:
    line number} from parse_unsafe_list. Every site needs its SAFETY comment,
    and the set of files must equal the list: a file with a site that is not
    listed fails, and so does a listed file with no site (or that is gone).
    """
    for path in sorted(sites):
        for line, has_safety in sites[path]:
            if not has_safety:
                report(path, line, f"unsafe_code opt-in without a `SAFETY:` comment (on the attribute or within {SAFETY_WINDOW} lines of it)")
        if path not in listed:
            report(path, sites[path][0][0],
                   f"opts in to unsafe_code but is not in the opt-in list of {list_path}: list it there, or remove the opt-in")
    for path, number in sorted(listed.items()):
        if path not in sites:
            report(list_path, number, f"{path}: listed as an unsafe opt-in but holds no #[allow(unsafe_code)] (or is gone): remove the entry")


def check_lints(root_manifest, manifests, report=error):
    """The lint wiring: the workspace denies `unsafe_code` and every crate inherits it.

    `root_manifest`: text of the root Cargo.toml; `manifests`: {path: text} of
    the crate manifests. Without this a crate that lost its `[lints]` table
    would silently allow `unsafe` again.
    """
    if not _WORKSPACE_LINT.search(root_manifest):
        report("Cargo.toml", 1, '[workspace.lints.rust] must set unsafe_code = "deny"')
    for path, text in sorted(manifests.items()):
        if not _CRATE_LINT.search(text):
            report(path, 1, 'no `[lints]` table with `workspace = true`: the crate does not inherit unsafe_code = "deny"')


# ---- cora scope rules ---------------------------------------------------------


def cora_code(line):
    """A .cora.yaml line without its comment (a `#` outside quotes, at the start or after a space)."""
    quote, i = None, 0
    while i < len(line):
        ch = line[i]
        if quote:
            if ch == "\\" and quote == '"':
                i += 1
            elif ch == quote:
                quote = None
        elif ch in "\"'":
            quote = ch
        elif ch == "#" and (i == 0 or line[i - 1] in " \t"):
            return line[:i].rstrip()
        i += 1
    return line.rstrip()


def parse_cora_scoped(text, report=error, path=CORA_CONFIG):
    """The `index-breaking-change` entries of `ignore.rules`: [(line, [(line, file)])].

    Reads only the layout .cora.yaml uses today (`ignore:` at column 0, `rules:` at
    indent 2, `- rule: X` or `- "plain string"` at 4, `files:`/`reason:` at 6, `files:`
    items at 8). The rule name must not appear anywhere in the file (any case, `-` or
    `_`) except as the head of such a scoped entry, and every `ignore.rules` item must
    be one of the forms above: else error (a flow map, escape or other YAML spelling
    would otherwise silence it everywhere).
    """
    lines = [cora_code(raw) for raw in text.split("\n")]
    heads, entries, in_rules, cur, files_mode = [], [], False, None, False
    for i, body in enumerate(lines):
        if _CORA_IGNORE_KEY.match(body):
            heads.append(i)
            if body != "ignore:":
                report(path, i + 1, f"the `ignore` key must be written exactly `ignore:`: {CORA_HINT}")
    if len(heads) > 1:
        report(path, heads[1] + 1, f"`ignore:` appears twice: {CORA_HINT}")
    for i in range(heads[0] + 1 if heads else len(lines), len(lines)):
        body = lines[i]
        if body[:1] not in ("", " ", "\t"):
            break  # next top-level key
        indent = len(body) - len(body.lstrip(" "))
        if not body.strip():
            continue
        if indent == 2:
            in_rules, cur = body == "  rules:", None
        elif not in_rules:
            continue
        elif indent == 4:
            head = _CORA_HEAD.match(body)
            cur, files_mode = None, False
            if head:
                cur = {"rule": head.group(1) or head.group(2), "line": i + 1, "files": []}
                entries.append(cur)
            elif not _CORA_PLAIN.match(body):
                report(path, i + 1, f"ignore.rules item not understood (only `- \"plain string\"` or `- rule: id`): {body.strip()!r}: {CORA_HINT}")
        elif cur is None:
            report(path, i + 1, f"not understood in ignore.rules: {body.strip()!r}: {CORA_HINT}")
        elif cur["rule"] != CORA_RULE:
            continue  # the body of another rule's entry
        elif indent == 6 and (key := _CORA_KEY.match(body)):
            files_mode, value = key.group(1) == "files", key.group(2).strip()
            if (files_mode and value) or (not files_mode and not re.fullmatch(r'"[^"\\]*"', value)):
                report(path, i + 1, f"`{key.group(1)}:` must be a block list or one plain double-quoted string: {CORA_HINT}")
        elif indent == 8 and files_mode and (item := _CORA_FILE.match(body)):
            cur["files"].append((i + 1, item.group(1)))
        else:
            report(path, i + 1, f"not understood in the {CORA_RULE} entry: {body.strip()!r} (only `reason:` and a `files:` list of exact crates/<name>/src/*.rs paths): {CORA_HINT}")
    scoped = [e for e in entries if e["rule"] == CORA_RULE]
    known = {e["line"] for e in scoped}
    for number, body in enumerate(lines, 1):
        if CORA_RULE in body.lower().replace("_", "-") and number not in known:
            report(path, number, f"{CORA_RULE} named outside a scoped `- rule: {CORA_RULE}` entry of ignore.rules (it would be silenced or disabled unscoped): {CORA_HINT}")
    for entry in scoped:
        if not entry["files"]:
            report(path, entry["line"], f"{CORA_RULE} entry has no readable `files:` list (it would be unscoped): {CORA_HINT}")
    return [(e["line"], e["files"]) for e in scoped]


def reachable(root, rel, stop=()):
    """Files reached from the crate root file `rel` through `mod x;` and `#[path]`.

    A file in `stop` is recorded but not entered (it is already the answer).
    Raises RuntimeError when the answer is unknown: a `mod` that does not resolve
    to exactly one file, a `#[path]` not on a `mod x;`, `include!(` or `cfg_attr(..path..)`.
    `include_str!`, macro-generated modules and other crates are not followed.
    """
    seen, todo = set(), [(rel, True)]
    while todo:
        cur, mod_rs = todo.pop()
        if cur in seen:
            continue
        seen.add(cur)
        if cur in stop and cur != rel:
            continue
        text = read_text(os.path.join(root, cur))
        if _RS_UNKNOWN.search(text):
            raise RuntimeError(f"{cur}: `include!(` or a cfg_attr path, so what the library reaches is unknown: {CORA_HINT}")
        base = os.path.dirname(cur)
        own = base if mod_rs or os.path.basename(cur) == "mod.rs" else os.path.join(base, os.path.basename(cur)[:-3])
        pending, used, attrs = None, 0, 0
        for number, line in enumerate(text.split("\n"), 1):
            code = line.strip()
            for m in _RS_TOKEN.finditer("" if code.startswith("//") else line):
                if m.group(1):
                    pending = m.group(1)
                    continue
                names = [os.path.join(base, pending)] if pending else [os.path.join(own, m.group(2) + ".rs"), os.path.join(own, m.group(2), "mod.rs")]
                found = [os.path.normpath(n) for n in names if os.path.isfile(os.path.join(root, n))]
                if len(found) != 1:
                    raise RuntimeError(f"{cur}:{number}: `mod {m.group(2)};` resolves to {len(found)} files, so what the library reaches is unknown: {CORA_HINT}")
                todo.append((found[0].replace(os.sep, "/"), bool(pending)))
                used, pending = used + bool(pending), None
            if code and not code.startswith("//"):
                attrs += len(re.findall(r"#\s*\[\s*path\b", code))
                if not (code.startswith("#[") and code.endswith("]")):
                    pending = None
        if used != attrs:
            raise RuntimeError(f"{cur}: a `#[path]` that is not directly on a `mod x;` ({attrs} attributes, {used} used), so what the library reaches is unknown: {CORA_HINT}")
    return seen


def check_cora_scope(entries, packages, root, report=error, path=CORA_CONFIG):
    """Each scoped file exists, is reached from a bin root of its crate and not from its lib root."""
    stop = {rel for _, files in entries for _, rel in files}
    for line, files in entries:
        for number, rel in files:
            owner = next((p for p in packages if rel.startswith(os.path.dirname(p["manifest"]) + "/src/")), None)
            if not os.path.isfile(os.path.join(root, rel)) or owner is None:
                report(path, number, f"{rel}: not a source file of a workspace crate: {CORA_HINT}")
                continue
            libs = [t for k, t in owner["targets"] if k in ("lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro")]
            bins = [t for k, t in owner["targets"] if k == "bin"]
            if any(rel in reachable(root, t, stop) for t in libs):
                report(path, number, f"{rel} is reached from the library target of {owner['name']}: a silenced {CORA_RULE} would hide real public API removals; {CORA_HINT}")
            elif not any(rel in reachable(root, t) for t in bins):
                report(path, number, f"{rel} is not reached from any bin target of {owner['name']} through mod declarations, so binary-only is not proven: {CORA_HINT}")


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
            "targets": [(k, os.path.relpath(t["src_path"], root).replace(os.sep, "/")) for t in pkg.get("targets", []) for k in t.get("kind", [])],
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


def read_text(path):
    """A file as UTF-8 text; unreadable or not UTF-8 raises (never a silent empty)."""
    with open(path, "rb") as fh:
        return fh.read().decode("utf-8")


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


def run_unsafe(root):
    packages = load_packages(root)
    sites, manifests = {}, {}
    for pkg in packages:
        manifests[pkg["manifest"].replace(os.sep, "/")] = read_text(os.path.join(pkg["dir"], "Cargo.toml"))
        for here, dirs, names in os.walk(pkg["dir"], followlinks=False, onerror=_walk_error):
            dirs[:] = sorted(d for d in dirs if d != "target")
            for name in sorted(names):
                if not name.endswith(".rs"):
                    continue
                full = os.path.join(here, name)
                found = find_opt_ins(read_text(full))
                if found:
                    sites[os.path.relpath(full, root).replace(os.sep, "/")] = found
    agents_path = os.path.join(root, AGENTS_FILE)
    if not os.path.isfile(agents_path):
        raise RuntimeError(f"{AGENTS_FILE} is missing: it holds the list of unsafe opt-in files")
    listed = parse_unsafe_list(read_text(agents_path))
    check_lints(read_text(os.path.join(root, "Cargo.toml")), manifests)
    check_unsafe(sites, listed)
    info(f"unsafe: {sum(len(v) for v in sites.values())} opt-ins in {len(sites)} files across {len(packages)} crates; "
         f"{len(listed)} files listed in {AGENTS_FILE}")


def run_cora(root):
    config = os.path.join(root, CORA_CONFIG)
    if not os.path.isfile(config):
        info(f"cora: no {CORA_CONFIG}, nothing is silenced")
        return
    entries = parse_cora_scoped(read_text(config))
    check_cora_scope(entries, load_packages(root), root)
    info(f"cora: {sum(len(f) for _, f in entries)} file(s) in {len(entries)} scoped {CORA_RULE} entr(ies) checked as binary-only")


# ---- main ---------------------------------------------------------------------


def main(argv):
    modes = {"kinds": run_kinds, "size": run_size, "unsafe": run_unsafe, "cora": run_cora}
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
