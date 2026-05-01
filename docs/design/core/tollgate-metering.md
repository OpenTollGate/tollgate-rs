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

Within tolerance: both sides note the discrepancy but bill on the higher value. Outside tolerance: see Transit Loss Resolution below.

---

## Transit Loss Resolution

When the two sides disagree on unit counts, **the higher value is used** for billing. The provider claims they did more work; even if the receiver dropped some units, the provider still expended resources sending them.

**Note:** this rule is honest-provider-optimistic. A dishonest provider could inflate unit counts. Mitigation (proof-of-delivery, reputation systems) requires further design — out of scope for v1.

| Situation | Billable amount | Action |
|-----------|----------------|--------|
| Within tolerance (default 5%) | Higher value | Normal — both sides note discrepancy |
| Exceeds tolerance | Higher value | Warning sent (Reject: transit loss tolerance exceeded) |
| Persistent (3+ intervals) | Higher value | Close and renegotiate |

Tolerance and the consecutive-over-tolerance threshold are configurable — see [tollgate-configuration.md](tollgate-configuration.md).

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
| Transit loss resolution | Use higher value | Favors provider (did the work), deterministic. Dishonest peer mitigation is future work. |
| Transit loss tolerance | 5% default, configurable | Accounts for loss between measurement points |
| Persistent over-tolerance | Close after 3 consecutive intervals | Something is wrong with the link or metering |
| Peer metrics | Opaque map (key → value) | Implementation provides whatever is relevant for its resource type |
