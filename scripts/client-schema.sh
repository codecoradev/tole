#!/usr/bin/env bash
# The JSON Schema of the tole-client wire and event types (#1068).
#
# Usage: scripts/client-schema.sh check|update
#   check   regenerate the schema in memory and fail when
#           crates/tole-client/schema/tole-client.schema.json differs; also
#           fail when the generator (schemars) reached a normal build of
#           tole-client, which would put it in front of a published crate.
#   update  rewrite the committed file from the types; commit the result.
#
# The generator is a unit test of tole-client (src/schema.rs) behind
# cfg(test), with schemars as a dev-dependency only.
set -euo pipefail
cd "$(dirname "$0")/.."

case "${1:-}" in
  check)
    if cargo tree -p tole-client -e normal --prefix none | grep -q '^schemars '; then
      echo "schemars is a normal dependency of tole-client; it must stay a dev-dependency" >&2
      exit 1
    fi
    cargo test -p tole-client --lib schema::
    ;;
  update)
    TOLE_UPDATE_SCHEMA=1 cargo test -p tole-client --lib schema::schema_matches_the_committed_file
    git --no-pager diff --stat -- crates/tole-client/schema || true
    ;;
  *)
    echo "usage: $0 check|update" >&2
    exit 2
    ;;
esac
