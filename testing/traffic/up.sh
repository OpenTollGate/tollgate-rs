#!/usr/bin/env bash
# Bring up the traffic demo and leave it running. The client floods real metered
# traffic through the gateway; watch it climb in the gateway's tolltop.
#
# Tear down with:  docker compose -f testing/traffic/docker-compose.yml down
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
COMPOSE="docker compose -f $SCRIPT_DIR/docker-compose.yml"
CF="docker compose -f testing/traffic/docker-compose.yml" # short form for the cheat-sheet

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    "$TESTING_DIR/scripts/build.sh"
fi

echo "Bringing up the traffic demo..."
$COMPOSE up -d --no-build

cat <<EOF

Traffic demo is up: the client floods metered ping traffic through the gateway.
Give it a few seconds, then watch SENT/RECV climb (B -> KB -> MB) and BALANCE
drain on the gateway, from the repo root:

  $CF exec -it gateway tolltop        # live TUI (Peers tab)
  $CF exec gateway tolltop --once     # one-shot table
  $CF logs -f client                  # the CONSUME pay / auto-top-up loop
  $CF down                            # tear it all down
EOF
