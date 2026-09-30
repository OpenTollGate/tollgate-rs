#!/bin/sh
# One TollGate node, as the packages run it: the mint and the merchant beside
# the protocol daemon. mintd and merchantd read /etc/tollgate/mint.yaml and
# merchant.yaml if a topology mounts them, and their defaults otherwise;
# tollgated gets whatever arguments the container was given.
#
# tollgated is started only once both answer. It would cope without that — a
# funding that fails is asked for again, and a settlement retried — but every
# failure at boot is a warning in the log and a round of retries the test then
# waits out, and a peer's first channel is funded against this mint the moment
# the peer connects. A daemon that has not come up in time is reported and
# tollgated started anyway, so the test fails on what it asserts rather than
# here.
set -eu

# Where to look. The defaults are mintd's private listener and merchantd's
# funding socket; a topology that moves either says so.
MINT_READY_URL="${MINT_READY_URL:-http://127.0.0.1:3337/v1/keysets}"
MERCHANT_SOCKET="${MERCHANT_SOCKET:-/tmp/merchantd.sock}"
READY_TIMEOUT_SECONDS="${READY_TIMEOUT_SECONDS:-60}"

if [ -f /etc/tollgate/mint.yaml ]; then
    mintd -c /etc/tollgate/mint.yaml --control-socket /run/mintd.sock &
else
    mintd --control-socket /run/mintd.sock &
fi
MINTD_PID=$!

if [ -f /etc/tollgate/merchant.yaml ]; then
    merchantd -c /etc/tollgate/merchant.yaml &
else
    merchantd &
fi
MERCHANTD_PID=$!

mint_ready() {
    curl -fsS -o /dev/null --max-time 2 "$MINT_READY_URL" 2>/dev/null
}

merchant_ready() {
    [ -S "$MERCHANT_SOCKET" ]
}

# Twice a second, up to the timeout. A daemon that has exited will never be
# ready, so that ends the wait early.
i=0
limit=$((READY_TIMEOUT_SECONDS * 2))
while [ "$i" -lt "$limit" ]; do
    if mint_ready && merchant_ready; then
        break
    fi
    if ! kill -0 "$MINTD_PID" 2>/dev/null; then
        echo "tollgate-node: mintd exited before it was ready" >&2
        break
    fi
    if ! kill -0 "$MERCHANTD_PID" 2>/dev/null; then
        echo "tollgate-node: merchantd exited before it was ready" >&2
        break
    fi
    i=$((i + 1))
    sleep 0.5
done

mint_ready || echo "tollgate-node: mintd is not answering at $MINT_READY_URL; starting tollgated anyway" >&2
merchant_ready || echo "tollgate-node: no merchantd socket at $MERCHANT_SOCKET; starting tollgated anyway" >&2

exec tollgated "$@"
