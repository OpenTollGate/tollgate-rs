#!/usr/bin/env bash
# A gateway whose enforcement is a separate program: enforcer.kind external.
#
# The enforcer is `stub-enforcer`, which speaks the enforcer protocol and
# carries nothing: it holds what it is told and reports a payer as having moved
# its rate each second. What this asserts is the protocol between the two,
# from both sides:
#
#   - the two find each other from the instance's name alone, at
#     /run/tollgate-ip/enforcer.sock;
#   - the enforcer starts closed, is told who the client is before anything is
#     sold — its address, in 16 bytes — and opens it at the rate it bought;
#   - the counts it reports are what the gateway draws the grant down by;
#   - with the enforcer gone the gateway sells nothing, and keeps the session;
#   - an enforcer that comes back is told the full state again, and the totals
#     the gateway counted never go backwards;
#   - the gateway refuses to start behind an enforcer built for another
#     identity, or counting another unit, and names both; and refuses
#     identity: pubkey on a network that proves no keys.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
SERVICES="gateway client"
tollgate::start

compose() { docker compose -f "$COMPOSE" "$@"; }

# What the enforcer holds, read through the gateway, which shares its runtime
# directory — so it can still be read while the enforcer is stopped.
enforcer_state() {
  compose exec -T gateway cat /run/tollgate-ip/state.json 2>/dev/null || echo '{}'
}
enforcer_field() {
  enforcer_state | jq -r --arg k "$CLIENT_PUBKEY" ".payers[\$k].$1 // empty" 2>/dev/null || true
}
gateway_field() {
  tollgate::peer_field_of gateway "$CLIENT_PUBKEY" "$1"
}
# The same, as a number: zero while there is no such peer yet.
gateway_count() {
  local v
  v="$(gateway_field "$1")"
  echo "${v:-0}"
}

# --- found by the instance's name -------------------------------------------

tollgate::wait_for "the gateway's control socket in its runtime directory" 60 \
  'compose exec -T gateway test -S /run/tollgate-ip/control.sock'
tollgate::logs gateway | grep -q "\[ip\] " \
  || tollgate::fail "the gateway's log lines should name the instance"
echo "  ok: the instance ip, at /run/tollgate-ip/"

# --- the enforcer opens at what was bought ----------------------------------

tollgate::wait_for "the enforcer to open the client at the rate it bought" 120 \
  '[[ "$(enforcer_field rate)" == "2500000" ]]'

# The address the client came from, IPv4 written IPv4-mapped: 16 bytes, hex.
subject="$(enforcer_field 'subjects[0].subject')"
[[ "$subject" =~ ^00000000000000000000ffff[0-9a-f]{8}$ ]] \
  || tollgate::fail "the client should be bound to the address it came from, got '$subject'"
# Read without `// empty`, which would turn `false` into nothing.
[[ "$(enforcer_state | jq -r --arg k "$CLIENT_PUBKEY" '.payers[$k].subjects[0].delegated')" == "false" ]] \
  || tollgate::fail "the gateway's own binding is not delegated"
echo "  ok: bound to $subject"

# --- the enforcer's counts reach the ledger ---------------------------------

tollgate::wait_for "the enforcer's counts to reach the gateway" 30 \
  '(( $(gateway_count delivered) > 0 && $(gateway_count consumed) > 0 ))'

# --- the enforcer goes away -------------------------------------------------

compose stop enforcer >/dev/null
tollgate::wait_for "the gateway to notice" 30 \
  'tollgate::logs gateway | grep -q "selling nothing until"'
delivered_down="$(gateway_count delivered)"

# A grant already bought runs out, and nothing replaces it.
tollgate::wait_for "the client's grant to lapse" 30 \
  '[[ "$(gateway_field grant_expires_in_ms)" == "0" ]]'
bought="$(gateway_count authorized)"
sleep 5
[[ "$(gateway_count authorized)" == "$bought" ]] \
  || tollgate::fail "the gateway sold while its enforcer was down: $bought -> $(gateway_field authorized)"
[[ -n "$(gateway_field phase)" ]] \
  || tollgate::fail "the client's session should be kept through the outage"
tollgate::logs gateway | grep -q "refused a peer's purchase" \
  || tollgate::fail "the gateway should refuse the client's purchases while the enforcer is down"
echo "  ok: nothing sold for 5 s with the enforcer down; the session is kept"

# --- the enforcer comes back ------------------------------------------------

compose start enforcer >/dev/null
tollgate::wait_for "the enforcer to be told the client again, and opened" 60 \
  '[[ "$(enforcer_field rate)" == "2500000" ]]'
tollgate::wait_for "sales to resume" 60 \
  '(( $(gateway_count authorized) > bought ))'
tollgate::wait_for "the enforcer's counts to add to the old ones" 30 \
  '(( $(gateway_count delivered) > delivered_down ))'
connections="$(enforcer_state | jq -r .connections)"
[[ "$connections" == "1" ]] \
  || tollgate::fail "the restarted enforcer should have one connection, has $connections"

# --- the hello is a check ---------------------------------------------------

# Run a node that must refuse to start, and check it says why.
refuses() {
  local service="$1" what="$2"; shift 2
  local out
  if out="$(compose --profile mismatch run --rm "$service" 2>&1)"; then
    echo "$out" >&2
    tollgate::fail "$what: the node started"
  fi
  for needle in "$@"; do
    grep -qF -- "$needle" <<<"$out" \
      || { echo "$out" >&2; tollgate::fail "$what: the refusal should say '$needle'"; }
  done
  echo "  ok: $what"
}

refuses gateway-identity "an enforcer built for pubkey, behind identity: address" \
  "refusing to start behind the enforcer" "enforcer.identity is address" "says pubkey"
refuses gateway-unit "an enforcer counting ml, behind mint.unit byte" \
  "refusing to start behind the enforcer" 'mint.unit is "byte"' '"ml"'
refuses gateway-pubkey "identity: pubkey with no FIPS to prove a key" \
  "enforcer.identity is pubkey" "runs no FIPS"
compose --profile mismatch rm -sf enforcer-pubkey enforcer-ml >/dev/null 2>&1 || true

echo "PASS: external"
