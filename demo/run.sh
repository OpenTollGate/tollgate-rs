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
GATEWAY_MAX_RATE=12000000

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
    bytes_per_second: 4096
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
    bytes_per_second: 4096
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
  --demand 500000 --ramp 2000000 --ramp-interval 4 \
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

cat <<BANNER

  gateway  127.0.0.1:4747   sells access, max_rate $((GATEWAY_MAX_RATE / 1000000)) MB/s to one peer
  client   127.0.0.1:4749   demand ramps 0.5 MB/s, +2 MB/s every 4s

  shaped = what the gateway will let the client draw, which is what it bought
  down   = what is actually arriving

BANNER

printf '%7s  %14s  %14s  %14s\n' "time" "demand" "shaped" "measured down"
printf '%7s  %14s  %14s  %14s\n' "-------" "--------------" "--------------" "--------------"

START=$SECONDS
while (( SECONDS - START < DURATION )); do
  sleep 1
  # The gateway knows what it is shaping the client to; the client knows what
  # is arriving. Neither number is exchanged — that is the point.
  shaped=$(grep -o 'shaped=[0-9]*' "$WORK/gateway.log" | tail -1 | cut -d= -f2 || true)
  demand=$(grep -o 'demand=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)
  down=$(grep -o 'down=[0-9]*' "$WORK/client.log" | tail -1 | cut -d= -f2 || true)

  capped=""
  [[ -n "$shaped" && "$shaped" == "$GATEWAY_MAX_RATE" ]] && capped="  <- refused, re-bought at the gateway's limit"

  printf '%6ss  %14s  %14s  %14s%s\n' \
    "$((SECONDS - START))" "${demand:-0}" "${shaped:-0}" "${down:-0}" "$capped"
done

echo
echo "logs kept until this script exits: $WORK"
echo "  gateway: $WORK/gateway.log"
echo "  client:  $WORK/client.log"
