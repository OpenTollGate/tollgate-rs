# TollGate Vouchers

This document specifies what TollGate peers pay each other with, and what
delivery costs. [tollgate-payment-channels.md](tollgate-payment-channels.md)
covers how payments are batched;
[tollgate-hazards.md](tollgate-hazards.md) covers what must never be priced.

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

Each node runs its own mint and issues vouchers against its own capacity.
Two peers settle the exchange rate between their vouchers themselves, with an
optional market on top for price discovery.

---

## Why

Pricing delivery directly means pricing it privately, between each pair of
peers. A peer cannot compare two providers without connecting to both, and
nothing anywhere prices an operator's *reliability* — a node that takes
payment and delivers poorly is noticed only by the peer that already paid it.

Vouchers take pricing out of the protocol entirely and put price discovery
somewhere any node can read it. What an issuer's vouchers fetch is a public,
continuously updated measure of what the network expects it to deliver.

---

## Delivery Costs One Voucher Per Unit

A node collects vouchers from a peer and delivers the same number of units.
The exchange is one-to-one by construction: a voucher *is* a claim on one unit
of capacity, so redeeming `n` of them is delivering `n` units.

```
units delivered = n
vouchers collected = n
```

**There is no price for delivery anywhere in the protocol** — no rate to
quote, no product to select, no multiplier to apply. What a unit costs in
money is settled where a peer acquires vouchers, which the protocol never
sees ([market/](../market/README.md)).

Three consequences:

- **A node's price is what its vouchers sell for.** A node that wants more
  revenue per unit sells its vouchers dearer. It renegotiates nothing with
  its peers.
- **Per-peer pricing needs no per-peer machinery.** Favoring a peer means
  selling that peer vouchers more cheaply, or giving them away.
- **Delivery cannot be repriced mid-session.** The peer already holds the
  vouchers and their claim is fixed.

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

### Two Accounting Modes

A proof holds a quantity and nothing else. To turn it into service, the payer
pays in **grants**: a number of units, paired with a **window** the payer
names. The window sets the grant's **deadline**, and whatever is left of it at
the deadline is forfeit. That is the same for every provider.

What differs is how the provider keeps its accounts with the payer, and a
provider does it in one of two ways. That choice is its **accounting mode**,
and it changes two things only:

| | **Superseding** (the default) | **Accumulative** |
|---|---|---|
| A new grant | Replaces the one in force. What was left of it is forfeit | Adds to what is left, and moves the deadline to now plus the new window |
| The speed | `grant / window`, bought by the payer | A cap the provider sets per peer |

