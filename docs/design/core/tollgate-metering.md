# TollGate Metering

This document specifies how TollGate counts units delivered between peers, how the two sides reconcile their measurements, and the trait the implementation provides.

Metering produces the inputs to billing. Billing is trivial — one voucher per unit delivered, see [tollgate-vouchers.md](tollgate-vouchers.md). Access control ([tollgate-access-control.md](tollgate-access-control.md)) decides whether delivery happens; metering counts what was delivered.

---

## What is Metered

Each node meters two things per peer, link-local: **units delivered to it** and **units received from it**. Both are billable — the peer pays for its downloads and its uploads alike ([tollgate-vouchers.md](tollgate-vouchers.md)).

```
This interval, for peer B:
  delivered_to_B  += 1,000,000     (B's download)
  received_from_B +=    50,000     (B's upload)
```

Billing applies the received multiplier; metering does not. The counts on the
wire stay raw so both sides can reconcile them against each other.

Both sides meter independently. Each side's counters are local; they are reconciled at each metering interval via the MeteringReport message.

All units crossing the link are metered — including TollGate protocol messages and locally-addressed resources. Distinguishing control plane from data plane at the metering layer adds complexity for negligible savings (protocol messages are tiny relative to delivered resources).

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

Within tolerance: both sides note the discrepancy but bill on the higher value. Outside tolerance: see Transit Loss Resolution below.

---

## Transit Loss Resolution

When the two sides disagree on unit counts, billing uses **the higher of the
two counts**. The node that carried the traffic expended resources doing so
even if some was lost in transit, so the residual bias sits with the party that
did the work.

There is no sign to reason about. Every billable amount is a count of units,
and the multiplier that scales the received side is unsigned, so nothing
billable can be negative.

| Situation | Billable amount | Action |
|-----------|----------------|--------|
| Within tolerance (default 5%) | Higher value | Normal — both sides note discrepancy |
| Exceeds tolerance | Higher value | Warning sent (Reject: transit loss tolerance exceeded) |
| Persistent (3+ intervals) | Higher value | Close and renegotiate |

**Note:** this rule is honest-carrier-optimistic. A dishonest node could inflate what it claims to have carried, capped at the tolerance. Mitigation (proof-of-delivery, reputation systems) requires further design — out of scope for v1.

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

    /// Get resource metrics for a peer. Not used for pricing — delivery has
    /// no price — but available to the operator for capacity and health
    /// decisions. None for resources without metrics.
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

Available from FIPS MMP or equivalent. Not used for pricing or access control — delivery has no price and metrics are peer-reported, so letting them set a price would let the peer set its own. Exposed for operator visibility and capacity decisions. Opaque to core; each adapter provides what's relevant for its resource type.

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
| Metering target | Both directions of the link, per peer | The peer pays for its downloads and uploads alike; the counters were already there |
| Counter model | Cumulative since session start, not deltas | Self-healing: lost/duplicated reports don't corrupt accounting |
| Counter delivery | Push/stream (watch channels) | Continuous updates from adapter; core snapshots at each metering interval |
| Reporting | Bidirectional (delivered + received) | Calibration without trust |
| Transit loss resolution | Use the higher of the two counts | Favors the party that did the work. No sign to handle: every billable amount is a count, and the multiplier that scales the received side is unsigned |
| Within-tolerance divergence | Track the signed mean across intervals | Persistent offset at the tolerance edge is manipulation; honest transit loss is noisy around a small mean |
| Transit loss tolerance | 5% default, configurable | Accounts for loss between measurement points |
| Persistent over-tolerance | Close after 3 consecutive intervals | Something is wrong with the link or metering |
| Peer metrics | Opaque map (key → value), never an input to price | The peer controls its own metrics, so pricing from them lets it price itself |
