#!/usr/bin/env bash
# The gateway forwards the client's traffic, gates it with nftables, and shapes
# it with tc.
#
# This is the only topology where the bytes belong to somebody: the client pulls
# a large file from a third host, and every packet crosses the gateway. What is
# asserted is that the rate the client *bought* is the rate it *gets* — measured
# by timing a real download, not by reading a counter the node maintains itself.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
SERVICES="gateway client origin phone-node"
# The dual-stack customer's key, from phone.yaml's secret.
PHONE_PUBKEY="023836aac0e36dc2a6cb763d43c4b652c6dd51fe3cb2b4a26848dfe049b60cfb93"
# The gateway sells to two customers, so every question names which one.
client_field() { tollgate::peer_field_of gateway "$CLIENT_PUBKEY" "$1"; }
phone_field() { tollgate::peer_field_of gateway "$PHONE_PUBKEY" "$1"; }
# The kernel side of the gateway, for when a check here fails: which interface
# holds which address, and what nftables and tc actually hold.
forwarding_diagnose() {
  local g=(docker compose -f "$COMPOSE" exec -T gateway)
  "${g[@]}" uname -r
  "${g[@]}" ip -br addr
  "${g[@]}" ip route
  "${g[@]}" cat /run/gateway.yaml | grep "interface:"
  "${g[@]}" nft list tables
  "${g[@]}" nft list table inet tollgate
  "${g[@]}" ip -6 neigh
  "${g[@]}" ip neigh
  "${g[@]}" tc qdisc show
  "${g[@]}" sh -c 'for i in $(ls /sys/class/net | grep "^eth"); do echo "== tc class $i"; tc class show dev "$i"; done'
  docker compose -f "$COMPOSE" exec -T client ip route
  docker compose -f "$COMPOSE" logs --no-color gateway 2>&1 | grep -iE "warn|error|nft|tc " | tail -20
}
DIAGNOSE=forwarding_diagnose
tollgate::start

tollgate::wait_for "the client to be paying" 120 \
  '[[ "$(client_field access)" == "active" ]]'

# 2 MB/s of demand at 125% headroom.
tollgate::wait_for "the gateway to shape the client to what it bought" 60 \
  '[[ "$(client_field shaped_rate)" == "2500000" ]]'

# The gateway shapes on the interface facing the client. Docker picks the
# kernel name, so the gateway looks it up by address at startup; check that the
# one it wrote into its config really is the client's side.
edge="$(docker compose -f "$COMPOSE" exec -T gateway awk '/^  interface:/ {print $2}' /run/gateway.yaml)"
[[ -n "$edge" ]] || tollgate::fail "the gateway's config names no interface"
docker compose -f "$COMPOSE" exec -T gateway ip -br addr show "$edge" | grep -q "172.28.0.20" \
  || tollgate::fail "$edge is not the interface facing the client"

# Every packet has to cross the gateway, or the measurement below would be of
# a path that was never shaped.
docker compose -f "$COMPOSE" exec -T client ip route get 172.29.0.10 | grep -q "via 172.28.0.20" \
  || tollgate::fail "the client is not routing to the origin through the gateway"

# The kernel is really doing the work: rules and a class exist for this peer.
docker compose -f "$COMPOSE" exec -T gateway nft list table inet tollgate >/dev/null 2>&1 \
  || tollgate::fail "the gateway installed no nftables table"
docker compose -f "$COMPOSE" exec -T gateway tc class show dev "$edge" | grep -q htb \
  || tollgate::fail "the gateway installed no tc class"

