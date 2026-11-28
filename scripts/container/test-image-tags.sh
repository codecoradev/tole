#!/usr/bin/env bash
# Self-test of image-tags.sh (issue #1015): the tag set of each channel, the
# forward-only floating tags, the absence of `latest`, and the refusals.
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
tags="$here/image-tags.sh"
fail=0
SHA=0123456789abcdef0123456789abcdef01234567
ALL=$'v0.9.0\nv0.10.0\nv0.10.1\nv0.11.0\nv0.12.0\nv0.12.0-rc.1\nv1.0.0-rc.1\nnot-a-version'

# expect NAME "expected tags joined by space" VAR=value...
expect() {
    local name="$1" want="$2"; shift 2
    local got
    got=$(env -i PATH="$PATH" "$@" "$tags" 2>/dev/null | tr '\n' ' ' | sed 's/ $//')
    if [ "$got" = "$want" ]; then echo "ok   $name"; else echo "FAIL $name: want '$want' got '$got'"; fail=1; fi
}
# refuse NAME VAR=value...  (exit code must be 2 and nothing on stdout)
refuse() {
    local name="$1"; shift
    local out rc
    out=$(env -i PATH="$PATH" "$@" "$tags" 2>/dev/null); rc=$?
    if [ "$rc" -eq 2 ] && [ -z "$out" ]; then echo "ok   $name"; else echo "FAIL $name: rc=$rc out='$out'"; fail=1; fi
}

expect "nightly" "nightly nightly-20261128 sha-0123456" CHANNEL=nightly SHA=$SHA DATE=20261128
expect "release newest" "0.12.0 sha-0123456 0.12 stable" CHANNEL=release SHA=$SHA GIT_TAG=v0.12.0 RELEASE_TAGS="$ALL"
expect "release first of a new minor" "0.13.0 sha-0123456 0.13 stable" CHANNEL=release SHA=$SHA GIT_TAG=v0.13.0 RELEASE_TAGS="$ALL"
expect "release patch of the newest minor" "0.12.1 sha-0123456 0.12 stable" CHANNEL=release SHA=$SHA GIT_TAG=v0.12.1 RELEASE_TAGS="$ALL"
expect "re-run of an old patch moves nothing" "0.10.0 sha-0123456" CHANNEL=release SHA=$SHA GIT_TAG=v0.10.0 RELEASE_TAGS="$ALL"
expect "backport patch moves X.Y not stable" "0.10.2 sha-0123456 0.10" CHANNEL=release SHA=$SHA GIT_TAG=v0.10.2 RELEASE_TAGS="$ALL"
expect "tag list without the tag itself" "0.1.0 sha-0123456 0.1 stable" CHANNEL=release SHA=$SHA GIT_TAG=v0.1.0 RELEASE_TAGS=""
expect "numeric not lexical order" "0.10.1 sha-0123456 0.10" CHANNEL=release SHA=$SHA GIT_TAG=v0.10.1 RELEASE_TAGS="$ALL"
expect "prerelease gets the exact tag only" "1.0.0-rc.2 sha-0123456" CHANNEL=release SHA=$SHA GIT_TAG=v1.0.0-rc.2 RELEASE_TAGS="$ALL"

refuse "no channel" SHA=$SHA
refuse "unknown channel" CHANNEL=latest SHA=$SHA
refuse "bad sha" CHANNEL=nightly SHA=xyz DATE=20261128
refuse "missing sha" CHANNEL=nightly DATE=20261128
refuse "bad date" CHANNEL=nightly SHA=$SHA DATE=2026-11-28
refuse "release without tag" CHANNEL=release SHA=$SHA
refuse "tag without v" CHANNEL=release SHA=$SHA GIT_TAG=0.12.0
refuse "tag with two parts" CHANNEL=release SHA=$SHA GIT_TAG=v0.12
refuse "branch name as tag" CHANNEL=release SHA=$SHA GIT_TAG=develop

# No channel ever prints a bare latest.
for args in "CHANNEL=nightly DATE=20261128" "CHANNEL=release GIT_TAG=v0.13.0"; do
    # shellcheck disable=SC2086
    if env -i PATH="$PATH" SHA=$SHA RELEASE_TAGS="$ALL" $args "$tags" | grep -qx latest; then
        echo "FAIL latest printed for $args"; fail=1
    fi
done
[ "$fail" -eq 0 ] && echo "all image-tags checks passed"
exit "$fail"
