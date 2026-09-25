# Market Protocol

Buying and swapping vouchers is **not part of paying for delivery**. It has
its own endpoints, its own messages, and no dependency on a TollGate session.

The boundary is stated in
[tollgate-protocol.md](../core/tollgate-protocol.md): no amount in any
TollGate message is denominated in money, no TollGate message buys or swaps
anything, and a node that offers no market services works normally.

**What does stay in the payment protocol** is the set of mints a node accepts
as payment, with a settlement ratio for each. A peer has to know what it can
pay with before it can pay, so that belongs in the Offer message. Those
ratios say how many units of delivery a voucher is credited as — they are not
exchange rates against money.

---

## Who Serves It

A node's mint, `mintd`, takes **no money**: it has no Lightning backend, and
its melt burns vouchers rather than paying out
([tollgate-daemons.md](../core/tollgate-daemons.md#mintd)). Everything that
involves money goes through `merchantd`, which serves these endpoints, holds
what it takes, and has `mintd` issue what it sells through standard NUT-04
on a private listener where quotes are paid on creation. `mintd` exposes
nothing but standard Cashu endpoints.

So the Cashu operations a buyer might expect map onto the two daemons:

| Operation | Where |
|---|---|
| Discover what the mint supports | `mintd`, NUT-06 info |
| Pay and receive this node's vouchers | `merchantd`, `swap` below — it issues at `mintd` against the buyer's blinded outputs |
| Return vouchers for money | `merchantd`, if it buys this node's paper back; `mintd` only burns |
| Get free vouchers, where the node gives service away | `mintd`, NUT-04 — quotes on its public listener are paid on creation when auto-accept is on |

A node that sells nothing runs no `merchantd` and implements nothing from
this document.

---

## Endpoints

Served under a path prefix distinct from the payment protocol:

```
payment    POST /tollgate/v1/exchange        GET /tollgate/v1/ws
market     GET  /tollgate/market/v1/info
           POST /tollgate/market/v1/swap
```

The market prefix is served by `merchantd`, and **may be served by a
different host or a third party**. A node that wants to offer swaps without
running a market itself points at somebody else's. Nothing in the payment
path depends on it being reachable, or existing.

Encoding is JSON rather than the payment protocol's CBOR. These calls are
infrequent, human-debuggable, and share a shape with the Cashu mint API that
sits beside them — compactness buys nothing here.

### `GET /tollgate/market/v1/info`

What this market will do. Discovery lives here rather than in the Offer
message, so a node can start or stop offering market services without
touching any payment session.

```json
{
  "version": 1,
  "sells": ["https://gateway.example.com/mint"],
  "accepts": [{"mint": "https://mint.minibits.cash/Bitcoin", "unit": "sat",
               "bytes_per_unit": 1000000},
              {"mint": "https://usd-mint.example.com",       "unit": "usd",
               "bytes_per_unit": 800000000}],
  "cross_mint_swap": false
}
```

`sells` lists mint URLs, not units — the unit is fixed by the resource.
`accepts` is `merchantd`'s configured list of what it takes as payment, each
mint with its unit (`sat`, `usd` or `eur`) and the **price in force**: how many
bytes one unit of that mint's tokens buys right now (a `usd` or `eur` unit is a
cent). A token from anything else is refused. `cross_mint_swap` means it will
trade one mint's vouchers for another's.

The price is per issuer: each `accepts` entry may carry its own price, so a
sat from a mint the operator trusts can buy more than a sat from one it does
not ([voucher-price-signal.md](voucher-price-signal.md)). Where a price is set
in a currency other than the token's unit, `bytes_per_unit` already includes
the current exchange rate, and moves when it does.

There is no list of what the market buys. `merchantd` buys its upstreams'
vouchers as a customer of *their* markets, whenever `tollgated` needs them
to fund a channel ([tollgate-daemons.md](../core/tollgate-daemons.md#funding-upstream)),
which is nothing it advertises here.

### `POST /tollgate/market/v1/swap`

Buy at the price in force. The asker sends a payment token and blinded
outputs for what it is buying; the market returns signatures on them.

```json
{ "tokens": ["cashuA…"], "outputs": [ /* NUT-00 BlindedMessages */ ] }
```

```json
{ "signatures": [ /* NUT-00 BlindSignatures */ ] }
```

Outputs rather than finished tokens, so that when the market is selling this
node's own vouchers it can pass them straight to `mintd` against a paid NUT-04
quote on its private listener, and never sees the secrets of what it sold
([tollgate-daemons.md](../core/tollgate-daemons.md#the-sale-end-to-end)). For
a cross-mint trade the outputs are for the mint being bought.

The outputs must add up to exactly what the payment buys at the price in
force, which the asker reads from `info`. If they do not — because the price
or the exchange rate moved in between — the swap is refused before any money
is taken, with the current price attached, and the asker rebuilds its outputs
and tries again. A rate move costs a retry, never a wrong price.

**There is no quote step, for now.** A quote would lock a price for a while:
useful when a fiat-derived price must hold across several calls, for
cross-mint trades whose amount depends on two issuers, and for selling future
capacity. None of that is needed to sell today's capacity, so it is left as an
extension that can be added beside `swap` without changing it.

**Atomicity is unsolved.** As written, one side moves first and trusts the
other to complete. Making this atomic across two mints needs a hash-locked
construction (NUT-11/NUT-14 style) with both mints online, and no working
implementation exists — see
[voucher-acquisition.md](voucher-acquisition.md). Until then, exposure is one
swap's worth, and the sensible mitigation is to keep swaps small or to swap
only with a counterparty that has something to lose.

---

## Why Keep Them Apart

**The payment protocol stays small and auditable.** Its whole job is: agree
what is accepted, fund channels, count units, settle. Nothing in it needs to
know what money is.

**Market failure is not service failure.** A market that is down, illiquid,
or absent does not stop anyone delivering or paying. A peer that already
holds vouchers is unaffected.

**Constrained devices can skip it.** An ESP32 runs the payment protocol and a
Cashu mint, and implements none of this. Its vouchers are sold by a merchant
somewhere else, or it gives service away.

**Market makers are not obliged to be routers.** Separating the endpoints
means a party that wants to make markets in voucher paper can do so without
forwarding a single byte, and a router can offer swaps by pointing at one.

**The unsolved parts stay contained.** Atomic cross-mint swap and the
reliability signal are the two weakest pieces of the whole design
([issuer-risk.md](issuer-risk.md)). Keeping them behind a separate interface
means their absence is a missing feature rather than a hole in the payment
path.

---

## Open Problems

| Problem | Notes |
|---|---|
| Swap atomicity | One side moves first. A hash-locked construction needs both mints online and has no working implementation. |
| Price honesty | Nothing binds a market to honor the price `info` showed. A market that swaps at a worse price than it advertised is caught only by the buyer, whose outputs no longer match. Exposure is one swap; reputation is the only correction, with the same observability problem as issuer default. |
| Locked prices | No quote step: a price holds only as long as the rate behind it. Quotes, cross-mint pricing and forward sales are future extensions. |
| Market discovery | How a peer finds a market at all, if the node it is talking to does not run one. Unspecified — a well-known path on the peer, a directory, or out-of-band. |
