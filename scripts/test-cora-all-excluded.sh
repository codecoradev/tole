#!/usr/bin/env bash
# Self-test for scripts/cora-all-excluded.sh (#914): a throwaway repository,
# one case per decision. Run by CI; also runnable locally.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/cora-all-excluded.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fails=0

new_repo() {
  rm -rf "$tmp/r"; mkdir -p "$tmp/r"; cd "$tmp/r"
  git init -q -b develop .
  git config user.email t@example.invalid; git config user.name t
  cat > .cora.yaml <<'YAML'
focus:
  - bugs
review:
  exclude:
    - "CHANGELOG.md"
    - "docs/**"   # prose
    - '*.lock'
    - "Cargo.lock"
hook:
  mode: warn
YAML
  mkdir -p docs/sub src
  echo a > docs/a.md; echo a > src/lib.rs; echo a > CHANGELOG.md
  # "nocfg": the base branch has no .cora.yaml at all.
  if [ "${1:-}" = nocfg ]; then rm .cora.yaml; fi
  git add -A; git commit -q -m base
  git switch -q -c work
}

# expect <name> <0|1>: run the script against develop and compare.
expect() {
  local name="$1" want="$2" got=0
  bash "$script" develop || got=$?
  if [ "$got" = "$want" ]; then echo "ok   $name"; else echo "FAIL $name: want $want, got $got"; fails=$((fails+1)); fi
}

new_repo; echo b >> docs/a.md; echo b >> docs/sub/n.md; echo c >> CHANGELOG.md
git add -A; git commit -q -m docs
expect "docs and changelog only is all excluded" 0

new_repo; echo b >> docs/a.md; echo b >> src/lib.rs
git add -A; git commit -q -m mixed
expect "one reviewable file means the review runs" 1

new_repo; echo b >> Cargo.lock; mkdir -p x; echo b >> x/y.lock
git add -A; git commit -q -m locks
expect "a lockfile change is never skipped" 1

new_repo; echo b >> docs/a.md; echo b >> Cargo.lock
git add -A; git commit -q -m docs-and-lock
expect "docs plus a lockfile is not skipped" 1

# The PR rewrites .cora.yaml to exclude EVERYTHING and changes code. Read from
# the working tree this would give 0; read from the base it gives 1.
new_repo; echo b >> src/lib.rs
printf 'review:\n  exclude:\n    - "*"\n' > .cora.yaml
git add -A; git commit -q -m widen
expect "a PR cannot widen the exclusion with its own .cora.yaml" 1

new_repo; git rm -q src/lib.rs; git commit -q -m del
expect "deleting a reviewable file is reviewable" 1

new_repo; git mv src/lib.rs docs/lib.rs; git commit -q -m mv
expect "moving code into docs is reviewable (both sides count)" 1

new_repo; echo b > "docs/café.md"; echo b > "docs/a b.md"; echo b > 'docs/q"x.md'
git add -A; git commit -q -m unicode
expect "non-ASCII and quoted docs paths are excluded" 0

new_repo; echo b >> docs/a.md; git commit -qam docs
git switch -q develop; echo c >> src/lib.rs; git commit -qam base-moves
git switch -q work; git merge -q --no-ff --no-edit develop
expect "a merge-commit HEAD counts only the PR's own files" 0

# Other `exclude:` keys must not be read as review.exclude.
new_repo; echo b >> src/lib.rs
printf 'ignore:\n  exclude:\n    - "src/**"\nreview:\n  exclude:\n    - "docs/**"\n' > .cora.yaml
git add -A; git commit -q -m other-key
expect "an exclude under another key is not review.exclude" 1

new_repo
expect "no changed file means the review runs" 1

new_repo nocfg; echo b >> docs/a.md; git add -A; git commit -q -m d
expect "no .cora.yaml at the base means the review runs" 1

new_repo; echo b >> docs/a.md; git add -A; git commit -q -m d
got=0; bash "$script" nosuchref || got=$?
if [ "$got" = 1 ]; then echo "ok   an unknown base ref fails closed"; else echo "FAIL unknown base ref: got $got"; fails=$((fails+1)); fi

[ "$fails" = 0 ] || { echo "$fails case(s) failed"; exit 1; }
echo "all cases passed"
