# TollGate Payment Channels

This document specifies how TollGate manages Cashu Spilman payment channels between peers — the channel lifecycle, rollover mechanics, offline resilience, and the ChannelBackend trait.

What peers pay each other with is documented in [tollgate-vouchers.md](tollgate-vouchers.md). Under vouchers, channels exist to keep the issuer's spent-proof set bounded rather than to prevent theft.

## Overview

Each pair of TollGate peers maintains **two unidirectional Spilman channels** — one per direction. Each is funded by the party that owes, and by default both sides owe: each pays for what it received from the other. A channel is absent only where a node has decided not to charge that peer at all.

![Channel Pair Structure](diagrams/channel-pair.svg)
<details><summary>Text version</summary>

```
  Peer A                                          Peer B
  ┌──────────┐                              ┌──────────┐
  │ receiver │←────── B delivers to A ──────│ sender   │
  │ on A→B   │╌╌ Channel A→B: A pays B ────→│ on A→B   │
  │          │                              │          │
  │ sender   │────── A delivers to B ──────→│ receiver │
  │ on B→A   │←╌╌ Channel B→A: B pays A ╌╌╌│ on B→A   │
  └──────────┘                              └──────────┘

  ── resource   ╌╌ payment (Spilman channel)
  Each side pays for what it received. Both channels, by default.
  A received_multiplier adds a surcharge on top, where a node
  would rather not carry what a peer pushes at it.
```
</details>

Spilman channels enable **streaming micropayments**: the sender locks ecash in a 2-of-2 multisig with a time-locked refund path, then signs successively larger balance updates. The receiver holds the latest signed update and can settle with the mint at any time.

A Spilman channel is already a prepaid instrument — funding it is money committed before anything is delivered, and each update ratchets the receiver's claim upward. Grants use it as exactly that: **one TopUp is one signed update and one purchase**, and the state it carries is the cumulative total the payer has authorized on that channel ([tollgate-protocol.md](tollgate-protocol.md)).

### Grants On A Channel

<details><summary>Text version</summary>

```
  Channel A→B, funded by A, capacity 500 M units.

  t=0    A → B: TopUp (cumulative 6.25M, window 5000 ms)
                B verifies, shapes A to 1.25 M/s until t=5
                B's claim on the channel: 6.25M

  t=3    A → B: TopUp (cumulative 106.25M, window 5000 ms)
                grant 100M, so 20 M/s until t=8
                B's claim on the channel: 106.25M
                A forfeits whatever was left of the first grant

  No acknowledgment. Cumulative state is self-correcting, so a lost
  or reordered TopUp costs nothing and A never waits for a reply.

  Channel B→A runs the same way, funded by B, on B's own schedule.
  The two never meet.
```
</details>

---

## Channel Pair Lifecycle

The Spilman channel lifecycle begins after peers have exchanged Announce and Offer messages. Each channel is funded in one of the mints the counterparty listed in its Offer — usually its own, which is reachable over the peering link by definition, but any mint on that list will do. A rollover channel is held to the same rule: it may use a different mint from the channel it replaces, but always one the counterparty lists.

![Channel Pair Lifecycle](diagrams/channel-pair-lifecycle.svg)
<details><summary>Text version</summary>

```
                    ┌─────────────────────────────────────┐
                    │                                     ▼
  Funding ──► Active ──► RollingOver ──► Settling ──► Closed
                │            │                          ▲
                │            └──────────────────────────┘
                │              (old channel settles
                │               while new one is active)
                │
          (no charge: skip funding, go directly to Active)
```
</details>

### Funding

Both peers can reach a mint. They exchange Accept messages containing Spilman funding proofs. Each side creates a channel where they are the sender (funder):
- A creates and funds the A→B channel (A pays B for what B delivered to A)
- B creates and funds the B→A channel (B pays A for what A delivered to B)

