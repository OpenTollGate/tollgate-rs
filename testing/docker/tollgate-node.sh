#!/bin/sh
# One TollGate node, as the packages run it: the mint and the merchant beside
# the protocol daemon. mintd and merchantd read /etc/tollgate/mint.yaml and
# merchant.yaml if a topology mounts them, and their defaults otherwise;
# tollgated gets whatever arguments the container was given.
#
# tollgated does not wait for either: a peer that funds against the mint before
# it answers, a settlement that reaches it too early, or a funding request that
# finds no merchant yet, is retried.
set -eu

if [ -f /etc/tollgate/mint.yaml ]; then
    mintd -c /etc/tollgate/mint.yaml --control-socket /run/mintd.sock &
else
    mintd --control-socket /run/mintd.sock &
fi

if [ -f /etc/tollgate/merchant.yaml ]; then
    merchantd -c /etc/tollgate/merchant.yaml &
else
    merchantd &
fi

exec tollgated "$@"
