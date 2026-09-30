# TollGate Gate Protocol

How `tollgated` drives an enforcement program it does not contain. The
**gate** is a separate process that owns a data plane — a firewall, a proxy,
a tunnel — and opens, shapes and counts it per peer as `tollgated` tells it.
`tollgated` keeps deciding what is owed; the gate enforces it and reports
what it carried.

The point is that a new use case is a new gate, not a new `tollgated`. The
built-in adapters ([tollgate-access-control.md](tollgate-access-control.md))
stay; `forwarding.mode: external` puts a gate behind the same
`ResourceAdapter` seam instead, over a local socket.

---

## Payer, Subject, Binding

Three things, never conflated:

| Term | What it is | Who knows it |
|---|---|---|
| **Payer** | The TollGate key that signs payments and owns the grant. `tollgated`'s ledger is keyed by it | `tollgated` |
| **Subject** | What the gate's data plane matches to open or close: an IPv4 or IPv6 address, a MAC, a FIPS address, an npub, a WireGuard key, anything | The gate |
| **Binding** | What ties a payer to a subject, with the **evidence** for it | Both: `tollgated` states it, the gate enforces it |

**A payer and its subject need not be the same party.** A proxy buying a
session for a phone pays with its own key for the phone's address; an exit's
allowlist is subjects with no payer at all. So the protocol never infers a
subject from a payer's key. Every subject the gate opens for a payer arrives
in a binding.

In the messages below, `peer` is always the payer: the key a TollGate session
runs under, the key `ResourceAdapter` is called with. Access, rate and
counters are per payer; a payer may hold several subjects, and a subject is
held by at most one payer.

### Subject kinds

| Kind | Value | Example |
|---|---|---|
| `ipv4` | 4 bytes | `192.168.1.23` |
| `ipv6` | 16 bytes | `2001:db8::5` |
| `mac` | 6 bytes | `aa:bb:cc:00:11:22` |
| `fips-addr` | 16 bytes, first byte `0xfd` | `fd10:93b2:8586:6046:e42d:c089:3228:ccff` |
| `npub` | 32-byte x-only key | the key behind `npub180cvv07…` |
| `opaque` | a `u32` kind and up to 64 bytes | a WireGuard public key, under a kind the two ends agree |

A `fips-addr` is an IPv6 address, but a distinct kind: it names a key
(`0xfd` and the first 15 bytes of SHA-256 of the x-only key,
`crates/tollgate-net/src/fips.rs`), and a gate matches it only on `fips0`. An
IPv6 address in `fd00::/8` that `tollgated` has not checked against a key is
an `ipv6`, never a `fips-addr`.

`opaque` is for subjects this protocol has no name for yet. The kind number is
agreed between whoever asserts the subject and the gate; `tollgated` neither
interprets nor derives it, and can only pass one on as `asserted`.

### Evidence

How a binding is known, strongest first:

| Level | Meaning | Example |
|---|---|---|
| `proven` | The transport authenticates it. Nobody but the payer could have produced it | FIPS: a connection from the peer's FIPS address on `fips0` could only come from the node that completed the Noise IK handshake for that key |
| `observed` | This node saw it, on a link where it can be forged | The source address of the payer's control connection on a LAN; neighbour-table entries, which any host on the segment can poison |
| `asserted` | A trusted local party vouches for it | A proxy on the same machine says a phone's address is paid for by one of its sessions |

The order is total: `proven` > `observed` > `asserted`. A gate states the
minimum it accepts per subject kind, and a gate that will not take a
third party's word sets it above `asserted`.

---

## Transport

A **Unix socket**. **The gate is the server**; `tollgated` connects, and
reconnects whenever the connection drops. The gate owns the data plane and
outlives `tollgated` restarts or starts before it, so it holds the listening
end; `tollgated` needs nothing but the path, `forwarding.gate_socket`.

**Access to the socket is the power to open the gate.** Its permissions admit
`tollgated` alone. The gate serves one connection at a time: a second one
replaces the first, and the gate resets to closed as if the first had dropped.

