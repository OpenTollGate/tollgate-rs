#!/usr/bin/env bash
# A gateway whose enforcement is a separate program: forwarding.mode external.
#
# The gate is `stub-gate`, which speaks the gate protocol and carries nothing:
# it holds what it is told and reports a payer as having moved its rate each
# second. What this asserts is the protocol between the two, from both sides:
#
#   - the gate starts closed, is told who the client is before anything is
#     sold, and opens at the rate the client bought;
#   - the counts it reports are what the gateway draws the grant down by;
#   - with the gate gone the gateway sells nothing, and keeps the session;
#   - a gate that comes back is told the full state again, and the totals the
#     gateway counted never go backwards;
#   - a node pinned to one Identify mode refuses to start behind a gate that
#     requires the other.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
SERVICES="gateway client"
tollgate::start

compose() { docker compose -f "$COMPOSE" "$@"; }

# What the gate holds, read through the gateway, which shares its socket
# directory — so it can still be read while the gate is stopped.
gate_state() {
  compose exec -T gateway cat /run/gate/state.json 2>/dev/null || echo '{}'
}
gate_field() {
  gate_state | jq -r --arg k "$CLIENT_PUBKEY" ".payers[\$k].$1 // empty" 2>/dev/null || true
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

# --- the gate opens at what was bought --------------------------------------

tollgate::wait_for "the gate to open the client at the rate it bought" 120 \
  '[[ "$(gate_field rate)" == "2500000" ]]'

subject="$(gate_field 'subjects[0].subject')"
[[ "$subject" == ipv4\ * ]] \
  || tollgate::fail "the client should be bound to the address it came from, got '$subject'"
# Read without `// empty`, which would turn `false` into nothing.
[[ "$(gate_state | jq -r --arg k "$CLIENT_PUBKEY" '.payers[$k].subjects[0].delegated')" == "false" ]] \
  || tollgate::fail "the gateway's own binding is not delegated"
echo "  ok: bound to $subject"

# --- the gate's counts reach the ledger -------------------------------------

tollgate::wait_for "the gate's counts to reach the gateway" 30 \
  '(( $(gateway_count delivered) > 0 && $(gateway_count consumed) > 0 ))'

# --- the gate goes away -----------------------------------------------------

compose stop gate >/dev/null
tollgate::wait_for "the gateway to notice" 30 \
  'tollgate::logs gateway | grep -q "selling nothing until"'
delivered_down="$(gateway_count delivered)"

# A grant already bought runs out, and nothing replaces it.
tollgate::wait_for "the client's grant to lapse" 30 \
  '[[ "$(gateway_field grant_expires_in_ms)" == "0" ]]'
bought="$(gateway_count authorized)"
sleep 5
[[ "$(gateway_count authorized)" == "$bought" ]] \
  || tollgate::fail "the gateway sold while its gate was down: $bought -> $(gateway_field authorized)"
[[ -n "$(gateway_field phase)" ]] \
  || tollgate::fail "the client's session should be kept through the outage"
tollgate::logs gateway | grep -q "refused a peer's purchase" \
  || tollgate::fail "the gateway should refuse the client's purchases while the gate is down"
echo "  ok: nothing sold for 5 s with the gate down; the session is kept"

# --- the gate comes back ----------------------------------------------------

compose start gate >/dev/null
tollgate::wait_for "the gate to be told the client again, and opened" 60 \
  '[[ "$(gate_field rate)" == "2500000" ]]'
tollgate::wait_for "sales to resume" 60 \
  '(( $(gateway_count authorized) > bought ))'
tollgate::wait_for "the gate's counts to add to the old ones" 30 \
  '(( $(gateway_count delivered) > delivered_down ))'
connections="$(gate_state | jq -r .connections)"
[[ "$connections" == "1" ]] \
  || tollgate::fail "the restarted gate should have one connection, has $connections"

# --- Identify mismatch ------------------------------------------------------

compose --profile mismatch up -d gate-fips >/dev/null
if out="$(compose --profile mismatch run --rm --no-deps gateway-pinned 2>&1)"; then
  echo "$out" >&2
  tollgate::fail "a node pinned to claimed started behind a gate that requires fips"
fi
grep -q "refusing to run behind the gate" <<<"$out" \
  || { echo "$out" >&2; tollgate::fail "the refusal should say why"; }
echo "  ok: a pinned node refuses a gate that requires the other mode"
compose --profile mismatch rm -sf gate-fips >/dev/null 2>&1 || true

echo "PASS: external"