# Time a real download through the gateway, and check what actually arrived.
#
# Elapsed time alone is not evidence: a transfer that fails instantly and one
# that is shaped can take a similar wall-clock time for entirely different
# reasons, so the byte count is what makes this a measurement rather than a
# coincidence. An earlier version of this test passed while curl was moving
# zero bytes.
echo "  measuring a real download through the gateway ..."
#
# The blob is far larger than anything that will arrive in the time allowed, so
# the transfer is cut off by the deadline rather than by running out of file.
# That is what makes this a rate measurement: the numerator is fixed and the
# bytes are whatever the shaper let through. (`curl` reports `200` rather than
# `206` because the origin serves the whole file; it does not honour Range.)
DURATION=20
read -r code bytes speed <<<"$(docker compose -f "$COMPOSE" exec -T client curl -s \
  --max-time "$DURATION" -o /dev/null \
  -w '%{http_code} %{size_download} %{speed_download}' \
  http://172.29.0.10:8080/blob)"

echo "  http $code, $bytes bytes at $speed B/s"

[[ "$code" == "200" ]] \
  || tollgate::fail "the download did not succeed (http $code); nothing crossed the gateway"
# At the rate bought, well over this arrives in the time allowed. A far smaller
# number means the flow stalled part way — which is what a grant lapsing under
# a transfer looks like, and is invisible in an average.
[[ "${bytes:-0}" -ge $(( DURATION * 1500000 )) ]] \
  || tollgate::fail "only $bytes bytes arrived in ${DURATION}s; the transfer stalled part way"

# The rate bought was 2.5 MB/s. Generous bounds either side: an unshaped bridge
# would deliver this an order of magnitude faster, and anything far below would
# mean the peer is not getting what it paid for.
speed=${speed%%.*}
[[ "$speed" -lt 6000000 ]] \
  || tollgate::fail "$speed B/s is far above the 2.5 MB/s bought; nothing is shaping"
[[ "$speed" -gt 1500000 ]] \
  || tollgate::fail "$speed B/s is far below the 2.5 MB/s bought"

# And the gateway counted it in the kernel, which is what draws the grant down.
delivered="$(client_field delivered)"
[[ "${delivered:-0}" -gt 1000000 ]] \
  || tollgate::fail "the gateway's nftables counters show only $delivered bytes"

echo "  ok: IPv4 forwarding"

# ---- A dual-stack customer's IPv6 ----
#
# The phone's session comes from its IPv4 address. The gateway ties its MAC to
# that, and through the MAC its IPv6 address, and gates, shapes and counts the
# IPv6 with the same grant. What is asserted is what crosses the gateway, not
# what the node believes.
PHONE6="fd00:28::40"
ORIGIN6="http://[fd00:29::10]:8080/blob"
p=(docker compose -f "$COMPOSE" exec -T phone)
gw=(docker compose -f "$COMPOSE" exec -T gateway)

# Whether an element is in one of the gateway's sets or maps.
in_set() { "${gw[@]}" nft list set inet tollgate "$1" 2>/dev/null | grep -q "$2"; }
in_map() { "${gw[@]}" nft list map inet tollgate "$1" 2>/dev/null | grep -q "$2"; }

# Pull from the origin over IPv6 for `$1` seconds; prints "code bytes speed".
fetch6() {
  "${p[@]}" curl -6 -g -s --max-time "$1" -o /dev/null \
    -w '%{http_code} %{size_download} %{speed_download}' "$ORIGIN6" || true
}

# The phone speaks IPv6 to the gateway itself, which is never gated, so the
# gateway has a neighbour entry tying the address to the phone's MAC.
"${p[@]}" ping -6 -c 2 -W 1 fd00:28::20 >/dev/null \
  || tollgate::fail "the phone cannot reach the gateway over IPv6"
"${p[@]}" ip -6 route get fd00:29::10 | grep -q "via fd00:28::20" \
  || tollgate::fail "the phone is not routing to the origin through the gateway"
phone_mac="$("${p[@]}" cat /sys/class/net/eth0/address)"

# Before payment: the phone has a session and buys nothing, and with no
# allowance it is not carried. (Its access can read `active` from opening a
# channel; with nothing bought it is shaped to zero, and zero is not carried.)
tollgate::wait_for "the phone to have a session" 90 \
  '[[ -n "$(phone_field access)" ]]'
