# TollGate Access Control

This document specifies how TollGate gates delivery per-peer based on payment status, meters resource usage, and enforces restrictions on unpaid peers.

## Overview

TollGate controls delivery at the peer level. Each peer has an **access level** determined by their payment status. The implementation (FIPS, IP stack, etc.) enforces access control on the delivery path — `tollgate-core` decides *what* to enforce, the resource adapter enforces *how*.

The core principle: **no pay, no delivery** — beyond the minimum flow allowance. A peer that hasn't paid gets at most the allowance: a small, free rate of delivery that lets it reach what it needs to start paying ([tollgate-vouchers.md](tollgate-vouchers.md)). If the node gives no allowance, an unpaid peer gets no delivery at all. Either way it can always exchange TollGate protocol messages with this node (Announce, Offer, Accept) to establish payment.

### What a TollGate session is

A **TollGate session** exists between two peers only while they have agreed a price and payment is flowing — a funded channel, or a grant it paid for still being delivered — or while they have agreed not to charge at all. The minimum flow allowance is **not** a session: it is what a connected peer gets outside one, so it can reach what it needs to start one. A peer that has never paid and a peer whose payment has lapsed are therefore in the same place, `None`.

The connection that carries TollGate messages is open before a session starts and stays open after one ends, so a peer can pay its way into a session without reconnecting. (In the code, `Sessions` and `PeerSession` track that connection from the moment a peer connects; they are wider than a TollGate session in this sense.)

---

## Access Levels

Each peer is in exactly one access level at any time:

| Level | Delivery | TollGate messages | Bloom filter visibility (FIPS) | When |
|-------|---------|-------------------|-------------------------------|------|
| `None` | Minimum flow allowance only; blocked if the allowance is zero | Allowed | Visible while carried at the allowance; hidden if blocked | No TollGate session: not paid yet, or payment lapsed |
| `Active` | Allowed (metered), never below the allowance | Allowed | Visible | Session: a channel funded, or a paid grant still running |
| `Free` | Allowed (unmetered) | Allowed | Visible | Session: this node does not charge the peer |

The access level says only whether delivery is allowed and how (metered or not). How much the peer has left to spend is tracked by the payment subsystem and does not surface as an access level.

### Transitions

```
None --> Active (Spilman channel funded)
None --> Free (this node does not charge the peer)
Active --> None (payment lapsed: last channel full, its grant run out)
Any --> None (disconnect)
```

![Access Level State Machine](diagrams/access-level-states.svg)
<details><summary>Text version</summary>

```
                ┌────────────────┐
                │      None      │  no session: allowance only
                └─┬──────▲─────┬─┘
      payment ok  │      │     │  not charged
                  ▼      │     ▼
           ┌──────────┐  │  ┌──────────┐
           │  Active  ├──┘  │   Free   │
           └──────────┘     └──────────┘
                  payment lapsed

  Active and Free are a TollGate session; None is not.
  Any state → None on disconnect
```
</details>

### None (Default)

Every newly connected peer starts at `None`, and a peer whose payment lapses returns to it. There is no TollGate session. The peer can exchange TollGate protocol messages — Announce, Offer, Accept — and is delivered at most the **minimum flow allowance** for or through this peer.

With a non-zero allowance, the peer's delivery is shaped to that rate. With the allowance at zero:
- Packets originating from this peer and addressed to other nodes are **dropped**
- Packets from other nodes destined for or through this peer are **not delivered to it**
- Only traffic to and from this node itself is allowed, which carries the TollGate protocol messages

### Active

A TollGate session with payment flowing: the peer has funded a channel to pay on. Delivery is allowed up to what it has bought, and its grant is drawn down as traffic passes.

Between grants — one expired, the next not yet bought — the peer stays `Active` and is shaped to the allowance. That is a pause in buying, not the end of the session: the channel is still funded, and the next TopUp restores the rate at once.

The session ends when payment lapses: the last channel is full, nothing replaces it, and the grant that channel paid for has run out. The peer returns to `None`.

### Free

Neither side charges the other — two nodes under one operator, or any pair whose operators decided not to. No payment infrastructure is needed: neither issues vouchers to the other, delivery is unmetered, and no metering or balance update messages are exchanged.

This is a decision about the relationship rather than a price set to zero, and each side decides only for itself. Where only one side charges, that side's peer sits in `Active` and funds one channel; `Free` covers the case where neither does.

**Free is not transitive.** It means free for *that peer's own traffic*, never free for anything that peer is nominally the beneficiary of — otherwise an uncharged peer becomes a way to launder free transit for others. See [tollgate-hazards.md](tollgate-hazards.md).

---

## What "Blocked" Means

When delivery is blocked (`None` with a zero allowance), the node:

1. **Stops resource delivery from this peer** — packets originating from the peer addressed to other nodes are silently dropped
2. **Does not deliver to this peer** — packets from other nodes destined for this peer are not delivered (they may be re-routed via other paths)
3. **Allows control messages** — packets from the peer addressed to *this node* are delivered (this is how TollGate protocol messages reach the node)
4. **Allows TollGate protocol messages** — the peer must be able to negotiate payment

Traffic addressed to or sent by this node itself is **never blocked**, whatever the peer's access level. It may be measured, but blocking it would cut off the payment that restores delivery.

The implementation decides how to enforce this. In FIPS, this could be a delivery filter that checks the peer's access level before delivering. In a traditional IP network, this could be firewall rules.

---

## Bloom Filter Visibility (FIPS)

In FIPS, bloom filters advertise reachability — "I can reach destination X through peer Y." If an unpaid peer is included in bloom filters, other nodes may route resources through it, only to have them blackholed at the gate.

