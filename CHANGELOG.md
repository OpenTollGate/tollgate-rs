# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing has been released yet. Everything below is on `master` and will ship as
0.1.0.

### Added

- A customer's IPv6 is gated, shaped and metered with its IPv4. The nftables
  adapter ties a customer on the link to its MAC (from the IPv4 neighbour
  table) and to the global and ULA IPv6 addresses the IPv6 neighbour table
  lists for it, on the existing refresh: out by source MAC, in by destination
  address, marked into the peer's `tc` class and counted into its counters.
  An address stays with its peer until another MAC claims it. Past 16
  addresses for one MAC, its IPv6 is withheld until the count drops back
  (logged once at warn), so extra addresses cannot push the real one out of
  the class and counters; its IPv4 is unaffected. Documented in
  `peering-ip.md`, "A Customer's IPv6". `testing/forwarding` now runs a
  dual-stack customer, and its gateway has no minimum flow allowance, so an
  unpaid or lapsed customer is dropped rather than carried at 4096 B/s.

- The market publishes `max_amount` in `info`: the most one sale may be for,
  the lower of `merchantd`'s `mint.max_amount` and `mintd`'s per-quote limit,
  and refuses a larger sale before taking the payment. If issuing fails after
  the payment was taken, `merchantd` refunds it as a token in the same paper
  (`502 {"error", "refund"}`, `market::Refunded` to a buyer). Refused swaps are
  logged at warn. The refund pays its own swap fee, sized by what the mint
  quotes, and never draws on `merchantd`'s other money; a refund token never
  reaches a buyer's error message or log. A mint that does not answer the
  limit lookup is asked again only after 5 s, so a hung `mintd` does not hold
  every `info` and `swap` for 30 s. `Wallet::split` swaps a token into parts
  of at most a given size, fees paid out of it, for buyers splitting one
  payment into several sales.

- Settling disposes of what a channel paid. `tollgated` keeps nothing a
  settlement brings in: a channel funded in this node's own vouchers is burned
  at `mintd` (`mint.local`) once closed, and one funded in another mint is
  burned there or deposited with `merchantd`, as that mint's `settle` says in
  `vouchers.accepted_mints` — written as a bare URL, which keeps, or as
  `{ url, settle: keep | burn }`. A mint with no burn method has its proofs
  dropped with a warning. `mintd` gains the `burn` melt method on both
  listeners, unauthenticated, whose quote request carries the amount to burn;
  it is the only melt it advertises, and the public listener still refuses
  `/v1/mint/` without auto-accept. A mint set to `keep` with no `merchantd` at
  `merchant.socket` is a startup warning, not an error: the deposit fails and
  is retried with its settlement. Settling a channel this node funded reclaims
  its change once the receiver has closed it and deposits it with `merchantd`.
  Not yet: nothing triggers that funder-side settle, so change is only
  recovered when something calls `settle` on such a channel.

- `merchantd`: the node's commercial side, reading `merchant.yaml`. It holds
  the wallet — `tollgated` no longer holds any money or vouchers — and funds
  `tollgated`'s channels over a local socket (`fund`), taking back anything of
  value it hands over (`deposit`). It serves the market endpoints, pricing per
  Mbit in `usd`, `eur` or `sat`, with a default price and an optional price per
  accepted mint; a BTC rate is fetched from a fallback list of public APIs only
  when a price and the payment are in different units, skipping zero answers
  and keeping the last good rate when every source fails. What it sells is
  issued at `mintd`'s private listener with the buyer's blinded outputs.
  `merchanttop` shows and changes prices and tops the wallet up; `tolltop` is
  peers only. `tollgate.yaml` loses its `market` and `wallet` sections and
  gains `merchant.socket`. Not yet: `prefetch`, and buying from an upstream
  that does not give its vouchers away.

- `mintd`: the mint is its own daemon, reading `mint.yaml`, so `tollgated`
  holds none of its keys. It serves the standard Cashu API and nothing else,
  on a public listener for peers and wallets and a private one, on loopback,
  where mint quotes are paid on creation — how `merchantd` will sell. The
  public listener hands out quotes only when `auto_accept` is on, rationed by
  `issue_quotes_per_minute`. The keyset comes from the mint's own seed
  (`seed_file`, created on first start) rather than from the node's identity,
  so upgrading a node issues against new keys. `minttop` watches it.
  `tollgated` no longer runs a mint: `mint.url` is what it advertises, and it
  settles channels funded in its own vouchers at `mint.local` over HTTP like any
  other mint. The dormant market endpoint is no longer served until
  `merchantd` takes it over. The packages and the docker topologies run both
  daemons.

