#!/usr/bin/env bash
# Two nodes find each other, fund channels in both directions, and reach Active.
#
# Nothing is bought in the first part — it is only the opening sequence, which
# has to work before any of the payment tests mean anything. The second part
# holds a payer that does not charge its provider back past the stale timeout,
# which is what the keepalive is for.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

# Funding a channel means the client acquires the gateway's vouchers from the
# gateway's mint first, so this waits on a mint coming up as well as a peering.
tollgate::wait_for "the gateway to see the client paying" 90 \
  '[[ "$(tollgate::peer_field gateway access)" == "active" ]]'

tollgate::wait_for "the client to see the gateway paying" 90 \
  '[[ "$(tollgate::peer_field client access)" == "active" ]]'

# Two channels is the default, not an exception: each side pays for what it
# received, so both owe and both fund.
[[ "$(tollgate::peer_field gateway incoming_channels | wc -l)" -ge 1 ]] \
  || tollgate::fail "the gateway has no channel the client pays it on"
[[ -n "$(tollgate::peer_field client outgoing_channel)" ]] \
  || tollgate::fail "the client has no channel to pay the gateway on"

# The two sides should name the same channel: the client's outgoing channel is
# the gateway's incoming one, which is the two ratchets agreeing.
client_out="$(tollgate::snapshot client | jq -r '.peers[0].outgoing_channel.id')"
gateway_in="$(tollgate::snapshot gateway | jq -r '.peers[0].incoming_channels[0].id')"
[[ "$client_out" == "$gateway_in" ]] \
  || tollgate::fail "channel mismatch: client pays on $client_out, gateway sees $gateway_in"

echo "  ok: the two sides name the same channel"

# A payer that does not charge its provider back — what proxyd's per-device
# sessions are. The gateway funds nothing toward it and buys nothing from it, and TopUps are
# never answered, so after the opening sequence only the gateway's keepalive
# says it is still there. Without one the client dropped a healthy session at
# the stale timeout (60 s) and forfeited the grant it had just paid for.
echo "--- a payer that does not charge back, past the stale timeout"
tollgate::down
COMPOSE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/no-charge.yml"
export COMPOSE
tollgate::up

tollgate::wait_for "the gateway to see the client paying" 90 \
  '[[ "$(tollgate::peer_field gateway access)" == "active" ]]'
tollgate::wait_for "the client to see the gateway as free" 30 \
  '[[ "$(tollgate::peer_field client access)" == "free" ]]'
tollgate::wait_for "the client to be buying" 30 \
  '[[ "$(tollgate::peer_field client bought_rate)" -gt 0 ]]'
paying_on="$(tollgate::snapshot gateway | jq -r '.peers[0].incoming_channels[0].id')"

# Past one stale timeout with margin, then look at what happened in between.
sleep 75

# Still one session, on the channel it opened with: a drop and a reconnect
# would each have logged, and a session started from nothing funds afresh.
if tollgate::logs --no-log-prefix client gateway | grep -q "dropping peer"; then
  tollgate::fail "a peer was dropped; the quiet side's keepalive did not reach it"
fi
[[ "$(tollgate::peer_field client access)" == "free" ]] \
  || tollgate::fail "the client no longer holds the gateway"
[[ "$(tollgate::peer_field gateway access)" == "active" ]] \
  || tollgate::fail "the gateway no longer sees the client paying"
[[ "$(tollgate::snapshot gateway | jq -r '.peers[0].incoming_channels[0].id')" == "$paying_on" ]] \
  || tollgate::fail "the client is paying on a new channel: the session started over"
[[ "$(tollgate::peer_field client bought_rate)" -gt 0 ]] \
  || tollgate::fail "the client stopped buying"

echo "  ok: held past the stale timeout on one connection"

echo "PASS: peering"
