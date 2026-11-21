#!/usr/bin/env bash
# Offscreen render harness of the desktop (#1028): draws the real window
# without showing it and writes one PNG per screen and state.
#
# Usage: scripts/desktop-render.sh <output-dir> [--size WxH]
#        scripts/desktop-render.sh --test
#
#   <output-dir>  where the PNG files and index.tsv go (created if missing).
#   --size WxH    window size in logical pixels (default: the window's own,
#                 1200x780; the design boards are 1440x900). The images are at
#                 2x, the fixed scale factor of GPUI's test platform.
#   --test        runs the harness checks of crates/tole-desktop/tests/render.rs
#                 (PNG written, size, tokens on screen, Tab and Shift+Tab
#                 order) instead of writing to a directory you name.
#
# WHY A SCRIPT. GPUI's `Window::render_to_image` and its test platform need the
# `test-support` feature of `gpui-pre` and `gpui-pre-platform`. That feature
# must not reach the committed manifest or Cargo.lock (the exact pins and the
# off-by-default `gpui` feature are guarded by `scripts/desktop-check.sh deps`).
# So this script never edits the repository. It copies the two desktop crates,
# `tole-client` and `tole-core` (the desktop depends on the client, which
# depends on the core) and a trimmed workspace manifest into a temporary
# directory, adds the two test-support dev-dependencies there (the same two
# packages, the same exact versions), builds the `render` example with the
# `render-harness` feature and removes the copy on exit. `git status` is the
# same before and after.
#
# Linux first: GPUI's test platform needs no display server and no window; the
# frame is drawn by wgpu (Vulkan, a software rasteriser such as lavapipe is
# enough) into a texture. If no adapter is found, run under a virtual display
# with `xvfb-run -a scripts/desktop-render.sh ...`. Build packages: see
# docs/design/desktop/README.md. The first build compiles about 500 crates.
#
# Environment: CARGO_TARGET_DIR (default <repo>/target/desktop-render, kept
# apart from your normal build because the feature set differs), CARGO.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
cargo_bin="${CARGO:-cargo}"
usage() { sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; }

mode=render
out=""
size_args=()
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --test) mode=test ;;
    --size) [ $# -ge 2 ] || { echo "error: --size needs WxH" >&2; exit 2; }; size_args=(--size "$2"); shift ;;
    -*) echo "error: unknown option $1" >&2; usage >&2; exit 2 ;;
    *) [ -z "$out" ] || { echo "error: one output directory only" >&2; exit 2; }; out="$1" ;;
  esac
  shift
done
if [ "$mode" = render ] && [ -z "$out" ]; then
  echo "error: name an output directory" >&2; usage >&2; exit 2
fi
command -v "$cargo_bin" > /dev/null 2>&1 || { echo "error: cargo not found" >&2; exit 2; }
if [ "$mode" = render ]; then
  mkdir -p "$out"
  out="$(cd "$out" && pwd)"  # absolute, because cargo runs elsewhere
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/crates"
for crate in tole-desktop-model tole-desktop tole-client tole-core; do
  cp -R "$root/crates/$crate" "$work/crates/$crate"
  rm -rf "$work/crates/$crate/target"
done

# A trimmed workspace: the two desktop crates, the typed client and tole-core
# (the desktop's one tole dependency, and its closure), and the workspace
# package and dependency tables they inherit from.
{
  printf '[workspace]\nresolver = "2"\nmembers = ["crates/tole-desktop-model", "crates/tole-desktop", "crates/tole-client", "crates/tole-core"]\n\n'
  sed -n '/^\[workspace\.package\]/,/^$/p' "$root/Cargo.toml"
  sed -n '/^\[workspace\.dependencies\]/,/^$/p' "$root/Cargo.toml"
} > "$work/Cargo.toml"
# The committed lockfile keeps every existing version; cargo adds only what
# test-support itself needs, in this copy.
cp "$root/Cargo.lock" "$work/Cargo.lock"

# The local dev-dependencies of the issue: same packages, same exact pins, plus
# test-support. Taken from the committed pins so an upgrade needs no change here.
pin() { sed -n "s/^$1 = .*version = \"\\(=[0-9.]*\\)\".*/\\1/p" "$root/crates/tole-desktop/Cargo.toml"; }
gpui_pin="$(pin gpui)"
platform_pin="$(pin gpui_platform)"
if [ -z "$gpui_pin" ] || [ -z "$platform_pin" ]; then
  echo "error: could not read the exact gpui pins from crates/tole-desktop/Cargo.toml" >&2
  exit 2
fi
cat >> "$work/crates/tole-desktop/Cargo.toml" <<TOML

# Added by scripts/desktop-render.sh in this temporary copy only.
[dev-dependencies]
gpui = { package = "gpui-pre", version = "$gpui_pin", features = ["test-support"] }
gpui_platform = { package = "gpui-pre-platform", version = "$platform_pin", features = ["test-support"] }
TOML

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/target/desktop-render}"
manifest="$work/Cargo.toml"
features=(--features render-harness -p tole-desktop)

if [ "$mode" = test ]; then
  "$cargo_bin" test --manifest-path "$manifest" "${features[@]}" --lib --test render -- --test-threads=1
else
  "$cargo_bin" run --manifest-path "$manifest" "${features[@]}" --example render -- "$out" ${size_args[@]+"${size_args[@]}"}
  echo "wrote the images and index.tsv to $out (2x; compare each with its board in docs/design/desktop, see index.tsv)"
fi
