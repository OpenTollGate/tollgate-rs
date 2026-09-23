# tollgate-rs v0.1.0

**Released**: not yet — this is the draft for the first release.

v0.1.0 is the first release of tollgate-rs, and it is the first time two
TollGate nodes buy and sell network capacity from each other with real ecash
at stake. A node runs its own Cashu mint, sells vouchers against it for sats,
and delivers exactly the rate a peer bought — enforced in the kernel's
forwarding path, not only counted. A peer that has bought nothing still gets
a trickle, so that it can reach a mint and buy.

There is no earlier release, so there is nothing to upgrade from and nothing
to stay compatible with. The wire format is new, and it may still change
before 1.0.

## At a glance

### Who this is for

- **Operators with an OpenWrt router** who want to sell its uplink: install
  the `.ipk`, name the mints you take payment in, and the router sells
  transit to anyone who pays.
- **Mac users** who want to buy from such a router: install the `.pkg`, top
  up the wallet from `tolltop` by paying a Lightning invoice, and the node
  buys capacity as traffic needs it.

### Before you install

- **The money is real.** A node configured against a public mint holds bearer
  ecash in its wallet. Treat the wallet file like cash.
- **Every node needs a secret key**, and a buyer needs its seller's public
  key in its config. `tollgated --show-identity` prints a fresh pair.
- **Building from source needs `protoc`.**

### What is in it

- A node, `tollgated`, that sells and buys capacity over TCP, with a Cashu
  mint, a voucher market and a wallet built in.
- Payment over Cashu Spilman channels, as prepaid grants: a quantity paired
  with a window, which buys a rate.
- Enforcement on the kernel forwarding path with nftables and `tc`, and
  gating over a FIPS mesh.
- `tolltop`, a dashboard for a running node.
- Packages for OpenWrt and macOS.

## How it works

A grant is a quantity paired with a window, so what a buyer pays for is a
rate. A purchase takes effect on arrival: the signed state is cumulative, so a
`TopUp` is idempotent, a lost message costs nothing, and a buyer may draw a
rate in the same breath as buying it. Nothing is delivered before it is paid
for.

Raising a rate mid-window forfeits what was left of the grant in force. That
is what makes the product bandwidth rather than a stored quantity of bytes,
and it is why the buyer only jumps early when demand rises by half again.

A seller can cap what one peer may buy (`max_rate`). Asked for more, it
refuses before taking any money and names a rate it would take, and the buyer
re-buys at that rate inside one round trip.

The payment protocol never prices anything. A voucher is a claim on one byte
of a node's capacity, redeemed one for one. What a byte costs in money is set
in the node's market — `bytes_per_unit` per issuer it accepts — which the
payment protocol never sees.

## Known limitations

**FIPS can gate but not shape.** A grant buys a rate, and FIPS exposes only a
binary forwarding policy per peer. Over FIPS a node can admit or refuse a
peer's transit, but cannot hold it to the rate it bought. The kernel
forwarding path has no such gap. The FIPS change is requested as feature 2 in
[FIPS_FEATURE_REQUESTS.md](docs/design/FIPS_FEATURE_REQUESTS.md).

**Issuer trust is not zero.** A voucher is a claim on the node that issued
it. Nothing cryptographic makes a node deliver what its vouchers promise;
what limits the exposure is policy and reputation. See
[issuer-risk.md](docs/design/market/issuer-risk.md).

**Public mints are somebody else's server.** A top-up is the one step that
depends on a stranger's server being up. The shipped configs list several
mints for that reason; if one refuses, point the wallet at another.

## Compatibility

This is the first release. Nodes built from different commits before it make
no promise to interoperate, and nodes on v0.1.0 make none with them.

## Getting v0.1.0

- **OpenWrt**: `.ipk` at the v0.1.0 release page.
- **macOS**: `.pkg` at the v0.1.0 release page.
- **From source**: `cargo build --release` from a checkout of the v0.1.0 tag
  (Rust 1.94.1 per `rust-toolchain.toml`; `protoc` is a required build
  prerequisite). See [packaging/README.md](packaging/README.md) to build the
  packages yourself.

The full changelog lives in [`CHANGELOG.md`](CHANGELOG.md). Issues and
discussion at
[github.com/OpenTollGate/tollgate-rs](https://github.com/OpenTollGate/tollgate-rs).

## Contributors

Thanks to everyone who contributed code, design work, bug reports, or reviews
to this release.

- [@Origami74](https://github.com/Origami74) (Arjen): the voucher design, the
  protocol, core and node, payment over Spilman channels, the market and
  wallet, `tolltop`, and the packages.
- [@c03rad0r](https://github.com/c03rad0r): internet exit over GRE and the
  Wi-Fi mesh architecture
  ([#6](https://github.com/OpenTollGate/tollgate-rs/pull/6)).
- [@felixfelix-bot](https://github.com/felixfelix-bot) (Felix): Nostr CI with
  ngit-ci ([#11](https://github.com/OpenTollGate/tollgate-rs/pull/11)).
