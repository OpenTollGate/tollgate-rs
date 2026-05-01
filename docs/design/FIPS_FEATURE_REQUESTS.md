# FIPS Feature Requests for TollGate Integration

This document consolidates all FIPS modifications required for TollGate v2 integration. `tollgate-net` and FIPS run as independent binaries and communicate over FIPS's control socket. Each feature below is framed as a generic capability FIPS exposes on that socket. Each feature is referenced from the relevant TollGate design doc.

---

## Critical (Required for v1)

### 1. Per-Peer Forwarding Policy

**What**: Control-socket command to set a per-peer forwarding policy — `local_only` or `full`.

**Behavior**:
- `local_only`: Only accept traffic FROM this peer addressed TO this node. Drop all transit traffic (addressed to other nodes) from this peer. Do not forward traffic from other nodes to this peer.
- `full`: Normal forwarding — no restrictions.

**Default for new peers must be `local_only`** — closes the race window between FIPS authenticating a peer and `tollgate-net` setting the access level. No traffic is forwarded for a peer until the operator (`tollgate-net` or any other consumer) explicitly allows it.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 2. Bloom Filter Exclusion

**What**: Bloom filter inclusion is inferred from the forwarding policy — `local_only` peers are excluded; `full` peers are included. This is a derived behavior of the policy from feature 1, not a separate API.

**Behavior**:
- Peers with `local_only` policy are excluded from outbound bloom filters (their node_addr is not advertised to other peers)
- Peers with `full` policy are included normally
- When a peer transitions from `full` to `local_only`, removal should be **delayed by 30 seconds** to avoid bloom filter flapping when a peer temporarily exhausts its payment balance. If the peer recovers within the delay, the removal is cancelled.
- When a peer transitions from `local_only` to `full`, inclusion is immediate.

**Why**: If an unpaid peer appears in bloom filters, other nodes may route traffic through it, only to have it dropped at the gate — wasting bandwidth and causing delivery failures.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-access-control.md](core/tollgate-access-control.md)

---

### 3. Per-Peer Traffic Counter Livestream

**What**: Control-socket subscription that livestreams per-peer rx/tx byte counts.

**Current state**: FIPS **already tracks per-peer link stats** via `LinkStats` on each peer (`peer.link_stats().bytes_sent`, `peer.link_stats().bytes_recv`). These count all link-layer bytes sent/received per peer, which is exactly what TollGate needs (all bytes are metered, including protocol overhead — negligible).

**What's needed**: Add a control-socket subscription that pushes per-peer counter updates as they change (or at a reasonable rate, e.g., once per second). A consumer subscribes once per peer and receives a stream of `{node_addr, bytes_sent_total, bytes_recv_total}` updates. `tollgate-net` snapshots the latest received value at every metering interval — no polling needed.

**Complexity**: Low — the data already exists internally, just needs to be exposed as a streaming subscription on the socket.

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md), [tollgate-metering.md](core/tollgate-metering.md)

---

### 4. Peer Lifecycle Events

**What**: Control-socket event stream announcing peer connect / disconnect.

**Events**:
- **Peer authenticated**: emitted after Noise IK handshake completes. Provides the peer's compressed public key (33 bytes) and node_addr (16 bytes).
- **Peer disconnected**: emitted when a peer link is lost (timeout, orderly disconnect, or error). Provides the same identifiers.

**Why**: `tollgate-net` (or any external consumer) needs to create per-peer state on connect (set initial `local_only` policy, begin protocol exchange) and clean up on disconnect (close channels, queue settlement).

**Referenced in**: [peering-fips.md](network-peering/peering-fips.md)

---

## High Priority

### 5. MMP Metrics Subscription

**What**: Control-socket subscription that streams per-peer MMP (Metrics Measurement Protocol) state changes.

**Metrics needed**:
- `srtt_ms` (smoothed round-trip time)
- `loss_rate` (packet loss fraction)
- `smoothed_etx` (expected transmission count)
- `goodput_bps` (throughput in bytes/sec)
- `jitter` (latency variance)
- Trend indicators (rising/falling/stable) for RTT, loss, goodput

**Current state**: These metrics exist in FIPS and are queryable on-demand via the existing `show_mmp` control-socket command. For TollGate, an on-demand query at every metering interval would work but is wasteful when the values change continuously.

**What's needed**: A subscription mode on the existing socket — consumer subscribes once and receives pushed updates as MMP state changes (or at a coalesced rate). `tollgate-net` keeps the latest value cached and reads it when the pricing engine asks. The `show_mmp` query mode can stay alongside for tooling.

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
| 3 | Per-peer traffic counter livestream (control socket) | Critical | Low — data exists |
| 4 | Peer lifecycle event stream (control socket) | Critical | Low |
| 5 | MMP metrics streaming subscription | High | Low |
| 6 | FSP port dispatch | Future | Medium |
| 7 | Payment-aware routing | Future | High |
