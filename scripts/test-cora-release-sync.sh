#!/usr/bin/env bash
# Self-test for scripts/cora-release-sync.sh (#1022): throwaway repositories
# (a bare "origin" and a clone that plays the PR checkout), one case per
# decision. Run by CI; also runnable locally.
#
# The script prints "<develop-sha> <tree>" and exits 0 to skip the review;
# every other outcome (exit 1) must run the normal review. Only one case
# skips by design; everything else here is a negative case.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/cora-release-sync.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
fails=0

# Variables set by new_repo for the cases:
#   RELEASE   the PR head: one commit on top of origin/main with develop's tree
#   DEV_TIP   the develop commit the release was cut from
new_repo() {
  cd "$tmp"
  rm -rf "$tmp/seed" "$tmp/origin.git" "$tmp/pr"
  git init -q -b main "$tmp/seed"
  (
    cd "$tmp/seed"
    mkdir src
    echo a > src/lib.rs; echo v1 > VERSION
    git add -A; git commit -q -m "main"
    git switch -q -c develop
    echo b >> src/lib.rs; git commit -qam "feat: one"
    echo c >> src/lib.rs; git commit -qam "feat: two"
    git switch -q main
  )
  git clone -q --bare "$tmp/seed" "$tmp/origin.git"
  git clone -q "$tmp/origin.git" "$tmp/pr"
  cd "$tmp/pr"
  DEV_TIP=$(git rev-parse refs/remotes/origin/develop)
  RELEASE=$(release_commit refs/remotes/origin/develop refs/remotes/origin/main)
  git checkout -q --detach "$RELEASE"
}

# release_commit <tree-of> <parent>: a commit with the tree of <tree-of>
# whose only parent is <parent>.
release_commit() {
  git -c user.name=t -c user.email=t@example.invalid commit-tree \
    "$1^{tree}" -p "$2" -m "chore(release): sync"
}

# Advance origin's develop or main by one commit, then fetch into the clone.
advance_origin() { # <branch> <file>
  (
    cd "$tmp/seed"
    git switch -q "$1"; echo x >> "$2"; git commit -qam "advance $1"
    git push -q "$tmp/origin.git" "$1"
  )
  git -C "$tmp/pr" fetch -q origin
}