The funding process follows the Cashu Spilman protocol:
1. Sender creates a 2-of-2 multisig token: `P2PK: (Sender AND Receiver) OR (Sender after expiry)`
2. Sender derives the channel secret via ECDH with the receiver's pubkey
3. Sender constructs deterministic blinded outputs using the channel secret
4. Sender sends funding proofs to receiver
5. Receiver verifies: re-derives blinded messages, verifies DLEQ proofs, checks mint/keyset policy
6. Receiver sends ChannelReady

### Active

Both channels are funded and verified. Each side buys grants on its own channel:
- A payer sends TopUp whenever it wants capacity, at least once per window if it wants continuous service
- The provider verifies, ratchets its claim, and shapes to `grant / window`
- Nothing is acknowledged and the two directions never synchronize

### RollingOver

A channel approaches exhaustion (default: 80% capacity used). The sender (channel funder) initiates rollover:
1. Sender sends RolloverInit with new channel funding
2. Receiver verifies and sends RolloverReady
3. Old channel continues draining to 100%
4. Once exhausted, charges seamlessly continue on the new channel
5. Old channel is settled by the receiver when mint connectivity allows

Both the old (draining) and new channel are active simultaneously during the overlap period.

![Channel Rollover Timeline](diagrams/channel-rollover.svg)
<details><summary>Text version</summary>

```
  0%              80% (rollover)         100% (exhausted)
  │                    │                      │
  │   Old Channel      │    draining...       │
  │████████████████████│░░░░░░░░░░░░░░░░░░░░░░│
  │                    │                      │
  │                    │   New Channel        │         charging...
  │                    │▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒│████████████████████
  │                    │                      │
  │                    ├──── overlap period ───┤
  │                                           │
  │                    e.g. 2 vouchers remain + a 5 voucher grant
  │                        = 2 to old, 3 to new
  ├──────────────────── time ─────────────────────────────→
```
</details>

### Settling

A channel is being closed — either cooperatively (ChannelClose/CloseAck) or because the channel is fully drained after rollover. Only the **receiver** submits the latest signed balance update to the mint.

Settlement produces two sets of proofs (Stage 1 → Stage 2 in Spilman terminology):
- Receiver's earned balance (receiver can spend)
- Sender's remaining change (sender can reclaim)

### Closed

Settlement complete. Proofs distributed. Channel is done.

### Free Peering Shortcut

When a node decides not to charge a peer, it meters nothing for it and says so in its Offer (field 5, [tollgate-protocol.md](tollgate-protocol.md)), so the peer funds no channel toward it and sends it no TopUp. If **both** sides decide that, the pair goes directly to `Free` with no funding, no channels, no metering, and no balance updates.

This is a decision about a relationship, not a price — there is no delivery price to set to zero. It is also one-sided: a node chooses only whether it charges, so a peering can legitimately run with one channel.

---

## What the Grant Window Is For

The window is **not** a settlement clock and not a renegotiation point. It is
the denominator of a rate: a grant of `n` units over a window of `w` buys
`n / w` ([tollgate-vouchers.md](tollgate-vouchers.md)).

It is chosen by the payer, per grant, inside the range the provider advertised.
Nothing is agreed between the two sides, and no boundary is shared — each side
buys on its own schedule for its own windows.

Three things follow from where the payer sets it:

- **Batching.** One signature per grant rather than one per unit, which is the
  whole point of a Spilman channel. A longer window means fewer signatures.
- **Forfeiture risk.** Raising the rate before a window ends discards the
  remainder, so the most a misjudgment can cost is one window's worth.
- **Reaction granularity.** A short window makes a rate change cheap, so a
  payer that expects bursty demand pays for that with message volume.

The provider bounds it from both ends, for reasons of its own:

| Bound | What it protects |
|---|---|
| `max_window_ms` | Stops capacity being bought off-peak and presented at peak |
| `min_window_ms` | Caps signature verifications per second — the binding constraint on a constrained device, not bandwidth |

