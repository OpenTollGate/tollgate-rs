# Voucher Acquisition

How a peer comes to hold vouchers is **not the protocol's business**, any
more than how it came to hold sats. A peer arrives holding vouchers for the
node it wants service from, or it does not get service.

This document collects the routes that exist. None of them needs protocol
support, and a node offering any of them is acting as a market participant
rather than executing a protocol phase.

---

## Why There Is No Bootstrap Mechanism

An earlier design carried **bootstrap tokens**: a regular Cashu token a peer
could hand over when it could not reach a mint. The problem it solved was
circular — you need to pay to get online, but funding a payment channel
needs mint connectivity.

That problem does not exist here. The mint you need is the peer you are
already talking to. A peer can mint, swap and fund against its counterparty
over the peering link alone, with no upstream connectivity at all.

So the mechanism is gone, and with it a whole subsystem: the bootstrap state
machine, its message types, its mint-verification path, its config block,
and the `bootstrap_received` peer state. Sessions begin at channel
establishment.

**What that costs:** the design no longer guarantees that a peer holding
only sats can walk up to any node and get connected. A local swap (below)
recovers it for nodes that choose to offer one, but it is no longer
something every node must implement — which also means it is no longer
something every constrained device must implement.

---

## Routes

### Mint over Lightning

The peer asks the node's mint for a quote, pays the Lightning invoice, and
receives vouchers (NUT-04). Standard Cashu, no TollGate involvement.

Needs the peer to have connectivity already — another peering, a cellular
link, or the minimum flow allowance
([tollgate-vouchers.md](../core/tollgate-vouchers.md)).

### Direct purchase from the issuer

The node sells its own vouchers for sats, at whatever price it likes. This
is the whole mechanism for most peerings and needs no market to exist.

Because the issuer is the counterparty, settlement is free for it: it is
selling a claim it will honor by delivering, not moving money.

### Local swap

The peer offers sat-denominated tokens and the node issues vouchers in
return, if it wants those sats. The node has upstream connectivity and can
verify them with their mint.

This is the closest thing to the old bootstrap flow, and it recovers the
walk-up case — but as an offer a node chooses to make, not a protocol
obligation.

### Cross-mint swap

Trading one issuer's vouchers for another's, which is what a market needs.
**No working implementation exists.** The options are:

| Approach | Problem |
|---|---|
| Lightning hop | Fees, seconds of latency, liquidity requirements |
| NUT-11/NUT-14 hash-locked swap | Several round trips, both mints online, a counterparty required |
| Trusted exchange | Reintroduces a central party, defeating the point |

None survives running once per metering interval, so market purchases have
to be made in bulk and drawn down slowly. That concentrates issuer risk in
whatever is being held — see [issuer-risk.md](issuer-risk.md).

This is the main thing blocking the price signal. It does **not** block
operation: direct purchase and paid acceptance both move value without it.

---

## Open Problems

| Problem | Notes |
|---|---|
| Cross-mint atomic swap | No working Cashu implementation. Blocks the price signal, not operation. |
| First connection with no connectivity | A peer holding only sats and having no other link depends on some node choosing to offer a local swap. Nothing guarantees one will. |
| Bulk holding | Amortizing expensive swaps means holding a large position in one issuer's vouchers, which is exactly the exposure the design otherwise tries to keep to one metering interval. |
