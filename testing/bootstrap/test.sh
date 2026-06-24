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

# Strip ANSI escape codes before matching, so colored tracing output (e.g. a
# reset sequence between a field key and its `=`) can't break literal greps.
strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }
CLIENT_LOG="$(echo "$CLIENT_LOG" | strip_ansi)"
GATEWAY_LOG="$(echo "$GATEWAY_LOG" | strip_ansi)"

# 1. Client's bootstrap was accepted.
echo "$CLIENT_LOG" | grep -q 'PAID .*accepted=true' \
    || fail "client did not report PAID accepted=true"

# 1b. Client discovered the gateway's PriceSheet (the configured per_unit=1
# rate over the accepted mint), proving price discovery works over the wire.
echo "$CLIENT_LOG" | grep -qE 'PRICESHEET .*mints=1 .*per_unit=1' \
    || fail "client did not receive the gateway PriceSheet (mints=1 per_unit=1)"

# 2. Gateway verified the token with the mint.
echo "$GATEWAY_LOG" | grep -q "bootstrap token verified" \
    || fail "gateway did not log a verified bootstrap token"

# 3. Gateway granted access for the client (backend-independent decision log).
echo "$GATEWAY_LOG" | grep -q "access decision.*allowed=true" \
    || fail "gateway did not log an access grant (allowed=true)"

# 4. Enforcement, not just the decision: the granted client IP is actually a
# member of the nftables paid-peers set the forward rules gate on.
client_ip="$(echo "$GATEWAY_LOG" | grep -E 'access decision.*allowed=true' \
    | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}' | head -1)"
[ -n "$client_ip" ] || fail "could not extract a granted client IP from the gateway log"

set_v4="$($COMPOSE exec -T gateway nft list set inet tollgate paid_peers_v4 2>/dev/null || true)"
echo "----- nft paid_peers_v4 -----"; echo "${set_v4:-<empty>}"
echo "$set_v4" | grep -qF "$client_ip" \
    || fail "granted client IP $client_ip is not in paid_peers_v4 (access not enforced)"

echo "PASS: bootstrap accepted, access granted, and IP enforced in nftables ($client_ip)"
