#!/usr/bin/env bash
# Self-test for scripts/desktop-check.sh (#1028): throwaway workspaces, one
# case per rule and per known bypass. Run by CI; also runnable locally. Needs
# cargo (offline: the fixtures use path dependencies only) and python3. Portable
# (no GNU-only sed -i).
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/desktop-check.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export CARGO_TARGET_DIR="$tmp/target"
fails=0
ws="$tmp/ws"

# manifest <dir> <name> [version]: write crates/<dir>/Cargo.toml for package
# <name> from stdin (everything after the [package] table).
manifest() {
  mkdir -p "$ws/crates/$1/src"
  { printf '[package]\nname = "%s"\nversion = "%s"\nedition = "2021"\n\n' "$2" "${3:-0.12.0}"; cat; } > "$ws/crates/$1/Cargo.toml"
  [ -f "$ws/crates/$1/src/lib.rs" ] || echo 'pub fn f() {}' > "$ws/crates/$1/src/lib.rs"
}

# fake <dir> <name> [version]: a crate with no dependencies, standing in for a real one.
fake() { manifest "$1" "$2" "${3:-}" < /dev/null; }

# desktop / model: write a manifest for the two crates from stdin.
desktop() { manifest tole-desktop tole-desktop; }
model() { manifest tole-desktop-model tole-desktop-model; }

# new_ws: a clean workspace. The desktop depends on its model; nothing else.
new_ws() {
  rm -rf "$ws"
  mkdir -p "$ws/crates"
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n' > "$ws/Cargo.toml"
  model < /dev/null
  desktop <<'TOML'
[dependencies]
tole-desktop-model = { path = "../tole-desktop-model" }
TOML
  mkdir -p "$ws/crates/tole-desktop/src/ui"
  printf '// GPUI is only named in this comment\nuse gpui::div;\n' > "$ws/crates/tole-desktop/src/ui/adapter.rs"
  echo 'fn view() { let _ = adapter::div().child(strings::TITLE).w(adapter::len(size::ROW_HEIGHT)); }' > "$ws/crates/tole-desktop/src/ui/shell.rs"
}

# put <path-under-ws> <text>: write a file of the fixture.
put() { mkdir -p "$(dirname "$ws/$1")"; printf '%s\n' "$2" > "$ws/$1"; }
ui() { put "crates/tole-desktop/src/ui/shell.rs" "$1"; }

# expect <name> <mode> <exit> [pattern]: run the script on the fixture, compare
# the exit code and, if given, require the output to match the pattern.
expect() {
  local name="$1" mode="$2" want="$3" pat="${4:-}" got=0 out
  out=$(bash "$script" "$mode" "$ws" 2>&1) || got=$?
  if [ "$got" != "$want" ]; then
    echo "FAIL $name: want exit $want, got $got"; echo "$out" | sed 's/^/     /'; fails=$((fails + 1))
  elif [ -n "$pat" ] && ! grep -qE -- "$pat" <<< "$out"; then
    echo "FAIL $name: output lacks /$pat/"; echo "$out" | sed 's/^/     /'; fails=$((fails + 1))
  else
    echo "ok   $name"
  fi
}

# expect_clean <name> <mode>: exit 0 and no warning or error line.
expect_clean() {
  local name="$1" mode="$2" got=0 out
  out=$(bash "$script" "$mode" "$ws" 2>&1) || got=$?
  if [ "$got" != 0 ] || grep -qE '^(error|warning): ' <<< "$out"; then
    echo "FAIL $name: want a clean run, got exit $got"; echo "$out" | sed 's/^/     /'; fails=$((fails + 1))
  else
    echo "ok   $name"
  fi
}

# ---- deps: a clean workspace ------------------------------------------------
new_ws
expect_clean "clean workspace: deps" deps

# ---- deps: the session host, in every spelling the resolver normalises ------
new_ws; fake tole-cli tole-cli; desktop <<'TOML'
[dependencies]
tole-cli = { path = "../tole-cli" }
TOML
expect "tole-cli" deps 1 "tole-cli"

