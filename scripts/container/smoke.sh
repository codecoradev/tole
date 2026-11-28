#!/usr/bin/env bash
# Smoke test of a built tole container image (issue #1015). Run by
# .github/workflows/container.yml before an image is pushed, and by hand:
#
#   scripts/container/smoke.sh tole-serve:local [EXPECTED_VERSION]
#
# IMAGE may be a tag or a digest reference (name@sha256:...).
# Checks: the binary runs (and reports EXPECTED_VERSION when given), the image
# runs as uid 10001, and `tole serve` started the way the compose recipe starts it
# (token from a secret file, the entrypoint reading the provider key file) reaches
# `healthy` through the image's own HEALTHCHECK. No model is called: the provider
# URL points at a closed port. Needs a Docker daemon; exit 0 on success.
set -euo pipefail

image="${1:?usage: smoke.sh IMAGE [EXPECTED_VERSION]}"
want_version="${2:-}"
name="tole-smoke-$$"
dir="$(mktemp -d)"
cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "$dir"
}
trap cleanup EXIT

echo "== tole --version"
version_line="$(docker run --rm "$image" tole --version)"
echo "$version_line"
if [ -n "$want_version" ]; then
    # `tole <version>` as whole words: 1.2.3 must not match 1.2.30 or 11.2.3.
    esc="$(printf '%s' "$want_version" | sed 's/[][\\.*^$+?(){}|/]/\\&/g')"
    if ! grep -Eq "(^|[[:space:]])tole ${esc}([[:space:]]|\$)" <<<"$version_line"; then
        echo "smoke: expected 'tole $want_version' in '$version_line'" >&2
        exit 1
    fi
fi

echo "== user"
uid="$(docker run --rm --entrypoint id "$image" -u)"
[ "$uid" = "10001" ] || { echo "smoke: runs as uid $uid, expected 10001" >&2; exit 1; }

echo "== tole serve reaches healthy"
# Mode 0444: the container user (10001) is not the owner of a file made here.
umask 022
printf '%s' "smoke-token-$$-$RANDOM$RANDOM" > "$dir/token"
printf '%s' "smoke-key" > "$dir/key"
chmod 0444 "$dir/token" "$dir/key"
docker run -d --name "$name" \
    -e TOLE_API_KEY_FILE=/run/secrets/tole_api_key \
    -e TOLE_BASE_URL=http://127.0.0.1:9/v1 \
    -e TOLE_MODEL=smoke \
    -v "$dir/token:/run/secrets/tole_serve_token:ro" \
    -v "$dir/key:/run/secrets/tole_api_key:ro" \
    "$image" >/dev/null
status=""
for _ in $(seq 1 60); do
    status="$(docker inspect --format '{{if .State.Running}}{{.State.Health.Status}}{{else}}exited{{end}}' "$name")"
    [ "$status" = "healthy" ] && break
    [ "$status" = "exited" ] && break
    sleep 2
done
echo "health: $status"
if [ "$status" != "healthy" ]; then
    docker logs "$name" 2>&1 | tail -n 40 >&2
    exit 1
fi
echo "smoke: ok"
