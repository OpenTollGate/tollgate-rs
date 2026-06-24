#!/usr/bin/env bash
# Bring up the manual-testing sandbox (gateway + fake mint + keep-alive client)
# and leave it running. Builds the tollgate-test:latest image first unless
# SKIP_BUILD=1 (reuse an image you already built).
#
# Tear down with:  docker compose -f testing/sandbox/docker-compose.yml down
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE="docker compose -f $SCRIPT_DIR/docker-compose.yml"
CF="docker compose -f testing/sandbox/docker-compose.yml" # short form for the cheat-sheet

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    "$TESTING_DIR/scripts/build.sh"
fi

echo "Bringing up the sandbox..."
$COMPOSE up -d --no-build

cat <<EOF

Sandbox is up. Both gateway and client run as nodes, so each has its own control
socket — exec into either and just run tolltop:

  $CF exec -it gateway bash
      tolltop          # live TUI (press q to quit)
      tolltop --once   # one-shot table
  $CF exec -it client bash
      tolltop          # the child's view: the gateway as a ↑ provider it pays

The client already buys from the gateway (an upstream in its config), so the mesh
is mutual out of the box: the gateway shows the client as a ↓ customer, the client
shows the gateway as a ↑ provider. You can also pay the gateway by hand:

  $CF exec -it client bash
      tollgate pay --peer http://gateway:4747 --mint http://mint:3338 --amount 20
      tollgate connect --peer http://gateway:4747    # just detect + PriceSheet

Or run any of these directly without a shell:

  $CF exec gateway tolltop --once
  $CF exec client tollgate pay --peer http://gateway:4747 --mint http://mint:3338 --amount 20
  $CF logs -f gateway          # access decisions, metering
  $CF down                     # tear it all down

Tip: to watch balances drain live in tolltop, set price_per_second (e.g. 50) in
testing/sandbox/gateway.yaml and re-run this script.
EOF
