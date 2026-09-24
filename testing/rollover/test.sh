#!/usr/bin/env bash
# Buying continues across a channel boundary.
#
# Channels are sized to be exhausted in a couple of seconds, so the test runs
# through several. Before per-channel cumulative this looped funding a channel
# per tick and then stopped buying for good.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the first purchase" 90 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

first_channel="$(tollgate::snapshot client | jq -r '.peers[0].outgoing_channel.id')"

# Long enough to run through several channels' worth of capacity.
tollgate::wait_for "the client to move onto a replacement channel" 90 \
  '[[ -n "$(tollgate::snapshot client | jq -r ".peers[0].outgoing_channel.id")" ]] &&
   [[ "$(tollgate::snapshot client | jq -r ".peers[0].outgoing_channel.id")" != "'"$first_channel"'" ]]'

# Still buying, and still shaped to it — the point is that nothing stalled.
# A wait rather than an instantaneous read: a snapshot taken between a grant
# lapsing and the next one landing would show the allowance and say nothing
# about whether the rollover worked.
tollgate::wait_for "the rate to still be what was bought, after the rollover" 30 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

# And the peer never dropped out of its session while channels were changing.
# The gateway logs None once when the peer connects, so only a None logged
# after it reached Active is a session ending.
tollgate::logs --no-log-prefix gateway \
  | awk '/access=Active/ { active = 1 } active && /access=None/ { lapsed = 1 } END { exit !lapsed }' \
  && tollgate::fail "the session lapsed mid-rollover; the replacement was not ready in time"

echo "  ok: the session never lapsed while rolling over"

echo "PASS: rollover"
