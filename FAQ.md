# TollGate v2 Design FAQ

Questions and answers gathered during design review of the tollgate-rs protocol and its relationship to the TollGate TIPs (v1).

---

## Protocol Comparison: tollgate-rs vs. TIPs

### How does the tollgate-rs protocol work?

TollGate v2 is a hop-by-hop streaming micropayment system for metered resource delivery using Cashu Spilman payment channels. The flow:

1. **Discovery**: Peers find each other via FIPS mesh events, IP probing, or static config.
2. **Pricing**: Each peer publishes a take-it-or-leave-it PriceSheet — per-peer, per-product, per-mint. No negotiation.
3. **Bootstrap** (if needed): A peer with no mint connectivity pays with a regular Cashu token to get online.
4. **Channel establishment**: Each direction gets a unidirectional Spilman channel funded by the party that will owe payment. Two channels per peer pair.
5. **Streaming payment**: Every 5 seconds, both sides exchange MeteringReport messages with cumulative counters. Only the **net debtor** signs a single BalanceUpdate (interval netting).
6. **Access control**: Four states — None (blocked), Active (paying), ZeroPrice (free), Suspended (exhausted).
7. **Rollover**: At 80% capacity, a new channel opens alongside the old one. Channels start small and grow with the relationship (2x per rollover, capped).
8. **Settlement**: Either party can settle at any time. Default channel TTL is 1 hour.

### How is tollgate-rs different from the TIPs?

| Dimension | TIPs (v1) | tollgate-rs (v2) |
|---|---|---|
| Topology | Tree: customer → gateway → internet | Mesh/peering: every link is bilateral |
| Payment | Whole Cashu tokens per step | Spilman streaming channels, sub-sat precision |
| Session model | Discrete steps (allotment/debit) | Continuous streaming, cumulative counters |
| Pricing | Single advertised price per step | Per-peer, per-product, per-mint, dynamic |
| Negative pricing | Not possible | Supported — leaf nodes can pay relays |
| Data model | Nostr event kinds (10021, 1022, 21023) | CBOR-encoded messages (15 types) |
| Interface | Captive portal (human-facing) | Device-to-device autonomous |
| Scope | Network access only | Resource-agnostic core |

### What do they have in common?

- Cashu ecash as the payment primitive
- Bearer asset philosophy (no accounts, no KYC)
- Permissionless provision
- Multi-mint support
- Operator sovereignty
- Multi-hop as a stated goal

---

## Payment and Channel Mechanics

### How does the channel capacity growth work?

Channels start small (default: 10 sats initial capacity) and grow with the relationship:

```
First channel:    10 sats
After 1 rollover: 20 sats  (10 × 2.0 growth_factor)
After 2 rollovers: 40 sats
After 3 rollovers: 80 sats
...
After 10 rollovers: 10,000 sats (hits max_capacity cap)
```

The growth factor is configurable (default 2.0). The cap prevents over-committing to any single peer.

### What are the challenges with channel capacity growth?

**Rollover frequency at small capacities.** A 10-sat channel at 0.01 sat/unit covers 1,000 units — at 1 MB/s throughput, that's 1,000 bytes. The channel could exhaust in milliseconds. The rollover overhead (2 mint round-trips per rollover) at small capacities could exceed the value being moved.

**Netting mitigates this.** With interval netting, only the net debtor's channel drains per interval. For balanced bidirectional flows, even a tiny channel lasts a long time.

**There's a doc inconsistency.** The payment-channels doc says "First channel: 100 sats" with a 1,000 sat cap, but the configuration defaults say `initial_capacity: 10` and `max_capacity: 10000`. These need reconciliation.

### What is interval netting?

Each peer pair has two channels (one per direction). At each metering interval, both sides report usage and compute who owes what. Only the **net debtor** signs a balance update on their channel. This means only one channel drains per interval, extending channel life significantly for balanced flows.

### What is ChannelSync?

ChannelSync is a proposed (but unspecified) protocol message that would allow an online peer to share back channel state to a rebooted peer. Without it, a rebooted peer loses all earned income on incoming channels because it lost the only proof of earnings (the signed BalanceUpdate).

### Why does the design punt on ChannelSync?

1. **Not needed for v1.** The loss is bounded by channel capacity (starts small) and TTL (1 hour). Maximum loss = one channel's worth of earned income per peer.
2. **Trust surface.** The rebooted peer must trust state sent by the online peer. The online peer could send stale state or withhold ChannelSync entirely to reclaim the full incoming channel via refund.
3. **Economic incentive misalignment.** The online peer holds the rebooted peer's earnings and profits from withholding ChannelSync. Requiring voluntary cooperation against economic self-interest is a design concern without an enforcement mechanism.

---

## Pricing and Economics

### How does negative pricing work?

