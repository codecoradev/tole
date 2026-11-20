#!/usr/bin/env bash
# Is a PR's whole diff excluded from the Cora review? (#914)
#
# Cora (0.21+) drops the files matched by `review.exclude` in `.cora.yaml`
# from the reviewed diff. A PR whose files are ALL excluded (docs only, say)
# then has nothing left to review and Cora answers "Review could not
# complete. Cora produced an empty result", which AGENTS.md section 2 treats
# as a merge blocker. The CI job uses this script to tell "nothing to
# review" apart from a real failure and to skip the review step in that
# case only.
#
# Usage: scripts/cora-all-excluded.sh <base-ref>
#
# Exit codes: 0 every changed file matches `review.exclude` (nothing to
# review); 1 at least one file is reviewable, OR the script cannot tell. It
# fails closed: any doubt means the review runs. A line on stderr says why.
#
# A lockfile is never "nothing to review", although `review.exclude` lists
# lockfiles: a changed dependency or source in Cargo.lock is a supply-chain
# change that must not go green without a verdict, so a PR touching one
# keeps the behaviour it had before this script (the review runs).
#
# The patterns are read from `.cora.yaml` at <base-ref>, never from the
# working tree, so a PR cannot widen the exclusion to skip its own review.
# A pattern is a shell glob; `*` also crosses `/` (`docs/**` is a prefix,
# `*.lock` any lockfile), which is how Cora's own exclusion reads them for
# the patterns this repository uses.
set -euo pipefail

base="${1:?usage: cora-all-excluded.sh <base-ref>}"

why() { echo "cora-all-excluded: $*" >&2; exit 1; }

config=$(git show "$base:.cora.yaml") || why "no .cora.yaml at $base, so the review runs"

# The list under the top-level `review:` key's `exclude:` (indented exactly two
# spaces, items four), one pattern per line, quotes and trailing comments
# removed. Any other layout yields nothing, and nothing means the review runs.
patterns=$(printf '%s\n' "$config" | awk '
  /^[^[:space:]#]/ { in_review = ($0 ~ /^review:/); in_ex = 0; next }
  !in_review { next }
  /^[[:space:]]*#/ { next }
  /^  exclude:[[:space:]]*$/ { in_ex = 1; next }
  in_ex && /^    -[[:space:]]+/ {
    s = $0
    sub(/^    -[[:space:]]+/, "", s)
    sub(/[[:space:]]+#.*$/, "", s)
    gsub(/^["\047]|["\047]$/, "", s)
    if (s != "") print s
    next
  }
  in_ex { in_ex = 0 }
')
[ -n "$patterns" ] || why "no review.exclude patterns at $base, so the review runs"

# NUL-separated: git quotes a path with non-ASCII or special characters
# unless told not to, and a quoted path would never match a pattern.
mapfile -d '' -t files < <(git diff -z --name-only --no-renames "$base"...HEAD) \
  || why "cannot list the changed files against $base"
[ "${#files[@]}" -gt 0 ] || why "no changed file against $base, so the review runs"

for f in "${files[@]}"; do
  case "$f" in
    *.lock) why "$f is a lockfile: never skipped" ;;
  esac
  excluded=0
  while IFS= read -r p; do
    # shellcheck disable=SC2053 # an unquoted right side is the glob match
    if [[ $f == $p ]]; then excluded=1; break; fi
  done <<<"$patterns"
  [ "$excluded" = 1 ] || why "$f is not excluded from the review"
done
exit 0
