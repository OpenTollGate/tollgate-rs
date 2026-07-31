#!/usr/bin/env bash
#
# Two TollGate nodes on one machine, with traffic that ramps.
#
# A gateway sells access and a client buys it. The client's traffic generator
# climbs, the client buys more capacity as it climbs, and the gateway shapes it
# to exactly what it bought. The gateway's max_rate then refuses a purchase and
# names a rate it will take, which the client re-buys at within one round trip.
#
#   ./demo/run.sh              # run for 40 seconds
#   ./demo/run.sh 90           # run for 90 seconds
#   DEBUG=1 ./demo/run.sh      # include every protocol message
#
set -euo pipefail

DURATION="${1:-40}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
BIN="$ROOT/target/release/tollgated"

# What the gateway will sell to any one peer. Set well under where the ramp
# ends, so the refusal path is on screen rather than only in the tests.
#
# These are binary-round on purpose. The unit being sold is the byte and a grant
# decomposes into power-of-two proofs, so 12 MiB/s is a handful of proofs where
# 12 MB/s is a number with bits set all the way down.
GATEWAY_MAX_RATE=$((12 * 1024 * 1024))
DEMAND_START=$((512 * 1024))
DEMAND_STEP=$((2 * 1024 * 1024))
DEMAND_STEP_SECONDS=4

# Traffic every peer gets without paying. Keep it small — it is given away, and
# it is what a peer falls back to whenever its grant lapses.
MINIMUM_FLOW=4096

cleanup() {
  [[ -n "${GATEWAY_PID:-}" ]] && kill "$GATEWAY_PID" 2>/dev/null || true
  [[ -n "${CLIENT_PID:-}" ]] && kill "$CLIENT_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

echo "building..."
cargo build --release --bin tollgated --manifest-path "$ROOT/Cargo.toml" 2>&1 | tail -1

# Peers are dialed by public key, so each side has to know the other's before
# either starts.
"$BIN" --show-identity > "$WORK/gateway.id"
"$BIN" --show-identity > "$WORK/client.id"
key() { grep "$1" "$2" | awk '{print $2}'; }

GATEWAY_PUB=$(key pubkey "$WORK/gateway.id")
GATEWAY_SEC=$(key secret_key "$WORK/gateway.id")
CLIENT_SEC=$(key secret_key "$WORK/client.id")

cat > "$WORK/gateway.yaml" <<YAML
identity:
  secret_key: "$GATEWAY_SEC"
mint:
  url: "http://gateway.local:3338"
  unit: "byte"
vouchers:
  accepted_mints:
    - "http://gateway.local:3338"
  # An unsigned surcharge on what the client pushes at us. The net rate is m-1,
  # so 2 charges an upload at the same rate as a download.
  received_multiplier: 2
access:
  minimum_flow:
    enabled: true
    bytes_per_second: $MINIMUM_FLOW
grants:
  # The payer picks any window in this range, per grant, without negotiating.
  window_range_ms: [200, 30000]
  max_rate: $GATEWAY_MAX_RATE
network:
  listen: "127.0.0.1:4747"
YAML

cat > "$WORK/client.yaml" <<YAML
identity:
  secret_key: "$CLIENT_SEC"
mint:
  url: "http://client.local:3338"
  unit: "byte"
vouchers:
  accepted_mints:
    - "http://client.local:3338"
access:
  minimum_flow:
    enabled: true
    bytes_per_second: $MINIMUM_FLOW
buying:
  # A short window keeps the forfeit small when the rate is raised early.
  window_ms: 1000
  renew_lead_ms: 300
network:
  listen: "127.0.0.1:4749"
peers:
  "$GATEWAY_PUB":
    endpoint: "127.0.0.1:4747"
YAML

LOG_LEVEL="info"
[[ -n "${DEBUG:-}" ]] && LOG_LEVEL="tollgate_net=debug,info"

RUST_LOG="$LOG_LEVEL" "$BIN" -c "$WORK/gateway.yaml" > "$WORK/gateway.log" 2>&1 &
GATEWAY_PID=$!
sleep 1

# --demand is the starting offered load; --ramp steps it up every interval.
RUST_LOG="$LOG_LEVEL" "$BIN" -c "$WORK/client.yaml" \
  --demand "$DEMAND_START" --ramp "$DEMAND_STEP" --ramp-interval "$DEMAND_STEP_SECONDS" \
  > "$WORK/client.log" 2>&1 &
CLIENT_PID=$!
sleep 1

# A node that could not bind dies immediately, and the table below would then
# quietly show zeros forever rather than saying so.
for node in gateway client; do
  pid_var="$(tr '[:lower:]' '[:upper:]' <<< "$node")_PID"
  if ! kill -0 "${!pid_var}" 2>/dev/null; then
    echo "the $node did not start:" >&2
    cat "$WORK/$node.log" >&2
    exit 1
  fi
done

# Binary units throughout, because the unit being sold is the byte and grants
# decompose into power-of-two proofs: 1 MiB/s is a round number of proofs where
# 1 MB/s is not.
rate() {
  awk -v v="${1:-0}" 'BEGIN {
    if (v + 0 <= 0)         { printf "0 B/s" }
    else if (v >= 1048576)  { printf "%.2f MiB/s", v / 1048576 }
    else if (v >= 1024)     { printf "%.1f KiB/s", v / 1024 }
    else                    { printf "%d B/s", v }
  }'
}

