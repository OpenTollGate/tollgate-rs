# TollGate Metering

This document specifies how TollGate counts units delivered between peers, what those counts are used for, and the trait the implementation provides.

**Metering is local.** Counts are not exchanged, not signed, and not an input to any payment. A peer prepays into a budget ([tollgate-vouchers.md](tollgate-vouchers.md)) and the provider draws that budget down as traffic passes; the counters are how it knows how much to draw. Access control ([tollgate-access-control.md](tollgate-access-control.md)) decides whether delivery happens; metering counts what was delivered.

---

## What is Metered

Each node meters two things per peer, link-local: **units delivered to it** and **units received from it**.

```
For peer B:
  delivered_to_B  += 1,000,000     (B's download)
  received_from_B +=    50,000     (B's upload)
```

Both counters feed the shaper. Each tick the peer's budget is drawn down by the sum, with its uploads weighted by the received multiplier — or by its reserved rate over the tick, if that is more:

```
moved     = delivered + received × received_multiplier   // this tick
consumed += max(moved, reserved_rate × tick)
```

That is the one rule of [tollgate-vouchers.md](tollgate-vouchers.md#the-one-rule): a payer that reserved a rate pays for it whether it uses it or not, and one that reserved nothing pays for what it moved. A tick in which the node did not carry the peer — the session was down, or the enforcer was not connected — draws nothing.

The counters themselves stay raw. The weighting and the reserved rate are applied when drawing down the budget, so what the meter reports and what the shaper charges stay separable.

What is metered is what the node delivers **for or through** the peer. Traffic addressed to or sent by the node itself — TollGate protocol messages, `mintd`, `merchantd`'s market endpoints — is not metered, and is **never blocked or shaped**, whatever the peer's access level: blocking it would cut off the payment that restores delivery. Where the delivery path already separates the two, as a kernel does between forwarded and locally-delivered packets, the exemption costs nothing. Where it does not, an implementation may count that traffic, but must still never block it.

## Cumulative Counter Model

Counters are **cumulative since session start** (the ChannelReady baseline). What they draw is added to `consumed`, which is compared against `authorized` — the budget the peer brought into the session plus what it has signed for since — and the difference is what the peer may still spend.

The counters are not reported to the peer. The payer knows what it signed for; the provider knows what it delivered. Neither has to convince the other, because the money moved before the traffic did. What the provider does report is the result, in a **Balance** message: what is left of the budget, when it expires, and the rate reserved ([tollgate-protocol.md](tollgate-protocol.md#0x0c-balance)). It is information. The payer keeps its own count and decides what to buy from it, so a Balance never moves money.

Grant state and the TopUp message are in [tollgate-protocol.md](tollgate-protocol.md).

---

## What The Payer Measures

The payer runs the same two counters, and they answer a different question: **is this provider worth buying from again?**

```
A reserved  102,400 units/s for 5 s     512,000 drawn
A received  460,000 units in those 5 s
gap                  52,000                ~10%
```

**Under-delivery is measurable one-sided.** The payer knows exactly what it bought and exactly what arrived, both from its own counters, with nothing to take on trust from the provider. The gap may be transit loss, deliberate shaping, or a provider taking payment and delivering less, and the payer cannot tell which — but it does not need to. All three mean the same thing about whether to keep buying here.

That makes it an input to **connection choice**: which peer to buy from, how large a budget to risk, whether to keep the peering at all. Delivered rate against purchased rate is a per-provider score a node can accumulate over time and across sessions, and acting on it needs no protocol, no cooperation, and no message.

This replaces the two-sided reconciliation of the metered-settlement model, where both sides exchanged counters, compared them, and split the difference by rule. That machinery existed because the counts decided how much money moved. They no longer do, so the disagreement they used to arbitrate cannot arise.

A Balance from the provider that says less is left than the payer's own count is the same gap seen from the other side, and is scored the same way; it is never a reason to buy more.

**What is one-sided is the evidence, not the measurement.** A payer can act on what it sees but cannot show it to anyone else, so a provider that skims a few percent from every peer is invisible outside those peers. The old cross-check was weak evidence — it relied on a counterparty reporting honestly about itself — but it was evidence. What is left is the same visibility problem the market layer already has for refused redemption ([issuer-risk.md](../market/issuer-risk.md)), and it wants the same answer.

**Revisit if this bites.** If skimming turns out to be common in practice, a reporting path can be added later without disturbing anything here: the counters, their names, and their locality all stay, and what gets added is a way to publish a delivered-against-purchased ratio. Nothing in the payment path depends on that not existing, which is why it is left out for now rather than designed around.

---

## Enforcer Trait (Metering Members)

The `Enforcer` trait (`ResourceAdapter` in the code today) spans both access control and metering. It belongs to the host (`tollgate-net`), not to `tollgate-core`: core never does I/O, so the host reads the enforcer and hands core what it found. The metering-related members:

```rust
pub trait Enforcer: Send + Sync {
    /// Cumulative units delivered to and received from a peer. The host reads
    /// these every tick and core draws the peer's budget down against them.
    fn counters(&self, peer: PubKey) -> Counters;

    /// Units per second we want to pull from this peer. Drives the buyer.
    fn demand(&self, peer: PubKey) -> u64;

    /// The rate a peer is currently shaped to.
    fn shaping_rate(&self, peer: PubKey) -> u64;

    /// Every peer the enforcer is tracking, and forgetting one that has gone.
    fn peers(&self) -> Vec<PubKey>;
    fn remove(&self, peer: PubKey);

    /// Resource metrics for a peer. Not used for pricing — delivery has no
    /// price — but available to the operator for capacity and health
    /// decisions. None for resources without metrics.
    fn peer_metrics(&self, peer: PubKey) -> Option<PeerMetrics>;

    // ... access control members documented in tollgate-access-control.md
}

/// Defined in tollgate-core, which draws budgets down against it.
pub struct Counters {
    /// Units delivered TO this peer — its download.
    pub delivered: u64,
    /// Units received FROM this peer — its upload.
    pub received: u64,
}
```

Counters are read, not pushed: reading them once per tick is enough to draw a budget down, and it needs nothing from the delivery path beyond a cumulative count. A delivery path that can push changes as they happen may do so as an optimisation.

### PeerMetrics

Available from FIPS MMP or equivalent. Not used for pricing or access control — delivery has no price and metrics are peer-reported, so letting them set a price would let the peer set its own. Exposed for operator visibility and capacity decisions. Opaque to core; each enforcer provides what's relevant for its resource type.

```rust
pub enum MetricValue {
    Float(f64),
    Int(i64),
    Text(String),
    Bool(bool),
}

pub type PeerMetrics = HashMap<String, MetricValue>;
```

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Metering target | Both directions of the link, per peer | The peer's budget is drawn down by both; the counters were already there |
| Draw rule | `max(moved, reserved_rate × tick)` per tick, and nothing for a tick the peer was not carried | One rule for time at a speed and pay per use. The reserved rate is the capacity set aside, so it is paid for whether used or not — but only while the node could deliver it |
| Counter model | Cumulative since session start, not deltas | Compares directly against the cumulative total the peer has signed for |
| Counter delivery | Read, not pushed: the host reads cumulative counters once per tick | Enough to draw a budget down, and needs nothing from the delivery path beyond a cumulative count |
| Counter names | `delivered` and `received`, unchanged | The payment model changed, the measurement did not. Both already mean the peer's download and upload |
| Reporting | Counters stay local; the provider reports only the resulting Balance, as information | Payment happens before delivery, so no shared number decides how much money moves and there is nothing to reconcile. A payer that reconnects needs to know what it left behind, but decides purchases from its own count |
| Multiplier | Applied when drawing down the budget, not when counting; fixed for a session | Keeps what the meter reports separable from what the shaper charges |
| Transit loss | The payer's cost | A budget is drawn whether or not packets arrive. The payer measures its own throughput and stops buying; no tolerance, no threshold, no message |
| Under-delivery detection | One-sided, from the payer's own counters | Delivered rate against purchased rate needs nothing from the provider. Feeds connection choice: which peer to buy from, how large a budget to risk |
| Under-delivery evidence | None — a payer can act but cannot prove | Accepted for now. Same visibility gap the market layer has for refused redemption, and revisitable as a reporting path if skimming proves common |
| Peer metrics | Opaque map (key → value), never an input to price | The peer controls its own metrics, so pricing from them lets it price itself |
