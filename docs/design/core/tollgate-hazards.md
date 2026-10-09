# TollGate Hazards

Constraints that exist because removing them reintroduces a known abuse. Each
one below looks like an arbitrary restriction until you know what it prevents,
which is why the rationale travels with the rule.

Read this before adding anything that prices, routes, or gives capacity away.

---

## Never Pay A Peer A Bonus To Send Or Accept Traffic

The design makes a **bonus** for sending traffic unrepresentable rather than
merely forbidden: the `received_multiplier` is unsigned, so a node can charge
more for carrying a peer's traffic, or charge nothing, but can never pay a peer
on top of what it owes for delivery
([tollgate-vouchers.md](tollgate-vouchers.md)). The section stays because the
temptation recurs, and because anything added later must preserve the
property.

What remains is the base rule itself. Each side pays for what it receives, so
with the default multiplier of `0` a node pays a peer 1× for the traffic that
peer uploads to it. A peer can therefore push traffic nobody asked for and have
it drawn against a grant the receiver bought. That exposure is bounded by the
receiver alone: it is never more than the grant the receiver chose to buy from
that peer, and a node that does not want to pay for a peer's uploads sets the
multiplier to `1` (free) or higher (charged).
Accepting traffic can be faked — the peer takes it, bills for it, and discards
it, having done no work. Under such a price, discarding becomes the most
profitable thing it can do, and metering cannot tell the difference because it
counts what crossed the link, not what the peer did next.

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
cannot be checked. The hazard was never a negative sign; it is pricing the
acceptance of something whose acceptance can be faked.

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

There is no formula to attack today, because delivery has no price. Prepaid
grants harden this further: counters are local, never exchanged, and decide no
payment at all ([tollgate-metering.md](tollgate-metering.md)). A peer cannot
move money by reporting anything, because it reports nothing.

Keep it that way. `peer_metrics()` exists for operator visibility and capacity
decisions, never as a price input, and nothing measured should acquire the
power to move a payment.

---

## Unspent Capacity Must Expire

A grant is a quantity paired with a window, and what is not drawn by the
window's deadline is forfeit. In superseding mode, the default, it is also
forfeit when a new grant replaces it
([tollgate-vouchers.md](tollgate-vouchers.md)). That looks harsh, and the
temptation is to soften it: credit the remainder into the next grant, let a
small allowance accumulate, extend the deadline instead of replacing it.

Each of those turns a rate back into a stored quantity. Capacity is
perishable — an unsold second is gone whether or not anyone paid for it — so a
claim that never expires lets a buyer accumulate cheaply off-peak and present
the whole position at peak, which is when the capacity is scarce. The seller
sold bandwidth and delivered volume.

Two bounds hold it, and both are needed:

- **Forfeiture** at the deadline, so a claim cannot outlive its window.
- **`max_window_ms`**, so a window cannot be made long enough to span from
  off-peak to peak. Without it a payer defeats forfeiture by never letting a
  window end.

This holds in both accounting modes. The same reasoning is why the minimum
flow allowance is a rate rather than a per-interval quantity. A quantity
accumulates; a rate cannot.

### When the Window Can Be Long

Accumulative mode ([tollgate-vouchers.md](tollgate-vouchers.md#accumulative-a-running-budget))
softens the rule on purpose, in the ways listed above: a new grant adds to
what is left, and the operator sets `max_window_ms` to a month or a year.
Capacity still expires, at the deadline. The window can be that long only
because the speed comes from a cap rather than from `grant / window`:

- **`accounting.rate_cap`**, so a budget is spent no faster than the cap,
  however large it is.
- **`accounting.max_budget`**, so no peer holds more than that unspent at
  once.

What remains, which those bounds limit but do not remove:

- **Stockpiling for the busiest hour.** A buyer can buy whenever vouchers are
  cheap or the link is quiet and spend at the busiest hour, and keep a budget
  alive by buying again before its deadline. Every buyer can do the same at
  the same hour. The provider cannot refuse it then: the budget is already
  sold, and accumulative mode has no committed rate for admission control to
  check. At peak, N accumulative peers can each draw up to their rate cap, and
  if N caps exceed the link, every peer on it — superseding ones included —
  gets less than it paid for. `rate_cap` and `max_budget` bound it; an
  operator selling in this mode sizes the caps against the link, or accepts
  that the link is shared at peak.
- **The provider holds more prepaid value.** A superseding payer has paid at
  most one short window ahead. An accumulative payer may have paid up to
  `max_budget` ahead, until its deadline. If the provider defaults,
  disappears or loses its disk, the payer loses all of it. The provider, for
  its part, owes service it has already been paid for, which it has to keep
  on disk and honor. This is why `max_budget` defaults to a modest amount and
  buyers top up small and often.
- **A carried budget is drawn at the session's multiplier.** A budget carried
  into a new session is drawn at that session's received multiplier, so
  raising the multiplier between sessions reprices units already sold.

Superseding mode stays the default because it needs none of this weighed.

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

Standard Cashu mints do not preserve locks that way, so any design that relies
on a lock to restrict who can spend a proof needs modified mint software or a
scheme that does not depend on the lock surviving.

---

## Free Identities Multiply Anything Given Away

Anything a node gives away per peer is multiplied by however many identities
an attacker creates, and one machine can run all of them over the same physical
link. Today that means the **minimum flow allowance**, and `mintd`'s
auto-accept, whose limit is for that reason an issue rate across everyone who
asks ([tollgate-daemons.md](tollgate-daemons.md#auto-accept)); it applies to
anything added later that grants per peer.

Every such mechanism needs an aggregate cap across all unpaid peers, not only
a per-peer one, plus ideally a cost to holding an identity — proof-of-work, a
deposit, or an operator allowlist. None is specified.

---

## Summary

| Rule | Prevents |
|---|---|
| Never pay a peer a bonus to send or accept traffic | Peers profiting from traffic nobody wants |
| No price-aware routing | Cheapest route being a blackhole |
| Metrics never price inputs | A peer degrading its link to move its own price |
| Unspent capacity expires, and windows are capped — in accumulative mode, where windows are long by the operator's choice, each peer's speed and stored budget are capped as well | Buying capacity off-peak to present at peak |
| Free peering not transitive | Laundered free transit |
| Locks survive every swap | Locks removed by swapping through change |
| Aggregate caps on anything granted per peer | Free identities multiplying anything given away |
