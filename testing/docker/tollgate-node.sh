#!/bin/sh
# One TollGate node, as the packages run it: the mint beside the protocol
# daemon. mintd reads /etc/tollgate/mint.yaml if a topology mounts one, and its
# defaults otherwise; tollgated gets whatever arguments the container was given.
#
# tollgated does not wait for the mint: a peer that funds against it before it
# answers, or a settlement that reaches it too early, is retried.
set -eu

if [ -f /etc/tollgate/mint.yaml ]; then
    mintd -c /etc/tollgate/mint.yaml --control-socket /run/mintd.sock &
else
    mintd --control-socket /run/mintd.sock &
fi

exec tollgated "$@"
