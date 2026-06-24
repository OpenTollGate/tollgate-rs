#!/usr/bin/env bash
# Integration test: the client's `consume` loop keeps itself online by topping up
# before the balance runs out, driven by the gateway's MeteringReports.
#
# Asserts:
#   1. the client received MeteringReports and tracked remaining balance
#      (CONSUME poll … remaining=…)
#   2. the client auto-topped-up proactively (CONSUME topup … cut_off=false)
#   3. the gateway granted access and NEVER suspended the peer — the top-ups beat
#      exhaustion (no Suspended access decision)
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

echo "Running client consume loop (this takes ~30s)..."
$COMPOSE up --no-build --exit-code-from client client || true

strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }
fail() { echo "FAIL: $1" >&2; exit 1; }

CLIENT_LOG="$($COMPOSE logs --no-color client 2>/dev/null | strip_ansi)"
GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null | strip_ansi)"

echo "----- client log -----"; echo "$CLIENT_LOG"
echo "----- gateway log -----"; echo "$GATEWAY_LOG"
echo "----------------------"

# 1. The client tracked its remaining balance from MeteringReports.
echo "$CLIENT_LOG" | grep -qE 'CONSUME poll=[0-9]+ .*remaining=' \
    || fail "client did not report remaining balance from a MeteringReport"

# 2. The client proactively topped up (before being cut off).
echo "$CLIENT_LOG" | grep -qE 'CONSUME topup .*cut_off=false' \
    || fail "client did not auto-top-up proactively"

# 3. The gateway granted access and never suspended the peer.
echo "$GATEWAY_LOG" | grep -qE 'access decision.*allowed=true' \
    || fail "gateway never granted access"
if echo "$GATEWAY_LOG" | grep -qE 'access decision.*level=Suspended'; then
    fail "gateway suspended the peer — top-ups did not beat exhaustion"
fi

echo "PASS: metering loop tops up before exhaustion; peer stayed Active"
