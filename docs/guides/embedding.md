# Embedding TollGate

This guide is for people who build TollGate into their own programs.

**If your software wants connectivity, embed a buyer.** An app, a daemon or
an AI agent that needs network access from a TollGate runs a buyer session,
`tollgate_net::client::run`, and pays the gateway directly over the
protocol. That is the main way to use a TollGate from software, and part 1
covers it. The captive portal of tollgate-suite-wrt is the fallback for
devices that cannot run a client, such as a phone with nothing installed: the
router's `proxyd` runs a buyer session on the phone's behalf. (The suite's
FIPS exit sells to native clients on the mesh through its own
`tollgate-fips-exit` instance; that path has not been tested end to end
yet.)

The guide covers four things:

1. [Embed a buyer in your app or agent](#1-embed-a-buyer-in-your-app-or-agent):
   buy access from a gateway with `tollgate_net::client::run`.
2. [An external enforcer](#2-an-external-enforcer): a program of your own
   that controls traffic for `tollgated`.
3. [A whole node](#3-a-whole-node): run `Node` inside your program.
4. [The control socket](#4-the-control-socket-and-the-snapshot): read what
   a running node is doing.

Each part has a compiled example in the repository. CI builds every example
on every change, so an example that no longer matches the code fails the
build. When this guide and an example disagree, trust the example.

To add the crates to your project, see
[getting-tollgate.md](getting-tollgate.md). For how TollGate works, start at
[tollgate-intro.md](../design/core/tollgate-intro.md).

> **Before 1.0, these APIs may change.** Function signatures, struct fields,
> the enforcer protocol and the snapshot's JSON can all change between
> releases. Pin a tag in `Cargo.toml` and read
> [CHANGELOG.md](../../CHANGELOG.md) when you move.

## A few words first

- A **payer** buys from a **provider**. A node can be both, toward different
  peers.
- A payer holds a **budget** with the provider: units it has paid for and not
  used yet. The budget has a **deadline**, after which what is left expires.
- A payer may **reserve a rate**. Every second the provider draws from the
  budget the larger of what the payer moved and the rate it reserved. So
  reserving a rate buys **time at a speed**: the budget drains at that rate,
  used or not. Reserving nothing is **pay per use**: only what moves is drawn.
  The full rule is in
  [tollgate-vouchers.md](../design/core/tollgate-vouchers.md#the-one-rule).
- A payer pays in **vouchers** issued by the provider's own mint, over a
  **Spilman channel**: money locked once, then signed over to the provider a
  little at a time. Each purchase (a **TopUp**) is one signed update.
- The provider tells the payer what is left, in a **Balance** message. That is
  information only; the payer keeps its own count.
- An **enforcer** is what lets traffic through, slows it to the rate paid
  for, and counts it.

## 1. Embed a buyer in your app or agent

**Example:** [`crates/tollgate-net/examples/buy_session.rs`](../../crates/tollgate-net/examples/buy_session.rs)

`tollgate_net::client::run` runs one buyer session: a small node inside your
program that dials one gateway, buys from it, and never sells. It needs no
mint and no `merchantd`: you hand it the vouchers to pay with. Two kinds of
program use it:

- **Software that buys for itself**: an app, a daemon or an agent on a
  machine that reaches the internet through a TollGate gateway. The session
  connects from the machine's own address, and under the gateway's usual
  `enforcer.identity: address` that address is what the gateway lets through
  at the rate bought.
- **A proxy that buys for a device without a client**: `proxyd` in
  tollgate-suite-wrt runs one session per phone on the LAN, with a
  `connector` so each session's connection comes from the phone's address.

```rust
use tollgate_net::client::{self, Buying, ClientSpec, Moved};

let (buying_tx, buying_rx) = tokio::sync::watch::channel(Buying::AtSpeed);
let (rate_tx, rate_rx) = tokio::sync::watch::channel(1_000_000u64); // bytes/s
let (moved_tx, moved_rx) = tokio::sync::watch::channel(Moved::default());

let spec = ClientSpec {
    secret_hex,               // this session's own key, hex; one per device
    gateway,                  // the gateway's public key, hex
    endpoint,                 // "192.168.1.1:4747"
    gateway_mint,             // the gateway's mint URL
    channel_capacity: 1 << 28,
    buying: buying_rx,
    moved: Some(moved_rx),    // needed for pay per use
};
client::run(spec, funding, None, rate_rx, shutdown).await?;
```

What the arguments mean:

- **`funding`** is where the session's vouchers come from: anything that
  implements `tollgate_net::channel::Funding`. That trait has two methods:
  `vouchers(mint, unit, amount)` returns a token of exactly that much, and
  `deposit(token)` takes back anything of value. The example uses
  `WalletFunding`, a wallet that mints at the gateway's mint; a real program
  pays out of what the device paid. Keep `channel_capacity` within what the
  device paid for, since a channel is funded in one go.
- **`connector`** (the `None` above) can make the session's connection come
  from the device's own address, so the gateway gates the device rather than
  your program.
- **`rate`** is a watch of bytes per second. Change it while the session runs;
  zero stops buying.
- **`shutdown`** is any future. The session ends when it resolves.

The session says `no_charge` to the gateway: it sells nothing, so the gateway
opens no channel toward it, and the device needs no mint of its own.

### Choosing how to pay

`ClientSpec::buying` is a watch of `Buying`. Choose per session:

- **`Buying::AtSpeed`**: time at a speed. The session reserves `rate` exactly
  and holds an hour of it. The budget drains at that rate whether the device
  uses it or not.
- **`Buying::PayPerUse { budget, renew_lead_ms }`**: pay for what you use. The
  session reserves nothing and holds `budget` units with the gateway, for as
  long as the gateway allows (30 days by default). Idle costs nothing. How fast
  the device is carried is the gateway's choice, its
  `burst.unreserved_rate`. `renew_lead_ms` is how early to buy again, and has
  to cover how late your `Moved` reports can be.

Send a new `Buying` on the watch to move a running session from one to the
other. Moving to a speed reserves it at once. Moving to pay per use ends the
reservation at once. Nothing already paid for is lost either way.

**For pay per use, tell the session what moved.** The session carries none of
the device's traffic, so it cannot count it. A program buying for itself can
count its own bytes, or read the machine's interface counters. Send it a `Moved` with the
**running totals** the gateway carried for the device: `to_device` (download)
and `from_device` (upload). The session uses the change between reports to
know when to buy again. A total lower than the last one is read as counting
again from zero, which is what the gateway's counters do when a session
starts again.

### Reading the budget

`client::run` does not publish its own count. To see where a session stands,
read the gateway's view of it. The example starts a gateway in the same
process and reads its control socket every second (part 4). Each row is what
the gateway holds for the session:

```text
$ cargo run -p tollgate-net --example buy_session -- --seconds 8
buying time at a speed from gateway 038a9d6dc7a2d30e…
   s   carried B/s      budget B    reserved B/s    deadline
   1  (not connected yet)
   2             0             0               0          0s
   3             0             0               0          0s
   4       1000000     268435456         1000000       3599s
   5       1000000     267435456         1000000       3598s
   6       1000000     266435456         1000000       3597s
```

For the first seconds the session connects and opens its channel. Then it
buys. An hour at 1 MB/s would be 3.6 GB, but a purchase can hold no more than
the channel it is paid over, so the budget is the channel's 256 MiB
(`channel_capacity`); when that runs low, the session buys again over a new
channel. The budget drops by the reserved rate every second, used or not.

With `--pay-per-use` the reserved rate is 0. The device is carried at the
gateway's cap, 2 MB/s, and the budget of 5 MB drops only by what the pretend
device moves, 400 kB a second. When it runs low the session buys back what
was used:

```text
$ cargo run -p tollgate-net --example buy_session -- --pay-per-use --seconds 16
buying pay per use from gateway 02c5ea899f2de5aa…
   s   carried B/s      budget B    reserved B/s    deadline
   …
   5       2000000       5000000               0    2591999s
   6       2000000       4600000               0    2591998s
   7       2000000       4200000               0    2591997s
  …
  11       2000000       2600000               0    2591993s
  12       2000000       5400000               0    2591999s
  13       2000000       5000000               0    2591998s
```

The deadline is 30 days, the longest the gateway allows. Just after a
purchase the budget can be a little above 5 MB: the session and the gateway
each keep their own count of what was used, and read it at slightly different
moments.

A program that runs a full buyer node, rather than a session, can read its
own count instead: `bought_budget`, `bought_reserved_rate` and the last
`reported_balance` in the snapshot (part 4).

## 2. An external enforcer

**Example:** [`crates/tollgate-protocol/examples/stub_enforcer.rs`](../../crates/tollgate-protocol/examples/stub_enforcer.rs)

`tollgated` has built-in enforcers for the kernel's forwarding path (`ip`),
for a FIPS mesh (`fips`) and for testing (`loopback`). For anything else
(a captive portal, a proxy, a tunnel, a water valve) write an **external
enforcer**: a separate program that owns the traffic, and that `tollgated`
tells who has paid for what. The full specification is
[tollgate-enforcer-protocol.md](../design/core/tollgate-enforcer-protocol.md).

An enforcer in Rust depends on `tollgate-protocol` alone. The messages are in
`tollgate_protocol::enforcer`, and the stub shows a complete one in about 400
lines of standard library code.

### How the two connect

Your enforcer **listens** on a Unix socket and `tollgated` **connects** to it.
By default the socket is `enforcer.sock` in the instance's runtime directory,
`/run/tollgate-<instance>/enforcer.sock`. Set `enforcer.socket` in
`tollgate.yaml` to move it. Make the socket readable by `tollgated` only:
whoever can open it can open the traffic.

Messages are CBOR maps, each preceded by its length as two bytes,
little-endian, the same framing as the TollGate wire protocol. Use
`enforcer::encode_frame` to write one, and `tollgate_protocol::FrameReader`
with `next_enforcer_message` to read them. The normative schema is
[`enforcer.cddl`](../../crates/tollgate-protocol/enforcer.cddl).

On the `tollgated` side:

```yaml
enforcer:
  kind: external
  identity: address     # required for external: pubkey or address
```

### The conversation

1. **Your enforcer speaks first**, with `hello`:

   ```rust
   use tollgate_protocol::enforcer::{self, EnforcerMessage, Hello, Identity, PROTOCOL_VERSION};

   let hello = Hello {
       version: PROTOCOL_VERSION,
       identity: Identity::Address, // what you match traffic by
       delegated: false,            // whether you accept delegated bindings
       unit: "byte".into(),         // what you count in
   };
   let mut out = Vec::new();
   enforcer::encode_frame(&EnforcerMessage::Hello(hello), &mut out)?;
   stream.write_all(&out)?;
   ```

   `tollgated` **checks** the identity and the unit and negotiates nothing.
   If `identity` differs from its `enforcer.identity`, or `unit` from its
   `mint.unit`, it refuses to start and names both:

   - `identity: address` means you recognize a payer by the address its
     traffic comes from. A subject is then 16 bytes: an IPv6 address, or an
     IPv4 address written as IPv6.
   - `identity: pubkey` means you recognize a payer by its 32-byte key, proven
     by the network it arrived over. Today only FIPS proves keys.

   `tollgated` sells nothing until it has a `hello` it accepts.

2. **`tollgated` sends the full state**: a `bind` and a `set` for every payer
   it knows. Then it sends only changes:

   | Message | Meaning | What to do |
   |---|---|---|
   | `bind(peer, bindings)` | Every subject this payer holds. Replaces any earlier list. | Remember which subjects belong to `peer`. If another payer already holds a subject, refuse it and send `conflict`. |
   | `set(peer, rate)` | `Some(0)`: closed. `Some(n)`: open, at most `n` units a second. `None`: open, unshaped. | Apply it to all of the payer's subjects. |
   | `remove(peer)` | The payer is gone. | Close its subjects and forget it. |

   Call `Hello::check(&binding)` on each binding: it fails for a subject of
   the wrong length or a delegated one you said you would not take. That is a
   protocol error, and you close the connection.

3. **Your enforcer reports**:

   | Message | When |
   |---|---|
   | `counters(peer, to_payer, from_payer)` | About once a second, for each payer whose counts changed. Running totals from the start of this connection: units sent to the payer's subjects, and units received from them. |
   | `conflict(peer, subject)` | When you refuse a binding because another payer holds the subject. |

### Rules to keep

- **Start closed.** Until a payer is bound and `set` open, carry nothing for
  anyone.
- **When the connection drops, close everything** and forget every binding.
  `tollgated` reconnects and sends the full state again.
- **One connection at a time.** A new one replaces the old one, and you reset
  to closed.
- **Never close the path to the node itself.** A payer must always reach
  `tollgated`, `mintd` and `merchantd`, or it cannot pay.
- `rate` is the payer's whole state. It already includes the minimum flow
  allowance and anything given above the reserved rate. You need nothing
  else.

### Try it

The stub enforces nothing. It holds what it is told and, with `--carry`,
pretends every open payer moved its full rate each second:

```sh
cargo run -p tollgate-protocol --example stub_enforcer -- \
    --socket /tmp/tollgate-ext/enforcer.sock --identity address --unit byte --carry
```

Then start `tollgated` with `enforcer.kind: external`,
`enforcer.identity: address` and `enforcer.socket` pointing at the same path.
[quickstart.md](quickstart.md#an-external-enforcer) shows both sides and what
they log. The `testing/external` topology runs the same pair in Docker.

## 3. A whole node

**Example:** `start_gateway` in
[`buy_session.rs`](../../crates/tollgate-net/examples/buy_session.rs)

`tollgated` is a thin program around `tollgate_net::node::Node`
([`src/bin/tollgated.rs`](../../crates/tollgate-net/src/bin/tollgated.rs)).
Your program can do the same. A node needs three things:

1. **A configuration.** Parse the same YAML an operator writes into
   `tollgate_net::config::File`, then `resolve()` it into a `NodeConfig`.
   Every key is described in
   [tollgate-configuration.md](../design/core/tollgate-configuration.md).

   ```rust
   let file: tollgate_net::config::File = serde_yaml::from_str(yaml)?;
   let config = file.resolve()?;
   ```

2. **A channel backend**, which carries the money. Use
   `SpilmanChannels::new(SpilmanConfig { .. })`, filled from the config, with
   a `Funding` for the vouchers this node pays its own peers with.
   `tollgated` uses `merchant::MerchantClient`, which asks `merchantd` over its
   socket; the example uses a `WalletFunding`.

3. **An enforcer**: anything that implements
   `tollgate_net::enforcer::Enforcer`. Use one of the built-in ones
   (`Loopback::new()`, `Ip::new(interface)` on Linux, `Fips::new(..)`,
   `External::connect(socket, expected).await` for an external enforcer), or
   implement the trait yourself if your program owns the traffic.

```rust
let node = Node::new(&config, channels, enforcer.clone());
let published = node.published();          // for the control socket, below
node.run(config, shutdown).await?;         // binds network.listen
// or: node.run_on(listener, config, shutdown).await? on a listener you bound
```

Some things `tollgated` does that a `Node` does not do for you:

- **Keeping budgets on disk.** Set `config.budget_file` to a path before
  `Node::new`, or budgets live in memory only. `tollgated` uses
  `config::budget_file(instance)`.
- **The loopback data plane.** With `Loopback`, start
  `dataplane::listen` and `dataplane::keep_dialing` yourself if you want real
  bytes to move, as `tollgated` does.
- **Demand.** A node buys only while it wants something from a peer. Call
  `enforcer.set_demand(peer, rate)`, or set `buying.demand` in the config.
- **The control socket.** Serve it yourself (part 4).
- **Changing how it buys while it runs.** `Node::with_buyer_policy` takes a
  watch of `BuyerPolicy`, which the node follows from its next decision.

## 4. The control socket and the snapshot

**Example:** [`crates/tollgate-net/examples/read_snapshot.rs`](../../crates/tollgate-net/examples/read_snapshot.rs)

Every running `tollgated` serves a **snapshot** of what it is doing, as JSON,
on a Unix socket: `control.sock` in its runtime directory,
`/run/tollgate-<instance>/control.sock` on Linux. `tolltop` reads it, and so
do the integration tests.

**One connection, one answer.** Connect, send nothing (or close your writing
half), and read one JSON document. Or send one line,
`{"command":"snapshot"}`, and get `{"status":"ok","data":{...}}` back. For
fresh data, connect again.

From Rust:

```rust
use tollgate_net::control;

let path = control::find_socket(Some("ip"))?;   // or None: the one running instance
let snapshot = control::fetch(&path).await?;
for peer in &snapshot.peers {
    println!("{} budget {} reserved {}/s", peer.pubkey, peer.budget, peer.reserved_rate);
}
```

From a shell:

```sh
nc -U -w 2 /run/tollgate-default/control.sock </dev/null
```

To serve the socket from your own `Node`:

```rust
tokio::spawn(control::serve(&path, node.published(), shutdown));
```

### What the snapshot holds

At the top: the node's `pubkey`, its `unit`, its `mint_url`, its
`uptime_ms`, and `peers`. For each peer:

| Field | Meaning |
|---|---|
| `pubkey`, `phase`, `access` | Who, how far its session has got (`opening` to `established`), and whether it is carried (`active`, `free`, `none`) |
| `shaped_rate` | The rate the enforcer carries it at now, units a second |
| **Selling to the peer** | |
| `budget`, `budget_expires_in_ms` | What it has left with this node, and time to its deadline |
| `reserved_rate` | The rate it reserved |
| `from_payer_weight` | What a unit it sends costs, against one for a unit sent to it |
| `incoming_channels` | Channels it pays this node over |
| **Buying from the peer** | |
| `bought_budget`, `bought_expires_in_ms`, `bought_reserved_rate` | What this node has left with the peer, by its own count |
| `reported_balance` | The last Balance the peer sent: `remaining`, `expires_in_ms`, `reserved_rate`, `age_ms`. Information only |
| `demand` | What this node wants from the peer, units a second |
| `outgoing_channel`, `rollover_ready` | The channel this node pays the peer over, and whether its replacement is ready |
| `refused_terms` | This node refused the peer's terms and buys nothing from it |
| **Counted** | |
| `to_payer`, `from_payer` | Units the enforcer carried to and from the peer |

Run the example against any running node:

```sh
cargo run -p tollgate-net --example read_snapshot -- --instance demo-gateway
```

The fields are listed in
[`control.rs`](../../crates/tollgate-net/src/control.rs). They may change
before 1.0.

## See also

- [quickstart.md](quickstart.md): run nodes, and what working looks like.
- [tollgate-enforcer-protocol.md](../design/core/tollgate-enforcer-protocol.md):
  the enforcer protocol in full.
- [tollgate-vouchers.md](../design/core/tollgate-vouchers.md): budgets,
  reserved rates, and how a buyer buys.
- [tollgate-configuration.md](../design/core/tollgate-configuration.md):
  every configuration key.
