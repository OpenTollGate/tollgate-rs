# TollGate Peering: FIPS Mesh Networks

This document describes how `tollgate-net` integrates with [FIPS](https://github.com/nicobao/fips) (Free Internetworking Peering System) — the **ideal** deployment target. FIPS provides everything TollGate wants from a network layer: cryptographic peer authentication, encrypted forwarding, self-organizing mesh routing, and rich per-link metrics. The doc covers the FIPS-specific `ResourceAdapter` implementation, how `tollgate-net` hooks into FIPS internals, and what FIPS modifications are required.

## Overview

FIPS provides everything TollGate needs from a network layer:
- **Peer authentication**: Noise IK handshakes mutually authenticate every peer
- **Encrypted forwarding**: All traffic is encrypted hop-by-hop (FMP) and end-to-end (FSP)
- **Self-organizing topology**: Spanning tree + bloom filters for routing without central coordination
- **Per-peer metrics**: MMP provides SRTT, loss, ETX, goodput, jitter per link
- **Session layer**: FSP provides port-based service dispatch for TollGate messages

`tollgate-net` and the FIPS daemon are independent binaries communicating over FIPS's control socket. FIPS exposes generic per-peer capabilities (forwarding policy, lifecycle events, livestreamed rx/tx counters, MMP metrics); `tollgate-net` consumes them.

Per-peer counters are pushed as a livestream subscription, so `tollgate-net` always has fresh values at metering-interval snapshot time. At a 5-second metering interval, socket overhead is negligible.

---

## Integration Points

TollGate hooks into FIPS at five points:

![FIPS Integration Points](diagrams/fips-integration.svg)
<details><summary>Text version</summary>

```
  ┌──────────────── FIPS Node ────────────────────┐
  │                                                │
  │  ┌───────────┐  ┌───────────┐  ┌───────────┐  │
  │  │Forwarding │  │  Bloom    │  │   MMP     │  │
  │  │  policy   │  │  Filter   │  │  Metrics  │  │
  │  └─────┬─────┘  └─────┬─────┘  └─────┬─────┘  │
  │       ①│              ②│             ③│        │
  │  ┌─────┴───────────────┴──────────────┴─────┐  │
  │  │       TollGate FIPS Adapter              │  │
  │  └─────┬────────────────────────────┬───────┘  │
  │       ④│                           ⑤│          │
  │  ┌─────┴─────┐              ┌──────┴──────┐   │
  │  │   Peer    │              │    IPv6     │   │
  │  │ Lifecycle │              │   Adapter   │   │
  │  └───────────┘              └─────────────┘   │
  └────────────────────────────────────────────────┘

  ① Per-peer forwarding policy (local_only / full)
  ② Bloom filter exclusion (inferred from policy)
  ③ MMP metrics for dynamic pricing
  ④ Peer connect/disconnect callbacks
  ⑤ TollGate message transport (HTTP over IPv6)
```
</details>

### 1. Per-Peer Forwarding Policy

TollGate sets a per-peer forwarding policy in FIPS. For blocked peers (`None`, `Suspended`), FIPS restricts the peer to **local-only traffic**:

- Traffic **from** this peer addressed **to this node**: Allowed (the peer can still communicate with us — TollGate protocol, payment negotiation)
- Traffic **from** this peer addressed **to other nodes** (transit): Dropped
- Traffic **from other nodes** destined **to or through** this peer: Not forwarded to this peer

For allowed peers (`Active`, `ZeroPrice`), FIPS forwards normally — no restrictions.

This is a data-plane policy, not a control-plane hook. FIPS simply needs to know: "for peer X, restrict to local-only" or "for peer X, forward normally."

**Required FIPS change**: Ability to set a per-peer forwarding policy — `local_only` (blocked) or `full` (allowed). The **default policy for new peers must be `local_only`** — so that a newly connected peer doesn't get full forwarding in the window between FIPS authenticating it and TollGate detecting it. FIPS enforces this in its existing forwarding path.

### 2. Bloom Filter Exclusion

TollGate controls which peers appear in bloom filter computation. Unpaid peers (`None`, `Suspended`) are excluded — their node_addr is not added to the bloom filter advertised to other peers. This prevents traffic from being routed toward a peer that will have it dropped at the gate.

When a peer's access level changes:
- `None` -> `Active`/`ZeroPrice`: Add to bloom filters immediately, trigger FilterAnnounce
- `Active` -> `Suspended`: Remove from bloom filters **after a delay** (default: 30 seconds) to avoid flapping. If the peer recovers (tops up, funds new channel) within the delay, the removal is cancelled and the peer stays visible. This prevents rapid bloom filter churn when a peer temporarily exhausts balance.
- `Suspended` -> `Active`: Re-add to bloom filters immediately (cancel any pending removal)

**Required FIPS change**: An API to include/exclude specific peers from bloom filter computation, inferred from the forwarding policy.

### 3. MMP Metrics Feed

TollGate consumes FIPS MMP metrics for dynamic pricing. Per-peer metrics are available after the Noise IK handshake completes and MMP starts reporting:

| MMP Metric | TollGate use |
|-----------|-------------|
| `srtt_ms` | Latency-based pricing adjustment |
| `loss_rate` | Loss-based pricing (wasted forwarding effort) |
| `etx` | Direct forwarding cost metric |
| `smoothed_etx` | Stable cost baseline |
| `goodput_bps` | Capacity utilization / congestion signal |
| `jitter` | Service quality indicator |
| Trend indicators | Predict near-future conditions |

The adapter subscribes to MMP metric updates over the FIPS control socket and exposes them via `peer_metrics()`. The subscription pushes per-peer state changes; `tollgate-net` reads the latest cached value when the pricing engine asks.

### 4. Peer Lifecycle Events

FIPS notifies TollGate when peers connect and disconnect:

- **Peer authenticated** (Noise IK handshake complete): TollGate creates a new peer state, sets access to `None`, begins TollGate protocol exchange
- **Peer disconnected** (link lost or orderly disconnect): TollGate cleans up peer state, closes any open channels, queues settlement

**Required FIPS change**: Callbacks for peer connect/disconnect events, providing the peer's public key and node_addr.

### 5. TollGate Protocol Transport

Initially, TollGate protocol messages travel over **HTTP through the FIPS IPv6 adapter**. FIPS provides an IPv6 TUN interface (`fips0`) that maps each peer's npub to an `fd00::/8` address. TollGate uses HTTP polling or WebSocket over this IPv6 interface — the same transport options as IP peering, but riding on the FIPS mesh.

This approach works today without any FIPS modifications to the session layer.

**Future**: When FSP port-based service dispatch is available, TollGate can register on a dedicated FSP port for more efficient message delivery — eliminating HTTP overhead. This is an optimization, not a requirement for the initial implementation.

---

## ResourceAdapter Implementation

### set_peer_access()

Maps TollGate access levels to FIPS forwarding policy and bloom filter state:

```rust
fn set_peer_access(&self, peer: &Pubkey, access: AccessLevel) -> Result<(), AdapterError> {
    let node_addr = NodeAddr::from_pubkey(peer);

    match access {
        AccessLevel::None | AccessLevel::Suspended => {
            // Restrict peer to local-only traffic (no transit forwarding)
            // Bloom filter exclusion is inferred — restricted peers are excluded
            self.node.set_peer_forwarding_policy(node_addr, ForwardingPolicy::LocalOnly);
        }
        AccessLevel::Active | AccessLevel::ZeroPrice => {
            // Allow full forwarding for this peer
            // Bloom filter inclusion is inferred — allowed peers are included
            self.node.set_peer_forwarding_policy(node_addr, ForwardingPolicy::Full);
        }
    }
    Ok(())
}
```

FIPS enforces the policy in its existing forwarding path. `LocalOnly` means only traffic addressed to this node is accepted from the peer; all transit is dropped.

### subscribe_meter()

FIPS livestreams per-peer rx/tx byte counters over the control socket. The adapter subscribes once per peer and wraps the stream as a `MeterStream`:

```rust
fn subscribe_meter(&self, peer: &Pubkey) -> Result<MeterStream, AdapterError> {
    let node_addr = NodeAddr::from_pubkey(peer);

    // Subscribe to FIPS's per-peer counter livestream over the control socket.
    // Each push updates the watch channel; tollgate-core snapshots at metering interval.
    let (delivered_tx, delivered) = watch::channel(0);
    let (received_tx, received) = watch::channel(0);
    self.subscribe_peer_counters(node_addr, delivered_tx, received_tx);

    Ok(MeterStream { delivered, received })
}
```

**Required FIPS change**: control-socket subscription that livestreams per-peer rx/tx byte counts. FIPS already tracks `LinkStats` internally — needs a generic per-peer counter feed exposed on the socket.

### peer_metrics()

Direct read from the peer's MMP state:

```rust
fn peer_metrics(&self, peer: &Pubkey) -> Option<PeerMetrics> {
    let node_addr = NodeAddr::from_pubkey(peer);
    let mmp = self.node.get_peer_mmp(node_addr)?;

    Some(PeerMetrics {
        srtt_ms: Some(mmp.srtt_ms),
        loss_rate: Some(mmp.loss_rate),
        etx: Some(mmp.smoothed_etx),
        goodput_bps: Some(mmp.goodput_bps),
        jitter_ms: Some(mmp.jitter as u32),
    })
}
```

Backed by a streaming subscription on the FIPS control socket, not per-call polling.

---

## Peer Identification

FIPS peers are identified by:
- **Public key**: secp256k1 compressed public key (33 bytes) — same as TollGate's peer identifier
- **node_addr**: SHA-256 hash of public key, truncated to 16 bytes — used in packet headers and bloom filters

The adapter maps between the two as needed. TollGate protocol uses pubkey; FIPS forwarding uses node_addr. The mapping is deterministic (`node_addr = SHA256(pubkey)[..16]`).

---

## Dynamic Pricing with MMP

FIPS MMP provides the richest metric set of any TollGate deployment target. The pricing engine can use:

### Cost-Plus Pricing (Recommended Default)

```
price = base_price x etx x (1 + srtt_ms / 100)
```

This mirrors FIPS's own link cost formula (`link_cost = etx x (1 + srtt_ms / 100)`). Higher ETX or latency = higher forwarding cost = higher price. The operator sets `base_price`; the formula scales it by actual link quality.

### Congestion-Aware Pricing

```
if goodput_trend == "falling" and loss_trend == "rising":
    price *= congestion_multiplier  // e.g., 1.5x
```

When MMP detects degrading link quality (falling goodput, rising loss), the node raises prices to reduce demand. As conditions improve, prices drop back.

### Quality-Tiered Pricing

Use MMP metrics to classify link quality and apply different product prices:

| Quality tier | Conditions | Price |
|-------------|------------|-------|
| Premium | loss < 1%, SRTT < 10ms | Highest |
| Standard | loss < 5%, SRTT < 50ms | Medium |
| Economy | loss < 10%, SRTT < 200ms | Lowest |
| Degraded | loss >= 10% or SRTT >= 200ms | Minimum / negative |

---

## FIPS Feature Requests Summary

The following FIPS modifications are required for TollGate integration. Full details in [FIPS_FEATURE_REQUESTS.md](../FIPS_FEATURE_REQUESTS.md).

| Feature | Purpose | Priority |
|---------|---------|----------|
| Per-peer forwarding policy | Set `local_only` or `full` forwarding per peer (default: `local_only`) | Critical |
| Bloom filter exclusion | Withhold unpaid peers from bloom filter computation | Critical |
| Per-peer traffic counters | Outbound/inbound byte counts per peer | Critical |
| Peer lifecycle callbacks | Notify on peer connect/disconnect | Critical |
| MMP metrics access | Direct read of per-peer MMP state | High |
| Future: FSP port dispatch | TollGate messages on native FSP port | Low (future, optimization) |
| Future: payment-aware routing | Well-paying peers get favorable routing | Low (future) |

---

## See Also

For routers with multiple egress paths of divergent cost (fiber + LoRa, WiFi + LTE, etc.), see [peering-multi-egress.md](peering-multi-egress.md) for the per-interface instance deployment pattern that keeps traffic class pricing independent.

---

## Differences from IP Peering

| Aspect | FIPS | IP |
|--------|------|-----|
| Integration | `tollgate-net` and `fips` are separate binaries; communicate over the FIPS control socket | `tollgate-net` standalone binary; uses kernel firewall + accounting |
| Forwarding policy | FIPS per-peer `local_only`/`full` (control socket) | nftables/iptables rules |
| Bloom filters | Controlled by `tollgate-net` (control socket) | N/A |
| Metering counters | Livestreamed by FIPS per-peer (control socket) | Firewall accounting |
| Metrics | MMP (SRTT, loss, ETX, goodput, jitter) — control-socket subscription | None / coarse |
| Peer discovery | Automatic (FIPS mesh protocol) | Dynamic probing / static |
| Authentication | Noise IK (automatic) | Unauthenticated (default) |
| Message transport | HTTP over IPv6 adapter (initially), FSP port (future) | HTTP polling / WebSocket |
| Control plane overhead | Negligible at 5s metering interval; livestreamed counters | Per-peer firewall rule installs/removes |

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Integration model | Separate binaries; `tollgate-net` talks to FIPS over the control socket | FIPS exposes generic capabilities; independent release cycles |
| Counter delivery | FIPS livestreams per-peer rx/tx over the control socket | `tollgate-net` always has a fresh value at metering-interval snapshot time without polling |
| Forwarding policy | Per-peer `local_only` or `full`, enforced by FIPS | Simple data-plane policy, not a control-plane hook |
| Default new-peer policy | `local_only` | Closes race window between FIPS auth and TollGate detection |
| Bloom filter control | Inferred from forwarding policy, with 30s removal delay | Prevents flapping on temporary balance exhaustion |
| Metering counters | Per-peer watch channels from FIPS | Continuous push, snapshot at metering interval |
| Metrics | Streaming subscription on the control socket | Pricing engine reads cached values; no per-call IPC |
| Message transport (initial) | HTTP over FIPS IPv6 adapter | Works today, no FIPS session layer changes needed |
| Message transport (future) | Native FSP port | Optimization, eliminates HTTP overhead |
| Default pricing strategy | Cost-plus using ETX and SRTT | Mirrors FIPS's own link cost formula |
| Peer identification | pubkey <-> node_addr mapping | Deterministic, same keypair serves both |
