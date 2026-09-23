# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing has been released yet. Everything below is on `master` and will ship as
0.1.0.

### Added

- `tollgate-protocol`: the wire format — messages, the CBOR codec and TCP
  framing — `no_std` + `alloc`. Messages are numbered contiguously, and every
  refusal carries a reason a peer can act on; `TopUpReject` names a rate the
  seller *would* take, so a buyer asking above `max_rate` re-buys inside one
  round trip.

- `tollgate-core`: grants, buying, metering and admission control, sans-IO and
  `no_std` + `alloc`, with tests written against the design documents' worked
  examples. A grant is a quantity paired with a window and buys a **rate**; a
  purchase takes effect on arrival, and the signed state is cumulative, so a
  `TopUp` is idempotent and a lost message costs nothing. One `TopUp` can carry
  a purchase across several channels. Raising a rate forfeits the old grant's
  remainder; the buyer's hysteresis only jumps early when demand rises by half
  again. Purchases are sized for what a peer charges on our uploads
  (`received_multiplier`).

- `tollgated`, the node. It drives core over two TCP planes, shapes each peer
  to exactly what it bought with a token bucket, and holds a peer that has
  bought nothing at the minimum flow allowance — the trickle that lets a peer
  holding no vouchers reach a mint at all. It shuts down cleanly, drops silent
  peers, and backs off from a rate ceiling a peer refused for `cap_hold_ms`
  before trying it again.

- A Cashu mint in every node, with a byte-denominated keyset, and payment over
  real Cashu Spilman channels: a channel is a 2-of-2 multisig token, each
  ratchet turn a signed balance update the receiver can take to the mint. The
  channel layer enforces the ratchet and the signature and prices nothing;
  what a peer may draw is core's decision. Channels roll over before they run
  out.

- A voucher market beside each node's mint: a node sells its vouchers for sats
  from mints it names (`market.accept`, with a `bytes_per_unit` per issuer), at
  a price the operator can move without restarting the node.

- A wallet: a node holds what it has bought and been paid, tops up from its
  `wallet.mint` over Lightning when a purchase needs more, and collects what it
  was paid for.

- `tolltop`, a dashboard over each node's control socket, which it finds on
  its own. Tabs for peers, pricing and the wallet; a top-up shows its Lightning
  invoice as a QR.

- Selling transit on the kernel forwarding path: nftables gates each peer and
  `tc` shapes only what the node forwards, leaving the payment path alone.

- Selling transit over a [FIPS](docs/design/network-peering/peering-fips.md)
  mesh: the FIPS node gates and shapes each peer to the rate it bought, set
  through its `set_transit_policy` control command. Unnamed peers are held at
  the allowance from their first packet, and a peer's announced key is checked
  against the mesh address it arrived from. Needs a FIPS build with per-peer
  transit policy.

- Packages for OpenWrt (`.ipk`) and macOS (`.pkg`). The OpenWrt package sells
  out of the box at 1000 sat for an hour at 5 MB/s; a Mac, once configured with
  its router's key and a `buying.demand`, keeps a grant in force on its own.

- Docker integration topologies under `testing/`: peering, purchase, refusal,
  rollover, allowance, forwarding and fips, each asserting against the nodes'
  control sockets rather than their logs.

- CI on GitHub Actions (format, clippy, tests, the `no_std` build for
  `thumbv7em-none-eabihf`, and every docker topology except fips), package
  builds for OpenWrt and macOS, and a Nostr CI (ngit-ci) workflow under
  `.ngit/`.

- `CONTRIBUTING.md`, `PR-REVIEW.md`, this changelog and `RELEASE-NOTES.md`.

### Changed

- The protocol is redesigned around **vouchers**. A voucher is a claim on one
  unit of a node's capacity, issued by that node's own mint; delivery has no
  price, and what a unit costs in money is settled in the market, which the
  payment protocol never sees. This replaces bootstrap tokens and the old
  pricing scheme.

- Payment is **prepaid capacity as grants** instead of settling metered
  intervals, and netting is gone with the intervals.

- Negative prices are replaced by an unsigned `received_multiplier`: a node can
  surcharge what a peer pushes at it, but never pays a bonus on top of what it
  owes for delivery. Direction classes are removed.

- A node accepts vouchers from several mints, as one ordered list.

- Raw TCP is the v1 transport; HTTP and WebSocket are specified as future
  alternatives.

- Payment rides upstream `cdk-spilman` rather than a fork, now that it
  round-trips a custom currency unit, and every cdk crate is on v0.18.1.

### Removed

- `tollgate-pricing.md`, replaced by `tollgate-hazards.md`.
- `tollgate-bootstrap.md` and its diagrams, with bootstrap tokens.