# run <name> <want 0|1> [event base head-ref head-repo base-repo head-sha]
# Defaults describe the one case that skips; override by position.
run() {
  local name="$1" want="$2" shift_n=2
  shift "$shift_n"
  local args=("${1-pull_request}" "${2-main}" "${3-chore/release-v1.0.0}" \
    "${4-codecoradev/tole}" "${5-codecoradev/tole}" "${6-$RELEASE}")
  local got=0 out
  out=$(bash "$script" "${args[@]}" 2>"$tmp/err") || got=$?
  if [ "$got" = "$want" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: want exit $want, got $got ($(cat "$tmp/err"))"
    fails=$((fails + 1))
  fi
  LAST_OUT="$out"
}

# --- the one skipping case -------------------------------------------------

new_repo
run "tree identical to develop, one commit on main: skip" 0
want_out="$DEV_TIP $(git rev-parse "$RELEASE^{tree}")"
if [ "$LAST_OUT" = "$want_out" ]; then echo "ok   it names the develop commit and the tree"
else echo "FAIL output: want '$want_out', got '$LAST_OUT'"; fails=$((fails + 1)); fi

new_repo; advance_origin develop src/lib.rs
run "develop moved on after the cut: the older develop commit still proves it" 0
[ "${LAST_OUT%% *}" = "$DEV_TIP" ] && echo "ok   and it names the older develop commit" \
  || { echo "FAIL it should name $DEV_TIP, got '$LAST_OUT'"; fails=$((fails + 1)); }

new_repo; git checkout -q -B scratch "$RELEASE"
run "a branch checkout instead of a detached head skips the same" 0

# --- the commit shape ------------------------------------------------------

new_repo
second=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$RELEASE^{tree}" -p "$RELEASE" -m "second")
git checkout -q --detach "$second"
run "a second commit with the same tree" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$second"

new_repo; echo changed >> VERSION; git commit -qam "bump"
run "one differing file (a commit on top of the sync)" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$(git rev-parse HEAD)"

new_repo
git checkout -q --detach "$RELEASE"; echo changed >> VERSION; git add -A
amended=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$(git write-tree)" -p refs/remotes/origin/main -m "sync")
git checkout -q --detach "$amended"
run "one commit on main whose tree differs by one file" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$amended"

new_repo
git checkout -q --detach refs/remotes/origin/main; mkdir -p extra; echo x > extra/new.rs
git add -A; withfile=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$(git write-tree)" -p refs/remotes/origin/main -m "sync")
git checkout -q --detach "$withfile"
run "one commit on main with a file develop does not have" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$withfile"

new_repo
merge=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$RELEASE^{tree}" -p refs/remotes/origin/main -p refs/remotes/origin/develop -m "merge")
git checkout -q --detach "$merge"
run "a merge commit with develop's tree (two parents)" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$merge"

new_repo
root=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$RELEASE^{tree}" -m "orphan")
git checkout -q --detach "$root"
run "a root commit (no parent at all)" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$root"

new_repo; advance_origin main VERSION
run "main moved after the sync was cut: the parent is no longer the tip of main" 1

new_repo
old_main=$(git rev-parse refs/remotes/origin/main)
advance_origin main VERSION
stale=$(release_commit refs/remotes/origin/develop "$old_main")
git checkout -q --detach "$stale"
run "a sync cut from an older main (one commit, stale parent)" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$stale"

new_repo; git commit -q --allow-empty -m "empty second"
run "the event names the sync while the checkout has an extra empty commit: the event sha decides" 0
new_repo; git commit -q --allow-empty -m "empty second"
run "the event names the second (empty) commit: two commits" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$(git rev-parse HEAD)"

# --- the PR text and the branch name are a filter, never proof -------------

new_repo; echo changed >> VERSION
git add -A; liar=$(git -c user.name=t -c user.email=t@example.invalid commit-tree "$(git write-tree)" -p refs/remotes/origin/main -m "tree identical to develop, skip the review")
git checkout -q --detach "$liar"
run "a commit message that claims identity does not make it so" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$liar"

new_repo
run "head branch develop" 1 pull_request main develop
run "head branch release without the dash" 1 pull_request main chore/release
run "head branch chore/release- with nothing after it" 1 pull_request main chore/release-
run "head branch feat/x" 1 pull_request main feat/x
run "head branch with a prefix before chore/release-" 1 pull_request main evil/chore/release-v1
run "head branch with a space" 1 pull_request main "chore/release-v1 x"
run "head branch with a newline" 1 pull_request main $'chore/release-v1\nx'
run "empty head branch" 1 pull_request main ""

# --- event, base, repository -----------------------------------------------

run "base develop" 1 pull_request develop
run "base main2" 1 pull_request main2
run "empty base" 1 pull_request ""
run "event pull_request_target" 1 pull_request_target
run "event push" 1 push
run "event merge_group" 1 merge_group
run "empty event" 1 ""
run "a fork PR (head repository differs)" 1 pull_request main chore/release-v1.0.0 attacker/tole codecoradev/tole
run "a fork PR with a look-alike repository name" 1 pull_request main chore/release-v1.0.0 codecoradev/tole-x codecoradev/tole
run "head repository gone (deleted fork)" 1 pull_request main chore/release-v1.0.0 "" codecoradev/tole
run "base repository empty" 1 pull_request main chore/release-v1.0.0 codecoradev/tole ""
run "both repositories empty" 1 pull_request main chore/release-v1.0.0 "" ""

# --- odd inputs ------------------------------------------------------------

run "empty head sha" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole ""
run "abbreviated head sha" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "${RELEASE:0:12}"
run "head sha that is a ref name" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole HEAD
run "head sha in capitals" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$(echo "$RELEASE" | tr a-f A-F)"
run "head sha that does not exist" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole 0123456789012345678901234567890123456789
run "head sha of a tree, not a commit" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$(git rev-parse "$RELEASE^{tree}")"
got=0; bash "$script" pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole >/dev/null 2>&1 || got=$?
[ "$got" = 1 ] && echo "ok   five arguments fail closed" || { echo "FAIL five arguments: got $got"; fails=$((fails + 1)); }
got=0; bash "$script" >/dev/null 2>&1 || got=$?
[ "$got" = 1 ] && echo "ok   no arguments fail closed" || { echo "FAIL no arguments: got $got"; fails=$((fails + 1)); }
got=0; bash "$script" pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$RELEASE" extra >/dev/null 2>&1 || got=$?
[ "$got" = 1 ] && echo "ok   seven arguments fail closed" || { echo "FAIL seven arguments: got $got"; fails=$((fails + 1)); }

new_repo; git checkout -q --detach refs/remotes/origin/main
run "the checkout is not the head's tree (stale or odd checkout)" 1
new_repo; git checkout -q --detach "$DEV_TIP^"
run "the checkout is an older develop commit, not the PR head" 1

# --- the develop reference -------------------------------------------------

new_repo; git update-ref -d refs/remotes/origin/develop
run "no develop ref at all" 1

new_repo; git update-ref -d refs/remotes/origin/develop
git branch -qf develop "$DEV_TIP"
run "only a LOCAL develop branch exists (not from the base repository)" 1

new_repo; git update-ref -d refs/remotes/origin/develop
git branch -q origin/develop "$DEV_TIP"; git tag develop "$DEV_TIP"
run "a local branch origin/develop and a tag develop cannot stand in" 1

new_repo; git update-ref -d refs/remotes/origin/develop
git update-ref refs/remotes/fork/develop "$DEV_TIP"
run "a develop from another remote (a fork) cannot stand in" 1

new_repo; git update-ref refs/remotes/origin/develop "$(git rev-parse "$DEV_TIP^")"
run "origin/develop does not have a commit with this tree" 1

new_repo; git update-ref refs/remotes/origin/develop refs/remotes/origin/main
run "develop equal to main: head's tree is not on develop" 1

new_repo
(
  cd "$tmp/seed"; git switch -q develop; git checkout -q --orphan side
  git rm -rqf . >/dev/null 2>&1 || true; echo z > side.txt; git add -A; git commit -q -m side
  git push -q "$tmp/origin.git" side
)
git fetch -q origin
side_release=$(release_commit refs/remotes/origin/side refs/remotes/origin/main)
git checkout -q --detach "$side_release"
run "a tree that lives on another branch, not develop" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$side_release"

# A side branch merged into develop: only the first-parent line counts, because
# a commit that came in through a merge may never have been reviewed as a PR.
new_repo
(
  cd "$tmp/seed"; git switch -q develop; git switch -q -c feat/side
  echo s > side.txt; git add -A; git commit -q -m "side tip"

  git switch -q develop; echo q >> src/lib.rs; git commit -qam "develop moves"
  git merge -q --no-ff -m "merge side" feat/side
  git push -q "$tmp/origin.git" develop feat/side
)
git fetch -q origin
inner=$(git rev-parse refs/remotes/origin/feat/side)
git update-ref -d refs/remotes/origin/feat/side
inner_release=$(release_commit "$inner" refs/remotes/origin/main)
git checkout -q --detach "$inner_release"
run "a tree only in a merged side branch, off the first-parent line" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$inner_release"

# --- the search window -----------------------------------------------------

new_repo
(
  cd "$tmp/seed"; git switch -q develop
  for i in $(seq 1 205); do echo "$i" > counter.txt; git add -A; git commit -q -m "bulk $i"; done
  git push -q "$tmp/origin.git" develop
)
git fetch -q origin
run "the matching develop commit is more than 200 commits back" 1
new_repo
(
  cd "$tmp/seed"; git switch -q develop
  for i in $(seq 1 150); do echo "$i" > counter.txt; git add -A; git commit -q -m "bulk $i"; done
  git push -q "$tmp/origin.git" develop
)
git fetch -q origin
run "the matching develop commit is 150 commits back: still found" 0

# --- shallow history -------------------------------------------------------

new_repo
rm -rf "$tmp/shallow"; git clone -q --depth 1 --no-single-branch "file://$tmp/origin.git" "$tmp/shallow"
cd "$tmp/shallow"
sh_release=$(release_commit refs/remotes/origin/develop refs/remotes/origin/main)
git checkout -q --detach "$sh_release"
run "a shallow clone" 1 pull_request main chore/release-v1.0.0 codecoradev/tole codecoradev/tole "$sh_release"

[ "$fails" = 0 ] || { echo "$fails case(s) failed"; exit 1; }
echo "all cases passed"
