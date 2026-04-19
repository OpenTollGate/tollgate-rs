# TollGate Peering: Traditional IP Networks

This document describes how TollGate operates on traditional IP networks — without FIPS or any mesh protocol.

## Overview

On a traditional IP network, TollGate peers connect over plain IP. There is no spanning tree, no bloom filters, no self-organizing topology. Peers are configured or discovered via simple mechanisms, and forwarding is handled by standard IP routing.

The typical topology is a **tree or chain**: an upstream provider sells connectivity to downstream customers, who may resell further. This is the classic ISP model — but with TollGate, every hop is independently priced and paid for with Cashu ecash.

```
Internet
    |
  [Gateway] ---- charges downstream peers
    |
  [Relay] ------ charges its downstream peers, pays gateway
    |
  [Client] ----- pays relay
```

Nothing prevents a mesh-like topology over IP (multiple peers, redundant paths), but without a mesh routing protocol, the implementation relies on the OS IP stack for routing decisions.

---

## Peer Authentication

### Unauthenticated (Open) — Default

The default IP peering mode is **unauthenticated**. The peer identifies itself only via the TollGate Announce message (pubkey). There is no transport-layer identity verification — the peer could be anyone.

This is the natural fit for TollGate's primary use case: a public gateway selling access to anyone willing to pay. The only protection needed is economic — a peer must pay (bootstrap token or Spilman channel) before any traffic is forwarded.

In unauthenticated mode:
- Any device can connect and start the TollGate protocol
- The peer's pubkey comes from the Announce message (self-declared)
- Risk is limited to the payment amount — no pay, no service
- A peer cannot impersonate another peer's payment channels — Spilman balance updates require a valid signature from the channel funder's private key, which an impersonator does not possess. However, a malicious peer could cause confusion during session setup by claiming a pubkey that already has an active session. Implementations should reject duplicate pubkey connections or require proof of key ownership (e.g., a challenge-response signed with the claimed key) in high-risk deployments

### Future: Authenticated Transports

For deployments that require transport-layer security or identity verification:

- **WireGuard**: Mutual authentication via public key exchange. Encrypted tunnel.
- **Mutual TLS**: Both sides present certificates. Suitable for infrastructure peering.

These are additive — they don't change the TollGate protocol, only the transport layer beneath it. TollGate core only sees peers identified by pubkey and does not enforce any particular authentication method.

---

## ResourceAdapter Implementation

### Access Control

On IP networks, access control is enforced via **firewall rules** (nftables, iptables, pf):

| Access level | Firewall action |
|-------------|----------------|
| `None` | Drop all forwarded traffic from/to this peer's IP. Allow traffic to local ports (TollGate protocol). |
| `Bootstrap` / `Active` | Allow forwarded traffic from/to this peer's IP. |
| `ZeroPrice` | Allow forwarded traffic from/to this peer's IP. |
| `Suspended` | Same as `None` — drop forwarded, allow local. |

The implementation maps `set_peer_access()` to firewall rule changes. The peer's IP address (from the TollGate protocol connection) is the identifier for firewall rules.

### Metering Counters

Per-peer metering counters are available from:
- **nftables/iptables accounting rules**: Per-peer byte/packet counts on the FORWARD chain, matching on source/destination IP
- **Interface-level counters**: If each peer has a dedicated interface (e.g., VLAN, tunnel), use interface rx/tx bytes

The implementation pushes cumulative counters via `MeterStream`:
- `delivered`: Units delivered to this peer's IP
- `received`: Units received from this peer's IP

For a simple deployment, per-IP nftables accounting rules are sufficient. The implementation adds a counter rule when a peer connects and reads it at each settlement interval.

### Peer Metrics

Traditional IP networks **do not provide FIPS MMP-style metrics**. The `peer_metrics()` method returns `None`.

If the operator wants dynamic pricing, the implementation can optionally provide:
- **Ping-based RTT**: Periodic ICMP pings to measure latency
- **Loss estimation**: From ping success rate
- **Static estimates**: Operator-configured values per peer

These are coarse approximations. Dynamic pricing on IP networks is less granular than on FIPS.

### Bloom Filter Visibility

Not applicable. There are no bloom filters on IP networks. The `set_peer_access()` call handles access control; bloom filter inference is a no-op in the IP adapter.

---

## Transport for TollGate Messages

TollGate protocol messages (CBOR-encoded) need a bidirectional channel between peers. On IP networks:

### HTTP Polling

The peer periodically polls the forwarder's HTTP endpoint for new messages and sends its own messages via POST.

- **Simple**: Works through NATs, proxies, and firewalls. Any HTTP client can participate.
- **Latency**: Polling interval adds delay. Acceptable for settlement intervals of 5s+.
- **Stateless**: Server doesn't need to maintain open connections per peer.
- Suitable for constrained clients and open-access hotspot scenarios.