A node can set negative prices (e.g., `price_per_unit: -2`), meaning it **pays** peers to carry its traffic. This is useful for leaf nodes that need to subsidize relays to forward their outbound traffic.

### Isn't negative pricing an abuse vector?

Not really. The node burning funds to pay for transit is spending real sats. Unlike a Sybil attack where fake identities cost nothing, negative pricing costs money proportional to traffic volume. The benefits:

- **It funds the network.** Other nodes earn revenue from the negative-pricing node.
- **It incentivizes buildout.** Consistent demand for forwarding along a path signals that deploying infrastructure there is profitable.
- **Competitors can't be denied service.** Paying peers to carry your traffic doesn't prevent them from also carrying others'. The mesh isn't zero-sum bandwidth allocation.

The actual risk is routing distortion if FIPS implements payment-aware routing, but this is explicitly a future feature that requires careful design.

### How does price discovery work in a 5-hop mesh with no gateway?

It doesn't — by design. TollGate is hop-by-hop. The client only knows what its immediate neighbor charges. The client can't query end-to-end cost or choose a cheaper path.

Each hop independently sets its price. The end-to-end cost accumulates across hops. Nobody has visibility into the total path cost. The design's bet is that competitive pressure at each hop keeps prices reasonable, and physical topology provides enough alternatives for market forces to work.

The honest answer: in a pure local mesh without a gateway, the client pays whatever its immediate neighbor charges, with no way to know or control the end-to-end cost. Whether this works in practice is an empirical question.

### What happens if a relay's upstream cost exceeds what it charges downstream?

The relay has two choices, both supported by the protocol:

1. **Raise its price.** TollGate allows price changes at every metering interval (5 seconds), piggybacked on MeteringReport. The downstream peer must accept or close the channel.
2. **Drop the traffic.** Close the channel, set peer to Suspended. Access control stops delivery.

There's a lag: the relay discovers the upstream cost increase, then passes it downstream at the next interval. During that lag, the relay operates at a loss. The relay needs to price based on observed upstream cost, which may not reflect future cost.

### Does reputation help with mesh economics?

Partially. Relays with consistent delivery earn good reputation, attracting clients and premium pricing. Relays that drop packets get bad reputation and must lower prices.

But reputation doesn't solve end-to-end price discovery or margin stacking. It also requires persistent state and identity — the design currently has neither.

### Do relays need persistent identity (npub)?

For reputation: yes. A relay that rotates its npub starts with zero reputation. The economic incentive to build reputation only exists if the identity persists.

Clients (leaf nodes) can rotate freely — they're consumers, not sellers. Their value is payment, not identity.

---

## Multi-Egress Segmentation

### What is the multi-egress segmentation problem?

A router with multiple egress paths (fiber, LoRa, WiFi, LTE) faces divergent cost structures. A single TollGate instance averages costs across all paths. When one path is 25x more expensive than another (LoRa vs fiber), the averaging makes the cheap path uncompetitively expensive.

### What is the solution?

Run one TollGate + FIPS instance per egress interface. Each instance is a separate process with its own npub, FIPS daemon, pricing, and metering. Downstream peers see multiple independent mesh nodes and open separate payment channels with each.

See [peering-multi-egress.md](docs/design/network-peering/peering-multi-egress.md) for the full design document.

### How does Carrol know which peer (Eric or Dave) to route through?

Carrol doesn't know about destinations. The routing works through:

1. **FIPS handles reachability.** FIPS's mesh routing knows which peers can reach which destinations.
2. **TollGate provides cost preferences.** Through the future TollGate → FIPS routing API, Carrol's TollGate tells its FIPS to prefer the cheaper peer (Eric) when both can reach a destination.
3. **FIPS routes accordingly.** Destinations reachable through both → Eric (cheaper). Destinations reachable only through Dave → Dave.
4. **Carrol observes and prices.** Carrol sees which upstream channel is draining for each downstream peer and prices accordingly.

### Does Carrol need to expose per-destination pricing?

No. Carrol charges Alice and Bob each a single price per peering relationship. The per-peer pricing model already supports different rates. Alice sees one price from Carrol, Bob sees a different price. Neither sees the other's price.

### What if two customers share an edge but one has expensive hops and the other doesn't?

With per-interface instances, this is solved. Each traffic class has its own TollGate instance with its own pricing. Eric charges 2 sat/MB for fiber, Dave charges 50 sat/MB for LoRa. Alice's traffic goes through Eric, Bob's through Dave. Neither subsidizes the other.

Without per-interface instances (single TollGate instance), the shared upstream cost would be averaged, and both customers would see the same price regardless of which path their traffic actually takes.

### Do Eric and Dave's FIPS instances need to be isolated from each other?

No. Eric's FIPS and Dave's FIPS may peer with each other on localhost. This isn't harmful — traffic is unlikely to flow through the wrong egress because the economics don't make sense (no reason to send fiber-bound traffic through LoRa).

