#!/usr/bin/env bash
# Run Cora on your own branch before opening a PR (see AGENTS.md section 3 and
# CONTRIBUTING.md, "Local review before a PR").
#
# Reviews `git diff <base>...HEAD`, split into chunks so each stays small
# (large single diffs are what make the CI Cora Review job slow or time out),
# and prints only the findings.
#
# Usage: scripts/review-local.sh [options]
#   -b, --base REF     base ref to diff against (default: origin/develop,
#                      else develop)
#   -m, --max-lines N  split a group that has more than N changed lines
#                      (default: 600)
#   -s, --severity S   minimum severity: info|minor|major|critical
#                      (default: minor)
#   -n, --dry-run      print the chunk plan and exit, no review
#   -h, --help         show this help
#
# Exit codes: 0 no findings, 2 findings reported, 1 error (no cora binary, no
# diff base, cora error in a chunk). Like the CI job, this only moves feedback
# earlier: the CI Cora Review verdict stays the merge gate.
set -euo pipefail

base=""
max_lines=600
severity="minor"
dry_run=0

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

while [ $# -gt 0 ]; do
  case "$1" in
    -b|--base) base="${2:?--base needs a ref}"; shift 2 ;;
    -m|--max-lines) max_lines="${2:?--max-lines needs a number}"; shift 2 ;;
    -s|--severity) severity="${2:?--severity needs a level}"; shift 2 ;;
    -n|--dry-run) dry_run=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "review-local: unknown option: $1" >&2; usage >&2; exit 1 ;;
  esac
done

case "$max_lines" in ''|*[!0-9]*) echo "review-local: --max-lines must be a number" >&2; exit 1 ;; esac
[ "$max_lines" -gt 0 ] || { echo "review-local: --max-lines must be above 0" >&2; exit 1; }

git rev-parse --git-dir >/dev/null 2>&1 || { echo "review-local: not inside a git repository" >&2; exit 1; }
cd "$(git rev-parse --show-toplevel)"

if [ "$dry_run" -eq 0 ] && ! command -v cora >/dev/null 2>&1; then
  echo "review-local: the cora binary is not on PATH (see CONTRIBUTING.md, Cora Workflow)" >&2
  exit 1
fi

if [ -z "$base" ]; then
  for candidate in origin/develop develop; do
    if git rev-parse --verify --quiet "$candidate^{commit}" >/dev/null; then base="$candidate"; break; fi
  done
fi
[ -n "$base" ] || { echo "review-local: no base ref found; pass --base" >&2; exit 1; }
git rev-parse --verify --quiet "$base^{commit}" >/dev/null || { echo "review-local: unknown base ref: $base" >&2; exit 1; }

range="$base...HEAD"
files=()
while IFS= read -r f; do files+=("$f"); done < <(git diff --name-only "$range")
if [ "${#files[@]}" -eq 0 ]; then
  echo "review-local: no changes in $range"
  exit 0
fi

# Group files: crates/<name>, <top-level dir>, or the root-level files together.
group_of() {
  case "$1" in
    crates/*/*) printf '%s\n' "$(printf '%s' "$1" | cut -d/ -f1-2)" ;;
    */*) printf '%s\n' "${1%%/*}" ;;
    *) printf '.\n' ;;
  esac
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# No associative array (bash 3.2, the macOS default, has none): one
# "group<TAB>path" line per file, then the paths of a group are read back.
group_lines="$tmp/groups.tsv"
for f in "${files[@]}"; do
  printf '%s\t%s\n' "$(group_of "$f")" "$f"
done >"$group_lines"
group_paths() { awk -F'\t' -v g="$1" '$1 == g { sub(/^[^\t]*\t/, ""); print }' "$group_lines"; }

# A chunk is a list of paths, written one per line to $tmp/chunk-N.paths.
n=0
add_chunk() { n=$((n + 1)); printf '%s' "$1" >"$tmp/chunk-$n.paths"; }

changed_lines() { # paths on stdin
  tr '\n' '\0' | xargs -0 -r git diff --numstat "$range" -- 2>/dev/null |
    awk '{ if ($1 != "-") s += $1 + $2 } END { print s + 0 }'
}

while IFS= read -r g; do
  list="$(group_paths "$g")"$'\n'
  total="$(printf '%s' "$list" | changed_lines)"
  if [ "$total" -le "$max_lines" ]; then
    add_chunk "$list"
    continue
  fi
  # Too big: pack the group's files into chunks of about max_lines each.
  cur=""; cur_lines=0
  while IFS=$'\t' read -r a d path; do
    [ -n "$path" ] || continue
    [ "$a" = "-" ] && { a=0; d=0; }
    size=$((a + d))
    if [ -n "$cur" ] && [ $((cur_lines + size)) -gt "$max_lines" ]; then
      add_chunk "$cur"; cur=""; cur_lines=0
    fi
    cur+="$path"$'\n'; cur_lines=$((cur_lines + size))
  done < <(printf '%s' "$list" | tr '\n' '\0' | xargs -0 -r git diff --numstat "$range" --)
  [ -z "$cur" ] || add_chunk "$cur"
done < <(cut -f1 "$group_lines" | sort -u)

echo "review-local: $range, ${#files[@]} files, $n chunk(s), max $max_lines lines per chunk"

status=0
deterministic=0
for i in $(seq 1 "$n"); do
  count="$(wc -l <"$tmp/chunk-$i.paths" | tr -d ' ')"
  lines="$(changed_lines <"$tmp/chunk-$i.paths")"
  echo "--- chunk $i/$n: $count file(s), $lines changed lines"
  if [ "$dry_run" -eq 1 ]; then sed 's/^/    /' "$tmp/chunk-$i.paths"; continue; fi

  tr '\n' '\0' <"$tmp/chunk-$i.paths" | xargs -0 -r git diff "$range" -- >"$tmp/chunk-$i.diff"
  set +e
  cora review --diff-file "$tmp/chunk-$i.diff" --format compact --no-color \
    --severity "$severity" >"$tmp/chunk-$i.out" 2>"$tmp/chunk-$i.err"
  rc=$?
  set -e
  if grep -q 'No LLM configured' "$tmp/chunk-$i.err"; then deterministic=1; fi
  case "$rc" in
    0) if [ -s "$tmp/chunk-$i.out" ]; then cat "$tmp/chunk-$i.out"; else echo "no findings"; fi ;;
    2) cat "$tmp/chunk-$i.out"; [ "$status" -eq 1 ] || status=2 ;;
    *) echo "cora error (exit $rc):"; cat "$tmp/chunk-$i.err" "$tmp/chunk-$i.out" 2>/dev/null | head -20
       status=1 ;;
  esac
done

if [ "$deterministic" -eq 1 ]; then
  echo "review-local: WARNING no LLM is configured, so only deterministic checks ran." >&2
  echo "review-local: run 'cora auth login' or review through 'cora mcp' (agent as reviewer); this is not a full review." >&2
fi

case "$status" in
  0) echo "review-local: no findings" ;;
  2) echo "review-local: findings above; fix real ones, state the result in the PR body" ;;
  *) echo "review-local: at least one chunk failed to review; this is not a pass" >&2 ;;
esac
exit "$status"
