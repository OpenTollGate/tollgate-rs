#!/usr/bin/env bash
# Integration test: a child pays a bootstrap token and the parent grants access.
#
# Asserts:
#   1. the client prints PAID accepted=true (the gateway accepted the token)
#   2. the gateway logs "bootstrap token verified" (it checked with the mint)
#   3. the gateway logs an access grant for the client
#
# Usage: ./test.sh            (builds image, runs topology, asserts, cleans up)
#        SKIP_BUILD=1 ./test.sh   (reuse an existing tollgate-test:latest)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE="docker compose -f $SCRIPT_DIR/docker-compose.yml"

cleanup() {
    $COMPOSE down -t 2 >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    "$TESTING_DIR/scripts/build.sh"
fi

echo "Bringing up mint + gateway..."
$COMPOSE up -d --no-build gateway

echo "Running client payment..."
$COMPOSE up --no-build --exit-code-from client client || true

CLIENT_LOG="$($COMPOSE logs --no-color client 2>/dev/null)"
GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null)"

echo "----- client log -----"; echo "$CLIENT_LOG"
echo "----- gateway log -----"; echo "$GATEWAY_LOG"
echo "----------------------"

fail() { echo "FAIL: $1" >&2; exit 1; }

# 1. Client's bootstrap was accepted.
echo "$CLIENT_LOG" | grep -q 'PAID .*accepted=true' \
    || fail "client did not report PAID accepted=true"

# 2. Gateway verified the token with the mint.
echo "$GATEWAY_LOG" | grep -q "bootstrap token verified" \
    || fail "gateway did not log a verified bootstrap token"

# 3. Gateway granted access for the client (backend-independent decision log).
echo "$GATEWAY_LOG" | grep -q "access decision.*allowed=true" \
    || fail "gateway did not log an access grant (allowed=true)"

echo "PASS: bootstrap payment accepted and access granted"
