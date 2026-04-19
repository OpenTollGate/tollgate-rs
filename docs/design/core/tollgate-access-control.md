# TollGate Access Control

This document specifies how TollGate gates delivery per-peer based on payment status, meters resource usage, and enforces restrictions on unpaid peers.

## Overview

TollGate controls delivery at the peer level. Each peer has an **access level** determined by their payment status. The implementation (FIPS, IP stack, etc.) enforces access control on the delivery path — TollGate core decides *what* to enforce, the resource adapter enforces *how*.

The core principle: **no pay, no delivery**. A peer that hasn't paid cannot have resources delivered through this node. It can only exchange TollGate protocol messages with this node (Announce, PriceSheet, BootstrapToken, etc.) to negotiate payment.

---

## Access Levels

Each peer is in exactly one access level at any time:

| Level | Delivery | TollGate messages | Bloom filter visibility (FIPS) | When |
|-------|---------|-------------------|-------------------------------|------|
| `None` | Blocked | Allowed | Hidden | Peer connected, no payment yet |
| `Bootstrap` | Allowed (metered against token balance) | Allowed | Visible | Bootstrap token verified |
| `Active` | Allowed (metered, Spilman channels) | Allowed | Visible | Spilman channels funded |
| `ZeroPrice` | Allowed (unmetered) | Allowed | Visible | Both sides agreed on zero pricing |
| `Suspended` | Blocked | Allowed | Hidden | Balance exhausted, awaiting top-up or renegotiation |

### Transitions

```
None --> Bootstrap (token verified)
None --> Active (Spilman channels funded)
None --> ZeroPrice (zero-price PriceSheet accepted)
Bootstrap --> Active (upgrade to Spilman)
Bootstrap --> Suspended (balance exhausted)
Active --> Suspended (channel exhausted, rollover timeout)
Suspended --> Bootstrap (new token received)
Suspended --> Active (new channel funded)
Any --> None (disconnect)
```

![Access Level State Machine](diagrams/access-level-states.svg)
<details><summary>Text version</summary>

```
                       ┌──────────────┐
                       │     None     │ (blocked, hidden)
                       └──┬────┬────┬┘
            token verified│    │    │zero-price
                          ▼    │    ▼
  ┌───────────┐  upgrade  ┌────▼─────┐      ┌───────────┐
  │ Bootstrap ├──────────→│  Active  │      │ ZeroPrice │
  │           │           │          │      │           │
  └─────┬─────┘           └────┬─────┘      └───────────┘
        │balance=0              │exhausted+timeout
        │                       │
        ▼                       ▼
  ┌─────┴───────────────────────┴──┐
  │          Suspended             │ (blocked, hidden)
  │   new token→Bootstrap          │
  │   new channel→Active           │
  └────────────────────────────────┘

  Red = blocked    Green = allowed    Blue = bootstrap
  Dashed = recovery    Any state → None on disconnect
```
</details>

### None (Default)

Every newly connected peer starts at `None`. The peer can exchange TollGate protocol messages — Announce, PriceSheet, Accept, BootstrapToken — but **no resources are delivered** for or through this peer.

This means:
- Packets originating from this peer and addressed to other nodes are **dropped**
- Packets from other nodes destined for or through this peer are **not delivered to it**
- Only TollGate protocol messages (identified by the implementation) are allowed

### Bootstrap

The peer has sent a bootstrap token that was verified with the mint. Delivery is allowed and metered against the token balance. When the balance reaches zero, the peer transitions to `Suspended`.

### Active

Spilman channels are funded and operational. Delivery is allowed and metered. Balance updates happen at each settlement interval via the normal Spilman flow.

### ZeroPrice

Both sides agreed on zero pricing for all products. No payment infrastructure is needed. Delivery is allowed and unmetered. No settlement messages are exchanged.

### Suspended

The peer's payment has been exhausted (bootstrap balance = 0, or Spilman channel exhausted with rollover timeout expired). Delivery is blocked. The peer can still exchange TollGate messages to send a new token or fund a new channel.

