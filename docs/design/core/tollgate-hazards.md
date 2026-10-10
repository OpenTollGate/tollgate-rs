# TollGate Hazards

Constraints that exist because removing them reintroduces a known abuse. Each
one below looks like an arbitrary restriction until you know what it prevents,
which is why the rationale travels with the rule.

Read this before adding anything that prices, routes, or gives capacity away.

---

## Never Pay A Peer A Bonus To Send Or Accept Traffic

The design makes a **bonus** for sending traffic unrepresentable rather than
merely forbidden: the `upstream_weight` is unsigned, so a provider can count
what a customer sends at more than what it receives, or at nothing, but can
never pay a customer for sending
([tollgate-vouchers.md](tollgate-vouchers.md#upstream-weight)). The section
stays because the temptation recurs, and because anything added later must
preserve the property.

A customer never earns anything by sending: it only buys. What remains is
peering, where each node buys from the other and usually at weight `0`, so
each pays for what flows towards it. A peer can therefore push traffic nobody
asked for and have it drawn against the budget the receiver holds with it.
That exposure is bounded by the receiver alone: it is never more than the
budget the receiver chose to hold with that peer, within its own buying limits
([tollgate-configuration.md](tollgate-configuration.md#buying)).
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
budgets harden this further: counters are local, never exchanged, and decide no
payment at all ([tollgate-metering.md](tollgate-metering.md)). A peer cannot
move money by reporting anything, because it reports nothing.

Keep it that way. `peer_metrics()` exists for operator visibility and capacity
decisions, never as a price input, and nothing measured should acquire the
power to move a payment.

---

## Unspent Capacity Must Expire

A payer's budget has a deadline, and what is left of it at the deadline
expires ([tollgate-vouchers.md](tollgate-vouchers.md#the-one-rule)). Without a
deadline a provider would owe service forever for units sold once, and would
have to keep every payer's record forever to honor it.

The deadline is all that expires. A new purchase adds to what is left and
forfeits nothing, and it moves the deadline to the later of the old one and
now plus its window. So **a payer can keep a budget alive by buying again
before each deadline**, however little it buys. That is accepted. It is safe
because of what the budget does and does not entitle the payer to, not
because the budget is kept small:

- **A reserved rate drains idle time.** While a payer reserves a rate, every
  second it is carried costs it at least that rate, used or not. Holding a
  reservation through a quiet hour costs a quiet hour at that rate, so a
  reserved payer cannot buy capacity cheaply off-peak and save it for the busy
  hour: what it saves, it pays for second by second.
- **A payer that reserves nothing is owed no speed.** Its budget is drawn only
  by what it moves, so it may keep units for a month. But the provider
  promised it nothing: the speed it is carried at is spare capacity, which the
  provider may give or withhold at any moment
  ([Speed Above the Reserved Rate](tollgate-vouchers.md#speed-above-the-reserved-rate)).
  A large budget is a claim on units, not on the busiest hour.
- **Admission control counts only reservations.** Every reserved rate the
  provider accepts fits under its capacity at once
  ([tollgate-protocol.md](tollgate-protocol.md#0x05-topupreject)). Nothing a
  payer holds in its budget can push that sum past capacity.

What a provider must therefore never do is promise speed it did not admit:
carry unreserved payers or bursts at a fixed speed, and then sell
reservations up to the full link as well. The operator leaves room for what it
gives above reservations, or gives it only when there is spare capacity
([tollgate-configuration.md](tollgate-configuration.md#burst)).

What remains, which the rule limits but does not remove:

- **The payer carries the risk of what it holds.** A payer has paid as far
  ahead as it chose to. If the provider defaults, disappears or loses its disk,
  the payer loses its whole budget. A deadline that passes while the provider
  is down takes the budget too, though nothing is drawn while the payer is not
  carried.
- **A carried budget goes to whoever holds the identity.** Under
  `enforcer.identity: address` a key is not proven, so whoever comes back with
  a departed payer's key from its address can draw its whole budget — not
  only the seconds of one purchase. Under `pubkey` the network proves the key
  and this does not arise.
- **A carried budget is drawn at the session's upstream weight.** A provider
  that raises its `upstream_weight` between sessions reprices units already
  sold, from the payer's next session on. The payer can refuse the new weight,
  but the budget it holds stays with that provider.

The same reasoning is why the minimum flow allowance is a rate rather than a
per-interval quantity. A quantity accumulates; a rate cannot.

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
| Unspent capacity expires at a deadline; reserved rates drain idle time; only reservations are admitted against capacity | Providers owing service forever, and capacity bought off-peak being presented at peak |
| Free peering not transitive | Laundered free transit |
| Locks survive every swap | Locks removed by swapping through change |
| Aggregate caps on anything granted per peer | Free identities multiplying anything given away |