new_ws; fake tole-cli tole-cli; desktop <<'TOML'
[dev_dependencies]
tole-cli = { path = "../tole-cli" }
TOML
expect "tole-cli in [dev_dependencies]" deps 1 "tole-cli"

new_ws; fake tole-cli tole-cli; desktop <<'TOML'
[build_dependencies]
tole-cli = { path = "../tole-cli" }
TOML
expect "tole-cli in [build_dependencies]" deps 1 "tole-cli"

new_ws; fake tole-cli tole-cli; desktop <<'TOML'
[dev-dependencies]
tole_cli = { package = "tole-cli", path = "../tole-cli" }
TOML
expect "tole_cli key" deps 1 "tole-cli"

new_ws; fake tole-core-underscore tole_core; desktop <<'TOML'
[dependencies]
engine = { package = "tole_core", path = "../tole-core-underscore" }
TOML
expect "package = \"tole_core\"" deps 1 "tole_core"

new_ws; fake tole-core-mixed Tole-Core; desktop <<'TOML'
[dependencies]
engine = { package = "Tole-Core", path = "../tole-core-mixed" }
TOML
expect "package = \"Tole-Core\"" deps 1 "Tole-Core"

new_ws; fake tole-cli tole-cli
printf '[workspace]\nresolver = "2"\nmembers = ["crates/*"]\n\n[workspace.dependencies]\nengine = { package = "tole-cli", path = "crates/tole-cli" }\n' > "$ws/Cargo.toml"
desktop <<'TOML'
[dependencies]
engine = { workspace = true }
TOML
expect "workspace = true alias of tole-cli" deps 1 "tole-cli"

new_ws; fake tole-tui tole-tui; desktop <<'TOML'
[target.'cfg(unix)'.dependencies]
tui = { package = "tole-tui", path = "../tole-tui" }
TOML
expect "tole-tui, target-specific and renamed" deps 1 "tole-tui"

new_ws; fake tole-core tole-core; manifest helper helper <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }
TOML
desktop <<'TOML'
[dependencies]
helper = { path = "../helper" }
TOML
expect "tole-core through another crate" deps 1 "tole-core"

# ---- deps: tole-client is the desktop's one tole dependency (#1029) ------------
# client_ws: tole-client depends on tole-core, as the real one does.
client_ws() {
  new_ws; fake tole-core tole-core
  manifest tole-client tole-client <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }
TOML
}
desktop_with() { # desktop_with <extra dependency line...>: the model plus the lines on stdin
  { printf '[dependencies]\ntole-desktop-model = { path = "../tole-desktop-model" }\n'; cat; } | desktop
}

client_ws; desktop_with <<'TOML'
tole-client = { path = "../tole-client" }
TOML
expect_clean "tole-client, with tole-core only through it, is allowed" deps

client_ws; desktop_with <<'TOML'
tole-client = { path = "../tole-client" }
tole-core = { path = "../tole-core" }
TOML
expect "a direct tole-core next to tole-client is refused" deps 1 "lists tole-core among its own dependencies"

client_ws; desktop <<'TOML'
[dependencies]
tole-client = { path = "../tole-client" }

[dev-dependencies]
core = { package = "tole-core", path = "../tole-core" }
TOML
expect "a renamed dev-dependency on tole-core is refused" deps 1 "lists tole-core among its own dependencies"

client_ws; desktop <<'TOML'
[dependencies]
tole-client = { path = "../tole-client" }

[target.'cfg(unix)'.build-dependencies]
core = { package = "tole-core", path = "../tole-core" }
TOML
expect "a target-specific build-dependency on tole-core is refused" deps 1 "lists tole-core among its own dependencies"