---

## What "Blocked" Means

When delivery is blocked (`None` or `Suspended`), the node:

1. **Suspends resource delivery from this peer** — packets originating from the peer addressed to other nodes are silently dropped
2. **Does not deliver to this peer** — packets from other nodes destined for this peer are not delivered (they may be re-routed via other paths)
3. **Allows control messages** — packets from the peer addressed to *this node* are delivered (this is how TollGate protocol messages reach the node)
4. **Allows TollGate protocol messages** — the peer must be able to negotiate payment

The implementation decides how to enforce this. In FIPS, this could be a delivery filter that checks the peer's access level before delivering. In a traditional IP network, this could be firewall rules.

---

## Bloom Filter Visibility (FIPS)

In FIPS, bloom filters advertise reachability — "I can reach destination X through peer Y." If an unpaid peer is included in bloom filters, other nodes may route resources through it, only to have them blackholed at the gate.

**Rule: unpaid peers are hidden from bloom filters.**

| Access level | Included in bloom filters? |
|-------------|---------------------------|
| `None` | No — hidden (FIPS) |
| `Bootstrap` | Yes — visible (FIPS) |
| `Active` | Yes — visible (FIPS) |
| `ZeroPrice` | Yes — visible (FIPS) |
| `Suspended` | No — hidden (FIPS) |

Bloom filter visibility is **inferred from the access level** — the implementation maps `set_peer_access(None/Suspended)` to hidden and `set_peer_access(Bootstrap/Active/ZeroPrice)` to visible. No separate API call needed.

This requires a FIPS modification — the ability to selectively include/exclude peers from bloom filter computation. See [FIPS_FEATURE_REQUESTS.md](../v1-to-v2-migration/FIPS_FEATURE_REQUESTS.md).

---

## Metering

### What is Metered

Each node meters **units delivered to each peer** (outbound). This is what the node charges for — it did the work of delivering those resources.

```
Node A delivers 1000 units to Peer B this interval
A's metering: units_delivered_to_B += 1000
```

Both sides meter independently. At each settlement interval, both exchange MeteringReports containing cumulative values since session start:
- `units_delivered`: cumulative units we delivered TO this peer
- `units_received`: cumulative units we received FROM this peer
- `elapsed_ms`: milliseconds since session start

Each side computes the interval delta as `current_cumulative - previous_cumulative`. Cumulative counters make the protocol self-healing — lost or duplicated reports don't corrupt accounting.

### Calibration

Both sides report what they sent and what they received. This allows calibration:
- A says: "I delivered 1000 units to you this interval" (delta from cumulative counters)
- B says: "I received 980 units from you this interval" (delta from cumulative counters)
- Transit loss = |1000 - 980| / 1000 = 2% (within default 5% tolerance)

### Transit Loss Resolution

When the two sides disagree on unit counts, **the higher value is used** for billing. The provider claims they did more work; even if the receiver dropped some units, the provider still expended resources sending them.

**Note:** This rule is honest-provider-optimistic. A dishonest provider could inflate unit counts. Dealing with dishonest peers (proof-of-delivery, reputation systems, etc.) requires further design work in future phases.

| Situation | Billable amount | Action |
|-----------|----------------|--------|
| Transit loss within tolerance (default 5%) | Higher value | Normal — both sides note discrepancy |
| Transit loss exceeds tolerance | Higher value | Warning sent (Reject: transit loss tolerance exceeded) |
| Persistent transit loss (3+ intervals) | Higher value | Close and renegotiate |

### Metering Scope

All units delivered to a peer are metered — including TollGate protocol messages and locally-addressed resources. Distinguishing control plane from data plane at the metering layer adds complexity for negligible savings (protocol messages are tiny relative to delivered resources). The cost of protocol overhead is effectively zero at normal delivery volumes.

---

## ResourceAdapter Trait

The core library uses the `ResourceAdapter` trait to interact with the resource layer. The implementation provides access control enforcement and metering counters.