- `tollgate-protocol`: the wire format — messages, the CBOR codec and TCP
  framing — `no_std` + `alloc`. Messages are numbered contiguously, and every
  refusal carries a reason a peer can act on; `TopUpReject` names a rate the
  seller *would* take, so a buyer asking for more than the node-wide
  `max_rate` leaves after its other peers' grants re-buys inside one round
  trip.

- Free peering is on the wire: an `Offer` carries key 5 when the sender will
  not charge that peer, so the peer funds no channel toward it and buys
  nothing. The key is written only when true, so an ordinary `Offer` is
  unchanged and a peer that predates it decodes as charging.

- `tollgate-core`: grants, buying, metering and admission control, sans-IO and
  `no_std` + `alloc`, with tests written against the design documents' worked
  examples. A grant is a quantity paired with a window and buys a **rate**; a
  purchase takes effect on arrival, and the signed state is cumulative, so a
  `TopUp` is idempotent and a lost message costs nothing. One `TopUp` can carry
  a purchase across several channels. Raising a rate forfeits the old grant's
  remainder; the buyer's hysteresis only jumps early when demand rises by half
  again. Purchases are sized for what a peer charges on our uploads
  (`received_multiplier`), and each peer's Offer carries the multiplier it is
  charged under, its per-peer override included.

- `tollgated`, the node. It drives core over two TCP planes, shapes each peer
  to exactly what it bought with a token bucket, and holds a peer that has
  bought nothing at the minimum flow allowance — the trickle that lets a peer
  holding no vouchers reach a mint at all. It shuts down cleanly, drops silent
  peers, and backs off from a rate ceiling a peer refused for `cap_hold_ms`
  before trying it again.

- A `TopUp` that fails verification — a signature that does not verify, a
  total that does not increase, or a channel named twice — is answered with
  `Reject` (0x06) instead of being dropped in silence, and every `Reject` sent
  or received is logged. A channel that fails three purchases in a row is no
  longer honored and is settled at its last verified state.

- A Cashu mint in every node, with a byte-denominated keyset, and payment over
  real Cashu Spilman channels: a channel is a 2-of-2 multisig token, each
  ratchet turn a signed balance update the receiver can take to the mint. The
  channel layer enforces the ratchet and the signature and prices nothing;
  what a peer may draw is core's decision. Channels roll over before they run
  out.

- Spilman channel funding travels as compact CBOR carrying only what the
  receiver cannot derive: the channel's terms, the opening signature, and per
  funding proof the mint's signature and DLEQ proof. A 1 GiB channel's funding
  is 344 bytes rather than 1,581. A node built before this change cannot open a
  channel with one built after it, in either direction.

- A voucher market beside each node's mint: a node sells its vouchers for sats
  from mints it names (`market.accept`, with a `bytes_per_unit` per issuer), at
  a price the operator can move without restarting the node.

- A wallet: a node holds what it has bought and been paid, tops up from its
  `wallet.mint` over Lightning when a purchase needs more, and collects what it
  was paid for.

- A peer that drops without a Disconnect — a Wi-Fi blip, a bare FIN, or
  silence — is held for `stale_timeout_seconds`, and one that reconnects in
  that time resumes both channels instead of funding new ones. The grants start
  again from zero; the channels and what was signed on them carry over. A
  ChannelReady between Announce and Offer says which channels are still held,
  so nothing changes on the wire. A held session that expires, an orderly
  Disconnect and shutdown all settle the channels the peer paid on, each with
  its expiry as the retry deadline, and a held channel that reaches its settle
  point inside the grace period is settled then rather than resumed. A resumed
  channel starts its verification-failure count over, and keeps growing on
  rollover from its own capacity.

- `tolltop`, a dashboard over each node's control socket, which it finds on
  its own. Tabs for peers, pricing and the wallet; a top-up shows its Lightning
  invoice as a QR.

- Selling transit on the kernel forwarding path: nftables gates each peer and
  `tc` shapes only what the node forwards, leaving the payment path alone. A
  peer with no session is forwarded at the minimum flow allowance, and dropped
  only when the allowance is zero.

- The kernel path meters an upstream by its link, not its IP: what arrives
  from its MAC is what it delivered, and what is routed via it as next hop is
  what it was delivered. A peer is an upstream when some route uses it as a
  gateway, read from the kernel every few seconds. Customers are still metered
  by IP. On OpenWrt the package now depends on `ip-full`.

- Selling transit over a [FIPS](docs/design/network-peering/peering-fips.md)
  mesh: the FIPS node gates and shapes each peer to the rate it bought, set
  through its `set_transit_policy` control command. Peers with no session,
  named or not, are held at the allowance from their first packet, or kept
  local-only when it is zero. A peer's announced key is checked against the
  mesh address it arrived from. Needs a FIPS build with per-peer transit
  policy.

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

