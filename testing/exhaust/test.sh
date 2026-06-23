#!/usr/bin/env bash
# Integration test: a time-priced gateway meters a paid balance to zero and
# suspends the peer.
#
# Asserts:
#   1. the client's bootstrap is accepted (PAID accepted=true)
#   2. the gateway grants access (access decision allowed=true)
#   3. the gateway then suspends the peer when the balance is exhausted
#      (access decision level=Suspended allowed=false) — the metering path that
#      emits a balance-exhausted Reject + SetAccess(Suspended).
#
# Metering runs server-side on a 5s interval, so suspension happens ~10-15s after
# payment; the test polls the gateway log until it appears (or times out).
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

# Strip ANSI escapes before matching, so colored tracing output can't break greps.
strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }
fail() { echo "FAIL: $1" >&2; exit 1; }

CLIENT_LOG="$($COMPOSE logs --no-color client 2>/dev/null | strip_ansi)"
echo "----- client log -----"; echo "$CLIENT_LOG"

# 1. Client's bootstrap was accepted.
echo "$CLIENT_LOG" | grep -q 'PAID .*accepted=true' \
    || fail "client did not report PAID accepted=true"

# 2 + 3. The gateway grants access, then suspends on exhaustion. Poll until the
# suspend decision shows up (metering is on a 5s server-side interval).
echo "Waiting for the gateway to meter the balance to exhaustion..."
granted=""
suspended=""
for _ in $(seq 1 30); do
    GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null | strip_ansi)"
    echo "$GATEWAY_LOG" | grep -qE 'access decision.*allowed=true' && granted=1
    if echo "$GATEWAY_LOG" | grep -qE 'access decision.*level=Suspended.*allowed=false'; then
        suspended=1
        break
    fi
    sleep 1
done

GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null | strip_ansi)"
echo "----- gateway log -----"; echo "$GATEWAY_LOG"
echo "----------------------"

[ -n "$granted" ] || fail "gateway never granted access (no allowed=true decision)"
[ -n "$suspended" ] || fail "gateway did not suspend the peer on exhaustion within timeout"

# Enforcement: suspension must actually remove the client IP from the nftables
# paid-peers set, not merely log the decision. Poll briefly — the suspend log
# line is emitted just before the nft delete runs.
client_ip="$(echo "$GATEWAY_LOG" | grep -E 'access decision.*allowed=true' \
    | grep -oE '([0-9]{1,3}\.){3}[0-9]{1,3}' | head -1)"
[ -n "$client_ip" ] || fail "could not extract a granted client IP from the gateway log"

removed=""
for _ in $(seq 1 5); do
    set_v4="$($COMPOSE exec -T gateway nft list set inet tollgate paid_peers_v4 2>/dev/null || true)"
    if ! echo "$set_v4" | grep -qF "$client_ip"; then removed=1; break; fi
    sleep 1
done
echo "----- nft paid_peers_v4 (post-suspend) -----"; echo "${set_v4:-<empty>}"
[ -n "$removed" ] || fail "client IP $client_ip still in paid_peers_v4 after suspension (deny not enforced)"

echo "PASS: access granted, enforced, then suspended and removed from nftables ($client_ip)"