```rust
pub trait ResourceAdapter: Send + Sync {
    /// Set the access level for a peer. The implementation enforces delivery rules
    /// AND infers bloom filter visibility from the access level:
    /// - None/Suspended -> hidden from bloom filters (FIPS)
    /// - Bootstrap/Active/ZeroPrice -> visible in bloom filters (FIPS)
    fn set_peer_access(&self, peer: &Pubkey, access: AccessLevel) -> Result<(), AdapterError>;

    /// Subscribe to metering counter updates for a peer. The implementation pushes
    /// cumulative unit counts as they change. Core takes a snapshot at each
    /// settlement interval to compute the delta.
    fn subscribe_meter(&self, peer: &Pubkey) -> Result<MeterStream, AdapterError>;

    /// Get resource metrics for a peer (for dynamic pricing). None for resources without metrics.
    fn peer_metrics(&self, peer: &Pubkey) -> Option<PeerMetrics>;
}

/// Continuous metering counter stream. Implementation pushes updates as delivery proceeds.
pub struct MeterStream {
    /// Cumulative units delivered TO this peer (outbound)
    pub delivered: watch::Receiver<u64>,
    /// Cumulative units received FROM this peer (inbound)
    pub received: watch::Receiver<u64>,
}

pub enum AccessLevel {
    /// No delivery. Only TollGate protocol messages allowed.
    None,
    /// Delivery allowed, metered against bootstrap token balance.
    Bootstrap,
    /// Delivery allowed, metered via Spilman channels.
    Active,
    /// Delivery allowed, unmetered. Zero-price peering.
    ZeroPrice,
    /// Delivery blocked. Balance exhausted, awaiting payment.
    Suspended,
}
```

### PeerMetrics (Optional)

Available from FIPS MMP or equivalent. Used for dynamic pricing, not access control. The metrics are an opaque map — each resource adapter provides whatever metrics are relevant to its domain.

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

## Access Control Flow

### New Peer Connects

```
1. Network layer authenticates peer (FIPS Noise IK, WireGuard, etc.)
2. Core sets access level to None
3. Peer and node exchange Announce
4. Peer and node exchange PriceSheet
5. Peer sends Accept (or BootstrapToken)
6. Access level transitions based on payment
```

### Bootstrap Payment

```
1. Peer sends BootstrapToken
2. Provider verifies with mint
3. If valid: set access to Bootstrap (bloom visible in FIPS)
4. Metering begins
5. When balance exhausted: set access to Suspended (bloom hidden in FIPS)
```

### Spilman Channels Funded

```
1. Both peers send Accept with channel funding
2. Both verify funding proofs
3. Both send ChannelReady
4. Set access to Active (bloom visible in FIPS)
5. Metering and settlement begin
```

### Balance Exhausted (Suspended)

```
1. Bootstrap balance = 0 OR channel exhausted + rollover timeout
2. Set access to Suspended (bloom hidden in FIPS)
3. Delivery stops
4. Peer can send BootstrapToken or fund new channel
5. On payment: transition back to Bootstrap or Active
```

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Default access | None (blocked) | No pay, no service |
| Unpaid resources | Only local-addressed + TollGate protocol | Peer must be able to negotiate payment |
| Metering target | All outbound units (delivered to peer) | What we charge for — includes protocol overhead (negligible) |
| Metering delivery | Push/stream (cumulative counters) | Continuous updates, snapshot at settlement |
| Transit loss resolution | Use higher value | Favors provider, deterministic. Dishonest peer mitigation is future work. |
| Transit loss tolerance | 5% default, configurable | Accounts for loss between measurement points |
| Bloom filter visibility | Inferred from access level (FIPS) | No separate API — access level implies visibility |
| Zero-price peers | Skip all payment, go to Active | Simplest path for free peering |
| Suspended state | Blocked but can still negotiate | Peer can recover without reconnecting |
| Protocol messages | Always allowed regardless of access level | Payment negotiation must work even when blocked |
