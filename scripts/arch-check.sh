#!/usr/bin/env bash
# Architecture guardrails of the CodeCora modular + thin-UI standard (#1249).
#
# Usage: scripts/arch-check.sh [kinds|all] [workspace-root]
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
fails=0

fail() { echo "error: $*"; fails=$((fails + 1)); }

# lint <kinds>: run the Python rules; exit 2 unless they finished.
lint() {
  local out rc=0 err line
  command -v python3 > /dev/null 2>&1 || { echo "error: python3 is needed by the architecture guardrails and was not found"; exit 2; }
  err=$(mktemp)
  out=$(python3 -I "$here/arch-lint.py" "$1" "$root" 2> "$err") || rc=$?
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

case "$mode" in
  kinds) lint kinds ;;
  all) lint kinds ;;
  *) echo "usage: $0 [kinds|all] [workspace-root]" >&2; exit 2 ;;
esac

if [ "$fails" -gt 0 ]; then
  echo "arch-check ($mode): $fails violation(s)"
  exit 1
fi
echo "arch-check ($mode): ok (enforced rules)"
