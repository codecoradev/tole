#!/bin/bash
# HEALTHCHECK of the tole container image: GET /health on the loopback of the
# container. bash's /dev/tcp is used so the image needs no curl. /health needs no
# credential; serve answers 200 when it is up and 503 while it drains.
set -eu
port="${TOLE_HEALTH_PORT:-7801}"
exec 3<>"/dev/tcp/127.0.0.1/${port}"
printf 'GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n' >&3
IFS= read -r -t 4 status <&3
case "$status" in
    "HTTP/1.1 200"*) exit 0 ;;
    *) exit 1 ;;
esac
