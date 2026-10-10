#!/usr/bin/env bash
# Demand becomes a purchase, and a purchase becomes a shaped rate; and the
# budget it bought outlives the session and a restart of the gateway.
#
# The whole loop in one assertion: the client wants 2 MB/s, reserves 125% of
# it, and the gateway shapes it to exactly that — sent as the TopUp's reserved
# rate, with no burst above it configured.
set -euo pipefail
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"
tollgate::start

tollgate::wait_for "the gateway to shape the client to what it bought" 90 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

# The payer's own view has to agree, and it is computed independently.
tollgate::wait_for "the client to agree on the rate it bought" 30 \
  '[[ "$(tollgate::peer_field client bought_reserved_rate)" == "2500000" ]]'
tollgate::wait_for "the gateway to report the reservation" 30 \
  '[[ "$(tollgate::peer_field gateway reserved_rate)" == "2500000" ]]'

# The budget is renewed before it runs out: added back, never forfeit.
expires="$(tollgate::peer_field gateway budget_expires_in_ms)"
[[ "${expires:-0}" -gt 0 ]] || tollgate::fail "the budget ran out instead of being renewed"
tollgate::wait_for "the client to hear its budget in a Balance" 30 \
  '[[ "$(tollgate::peer_field client reported_balance.remaining)" -gt 0 ]]'

# And bytes actually moved.
to_payer="$(tollgate::peer_field gateway to_payer)"
[[ "${to_payer:-0}" -gt 1000000 ]] \
  || tollgate::fail "only $to_payer bytes sent to the client; the data plane is not carrying traffic"

# --- the budget outlives the session and the gateway ------------------------

# The client goes, and the gateway restarts while it is away. Its budget is
# thirty seconds of its reserved rate, so it is still there when it comes back.
compose() { docker compose -f "$COMPOSE" "$@"; }
compose stop client >/dev/null
compose restart gateway >/dev/null
compose exec -T gateway sh -c 'cat /var/lib/tollgate/budgets-*.json' 2>/dev/null \
  | jq -e 'to_entries | any(.value.remaining > 0)' >/dev/null \
  || tollgate::fail "the gateway kept no budget on disk for the client"
compose start client >/dev/null
tollgate::wait_for "the gateway to hand the client back its budget" 60 \
  'tollgate::logs gateway | grep -q "a payer came back to its budget"'
tollgate::wait_for "the client to be carried again at what it reserves" 60 \
  '[[ "$(tollgate::peer_field gateway shaped_rate)" == "2500000" ]]'

echo "PASS: purchase"
