#!/usr/bin/env bash
# Integration test: a lying / malfunctioning peer that under-reports what it
# received is warned every metering interval and then cut off (suspended) after
# three consecutive over-tolerance intervals — the transit-loss escalation from
# docs/design/core/tollgate-metering.md.
#
# The client runs `consume --understate-received-pct 50` (it acknowledges only
# half of what the gateway delivers) and floods ping to generate real metered
# transit. The gateway meters what it actually delivered, sees the gap, and cuts
# the peer.
#
# Asserts (from the gateway log):
#   1. the gateway raised the metering-drift (transit-loss) warning repeatedly —
#      at least 3 times, the default cut-off threshold
#   2. the gateway then suspended the peer (a Suspended access decision)
#
# Usage: ./test.sh              (builds image, runs topology, asserts, cleans up)
#        SKIP_BUILD=1 ./test.sh (reuse an existing tollgate-test:latest)
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE="docker compose -f $SCRIPT_DIR/docker-compose.yml"

cleanup() { $COMPOSE down -t 2 >/dev/null 2>&1 || true; }
trap cleanup EXIT

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    "$TESTING_DIR/scripts/build.sh"
fi

echo "Bringing up mint + gateway + upstream..."
$COMPOSE up -d --no-build gateway upstream

echo "Running the lying client (consume + flood; ~18s)..."
$COMPOSE up --no-build --exit-code-from client client || true

strip_ansi() { sed $'s/\x1b\\[[0-9;]*m//g'; }
fail() { echo "FAIL: $1" >&2; exit 1; }

GATEWAY_LOG="$($COMPOSE logs --no-color gateway 2>/dev/null | strip_ansi)"
echo "----- gateway log -----"; echo "$GATEWAY_LOG"; echo "-----------------------"

# 1. The gateway raised the drift warning at least 3 times (the cut-off threshold).
WARNINGS="$(echo "$GATEWAY_LOG" | grep -c 'metering drift over tolerance' || true)"
echo "drift warnings observed: $WARNINGS"
[ "$WARNINGS" -ge 3 ] || fail "expected >=3 drift warnings, got $WARNINGS"

# 2. The gateway then suspended the lying peer (drift, not exhaustion — the warning
#    above is the transit-loss path, never emitted on balance exhaustion).
echo "$GATEWAY_LOG" | grep -qE 'access decision.*level=Suspended' \
    || fail "gateway did not suspend the lying peer"

echo "PASS: lying peer warned ${WARNINGS}× then suspended after persistent drift"
