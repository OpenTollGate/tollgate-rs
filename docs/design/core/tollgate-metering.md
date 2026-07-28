# TollGate Metering

This document specifies how TollGate counts units delivered between peers, how the two sides reconcile their measurements, and the trait the implementation provides.

Metering produces the inputs to billing. Pricing turns metered units into Cashu payments — see [tollgate-pricing.md](tollgate-pricing.md). Access control ([tollgate-access-control.md](tollgate-access-control.md)) decides whether delivery happens; metering counts what was delivered.

---

## What is Metered

Each node meters **units delivered to each peer** (outbound). This is what the node charges for — it did the work of delivering those resources.

```
Node A delivers 1000 units to Peer B this interval
A's metering: units_delivered_to_B += 1000
```

Both sides meter independently. Each side's counters are local; they are reconciled at each metering interval via the MeteringReport message.

All units delivered to a peer are metered — including TollGate protocol messages and locally-addressed resources. Distinguishing control plane from data plane at the metering layer adds complexity for negligible savings (protocol messages are tiny relative to delivered resources).

### Future: units handled on behalf of a peer

Metering the **outbound** direction and charging the deliverer's price is
what makes negative prices necessary: a leaf delivers its own uplink to its
relay, so the rule bills the relay for the leaf's traffic and a negative
price has to cancel it out.

The decided direction is to meter **units handled on behalf of a peer** —
both directions of that peer's traffic — and bill that peer. The sign never
goes negative because the beneficiary always pays. See the Negative Pricing
section of [tollgate-pricing.md](tollgate-pricing.md).

Two consequences for this document, both **future work**:

- **Metered units carry a direction class.** Removing the sign asymmetry
  must not remove the rate asymmetry: uplink and downlink are different
  goods on asymmetric backhaul, and one rate across both underprices the
  scarce direction. `MeterStream` would report counters per
  adapter-defined class rather than a single `delivered` / `received` pair.
  The core neither enumerates nor interprets the classes.
- **Attribution decides who pays.** Today a byte's
  direction determines who is billed; under the change it is the byte's
  beneficiary. Misattribution becomes a way to shift cost, which the current
  counter model has no need to defend against.

---

## Cumulative Counter Model

Counters are **cumulative since session start** (the ChannelReady baseline). Each peer reports its cumulative totals; both sides compute the per-interval delta locally as `current_cumulative - previous_cumulative`.

This is self-healing: a lost or duplicated MeteringReport doesn't corrupt accounting. The next report still carries the correct totals. No sequence numbers are needed.

Wire format details — `MeteringReport` message, interval flow — are in [tollgate-protocol.md](tollgate-protocol.md).

---

## Calibration

Both sides report what they sent (`delivered`) AND what they received (`received`) from the peer. This bidirectional reporting allows calibration even when raw counts diverge.

```
A says: "I delivered 1000 units to you this interval"   (delta from cumulative)
B says: "I received 980 units from you this interval"   (delta from cumulative)
Transit loss = |1000 - 980| / 1000 = 2%   (within default 5% tolerance)
```

Within tolerance: both sides note the discrepancy but bill on the value that favors the deliverer. Outside tolerance: see Transit Loss Resolution below.

---

## Transit Loss Resolution

When the two sides disagree on unit counts, billing uses **the value that favors the deliverer**. Even if the receiver dropped some units, the deliverer still expended resources sending them, so the residual bias is deliberately placed on the party that did the work.

Which raw value that is depends on the **sign of the price**, because the sign determines who pays:

| Price sign | Who pays | Billable value | Effect |
|---|---|---|---|
| `price > 0` | receiver of the delivery | **higher** of the two counts | deliverer is paid more |
| `price < 0` | deliverer (subsidy) | **lower** of the two counts | deliverer pays less |
| `price == 0` | nobody | — | no billing |

Stating the rule as "always use the higher value" is only correct for positive prices. Under a negative price the deliverer is the *payer*, so the higher value favors the counterparty instead — letting a peer inflate its received-count and skim up to the full tolerance every interval, indefinitely, without ever crossing the threshold that triggers a warning. The rule is expressed in terms of the deliverer so that the bias lands on the same party regardless of sign.

| Situation | Billable amount | Action |
|-----------|----------------|--------|
| Within tolerance (default 5%) | Deliverer-favoring value | Normal — both sides note discrepancy |
| Exceeds tolerance | Deliverer-favoring value | Warning sent (Reject: transit loss tolerance exceeded) |
| Persistent (3+ intervals) | Deliverer-favoring value | Close and renegotiate |

**Note:** this rule is honest-deliverer-optimistic. Under a positive price a dishonest provider could inflate unit counts; under a negative price a dishonest subsidy payer could deflate them. In both cases the abuse is capped at the tolerance. Mitigation (proof-of-delivery, reputation systems) requires further design — out of scope for v1.

Tolerance and the consecutive-over-tolerance threshold are configurable — see [tollgate-configuration.md](tollgate-configuration.md).

**Within-tolerance divergence is not free.** A counterparty that sits persistently at the edge of the tolerance band is extracting the full tolerance every interval while never triggering the over-tolerance path. Implementations should track the *signed mean* of the divergence across intervals, not just its magnitude: honest transit loss is noisy around a small positive mean, whereas manipulation shows as a stable offset pinned near the tolerance limit.

---

## ResourceAdapter Trait (Metering Members)

The `ResourceAdapter` trait spans both access control and metering. The metering-related members:

```rust
pub trait ResourceAdapter: Send + Sync {
    /// Subscribe to metering counter updates for a peer. The implementation
    /// pushes cumulative unit counts as they change. Core takes a snapshot
    /// at each metering interval to compute the delta.
    fn subscribe_meter(&self, peer: &Pubkey) -> Result<MeterStream, AdapterError>;

    /// Get resource metrics for a peer (for dynamic pricing). None for
    /// resources without metrics.
    fn peer_metrics(&self, peer: &Pubkey) -> Option<PeerMetrics>;

    // ... access control members documented in tollgate-access-control.md
}

/// Continuous metering counter stream. Implementation pushes updates as delivery proceeds.
pub struct MeterStream {
    /// Cumulative units delivered TO this peer (outbound)
    pub delivered: watch::Receiver<u64>,
    /// Cumulative units received FROM this peer (inbound)
    pub received: watch::Receiver<u64>,
}
```

### PeerMetrics

Available from FIPS MMP or equivalent. Used for dynamic pricing only — not access control. Opaque to core; each adapter provides what's relevant for its resource type.

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
| Metering target | All outbound units (delivered to peer) | What we charge for; protocol overhead is negligible at normal volumes |
| Counter model | Cumulative since session start, not deltas | Self-healing: lost/duplicated reports don't corrupt accounting |
| Counter delivery | Push/stream (watch channels) | Continuous updates from adapter; core snapshots at each metering interval |
| Reporting | Bidirectional (delivered + received) | Calibration without trust |
| Transit loss resolution | Use the deliverer-favoring value (higher if price > 0, lower if price < 0) | Favors the party that did the work, deterministic, and sign-stable. A flat "higher value" rule inverts under negative prices. Dishonest peer mitigation is future work. |
| Within-tolerance divergence | Track the signed mean across intervals | Persistent offset at the tolerance edge is manipulation; honest transit loss is noisy around a small mean |
| Transit loss tolerance | 5% default, configurable | Accounts for loss between measurement points |
| Persistent over-tolerance | Close after 3 consecutive intervals | Something is wrong with the link or metering |
| Peer metrics | Opaque map (key → value) | Implementation provides whatever is relevant for its resource type |
