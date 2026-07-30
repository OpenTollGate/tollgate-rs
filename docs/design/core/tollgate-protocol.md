# TollGate Protocol

This document specifies the wire protocol for communication between TollGate peers — the messages exchanged, their encoding, and their sequencing.

## Overview

The TollGate protocol is a set of messages exchanged between authenticated peers to negotiate pricing, establish payment channels, meter resource delivery, and settle balances. It is **transport-agnostic** — messages can travel over any bidirectional channel between peers (FIPS session, TCP socket, HTTP, custom transport). The implementation provides the transport; the protocol defines the messages.

**No handshake.** Peers are already authenticated out-of-band (by FIPS Noise IK, WireGuard, etc.). The TollGate protocol begins with an Announce message.

---

## Encoding

All messages use **CBOR** ([RFC 8949](https://www.rfc-editor.org/rfc/rfc8949)) encoding.

**Why CBOR over binary (FIPS-style)?**
- TollGate is transport-agnostic — messages may traverse different substrates. Self-describing format avoids custom parsers per transport.
- Variable-length fields (mint URLs, class names) are natural in CBOR, awkward in fixed binary.
- Well-supported in Rust (`ciborium`, `minicbor`), Go, Python, TypeScript — important for interop with Cashu Spilman ecosystem.
- Compact enough for constrained devices (ESP32). CBOR is more compact than JSON, comparable to Protocol Buffers for small messages.

**Why not JSON?**
- Larger on the wire. Parsing overhead on constrained devices.

**Why not FIPS-style binary?**
- TollGate messages contain variable-length strings (mint URLs, units, class names). Binary encoding for these is complex and fragile.
- FIPS binary encoding is optimized for fixed-structure, high-frequency, low-latency packets (TreeAnnounce, MMP reports). TollGate messages are infrequent (every 5s) and don't need that level of optimization.

### Message Structure

Each TollGate message is a CBOR map with a `type` field (integer) as discriminator:

```cbor
{
  0: <message_type>,    // u8 — message type tag
  ...                   // type-specific fields
}
```

Field keys are small integers (not strings) for compactness:

| Key | Name | Present in |
|-----|------|-----------|
| 0 | `type` | All messages |
| 1-9 | Type-specific fields | Varies |

How messages are framed on the wire depends on the transport — see [Transports](#transports).

---

## Transports

The protocol is transport-agnostic, but each transport has a concrete spec for how CBOR messages are framed, exchanged, and how peers detect failure. v1 defines two transports: **HTTP polling** and **WebSocket**. Both run on default port **4747**.

### HTTP polling

Suitable for open access (public hotspots) and constrained clients. Stateless on the wire — each request is a complete bidirectional exchange.

**Endpoint:** `POST /tollgate/v1/exchange`

**Content type:** `application/cbor` for both request and response.

**Framing:** request and response bodies each contain zero or more CBOR messages, each prefixed with a 2-byte little-endian length:

```
+------------+-----------------+------------+-----------------+----
|  len (LE)  |  CBOR message   |  len (LE)  |  CBOR message   | ...
+------------+-----------------+------------+-----------------+----
   2 bytes      <len> bytes       2 bytes      <len> bytes
```

An empty body is valid (no messages queued / nothing to send).

**Bidirectionality:** every POST is a full exchange. The request body carries messages from the client to the server; the response body carries messages the server has queued for the client since the last poll. There is no separate inbox endpoint.

**Identity:** the sender's pubkey is established by the Announce message (always the first message of a new session). The server tracks per-pubkey state keyed by that pubkey. There is no separate authentication header — TollGate runs on top of whatever transport-layer authentication the deployment uses (none, by default, in IP peering).

**Polling cadence:** the client polls at the negotiated metering interval (default: 5 seconds). When the client knows it is expecting an immediate response (e.g., during initial channel setup), it may poll more aggressively until the response arrives.

**Failure detection:** the server marks a peer disconnected if no poll is received within `3 × metering_interval`. The client detects server failure when an HTTP request fails (network error, 5xx). On either case, both sides clean up channel state per [Reboot / State Loss](tollgate-payment-channels.md#reboot--state-loss).

**Reconnection:** a new Announce starts a fresh session. If the server receives a duplicate-pubkey reconnection while it still holds state for that pubkey, the friendly path (sharing back channel state) applies — see the reboot section above.

### WebSocket

Suitable for higher-frequency exchanges, infrastructure peering, and lower-latency operation.

**Endpoint:** `GET /tollgate/v1/ws` (HTTP Upgrade to WebSocket)

**Framing:** each WebSocket binary frame contains exactly one CBOR message. No length prefix — the frame boundary delimits the message. Text frames are not used.

**Bidirectionality:** native. Either side may send a message at any time.

**Identity:** the client sends Announce as the first frame after connection open. The server replies with its own Announce.

**Failure detection:** WebSocket close frame, missing ping/pong (default: 30s ping interval, 90s timeout), or TCP-level disconnect. Both sides should send Disconnect (CBOR) before closing the WebSocket where possible, so the peer knows it was orderly.

**Reconnection:** opening a new WebSocket starts a fresh session. Same duplicate-pubkey reconnection path as HTTP polling.

---

## Message Types

| Type | Name | Direction | Purpose |
|------|------|-----------|---------|
| 0x00 | Announce | Bidirectional | "I am a TollGate node" — protocol version, pubkey |
| 0x01 | Offer | Bidirectional | Own mint, unit, direction classes, interval range, accepted mints |
| 0x02 | Accept | Bidirectional | Accept the offer, provide Spilman funding |
| 0x03 | ChannelReady | Bidirectional | Confirm Spilman channel funded and active |
| 0x04 | MeteringReport | Bidirectional | Unsigned resource stats for this interval |
| 0x05 | BalanceUpdate | Net debtor → creditor | Signed Spilman update for the net amount owed |
| 0x06 | BalanceAck | Net creditor → debtor | Confirm balance update accepted |
| 0x07 | *reserved* | — | Was BootstrapToken; bootstrap is removed |
| 0x08 | *reserved* | — | Was BootstrapAck; bootstrap is removed |
| 0x09 | RolloverInit | Sender → Receiver | New channel alongside exhausting one |
| 0x0A | RolloverReady | Receiver → Sender | New channel funded, ready |
| 0x0B | ChannelClose | Either → Either | Request cooperative close |
| 0x0C | CloseAck | Either → Either | Acknowledge close |
| 0x0D | Reject | Either → Either | Reject proposal (with reason) |
| 0x0E | Disconnect | Either → Either | Orderly teardown |

---

## Message Definitions

### 0x00 Announce

First message sent by each peer after network-layer authentication. Identifies the sender as a TollGate node, declares the protocol version, and signals which optional capabilities it supports.

```cbor
{
  0: 0x00,                         // type: Announce
  1: <protocol_version>,           // u8 — current: 1
  2: <pubkey>,                     // bytes(33) — sender's compressed secp256k1 public key
  3: <unit>,                       // text — "bytes", "wh", "ml", etc.
  4: <capabilities>,               // u32 — bitfield of supported capabilities
}
```

Both peers send Announce. If versions don't match, the peer with the lower version sends Reject. No other message exchange occurs before Announce.

**Capability bits** (field 4):

| Bit | Name | Meaning |
|-----|------|---------|
| `0x01`–`0x80000000` | reserved | Must be zero in v1. Reserved for future capabilities (e.g., FSP transport, batch settlement). |

Spilman support is universal in v1 — there is no per-token payment mode to signal. Zero-price peering and channel rollover fall out of the existing message set and need no capability bit either.

### 0x01 Offer

Sent by each peer after Announce. Declares where the sender's own vouchers come from, and which other mints' vouchers it will take.

There is no price for delivery: one voucher buys one unit
([tollgate-pricing.md](tollgate-pricing.md)). The only prices here are
per-accepted-mint — what this node will give for a voucher issued by that
mint, or charge to take it.

```cbor
{
  0: 0x01,                         // type: Offer
  1: <mint_url>,                   // text — this node's own mint
  2: <unit>,                       // text — "byte", "wh", "ml"
  3: [<class>, ...],               // array of text — direction classes, e.g. ["up", "down"]
  4: [<min_interval_ms>, <max_interval_ms>],  // [u32, u32] — metering interval range
  5: [                             // mints whose vouchers this node accepts
    {
      1: <mint_url>,               // text
      2: <price>,                  // i64 — scaled; > 0 we buy them,
                                   //   0 even swap, < 0 the holder pays us to take them
    },
    ...
  ],
  6: <price_scale>,                // u32 — divisor for prices; default 1000
  7: <sat_swap>,                   // bool — will swap sats for accepted vouchers on request
}
```

Field 1 is implicitly accepted at par and need not appear in field 5. An
empty field 5 means the node takes only its own vouchers, which is the
default.

Because the unit is the same network-wide, any mint's vouchers are
denominated identically — only the issuer differs, which is what the
per-mint price expresses. See Accepted Mints in
[tollgate-vouchers.md](tollgate-vouchers.md).

Keysets, and therefore per-class amounts, are fetched from each `mint_url` by
ordinary Cashu means (NUT-01/02). The protocol does not restate them.

Field 7 signals the node will sell its accepted vouchers for sats on request,
so a peer arriving with only sats need not go elsewhere first — see
[voucher-acquisition.md](../market/voucher-acquisition.md). The swap itself is
a Cashu operation against the node's mint, not a TollGate message.

The accepted-mint prices are the only fields that can change mid-session, and
only via MeteringReport.

### 0x02 Accept

Sent by the peer to accept the offer and fund its outgoing channel.

```cbor
{
  0: 0x02,                         // type: Accept
  1: [<min_interval_ms>, <max_interval_ms>],  // [u32, u32] — peer's interval range
  2: <channel_funding>,            // bytes — Spilman funding proofs (CBOR-encoded)
}
```

There is no product or option to echo back: the offer carries a single mint,
a single unit, and one voucher price.

The metering interval is resolved deterministically by both sides:
```
overlap = max(A.min, B.min) .. min(A.max, B.max)
interval = (overlap.start + overlap.end) / 2
```

If ranges don't overlap, the Accept is implicitly rejected.

### 0x03 ChannelReady

Sent after the receiver verifies funding proofs and the channel is active.

```cbor
{
  0: 0x03,                         // type: ChannelReady
  1: <channel_id>,                 // bytes(32) — Spilman channel ID
  2: <direction>,                  // u8 — 0 = A→B, 1 = B→A
}
```

Both peers send ChannelReady for their respective channel directions. Resource metering begins when both channels are ready.

### Metering Baseline

When a session starts (both ChannelReady messages exchanged), both sides reset their metering counters to zero. This establishes a shared baseline — if either node restarted, its counters were already at zero; if neither restarted, both agree to start fresh from this point.

All MeteringReport values are **cumulative since session start** (the ChannelReady baseline). Each side computes the interval delta as `current_cumulative - previous_cumulative`. This makes the protocol self-healing: if a MeteringReport is lost, duplicated, or delivered out of order, the next report still carries the correct totals — no data is lost and no sequence numbers are needed. The Spilman channel's cumulative balance is the authoritative payment record, and metering counters only need to be consistent within the current session.

### 0x04 MeteringReport

Sent by both peers at each metering interval. Contains **unsigned** resource stats only — no balance signature. This exchange allows both sides to compute the same cost and determine the net.

```cbor
{
  0: 0x04,                         // type: MeteringReport
  1: <elapsed_ms>,                 // u64 — milliseconds since session start (cumulative)
  2: [{<class>: <delivered>}, ...], // cumulative units we delivered TO this peer, per class
  3: [{<class>: <received>}, ...],  // cumulative units we received FROM this peer, per class
  4: [{1: <mint_url>, 2: <price>}, ...],  // updated accepted-mint prices, or null
}
```

Both peers send MeteringReport. Each side computes the interval delta (`current_cumulative - previous_cumulative`) per class. Once both reports are received, each side independently computes:
1. Units A delivered to B this interval — B owes A that many A-vouchers
2. Units B delivered to A this interval — A owes B that many B-vouchers
3. Any voucher-price settlement agreed under paid acceptance
4. Net per channel; the debtor sends BalanceUpdate, the creditor acks

Delivery itself needs no arithmetic — one voucher per unit
([tollgate-pricing.md](tollgate-pricing.md)) — so both sides trivially agree
on the amounts. This is deterministic.

**Field 4** is the only renegotiation mechanism left. A node that wants to change what it will give for some mint's vouchers — including dropping a mint from the accepted set — includes the revised entries; the peer accepts by continuing, or rejects with ChannelClose. Delivery cannot be repriced mid-session, because the peer already holds the vouchers and their claim is fixed.

### 0x05 BalanceUpdate

Sent by the **net debtor** after both MeteringReports have been exchanged. Contains the signed Spilman balance update for only the net amount owed.

```cbor
{
  0: 0x05,                         // type: BalanceUpdate
  1: <channel_id>,                 // bytes(32) — the debtor's Spilman channel
  2: <cumulative_balance>,         // u64 — new cumulative balance on this channel
  3: <balance_signature>,          // bytes(64) — Schnorr signature over balance update
  4: <net_amount>,                 // u64 — the net amount being charged this interval
}
```

### 0x06 BalanceAck

Sent by the creditor to confirm the balance update.

```cbor
{
  0: 0x06,                         // type: BalanceAck
  1: <channel_id>,                 // bytes(32)
  2: <accepted_balance>,           // u64 — the cumulative balance we acknowledge
}
```

### 0x07, 0x08 — reserved

These carried BootstrapToken and BootstrapAck. Bootstrap is removed: the
mint a peer needs is the peer it is already talking to, so mint reachability
is never the obstacle the mechanism existed for. How a peer acquires
vouchers is outside the protocol — see
[voucher-acquisition.md](../market/voucher-acquisition.md).

The type codes stay reserved rather than reused, so an old implementation
sending one gets a clean Reject instead of a misparse.

### 0x09 RolloverInit

Sent by the channel sender (the funder) when its outgoing channel approaches exhaustion (default: 80% capacity used). Each channel does carry shared state (cumulative balance, signatures), but rollover is initiated by the funder alone — only the party putting up new funds needs to decide when to do it. There is no leader/follower coordination across the two channels in a peer pair.

```cbor
{
  0: 0x09,                         // type: RolloverInit
  1: <old_channel_id>,             // bytes(32) — current exhausting channel
  2: <new_channel_funding>,        // bytes — Spilman funding proofs for new channel
}
```

### 0x0A RolloverReady

```cbor
{
  0: 0x0A,                         // type: RolloverReady
  1: <old_channel_id>,             // bytes(32)
  2: <new_channel_id>,             // bytes(32) — new Spilman channel ID
}
```

After RolloverReady, the old channel continues draining to 100%. Once exhausted, charges continue on the new channel seamlessly.

### 0x0B ChannelClose

Request cooperative close of a channel.

```cbor
{
  0: 0x0B,                         // type: ChannelClose
  1: <channel_id>,                 // bytes(32)
  2: <final_balance>,              // u64 — proposed final balance
  3: <final_signature>,            // bytes(64) — signature over final balance
  4: <reason>,                     // u8 — 0 = normal, 1 = price_rejected, 2 = peer_leaving
}
```

### 0x0C CloseAck

```cbor
{
  0: 0x0C,                         // type: CloseAck
  1: <channel_id>,                 // bytes(32)
  2: <accepted_balance>,           // u64 — agreed final balance
}
```

### 0x0D Reject

General-purpose rejection for any proposal.

```cbor
{
  0: 0x0D,                         // type: Reject
  1: <rejected_type>,              // u8 — type of message being rejected
  2: <reason_code>,                // u8 — machine-readable reason
  3: <reason_text>,                // text | null — human-readable reason
}
```

**Reason codes:**

| Code | Meaning |
|------|---------|
| 0x01 | Voucher price unacceptable |
| 0x02 | Mint not in the accepted set |
| 0x03 | Unit not accepted |
| 0x04 | Metering interval out of range |
| 0x05 | Channel funding invalid |
| 0x06 | Balance verification failed |
| 0x07 | Transit loss tolerance exceeded |
| 0x08 | Direction class not supported |
| 0x09 | Protocol version unsupported |
| 0x0A | Message type retired (bootstrap) |
| 0xFF | Other (see reason_text) |

### 0x0E Disconnect

Orderly teardown of the entire TollGate relationship.

```cbor
{
  0: 0x0E,                         // type: Disconnect
  1: <reason_code>,                // u8 — same codes as Reject
}
```

---

## Message Sequences

Both sides send Offer and Accept because each delivers independently. The interval flow is deterministic: delivery is one voucher per unit, so both sides compute the same amounts from the same metering data.

### Connection

![Normal Connection Sequence](diagrams/connection-sequence.svg)
<details><summary>Text version</summary>

```
  1. Identity
     A → B: Announce (v1, pubkey_A, capabilities)
     B → A: Announce (v1, pubkey_B, capabilities)

  2. Offer
     A → B: Offer (own mint, unit, classes, interval range, accepted mints)
     B → A: Offer (own mint, unit, classes, interval range, accepted mints)

  3. Channels
     B → A: Accept + funding (B→A channel)
     A → B: Accept + funding (A→B channel)
     B → A: ChannelReady (B→A)
     A → B: ChannelReady (A→B)

  4. Settle (repeat every metering interval)
     A → B: MeteringReport (cumulative delivered, received, per class)
     B → A: MeteringReport (cumulative delivered, received, per class)
     [both compute net]
     debtor → creditor: BalanceUpdate (signed)
     creditor → debtor: BalanceAck
```
</details>

A peer arrives already holding the other side's vouchers, or it gets no
service. There is no pre-channel phase.

### Voucher Price Change at Metering Interval

![Price Change](diagrams/price-change.svg)
<details><summary>Text version</summary>

```
  A wants to change what it will give for some mint's vouchers:

  A → B: MeteringReport (field 4: revised accepted-mint prices)

  alt: ACCEPT (continue)
       B → A: MeteringReport (continues normally)
       [next interval uses the new price]

  alt: REJECT (close)
       B → A: ChannelClose (reason=price_rejected)
       A → B: CloseAck
       [channel settles, B may renegotiate or disconnect]

  Delivery is never repriced — B already holds A's vouchers.
```
</details>

### Channel Rollover

![Channel Rollover](diagrams/rollover-sequence.svg)
<details><summary>Text version</summary>

```
  Channel B→A at 80% capacity:

  Sender → Receiver: RolloverInit (old channel + new funding)
  Receiver → Sender: RolloverReady (new channel ID)

  [old channel continues draining to 100%]
  [once exhausted, charges continue on new channel]
  [old channel settles with mint when possible]
```
</details>

### Zero-Price Peering

![Zero-Price Peering](diagrams/zero-price-peering.svg)
<details><summary>Text version</summary>

```
  A → B: Announce
  B → A: Announce

  A → B: Offer (zero-price peering)
  B → A: Offer (zero-price peering)
  B → A: Accept (no Spilman funding — zero price)
  A → B: Accept (no Spilman funding — zero price)

  [delivery active, no metering, no balance update messages]
```
</details>

---

## Protocol Versioning

The protocol version is declared in the Announce message (field 1). Both peers must support the same version. If versions don't match, the peer with the lower version sends Reject with reason code 0x09 (protocol version unsupported).

Version negotiation is outside scope for v1 — both peers must run the same version. Future versions may add a version negotiation step.

---

## Size Estimates

Typical message sizes (CBOR encoded):

| Message | Estimated size |
|---------|---------------|
| Announce | ~40 bytes |
| Offer (own mint only) | ~90 bytes |
| Offer (3 accepted mints) | ~200 bytes |
| Accept | ~180 bytes (dominated by Spilman funding) |
| ChannelReady | ~40 bytes |
| MeteringReport | ~70 bytes (two direction classes) |
| BalanceUpdate | ~110 bytes |
| BalanceAck | ~40 bytes |
| RolloverInit | ~200 bytes (Spilman funding) |
| ChannelClose | ~110 bytes |
| Disconnect | ~10 bytes |

These are infrequent messages (every 5s at the metering interval, one-time for setup). CBOR overhead is negligible compared to the resource being metered.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Encoding | CBOR (RFC 8949) | Compact, self-describing, handles variable strings/arrays, cross-platform |
| Framing | Per-transport — HTTP polling uses 2-byte LE length prefix; WebSocket uses frame boundaries | Each transport already has a natural boundary mechanism; reuse it |
| Field keys | Small integers, not strings | Compact, avoids string overhead in CBOR |
| Message discrimination | Integer `type` field (key 0) | Simple, extensible |
| First message | Announce (protocol version + pubkey) | Identifies TollGate capability before negotiation |
| Metering counters | Cumulative since session start, not deltas | Self-healing: lost/duplicated reports don't corrupt accounting |
| Interval flow | MeteringReport (both) → BalanceUpdate (net debtor only) → Ack | Deterministic netting, only net amount moves |
| Delivery pricing | None in the protocol — one voucher per unit | A voucher is a claim on one unit, so redemption is delivery |
| Accepted mints | A set per node, own mint implicitly at par, each foreign mint carrying a signed price | One unit of account network-wide means any mint's vouchers are usable; the price is where issuer risk is expressed |
| Accepted-mint price changes | Piggybacked on MeteringReport (field 4) | No extra round-trips; the only prices that can change mid-session |
| Sat swap | A boolean in Offer; the swap itself is a plain Cashu operation | Lets a peer arrive with only sats without adding a protocol phase |
| Bootstrap messages | Removed, type codes 0x07/0x08 left reserved | The mint a peer needs is the peer it is talking to; a retired code should Reject cleanly rather than misparse |
| Zero-price mode | Accept without funding, skip metering | Simplest path for free peering |
| Channel ownership | Each peer manages its own outgoing channel | Channels carry shared state, but rollover is initiated by the funder alone — only the party putting up new funds decides when to do it |
| Capability signaling | u32 bitfield in Announce (field 4), all bits reserved in v1 | Spilman is universal now that per-token payment is gone; the field stays for future use |
| Versioning | Single byte in Announce, must-match | Simple for v1, can add negotiation later |
| Reject | General-purpose with reason codes | One message type handles all rejection scenarios |
