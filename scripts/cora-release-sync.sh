#!/usr/bin/env bash
# Is a PR a release sync whose tree is identical to develop? (#1022)
#
# The `Cora Review` job cannot cover a release sync PR (one squash commit
# whose tree equals develop's: hundreds of files, tens of thousands of lines,
# all of it already reviewed in the PRs that put it on develop). The CI job
# uses this script to tell that case apart from every other PR and to skip the
# review step for it only.
#
# Usage: scripts/cora-release-sync.sh <event> <base-ref> <head-ref> \
#          <head-repo> <base-repo> <head-sha>
#   event      github.event_name
#   base-ref   github.base_ref (the target branch name)
#   head-ref   github.head_ref (the PR's branch name; a FILTER, never proof)
#   head-repo  github.event.pull_request.head.repo.full_name (empty if gone)
#   base-repo  github.repository
#   head-sha   github.event.pull_request.head.sha
#
# Exit codes: 0 skip the review (stdout: "<develop-sha> <tree>"); 1 run the
# review, OR the script cannot tell. It fails closed: any missing argument,
# any mismatch and any doubt means the review runs. A line on stderr says why.
#
# Skip only when ALL hold:
#   1. the event is pull_request, the base is main and the head branch is
#      chore/release-<something>;
#   2. the PR is not from a fork (head repo == base repo, both non-empty);
#   3. the PR head is exactly one commit on top of main: its only parent is
#      the current tip of main and `main..head` is one commit, so the tree that
#      lands on main is the head tree for any merge method;
#   4. the tree of the PR head equals the tree of a commit on develop, found in
#      the first-parent history (the squash-merged PRs) of the develop ref that
#      `git fetch` made from the BASE repository, newest 200 commits.
# The proof is the two tree hashes, computed from git objects. The branch name
# and the repository names only narrow down; no PR text (title, body, commit
# message) is read. The checked-out tree must also equal the head tree, so a
# stale or odd checkout never skips.
#
# Only git plumbing runs here (rev-parse, rev-list, log with %H/%T): nothing
# from the PR checkout is executed, no diff driver, no hook, no filter. The
# develop ref is the fully qualified refs/remotes/origin/develop, so a local
# branch, a tag or a PR-supplied ref of a similar name cannot stand in for it.
set -euo pipefail

why() { echo "cora-release-sync: $*" >&2; exit 1; }

[ "$#" -eq 6 ] || why "usage: cora-release-sync.sh <event> <base-ref> <head-ref> <head-repo> <base-repo> <head-sha>"
event="$1" base_ref="$2" head_ref="$3" head_repo="$4" base_repo="$5" head_sha="$6"

window=200
main_ref=refs/remotes/origin/main
develop_ref=refs/remotes/origin/develop

# 1. event, base, head branch (a filter only).
[ "$event" = pull_request ] || why "not a pull_request event, so the review runs"
[ "$base_ref" = main ] || why "the base is not main, so the review runs"
[[ $head_ref =~ ^chore/release-[0-9A-Za-z._-]+$ ]] || why "the head branch is not chore/release-*, so the review runs"

# 2. not from a fork; both names present.
[ -n "$base_repo" ] && [ -n "$head_repo" ] || why "head or base repository unknown, so the review runs"
[ "$head_repo" = "$base_repo" ] || why "the PR is from another repository, so the review runs"

# The history must be complete for the counts below to mean anything.
[ "$(git rev-parse --is-shallow-repository)" = false ] || why "shallow clone, so the review runs"

# 3. head is exactly one commit on top of the tip of main.
[[ $head_sha =~ ^[0-9a-f]{40}$ ]] || why "the head sha is not a full sha, so the review runs"
head=$(git rev-parse --verify --quiet "$head_sha^{commit}") || why "the head commit is not in the repository, so the review runs"
[ "$head" = "$head_sha" ] || why "the head sha does not name a commit as given, so the review runs"
main_tip=$(git rev-parse --verify --quiet "$main_ref^{commit}") || why "no $main_ref, so the review runs"

# `rev-list --parents -n 1` prints "<commit> <parent>...": exactly two fields
# means exactly one parent, and it must be the tip of main.
line=$(git rev-list --parents -n 1 "$head") || why "cannot read the head commit, so the review runs"
read -r -a fields <<<"$line"
[ "${#fields[@]}" -eq 2 ] || why "the head commit does not have exactly one parent, so the review runs"
[ "${fields[1]}" = "$main_tip" ] || why "the head commit is not directly on top of the tip of main, so the review runs"
ahead=$(git rev-list --count "$main_tip..$head") || why "cannot count the commits on top of main, so the review runs"
[ "$ahead" = 1 ] || why "the PR is not exactly one commit on top of main, so the review runs"

# 4. the tree of the head equals the tree of a develop commit.
head_tree=$(git rev-parse --verify --quiet "$head^{tree}") || why "cannot read the head tree, so the review runs"
checkout_tree=$(git rev-parse --verify --quiet "HEAD^{tree}") || why "cannot read the checked-out tree, so the review runs"
[ "$checkout_tree" = "$head_tree" ] || why "the checkout is not the head's tree, so the review runs"
git rev-parse --verify --quiet "$develop_ref^{commit}" >/dev/null || why "no $develop_ref, so the review runs"

history=$(git log --first-parent -n "$window" --format='%H %T' "$develop_ref" --) \
  || why "cannot read the history of $develop_ref, so the review runs"
found=""
while read -r sha tree extra; do
  [ -z "${extra:-}" ] || why "unexpected log output, so the review runs"
  if [ "$tree" = "$head_tree" ]; then found="$sha"; break; fi
done <<<"$history"
[ -n "$found" ] || why "no develop commit has the head's tree, so the review runs"
[[ $found =~ ^[0-9a-f]{40}$ && $head_tree =~ ^[0-9a-f]{40}$ ]] || why "unexpected object names, so the review runs"

echo "$found $head_tree"
