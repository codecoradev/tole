#!/usr/bin/env bash
# Architecture guardrails of the CodeCora modular + thin-UI standard (#1249).
#
# Usage: scripts/arch-check.sh [kinds|size|unsafe|cora|all] [workspace-root] [--init]
#   workspace-root defaults to the repository root (the self-test points it at
#   fixture workspaces). Needs bash, cargo and Python 3.8+. Nothing is
#   downloaded: `cargo metadata --no-deps --offline`.
#
# ENFORCED (a violation makes this script exit 1; CI runs it in the `Check`
# job, so no new required check name exists):
#
# kinds     (#1250, standard rules L1 to L3)
#             - every workspace crate declares `[package.metadata.tole] kind`,
#               one of core, engine, integration, os, feature, app, ffi, ui,
#               feature-ui, bin, tool;
#             - a normal dependency between workspace crates follows the
#               matrix: core on core; engine and integration on core and
#               engine; os on core; feature and app on core, engine,
#               integration, os and feature; ffi on core and app; ui and
#               feature-ui on core, app and ui; bin and tool on everything;
#             - a UI toolkit package (crossterm, ratatui, gpui*, zed*) is a
#               normal dependency of ui, feature-ui and bin crates only.
#             Build and dev dependencies are not checked (test and build
#             wiring). The module layering inside tole-core and tole-cli is a
#             test (#752, `cargo test -p tole-core --test layering`); this
#             check is the crate level above it. The desktop crates keep their
#             own stricter script, scripts/desktop-check.sh.
#
# size      (#1251, standard rule D1)
#             - every .rs file of a crate's src/, tests/, examples/ and benches/
#               and its build.rs has at most 500 lines, and the src/ of a crate
#               at most 50,000 lines (the cap the desktop crates already have);
#             - today's violations are in scripts/arch-size-allow.txt and that
#               list can only go down: a file or crate over its cap and not
#               listed fails; an entry that is smaller or larger than the
#               measured size fails (stale, or grown past its entry); an entry
#               within its cap fails (remove it);
#             - `--init` writes the first list and refuses to run when it
#               exists. No mode adds an entry: split the file instead.
#             The desktop crates keep their own check, which this one does not
#             weaken.
#
# unsafe    (#1252, standard rule D4)
#             - the root Cargo.toml sets `[workspace.lints.rust] unsafe_code =
#               "deny"` and every workspace crate has `[lints] workspace = true`,
#               so the compiler rejects an `unsafe` block, fn, impl or
#               attribute that is not in an opt-in;
#             - an opt-in is `#[allow(unsafe_code)]` (or `#![allow(...)]`, also
#               `expect` and `cfg_attr`) on a named module or function, with a
#               `SAFETY:` comment on it or within 3 lines of it;
#             - the files that hold an opt-in are exactly the files listed in
#               AGENTS.md between the `unsafe-opt-ins:start` and
#               `unsafe-opt-ins:end` marker comments: a file with an opt-in that
#               is not listed fails, and so does a listed file without one.
#             To add an opt-in: put the allow and its SAFETY comment on the
#             module or function that wraps the FFI, and add the file to the
#             AGENTS.md list in the same change.
#
# cora      (#1249)
#             - a .cora.yaml `ignore.rules` entry that silences
#               `index-breaking-change` lists exact `crates/<name>/src/*.rs`
#               files that exist, are reached from a bin target through `mod`
#               declarations and are NOT reached from the crate's lib target
#               (`pub mod serve;` in lib.rs would hide real API removals).
#               Entries are read by a strict line parser: what it does not
#               understand fails, an unresolvable `mod` is exit 2.
#
# FAIL CLOSED: anything that stops a check from finishing (python3 or cargo
# missing, cargo failing, bad metadata, an exception in the helper) makes this
# script exit 2 with a message. It never prints `ok` for a check that did not
# run to the end.
#
# Exit: 0 no enforced violation, 1 an enforced rule is broken (each is printed
# as `error: path:line: message`), 2 the check could not run or usage error.
set -uo pipefail

mode="${1:-all}"
here="$(cd "$(dirname "$0")" && pwd)"
root="${2:-$(cd "$here/.." && pwd)}"
init=""
if [ "${3:-}" = "--init" ]; then init="--init"; elif [ -n "${3:-}" ]; then echo "usage: $0 [kinds|size|unsafe|cora|all] [workspace-root] [--init]" >&2; exit 2; fi
fails=0

fail() { echo "error: $*"; fails=$((fails + 1)); }

# lint <kinds|size|unsafe|cora> [--init]: run the Python rules; exit 2 unless they finished.
lint() {
  local out rc=0 err line
  command -v python3 > /dev/null 2>&1 || { echo "error: python3 is needed by the architecture guardrails and was not found"; exit 2; }
  err=$(mktemp)
  out=$(python3 -I "$here/arch-lint.py" "$1" "$root" ${2:+"$2"} 2> "$err") || rc=$?
  if [ "$rc" -gt 1 ] || { [ "$rc" = 1 ] && ! grep -q '^error: ' <<< "$out"; }; then
    echo "error: the architecture $1 check could not finish (exit $rc); the gate is NOT passed:"
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

if [ -n "$init" ] && [ "$mode" != "size" ]; then echo "usage: --init is for the size mode only" >&2; exit 2; fi
case "$mode" in
  kinds) lint kinds ;;
  size) lint size "$init" ;;
  unsafe) lint unsafe ;;
  cora) lint cora ;;
  all) lint kinds; lint size; lint unsafe; lint cora ;;
  *) echo "usage: $0 [kinds|size|unsafe|cora|all] [workspace-root] [--init]" >&2; exit 2 ;;
esac

if [ "$fails" -gt 0 ]; then
  echo "arch-check ($mode): $fails violation(s)"
  exit 1
fi
echo "arch-check ($mode): ok (enforced rules)"
