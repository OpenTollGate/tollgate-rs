#!/usr/bin/env bash
# Integration test: parent and child detect each other.
#
# Asserts:
#   1. the client probe prints a DETECTED line carrying the gateway's pubkey
#   2. the gateway logs "peer announced" carrying the client's pubkey
#   3. the two pubkeys differ (sanity: they are distinct nodes)
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

echo "Bringing up topology..."
$COMPOSE up -d --no-build gateway

# Run the client to completion (it probes once and exits).
echo "Running client probe..."
$COMPOSE up --no-build --exit-code-from client client || true

CLIENT_LOG="$($COMPOSE logs --no-color client 2>/dev/null)"
GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null)"

echo "----- client log -----"
echo "$CLIENT_LOG"
echo "----- gateway log -----"
echo "$GATEWAY_LOG"
echo "----------------------"

fail() { echo "FAIL: $1" >&2; exit 1; }

# 1. Client detected the gateway.
detected_line="$(echo "$CLIENT_LOG" | grep -o 'DETECTED peer=[0-9a-f]* unit=[^ ]* version=[0-9]*' | head -1 || true)"
[ -n "$detected_line" ] || fail "client did not print a DETECTED line"
gateway_pubkey="$(echo "$detected_line" | sed -n 's/.*peer=\([0-9a-f]*\).*/\1/p')"
[ ${#gateway_pubkey} -eq 66 ] || fail "gateway pubkey is not 33 bytes hex (got '${gateway_pubkey}')"

# 2. Gateway saw the client announce.
echo "$GATEWAY_LOG" | grep -q "peer announced" || fail "gateway did not log a peer announce"
client_pubkey="$(echo "$GATEWAY_LOG" | sed -n 's/.*peer announced.*peer=\([0-9a-f]*\).*/\1/p' | head -1)"
[ ${#client_pubkey} -eq 66 ] || fail "client pubkey not found in gateway log (got '${client_pubkey}')"

# 3. They are distinct nodes.
[ "$gateway_pubkey" != "$client_pubkey" ] || fail "gateway and client report the same pubkey"

echo "PASS: mutual detection"
echo "  gateway pubkey: $gateway_pubkey"
echo "  client  pubkey: $client_pubkey"
