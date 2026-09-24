#!/usr/bin/env bash
# An oversized purchase is refused, and the payer re-buys at the rate named.
#
# The gateway will sell at most 3 MB/s across all its peers together, and the
# client is at first the only one; the client wants 8. It should end up at
# exactly the cap rather than at nothing, and within a round trip — declining
# to ratchet already leaves the payer's money untouched, so the refusal exists
# to carry a rate back. A second client then shows the cap is shared.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the client to settle at the gateway's cap" 90 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "3000000" ]]'

# It should be shaped at the cap, not blocked and not at the allowance.
access="$(tollgate::peer_field gateway access)"
[[ "$access" == "active" ]] || tollgate::fail "expected active at the cap, got $access"

# The refusal should be visible to the operator on both sides — a peer pinned
# at a limit with nothing saying why is the thing this logging exists for.
tollgate::logs gateway | grep -q "refused a peer's purchase" \
  || tollgate::fail "the gateway did not log refusing the purchase"
tollgate::logs client | grep -q "a peer refused our purchase" \
  || tollgate::fail "the client did not log being refused"

# The cap is node-wide, not per buyer. A second client arrives wanting 2 MB/s
# while the first holds all 3: it should be refused, and the gateway's total
# should stay at the cap. Were the cap per peer it would sell both, about
# 5.5 MB/s together.
export COMPOSE_PROFILES=second-buyer
SERVICES="gateway client client2"
docker compose -f "$COMPOSE" up -d client2

tollgate::wait_for "the second client to be refused" 90 \
  'tollgate::logs client2 | grep -q "a peer refused our purchase"'

# Each peer is shaped to at least the 4096 B/s allowance, so a buyer refused
# everything still shows 4096. Sample for several windows, since grants renew
# every second and the two buyers take turns at what is free.
cap=$(( 3000000 + 2 * 4096 ))
for _ in $(seq 10); do
  total="$(tollgate::snapshot gateway | jq '[.peers[].shaped_rate] | add // 0')"
  (( total <= cap )) \
    || tollgate::fail "gateway shapes its buyers to $total B/s together, over the 3 MB/s cap"
  sleep 1
done

echo "PASS: refusal"