**The provider carries no delivery exposure at all.** Payment lands before the
traffic it covers, so a peer that vanishes leaves nothing unpaid. What remains
is the payer's exposure, and it has two distinct bounds:

| Risk | Bounded by |
|---|---|
| Payer buys a grant and the provider does not deliver | The grant — which is one window's worth |
| Payer funds a channel and the issuer refuses to honor the refund | Channel capacity |

The second is the trust cost of the issuer being the mint
([issuer-risk.md](../market/issuer-risk.md)). Shorter windows do not help it;
funding smaller channels does.

---

## Channel Ownership

Each Spilman channel is unidirectional. The **sender** (funder) of each channel is responsible for managing that channel's lifecycle — including rollover, capacity decisions, and funding. Channels do carry shared state (cumulative balance, signatures between sender and receiver), but rollover is initiated by the funder alone because only the funder puts up new funds.

- A→B channel: A is the sender, A manages rollover, A decides when to fund a new channel
- B→A channel: B is the sender, B manages rollover, B decides when to fund a new channel

Each peer independently monitors their own outbound channel and initiates rollover when capacity runs low. Communication is still needed (RolloverInit/RolloverReady messages), but the sender always initiates.

---

## Rollover Mechanics

### When to Rollover

Rollover triggers when a channel reaches the **rollover threshold** — a configurable percentage of channel capacity (default: 80%).

```
Channel capacity: 1000 vouchers
Rollover threshold: 80% (800 spent)

At 800 spent: Sender initiates RolloverInit
New channel funded alongside old channel
Old channel continues draining: 801, 802, ... 1000
At 1000: old channel exhausted, charges continue on new channel
```

### Overlap Period

During rollover, **two channels exist simultaneously** for the same direction:
- Old channel: draining to 100%
- New channel: funded and ready, accepting charges once old is exhausted

A grant uses whichever channel has remaining capacity. When the old channel holds less than the grant, the remainder is signed onto the new one, and the provider adds the two into a single allowance.

**Example:**
- Old channel: 998 of 1000 vouchers spent (2 remaining)
- Grant: 5 vouchers
- Result: 2 signed onto the old channel (now exhausted), 3 onto the new

### Rollover While Offline

