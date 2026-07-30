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
- Variable-length fields (mint URLs, units) are natural in CBOR, awkward in fixed binary.
- Well-supported in Rust (`ciborium`, `minicbor`), Go, Python, TypeScript — important for interop with Cashu Spilman ecosystem.
- Compact enough for constrained devices (ESP32). CBOR is more compact than JSON, comparable to Protocol Buffers for small messages.

**Why not JSON?**
- Larger on the wire. Parsing overhead on constrained devices.

**Why not FIPS-style binary?**
- TollGate messages contain variable-length strings (mint URLs, units). Binary encoding for these is complex and fragile.
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

## Protocol Boundary

The payment protocol and the market are deliberately separate, and the
separation is structural rather than a convention:

- **No amount in any TollGate message is denominated in money.** Messages
  count units and vouchers. Sats appear nowhere.
- **No TollGate message buys, sells or swaps a voucher.** Acquiring vouchers
  happens before a session and outside it.
- **Market operations use their own endpoints and their own protocol** — see
  [market-protocol.md](../market/market-protocol.md). They may be served by a
  different process, a different host, or a third party entirely.
- **A node that offers no market services is fully functional.** It sells its
  own vouchers through its Cashu mint, or does not sell them at all, and
  peers arrive holding what they need.

The Offer carries no price at all. It names which mints this node will take
payment in, and one unsigned multiplier saying how much a unit carried
*outward* on the peer's behalf counts. Neither is denominated in money.

---

## Transports

The protocol is transport-agnostic, but each transport needs a concrete spec
for how CBOR messages are framed, how peers detect failure, and how sessions
resume. **v1 defines one: raw TCP.** HTTP polling and WebSocket are recorded
below as future alternatives for the cases raw TCP cannot reach.

**The transport carries no security burden.** The end goal is FIPS, where a
Noise IK handshake mutually authenticates and encrypts every link before
TollGate sees the peer ([peering-fips.md](../network-peering/peering-fips.md)).
On plain IP without FIPS the operator chooses what to wrap the connection in —
nothing, TLS, WireGuard, mTLS — and accepts the trade-off; see Transport Layer
Security in [peering-ip.md](../network-peering/peering-ip.md).

### Raw TCP

Default port **4747**.

**Why this one first.** Peerings are adjacent by construction — hop-by-hop
payment means the counterparty is one link away, usually the LAN or WiFi the
peer just associated with. There are no proxies, TLS terminators or corporate
middleboxes in between, which is the usual argument for dressing a protocol up
as HTTP. Skipping that costs nothing here and saves a great deal on
constrained devices: no HTTP parsing, no SHA-1 or base64 for a WebSocket
upgrade, no frame parser with three payload-length encodings, no masking, no
ping/pong, no close handshake. Read two bytes, read the body, hand it to the
CBOR decoder.

The wire saving is incidental — a few bytes per message. The code saving is
the point.

**Framing:** each CBOR message is prefixed with a 2-byte little-endian length.

```
+------------+-----------------+------------+-----------------+----
|  len (LE)  |  CBOR message   |  len (LE)  |  CBOR message   | ...
+------------+-----------------+------------+-----------------+----
   2 bytes      <len> bytes       2 bytes      <len> bytes
```

The 2-byte prefix caps a message at 65535 bytes, which is a useful bound in
itself: a peer cannot announce a huge length and make the receiver allocate for
it. No TollGate message comes close to the cap.

**Bidirectionality:** native. Either side may send at any time.

**Identity:** each side sends Announce as its first message. Per-peer state is
keyed by the pubkey it carries.

**Failure detection:** no separate keepalive. MeteringReport is already sent
every metering interval, so it *is* the heartbeat — a peer that sends nothing
for `3 × metering_interval` is considered gone. Before channels are up there is
no such traffic, so during setup a peer that sends nothing for
`stale_timeout_seconds` (default 60) is dropped instead. Both knobs already
exist in [tollgate-configuration.md](tollgate-configuration.md).

