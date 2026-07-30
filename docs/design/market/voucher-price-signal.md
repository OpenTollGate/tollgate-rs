# Voucher Price Signal

Every node uses the same unit for a given resource — bytes for network
forwarding — but each node issues its own vouchers. The unit is shared; the
issuer is not. A 1 KiB voucher from a reliable gateway and a 1 KiB voucher
from an unreliable one both claim 1024 bytes, and they are worth different
amounts.

**That difference is the signal.** What an issuer's vouchers sell for,
compared to the quantity printed on them, is a direct measure of how much
the network expects that issuer to deliver.

| Selling price of a 1 KiB voucher | What it means |
|---|---|
| 1.00 KiB | Full value — redemption expected in full |
| 0.70 KiB | Issued more than it can deliver, unreliable, or in low demand |
| 1.05 KiB | In demand — access to this node is scarce |

It is public, updated continuously, and set by people who lose money if they
get it wrong. Nothing in the payment protocol produces anything comparable:
there, a node that takes payment and delivers poorly is noticed only by the
peer that already paid it.

---

## The Same Price, Below Zero

The selling price above and the voucher price quoted in **paid acceptance**
([tollgate-vouchers.md](../core/tollgate-vouchers.md)) are one number, not
two. Paid acceptance is what happens when it goes below zero.

![Voucher Price Scale](../core/diagrams/voucher-price-scale.svg)
<details><summary>Text version</summary>

```
  voucher price:  positive ──────── zero ──────── negative ──── refused
                     peer buys       even swap     issuer pays   nobody will
                     them                          to place them hold them
```
</details>

- **Above 1.00** — access to the issuer is scarce.
- **Between 0 and 1.00** — the market discounts the issuer's promise.
- **At 0** — the vouchers are neither wanted nor a burden.
- **Below 0** — the issuer must pay to have them held at all. This is a leaf
  node whose only "capacity" is uplink nobody upstream wants.
- **Refused** — no price works.

A leaf sits permanently at the negative end, and that is correct rather than
a failure: its capacity genuinely is worth nothing to its parent.

---

## Liquidity

This is the weakest part of the design.

Each issuer is its own small, thin market. Anyone making that market has to
hold vouchers, and holding them means taking the risk that an anonymous
router stops redeeming — see [issuer-risk.md](issuer-risk.md). The business
case for providing that liquidity is unproven, and it does not obviously
improve with scale: more issuers means more books, each thinner.

Three things make it less bad than it first looks:

- **The market is optional.** Value moves without it, via direct purchase
  and paid acceptance ([voucher-acquisition.md](voucher-acquisition.md)). A
  market that never appears costs the reliability signal, not the network.
- **Issuers are natural market makers in their own paper.** A node always
  wants to sell its own vouchers and is always willing to redeem them, so
  each book has one committed participant by construction.
- **Multi-mint acceptance concentrates demand without anyone planning it.**
  Because a node can accept any mint denominated in the same unit
  ([tollgate-vouchers.md](../core/tollgate-vouchers.md)), the vouchers that
  many nodes happen to accept become the ones worth holding. Those books get
  deep while the rest stay shallow, and the deep ones start functioning as
  money for the network. Nothing designates a hub currency; acceptance
  decisions produce one.

That last point cuts the other way too. A voucher accepted everywhere is
systemically important, and its issuer's failure stops being one router's
problem.

---

## Open Problems

| Problem | Notes |
|---|---|
| Liquidity | Per-issuer books are small and thin; the case for making them is unproven. |
| Observability of failure | The signal only prices reliability if failures to redeem are visible to people who are not the victim. Nothing currently makes them visible — see [issuer-risk.md](issuer-risk.md). |
| Quoting a price for a peer's vouchers | Every node has to price every peer's vouchers, continuously. Refusing foreign vouchers by default avoids the question but does not answer it. |