**Encoding: CBOR, with the same framing as the TollGate wire protocol** — each
message preceded by its length as a 2-byte little-endian integer, so at most
65535 bytes ([tollgate-protocol.md](tollgate-protocol.md#raw-tcp)). The
encoding rules are the wire protocol's too: a definite-length map, key `0`
the message type, integer keys `0..255`, unknown keys skipped. Not JSON: a
gate in Rust links the message types, and one in any other language gets a
CBOR schema to validate against rather than a convention.

The message types live in **`tollgate-protocol`**, in a module of their own,
`no_std` + `alloc` like the rest of it, so a gate needs that crate and nothing
else of TollGate. Their normative schema is a CDDL file beside
[`tollgate.cddl`](../../../crates/tollgate-protocol/tollgate.cddl),
`gate.cddl`, checked by the same schema tests; the sketch
[below](#schema-sketch) is what it will say.

---

## Messages

| Message | Direction | Meaning |
|---|---|---|
| `hello(version, requires, opaque_kinds)` | gate → tollgated | First message on every connection. The subject kinds this gate matches, each with the minimum evidence it requires; the `opaque` kinds it knows |
| `bind(peer, [subject + evidence])` | tollgated → gate | The complete set of subjects this payer holds, replacing any earlier set. Empty unbinds them all |
| `access(peer, level)` | tollgated → gate | The payer's access level: `none`, `active` or `free` |
| `rate(peer, units_per_s)` | tollgated → gate | The rate to shape the payer's subjects to. Bytes per second for network forwarding |
| `remove(peer)` | tollgated → gate | Forget the payer: its subjects return to closed |
| `counters(peer, delivered, received)` | gate → tollgated | Units carried to and from the payer's subjects, cumulative on this connection |
| `conflict(peer, subject)` | gate → tollgated | The gate refused to bind `subject` to `peer`. See [Conflicts](#conflicts) |

These are the members of `ResourceAdapter` that enforce and count, one for
one. `register(peer, addr)` becomes a `bind` carrying the evidence
`tollgated` actually has for `addr`; `set_access`, `set_shaping_rate` and
`remove` become `access`, `rate` and `remove`; `counters()` returns the last
`counters` the gate reported. `demand` belongs to the buyer and never reaches
the gate.

**What opens the gate is the pair.** Whether a payer's subjects are carried
is `AccessLevel::carried(level, rate)`: `free`, or any rate above zero. The
rate already includes the minimum flow allowance as its floor, so the gate
applies two numbers and needs to know nothing about grants. `tollgated`
writes both whenever core changes either, back to back.

**What the gate never closes is the path to the node itself.** A payer must
always reach `tollgated` to pay, and `mintd` and `merchantd` to buy vouchers
([tollgate-access-control.md](tollgate-access-control.md#what-blocked-means)).
How a gate keeps that path open is its own business; a firewall exempts the
node's own addresses, a proxy serves its portal to anyone.

### Sequence

```
gate                                   tollgated
  │  (listening; everyone closed)          │
  │◄────────────── connect ────────────────│
  │── hello(1, requires, opaque_kinds) ───►│  check against own mode
  │                                        │  (mismatch: startup error)
  │◄── bind / access / rate, every peer ───│  full state
  │                                        │
  │◄──────── bind(peer, subjects) ─────────│  a peer connects
  │──────── conflict(peer, subject) ──────►│  only if refused
  │◄──────── access / rate ────────────────│  core decided
  │─────── counters(peer, d, r) ──────────►│  every tick
  │◄──────────── remove(peer) ─────────────│  peer gone
```

**`hello` comes first, from the gate.** `tollgated` sends nothing until it has
one, and closes a connection whose `version` it does not speak. It then sends
the **full state**: for every payer it tracks, a `bind`, an `access` and a
`rate`. After that everything is incremental.

**`bind` replaces.** It always carries every subject the payer holds, so it is
idempotent and a resend is harmless. `tollgated` binds a payer when its
control connection arrives, before it sends its Offer, and again whenever the
payer's subjects change — an asserted binding added, say.

**`counters`** are sent every tick, by default once a second, for every payer
whose counts changed. Each is cumulative from the payer's first `bind` on
this connection and summed over all its subjects: `delivered` is what went to
them, `received` what came from them. Because each connection starts from zero,
`tollgated` rebases on reconnect — it adds the last counts of the previous
connection — so the totals core sees never go backwards.

**Protocol errors close the connection.** An unknown message type, a
malformed message, a `bind` naming a kind the gate did not list or evidence
below its minimum: the receiver closes the socket. Closing is the safe
failure for both ends — the gate returns to closed, and `tollgated` stops
selling (below).

---

## Failure Rules

**A gate starts closed.** Until `tollgated` binds and opens a payer, every
subject is denied: nothing is carried for anyone. A subject no payer holds
stays closed.

**Losing the connection closes the gate too.** When the socket drops the gate
forgets every binding, access level and rate it was given and is back where it
started. It does not go on carrying peers at their last rate, because the one
party that knows when their grants run out is gone.

**`tollgated` resends full state on every reconnect.** Every connection starts
from closed, so the full state after `hello` is the whole of what the gate
knows. There is no resume and nothing to reconcile.

**While the gate is unreachable, `tollgated` sells nothing.** It keeps
reconnecting, about once a second. Meanwhile it answers every TopUp with a
TopUpReject, `max-rate-available` 0 and reason `rate-exceeds-capacity` — the
capacity it can deliver is zero — and does not fund or accept new channels.
Sessions and channels already running are kept, so the peers pick up where
they were once the gate is back. The same holds before the first connection:
a `tollgated` in `external` mode starts selling when it has a `hello` it
accepts, not when it starts.

What an outage costs a payer is the rest of the grant in force: it paid for a
window the gate stopped carrying. That is bounded by one grant, the payer sees
it in its own counters ([tollgate-metering.md](tollgate-metering.md)), and it
is the price of never carrying anyone nobody is metering.

---

## Identify Coupling

`tollgated` decides who a peer is — `wire::Identify` — before any binding
exists. Under `Identify::Fips` a control connection must come from the FIPS
address of the key it announces
([peering-fips.md](../network-peering/peering-fips.md#verifying-the-peer));
under `Identify::Claimed` the announced key is taken at its word. That decides
what evidence `tollgated` can honestly give:

| Identify | What `tollgated` has | What it binds |
|---|---|---|
| `Fips` | A connection from `fips-addr(peer)`, checked, so the key is the connection's | `fips-addr`, `npub` or both — whichever the gate matches — `proven` |
| `Claimed` | The connection's source address, unchecked | `ipv4` or `ipv6`, `observed` |
| either | A local trusted client's word | whatever it asserted, `asserted` |

**So the gate's requirements set the mode.** In `external` mode `forwarding.mode`
cannot say which network carries the control plane, so the `hello` does: a
gate that requires `proven` for `fips-addr` or `npub` forces `Identify::Fips`,
and any other gate runs under `Identify::Claimed`.

**A mismatch is a startup error, not a silent hole.** `tollgated` refuses to
start — or, on a reconnect, refuses that gate and stays in the not-selling
state above — when:

- the operator pinned the mode (`forwarding.identify: fips` or `claimed`,
  optional in `external` mode) and the `hello` implies the other one;
- the `hello` requires `proven` for a kind `tollgated` can never prove:
  `ipv4`, `ipv6`, `mac` or `opaque`;
- a reconnecting gate implies a different mode from the one `tollgated` is
  running. Live sessions were identified under the old mode, and switching
  would either re-trust peers that were checked or strand peers that were not.

The hole this closes: a gate that needs proof, fed by a `tollgated` that
believes whatever key it is told, would open a paying peer's subject to anyone
who claims its key. Running on regardless is never the answer; refusing is.

---

## Deriving Subjects

**Deriving subjects is the gate's job.** `tollgated` passes only the evidence
it genuinely has, and the gate extends it with what its own data plane can
see:

- a LAN gate goes from an `ipv4` to its `mac` through the neighbour table,
  and from the `mac` to the device's `ipv6` addresses
  ([peering-ip.md](../network-peering/peering-ip.md#a-customers-ipv6));
- a mesh gate goes from an `npub` to its `fips-addr`, or the other way.

A derived subject carries the evidence of the one it came from, or less:
the neighbour table is itself `observed`, so a `mac` derived from an
`observed` `ipv4` is `observed`, never better. Derived subjects are subject
to the same conflict rule as bound ones.

This keeps `tollgated` free of every data plane's details. It never learns
the neighbour table exists; a new gate that matches something new needs no
new `tollgated`.

---

## Asserted Bindings

A **local trusted client** — a program on the same machine that reaches
`tollgated` over its control socket, whose permissions are the trust — may
ask `tollgated` to add a subject to a payer's bindings. The typical one is a
proxy buying a session per phone: its session's key is the payer, the phone's
address the subject.

The binding reaches the gate **through `tollgated`**, in the payer's next
`bind`, marked `asserted`. The client never talks to the gate: `tollgated`
remains the one party that tells the gate who has paid, and the payer must be
one `tollgated` tracks.

**A gate may refuse asserted bindings** by requiring more than `asserted` for
the kinds concerned. `tollgated` knows that from the `hello` and turns the
client's request down itself, so no refused binding is ever sent.

What the request looks like on the control socket belongs to the control
socket, not to this protocol.

---

## Conflicts

> **PLACEHOLDER.** These rules are the minimum that fails safe. They are not
> the design; conflict handling is an open problem, tracked separately, and
> will replace this section.

A **conflict** is two payers claiming one subject: several keys behind one
NAT address, a neighbour answering NDP with a victim's MAC, an asserted
binding that overlaps an observed one. If the later binding silently won, one
party would ride on another's payment, or a victim would be charged for
someone else's traffic. So, for now:

1. **The gate refuses the second binding.** The first payer keeps the
   subject. Never last-wins.
2. **It fails closed.** The refused subject is not carried for the second
   payer, and nothing is derived from it for that payer.
3. **It reports back** with `conflict(peer, subject)`, naming the payer it
   refused and the subject in question.
4. **`tollgated` refuses service to that payer**: it sells it nothing,
   rejects its TopUps as while the gate is unreachable, and logs the conflict
   for the operator.

`tollgated` binds a payer when it connects, before its Offer, so a conflict
normally arrives long before a grant could be bought. Nothing guarantees it —
a `bind` that succeeds is not acknowledged — and a conflict that arrives
mid-session is handled the same way.

**Open question: may stronger evidence displace weaker?** A `proven` binding
contesting an `observed` one, or an `observed` one contesting an `asserted`
one. Displacing would let a real owner reclaim a subject that was poisoned
first, and would let whoever gets to be "stronger" evict a paying peer. Not
decided; until it is, the first binding holds whatever the evidence.

---

## Schema Sketch

Informative. The normative schema is `gate.cddl` in `tollgate-protocol`, and
where the two differ it wins. Message type tags start at `0x20` so a frame
sent to the wrong socket fails to decode rather than meaning something else.

```cddl
gate-message = hello / bind / access / rate / remove
             / counters / conflict

u8  = uint .size 1
u32 = uint .size 4
u64 = uint .size 8
pubkey = bstr .size 33            ; the payer: compressed secp256k1 key

evidence = &(
  asserted: 0,
  observed: 1,
  proven:   2,                    ; ordered: a higher value is stronger
)

subject-kind = &(
  kind-ipv4:      0,
  kind-ipv6:      1,
  kind-mac:       2,
  kind-fips-addr: 3,
  kind-npub:      4,
  kind-opaque:    5,
)

subject = [0, bstr .size 4]       ; ipv4
        / [1, bstr .size 16]      ; ipv6
        / [2, bstr .size 6]       ; mac
        / [3, bstr .size 16]      ; fips-addr, first byte 0xfd
        / [4, bstr .size 32]      ; npub, x-only key
        / [5, u32, bstr .size (0..64)]   ; opaque: kind, value

binding = [subject, evidence]

access-level = &(
  access-none:   0,
  access-active: 1,
  access-free:   2,
)

; 0x20 -- gate -> tollgated. First message on every connection.
hello = {
  0: 0x20,
  1: u8,                          ; version; 1
  2: [+ [subject-kind, evidence]],; kinds matched, each with its minimum
  ? 3: [* u32],                   ; opaque kinds matched; absent means none
  * uint => any,
}

; 0x21 -- tollgated -> gate. Every subject the payer holds; replaces.
bind = {
  0: 0x21,
  1: pubkey,
  2: [0*8 binding],               ; empty unbinds everything
  * uint => any,
}

; 0x22 -- tollgated -> gate.
access = {
  0: 0x22,
  1: pubkey,
  2: access-level,
  * uint => any,
}

; 0x23 -- tollgated -> gate. Units per second, allowance included.
rate = {
  0: 0x23,
  1: pubkey,
  2: u64,
  * uint => any,
}

; 0x24 -- tollgated -> gate. The payer and its subjects are forgotten.
remove = {
  0: 0x24,
  1: pubkey,
  * uint => any,
}

; 0x25 -- gate -> tollgated. Cumulative on this connection.
counters = {
  0: 0x25,
  1: pubkey,
  2: u64,                         ; delivered: to the payer's subjects
  3: u64,                         ; received: from them
  * uint => any,
}

; 0x26 -- gate -> tollgated. A binding refused because another payer
; holds the subject. PLACEHOLDER semantics, see Conflicts.
conflict = {
  0: 0x26,
  1: pubkey,                      ; the payer refused
  2: subject,
  * uint => any,
}
```

At most eight subjects per `bind` keeps the largest message under 1 KiB.
The derived ones — a device's many IPv6 addresses — are the gate's, and never
cross the socket.

---

## Worked Example: LAN (observed)

A router sells internet access on `br-lan`. Its gate is a firewall program
that matches addresses and MACs; it trusts nobody's word for them.

```
hello(1, requires: [ipv4 ≥ observed, ipv6 ≥ observed], opaque_kinds: [])
```

No kind requires `proven`, so `tollgated` runs `Identify::Claimed`. It sends
full state — nothing yet.

A laptop running TollGate with key `02ab…` connects to the control plane from
`192.168.1.23`. The source address is all `tollgated` has, unchecked:

```
tollgated → gate   bind(02ab…, [[ipv4 192.168.1.23, observed]])
tollgated → gate   access(02ab…, none)
tollgated → gate   rate(02ab…, 0)          no allowance on this node
```

The gate looks `192.168.1.23` up in the neighbour table, finds
`aa:bb:cc:00:11:22`, and from that MAC the laptop's `2001:db8::5` and a
privacy address. All four are the payer's, all `observed`, all closed: rate
0 and level `none` is not carried. The laptop can still reach the router
itself, so it pays.

It funds a channel and buys 3.12 MiB/s:

```
tollgated → gate   access(02ab…, active)
tollgated → gate   rate(02ab…, 3276800)
```

The gate opens the IPv4 address, the MAC and both IPv6 addresses, and shapes
their downloads to one class at that rate. Each second:

```
gate → tollgated   counters(02ab…, 1841203, 90210)
gate → tollgated   counters(02ab…, 5102337, 188400)
```

`tollgated` draws the grant down by the difference, as it would from its own
adapter. When the grant lapses, `access(02ab…, none)` and `rate(02ab…, 0)`
close all four again.

**A conflict.** A second key, `03cd…`, connects from `192.168.1.23` too —
another machine behind a NAT router plugged into the LAN:

```
tollgated → gate   bind(03cd…, [[ipv4 192.168.1.23, observed]])
gate → tollgated   conflict(03cd…, ipv4 192.168.1.23)
```

`02ab…` keeps the address. `tollgated` sells `03cd…` nothing and logs the
conflict; the operator decides what the NAT is doing there.

**An asserted binding.** Had the gate listed `ipv4 ≥ asserted`, a proxy on
the router could have its session key `02ef…` pay for a phone at
`192.168.1.40`: it asks `tollgated` on the control socket, and `tollgated`
sends `bind(02ef…, [[ipv4 192.168.1.40, asserted]])`. With this gate's
`observed` minimum, `tollgated` turns the proxy down instead.

---

## Worked Example: FIPS (proven)

A FIPS node with internet access sells a proxy to mesh nodes. Its gate is the
proxy itself: it knows each connection's peer by the FIPS address the mesh
delivered it from, on `fips0`.

```
hello(1, requires: [fips-addr ≥ proven], opaque_kinds: [])
```

`proven` for `fips-addr` forces `Identify::Fips`: from here on, a control
connection that does not come from the FIPS address of the key it announces is
dropped before a session exists. `tollgated` sends full state.

A mesh node with the key behind
`npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6` connects to
the control plane. Its FIPS address is
`fd10:93b2:8586:6046:e42d:c089:3228:ccff`; the connection comes from exactly
that address, so the Identify check passes and the address is proof:

```
tollgated → gate   bind(023bf0…, [[fips-addr fd10:93b2:…:ccff, proven]])
tollgated → gate   access(023bf0…, none)
tollgated → gate   rate(023bf0…, 0)
```

The proxy refuses its connections: carried nothing. The node can still reach
the exit's own address to pay. After it funds a channel and buys a rate:

```
tollgated → gate   access(023bf0…, active)
tollgated → gate   rate(023bf0…, 1048576)
```

The proxy accepts connections from `fd10:93b2:…:ccff` arriving on `fips0` —
never the same address on any other interface, where it would be forgeable —
shapes them to 1 MiB/s together, counts the bytes each carries, and reports
the sums as `counters`.

**A mismatch.** The same gate, with `tollgated` configured to pin
`Identify::Claimed`: the `hello` requires proof `tollgated` cannot give, and
`tollgated` exits at startup naming both. Without the check it would bind
whatever address a peer claimed a key from, and the gate would trust it as
proof.

---

## Open Problems

| Problem | Notes |
|---|---|
| Binding conflicts | [Conflicts](#conflicts) is a placeholder: refuse the second, fail closed, report. Whether stronger evidence may displace weaker is undecided |
| No positive bind acknowledgement | A refused `bind` is reported; an accepted one is silent. In practice the conflict arrives before any grant, but nothing guarantees it. Settled with conflict handling |
| Outage cost | Closing on disconnect makes a paying peer lose the rest of its grant in force. A grace period would carry peers unmetered for as long as it lasts |
| The asserting client's request | Its form on the control socket is left to the control socket |
| Opaque kind numbers | Agreed per deployment between asserting client and gate. A registry, if kinds get common enough to need one |
| Moving built-in adapters behind the socket | The `nftables` LAN gate could become a gate program. Not planned; nothing requires it |

---

## Design Decisions

| Decision | Resolution | Rationale |
|---|---|---|
| Identity model | Payer, subject and binding kept apart; `peer` is always the payer | A proxy pays for a phone, an allowlist has no payer. Conflating them makes both impossible and lets a key stand in for an address it never proved |
| Subject from payer | Never inferred; every subject arrives in a binding | Payer and subject need not be the same party |
| Evidence | `proven` > `observed` > `asserted`, a minimum per subject kind | One order is enough to say what a gate will accept, and per kind lets a gate trust proof on one plane and observation on another |
| Transport | Unix socket, gate as server, `tollgated` reconnects | The gate owns the data plane and may start first or outlive `tollgated`; socket permissions are the authentication |
| Encoding | CBOR with the wire protocol's 2-byte framing; types in `tollgate-protocol` | One codec and one framing in the codebase; a Rust gate links the types, `no_std` included, and others get a CDDL schema |
| Type tags | `0x20` upwards, disjoint from the wire protocol's | A frame on the wrong socket fails to decode instead of being misread |
| `bind` | Replaces the payer's whole subject set | Idempotent, so a full-state resend is the same messages as live operation |
| Startup | Gate closed until told otherwise | Nobody is carried before someone is metering them |
| Disconnect | Gate returns to closed; `tollgated` resends full state; sells nothing meanwhile | Nothing to reconcile, and nothing carried that nobody meters. Costs a paying peer at most its grant in force |
| Counters | Cumulative per payer per connection, every tick; `tollgated` rebases on reconnect | Cumulative counts survive a lost report; rebasing keeps core's totals monotonic |
| Identify | Set by the gate's `hello`; a mismatch is a startup error | Believing a claimed key while the gate trusts it as proof opens a paying peer's subject to anyone |
| Deriving subjects | The gate's job; derived evidence is never stronger than its source | `tollgated` stays free of every data plane's details, and cannot upgrade evidence it does not have |
| Asserted bindings | Through `tollgated`, marked `asserted`, refusable by the gate's minimum | One party tells the gate who has paid; a gate that trusts no third party says so once, in `hello` |
| Conflicts | Placeholder: refuse the second, fail closed, report | Last-wins lets one party ride on another's payment. The full rule is open |
| Protocol errors | Close the connection | Both ends then fall back to their safe state |
