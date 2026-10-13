# tollgate-rs v0.1.0-rc.1

**Tagged**: 2026-10-13, as `v0.1.0-rc.1`. This is a **release candidate** for
v0.1.0, the first release. It is everything built so far, tested between
real nodes, in the Docker topologies under `testing/` and on an OpenWrt
router. If nothing serious turns up, v0.1.0 follows from it.

TollGate lets one device pay another for a metered resource, here network
access. A payer prepays a budget with a provider in vouchers issued by the
provider's own mint, one voucher per unit (one byte), over a Spilman payment
channel. The provider carries the payer at what it paid for, and counts what
it carried.

**Before 1.0, everything may change**: the Rust APIs, the configuration keys
and the wire format. A node on this release makes no promise to talk to one
built from another commit. If you depend on the crates, pin the tag.

## What is in it

The full list is under `[0.1.0-rc.1]` in [CHANGELOG.md](CHANGELOG.md). The
main points:

- **One accounting rule.** Each payer has a budget, a deadline and a reserved
  rate. A purchase adds to the budget, and nothing already paid for is lost.
  Every second the provider draws the larger of what the payer moved and the
  rate it reserved. Reserving a rate buys time at a speed; reserving nothing
  is pay per use ([tollgate-vouchers.md](docs/design/core/tollgate-vouchers.md#the-one-rule)).
- **The from-payer weight.** `grants.from_payer_weight` says what a unit the
  payer sends costs, against one for a unit sent to it. `10` suits a line with
  a tenth as much upload as download; `0` makes uploads free. A buyer can
  refuse a provider whose weight is too high, before any money moves.
- **Balance.** The provider tells the payer what is left of its budget, the
  time to its deadline and its reserved rate.
- **Budgets on disk.** A payer's budget is kept in
  `budgets-<instance>.json` in the state directory. It comes back when the
  payer reconnects or the node restarts, until its deadline.
- **Buyer sessions choose how to pay.** `tollgate_net::client` sessions pay
  for time at a speed or per use, and can move between the two while they
  run.
- **The enforcer protocol.** With `enforcer.kind: external`, a separate
  program controls the traffic, and `tollgated` tells it who has paid for what
  over a Unix socket
  ([tollgate-enforcer-protocol.md](docs/design/core/tollgate-enforcer-protocol.md)).
  The enforcer's `hello` states the identity and unit it was built for, and
  `tollgated` refuses to start behind one that does not match. A stub
  enforcer is included as an example.
- **`enforcer.identity: pubkey | address`** says who a connecting peer is:
  its key, proven by the network it came over (today only FIPS), or the
  address it came from.
- **Named instances.** One machine runs `tollgated` once per task, each
  under a name (`instance:` or `--instance`), with its own runtime directory
  and control socket, `/run/tollgate-<instance>/control.sock`. `tolltop`
  finds every running instance and Tab moves between them.
- **`burst`**: how fast a payer is carried above what it reserved, or when it
  reserved nothing.
- **A quiet link is kept alive**, and a failed first channel funding is asked
  for again.
- **IPv6** of a customer is gated, shaped and counted with its IPv4 on the
  `ip` enforcer.
- **`selling: false`** is specified for a node that only buys
  ([tollgate-configuration.md](docs/design/core/tollgate-configuration.md#selling)),
  but **not implemented**: `tollgated` does not accept the key yet. Use
  `no_charge: true` per peer meanwhile.

## Known limits

- **Channels are kept in memory only.** A `tollgated` that restarts loses its
  open channels. Budgets survive; channel state does not. Settlement retries
  are in memory too.
- **The configuration is not reloaded while running.** The configuration
  document lists settings that can change at runtime; today every change
  needs a restart.
- **A tiny reserved rate may draw nothing.** Draws are counted in whole
  units, rounded down, so a reserved rate of a few units a second can come to
  nothing over a short stretch.
- **Binding conflicts are a placeholder.** When two payers claim the same
  subject at an external enforcer, the second is refused and kept closed. The
  full rule is still open
  ([Conflicts](docs/design/core/tollgate-enforcer-protocol.md#conflicts)).
- **Selling over FIPS needs a FIPS build with per-peer transit policy**
  (`set_transit_policy`).
- **The market is deferred.** Each mint gives its vouchers away while
  `auto_accept` is on, the default, so service is free to any peer that can
  reach the mint. A node that sells turns it off.
- **Issuer trust is not zero.** A voucher is a claim on the node that issued
  it; nothing cryptographic makes that node deliver. See
  [issuer-risk.md](docs/design/market/issuer-risk.md).

## Getting it

- **From GitHub:** `git clone https://github.com/OpenTollGate/tollgate-rs.git`
  and `git checkout v0.1.0-rc.1`.
- **Over Nostr, with ngit:**
  `git clone nostr://npub1hw6amg8p24ne08c9gdq8hhpqx0t0pwanpae9z25crn7m9uy7yarse465gr/relay.ngit.dev/tollgate-rs`.
- **As a Rust dependency:**
  `tollgate-net = { git = "https://github.com/OpenTollGate/tollgate-rs", tag = "v0.1.0-rc.1" }`,
  and likewise `tollgate-core` and `tollgate-protocol`.
- **Building** needs Rust 1.94.1 (pinned in `rust-toolchain.toml`) and
  `protoc`. Packages for OpenWrt and macOS are built from the same source;
  see [packaging/README.md](packaging/README.md).

Guides for running and embedding TollGate were written after the tag and
apply to it: [getting-tollgate.md](docs/guides/getting-tollgate.md) has the
details above, [quickstart.md](docs/guides/quickstart.md) runs a node, and
[embedding.md](docs/guides/embedding.md) uses the library.

## Upgrading

Nothing to upgrade from: this is the first tagged release.

## Contributors

Thanks to everyone who contributed code, design work, bug reports, or reviews
to this release.

- [@Origami74](https://github.com/Origami74) (Arjen): the voucher design, the
  protocol, core and node, payment over Spilman channels, the market and
  wallet, the enforcer protocol, `tolltop`, and the packages.
- [@c03rad0r](https://github.com/c03rad0r): design docs for internet exit
  over GRE and the Wi-Fi mesh architecture
  ([#6](https://github.com/OpenTollGate/tollgate-rs/pull/6)).
- [@felixfelix-bot](https://github.com/felixfelix-bot) (Felix): Nostr CI with
  ngit-ci ([#11](https://github.com/OpenTollGate/tollgate-rs/pull/11)).
- [@Amperstrand](https://github.com/Amperstrand): the idea of a normative CDDL schema for the protocol
  ([#8](https://github.com/OpenTollGate/tollgate-rs/pull/8)).