If mint connectivity is lost during rollover:
- Balance updates on the old channel continue (they don't need the mint)
- The new channel cannot be funded until mint returns
- If the old channel exhausts before the new one is funded, that direction falls back to the minimum flow allowance — or pauses, if the allowance is zero — once the grant the old channel paid for runs out. That ends the TollGate session ([tollgate-access-control.md](tollgate-access-control.md)), but the connection stays open, so a new one starts as soon as the new channel is funded.
- Once mint returns: new channel is funded, old channel is settled by the receiver

---

## Offline Resilience

### What Needs the Mint

| Operation | Needs mint? | Notes |
|-----------|-------------|-------|
| Grants (signing and verifying) | No | Signed between peers, no mint involvement |
| Metering | No | Local computation, never exchanged |
| Channel funding (open) | **Yes** | Must create 2-of-2 multisig token |
| Channel settlement (close) | **Yes** | Receiver must submit swap to mint |
| Channel rollover (new) | **Yes** | New channel needs funding |
| Keyset refresh | **Yes** | Fetch active keysets |

### Offline Scenarios

**Mint goes down during active session:**
- Grants continue normally (no mint needed)
- TopUp signing and verification work fine
- If a channel exhausts, rollover is blocked until mint returns
- If channel approaches expiry, urgency increases

**Mint goes down during funding:**
- Funding fails. Retry when mint connectivity returns.

**Mint goes down during settlement/close:**
- The settlement is retried with backoff (see [Settlement Failure](#settlement-failure)). Receiver holds the latest signed update.
- When mint returns, the next retry submits the swap.
- Keyset errors (12xxx) trigger one retry after refresh.

---

## Reboot / State Loss

Nodes are not expected to persist runtime state between restarts. On reboot, a node loses metering counters, grant state, channel tracking, and any signed TopUps it was holding. The identity key survives (it's in the config file), so the rebooted node has the same pubkey and can be recognized by peers.

The remaining peer (still online) is the only party that holds the latest channel state. Two scenarios apply:

### Friendly recovery

The online peer recognizes the reconnecting pubkey and shares back the channel state for both directions: channel IDs, cumulative balances, and signatures. The rebooted peer validates every signature before trusting any of it. If validation succeeds, both channels resume — the rebooted peer knows how much of its outgoing channel is spent, and holds the latest signed TopUp for its incoming channel. Grants themselves do not survive: the rebooted peer starts with no allowance for anyone, and each payer buys again.

This requires a protocol message (proposed `ChannelSync`) that the online peer sends after Announce when it detects a reconnecting pubkey with live channels. Not yet specified in [tollgate-protocol.md](tollgate-protocol.md) — **future work**.

### Unfriendly outcome

The online peer stays silent about the old channels. The rebooted peer falls back to a fresh session with new channels.

- **Outgoing channel** (rebooted peer was sender): the online peer holds the rebooted peer's last signed TopUp and can settle with the mint. The rebooted peer reclaims any remainder via Spilman's refund timelock after expiry. No loss beyond what was legitimately owed.
- **Incoming channel** (rebooted peer was receiver): the rebooted peer lost the only proof of earnings. The online peer waits for expiry and reclaims the full channel via the refund path. **The rebooted peer loses all earned income on that channel.**

Exposure is bounded by channel capacity (the "start small, grow with relationship" model limits new-peer exposure), time since last mint settlement, and the channel TTL (1 hour default). Worst case: one channel's worth of earned income.

---

## Channel Expiry

### Choosing TTL

The channel's expiry timestamp (`expiry_timestamp`) must balance two concerns:
- **Too short**: Frequent rollovers, more mint interaction, more overhead
- **Too long**: More capital locked up, longer exposure if peer disappears

Default TTL: **1 hour**. Configurable.

### Safety Margin

The sender initiates a **rollover** when a channel enters the safety margin before expiry — creating a new channel and allowing the old one to be settled before the refund timelock activates.

```
safety_margin = max(60 seconds, 2 × max_window_ms)
```

Within the safety margin:
1. Sender initiates rollover (RolloverInit) to create a new channel
2. Old channel is settled by the receiver before expiry
3. If receiver is unresponsive: sender waits for expiry and reclaims via refund path
4. If mint unreachable: receiver retries aggressively until expiry

### Expiry Timeline

```
Channel created:  T₀
Channel expiry:   T₀ + TTL (e.g., 1 hour)
Danger zone:      expiry - safety_margin (e.g., expiry - 60 seconds)
```

1. **Normal**: Well before expiry, channels rollover naturally as they exhaust
2. **Warning**: If a channel enters the danger zone without having been settled, the sender initiates rollover — even if the channel isn't near capacity
3. **Expiry**: If the receiver fails to settle before expiry, the sender reclaims funds via the refund path. The receiver loses any unsettled balance.

---

## Channel Capacity

### Initial Capacity

Channel capacity starts **small** because the session may not last long. A new peer connection doesn't warrant a large upfront commitment.

Factors:
- **Estimated usage**: Based on expected consumption over the session
- **Expected session duration**: Short for mobile peers, longer for infrastructure
- **Operator configuration**: Minimum and maximum channel capacity settings
- **Available balance**: Can't fund more than the wallet holds

### Capacity Growth

As a peer relationship proves stable (multiple successful rollovers), the node can increase channel capacity for new channels. This reduces rollover frequency and overhead.

```
First channel:    100 vouchers (minimum viable)
After 1 rollover: 200
After 3 rollovers: 500
After 10 rollovers: 1000 (configurable cap)
```

The exact growth curve is operator-configurable.

---

## ChannelBackend Trait

`tollgate-core` does no Cashu operations and no I/O: it decides when a channel should be funded, rolled over or settled, and emits that as an action. The host carries the action out through a `ChannelBackend`, which it owns (`tollgate-net`). A Spilman backend is one implementation; core never learns which one it is talking to.

```rust
pub trait ChannelBackend: Send + Sync {
    /// Fund a channel to pay `peer` on, against `mint_url` — one of the mints
    /// the peer listed in its Offer.
    fn fund(&self, peer: PubKey, mint_url: &str, capacity: u64) -> Result<FundedChannel>;

    /// Check funding a peer sent us and return the channel it opens.
    fn verify(&self, peer: PubKey, funding: &[u8]) -> Result<VerifiedChannel>;

    /// Sign a ratchet turn on a channel we fund.
    fn sign_update(&self, channel_id: ChannelId, cumulative: u64) -> Result<Signature>;

    /// Check a peer's ratchet turn on a channel it funds, without keeping it.
    /// Called on every update in a TopUp before the message reaches core,
    /// which trusts what it is handed. Must not change what the backend has
    /// recorded: the update may belong to a purchase that is refused.
    fn verify_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> bool;

    /// Keep a verified ratchet turn as the channel's latest signed state, the
    /// one `settle` submits. Called only once core has accepted the whole
    /// purchase (`Action::RecordUpdates`).
    fn record_update(
        &self,
        peer: PubKey,
        channel_id: ChannelId,
        cumulative: u64,
        signature: Signature,
    ) -> Result<()>;

    /// Settle a channel: submit the latest signed state to the mint the
    /// channel was funded in, and reclaim the change.
    fn settle(&self, channel_id: ChannelId) -> Result<()>;
}

pub struct FundedChannel {
    pub channel_id: ChannelId,
    pub capacity: u64,
    /// Opaque funding blob carried in Accept and RolloverInit.
    pub funding: Vec<u8>,
}

pub struct VerifiedChannel {
    pub channel_id: ChannelId,
    pub capacity: u64,
}
```

What a channel update commits to is the channel scheme's business, which is why signing lives in the backend: core treats the funding blob and the signature as opaque bytes.

### Key Operations by State

| State | Backend operations used |
|-------|----------------------|
| Funding | `fund`, `verify` |
| Active | `sign_update`, `verify_update`, `record_update` |
| Rollover | `fund`, `verify` (new channel), `settle` (old channel) |
| Settling | `settle` |
| Offline | `sign_update`, `verify_update`, `record_update` (no mint needed) |

---

## Error Handling

### Funding Failure

If channel funding fails (mint unreachable, insufficient balance, keyset error):
- Retry on next mint connectivity check

### Settlement Failure

Core emits `SettleChannel` once and drops the channel from the grant in the same step, so it never asks again. A failed settlement that nobody retried would be lost outright: after the refund timelock the funder reclaims the whole channel, including what it had already paid. Retrying is therefore the host's job — it is I/O and time, which core does not do.

If settlement fails:
- Keyset errors (12xxx): the backend refreshes keysets and retries once, inside the one `settle` call
- **Permanent** errors — a channel the backend has never seen, one with nothing to claim, one its funder already reclaimed, or a mint rejecting the proofs themselves (10xxx, 11xxx: the channel state is corrupted or already spent) — are reported as `CannotSettle` and not retried; retrying them would only make noise
- Anything else (the mint unreachable, a keyset still stale) is **transient**, and the node retries it: after 1 s, doubling to a cap of 5 min, until it succeeds or the node shuts down. Each failure is logged at `warn` with the attempt count and the next wait; a success after retries is logged at `info`
- One channel is never settled by two attempts at once
- `settle` is idempotent: settling a channel that already settled succeeds and moves nothing, so a retry that races a success is harmless

On shutdown the node settles every channel still in a grant, and every retry still waiting wakes for a last attempt. They all share a short grace period (5 s), retrying on the same backoff but never past it; whatever is still unsettled then is abandoned rather than holding the node open.

Each retry can carry a deadline past which it gives up. Nothing sets one yet: the natural one is the channel's refund expiry, once the backend reports it, since past it the funder can take the money back.

Retries live in memory only. A settlement still failing when the node stops is forgotten, and a restart does not resume it.

### Balance Verification Failure

A TopUp is honored or refused as a whole, and one TopUp can carry updates for several channels — a purchase spanning a rollover carries two. So verifying and keeping are separate steps. The host verifies every update's signature with `verify_update`, which records nothing, and hands core the TopUp only if all of them pass. Core decides whether to grant it; only when it accepts does it emit `Action::RecordUpdates`, and only then does the host call `record_update` on each, making it the state `settle` submits. A TopUp refused for any reason — one bad signature, a window out of range, a rate over capacity — leaves the backend's record where it was, so the backend never holds a signed state the grant did not pay for, and a payer retrying the same purchase is judged against the same state as the first time. `RecordUpdates` comes before any `SettleChannel` the same purchase sets off, so a channel the purchase filled settles at the state it paid for.

If a received TopUp fails signature verification, or its cumulative total does not exceed the current one (including a channel named twice in one purchase):
- Send Reject (reason: grant signature invalid, or cumulative not increasing)
- Do NOT close the channel — this could be a transient error
- Log the failure for operator review
- If repeated failures: close the channel

The signature is checked by the host; core is only told which channel failed. Failures are counted per channel and only in a row: a purchase that verifies resets the count. At three (`MAX_VERIFICATION_FAILURES`) the provider stops recognising the channel and settles the last state that did verify, so nothing the channel already paid for is lost. No ChannelClose is sent — the provider holds no final signature to put in it — so the payer learns of the close only by having its later purchases over that channel refused: with Reject once the channel backend has closed the channel, otherwise with TopUpReject (channel funding invalid).

### Grant Rejected

If a TopUp cannot be honored — the rate would oversubscribe committed capacity, the window falls outside the advertised range, or the grant exceeds what is left in the channel — the provider sends TopUpReject and does **not** ratchet its claim. The payer's money is untouched, because an unclaimed Spilman state is worth nothing. The payer re-purchases at the rate the reject offered, or stops.

### Under-Delivery

A payer that receives less than it bought has no protocol recourse: the grant was consumed and the provider holds the claim. This is deliberate — see [tollgate-metering.md](tollgate-metering.md). The channel layer's only role is to make the exit cheap: close the channel, settle at the current state, and take the remaining funding elsewhere.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Channels per peer pair | Two unidirectional, one per direction | Each side pays for what it received, so both owe by default. Absent only where a node declines to charge a peer |
| Channel ownership | Sender manages own channel lifecycle | Rollover initiated by the funder alone — only the party putting up new funds decides when |
| Rollover threshold | 80% capacity (configurable, default 20% overlap) | New channel ready before old exhausts |
| Rollover drain | Old channel drains to 100%, then new channel continues | No wasted capacity |
| Stale session timeout | 60 seconds (configurable) | Close the connection to a peer that has gone silent; a lapsed payment ends the TollGate session but not the connection |
| Grant window | Payer chooses per grant, inside a provider-advertised range | It is the denominator of a rate, not a settlement clock. Nothing is negotiated and no boundary is shared |
| Provider delivery exposure | None | Payment lands before the traffic it covers, so a peer that vanishes leaves nothing unpaid |
| Under-delivery | No recourse in the channel layer | The grant is consumed whether or not packets arrive. The remedy is to stop buying, and the channel layer's job is only to make leaving cheap |
| Channel capacity | Start small, grow with relationship | Don't over-commit to new peers |
| Channel TTL | 1 hour default, configurable | Balance between overhead and capital lockup |
| Safety margin | max(60s, 2×max_window_ms) before expiry — triggers rollover | Create new channel, settle old before expiry |
| Settlement | Only receiver submits to mint | Receiver holds the signed proof |
| Offline operation | Grants continue; funding/settlement queued | Mint only needed for channel lifecycle transitions |