### WebSocket

The peer opens a WebSocket connection to the forwarder. Messages flow bidirectionally over the persistent connection.

- **Real-time**: No polling delay. Messages delivered immediately.
- **Persistent**: Requires maintaining open connections per peer.
- Better for high-frequency settlement or large numbers of peers.

### Future: Tunnel-Based Transport

For authenticated deployments, TollGate messages can flow inside an encrypted tunnel (WireGuard, TLS). Already encrypted and authenticated — no additional framing needed beyond the standard 2-byte length prefix.

---

## Topology

### Tree (Most Common)

```
Internet
    |
  [A: Gateway]
    |       \
  [B: Relay]  [C: Relay]
    |
  [D: Client]
```

- A charges B and C for forwarding to the internet
- B charges D for forwarding to A (and beyond)
- Each peer-pair has independent TollGate pricing
- Routing is handled by the OS IP stack (default route points upstream)

### Chain

```
[A] --- [B] --- [C] --- [D]
```

Linear topology. Each node pays its neighbor to the left for forwarding.

### Multi-Homed

```
  [A: Gateway 1]    [B: Gateway 2]
       \                /
        \              /
         [C: Relay]
              |
         [D: Client]
```

C has two upstream peers and pays both. Traffic routes via OS routing table (policy routing, ECMP). TollGate doesn't influence routing — it meters and charges on each peer link independently.

---

## Peer Discovery and Configuration

Peers can be discovered **dynamically** or configured **statically**.

### Dynamic Discovery

The implementation detects new connections (e.g., a device joining the local network) and probes them for TollGate capability by attempting the TollGate protocol handshake (Announce message). If the peer responds with a valid Announce, a TollGate session begins.

For IP networks, this means:
- **Listening**: The node listens on a known port (e.g., 4747) for incoming TollGate connections
- **Probing**: When the implementation detects a new device (DHCP lease, ARP entry, etc.), it connects to the device's TollGate port and sends Announce. If the device responds, it's a TollGate peer.
- **Open access**: For public gateways, any device that connects and sends a valid Announce is accepted as a peer

### Static Configuration

The operator can also pre-configure known peers:

```yaml
peers:
  - pubkey: "02abc..."
    endpoint: "192.168.1.1:4747"

  - pubkey: "03def..."
    endpoint: "192.168.1.100:4747"
```

Each peer relationship is independently priced and negotiated. There is no concept of "upstream" or "downstream" at the TollGate level — pricing determines the economic direction.

Static and dynamic discovery can coexist — static peers are always attempted, dynamic peers are discovered as they appear.

---

## Differences from FIPS Peering

| Aspect | FIPS | IP |
|--------|------|-----|
| Peer discovery | Automatic (mesh protocol) | Dynamic probing + static config + open access |
| Authentication | Noise IK (built into FIPS) | Unauthenticated (default), WireGuard/TLS (future) |
| Routing | Spanning tree + bloom filters | OS IP routing table |
| Bloom filters | Yes — TollGate controls visibility | N/A |
| Per-peer metrics | MMP (SRTT, loss, ETX, goodput, jitter) | None by default (optional: ping, static) |
| Dynamic pricing | Rich metric inputs | Coarse or static |
| Traffic counters | FIPS forwarding stats | Firewall accounting rules |
| Access control | FIPS forwarding filter | Firewall rules (nftables/iptables) |
| Topology | Self-organizing mesh | Manual tree/chain/star |
| TollGate transport | FIPS session layer | HTTP polling, WebSocket |

---

## Limitations

- **No automatic failover**: If an upstream peer goes down, the operator must reconfigure routing. FIPS reroutes automatically.
- **No rich dynamic pricing**: Without MMP, dynamic pricing is limited to coarse estimates or static configuration.
- **Simpler peer discovery**: Dynamic probing works on local networks but doesn't scale like FIPS's mesh protocol for multi-hop discovery.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Authentication | Unauthenticated by default | Open access is the primary use case; payment is the gatekeeper |
| Access control | Firewall rules (nftables/iptables) | Standard IP mechanism, per-peer by IP |
| Metering counters | Firewall accounting or interface stats | Per-peer granularity via nftables rules |
| Peer metrics | None by default | No MMP equivalent; optional coarse metrics |
| Bloom filters | N/A (no-op) | Only exists in FIPS |
| Peer discovery | Dynamic probing, static config, or open access | Local network probing, not multi-hop |
| TollGate transport | HTTP polling (simple) or WebSocket (real-time) | Works without tunnels, through NATs |
| Routing | OS IP stack | Separation of concerns — TollGate handles payment, not routing |
