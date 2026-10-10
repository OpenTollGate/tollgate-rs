#!/usr/bin/env bash
# Buying continues across a channel boundary, and each boundary costs one
# replacement channel however slow funding is.
#
# Channels are sized to be exhausted in a couple of seconds, so the test runs
# through several. Before per-channel cumulative this looped funding a channel
# per tick and then stopped buying for good. Later, with funding a mint round
# trip many ticks long, each rollover funded three or four replacements, one
# per tick until the first came back.
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
# A wait rather than an instantaneous read: a snapshot taken as a budget runs
# out and before the next purchase lands would show the allowance and say
# nothing about whether the rollover worked.
tollgate::wait_for "the rate to still be what was bought, after the rollover" 30 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

# And the peer never dropped out of its session while channels were changing.
# The gateway logs None once when the peer connects, so only a None logged
# after it reached Active is a session ending.
tollgate::logs --no-log-prefix gateway \
  | awk '/access=Active/ { active = 1 } active && /access=None/ { lapsed = 1 } END { exit !lapsed }' \
  && tollgate::fail "the session lapsed mid-rollover; the replacement was not ready in time"

echo "  ok: the session never lapsed while rolling over"

# Then a slow funder. Funding is a round of HTTP requests to the gateway's
# mint and market on 3338; from here every packet the client sends there waits
# 100 ms, so a funding takes seconds — dozens of 100 ms ticks. Nothing else is
# slowed: TopUps go to 4747. Funding that slow outruns channels this small, so
# the session may lapse between channels here; what is asserted is the count.
docker compose -f "$COMPOSE" exec -T client sh -c '
  tc qdisc add dev eth0 root handle 1: prio &&
  tc qdisc add dev eth0 parent 1:3 handle 30: netem delay 100ms &&
  tc filter add dev eth0 parent 1:0 protocol ip prio 1 u32 \
    match ip dport 3338 0xffff flowid 1:3' \
  || tollgate::fail "could not slow the client's funding down"

# One replacement per rollover. The client logs every channel it funds, and
# the gateway every channel it settles once the client has drained it, so the
# two counts differ by the channel in use and at most one replacement on the
# way. Funded one per tick while the wallet worked, the first count ran at
# three or four times the second.
settled() { tollgate::logs --no-log-prefix gateway | grep -c "settling a channel" || true; }
funded() { tollgate::logs --no-log-prefix client | grep -c "asking for vouchers to fund with" || true; }
before="$(settled)"
tollgate::wait_for "three more channels drained and settled, funded slowly" 120 \
  '(( $(settled) >= before + 3 ))'
f="$(funded)"
s="$(settled)"
(( f <= s + 2 )) \
  || tollgate::fail "the client funded $f channels for $s rollovers; a rollover funds one"
echo "  ok: $f channels funded, $s drained and settled"

# And no funding that came back late was left unused beside the one asked for
# in its place.
if tollgate::logs --no-log-prefix client | grep -q "no longer wanted"; then
  tollgate::fail "a channel was funded and then not wanted"
fi

echo "PASS: rollover"