# tole-core reached through another crate, with tole-client present: only the
# subtree of tole-client is allowed to hold it.
client_ws; manifest helper helper <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }
TOML
desktop_with <<'TOML'
tole-client = { path = "../tole-client" }
helper = { path = "../helper" }
TOML
expect "tole-core through another crate, next to tole-client, is refused" deps 1 "other than through tole-client"

client_ws; fake tole-cli tole-cli
manifest tole-client tole-client <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }
tole-cli = { path = "../tole-cli" }
TOML
desktop_with <<'TOML'
tole-client = { path = "../tole-client" }
TOML
expect "tole-cli through tole-client is refused" deps 1 "on tole-cli"

client_ws; fake tole-tui tole-tui
manifest tole-client tole-client <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }

[target.'cfg(windows)'.dependencies]
tole-tui = { path = "../tole-tui" }
TOML
desktop_with <<'TOML'
tole-client = { path = "../tole-client" }
TOML
expect "tole-tui through tole-client (target-specific) is refused" deps 1 "on tole-tui"

client_ws; desktop <<'TOML'
[dependencies]
client = { package = "tole-client", path = "../tole-client" }
TOML
expect_clean "tole-client under another name is the same allowed crate" deps

client_ws; model <<'TOML'
[dependencies]
tole-client = { path = "../tole-client" }
TOML
expect "tole-client in the model is refused" deps 1 "tole-desktop-model depends"

# ---- deps: the model crate ---------------------------------------------------
new_ws; fake tole-core tole-core; model <<'TOML'
[dependencies]
tole-core = { path = "../tole-core" }
TOML
expect "tole-core in the model" deps 1 "tole-desktop-model depends"

new_ws; fake gpui-x gpui-x; model <<'TOML'
[dependencies]
gpui-x = { path = "../gpui-x" }
TOML
expect "a gpui package in the model" deps 1 "tole-desktop-model depends"

new_ws; fake zed-fork zed-fork; model <<'TOML'
[target.'cfg(windows)'.dependencies]
zed-fork = { path = "../zed-fork" }
TOML
expect "a zed package in the model, target-specific" deps 1 "tole-desktop-model depends"

# ---- deps: the toolkit pins ---------------------------------------------------
new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[features]
gpui = ["dep:gpui"]

[dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12", optional = true }
TOML
expect "toolkit from a path, not crates.io" deps 1 "crates.io"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12" }
TOML
expect "toolkit not optional" deps 1 "must be optional"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[features]
gpui = ["dep:gpui"]

[dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3", optional = true }
TOML
expect "version = \"=0.3\" is not an exact pin" deps 1 "exact-pinned"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[features]
gpui = ["dep:gpui"]

[dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "0.3.12", optional = true }
TOML
expect "unpinned toolkit" deps 1 "exact-pinned"

new_ws; fake gpui-component gpui-component 0.9.1; desktop <<'TOML'
[features]
gpui = ["dep:gpui-component"]

[dependencies.gpui-component]
path = "../gpui-component"
version = "0.9"
optional = true
TOML
expect "table-form toolkit dependency, unpinned" deps 1 "exact-pinned"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[features]
gpui = ["dep:ui"]

[dependencies]
ui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12", optional = true }
TOML
expect "toolkit renamed away from gpui*" deps 1 "gpui\* name"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[dev_dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12" }
TOML
expect "toolkit in [dev_dependencies]" deps 1 "dev- or build"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[build_dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12" }
TOML
expect "toolkit in [build_dependencies]" deps 1 "dev- or build"

new_ws; fake gpui-pre gpui-pre 0.3.12; desktop <<'TOML'
[features]
default = ["window"]
window = ["gpui"]
gpui = ["dep:gpui"]

[dependencies]
gpui = { package = "gpui-pre", path = "../gpui-pre", version = "=0.3.12", optional = true }
TOML
expect "a default feature enables the toolkit" deps 1 "without the gpui feature"

new_ws; desktop <<'TOML'
[dependencies]
gpui = { package = "gpui-pre", registry = "x", version = "=0.3.12", optional = true }
TOML
expect "alternative registry fails closed (cannot resolve)" deps 2 "could not finish"

# ---- deps: the text check of the desktop crate ----------------------------------
new_ws; put crates/tole-desktop/src/lib.rs 'use gpui::App;'
expect "gpui in the crate root" deps 1 "outside src/ui/adapter.rs"

new_ws; put crates/tole-desktop/src/lib.rs 'use ::gpui as g;
fn f(_: g::App) {}'
expect "use ::gpui as g" deps 1 "outside src/ui/adapter.rs"

new_ws; put crates/tole-desktop/src/lib.rs 'use {gpui as g};'
expect "use {gpui as g}" deps 1 "outside src/ui/adapter.rs"

new_ws; ui 'use gpui_platform::application;'
expect "gpui_platform in a view" deps 1 "outside src/ui/adapter.rs"

new_ws; put crates/tole-desktop/tests/t.rs 'use gpui::App;'
expect "gpui in tests/" deps 1 "outside src/ui/adapter.rs"

new_ws; put crates/tole-desktop/build.rs 'fn main() { let _ = gpui_component::init; }'
expect "gpui in build.rs" deps 1 "outside src/ui/adapter.rs"

new_ws; put crates/tole-desktop/src/lib.rs '#[path = "../../leak.rs"]
mod leak;'
expect "#[path]" deps 1 "path. attribute"

new_ws; put crates/tole-desktop/src/lib.rs 'include!("../../leak.rs");'
expect "include!" deps 1 "include!"

new_ws; put crates/tole-desktop/src/lib.rs '// gpui::App only in a comment
/* and gpui_platform::application() in a block */
const S: &str = "gpui::App in a string";
#[cfg(feature = "gpui")]
fn f() {}'
expect_clean "comments, strings and the feature name are not toolkit use" deps

# ---- the one walker: where the checks look ------------------------------------------
big() { seq 1 "${2:-600}" | sed 's/.*/\/\/ line/' > "$1"; }

# A checkout under a directory named `target` is still checked (the old find
# pattern matched the absolute path and counted zero files).
ws_saved="$ws"; ws="$tmp/parent/target/ws"
new_ws; big "$ws/crates/tole-desktop/src/big.rs"
expect "workspace under a parent directory named target: size is checked" size 1 "cap 500 per file"
new_ws; put crates/tole-desktop/src/lib.rs 'use gpui::App;'
expect "workspace under a parent directory named target: text check runs" deps 1 "outside src/ui/adapter.rs"
ws="$ws_saved"

# A source directory named `target` inside a crate is scanned...
new_ws; mkdir -p "$ws/crates/tole-desktop/src/target"; big "$ws/crates/tole-desktop/src/target/big.rs"
expect "src/target/ module directory: size is checked" size 1 "cap 500 per file"
new_ws; put crates/tole-desktop/src/target/leak.rs 'use gpui::App;'
expect "src/target/ module directory: gpui outside the adapter" deps 1 "outside src/ui/adapter.rs"
new_ws; put crates/tole-desktop/src/ui/target/view.rs 'fn v() { let _ = div().child("New task"); }'
expect "src/ui/target/ module directory: literals are scanned" literals 0 "^warning: "

# ...and only the crate's own top-level target/ (cargo's output) is skipped.
new_ws; mkdir -p "$ws/crates/tole-desktop/target"; big "$ws/crates/tole-desktop/target/generated.rs"
expect "the crate's own target/ is skipped" size 0 "ok \(enforced rules\)"

# A symlinked .rs file is followed.
new_ws; mkdir -p "$tmp/outside"; big "$tmp/outside/linked.rs"
ln -s "$tmp/outside/linked.rs" "$ws/crates/tole-desktop/src/linked.rs"
expect "symlinked .rs file is counted" size 1 "cap 500 per file"
new_ws; printf 'use gpui::App;\n' > "$tmp/outside/leak.rs"; ln -s "$tmp/outside/leak.rs" "$ws/crates/tole-desktop/src/leak.rs"
expect "symlinked .rs file is scanned" deps 1 "outside src/ui/adapter.rs"
new_ws; mkdir -p "$tmp/outside/dir"; ln -s "$tmp/outside/dir" "$ws/crates/tole-desktop/src/linked_dir"
expect "symlinked directory is refused" size 2 "could not finish"

# A crate with nothing to check is an error, never a pass.
new_ws; rm "$ws/crates/tole-desktop-model/src/lib.rs"
expect "model crate without any .rs file" size 2 "no .rs files"
new_ws; rm -r "$ws/crates/tole-desktop-model/src"
expect "model crate without src/" size 2 "no src/"
new_ws; rm -r "$ws/crates/tole-desktop/src"
expect "desktop crate without src/" deps 2 "could not finish"
new_ws; rm -r "$ws/crates/tole-desktop/src/ui"
expect "desktop crate without src/ui for the literals check" literals 2 "no .rs files under src/ui"

# ---- an unreadable directory is an error, not a skipped subtree ------------------------
# Run as a user that cannot read the directory: root bypasses file modes, so as
# root the test drops to uid 65534 with setpriv; with neither a non-root user nor
# setpriv it is skipped (and says so).
new_ws; mkdir -p "$ws/crates/tole-desktop/src/hidden"; big "$ws/crates/tole-desktop/src/hidden/big.rs" 900
chmod 000 "$ws/crates/tole-desktop/src/hidden"; chmod 755 "$tmp"
got=0
if [ "$(id -u)" != 0 ]; then
  out=$(bash "$script" size "$ws" 2>&1) || got=$?; ran=1
elif command -v setpriv > /dev/null 2>&1; then
  out=$(setpriv --reuid=65534 --regid=65534 --clear-groups bash "$script" size "$ws" 2>&1) || got=$?; ran=1
else
  ran=0
fi
if [ "$ran" = 0 ]; then echo "skip unreadable directory (root and no setpriv)"
elif [ "$got" = 2 ] && grep -q "cannot read" <<< "$out"; then echo "ok   unreadable directory fails closed"
else echo "FAIL unreadable directory: want exit 2 'cannot read', got $got"; echo "$out" | sed 's/^/     /'; fails=$((fails + 1)); fi
chmod 755 "$ws/crates/tole-desktop/src/hidden"

# ---- deps: target sources named in the manifest ---------------------------------------
new_ws; mkdir -p "$ws/crates/tole-desktop/target"; big "$ws/crates/tole-desktop/target/big.rs" 900
desktop <<'TOML'
[dependencies]
tole-desktop-model = { path = "../tole-desktop-model" }

[[bin]]
name = "hidden"
path = "target/big.rs"
TOML
expect "[[bin]] path under the crate's target/" deps 1 "under the crate's target/"

new_ws; mkdir -p "$ws/extra"; big "$ws/extra/t.rs" 900
desktop <<'TOML'
[dependencies]
tole-desktop-model = { path = "../tole-desktop-model" }

[[test]]
name = "outside"
path = "../../extra/t.rs"
TOML
expect "[[test]] path outside the crate" deps 1 "outside the crate"

new_ws; printf 'fn main() {}\n' > "$ws/crates/tole-desktop/build.rs"
desktop <<'TOML'
[package.metadata.x]
y = 1
TOML
expect_clean "a build script inside the crate is fine" deps

# ---- deps: features of the checked crate only ----------------------------------------
# An optional feature of a helper that pulls tole-core is not on for the desktop.
new_ws; fake tole-core tole-core; manifest helper helper <<'TOML'
[features]
x = ["dep:tole-core"]

[dependencies]
tole-core = { path = "../tole-core", optional = true }
TOML
desktop <<'TOML'
[dependencies]
tole-desktop-model = { path = "../tole-desktop-model" }
helper = { path = "../helper" }
TOML
expect_clean "an optional feature of a dependency that is off is not counted" deps

new_ws; fake tole-core tole-core; manifest helper helper <<'TOML'
[features]
x = ["dep:tole-core"]

[dependencies]
tole-core = { path = "../tole-core", optional = true }
TOML
desktop <<'TOML'
[dependencies]
tole-desktop-model = { path = "../tole-desktop-model" }
helper = { path = "../helper", features = ["x"] }
TOML
expect "the same feature switched on by the desktop is counted" deps 1 "tole-core"

# ---- deps: text check, hiding tricks ----------------------------------------------------
new_ws; put crates/tole-desktop/src/lib.rs '#[cfg_attr(all(), path = "../../leak.rs")]
mod leak;'
expect "path attribute inside cfg_attr" deps 1 "path. attribute"

new_ws; put crates/tole-desktop/src/lib.rs '#![cfg_attr(unix, path = "x.rs")]'
expect "inner path attribute inside cfg_attr" deps 1 "path. attribute"

new_ws; ui 'fn f() { let _ = br"\"; }
use gpui::App;
fn g() { let _ = "q"; }'
expect "gpui after a br raw string ending in a backslash" deps 1 "outside src/ui/adapter.rs"

new_ws; ui 'fn f() { let _ = cr#"x\"#; }
use gpui::App;'
expect "gpui after a cr raw string" deps 1 "outside src/ui/adapter.rs"

new_ws; ui 'const A: &[u8] = b"gpui::App";
const B: &[u8] = br"gpui::App";
const C: &[u8] = br#"gpui::App "quoted""#;
const D: &str = r"gpui::App";'
expect_clean "gpui inside byte and raw strings is not toolkit use" deps

# ---- fail closed ------------------------------------------------------------------
new_ws; ln -s missing-target.rs "$ws/crates/tole-desktop/src/dangling.rs"
expect "dangling symlink" deps 2 "could not finish"

new_ws; printf '\377\376 not utf-8 \200\n' > "$ws/crates/tole-desktop/src/bad.rs"
expect "non-UTF-8 source file" deps 2 "could not finish"

new_ws; ln -s missing-target.rs "$ws/crates/tole-desktop/src/ui/dangling.rs"
expect "dangling symlink in ui, literals" literals 2 "could not finish"

new_ws; rm "$ws/crates/tole-desktop/Cargo.toml"
expect "missing manifest" deps 2 "could not finish"

# A helper that crashes, or exits 1 without an error line, must not read as ok.
new_ws
mkdir -p "$tmp/scripts-copy"; cp "$here/desktop-check.sh" "$tmp/scripts-copy/"
printf 'import sys\nraise ZeroDivisionError("boom")\n' > "$tmp/scripts-copy/desktop-lint.py"
got=0; out=$(bash "$tmp/scripts-copy/desktop-check.sh" deps "$ws" 2>&1) || got=$?
if [ "$got" = 2 ] && grep -q "could not finish" <<< "$out"; then echo "ok   uncaught exception in the helper"; else echo "FAIL uncaught exception in the helper: exit $got"; echo "$out"; fails=$((fails + 1)); fi
printf 'import sys\nsys.exit(1)\n' > "$tmp/scripts-copy/desktop-lint.py"
got=0; out=$(bash "$tmp/scripts-copy/desktop-check.sh" deps "$ws" 2>&1) || got=$?
if [ "$got" = 2 ]; then echo "ok   helper exits 1 without a finding"; else echo "FAIL helper exits 1 without a finding: exit $got"; echo "$out"; fails=$((fails + 1)); fi
printf 'import sys\nsys.exit(7)\n' > "$tmp/scripts-copy/desktop-lint.py"
got=0; out=$(bash "$tmp/scripts-copy/desktop-check.sh" literals "$ws" 2>&1) || got=$?
if [ "$got" = 2 ]; then echo "ok   helper exits with another code"; else echo "FAIL helper exits with another code: exit $got"; echo "$out"; fails=$((fails + 1)); fi

# python3 missing: a PATH with the tools the script needs, but no python3.
shim="$tmp/shim"; mkdir -p "$shim"
for t in dirname mktemp sed grep find wc sort rm cat; do ln -s "$(command -v $t)" "$shim/$t"; done
got=0; out=$(PATH="$shim" "$BASH" "$script" deps "$ws" 2>&1) || got=$?
if [ "$got" = 2 ] && grep -q "python3 is needed" <<< "$out"; then echo "ok   python3 missing"; else echo "FAIL python3 missing: exit $got"; echo "$out"; fails=$((fails + 1)); fi

# ---- size ---------------------------------------------------------------------------
new_ws; seq 1 501 | sed 's/.*/\/\/ line/' > "$ws/crates/tole-desktop-model/src/big.rs"
expect "model file over 500 lines" size 1 "cap 500 per file"

new_ws; for i in 1 2 3 4 5 6 7 8 9; do seq 1 450 | sed 's/.*/\/\/ line/' > "$ws/crates/tole-desktop/src/f$i.rs"; done
expect "desktop crate over 4000 lines" size 1 "cap 4000 per crate"

# Between the old cap (2000) and the new one (4000, owner rule D1): allowed.
new_ws; for i in 1 2 3 4 5 6; do seq 1 450 | sed 's/.*/\/\/ line/' > "$ws/crates/tole-desktop/src/f$i.rs"; done
expect "desktop crate of 2700 lines is under the cap" size 0 "ok \(enforced rules\)"

new_ws; fake other other; seq 1 600 | sed 's/.*/\/\/ line/' > "$ws/crates/other/src/lib.rs"
expect "other crates only warn" size 0 "warning: .*other"

# ---- literals: advisory, warn and never fail --------------------------------------
new_ws
expect_clean "clean ui: no warning" literals

lit() { # lit <name> <source>: the finding is a warning and the run still passes
  new_ws; ui "$2"; expect "$1" literals 0 "^warning: "
}
lit "numeric rgb" 'fn v() { let _ = adapter::rgb(0x5CC8B0); }'
lit "hex literal with underscores" 'const C: u32 = 0x5C_C8_B0;'
lit "hex string" 'const C: &str = "#5CC8B0";'
lit "Hsla built from fields" 'fn v() { let _ = Hsla { h: 0.1, s: 0.2, l: 0.3, a: 1.0 }; }'
lit "numeric px" 'fn v() { let _ = div().w(px(264.0)); }'
lit "numeric len(..), the real size helper" 'fn v() { let _ = div().w(adapter::len(264.0)); }'
lit "string literal child" 'fn v() { let _ = div().child("New task"); }'
lit "string literal child, split over lines" 'fn v() {
    let _ = div().child(
        "New task",
    );
}'
lit "raw string child" 'fn v() { let _ = div().child(r#"New task"#); }'
lit "font family literal" 'fn v() { let _ = div().font_family("Inter"); }'
lit "String::from literal" 'fn v() { let _ = div().child(String::from("New task")); }'
lit "literal .into()" 'fn v() { let _ = div().child("New task".into()); }'

new_ws; ui 'fn v() { let _ = div().child(strings::TITLE); } // .child("x") in a comment, #5CC8B0
/* rgb(0x5CC8B0) and .child("y")
   in a block comment */
const S: &str = "//";
fn w() { let _ = div().child(strings::BODY); }'
expect_clean "comments are ignored, and \"//\" does not hide code" literals

# The enforced gate does not fail on a literal.
new_ws; ui 'fn v() { let _ = div().child("New task"); }'
expect "all: a literal warns but does not fail" all 0 "ok \(enforced rules; literals are advisory\)"
expect "literals mode does not claim an enforced rule ran" literals 0 "advisory only, no enforced rule ran"

if [ "$fails" -gt 0 ]; then echo "$fails case(s) failed"; exit 1; fi
echo "all desktop-check cases passed"