[[ "$(phone_field shaped_rate)" == "0" ]] \
  || tollgate::fail "the phone is shaped to $(phone_field shaped_rate) without having bought anything"
tollgate::wait_for "the phone's MAC and IPv6 to be tied to its session" 30 \
  'in_set known_mac "$phone_mac" && in_set known6 "$PHONE6"'
in_set allowed6 "$PHONE6" && tollgate::fail "the unpaid phone's IPv6 is allowed"
read -r code bytes _ <<<"$(fetch6 5)"
echo "  unpaid: http $code, $bytes bytes"
[[ "$code" == "000" && "${bytes:-0}" == "0" ]] \
  || tollgate::fail "the unpaid phone reached the origin over IPv6 (http $code, $bytes bytes)"
echo "  ok: the unpaid phone's IPv6 is dropped"

# Now it pays: the same phone, its node started again wanting 2 MB/s.
PHONE_DEMAND=2000000 docker compose -f "$COMPOSE" up -d --no-deps phone-node
tollgate::wait_for "the phone to be paying" 120 \
  '[[ "$(phone_field access)" == "active" ]]'
tollgate::wait_for "the gateway to shape the phone to what it bought" 60 \
  '[[ "$(phone_field shaped_rate)" == "2500000" ]]'
tollgate::wait_for "the phone's IPv6 to be allowed" 30 \
  'in_set allowed_mac "$phone_mac" && in_set allowed6 "$PHONE6"'

# Its class is selected for IPv6 too: a mark for its address, and a tc filter
# for the ipv6 protocol pointing at that mark.
in_map mark6 "$PHONE6" || tollgate::fail "no mark6 element for $PHONE6"
mark="$("${gw[@]}" nft list map inet tollgate mark6 | grep -o "$PHONE6 : 0x[0-9a-f]*" | awk '{print $3}')"
"${gw[@]}" tc filter show dev "$edge" protocol ipv6 | grep -q "handle $mark" \
  || tollgate::fail "no tc ipv6 filter selects the phone's mark $mark"
in_map down6_tx "$PHONE6" || tollgate::fail "no down6_tx counter element for $PHONE6"

before="$(phone_field delivered)"
DURATION6=10
read -r code bytes speed <<<"$(fetch6 "$DURATION6")"
echo "  paid: http $code, $bytes bytes at $speed B/s"
[[ "$code" == "200" && "${bytes:-0}" -ge $(( DURATION6 * 1000000 )) ]] \
  || tollgate::fail "the paid phone's IPv6 was not forwarded (http $code, $bytes bytes)"
speed=${speed%%.*}
[[ "$speed" -lt 6000000 ]] \
  || tollgate::fail "$speed B/s over IPv6 is far above the 2.5 MB/s bought; nothing is shaping it"
echo "  ok: the paid phone's IPv6 is forwarded and shaped"

# Counted into the phone's own counters, which is what draws its grant down.
tollgate::wait_for "the phone's IPv6 to be counted" 15 \
  '[[ $(( $(phone_field delivered) - ${before:-0} )) -ge $(( bytes * 9 / 10 )) ]]'

# The grant lapses: its node is frozen, so it renews nothing, but the session
# stays up — a stopped node would end it, and a peer the gateway no longer
# knows is none of its business to gate.
docker compose -f "$COMPOSE" pause phone-node
tollgate::wait_for "the phone's grant to lapse" 30 \
  '[[ "$(phone_field shaped_rate)" == "0" ]]'
in_set allowed6 "$PHONE6" && tollgate::fail "the lapsed phone's IPv6 is still allowed"
read -r code bytes _ <<<"$(fetch6 5)"
echo "  lapsed: http $code, $bytes bytes"
[[ "$code" == "000" && "${bytes:-0}" == "0" ]] \
  || tollgate::fail "the lapsed phone reached the origin over IPv6 (http $code, $bytes bytes)"
echo "  ok: the lapsed phone's IPv6 is dropped again"
docker compose -f "$COMPOSE" unpause phone-node

echo "PASS: forwarding"