Why two: some buyers want speed and some want volume. A router reselling its
uplink is selling speed. Capacity it does not sell this second is gone, so it
wants buyers to pay for the second, not for a stock of bytes to use later.
That is superseding, with windows of seconds, and it is the default because it
is the safe one
([tollgate-hazards.md](tollgate-hazards.md#unspent-capacity-must-expire)). A
phone on a data pack wants the opposite: pay for 1 GB, use it over the month,
and lose nothing by being idle for an hour. That is accumulative, with windows
of weeks.

The provider picks the mode, per node and if it likes per peer, and says which
in its Offer ([tollgate-protocol.md](tollgate-protocol.md#0x01-offer)). The
payer does not choose. It buys in the mode it is offered, or it does not buy.
The mode is fixed for the session.

Everything else is the same in both modes: one voucher per unit, the same
channels, the same TopUp message with the same window, the same deadline, the
same metering, and the same enforcers.

Selling **time** — an hour at a fixed speed — is not a third mode. It is
superseding with the buyer holding one rate. See
[Selling Time](#selling-time).

### Buying a Rate

In superseding mode the buyer wants a rate, so the rate is expressed by pairing
a quantity with a **window** — a span of time the buyer names, inside which
that quantity may be drawn:

```
rate = grant / window

  5,120 bytes over 5 s      1 KiB/s      (2 proofs)
512,000 bytes over 5 s    100 KiB/s      (6 proofs)
512,000 bytes over 1 s    500 KiB/s      (same 6 proofs, shorter window)
```

The window lives in the signed channel state, not in the money. That matters:
a proof denominated in bytes-per-second would only be meaningful against some
particular window, so proofs from different windows would stop being
interchangeable and the accepted-mint set would collapse. Keeping time out of
the instrument is what lets a byte be a byte wherever it is spent, and what
lets vouchers trade at a single price on a market.

Amounts are ordinary Cashu amounts needing no special handling:
`5,120 = 4096 + 1024`, and `512,000 = 125 × 4096` where 125 has six bits set.

### Grants Replace Each Other

In superseding mode a grant is this many units, spendable within this
window, starting when the provider receives it. Buying again **replaces** the
grant in force. Whatever was left of the old one is forfeit at that moment, and
the provider keeps the payment.

```
t=0   buy 6.25 M units, window 5 s      1.25 M/s, expires t=5
t=3   buy   100 M units, window 5 s     20 M/s,   expires t=8
      the 2.5 M left from the first grant burn at t=3
```

This is what makes the product bandwidth rather than a stored quantity of
bytes. Capacity is perishable — a second of it that goes unsold is gone whether
or not anyone paid, and the buyer carries that risk on the seconds it bought.
Without forfeiture, a buyer could accumulate claims off-peak and present them
all at peak, which is selling volume, not bandwidth.

**Raising the rate costs the remainder**, so the exchange rate between
responsiveness and waste is set by window length:

| Situation | Forfeit | As a share of the new grant |
|---|---|---|
| 1.25 M/s, 2 s left, jump to 20 M/s | 2.5 M | 2.5% |
| 20 M/s, 4 s left, jump to 24 M/s | 80 M | 67% |

Large jumps are cheap and small adjustments are punitive, which discourages
fiddling and holds down the provider's verification load. A buyer that wants
fine control uses short windows, where the forfeit can never exceed one
window's worth. That costs more messages, and `min_window_ms` is where the
provider says how many it will take.

**The rate is fixed for the life of the grant** at `grant / window`. Capacity
left unused early is not banked for later — otherwise a buyer that waited would
be entitled to an unbounded burst just before the deadline.

### Accumulative: A Running Budget

In accumulative mode a new grant does not replace what was left of the last
one. It adds to it. The sum is the payer's **budget**: the units it has paid
for and not yet used. Traffic takes units out of it, weighted exactly as in
superseding mode ([The Received Multiplier](#the-received-multiplier)).
Nothing else does, until the deadline.

Each grant moves the deadline to now plus its own window, for the whole
budget. A payer that keeps buying keeps its budget; one that stops loses
what is left at the deadline of its last grant, exactly as a superseding grant
expires.

Take a phone that buys 1 GB from a hotspot that sells in accumulative mode,
capped at 10 MB/s per peer, with a 30-day window:

```
day 0, t=0       buy 1,000 MB, window 30 days    budget 1,000 MB   deadline day 30
t=0 – 30 s       downloads 300 MB at 10 MB/s     budget   700 MB
t=30 s – 1 h     idle                            budget   700 MB   nothing drains
t=1 h            downloads 250 MB                budget   450 MB
day 3            buys 500 MB, window 30 days     budget   950 MB   deadline day 33
day 33           nothing bought since            what is left expires
```

The same purchase in superseding mode, with the 30 s longest window a provider
accepts by default:

```
t=0          buy 1,000 MB over 30 s          33 MB/s until t=30 s
t=0 – 9 s    downloads 300 MB
t=9 – 30 s   idle
t=30 s       the 700 MB left expire          nothing left
```

**The window can be long because the speed is not in it.** In superseding
mode the window divides the grant into a rate, so a 30-day window would buy a
trickle. In accumulative mode the provider shapes each peer to a speed it
configures for that peer, its **rate cap** (`accounting.rate_cap`), or not at
all if it sets none. The window only says how long the budget lasts. As the
budget nears zero the provider also slows the peer so that the budget cannot
be overrun before the next time it reads its counters — see
[Grant State](tollgate-protocol.md#grant-state). At zero the peer falls to the
minimum flow allowance, exactly as when a superseding grant expires.

So an accumulative provider simply accepts much longer windows: its
`max_window_ms` is a month, or a year, where a superseding provider's is
seconds. The bound means the same in both modes — the longest window this
node accepts.

**The budget belongs to the payer, not to the session or to a channel.** The
provider keeps it in its own state and writes it to disk beside its channel
backups, so it survives a reconnect, a channel rollover, a channel settlement
and a restart of the provider, until its deadline. A phone that walks out of
range and comes back still has its 700 MB. The money for it is already in the
provider's hands: the channel updates that bought it are what the provider
settles. So settling a channel changes nothing about the budget, and the
budget does not tie a channel to its window — see
[Safety Margin](tollgate-payment-channels.md#safety-margin).

Besides at its deadline, a budget is also lost when:

- the provider loses the disk it was written to
- the payer comes back under a different identity — a new key, or under
  `enforcer.identity: address` a different address

None of these refunds anything. The units were paid for when they were
bought, and the protocol has no message that hands vouchers back.

**Small budgets, topped up often.** The provider caps how much one peer may
hold unspent with `accounting.max_budget`, and its default is modest: 1 GiB.
A TopUp that would take a peer's budget past it is refused before any money
moves. Small budgets are better for both sides. The payer has less paid in
advance at any one time, so a provider that fails or disappears takes less
with it. The provider owes less service it has already been paid for, and
has less to honor at its busiest hour. A TopUp is one message, so buying
often costs little. A peer that moves large volumes on behalf of others, such
as a proxy buying for its users, can be given a larger `max_budget` of its
own.

**What it costs the provider.** It takes on a liability: units it has been
paid for and not yet delivered, possibly for weeks, and possibly wanted all at
once by every peer at its busiest hour. That is why accumulative mode is a
choice and never the default
([tollgate-hazards.md](tollgate-hazards.md#when-the-window-can-be-long)). The
rate cap bounds how fast a large budget can be spent, and `max_budget` how
large it can be.

### Selling Time

An operator who wants to sell **an hour at 1 MB/s** needs no third mode. A
fixed speed for a fixed time is superseding mode with a buyer that always
buys the same rate and keeps renewing it. The hour is how many vouchers the
buyer holds, not anything the protocol knows about.

```
rate pinned:      1,000,000 bytes/s      (buying.min_rate = buying.max_rate)
window:           30 s                   (the provider's longest)
each grant:       30,000,000 bytes
renewed:          1.2 s before each deadline, so every 28.8 s
forfeit per grant: 1.2 MB, 4%
an hour:          3,600 / 28.8 = 125 grants = 3,750 MB of vouchers
```

The 30 s window does not get in the way. A buyer renews inside it for as long
as it holds vouchers, and the hour is simply 125 renewals. The price of that
is the forfeit on each renewal, 4% here, which the seller can build into what
it charges for "an hour". It drains whether the buyer uses it or not, which is
what selling time means.

In superseding mode, raising `max_window_ms` to the whole hour is not the
answer:

- **The channel would have to outlive it.** The safety margin before a
  channel's expiry is twice the longest window
  ([Safety Margin](tollgate-payment-channels.md#safety-margin)), and a
  channel must live at least twice its margin. A one-hour window means
  channels of at least four hours, and money locked up for that long.
- **The buyer would risk the whole hour.** A grant is forfeit if the link
  drops or the buyer wants to change speed. With 30 s windows the most it
  loses is 30 s; with an hour it loses whatever is left of the hour.
- **The anti-hoarding bound would go.** `max_window_ms` is what stops a buyer
  buying capacity off-peak to spend at peak. An hour-long window spans both.

An operator who still wants longer superseding windows may raise
`max_window_ms`; nothing forbids it. The channel TTL then has to grow with it,
which `tollgated` enforces at startup.

### How a Buyer Buys

A node buys in one mode only: its own `accounting.mode`. It buys from the
providers whose Offer is in that mode, and skips the others, with a line in
its log. The buyer's job is different in each mode, because what it can lose
is different.

**Superseding.** The buyer buys a rate and has to renew it before its deadline,
or the peer falls to the minimum flow allowance. It renews a little before the
deadline (`buying.renew_lead_ms`). Between renewals it only buys again when
demand has risen a lot (`buying.raise_threshold_pct`), because buying early
forfeits what is left. That threshold is hysteresis: it stops the buyer
throwing away a little of every grant to follow every small change in demand.

**Accumulative.** Buying early forfeits nothing, so there is nothing to time.
The buyer watches its remaining budget instead, and tops up when it falls
below a **low-water mark** (`buying.low_water`), by a fixed amount
(`buying.top_up`), with a window it chooses (`buying.budget_window_ms`, no
longer than the provider's `max_window_ms`). It never asks for more than the
provider's `max_budget` leaves room for. It needs no hysteresis: a top-up
wastes nothing, so buying a little early costs only the money held a little
longer. A budget that reaches its deadline is gone, so the buyer also tops up
`buying.renew_lead_ms` before the deadline if something still wants the link.

In both modes it buys only while something wants the link: observed demand,
or the standing `buying.demand`. An idle accumulative buyer above its
low-water mark buys nothing, and loses nothing for it until the deadline.

The accumulative buyer keeps its own count of the budget between purchases,
from what it signed and what it measured crossing the link. The provider
corrects that count with a **Balance** message on every connection and after
each TopUp it accepts
([tollgate-protocol.md](tollgate-protocol.md#0x0c-balance)), so a buyer that
reconnects learns the budget it left behind and when it expires.

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
and they are worth different amounts. What that difference means, and how it
becomes a public signal about an operator, is covered in
[voucher-price-signal.md](../market/voucher-price-signal.md).

Byte denomination makes metering exact, so every payment lands on a whole
number of units. The cost of that precision is proof count: amounts are
powers of two and a payment takes one proof per set bit, so the count scales
with how many bits the number has. A 1 GiB payment is a 30-bit number and
takes up to 30 proofs, about 15 on average. That is what the spent-proof set
below has to absorb.

### Each Side Pays For What It Received

The base rule is unchanged and symmetric: **a node pays for what it received
from a peer**, which is what that peer delivered. Both sides owe each other,
both fund a channel, and each buys its own grants on its own schedule.

```
A buys from B      the right to receive        (A's download)
B buys from A      the right to receive        (A's upload)
```

For an ordinary peering that is the whole story, and it is why **two channels
is the default**, not an exception. The two streams are independent: different
mints, different windows, bought at different moments.

### The Received Multiplier

That leaves one gap. A leaf's upload is something the leaf *delivers*, so by
the base rule the relay pays the leaf for it — which is backwards when the
relay would rather not carry it at all.

A node closes the gap by adding a **surcharge on what it receives** from a
given peer. It is applied as a weight on how fast that peer's grant is drawn
down:

```
B draws down A's grant as:   delivered + received × m_B
A draws down B's grant as:   delivered + received × m_A
```

> **`received_multiplier`** — a surcharge on units I receive from you.
> Default `0`: no surcharge, the base rule stands.

A unit A downloads draws one unit from A's grant. A unit A uploads draws `m_B`.
So a peer that wants to push a lot has to buy a bigger grant, and the shaper
enforces it as the traffic happens rather than a bill arriving afterward.

Netted out on the leg where A uploads `X` to B, the two grants partly cancel —
B is still buying that upload from A at one unit each:

| `m_B` | B pays A | A pays B | **Net per unit A uploads** |
|---|---|---|---|
| `0` (default) | `X` | — | **B pays 1×** — the traffic is wanted |
| `1` | `X` | `X` | **zero** — upload is free to A |
| `2` | `X` | `2X` | **A pays 1×** — upload costs the same as download |
| `10` | `X` | `10X` | **A pays 9×** |
| `11` | `X` | `11X` | **A pays 10×** — matches a 10:1 backhaul |

**The net rate is `m − 1`, not `m`.** To charge uploads at `k` times the rate
of downloads, set:

```
received_multiplier = k + 1
```

An operator that wants 10× uplink and sets `10` gets 9×. The off-by-one is the
price of the field being unsigned, and it is worth the trade: with no negative
value available, a node can decline traffic as hard as it likes but can never
pay a *bonus* on top of what it already owes for delivery. `0` is the floor,
and `0` is simply the base rule with no surcharge at all.

![Each Side Pays For What It Received](diagrams/who-pays.svg)
<details><summary>Text version</summary>

```
Ordinary peering, both multipliers 0:
  A's grant drains on A's downloads      both buy, two channels
  B's grant drains on B's downloads

Relay charging upload at the same rate as download, m_B = 2:
  A's grant drains on A's downloads + 2 × A's uploads
  B's grant drains on A's uploads
  net: A pays 1× for each, in both directions

Relay on a 10:1 backhaul, m_B = 11:
  net: A pays 1× per unit down, 10× per unit up
```
</details>

It is **per peer**, so a node can welcome one peer's traffic and discourage
another's. A grant already bought keeps the multiplier it was bought under; a
revised Offer takes effect on the next one. In accumulative mode every grant
adds to one budget, so there the multiplier is fixed for the session, and a
change takes effect at the payer's next session — including on a budget
carried into it.

**The field is unsigned, and that is load-bearing.** A negative surcharge would
mean paying a peer *on top of* already paying for its delivery — compounding
into the sink hazard, where generating unwanted traffic becomes profitable
([tollgate-hazards.md](tollgate-hazards.md)). Zero is the floor, and zero
already means "I am happy to pay you for this."

---

## Accepted Mints

Because the unit is the same everywhere, a node is not limited to its own
vouchers. A byte-voucher from any mint claims one byte; only the issuer
differs. **A node advertises a list of mints whose vouchers it will take**, in
its Offer, ordered by preference and never empty.

This is a merchant accepting several banks' notes: one unit of account,
several credits, each worth what its issuer is worth.

```yaml
# what this node will take, best first
mint A (upstream provider)     we can spend these onward
own mint                       we issue these
mint C (well-known hub)        widely held
anything else                  refused
```

Order is a preference, not a rule: a payer that holds vouchers from more than
one accepted mint should reach for the earliest, and a node that lists only
one mint has said everything it needs to. Its own mint has no special place —
the relay above would rather be paid in the vouchers it owes upstream.

What happens to another mint's vouchers after settlement is set per mint in
`tollgated`: **keep** them, swapped at their mint and deposited with
`merchantd`, or **burn** them at their mint where they are worth nothing to
this node ([tollgate-daemons.md](tollgate-daemons.md#other-mints-keep-or-burn)).
The node's own vouchers are always burned once delivered.

**Accept or refuse is binary — there is no haircut.** What an issuer's paper
is worth is expressed in what you pay for it on the market, not in a discount
applied at settlement. Taking a mint on means taking its credit, so the
decision to accept it at all is where an operator weighs issuer risk
([issuer-risk.md](../market/issuer-risk.md)).

### What This Buys

**Relays stop holding a currency position.** A relay that buys upstream
transit from A can accept A-vouchers from its downstream customers and spend
them upstream. Settling the channel swaps them at A, `merchantd` keeps the
proceeds, and they come back to `tollgated` the next time it funds a channel
to A ([tollgate-daemons.md](tollgate-daemons.md)). No market swap, no spread,
no rebalancing. This was listed
below as a real cost of the design; multi-mint acceptance removes most of it,
because the vouchers a relay wants to receive are exactly the ones it needs
to pay with.

**Buyers stop swapping per peering.** A client holding vouchers from a mint
several nodes accept can move between them without acquiring anything new.

**Hub vouchers become usable as common currency.** A well-regarded mint's
paper gets accepted broadly because accepting it is cheap and useful, and it
starts functioning as money for the network — without anyone designating it,
and without the protocol knowing. The N-currency problem softens on its own
wherever demand concentrates.

**Market spreads narrow.** Every mint a node accepts is one fewer swap
somebody has to pay for.

### What It Costs

The three properties that make own-voucher redemption cheap hold **only for
own vouchers**:

| Property | Own vouchers | Foreign vouchers |
|---|---|---|
| Settlement cost | Free — burned at `mintd`, which cancels a claim | Settled at that mint, then kept or burned there |
| Double-spend check | Lookup at `mintd`, on the same machine | Requires reaching that mint |
| Payment liveness | Equals service liveness | Depends on that mint being reachable |

**The accepted set is exactly what the operator lists** — any mint, a
neighbor's or not. Nothing about where a mint sits in the network puts it on
the list or keeps it off. Each one listed costs two things:

- **Reachability**, always: the node verifies funding and settles at that
  mint, so it must be reachable before a channel funded in it expires.
- **Credit**, only for a mint set to `keep`: the node holds that issuer's
  paper — in `merchantd` — until it is spent or redeemed. A mint set to `burn`
  holds nothing, so its issuer's credit does not matter; accepting it is free
  service, rationed by how much of that issuer's paper the peer can get.

Unsettled channels carry both, for any foreign mint, until they settle.

---

## Normal Operation

One Cashu token buys one grant. Which node's keyset it was minted against
says who owes the delivery; the amount says how much. There is no extra
structure — a wallet that can hold sat-denominated proofs can hold these.

Each direction of a peering is bought on its own. **You pay in the vouchers
of whoever is delivering**, one voucher per unit.

![Voucher Lifecycle](diagrams/voucher-lifecycle.svg)
<details><summary>Text version</summary>

```
  1. Node B issues vouchers against its own capacity
  2. Client A acquires B-vouchers — on a market, or directly from B
  3. A spends B-vouchers with B
  4. B delivers the service, settles, and burns the vouchers at its
     own mintd: the claim is honored, so it is cancelled

  Redemption costs B nothing but the service it already sells.
```
</details>

The same thing happens in the other direction, in the other node's vouchers,
bought separately and on a different schedule. A leaf is no exception: it
delivers its uploads, so its relay buys those from it, and both channels exist.
What makes the leaf a net payer is the relay's received multiplier, not a
missing channel.

That is the whole mechanism for the large majority of peerings. Vouchers can
be acquired **on a market or directly from the issuer**, and the rest of
this document only needs the direct route.

---

## What Improves

**Settlement is free for the provider.** A node receiving its own vouchers
is canceling its own claim: `tollgated` settles and burns at its own `mintd`,
over a local socket. No foreign mint round-trip, no trust in a foreign mint,
nobody to rely on. All the work moves to whoever acquires the vouchers, who
can spread it over many payments.

This applies to the redemption step. A node that also accepts its peers'
vouchers from other mints takes on those issuers, bounded by how many it holds
at once.

**Double-spend checking becomes local.** Today a provider must reach a
third-party mint to confirm a token is unspent, and that network hop is
Spilman's main justification. Under vouchers the provider decides on its own
vouchers: a lookup in its own `mintd`'s database, on the same machine,
sub-millisecond, and it works during an outage.

**Payment works whenever service works.** Today a mint outage blocks
funding, rollover, and settlement even though the link itself is fine
([tollgate-payment-channels.md](tollgate-payment-channels.md), "What Needs
the Mint"). When the provider is the mint, the two fail together — if you
can be served, you can pay.

**A fixed amount for the consumer.** A voucher's claim is a quantity, fixed
when it is acquired. No exposure to the sat price mid-session and no price
sheet changing underneath. What is *not* fixed, in superseding mode, is when it
must be used: a grant carries a deadline, and capacity left unspent behind it
is gone. In accumulative mode the deadline is weeks away rather than
seconds, and moves with every purchase.

**Selling capacity ahead of time.** Issuing vouchers is selling capacity
before delivering it, which is a way to raise funds. A rooftop antenna can
pay for itself against next month's bytes.

**Honest about what it is.** If a node is going to issue a claim at all, a
voucher is the more honest form than node-issued sat-denominated ecash. It
says outright that redemption is service, and promises nothing about being
spendable anywhere else.

---

## Accepted Limitations

These are the costs of the design, accepted deliberately. They are not
resolved.

**Payment is not zero-trust.** What protects a sender from a receiver who
takes payment and disappears is Spilman's time-locked refund, and the mint
is what honors it. When the provider is the mint, the only party who can
cheat is also the party that would have to honor the refund. It will simply
refuse.

No cryptography fixes this. Two things limit it instead:

- **Policy** — the loss is however many vouchers are held or committed at
  once. Hold one grant's worth, risk one grant's worth, and the window is
  the payer's own choice. In accumulative mode the payer risks its whole
  unspent budget, which `accounting.max_budget` bounds.
- **Reputation** — an issuer that stops redeeming sees its vouchers sell for
  less, which makes everything it issues later worth less too.

That is a workable security model, but it rests on reputation rather than
cryptography. [tollgate-intro.md](tollgate-intro.md) states it in those
terms; any claim of a cryptographic guarantee against a defaulting issuer
would be false.

**Getting hold of vouchers is continuous, not one-time.** Sats can be loaded
in advance from anywhere. Vouchers cannot, because you do not know which
node you will meet. Every new peering needs vouchers acquired before it can
pay for anything. That is not a protocol concern (see Acquiring Vouchers
below), which keeps the protocol small but leaves the peer to solve it — via
another link, a Lightning payment, or a node willing to swap sats locally.

**Relays can end up holding two kinds of vouchers.** A relay paid in its own
vouchers burns them on delivery, and still needs upstream vouchers to pay
onward — which `merchantd` has to buy with what its sales brought in.
Keeping that conversion going is real work on an ESP32.

Two things reduce this, and together they mostly remove it. Accepting the
upstream's mint (see Accepted Mints above) lets the relay take downstream
payment in exactly the paper it needs to spend, so nothing has to be
converted.

What remains is the relay that accepts only its own mint and has no upstream
overlap — an operator choice rather than a structural cost.

**A market needs liquidity that may not appear.** Each issuer is its own
small, thin market, and cross-mint atomic swap has no working
implementation. None of that blocks operation — vouchers can be bought
directly from the issuer. It blocks the **reliability signal**, which is the original reason
for the design and the part least likely to arrive on its own. See
[voucher-price-signal.md](../market/voucher-price-signal.md) and
[voucher-acquisition.md](../market/voucher-acquisition.md).

---

## Spilman Channels

Channels are still needed, but **for a different reason: keeping the
issuer's database small, not preventing theft.**

Checking for double spends locally is cheap, but every spent proof has to be
recorded in the issuer's spent-proof set permanently. That is one record per
**proof**, not per payment — and because amounts are powers of two, a
payment is a set of proofs, roughly one per set bit. Byte denomination makes
those numbers large: a grant of 1 MB/s over a 5 s window is 5,242,880 bytes, a
23-bit number, so about 11–12 proofs per payment.

![Spilman as State Compression](diagrams/voucher-state-compression.svg)
<details><summary>Text version</summary>

```
10 peers, 5 s windows, 1 MB/s each:
  86400 / 5 × 10              = 172,800 grants/day
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

- The spent-proof set needs an atomic check-and-set. It lives in `mintd` alone
  ([tollgate-daemons.md](tollgate-daemons.md)), so that is one process's
  database transaction — but it has to be specified.
- Cryptographic effort belongs on the **exchange step**, where two parties
  who do not trust each other trade vouchers and neither one issued what the
  other is handing over. That is the only step with a real adversary, and
  the one with no implementation today.

---

## Acquiring Vouchers

**How a peer came to hold vouchers is not the protocol's business**, any
more than how it came to hold sats. It arrives holding vouchers for the node
it wants service from, or it does not get service.

The routes — buying over Lightning or directly from the issuer's `merchantd`,
local swaps of sat tokens, cross-mint swaps — are covered in
[voucher-acquisition.md](../market/voucher-acquisition.md), along with why the
protocol needs no mechanism for handing a peer its first vouchers. The
issuer's `mintd` takes no money; issuing is `merchantd`'s, or free where the
mint auto-accepts ([tollgate-daemons.md](tollgate-daemons.md)).

The one thing worth noting here: a peer can buy, swap and fund against its
counterparty **over the peering link alone**, because the issuer it needs is
the peer it is already talking to. Mint reachability is never the obstacle.

Inside a node, acquiring is `merchantd`'s job. `tollgated` holds no vouchers:
when it funds a channel it asks `merchantd` for the counterparty's
([tollgate-daemons.md](tollgate-daemons.md#funding-upstream)).

Paying per token for a whole session instead of opening channels still
works, but the cost falls on the provider. Every grant's payment lands in
its spent-proof set, which is the growth channels exist to avoid.

---

## Paying Before Delivery

A grant is bought **before** the traffic it covers, not billed afterward. Three
things follow.

**It adds no trust.** Holding an unredeemed voucher is already a claim on the
issuer — that exposure exists the moment a peer acquires vouchers at all.
Prepaying converts a claim into a claim of the same size against the same
party. Billing afterward would instead have the provider extend credit *on top*
of the payer already holding its paper: two exposures where one will do.

**Non-payment enforces itself.** A peer that stops buying simply runs out of
grant and drops to the minimum flow allowance. Nothing has to detect it, no
message announces it, and there is no delivered-but-unpaid balance to chase.
The provider's exposure is not one interval of traffic — it is nothing.

**Reaction is one message, not one interval.** Under postpaid settlement a
rate could only change at a settlement boundary, so a traffic spike waited out
the remainder of the interval. A grant takes effect when it arrives. Because
the signed state is cumulative, a TopUp needs no acknowledgment, so a payer can
send one and immediately start using the rate it just bought; the worst case is
a single round trip of shaping at the old rate. On an adjacent link that is
sub-millisecond.

What the payer gives up is the ability to pay only for what actually arrived.
A grant is consumed whether or not packets land, so transit loss is the buyer's
cost. It remains **measurable one-sided** — the payer knows what it bought and
what arrived, both locally — so the correction needs no protocol and no
cooperation: buy a smaller grant, or buy somewhere else. Delivered against
purchased is a per-provider score that informs which peerings are worth keeping
([tollgate-metering.md](tollgate-metering.md)).

---

## Minimum Flow Allowance

A small, free rate of traffic every peer gets without paying. It serves three
purposes:

- **Getting started.** A new peer cannot pay until it holds vouchers, and
  it cannot acquire vouchers without connectivity. The allowance breaks that
  circle.
- **Basic access.** An operator may want any peer to be able to do the small
  things — resolve a name, fetch a message — whether or not it is paying.
- **Keeping a link alive between grants.** A grant expires and the next one
  has not arrived yet. Without an allowance the link would go silent, and the
  peer would have no way to send the TopUp that revives it.

```yaml
access:
  minimum_flow:
    enabled: false
    bytes_per_second: 0
```

It is a rate, not a stored quantity, and it is what a peer falls back to
whenever its grant is exhausted or expired. That makes it the floor of the
shaper rather than a separate mechanism.

**No vouchers are issued for it.** The allowance is delivery the node gives
away, not a payment it makes: nothing is minted, nothing changes hands, and
nothing is drawn from a grant. Handing out free vouchers is a way of selling
them, and so belongs to `merchantd` and the market
([../market/README.md](../market/README.md)), or to `mintd`'s auto-accept
([tollgate-daemons.md](tollgate-daemons.md#auto-accept)) — not to the protocol.

### Abuse

The allowance is given away, so it can be farmed. Identities are free, so N of
them collect N allowances: N identities draw N allowances of real bandwidth,
and one machine can run all N over the same physical link.

Because the allowance is a rate and never a token, there is nothing to resell:
an attacker can use the bandwidth while connected, but cannot carry any of it
away or turn it into sats.

Consumption still needs an aggregate cap across all unpaid peers plus a cost
to holding an identity — proof-of-work, a deposit, or an operator allowlist.

The allowance does not accumulate. It is a floor on the shaping rate, so an
unused second of it is gone the same way an unsold second of capacity is —
there is nothing to save up and nothing for an attacker to gather before
spending.

---

## Open Problems

Problems belonging to the market layer — cross-mint swap, liquidity,
selling without redeeming, redemption congestion, operator shutdown — are
tracked in [issuer-risk.md](../market/issuer-risk.md) and
[voucher-acquisition.md](../market/voucher-acquisition.md). What remains
here is protocol-side.

| Problem | Notes |
|---|---|
| Choosing a received multiplier | Every node has to decide, per peer, how much to surcharge what that peer pushes at it. New operator work with no obvious default beyond `0`. |
| Relays holding two kinds of vouchers | A relay that does not accept its upstream's mint burns what it is paid and has `merchantd` buy the upstream's paper with its sales revenue, continuously. Accepting the upstream's mint with `keep` removes the problem; not every relay can. |
| Minimum-flow abuse | N free identities draw N allowances of real bandwidth, and one machine can run all N over the same link. Needs an aggregate cap across unpaid peers plus a cost to holding an identity. |
| Choosing a window | The payer trades responsiveness against forfeiture and message count, with no obvious default. A provider's `[min_window_ms, max_window_ms]` bounds it but does not choose it. |
| Multiplier on a carried budget | In accumulative mode the received multiplier is fixed for the session, but a budget carried into a later session is drawn down at that session's multiplier. A provider that raises it between sessions reprices units it has already sold. |
| Claiming someone else's budget | Under `enforcer.identity: address` a key is not proven, so a carried budget is restored only to the same key from the same address. A device that takes over a departed payer's address and announces its key still gets its budget. Under `pubkey` the network proves the key and this does not arise. |
| Spending ceiling in accumulative mode | `buying.max_rate` caps what a superseding buyer spends per second. An accumulative buyer tops up by `buying.top_up` whenever it falls below its low-water mark, and nothing caps what that adds up to over a day except the wallet behind it. |
| Under-delivery has no public evidence | A payer measures delivered against purchased from its own counters and can act on it, but cannot show it to anyone else. A provider skimming a few percent from every peer stays invisible outside those peerings. Revisitable as a reporting path if it proves common. |
| Admission control policy | A provider can refuse a grant that would oversubscribe, but nothing says how it should divide capacity between peers that all want more, or whether an existing grant may be honored at a reduced rate rather than run to its deadline. |
| Atomic spent-proof check | The spent-proof set lives in `mintd` alone, so check-and-set is one process's transaction. Straightforward, but unspecified. |

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Terminology | "Voucher" is an explanatory name, not a protocol term | The protocol object is a Cashu token holding byte-denominated proofs. Nothing new is defined; the word only names what such a token means |
| Denomination | The byte for network forwarding; each resource's own quantity unit otherwise | A proof carries an integer amount in its keyset's unit, so this is what the existing wallet already handles |
| Proof amounts | Powers of two, as in any Cashu keyset | Nothing about the existing wallet or keyset machinery changes; a payment is a set of proofs that split and combine normally |
| Token vs proof | One token buys one grant; the proofs inside it are the power-of-two pieces its amount decomposes into | Only the proof count changes with amount, and the spent-proof set records proofs — which is what makes channels necessary |
| Unit | Fixed by the resource, one per resource, and always a quantity — watt-hours, not watts | Every node selling the same resource denominates the same way, which is what makes one issuer's vouchers comparable to another's |
| Network unit | The byte | Metering is exact and no payment rounds. The cost is proof count: a 23-bit grant amount takes ~11–12 proofs, which the spent-proof set has to absorb |
| Buying a rate | A grant: a quantity paired with a window the payer names. `rate = grant / window` | A proof holds a quantity and nothing else, so time belongs in the signed state. A bytes-per-second keyset would make proofs from different windows non-interchangeable and break the accepted-mint set |
| Payment timing | Prepaid — the grant is bought before the traffic it covers | Holding a voucher is already a claim on the issuer, so prepaying adds no new exposure. Postpaying would add provider credit risk on top of it. Non-payment then enforces itself: the grant runs out |
| Accounting modes | Two, set by the provider per node and per peer, and carried in the Offer: superseding and accumulative | Some buyers want speed and some want volume, and neither model sells the other well. One protocol with a mode keeps the channels, the TopUp, metering and enforcers the same for both |
| One window model | Every grant has a window and a deadline in both modes. The mode decides only what a new grant does to the old one (replace and forfeit, or add and move the deadline) and where the speed comes from (`grant / window`, or a per-peer cap) | One grant shape and one message shape, so the mode is two switches rather than two protocols. The rule that unspent capacity expires holds in both |
| Default mode | Superseding | It is the mode that keeps capacity perishable on a scale of seconds, so it is safe to leave on without thinking. Accumulative trades that away for convenience, which an operator should do on purpose |
| Grant semantics (superseding) | A new grant replaces the one in force; the remainder is forfeit | This is what makes the product bandwidth rather than stored volume. Without forfeiture a buyer accumulates claims off-peak and presents them at peak |
| Grant semantics (accumulative) | A new grant adds to what is left and moves the deadline to now plus its window | Pay for what you use. The hoarding it allows is bounded by a per-peer rate cap, a per-peer budget limit, and still by the deadline |
| Long accumulative windows | Not a separate setting: an accumulative node sets `max_window_ms` much larger, a month or a year | The bound means one thing in both modes, the longest window accepted. In accumulative mode the window no longer sets the speed, so a long one is safe |
| Budget lifetime | Kept per payer in the provider's state across reconnects, rollovers, settlements and restarts, written to disk, until its deadline | The budget is already paid for, so losing it at a reconnect would take a phone's money for walking out of range. The deadline keeps the provider from owing service indefinitely |
| Budget size | `accounting.max_budget`, 1 GiB by default; buyers top up small and often | Less paid in advance means less lost if the provider fails, and less owed at the busiest hour. A TopUp is one message, so buying often is cheap. A proxy moving large volumes for its users can be given a larger limit |
| Speed in accumulative mode | A per-peer rate cap set by the provider, or none; slowed near zero so the budget is not overrun | The window no longer divides out a rate. A cap keeps a large budget from being spent any faster than a small one |
| Selling time | Superseding with a pinned rate, renewed inside the usual windows; not a third mode | A fixed speed for a fixed time is exactly a rate renewed. Raising `max_window_ms` to the interval would force long-lived channels, put the whole interval at risk on a dropped link, and undo the anti-hoarding bound |
| Buying across modes | A node buys only from providers whose Offer is in its own `accounting.mode`, and skips the others | A relay cannot hedge buying in one mode and selling in the other: what it owes and what it holds would expire on different terms. One buyer algorithm per node. A bridge between the modes is possible with two nodes and a balancer between them, or a custom implementation |
| Accumulative buyer | Tops up by a fixed amount below a low-water mark, with a long window of its own choosing; no hysteresis | Buying early forfeits nothing, so there is nothing to time |
| Rate within a grant | Fixed at `grant / window`, not banked (superseding) | Otherwise a buyer that waited would be owed an unbounded burst just before the deadline |
| Reaction latency | One message, no acknowledgment | Cumulative signed state makes TopUp idempotent, so fire-and-forget is safe and a payer can use a rate the moment it buys it |
| Who pays | Each side pays for what it received, in the vouchers of whoever delivered it | Symmetric and unchanged. Both owe by default, so both fund a channel and buy their own grants |
| Acquiring vouchers | Not a protocol concern — see the market documents. Inside a node, `merchantd` acquires; `tollgated` asks it per funding | The direct route from the issuer is enough to operate, and checking a voucher takes one hop. The protocol daemon holds nothing of value |
| Received multiplier | An unsigned surcharge per peer on what that peer pushes at us, applied as a consumption weight on its grant, default `0` | Net rate is `m − 1`, so `1` makes a peer's upload free, `2` charges it like a download, `k + 1` charges it `k` times. Unsigned, so a node can never pay a bonus on top of what it already owes for delivery |
| Accepted mints | One ordered list, at least one entry, no prices | One unit of account network-wide makes any mint's vouchers usable. A relay accepting its upstream's mint can spend what it receives without converting |
| Accepted-mint haircuts | None — accept or refuse | What an issuer's paper is worth belongs on the market, not in a settlement discount |
| After settlement | Own vouchers burned at `mintd`; another mint's kept (deposited with `merchantd`) or burned, set per mint | The claim on this node has been honored. Whether another issuer's paper is worth anything depends on the relationship — see [tollgate-daemons.md](tollgate-daemons.md) |
| Foreign voucher cost | Gives up free settlement, local double-spend checks, and payment-liveness-equals-service-liveness | Those three properties hold only for own vouchers. The accepted set is the operator's explicit list, any mint; each costs reachability, and credit only if kept |
| Market operations | Separate endpoints and protocol; never TollGate messages | Buying and swapping is not paying for delivery. A node offering neither is fully functional — see [market-protocol.md](../market/market-protocol.md) |
| Minimum flow allowance | A floor on the shaping rate, not a stored quantity | It is what a peer falls back to when its grant expires, which is what keeps a link alive long enough to send the next TopUp. Being a rate, it cannot be accumulated |
| Allowance vouchers | None — the allowance is delivery, not payment | Free vouchers are `merchantd`'s or `mintd` auto-accept's matter. As a rate it cannot be resold or accumulated, so there is nothing to lock |
| Spilman channels | Kept, to bound the issuer's database rather than to prevent theft | The spent-proof set is ~700× larger without channels |
| Cryptographic effort | Concentrate on the exchange step | The only step with a real adversary once the issuer redeems its own vouchers |
| Trust model | Reputation and exposure limits, not cryptography | When the provider is the mint, the only party who can cheat is the one who would honor the refund. Accepted deliberately |
