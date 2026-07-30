# TollGate Hazards

Constraints that exist because removing them reintroduces a known abuse. Each
one below looks like an arbitrary restriction until you know what it prevents,
which is why the rationale travels with the rule.

Read this before adding anything that prices, routes, or subsidises.

---

## Never Price Traffic Acceptance

Paying a peer to accept **vouchers** is safe: whether it took them can be
checked, and it gains nothing by taking them and lying. That is what paid
acceptance does ([tollgate-vouchers.md](tollgate-vouchers.md)).

Paying a peer to accept **traffic** is not safe, and must never be added.
Accepting traffic can be faked — the peer takes it, bills for it, and discards
it, having done no work. Under such a price, discarding becomes the most
profitable thing it can do, and metering cannot tell the difference because it
counts what was delivered *to* the peer, not what the peer did next.

Three specific hazards:

- **Sink peers.** Advertise attractive terms for accepting traffic, receive,
  bill, discard. Zero cost, full revenue.
- **Cheapest route is a blackhole.** Peer identities are free, so a sink can
  undercut honest forwarders indefinitely.
- **Margin squeeze.** A peer floods a node whose terms for accepting traffic
  sit below that node's own onward cost.

Traffic is therefore always priced as **transit**, paid by whoever the traffic
is for, and verified by that party end-to-end. It stops paying when nothing
arrives.

**The general form:** never price the acceptance of something whose acceptance
cannot be checked. The hazard is not the negative sign — paid acceptance is
negative and safe — it is unverifiable acceptance.

---

## No Price-Aware Routing

TollGate does not influence path selection, and that is currently what defuses
the blackhole hazard above. Free identities plus any price for traffic
acceptance plus price-driven path selection selects for blackholes by
construction: the cheapest next hop is the one doing least work.

If price-aware routing is ever adopted it needs, first, a cost to holding an
identity, and second, a delivery signal the payer can verify. Neither exists.

---

## Metrics Are Never An Input To Price

Scaling price by link quality is tempting — `price = base × etx × (1 +
srtt_ms / 100)` mirrors FIPS's own link cost formula, and charging more for a
worse link looks like cost recovery.

It is unsafe. The metrics are measured **against the peer being priced**, so a
peer can degrade its own link to move its own price. With a positive price
that is self-limiting — the customer leaves. With a negative one it pays the
peer more for being worse, and the incentive runs the wrong way with no bound.

There is no formula to attack today, because delivery has no price. Keep it
that way: `peer_metrics()` exists for operator visibility and capacity
decisions ([tollgate-metering.md](tollgate-metering.md)), never as a price
input.

---

## Negative Delivery Prices Only For Conserved Resources

Negative pricing on delivery survives for one thing: **getting rid of
surplus** — a node with more of something than it can use, paying others to
take it. It cannot pay in its own vouchers, because those vouchers are a claim
on the very thing it is shedding.

For network forwarding this does not arise; every apparent example is ordinary
buying seen backwards. For **electricity it is real** — grids do go to
negative prices — and `tollgate-core` is resource-agnostic, so it is in scope.

Electricity is also the safe case: the meter is physical and the resource is
conserved, so whether it was actually consumed can be measured, which is never
true of forwarded bytes.

Negative delivery prices are therefore permitted only where the
ResourceAdapter declares the resource conserved and physically metered, and
require an absolute spending budget when enabled — see `subsidy` in
[tollgate-configuration.md](tollgate-configuration.md).

---

## Subsidy Budgets Are Absolute, Never Multipliers

Where a node spends outward, no counterparty's willingness to pay bounds the
total, so the bound has to be configured. Two failure modes make this sharp:

**Multipliers invert.** Bounds expressed as a fraction of a base price flip
sign against a negative base. With `base = -10`, a "ceiling" of `10.0` permits
`-100` — ten times deeper spend, under a name that reads as protection.

**Per-peer caps alone are defeated.** Identities are free, so an attacker
creates more peers. Caps must apply per peer *and* in aggregate.

---

## Capacity Growth Only On Revenue Channels

`capacity_growth_factor` rewards a peer relationship that has proven stable
across rollovers. That is right on a channel the *peer* funds.

On a channel this node funds because it owes the peer, the same rule rewards
whichever peer drains it fastest — and combined with automatic rollover it
drains the wallet unattended, limited only by the balance:

```
10 → 20 → 40 → 80 → … → max_capacity, then refilled indefinitely
```

Growth is therefore not applied to negatively-priced channels, and their
rollover is bounded by the subsidy budget.

---

## Transit Loss Resolution Is Sign-Aware

When two sides disagree on unit counts, billing uses the value that favors the
**deliverer** — the party that expended resources sending.

Stating that as "use the higher value" is only correct for a positive price.
Under a negative price the deliverer is the *payer*, so the higher value
favors the counterparty instead, letting a peer inflate its received-count and
skim up to the full tolerance every interval — indefinitely, without ever
crossing the threshold that triggers a warning.

See [tollgate-metering.md](tollgate-metering.md). Related: a counterparty
sitting persistently at the edge of the tolerance band is extracting the full
tolerance while never tripping the over-tolerance path, so implementations
should track the *signed mean* of divergence rather than its magnitude.

---

## Free Peering Is Not Transitive

Deciding not to charge a peer means free for **that peer's own traffic**, never
free for anything that peer is nominally the beneficiary of. Otherwise an
uncharged peer becomes a way to launder free transit for others.

---

## Locks Must Survive Every Swap

Any spending condition on a proof — P2PK or otherwise — only holds if it
survives every swap **including change**. Otherwise the holder swaps the
locked proof for something else and the lock is gone in one step.

This is why the minimum flow allowance is issued unlocked: standard Cashu
mints do not preserve locks that way, so locking it would need modified mint
software ([tollgate-vouchers.md](tollgate-vouchers.md)).

---

## Free Identities Bound Every Subsidy

Anything a node gives away per peer is multiplied by however many identities
an attacker creates, and one machine can run all of them over the same
physical link. This applies to the minimum flow allowance, to subsidy budgets,
and to anything added later that grants per peer.

Every such mechanism needs an aggregate cap across all unpaid peers, not only
a per-peer one, plus ideally a cost to holding an identity — proof-of-work, a
deposit, or an operator allowlist. None is specified.

---

## Summary

| Rule | Prevents |
|---|---|
| Never price traffic acceptance | Sink peers billing for discarded traffic |
| No price-aware routing | Cheapest route being a blackhole |
| Metrics never price inputs | A peer degrading its link to move its own price |
| Negative delivery prices only for conserved resources | Paying for consumption that cannot be verified |
| Absolute subsidy budgets, per peer and aggregate | Sign-inverted multipliers; Sybil-multiplied caps |
| Capacity growth on revenue channels only | Unattended wallet drain via automatic rollover |
| Sign-aware transit loss resolution | A permanent within-tolerance skim |
| Free peering not transitive | Laundered free transit |
| Locks survive every swap | Locks removed by swapping through change |
| Aggregate caps on anything granted per peer | Free identities multiplying every subsidy |
