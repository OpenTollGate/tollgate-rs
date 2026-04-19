# FIPS Feature Requests for TollGate Integration

This document consolidates all FIPS modifications required for TollGate v2 integration. Each feature is referenced from the relevant TollGate design doc.

---

## Critical (Required for v1)

### 1. Per-Peer Forwarding Policy

**What**: Ability to set a per-peer forwarding policy — `local_only` or `full`.

**Behavior**:
- `local_only`: Only accept traffic FROM this peer addressed TO this node. Drop all transit traffic (addressed to other nodes) from this peer. Do not forward traffic from other nodes to this peer.
- `full`: Normal forwarding — no restrictions.

**Default for new peers must be `local_only`** — closes the race window between FIPS authenticating a peer and TollGate setting the access level. No traffic is forwarded for a peer until TollGate explicitly allows it.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 2. Bloom Filter Exclusion

**What**: Ability to exclude specific peers from bloom filter computation, inferred from the forwarding policy.

**Behavior**:
- Peers with `local_only` policy are excluded from outbound bloom filters (their node_addr is not advertised to other peers)
- Peers with `full` policy are included normally
- When a peer transitions from `full` to `local_only`, removal should be **delayed by 30 seconds** to avoid bloom filter flapping when a peer temporarily exhausts its payment balance. If the peer recovers within the delay, the removal is cancelled.
- When a peer transitions from `local_only` to `full`, inclusion is immediate.

**Why**: If an unpaid peer appears in bloom filters, other nodes may route traffic through it, only to have it dropped at the gate — wasting bandwidth and causing delivery failures.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 3. Per-Peer Traffic Counters (Expose Existing)

**What**: Expose existing per-peer `LinkStats` (`bytes_sent`, `bytes_recv`) as watchable values for TollGate consumption.

**Current state**: FIPS **already tracks per-peer link stats** via `LinkStats` on each peer (`peer.link_stats().bytes_sent`, `peer.link_stats().bytes_recv`). These count all link-layer bytes sent/received per peer, which is exactly what TollGate needs (all bytes are metered, including protocol overhead — negligible).

**What's needed**: Expose these counters as `tokio::sync::watch` channels (or equivalent push mechanism) so TollGate can snapshot them at settlement intervals without polling. Alternatively, a simple read accessor via the native integration may suffice given the 5s settlement interval.

**Complexity**: Low — the data already exists, just needs to be exposed.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 4. Peer Lifecycle Callbacks

**What**: Callbacks that notify external code when peers connect and disconnect.

**Events**:
- **Peer authenticated**: Fired after Noise IK handshake completes. Provides the peer's compressed public key (33 bytes) and node_addr (16 bytes).
- **Peer disconnected**: Fired when a peer link is lost (timeout, orderly disconnect, or error). Provides the same identifiers.

**Why**: TollGate needs to create peer state on connect (set initial `local_only` policy, begin protocol exchange) and clean up on disconnect (close channels, queue settlement).

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

## High Priority

### 5. MMP Metrics Access

**What**: Direct read access to per-peer MMP (Metrics Measurement Protocol) state from native Rust code.

**Metrics needed**:
- `srtt_ms` (smoothed round-trip time)
- `loss_rate` (packet loss fraction)
- `smoothed_etx` (expected transmission count)
- `goodput_bps` (throughput in bytes/sec)
- `jitter` (latency variance)
- Trend indicators (rising/falling/stable) for RTT, loss, goodput

**Current state**: These metrics exist in FIPS and are queryable via the control socket (`show_mmp`). TollGate needs native Rust access (not JSON over control socket) for performance — metrics are read at every settlement interval for dynamic pricing.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-pricing.md](core/tollgate-pricing.md)

---

## Future

### 6. FSP Port Dispatch for TollGate

**What**: Register a dedicated FSP (FIPS Session Protocol) port for TollGate message delivery.

**Why**: The initial implementation uses HTTP over the FIPS IPv6 adapter, which works but adds HTTP overhead. A native FSP port would allow direct CBOR message delivery without HTTP framing.

**Priority**: Low — the IPv6 adapter approach works today. This is a performance optimization.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

### 7. Payment-Aware Routing

**What**: Allow the forwarding decision to consider payment status — well-paying peers get more favorable routing decisions.

**How this might work**: When `find_next_hop()` ranks candidate peers, include a payment quality signal (e.g., "this peer pays on time, has high balance, good payment history") as a factor alongside tree distance and link cost.

**Why**: Creates a market incentive — peers that pay more get better service. This aligns network quality with economic incentives.

**Priority**: Low — not needed for initial deployment. Requires careful design to avoid routing instability.

**Referenced in**: [tollgate-pricing.md](core/tollgate-pricing.md), [peering-fips.md](network-peering/peering-fips.md)

---

## Summary

| # | Feature | Priority | Complexity |
|---|---------|----------|-----------|
| 1 | Per-peer forwarding policy (`local_only`/`full`) | Critical | Medium |
| 2 | Bloom filter exclusion (inferred from policy) | Critical | Medium |
| 3 | Per-peer traffic counters (expose existing `LinkStats`) | Critical | Low — data exists |
| 4 | Peer lifecycle callbacks | Critical | Low |
| 5 | MMP metrics native access | High | Low |
| 6 | FSP port dispatch | Future | Medium |
| 7 | Payment-aware routing | Future | High |