**Rule: a peer is in the bloom filters exactly when its traffic is carried.** A peer held at the minimum flow allowance is carried — slowly — so it is advertised; only a peer whose delivery is blocked is hidden.

| Access level | Included in bloom filters? |
|-------------|---------------------------|
| `None` | Yes while carried at the allowance; no if the allowance is zero |
| `Active` | Yes — visible (FIPS) |
| `Free` | Yes — visible (FIPS) |

Bloom filter visibility is **inferred from whether the peer is carried** — the level together with its shaping rate — so it needs no separate API call and can never disagree with the gate.

This requires a FIPS modification — the ability to selectively include/exclude peers from bloom filter computation. See [FIPS_FEATURE_REQUESTS.md](../FIPS_FEATURE_REQUESTS.md).

---

## ResourceAdapter Trait (Access Control Members)

`tollgate-core` never enforces anything itself: it decides, and the host applies the decision through a `ResourceAdapter`. The trait belongs to the host (`tollgate-net`), not to core, so core stays free of I/O. Its access-control members:

```rust
pub trait ResourceAdapter: Send + Sync {
    /// Apply an access level decided by core. The implementation enforces
    /// delivery rules AND infers bloom filter visibility (FIPS): a peer whose
    /// traffic is carried, even only at the allowance, is visible; a blocked
    /// peer is hidden.
    fn set_access(&self, peer: PubKey, access: AccessLevel);

    /// Apply a shaping rate decided by core, in units per second. The rate
    /// already has the minimum flow allowance as its floor.
    fn set_shaping_rate(&self, peer: PubKey, rate: u64);

    // ... metering members documented in tollgate-metering.md
}

pub enum AccessLevel {
    /// No TollGate session. Minimum flow allowance only; TollGate messages flow.
    None,
    /// Session: delivery allowed, metered against funded Spilman channels.
    Active,
    /// Session: delivery allowed, unmetered. This node does not charge the peer.
    Free,
}
```

An adapter enforces two numbers per peer — the access level and the shaping rate — because a grant buys a rate: a gate alone cannot express what was sold.

Peers are always identified by public key. A delivery path that knows peers by some other address — an IP address, for a firewall — binds the key to that address itself, on the host side; one already keyed by public key, as FIPS is, needs no binding at all.

Counting units delivered and peer metrics are documented in [tollgate-metering.md](tollgate-metering.md).

---

## Access Control Flow

### New Peer Connects

```
1. Network layer authenticates peer (FIPS Noise IK, WireGuard, etc.)
2. Core sets access level to None
3. Peer and node exchange Announce
4. Peer and node exchange Offer
5. Peer sends Accept with channel funding
6. Access level transitions based on payment
```

### Spilman Channels Funded

```
1. Both peers send Accept with channel funding
2. Both verify funding proofs
3. Both send ChannelReady
4. Set access to Active (bloom visible in FIPS)
5. Each side buys grants on its own channel; delivery is shaped to what each has bought
```

### Payment Lapsed

```
1. The last channel fills, nothing replaces it, and the grant it paid for runs out
2. Set access to None: the session has ended
3. Shape the peer to the allowance, still visible in FIPS — or block and hide it, if the allowance is zero
4. Peer funds a new channel and buys a grant: back to Active
```

A grant expiring while a channel is still funded is not a lapse. The peer stays `Active`, shaped to the allowance until it buys again.

---

## Scope and Future Work

The access level governs **outbound delivery** — whether we forward, transit, or deliver to the peer. It is determined by the peer's payment to us.

The reverse direction — whether we *accept* what the peer delivers to us — is not modeled by the access enum today. Locally-addressed packets from a peer are always accepted, and our payment to the peer (which gates whether we are buying from them) lives in the wallet/channel state, not in the access enum. This works in practice but leaves the asymmetric case (e.g., we paid them, but they stopped paying us) implicit.

A future revision may replace the enum with a directional model that captures both directions in a single type:

| State | Outbound (we deliver) | Inbound (we accept) |
|---|---|---|
| `None` | Blocked | Blocked |
| `InboundOnly` | Blocked | Allowed (we pay them) |
| `OutboundOnly` | Allowed (they pay us) | Blocked |
| `Full` | Allowed | Allowed |
| `Free` | Allowed (unmetered) | Allowed (unmetered) |

This makes the asymmetric case (`InboundOnly` / `OutboundOnly`) a first-class state, simplifies bloom-filter-inclusion logic, and maps cleanly to FIPS forwarding policy variants. Out of scope for v1 — flagged for future design work.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Default access | None (allowance only) | No pay, no service beyond the allowance |
| TollGate session | Agreed price with payment flowing, or agreed not to charge; the allowance is not one | The allowance is how a peer reaches a session, not a kind of session |
| Unpaid resources | Minimum flow allowance + traffic to this node | Peer must be able to reach what it needs to start paying |
| Traffic to this node | Never blocked | Blocking it would cut off the payment that restores delivery |
| Bloom filter visibility | Inferred from whether the peer is carried (FIPS) | No separate API, and it can never disagree with the gate |
| Free peers | Skip all payment, go to Free; not transitive | Simplest path for free peering; transitivity would launder free transit |
| Lapsed payment | Back to None | The session has ended; an unpaid peer is an unpaid peer, whatever its history. It can still negotiate, so it recovers without reconnecting |
| Bloom visibility on the allowance | Visible | A peer that is carried but hidden has an allowance nobody can route to |
| Protocol messages | Always allowed regardless of access level | Payment negotiation must work even when blocked |
