# TollGate Vouchers (Proposal)

> **Status: proposal — not adopted.** Nothing in this document is implemented
> or normative. The adopted payment model is sat-denominated Cashu with
> Spilman channels, specified in
> [tollgate-payment-channels.md](tollgate-payment-channels.md) and
> [tollgate-pricing.md](tollgate-pricing.md). This document records an
> alternative worth evaluating, the arguments for and against it, and the
> parts of it worth adopting on their own.

**Voucher** is the plain name for what a Cashu token *means* when its keyset
is denominated in bytes and its mint is the node that will deliver them: a
bearer claim on that node's capacity, redeemable only against it. Redeeming
it gets you the service, not money.

The protocol term is **Cashu token**, holding **proofs** whose amounts are
powers of two and whose unit is the **byte**. Splitting, combining, DLEQ
verification, P2PK locking and the spent-proof set all work exactly as they
do today.

Three things change, and nothing else:

- the keyset unit is `byte` rather than `sat`
- the mint is run by the node that will deliver those bytes
- redemption means that node delivers, rather than a mint paying out

The proposal: each node runs its own mint and issues vouchers instead of
quoting prices in sats. Two peers set the exchange rate between their
vouchers themselves, with an optional market on top for price discovery.

---

## Motivation

Pricing today is bilateral and take-it-or-leave-it
([tollgate-pricing.md](tollgate-pricing.md)). A peer cannot compare two
providers without connecting to both, and nothing in the design prices an
operator's reliability. A node that takes payment and delivers poorly is
noticed only by the peer that already paid it.

Vouchers move price discovery out of the private price sheet, where only the
two peers can see it, into a form any node can read.

---

## The Instrument

### Denomination

A proof carries an integer amount in its keyset's unit. For network
forwarding that unit is the **byte**. Amounts are powers of two and any
total is a set of proofs summing to it, exactly as in any other Cashu wallet
— a 1 KiB claim is a single `1024` proof, two of them make 2 KiB, and they
split and combine like any other token.

Other resources use their own quantity unit: watt-hours for electricity, mL
for water. The existing keyset machinery covers all of them unchanged.

### Buying a Rate

The buyer thinks in rates. The metering interval is fixed for the peering
before any payment happens (both peers send an acceptable range and the
interval is the average of the overlap — see Price Negotiation in
[tollgate-pricing.md](tollgate-pricing.md)), so the amount to hand over each
interval is the product:

```
voucher_amount = desired_rate × interval

5 s interval, 1 KiB/s     1024 × 5       =   5,120 bytes   (2 proofs)
5 s interval, 100 KiB/s   1024 × 100 × 5 = 512,000 bytes   (6 proofs)
```

One voucher settles each interval in both cases. What changes is the amount
it carries and how many proofs that amount decomposes into:
`5,120 = 4096 + 1024`, and `512,000 = 125 × 4096` where 125 has six bits
set. Both are ordinary Cashu amounts needing no special handling.

**Paying more in one interval buys a higher rate for the next.** That is the
whole rate-auction mechanism — see Rate Auction and Token Bucket below.

A byte is a byte wherever it is spent, so two vouchers for the same amount
are interchangeable. That is what lets them trade at a single price on a
market.

### One Unit Per Resource, Many Issuers

The resource fixes the unit. Every node selling the same resource
denominates the same way, so two units matter per resource — the rate a peer
draws at, and the quantity a voucher holds:

| Resource | Rate — how fast a peer may draw | Voucher unit — what the claim holds |
|---|---|---|
| Network forwarding | bytes/second | bytes |
| Electricity | watts | watt-hours |
| Water | mL/second | mL |

Electricity is the familiar case: capacity is drawn at a rate in watts and
sold in watt-hours. A watt-hour is a rate already multiplied by a duration,
which is the step a voucher takes before it exists. The byte is the network
equivalent of the watt-hour.

The unit is shared; the issuer is not. A 1 KiB voucher from a reliable
gateway and a 1 KiB voucher from an unreliable one both claim 1024 bytes,
and they are worth different amounts.

Byte denomination makes metering exact, so every payment lands on a whole
number of units. The cost of that precision is proof count: amounts are
powers of two and a payment takes one proof per set bit, so the count scales
with how many bits the number has. A 1 GiB payment is a 30-bit number and
takes up to 30 proofs, about 15 on average. That is what the spent-proof set
below has to absorb.

What an issuer's voucher sells for, compared to the quantity printed on it,
is therefore a direct measure of how much the network expects that issuer to
deliver:

| Selling price of a 1 KiB voucher | What it means |
|---|---|
| 1.00 KiB | Full value — the market expects redemption in full |
| 0.70 KiB | Issued more than it can deliver, unreliable, or in low demand |
| 1.05 KiB | In demand — access to this node is scarce |

The adopted design has no comparable signal. Here it is public, updated
continuously, and set by people who lose money if they get it wrong.

---

## Normal Operation

One Cashu token settles one metering interval. Which node's keyset it was
minted against says who owes the delivery; the amount says how much. There
is no extra structure — a wallet that can hold sat-denominated proofs can
hold these.

Each direction of a peering is priced and paid on its own, exactly as today.
What changes is what you pay with: **you pay in the vouchers of whoever is
delivering.**

![Voucher Lifecycle](diagrams/voucher-lifecycle.svg)
<details><summary>Text version</summary>

```
  1. Node B issues vouchers against its own capacity
  2. Client A acquires B-vouchers — on a market, or directly from B
  3. A spends B-vouchers with B
  4. B redeems: cancels its own claim, delivers the service

  Redemption costs B nothing but the service it already sells.
```
</details>

If A also delivers something B wants, the same thing happens in the other
direction with A-vouchers, priced and paid separately. Where A delivers
nothing B wants — a leaf node — that second payment is simply absent.
Nothing has to be zero-priced or cancelled out; there is one payment instead
of two.

That is the whole mechanism for the large majority of peerings. Vouchers can
be acquired **on a market or directly from the issuer**, and the rest of
this document only needs the direct route.

---

## Paid Acceptance

Take a leaf node A peering with its parent relay B. A's download is
straightforward — B delivers it, A pays for it in B-vouchers. A's upload is
the awkward half.

B gains nothing from receiving A's upload. It is not a service A performs
for B; it is traffic B has to carry onward on A's behalf, at a cost to B.
So **A has to pay for its upload too**.

The payment protocol says the deliverer charges, and on the upload leg the
deliverer is A. Taken literally that has B owing A for A's own outgoing
traffic. Correcting it inside the payment flow is what forces signed prices,
sign-aware metering and a subsidy budget — the machinery described in
[tollgate-pricing.md](tollgate-pricing.md).

Paid acceptance corrects it outside the payment flow instead. A issues its
own vouchers, which B has no use for. A **pays B — in B's vouchers — to hold
them**. B now has A-vouchers on hand to pay A with when A delivers its
upload, and both directions run under the ordinary rule with ordinary
positive prices.

**The negative price lives entirely in the price of A's vouchers, so the
payment protocol never sees a negative number.**

![Paid Acceptance](diagrams/paid-acceptance.svg)
<details><summary>Text version</summary>

```
  A → B:  an A-voucher worth n  +  a B-voucher worth m

  The voucher price is −m/n: A pays m to place n, so A's vouchers
  price out negative. Quoted by B, renegotiated at each metering
  interval like any other price.
```
</details>

Following the value through one interval: A hands B `m` B-vouchers plus some
of its own. When A delivers its upload, B pays for it with those same
A-vouchers, and A redeems them — canceling a claim it issued itself, at no
cost to anyone. A's real outlay is the `m` B-vouchers, which is what having
B carry the upload is worth.

The A-voucher side is a float A keeps topped up. If A stops topping it up, B
runs out and cannot pay for A's upload — which costs A nothing, since A
never wanted to be paid for it in the first place.

The voucher price measures one thing: how much B wants A's capacity. The
leaf sits at one end, where B wants it so little that A must pay. A gateway
trying to attract traffic sits at the other, buying its peers' vouchers
because it wants what they deliver — the "attract resources" case from
[tollgate-pricing.md](tollgate-pricing.md), and the same price with the
opposite sign.

### One Price, Crossing Zero

Paid acceptance and normal operation are one price per peering that happens
to cross zero, so leaf nodes need no separate rule.

The price is the price **of A's vouchers**, quoted by B. It follows the same
sign convention as the rest of the design ([tollgate-pricing.md](tollgate-pricing.md),
where `negative = node pays peer`): positive when the vouchers are worth
something, negative when they are a burden someone has to be paid to take.

![Voucher Price Scale](diagrams/voucher-price-scale.svg)
<details><summary>Text version</summary>

```
  voucher price:  positive ──────── zero ──────── negative ──── refused
                     B buys A's       even swap     A pays B      B will not
                     vouchers                       to take them  hold them
                     │                │             │             at any price
                     │                │             │             │
                     normal operation │             paid          normal
                     in the other     │             acceptance    operation,
                     direction        │                           one payment
```
</details>

