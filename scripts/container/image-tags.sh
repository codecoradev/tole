#!/usr/bin/env bash
# Tag names of the tole container image for one build (issue #1015). Pure: reads
# the environment, prints one tag name per line (no registry prefix), touches no
# network. The workflow .github/workflows/container.yml calls it; the self-test is
# scripts/container/test-image-tags.sh. The channels are documented in
# docs/container.md ("Channels and tags").
#
#   CHANNEL       nightly | release
#   SHA           full commit id of the build (any length >= 7)
#   DATE          nightly only: YYYYMMDD (UTC) of the build
#   GIT_TAG       release only: the pushed tag, vX.Y.Z or vX.Y.Z-<prerelease>
#   RELEASE_TAGS  release only: the release tags that are on main (the caller passes
#                 `git tag --merged origin/main`), one per line. A tag that is not
#                 on main, or is not a plain vX.Y.Z, never counts as "highest", so a
#                 stray tag cannot freeze `stable`; the floating tags only move
#                 forward, so a re-run of an old tag never takes `stable` back.
#
# nightly -> nightly, nightly-YYYYMMDD, sha-<7>
# release -> X.Y.Z, sha-<7>, and for a final release (no prerelease suffix) also
#            X.Y when no higher X.Y.* final tag exists and `stable` when no higher
#            final tag exists at all.
# There is deliberately no `latest`.
#
# Versions: each of X, Y and Z is 0 or has no leading zero, at most 6 digits; the
# prerelease suffix (with its dash) is at most 40 characters. Anything else is
# refused here, before a digest is pushed.
#
# Exit codes: 0 tags printed, 2 bad or missing input.
set -euo pipefail

die() { echo "image-tags: $*" >&2; exit 2; }

channel="${CHANNEL:-}"
sha="${SHA:-}"
[[ "$sha" =~ ^[0-9a-f]{7,64}$ ]] || die "SHA must be a lowercase hex commit id, got '${sha}'"
short="${sha:0:7}"

case "$channel" in
nightly)
    date="${DATE:-}"
    [[ "$date" =~ ^[0-9]{8}$ ]] || die "DATE must be YYYYMMDD, got '${date}'"
    printf '%s\n' "nightly" "nightly-${date}" "sha-${short}"
    ;;
release)
    git_tag="${GIT_TAG:-}"
    num='(0|[1-9][0-9]{0,5})'
    re="^v${num}\\.${num}\\.${num}(-[0-9A-Za-z][0-9A-Za-z.-]*)?\$"
    [[ "$git_tag" =~ $re ]] || die "GIT_TAG must be vX.Y.Z or vX.Y.Z-<prerelease> (no leading zeros, at most 6 digits each), got '${git_tag}'"
    major="${BASH_REMATCH[1]}"
    minor="${BASH_REMATCH[2]}"
    pre="${BASH_REMATCH[4]:-}"
    [ "${#pre}" -le 40 ] || die "the prerelease suffix of '${git_tag}' is longer than 40 characters"
    version="${git_tag#v}"
    printf '%s\n' "$version" "sha-${short}"
    if [ -z "$pre" ]; then
        # Final tags of the repository, this one included, as bare versions.
        finals=$( { printf '%s\n' "$git_tag"; printf '%s\n' "${RELEASE_TAGS:-}"; } \
            | grep -E "^v${num}\\.${num}\\.${num}\$" | sed 's/^v//' | sort -uV)
        top_all=$(printf '%s\n' "$finals" | tail -n 1)
        top_minor=$(printf '%s\n' "$finals" | grep -E "^${major}\.${minor}\." | tail -n 1)
        [ "$top_minor" = "$version" ] && printf '%s\n' "${major}.${minor}"
        [ "$top_all" = "$version" ] && printf '%s\n' "stable"
    fi
    ;;
*)
    die "CHANNEL must be nightly or release, got '${channel}'"
    ;;
esac
exit 0