**Orderly teardown:** send Disconnect, then close. A bare FIN is treated as an
unclean disconnect and triggers the same cleanup as a timeout — see
[Reboot / State Loss](tollgate-payment-channels.md#reboot--state-loss).

**Reconnection:** a new connection starts a fresh session with a new Announce.
If the listener still holds state for that pubkey, the friendly path applies
and it shares the channel state back.

### Future: HTTP polling

Raw TCP fails where something between the peers blocks an unusual port. That is
not the common case for adjacent peerings, but it is real for
internet-traversing infrastructure peering — a relay paying a known upstream
across the public internet — and for any deployment behind a hostile network.

The shape, if it is added: `POST /tollgate/v1/exchange`, `application/cbor`,
request and response bodies each carrying zero or more messages with the same
2-byte length framing. Every POST is a full exchange: the request carries
client-to-server messages, the response carries whatever the server has
queued. Client polls at the metering interval; the server marks a peer gone
after `3 × metering_interval` with no poll.

Being stateless on the wire, it also suits a client that cannot hold a
connection open.

### Future: WebSocket

For clients that must look like a browser, or reach through something that
only passes HTTP. `GET /tollgate/v1/ws` with an HTTP Upgrade, one CBOR message
per binary frame — the frame boundary replaces the length prefix. Liveness via
ping/pong rather than the MeteringReport heartbeat.

This is the only option that a browser can originate, since JavaScript cannot
open a raw socket. Whether that matters depends on whether a browser is ever a
TollGate peer; human-facing UI is currently a non-goal
([tollgate-intro.md](tollgate-intro.md)).

---

## Message Types

| Type | Name | Direction | Purpose |
|------|------|-----------|---------|
| 0x00 | Announce | Bidirectional | "I am a TollGate node" — protocol version, pubkey |
| 0x01 | Offer | Bidirectional | Preferred mint, accepted mints, unit, interval range, received multiplier |
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

Spilman support is universal in v1 — there is no per-token payment mode to signal. Free peering and channel rollover fall out of the existing message set and need no capability bit either.

### 0x01 Offer

Sent by each peer after Announce. Declares where the sender wants to be paid,
which other mints it will take, and how welcome the peer's outgoing traffic is.

```cbor
{
  0: 0x01,                         // type: Offer
  1: <preferred_mint>,             // text — pay me in this
  2: [<mint_url>, ...],            // text array — also accepted; may be empty
  3: <unit>,                       // text — "byte", "wh", "ml"
  4: [<min_interval_ms>, <max_interval_ms>],  // [u32, u32] — metering interval range
  5: <received_multiplier>,        // u16 — surcharge on units I receive from you,
                                   //   on top of you paying for what I deliver.
                                   //   Default 0 = no surcharge
}
```

**There is no price here.** Delivery is one voucher per unit
([tollgate-vouchers.md](tollgate-vouchers.md)), and what a voucher costs in
money is settled on the market. A mint is either accepted or it is not —
there is no haircut, because what an issuer's paper is worth is expressed in
what you pay for it, not in a discount applied at settlement.

The sender's **own** mint need not appear at all. A peer never needs it: it
funds its channel in `preferred_mint`, and this node pays the peer in whatever
the *peer* prefers. A pure pass-through relay can therefore name its
upstream's mint and never issue vouchers of its own.

The base rule is symmetric and needs no field: **each side pays for what it
received**, which is what the other delivered. Both owe, both fund a channel.

`received_multiplier` is a surcharge on top of that, for the case where a node
would rather not carry what a peer pushes at it. It defaults to `0`. Because
the peer is still paid for delivering, the **net** rate on its upload is
`m − 1`: `1` makes the upload free, `2` charges it at the same rate as a
download, `k + 1` charges it at `k` times
([tollgate-vouchers.md](tollgate-vouchers.md)).

It is **unsigned**. A negative surcharge would mean paying a peer on top of
already paying for its delivery, compounding into the sink hazard
([tollgate-hazards.md](tollgate-hazards.md)) — so it is unrepresentable rather
than merely forbidden.

Keysets are fetched from each mint by ordinary Cashu means (NUT-01/02). The
protocol does not restate them.

The multiplier is the only field that can change mid-session, via
MeteringReport. The accepted-mint set is fixed for the session, because a peer's
channel is funded in a specific mint and dropping it would strand the channel.

### 0x02 Accept

Sent by the peer to accept the offer and fund its outgoing channel.

```cbor
{
  0: 0x02,                         // type: Accept
  1: [<min_interval_ms>, <max_interval_ms>],  // [u32, u32] — peer's interval range
  2: <channel_funding>,            // bytes — Spilman funding proofs (CBOR-encoded)
}
```

There is nothing to echo back: the offer carries a single preferred mint, a
single unit, and one multiplier.

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

Sent by both peers at each metering interval. **Unsigned** resource stats only
— no balance signature.

```cbor
{
  0: 0x04,                         // type: MeteringReport
  1: <elapsed_ms>,                 // u64 — since session start (cumulative)
  2: <delivered>,                  // u64 — cumulative units delivered TO this peer
  3: <received>,                   // u64 — cumulative units received FROM this peer
  4: <new_received_multiplier>,    // u16 | null — revised multiplier
}
```

Counts are **raw**, cumulative since session start, with no multiplier applied.
That keeps the bill checkable: each side's `delivered` pairs with the other's
`received`, exactly as it always has.

Each side then computes what it owes from the **other side's** counts and the
multiplier that side quoted:

```
A owes B = B.delivered + B.received × B.received_multiplier
B owes A = A.delivered + A.received × A.received_multiplier
```

With both multipliers at `0` this reduces to each side paying for what it
received, which is the default and gives two channels. A surcharge above `1`
is what makes a peer pay net for both directions of its own traffic.

**Whether the two directions net depends on the mint.** If each side is paid in
a different mint the amounts are claims on different issuers and cannot be
subtracted: both sides send a BalanceUpdate. If both are paid in the same mint,
only the net debtor sends one. Both peers know both preferences from the Offer
exchange, so the choice is deterministic. See
[Netting](tollgate-payment-channels.md#netting).

**Field 4** is the only renegotiation left. A node raising what it charges for
traffic a peer pushes at it includes the new value; the peer accepts by
continuing, or rejects with ChannelClose. Delivery itself cannot be repriced —
the peer already holds the vouchers and their claim is fixed.

### 0x05 BalanceUpdate

Sent after both MeteringReports have been exchanged. Where the two directions settle in different mints, each side sends one on its own channel. Where they share a mint, only the net debtor sends one, for the net amount.

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
| 0x01 | Received multiplier unacceptable |
| 0x02 | Mint not in the accepted set |
| 0x03 | Unit not accepted |
| 0x04 | Metering interval out of range |
| 0x05 | Channel funding invalid |
| 0x06 | Balance verification failed |
| 0x07 | Transit loss tolerance exceeded |
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
     A → B: Offer (preferred mint, accepted mints, unit, interval, multiplier)
     B → A: Offer (preferred mint, accepted mints, unit, interval, multiplier)

  3. Channels
     B → A: Accept + funding (B→A channel)
     A → B: Accept + funding (A→B channel)
     B → A: ChannelReady (B→A)
     A → B: ChannelReady (A→B)

  4. Settle (repeat every metering interval)
     A → B: MeteringReport (cumulative delivered, received)
     B → A: MeteringReport (cumulative delivered, received)
     [both compute net]
     debtor → creditor: BalanceUpdate (signed)
     creditor → debtor: BalanceAck
```
</details>

A peer arrives already holding the other side's vouchers, or it gets no
service. There is no pre-channel phase.

### Multiplier Change at Metering Interval

![Price Change](diagrams/price-change.svg)
<details><summary>Text version</summary>

```
  A wants to change what it charges for traffic B pushes at it:

  A → B: MeteringReport (field 4: revised received multiplier)

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

### Free Peering

![Free Peering](diagrams/free-peering.svg)
<details><summary>Text version</summary>

```
  A → B: Announce
  B → A: Announce

  A → B: Offer (no charge)
  B → A: Offer (no charge)
  B → A: Accept (no Spilman funding — neither charges)
  A → B: Accept (no Spilman funding — neither charges)

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
| Offer (preferred mint only) | ~80 bytes |
| Offer (3 accepted mints) | ~180 bytes |
| Accept | ~180 bytes (dominated by Spilman funding) |
| ChannelReady | ~40 bytes |
| MeteringReport | ~60 bytes |
| BalanceUpdate | ~110 bytes |
| BalanceAck | ~40 bytes |
| RolloverInit | ~200 bytes (Spilman funding) |
| ChannelClose | ~110 bytes |
| Disconnect | ~10 bytes |

Plus 2 bytes of length prefix per message. These are infrequent (every 5 s at the metering interval, one-time for setup), so both the CBOR and the framing overhead are negligible compared to the resource being metered.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Encoding | CBOR (RFC 8949) | Compact, self-describing, handles variable strings/arrays, cross-platform |
| Transport | Raw TCP for v1; HTTP polling and WebSocket recorded as future alternatives | Peerings are adjacent, so nothing sits between the peers to require HTTP dressing. Saves HTTP parsing, an upgrade handshake, a frame parser, masking and ping/pong on constrained devices |
| Framing | 2-byte little-endian length prefix per message | Also caps a message at 65535 bytes, so a peer cannot make the receiver allocate for a claimed huge length |
| Transport security | None at this layer | FIPS Noise IK authenticates and encrypts before TollGate sees the peer; on plain IP the operator wraps the connection as it sees fit |
| Keepalive | None — MeteringReport is the heartbeat | It already runs every interval, so a separate ping would be redundant. Setup uses `stale_timeout_seconds` instead, since no metering traffic exists yet |
| Field keys | Small integers, not strings | Compact, avoids string overhead in CBOR |
| Message discrimination | Integer `type` field (key 0) | Simple, extensible |
| First message | Announce (protocol version + pubkey) | Identifies TollGate capability before negotiation |
| Metering counters | Cumulative since session start, not deltas | Self-healing: lost/duplicated reports don't corrupt accounting |
| Interval flow | MeteringReport (both) → BalanceUpdate | One update per direction when the mints differ; one net update when both sides settle in the same mint. Decided deterministically from the Offers |
| Delivery pricing | None in the protocol — one voucher per unit, both directions | A voucher is a claim on one unit, so redemption is delivery |
| Accepted mints | One preferred mint plus an optional accepted set; no prices | Accept or refuse is binary. What an issuer's paper is worth is expressed in what you pay for it on the market, not in a settlement discount |
| Who pays | Whoever the traffic is for, for both directions | One rule covers upload and download, so no reverse payment and no negative amount anywhere |
| Received multiplier | Unsigned u16 per peer, piggybacked for renegotiation | Prices scarce uplink and signals how welcome a peer's traffic is. Unsigned makes paying a peer to send traffic unrepresentable rather than merely forbidden |
| Metering counts | Unchanged: delivered and received, raw | Billing changed, measurement did not — both counters already existed and already mean the peer's download and upload |
| Market operations | Separate endpoints and a separate protocol; never TollGate messages | Buying and swapping vouchers is not part of paying for delivery, and a node that offers neither is fully functional |
| Money in the protocol | Never appears | Sats are a market concern; the payment protocol only ever counts units and vouchers |
| Bootstrap messages | Removed, type codes 0x07/0x08 left reserved | The mint a peer needs is the peer it is talking to; a retired code should Reject cleanly rather than misparse |
| Free mode | Accept without funding, skip metering | Simplest path for free peering |
| Channel ownership | Each peer manages its own outgoing channel | Channels carry shared state, but rollover is initiated by the funder alone — only the party putting up new funds decides when to do it |
| Capability signaling | u32 bitfield in Announce (field 4), all bits reserved in v1 | Spilman is universal now that per-token payment is gone; the field stays for future use |
| Versioning | Single byte in Announce, must-match | Simple for v1, can add negotiation later |
| Reject | General-purpose with reason codes | One message type handles all rejection scenarios |