- **Positive** — B wants A's vouchers enough to buy them. That is normal
  operation running in the other direction.
- **Zero** — an even swap; the two sides trade vouchers and nothing else
  moves.
- **Negative** — A pays B to take them. Paid acceptance.
- **Refused** — B will not hold A's vouchers at any price. A pays purely in
  B-vouchers, which is normal operation with only one direction.

Above zero this is the same number as the selling price in One Unit Per
Resource, Many Issuers — 1.05 for a node in demand, 0.70 for one the market
doubts. The scale simply continues below zero, where the issuer has to pay
to place its vouchers at all.

The refused end is the **safe default**. A node not configured to accept
other nodes' vouchers refuses, everything still works, and the peering falls
back to normal operation.

Three consequences:

- **A market is not required to operate.** Cross-mint atomic swap and market
  liquidity are still needed for the reliability signal above, but not for
  moving value.
- **Checking a voucher takes one hop.** B validates A-vouchers by asking A,
  the peer it is already connected to and already exchanging metering
  reports with. No third-party mint has to be reachable. The risk that A
  refuses to redeem is unchanged, and is limited by how many A-vouchers B
  agrees to hold — which is what the voucher price controls.
- **Issuing more vouchers does not help the issuer.** B sets the acceptance
  price from its own estimate of A's capacity, not from how many vouchers A
  offers, so printing more only moves the price.

### Pay for Voucher Acceptance, Never for Traffic Acceptance

Paid acceptance is safe because whether B took the vouchers is a fact anyone
can check, and B gains nothing by taking them and lying about it.

The same arrangement applied to traffic is not safe. Accepting traffic can
be faked: the peer takes it, bills for it, and discards it, having done no
work. Discarding becomes the most profitable thing it can do. See the
Negative Pricing section of [tollgate-pricing.md](tollgate-pricing.md).

Traffic must therefore stay priced as **transit**, which the payer checks
end-to-end and stops paying for when nothing arrives. Merging the two into a
single "pay you to take my traffic" price brings that abuse straight back.

![Two Things, Priced Separately](diagrams/voucher-two-legs.svg)
<details><summary>Text version</summary>

```
  voucher acceptance — the negative price lives here
  A ─────── pays B to accept A-vouchers ──────→ B
            ✓ whether B took them can be checked directly

  transit — always positive, always the beneficiary paying
  A ←────────── B provides transit ──────────── B
            ✓ A checks end-to-end, stops paying if nothing arrives

  ✗ never merge into one "pay you to take my traffic" price —
    accepting traffic can be faked, and discarding it then becomes
    the most profitable thing the peer can do.
```
</details>

---

## What Improves

**Settlement is free for the provider.** A node receiving its own vouchers
is canceling its own claim. No mint round-trip, no trust in a foreign mint,
nobody to rely on. All the work moves to whoever acquires the vouchers, who
can spread it over many payments.

This applies to the redemption step. A node that also accepts its peers'
vouchers under paid acceptance does take on those issuers, and prices that
risk through the voucher price it quotes.

**Double-spend checking becomes local.** Today a provider must reach a
third-party mint to confirm a token is unspent, and that network hop is
Spilman's main justification. Under vouchers the provider decides on its own
vouchers: a local database lookup, sub-millisecond, and it works during an
outage.