cat <<BANNER

  gateway  127.0.0.1:4747   sells access, max_rate $(rate "$GATEWAY_MAX_RATE") to one peer
  client   127.0.0.1:4749   demand starts at $(rate "$DEMAND_START"), +$(rate "$DEMAND_STEP") every ${DEMAND_STEP_SECONDS}s

  demand   what the client's traffic generator wants. The only input; everything
           else below is a consequence of it.
  shaped   what the gateway will let the client draw. Not negotiated — it is
           what the client bought, and the gateway derives it from the payment
           alone. Measured on the gateway.
  down     bytes actually arriving, counted off the socket. Measured on the
           client.

  Neither node is ever told the other's number. The client knows what it signed
  for, the gateway knows what it delivered, and nothing reconciles them —
  the money moved before the traffic did.

BANNER

printf '%7s  %12s  %12s  %14s   %s\n' "time" "demand" "shaped" "down" "note"
printf '%7s  %12s  %12s  %14s   %s\n' "-------" "------------" "------------" "--------------" "----"

START=$SECONDS
prev_shaped=""

while (( SECONDS - START < DURATION )); do
  sleep 1
  # The gateway knows what it is shaping the client to; the client knows what
  # is arriving. Neither number is exchanged — that is the point.
  shaped=$(grep -o 'shaped=[0-9]*' "$WORK/gateway.log" | tail -1 | cut -d= -f2 || true)
  demand=$(grep -o 'demand=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)
  down=$(grep -o 'down=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)
  : "${shaped:=0}" "${demand:=0}" "${down:=0}"

  # Annotate only the rows that are doing something, so the ones that are stand
  # out instead of every row carrying text.
  note=""
  if (( shaped <= MINIMUM_FLOW )); then
    note="nothing bought yet — minimum flow allowance only"
  elif (( shaped == GATEWAY_MAX_RATE )) && [[ "$shaped" != "$prev_shaped" ]]; then
    note="asked above the gateway's cap: refused, re-bought at the cap"
  elif [[ -n "$prev_shaped" ]] && (( prev_shaped <= MINIMUM_FLOW && shaped > prev_shaped )); then
    note="first purchase — no grant in force, so nothing forfeited"
  elif [[ -n "$prev_shaped" ]] && (( shaped > prev_shaped )); then
    note="demand rose: bought more, forfeiting the old grant's remainder"
  elif [[ -n "$prev_shaped" ]] && (( shaped < prev_shaped )); then
    note="demand fell: renewed lower once the old grant lapsed"
  elif (( shaped == GATEWAY_MAX_RATE )); then
    note="at the gateway's cap"
  elif (( down * 10 < shaped * 9 )); then
    note="shaper still filling after the raise"
  fi

  printf '%6ss  %12s  %12s  %14s   %s\n' \
    "$((SECONDS - START))" "$(rate "$demand")" "$(rate "$shaped")" "$(rate "$down")" "$note"

  prev_shaped="$shaped"
done

echo
echo "logs kept until this script exits: $WORK"
echo "  gateway: $WORK/gateway.log"
echo "  client:  $WORK/client.log"
