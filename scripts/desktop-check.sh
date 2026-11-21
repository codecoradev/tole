#!/usr/bin/env bash
# Guardrails of the desktop crates (#1028).
#
# Usage: scripts/desktop-check.sh [deps|size|literals|all] [workspace-root]
#   workspace-root defaults to the repository root (the self-test points it at
#   fixture workspaces). Needs bash, cargo and Python 3.8+. `deps` runs
#   `cargo metadata` and `cargo tree`, both --offline, so the crates must be downloaded first
#   (`cargo fetch`).
#
# ENFORCED (a violation makes this script exit 1; CI runs it in the `Desktop`
# workflow, which is not a required check unless the owner makes it one):
#
# deps      Over the resolved dependency graph (`cargo metadata` for what is
#           declared and where it comes from, `cargo tree -p` for what a build
#           of that crate reaches, so table form, renames, target tables,
#           underscore or case spellings and `workspace = true` are already
#           normalised and another workspace member's feature is not counted):
#             - tole-desktop-model (view state, tokens, strings) has no package
#               named gpui* or zed*, and none of tole-cli, tole-core, tole-tui,
#               tole-client, anywhere in its dependency closure. As it is a
#               separate crate, the compiler also refuses a toolkit name there.
#             - tole-desktop lists none of tole-cli (the session host), tole-core
#               or tole-tui among its own dependencies, in any kind or target.
#               tole-client (#1040) is the one tole crate it may depend on
#               (ALLOWED_FOR_DESKTOP in desktop-lint.py; it must be the
#               workspace crate). tole-core is in its closure only through
#               tole-client (the closure minus tole-client's subtree has none),
#               and tole-cli and tole-tui are in it nowhere.
#             - every toolkit dependency of tole-desktop is optional, exact
#               (`=X.Y.Z`), from crates.io and a normal dependency, and a
#               toolkit package resolved from anywhere else (a [patch], a path)
#               is refused.
#             - a default build (no features) reaches no gpui* or zed* package.
#             - secondary, by text: in the tole-desktop crate (every .rs file,
#               tests and build scripts included) only src/ui/adapter.rs names
#               gpui*, and `#[path]` and `include!` are not used.
# size      At most 500 lines per .rs file and 4,000 per crate (owner rule D1,
#           2026-11-20: ideal 1,500 to 2,500, hard ceiling 4,000) in each desktop
#           crate. Other crates are only reported, as warnings: the cap is for
#           the desktop alone. Every check walks a crate the same way: paths
#           relative to the crate, pruning only the crate's own top-level
#           `target/`, following symlinked .rs files, refusing a dangling link
#           or a symlinked directory, and refusing a crate with no src/ or no
#           .rs file (so a check can never pass over zero files).
#
# ADVISORY (warns, never fails the gate):
#
# literals  Under tole-desktop/src/ui: numeric colours and sizes (`rgb(0x..)`,
#           `0xRRGGBB`, `px(..)`, `len(8.0)`), and string literals handed to
#           text (`.child("..")`, `.label("..")`, `String::from("..")`,
#           `"..".into()`, `.font_family("..")`). Colours and sizes belong in
#           tole-desktop-model's tokens, words in its strings. It is a pattern
#           match over comment-stripped source and CANNOT see: a literal first
#           bound to a `let`, `const` or `static` and used later, a literal made
#           by a macro or a helper, a number reaching a size by arithmetic on a
#           non-token constant, or text assembled at run time. Review reads the
#           diff; do not read a clean run as proof.
#
# FAIL CLOSED: anything that stops a check from finishing (python3 or cargo
# missing, cargo failing, an unreadable or non-UTF-8 file, a dangling symlink,
# a crate with no source, an exception in the helper) makes this script exit 2 with a message.
# It never prints `ok` for a check that did not run to the end.
#
# Exit: 0 no enforced violation, 1 an enforced rule is broken (each is printed
# as `error: path:line: message`), 2 the check could not run or usage error.
set -uo pipefail

mode="${1:-all}"
here="$(cd "$(dirname "$0")" && pwd)"
root="${2:-$(cd "$here/.." && pwd)}"
fails=0

fail() { echo "error: $*"; fails=$((fails + 1)); }

# lint <deps|size|literals>: run the Python rules; exit 2 unless they finished.
lint() {
  local out rc=0 err line
  command -v python3 > /dev/null 2>&1 || { echo "error: python3 is needed by the desktop guardrails and was not found"; exit 2; }
  err=$(mktemp)
  out=$(python3 -I "$here/desktop-lint.py" "$1" "$root" 2> "$err") || rc=$?
  if [ "$rc" -gt 1 ] || { [ "$rc" = 1 ] && ! grep -q '^error: ' <<< "$out"; }; then
    echo "error: the desktop $1 check could not finish (exit $rc); the gate is NOT passed:"
    sed 's/^/  /' "$err"
    rm -f "$err"
    exit 2
  fi
  rm -f "$err"
  while IFS= read -r line; do
    case "$line" in
      "error: "*) fail "${line#error: }" ;;
      "warning: "*|"info: "*) echo "${line#info: }" ;;
    esac
  done <<< "$out"
}

case "$mode" in
  deps) lint deps ;;
  size) lint size ;;
  literals) lint literals ;;
  all) lint deps; lint size; lint literals ;;
  *) echo "usage: $0 [deps|size|literals|all] [workspace-root]" >&2; exit 2 ;;
esac

if [ "$fails" -gt 0 ]; then
  echo "desktop-check ($mode): $fails violation(s)"
  exit 1
fi
case "$mode" in
  literals) echo "desktop-check (literals): finished (advisory only, no enforced rule ran)" ;;
  all) echo "desktop-check (all): ok (enforced rules; literals are advisory)" ;;
  *) echo "desktop-check ($mode): ok (enforced rules)" ;;
esac