**Payment works whenever service works.** Today a mint outage blocks
funding, rollover, and settlement even though the link itself is fine
([tollgate-payment-channels.md](tollgate-payment-channels.md), "What Needs
the Mint"). When the provider is the mint, the two fail together — if you
can be served, you can pay.

**A fixed amount for the consumer.** You buy 1 GiB and you get 1 GiB. No
exposure to the sat price mid-session, and no price sheet changing under you
at the next interval.

**Selling capacity ahead of time.** Issuing vouchers is selling capacity
before delivering it, which is a way to raise funds. A rooftop antenna can
pay for itself against next month's bytes.

**Honest about what it is.** If a node is going to issue a claim at all, a
voucher is the more honest form than node-issued sat-denominated ecash. It
says outright that redemption is service, and promises nothing about being
spendable anywhere else.

---

## What It Costs

**Zero-trust payment does not survive.** This is the main objection. What
protects a sender from a receiver who takes payment and disappears is
Spilman's time-locked refund, and the mint is what honors it. When the
provider is the mint, the only party who can cheat is also the party that
would have to honor the refund. It will simply refuse.

No cryptography fixes this. Two things limit it instead:

- **Policy** — the loss is however many vouchers are held or committed at
  once. Hold one interval's worth, risk one interval's worth. The adopted
  design already accepts this same bound for receiver-rugpull.
- **Reputation** — an issuer that stops redeeming sees its vouchers sell for
  less, which makes everything it issues later worth less too.

That is a workable model, but it rests on reputation rather than
cryptography, and [tollgate-intro.md](tollgate-intro.md) currently claims
the stronger property. Adopting vouchers means withdrawing that claim, not
rewording it.

**Cross-mint atomic swap has no working implementation.** Buying vouchers on
a market needs one of: a Lightning hop (fees, seconds, liquidity), a
NUT-11/NUT-14 hash-locked swap (several round trips, both mints online, a
counterparty), or a trusted exchange (which defeats the point). None of them
survives running once per metering interval, so market purchases have to be
made in bulk and used up slowly — which brings back the bulk prepayment
model [tollgate-intro.md](tollgate-intro.md) deliberately moved away from,
and puts all the issuer risk in whatever you are holding.

Paid acceptance takes this off the critical path: the exchange happens inside
the peering, priced by the counterparty, checked in one hop. The problem
still applies to the reliability signal, which needs a market to exist.

**Getting hold of vouchers is continuous, not one-time.** Sats can be loaded
in advance from anywhere. Vouchers cannot, because you do not know which
node you will meet. Every new peering needs vouchers acquired before it can
pay for anything. That is no longer a protocol concern (see Acquiring
Vouchers below), which simplifies the protocol but leaves the peer to solve
it — with another link, a Lightning payment, or a node willing to swap sats
locally.

**Relays hold two kinds of vouchers.** A relay receives its own vouchers
from downstream and needs upstream vouchers to pay onward. Its margin is the
gap between the two, and it has to keep rebalancing. On an ESP32 that is a
real new burden.

Paid acceptance reduces this without removing it. A relay's capacity is
genuinely useful to its upstream, because return traffic flows through it,
so the relay's own vouchers price positive and it holds fewer foreign ones.
The burden lands hardest on leaf nodes, which are net consumers bringing in
outside money anyway.

**Market liquidity is unproven.** Each issuer is its own small, thin market.
Anyone making that market has to hold vouchers, and holding them means
taking the risk that an anonymous router stops redeeming. The case for doing
that business is the weakest part of the proposal — though once vouchers can
be acquired directly from the issuer, the market only buys the reliability
signal and is not needed to move value.

---

## Spilman Channels Under Vouchers

Channels are still needed, but **for a different reason: keeping the
issuer's database small, not preventing theft.**

Checking for double spends locally is cheap, but every spent proof has to be
recorded in the issuer's spent-proof set permanently. That is one record per
**proof**, not per payment — and because amounts are powers of two, a
payment is a set of proofs, roughly one per set bit. Byte denomination makes
those numbers large: 1 MB/s over a 5 s interval is 5,242,880 bytes, a 23-bit
number, so about 11–12 proofs per payment.

![Spilman as State Compression](diagrams/voucher-state-compression.svg)
<details><summary>Text version</summary>

```
10 peers, 5 s interval, 1 MB/s each:
  86400 / 5 × 10              = 172,800 payments/day
  × ~11.5 proofs per payment  ≈ 2.0M proof records/day
  × ~80 bytes per record      ≈ 160 MB/day

Same load with channels (1 h TTL, so ≥24 rollovers/day/channel):
  24 × 10                     = 240 settlements/day
  × ~11.5 proofs each         ≈ 2,800 proof records/day
                              ≈ 216 KB/day
```
</details>

Roughly **700× fewer records**. The decomposition factor applies to both
sides, so it cancels out of the ratio — but it does mean the absolute figure
is far higher than counting one record per payment suggests. OpenWrt targets
have 16–128 MB of flash and ESP32 has far less, so without channels the
spent-proof set alone ends the session within hours.

The channel machinery and the rollover logic therefore both stay, but the
reasoning in
[tollgate-payment-channels.md](tollgate-payment-channels.md) would need
rewriting: channels are not protecting the payer from the issuer, they are
keeping the issuer's database bounded.

Two consequences:

- The local spent-proof check needs an atomic check-and-set if the provider
  runs as more than one process. Straightforward on a router, but it has to
  be specified.
- Cryptographic effort belongs on the **exchange step**, where two parties
  who do not trust each other trade vouchers and neither one issued what the
  other is handing over. That is the only step with a real adversary, and
  the one with no implementation today.

---

## Acquiring Vouchers

Bootstrap tokens solve a problem vouchers do not have. A new peer cannot
fund a Spilman channel without reaching a mint, and cannot reach a mint
without first getting online
([tollgate-bootstrap.md](tollgate-bootstrap.md)). When the mint *is* the
peer you are already talking to, reachability is never the obstacle — a peer
can mint, swap and fund against its counterparty over the peering link
alone, with no upstream connectivity at all.

**So the bootstrap mechanism comes out of the protocol entirely.** How a
peer came to hold vouchers is no more the protocol's business than how it
came to hold sats today. It arrives holding them, or it does not get
service.

Two routes exist, both outside the protocol:

- **Mint over Lightning.** The peer requests a quote from the node's mint,
  pays the invoice, and receives byte-vouchers (NUT-04). This needs the peer
  to have some connectivity already — another peering, a cellular link, or
  the minimum flow allowance.
- **Swap locally.** The peer offers sat-denominated tokens and the node
  issues byte-vouchers in return, if it wants those sats. The node has
  upstream connectivity and can verify them.

Neither needs protocol support. In the second case the node is acting as a
market participant, not executing a protocol phase.

**This is a market problem, not a payment protocol problem.** Moving it
there removes a whole subsystem: the bootstrap state machine, its message
types, its mint-verification path, and the `bootstrap` config block. The
peer state machine loses `bootstrap_received` and starts at channel
establishment.

What it costs: the design stops guaranteeing that a peer holding only sats
can walk up to any node and get connected. The local swap recovers that for
nodes choosing to offer it, but it is no longer something every node must
implement — which also means it is no longer something every constrained
device must implement.

Paying per token for a whole session instead of opening channels still
works, but the cost moves to the provider. Every interval's payment lands in
the issuer's spent-proof set, which is the growth channels exist to avoid.

---

## Effect on Negative Pricing

The adopted design treats negative pricing as a core mechanism
([tollgate-pricing.md](tollgate-pricing.md)). Under vouchers it does not
disappear. It **moves off the traffic leg and onto the voucher leg**, where
it cannot be abused.

B accepting A's vouchers *is* a negative price: B is paid, in B-vouchers, to
take something it did not ask for. That is the same shape as the adopted
design's "pay a peer to accept" — but applied to vouchers instead of
traffic.

The difference is what can be checked. Whether B took the vouchers is a
fact, and B gains nothing by taking them and lying about it. Whether B did
anything useful with traffic is not a fact anyone can establish, which is
why the same arrangement is dangerous there.

**The hazard was never the negative sign. It was pricing acceptance of
something whose acceptance can be faked.**

Everything below follows from moving it to the leg where acceptance is
verifiable.

A pays B in **B's vouchers**, acquired either from B directly or on a
market. Either way they exist only because B issued them, and B issues
against capacity someone believed it would deliver.

A node whose service nobody wants therefore has no vouchers in circulation
and **cannot be paid at all**. What you pay with is itself a filter against
peers that discard traffic and against fake identities. And a node receiving
its own vouchers back is only canceling a claim it previously sold — worth
what someone paid for it, which required believing it would deliver. The
subsidy can never exceed what the issuer already earned by selling the
vouchers in the first place.

Re-reading the adopted design's negative-price cases:

| Adopted case | Under vouchers |
|---|---|
| Leaf pays peer to carry its outgoing traffic | The leaf is *buying uplink*. Ordinary positive payment. The sign flip existed only because the design prices the **direction bytes travel** rather than **who the service is for**. |
| Node pays peers to attract resources | The node quotes a positive price for its peer's vouchers and buys them, because it wants what that peer delivers. The cost is bounded and deliberate rather than set by a formula the peer can manipulate. |
| Same-operator pair, zero price both ways | Issue each other vouchers freely, or keep the zero-price shortcut. Unchanged. |

### Subsidy Is Paid For, Not Given Away

Issuing your own vouchers is not free. Under paid acceptance you **pay to
have your vouchers taken** — the `m` component is the real transfer, and how
far below zero B prices them is what B charges for taking on that risk. A
node with unwanted capacity does not subsidize by printing. It subsidizes by
buying its peer's vouchers.

Two things follow that a sat-denominated subsidy does not give you:

**Subsidy and revenue use the same vouchers.** A node cannot increase its
subsidy without making the vouchers its paying customers hold worth less.
Issuing beyond capacity dilutes every outstanding claim, including the ones
it sold for real money, so overissuing punishes the issuer directly rather
than merely being detectable.

**There is no wallet to drain.** A sat-denominated subsidy drains because
the paying node funds an outgoing channel that rolls over automatically —
an unattended loop limited only by wallet balance (see
[tollgate-configuration.md](tollgate-configuration.md)). Under paid
acceptance there is no standing outgoing channel to refill. Every subsidy
payment is a deliberate purchase of the peer's vouchers, in a fixed amount,
at a quoted voucher price. The worst case is a subsidy that does not work, not an
empty wallet, and that holds without the operator configuring anything
correctly.

Two problems remain — selling vouchers without redeeming them, and too many
redemptions arriving at once. Both are in Open Problems below.

### Status of the Resolution

The general point holds whether or not the full voucher model is adopted:
**negative pricing is a symptom of pricing the wrong thing.**

The decided direction is both parts together — price the service so whoever
the service is for pays for both directions, and settle the one genuinely
negative case with vouchers, at a voucher price the peer quotes. This is recorded
as future work in the Negative Pricing section of
[tollgate-pricing.md](tollgate-pricing.md), because the first part changes
what the meter counts and reaches into the ResourceAdapter and the
reconciliation path.

The first part can be adopted **without** vouchers. It is a metering and
pricing change against the existing sat model, and only the subsidy half
depends on this proposal.

The specific defects the analysis turned up have been fixed in place in the
meantime: the negative-pricing constraints in
[tollgate-pricing.md](tollgate-pricing.md), the deliverer-favoring metering
rule in [tollgate-metering.md](tollgate-metering.md), and the subsidy
budgets in [tollgate-configuration.md](tollgate-configuration.md).

### The Case That Cannot Be Removed

Negative pricing survives for **getting rid of surplus** — a node with more
of something than it can use, paying others to take it. It cannot pay in its
own vouchers, because those vouchers are a claim on the very thing it is
trying to shed.

For network forwarding this does not really come up; every apparent example
turns out to be ordinary buying seen backwards. For **electricity it is
real** — grids do go to negative prices — and `tollgate-core` is
resource-agnostic, so it is in scope.

Electricity is also the safe case. The meter is physical and the resource is
conserved, so whether it was actually consumed can be measured, which is
never true of forwarded bytes. Negative pricing is dangerous exactly where
the resource can be quietly thrown away.

---

## Rate Auction and Token Bucket

**This section does not depend on vouchers and can be adopted against the
existing sat-denominated design today.** It is the most useful part of the
proposal and the cheapest to build.

Instead of a fixed `bandwidth_limit` in product extensions, let the payment
set the allowance: what a peer pays during one interval determines the
capacity it gets in the next. Each interval becomes a small auction for the
link.

Under vouchers this is the arithmetic from Buying a Rate, read backwards.
The peer hands over `rate × interval` worth, and
that quantity is what sets its allowance for the next interval. Against the
adopted sat-denominated model the same thing works with `cost / price` in
place of the voucher count.

Build it as a **token bucket filled by payment**, not as a hard per-interval
rate cap:

- paid units fill the bucket
- bucket depth sets how much burst is allowed
- drain rate sets sustained throughput

A hard cap recomputed every interval adjusts the rate on the interval
timescale, while TCP adjusts on the round-trip timescale, and the two fight
each other and produce sawtooth throughput. A bucket smooths the same
allocation, allows bursts, and lets the 5 s default metering interval stand
instead of forcing per-second payments and five times the signing load.

This maps onto the existing `extensions.bandwidth_limit` field: the cap
comes from payment history instead of from static configuration.

---

## Minimum Flow Allowance

A small amount of traffic every peer gets without paying. It serves two
purposes:

- **Bootstrap.** A new peer cannot pay until it holds vouchers, and it
  cannot acquire vouchers without connectivity. The allowance breaks that
  circle.
- **Basic access.** An operator may want any peer to be able to do the small
  things — resolve a name, fetch a message — whether or not it is paying.

```yaml
access:
  minimum_flow:
    enabled: false
    bytes_per_interval: 0
```

### Abuse

The allowance is a subsidy, so it can be farmed. Identities are free, so N
of them collect N allowances. Two separate harms follow:

- **Consumption.** N identities draw N allowances of real bandwidth, and one
  machine can run all N over the same physical link.
- **Resale.** Granted vouchers are bearer instruments, so an attacker can
  accumulate and sell them, turning a service subsidy into sats.

Resale could be closed by issuing the allowance P2PK-locked to the receiving
peer, so only that peer could spend it. The obstacle is that a lock only
holds if it survives **every** swap, including change — otherwise the peer
swaps the locked proof for something else and the lock is gone in one step.
Standard Cashu mints do not preserve locks that way, so this needs modified
mint software or a scheme not yet designed. **Future work**, and out of
scope here.

Until then the allowance is unlocked and resale is bounded by economics
rather than cryptography. At a realistic size — a few KB per interval — what
an attacker can farm is worth very little, thinly traded, and issued by one
obscure router, so the effort likely exceeds the return. That argues for
keeping the allowance small; it is not a guarantee.

Consumption is unaffected either way, and still needs an aggregate cap
across all unpaid peers plus a cost to holding an identity — proof-of-work,
a deposit, or an operator allowlist.

Open: whether the allowance accumulates or expires each interval.
Accumulating lets a peer save up for a burst, which is also how an attacker
gathers a large grant before spending it.

---

## Open Problems

| Problem | Notes |
|---|---|
| Cross-mint atomic swap | No working Cashu implementation. Blocks the market, not basic operation — paid acceptance exchanges vouchers inside the peering. |
| Market liquidity | Each issuer's market is small and thin, and anyone making it takes the risk that an anonymous operator stops redeeming. Needed for the reliability signal, not for moving value. |
| Quoting the voucher price | Every node has to price every peer's vouchers, continuously. That is new work for the operator, and a node that accepts vouchers at close to full value quietly builds up vouchers it can never redeem. Refusing foreign vouchers by default helps, but does not say how any other price should be chosen. |
| Relays holding two kinds of vouchers | Multi-hop relays sit between two issuers and must keep rebalancing. Burdensome on constrained devices. |
| Minimum-flow abuse | N free identities draw N allowances of real bandwidth, and one machine can run all N over the same link. Needs an aggregate cap across unpaid peers plus a cost to holding an identity. |
| Locking the allowance | Issuing it P2PK-locked would close the resale route, but a lock only holds if it survives every swap including change, which standard Cashu mints do not do. Needs modified mint software or a scheme not yet designed. |
| Allowance accumulation | Whether an unspent allowance carries into the next interval. Accumulating helps a peer that needs a burst, and equally helps an attacker gather a large grant before spending it. |
| First connection with no connectivity | With bootstrap removed, a peer holding only sats and having no other link depends on a node choosing to swap sats for vouchers locally. Nothing in the protocol guarantees one will. |
| Withdrawing the trust claim | [tollgate-intro.md](tollgate-intro.md) claims a cryptographic guarantee that vouchers cannot provide. |
| Voucher expiry, operator shutdown | What happens to outstanding vouchers when an operator turns the node off. Unspecified. |
| **Too many redemptions at once** | A voucher says how much but not when. Capacity is a rate and it is finite, so everyone redeeming at the same time can exceed it without the issuer doing anything wrong. Vouchers need a time element — an expiry, a validity window, or a queue. |
| **Selling vouchers without redeeming them** | An issuer can hand vouchers to an accomplice, sell them for sats, and never redeem any. Reputation only corrects this if failures to redeem are visible to others, and nothing in the design makes them visible. Some way to report this is a prerequisite for reputation to work at all. |
| Direction classes and vouchers | If pricing carries a rate per direction class, a voucher has to say which class it claims — a KiB of scarce uplink is not a KiB of downlink. Affects how interchangeable vouchers are, and how thin each market gets. |

---

## Staged Adoption Path

The four pieces are separable and should not be adopted together.

1. **Rate auction and token bucket**, and **service pricing with direction
   classes** — no protocol upheaval, both work against the adopted sat
   model, most value for least risk. Service pricing is the larger change,
   since it alters what the meter counts, but it does not depend on this
   proposal at all. Adopt first.
2. **Vouchers as an optional product type** — a node sells "1 GiB of my
   capacity" for sats alongside its ordinary priced products, and accepts
   either. The provider gets free redemption and can sell capacity ahead of
   time; the consumer gets a fixed amount. No market required.
3. **Paid acceptance** — peers quote a voucher price and exchange vouchers
   inside the peering. This is what turns vouchers from a product into the
   way payment works, and it does not need a market. Gated on stage 2
   showing that anyone wants to hold voucher paper at all, and on an answer
   to how the voucher price gets chosen.
4. **Market and reliability signal** — optional, and last. It buys the
   operator-reliability signal, which is the original motivation for the
   whole proposal but the part with the weakest story for how it gets built.

Making vouchers the main way payment works is a stage-3 decision. It is
*not* gated on the market, since paid acceptance moves value without one, but
it is gated on the trust-model objection above, which no stage resolves.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Status | Proposal only; the adopted model stays sat-denominated | The main objection, loss of zero-trust payment, is unresolved |
| Terminology | "Voucher" is an explanatory name, not a protocol term | The protocol object is a Cashu token holding byte-denominated proofs. Nothing new is defined; the word only names what such a token means |
| Denomination | The byte for network forwarding; each resource's own quantity unit otherwise | A proof carries an integer amount in its keyset's unit, so this is what the existing wallet already handles |
| Buying a rate | `voucher_amount = desired_rate × interval` | The interval is fixed for the peering before payment starts, so a desired rate converts to an amount by multiplication. Nothing needs a rate-denominated instrument, and paying more in one interval buys a higher rate for the next |
| Token vs proof | One token settles one interval; the proofs inside it are the power-of-two pieces its amount decomposes into | Only the proof count changes with amount, and the spent-proof set records proofs — which is what makes channels necessary |
| Proof amounts | Powers of two, as in any Cashu keyset | Nothing about the existing wallet or keyset machinery changes; a payment is a set of proofs that split and combine normally |
| Unit | Fixed by the resource, one per resource, and always a quantity — watt-hours, not watts | Every node selling the same resource denominates the same way, so the selling price of a voucher is a reliability signal rather than an exchange rate |
| Network unit | The byte | Metering is exact and no payment rounds. The cost is proof count: a 23-bit interval amount takes ~11–12 proofs, which is what the spent-proof set has to absorb |
| Normal operation | Pay in the vouchers of whoever is delivering; each direction priced and paid on its own | Covers the large majority of peerings with one payment and no exchange. A leaf simply has no second payment, rather than a zero or negative one |
| Acquiring vouchers | On a market, or directly from the issuer | The direct route is enough to operate, so no market is needed to move value, and checking a voucher takes one hop to its issuer |
| Paid acceptance | The exception: pay a peer to hold your vouchers when it has no use for them | Covers a leaf paying for its own upload, and the "attract resources" case at the opposite sign. The negative price sits entirely in the voucher price, so the payment protocol runs both directions at positive prices and needs no change |
| Subsidy funding | The payer buys the peer's vouchers deliberately, in a fixed amount | No standing outgoing channel to refill, so an unattended drain cannot start |
| Acceptance price | One price per peering, crossing zero — negative (peer buys), zero (even swap), positive (paid acceptance), refused | Normal operation and paid acceptance are the same price at different points, so leaf nodes need no separate rule |
| Foreign vouchers | Refused by default | The default is then plain normal operation; otherwise a node builds up vouchers it cannot redeem |
| Minimum flow allowance | Granted as ordinary unlocked vouchers; keep it small | Locking it would need a mint that preserves locks through swaps and change, which standard Cashu does not do. Small size bounds the resale value economically instead |
| Bootstrap tokens | Removed from the protocol | Provider-as-mint dissolves the reachability problem the mechanism existed for. Acquiring vouchers becomes a wallet and market concern, exactly like acquiring sats today |
| Acquiring vouchers | Lightning mint quote, or a local swap of sat tokens with a node willing to take them | Neither needs protocol support, so the bootstrap state machine, its messages, its verification path and its config block all come out |
| Voucher acceptance vs traffic acceptance | Priced separately, never merged | Whether vouchers were accepted can be checked; whether traffic was accepted cannot, and merging them brings back the discard abuse |
| Subsidy cost | Buying the peer's vouchers, not giving your own away | Bounded and deliberate, and there is no standing outgoing channel to refill |
| Spilman under vouchers | Kept, to bound the issuer's database rather than to prevent theft | The spent-proof set is ~700× larger without channels |
| Cryptographic effort | Concentrate on the exchange step | The only step with a real adversary once the issuer redeems its own vouchers |
| Negative pricing | Moved from the traffic leg to the voucher leg, not removed | The hazard was pricing acceptance of something whose acceptance can be faked, not the negative sign itself. Whether a peer took vouchers can be checked; whether it forwarded traffic cannot |
| Service pricing | Adoptable without vouchers | It is a metering and pricing change against the existing sat model |
| Negative pricing, remaining case | Getting rid of surplus, for a conserved and physically metered resource | Bytes can be quietly discarded; watt-hours cannot |
| Rate auction / token bucket | Adopt on its own, against the existing sat model | No dependency on vouchers, and it avoids fighting TCP congestion control |
| Adoption order | Rate auction and service pricing → optional voucher product → paid acceptance → market | Each stage produces evidence for the next; the market is last and optional |