### Does this pattern cascade across multiple hops?

Yes. Each hop segments independently. If Frank also runs parallel instances for fiber and LoRa, Eric pays Frank's fiber instance and Dave pays Frank's LoRa instance. No coordination needed — each hop is independent bilateral relationships.

### When should you split into separate instances?

Split when egress path costs diverge significantly (2-3x or more). Keep a single instance when paths have similar cost structures. This is an operator decision, not a protocol constraint.

---

## IP Peering vs. FIPS Peering

### How does IP peering work?

Single `tollgate-net` binary using standard kernel tools:
- **Access control**: nftables/iptables firewall rules per peer IP
- **Metering**: nftables counter rules or interface-level byte counters
- **Discovery**: Listening (port 4747), platform-specific probing (DHCP, netlink), or static config
- **Authentication**: Unauthenticated by default. Optional challenge-response or WireGuard/mTLS
- **Metrics**: None by default. Optional ICMP ping for coarse RTT/loss
- **Transport**: HTTP polling or WebSocket on port 4747

### How does FIPS peering work?

`tollgate-net` + `fips` daemon communicating over FIPS control socket:
- **Access control**: Per-peer forwarding policy (`local_only`/`full`) via control socket
- **Metering**: FIPS livestreams per-peer rx/tx counters via control socket
- **Discovery**: Automatic via FIPS mesh protocol (Noise IK handshakes)
- **Authentication**: Automatic — Noise IK handshake authenticates every peer
- **Metrics**: Full MMP (SRTT, ETX, goodput, jitter, trends) via streaming subscription
- **Transport**: HTTP over FIPS IPv6 adapter (initially). Future: native FSP port

### IP pros and cons

**Pros**: Deployable today. No upstream dependencies. Single process. Standard kernel tools.

**Cons**: No automatic mesh. No rich metrics (limits dynamic pricing). No authentication by default. No automatic failover. Peer discovery doesn't scale to multi-hop.

### FIPS pros and cons

**Pros**: Full mesh routing. Cryptographic authentication. Encrypted by default. Rich link metrics for dynamic pricing. Bloom filter integration. Seamless multi-hop.

**Cons**: Blocked on 4 critical FIPS features that don't exist yet. Two-process operation. FIPS itself is under development.

### Recommendation

Start with IP peering (validates core protocol, deployable today). Migrate to FIPS as features land. The `ResourceAdapter` trait makes this clean — `IpAdapter` ships first, `FipsAdapter` follows.

---

## FIPS Integration

### What does TollGate need from FIPS?

Four critical features (blocking v1):

1. **Per-peer forwarding policy** — set `local_only` or `full` per peer. Without it, freeloading is unpreventable.
2. **Bloom filter exclusion** — hide unpaid peers from routing. Without it, traffic routes toward black holes.
3. **Per-peer traffic counter livestream** — metering. Without it, no billing.
4. **Peer lifecycle events** — know when peers appear/disappear. Without it, race conditions.

### What is the default forwarding policy for new peers?

`local_only`. This closes the race window between FIPS authenticating a peer and TollGate setting the access level. No traffic is forwarded until the operator explicitly allows it.

### Will TollGate tell FIPS which path to route through?

Yes, in the future. Through a routing API, TollGate will provide routing rules to FIPS. FIPS will implement those rules. This allows TollGate to make economically-informed routing decisions (prefer cheaper peers) while FIPS handles the mechanics of forwarding.

The exact interface is not yet specified — the FIPS team will design it.

---

## Specific Gaps for Implementation

### What's missing before code can be written?

**Must-have:**
- Working Cashu Spilman channel library (biggest blocker)
- FIPS control socket API specification (if targeting FIPS)
- Formal peer state machine (state transition table, error handling per state)
- Full CBOR schema for all 15 message types

**Should-have:**
- ChannelSync message spec (or accept bounded reboot loss)
- Test strategy (mock mints, simulated traffic, rollover during outage)
- Error recovery catalog (what happens when X fails in state Y)

### Is there a v1-to-v2 migration path?

No. The two versions use different ports, encodings, payment models, and transport protocols. They cannot interoperate. If both need to coexist, that's an open problem.

---

## The TIPs and Human-Facing UX

### What does the TIPs model have that tollgate-rs doesn't?

The TIPs have a human-facing design: captive portal, `/whoami` endpoint for device discovery, `/usage` for consumption tracking, QR code payment flows. tollgate-rs is device-to-device only. A human connecting a phone to a tollgate-rs mesh node has no captive portal or human-readable session info — a separate UI layer would be needed.

### Why doesn't tollgate-rs have a captive portal?

The design explicitly scopes it out: "Captive portal / user interface — TollGate is device-to-device. Human-facing UI is built on top, not inside." The assumption is that autonomous device-to-device payment is the primary use case, and any human-facing UX is a separate concern.
