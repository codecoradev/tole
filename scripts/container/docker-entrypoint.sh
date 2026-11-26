#!/bin/sh
# Entrypoint of the tole container image (issue #1044).
#
# tole reads the provider key from the environment only (TOLE_API_KEY), and a
# Docker secret is a file. This script bridges the two: for each NAME_FILE below
# that is set, it reads the file into NAME and then execs the command, so the
# value is in neither the image, the compose file, `docker inspect` nor any
# argument list. tole marks itself non-dumpable at start (#675) and scrubs
# secret-shaped variables from the environment of every command it spawns, so a
# command the model runs does not see the value in its own environment.
#
# The serve token is not handled here: `tole serve --token-file` reads it.
set -eu

# Session ledgers, cron store and logs are private to the tole user.
umask 077

read_secret() {
    # $1 = name of the variable, $2 = path of the secret file
    if [ ! -f "$2" ] || [ ! -r "$2" ]; then
        echo "docker-entrypoint: $1_FILE=$2 is not a readable file" >&2
        exit 64
    fi
    cat "$2"
}

if [ -n "${TOLE_API_KEY_FILE:-}" ]; then
    TOLE_API_KEY=$(read_secret TOLE_API_KEY "$TOLE_API_KEY_FILE")
    export TOLE_API_KEY
fi
if [ -n "${TOLE_FALLBACK_API_KEY_FILE:-}" ]; then
    TOLE_FALLBACK_API_KEY=$(read_secret TOLE_FALLBACK_API_KEY "$TOLE_FALLBACK_API_KEY_FILE")
    export TOLE_FALLBACK_API_KEY
fi
unset TOLE_API_KEY_FILE TOLE_FALLBACK_API_KEY_FILE

exec "$@"