- A normative CDDL (RFC 8610) schema for the wire protocol,
  `crates/tollgate-protocol/tollgate.cddl`, and one for the Spilman funding
  blob, `crates/tollgate-net/src/channel/spilman/funding.cddl`. Tests validate
  every message the codec encodes against it, and fail when a message type has
  no schema entry, so a wire change cannot land unannounced. The idea is from
  #8 by Amperstrand, which proposed a schema for the protocol before its
  redesign.

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

- A channel settlement that fails is retried rather than logged and forgotten,
  so a mint that is briefly unreachable no longer costs the channel to its
  refund timelock. The node retries after 1 s, doubling to 5 min, until it
  succeeds or the node stops; a shutdown gives what is outstanding 5 s. A
  backend reports a failure no retry can fix as `CannotSettle`, and settling
  a channel that already settled succeeds.

- The market is deferred. A node's mint now issues its vouchers to anyone who
  asks — an ordinary NUT-04 mint quote, reported paid as soon as it is checked
  — and a buyer funds a channel by minting what it needs at the seller's mint,
  with no Lightning, money mint or swap involved. `mint.auto_accept` (on by
  default) turns it off. While it is on, service is free to any peer that can
  reach the mint, with no cap on how much: only requests are rationed, by
  `mint.issue_quotes_per_minute`, and a quote over that limit is refused. The
  market and the wallet's top-up are no longer on the funding path: a node
  still serves its market endpoint and its configuration still parses, but
  nothing it runs buys through a market any more. This is a deliberate break
  with no compatibility shim: a node on this version cannot fund a channel to a
  node on an earlier one, whose mint issues nothing for the asking.

- Channels track their expiry. The funder rolls a channel over when it enters
  the safety margin before expiry (`max(60 s, 2 × max_window_ms)`) as well as
  at the capacity threshold, and the receiver settles it half that margin
  before expiry, so a slowly drawn channel can no longer outlive its TTL and
  be reclaimed with earnings on it. The TTL is `channels.ttl_seconds` (default
  one hour) instead of a hard-coded two, and a receiver refuses a channel
  expiring sooner than half its own TTL rather than a fixed hour — so a node
  on an earlier commit, which wants an hour left, refuses channels funded at
  the new one-hour default.
  `channels.safety_margin_seconds` (default 60) is the margin's floor, and
  `ttl_seconds` has to be at least twice the margin. A settlement that fails
  is retried until the channel's expiry and no further, since past it the
  funder can reclaim the channel anyway.

- Channels start at `channels.initial_capacity` (now 1 GiB) and grow by
  `capacity_growth_factor` (default 2.0) on each rollover forced by use,
  clamped to `min_capacity` (128 MiB) and `max_capacity` (16 GiB); a rollover
  forced by expiry keeps the size. The node's mint and market now cap one swap
  at `max_capacity` rather than `initial_capacity`.

- The market sells a capacity that is not a whole number of units. It checks
  a payment against the capacity's price rounded up to a whole unit, the rule
  the buyer pays by, instead of requiring outputs worth exactly
  `paid × bytes_per_unit` — which refused 1 GiB at a megabyte a sat (1074 sat)
  in full. A buyer now reports the market's refusal rather than a JSON parse
  error.

### Removed

- `tollgate-pricing.md`, replaced by `tollgate-hazards.md`.
- `tollgate-bootstrap.md` and its diagrams, with bootstrap tokens.

### Fixed

- `mintd` applies `max_amount` (and its URL, name and methods) from its config
  at every start: cdk kept serving the copy of its info saved on first start,
  so a raised limit never took effect. The default `max_amount` for `mintd`
  and `merchantd` is now 1 TiB, up from 16 GiB, which about 960 sat bought at
  the packaged price.

- `Wallet::collect` checks that a quote is paid before minting against it.
  cdk reserved an unpaid quote and never released it, so every later claim,
  once paid, failed with "already in use by another operation"; such a quote
  is now released and claimed.

- A `TopUp` refused in part no longer moves a channel's recorded state. The
  Spilman backend kept each update as it verified it, so when a purchase
  spanning a rollover had one bad signature, or core declined it, the
  receiver still held the good update and would have settled at a state no
  grant paid for. `ChannelBackend::verify_update` now keeps nothing; a new
  `record_update` keeps the state, called only once core accepts the whole
  purchase and emits `Action::RecordUpdates`.

- A channel a peer funded in another mint we accept now settles. The closing
  swap goes to the mint the channel was funded in: our own in process, as
  before, and any other over HTTP. It used to go to our own mint every time,
  which never issued the funding proofs and so could not swap them.

- The mint's spent-proof set is kept on disk (`mint.file`, by default
  `mint.sqlite` beside the wallet) instead of in memory. The keyset is derived
  from the identity and survives a restart, so an in-memory spent set let every
  voucher already redeemed redeem again after one. Both packages keep the file
  across an upgrade.
