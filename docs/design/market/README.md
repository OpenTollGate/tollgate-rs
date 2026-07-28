# TollGate Market

TollGate's payment protocol does not price anything. A voucher is a claim on
one unit of a node's capacity, and redeeming it is delivery — one for one,
with no rate to quote ([tollgate-pricing.md](../core/tollgate-pricing.md)).

What a unit costs in money is settled where vouchers change hands. That is
what these documents cover: how a peer gets vouchers, what determines their
price, and what can go wrong with an issuer.

None of this is required to move value. A peer buys vouchers from the node
it wants service from and spends them there. The market layer exists to make
prices comparable across issuers and to surface which operators actually
deliver — the reliability signal the protocol itself cannot produce.

## Documents

| Document | Description |
| -------- | ----------- |
| [voucher-acquisition.md](voucher-acquisition.md) | How a peer comes to hold vouchers: Lightning mint quotes, direct purchase, local swaps, cross-mint swaps |
| [voucher-price-signal.md](voucher-price-signal.md) | Selling price against face value as a public measure of expected delivery; liquidity and market making |
| [issuer-risk.md](issuer-risk.md) | Overissuance, selling without redeeming, redemption congestion, operator shutdown |

## Relationship to the Core Protocol

| Question | Answered by |
| -------- | ----------- |
| What is a voucher, and how is it spent? | [tollgate-vouchers.md](../core/tollgate-vouchers.md) |
| What does delivery cost? | [tollgate-pricing.md](../core/tollgate-pricing.md) — one voucher per unit |
| What does a voucher cost in sats? | Here |
| How are payments batched? | [tollgate-payment-channels.md](../core/tollgate-payment-channels.md) |

The core protocol is deliberately ignorant of everything in this directory.
A node that never touches a market still works: it sells its own vouchers
for whatever it likes and redeems them on delivery.
