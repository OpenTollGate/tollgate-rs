# TollGate Configuration

This document specifies the configuration schema for TollGate — the YAML format, parameter hierarchy, defaults, and platform-specific paths.

## Overview

TollGate uses YAML-based configuration following the same pattern as FIPS. Every parameter has a sensible default — a minimal config only specifies what differs.

Delivery has no price to configure: one voucher buys one unit ([tollgate-vouchers.md](tollgate-vouchers.md)). What a unit costs in money is decided where vouchers are sold — by `merchantd`, not in the protocol's configuration.

A node runs as three daemons ([tollgate-daemons.md](tollgate-daemons.md)), and each reads its own file:

| File | Daemon | Configures |
|------|--------|------------|
| `tollgate.yaml` | `tollgated` | The protocol: identity, accepted mints, channels, grants, peers. **Most of this document** |
| `mint.yaml` | `mintd` | This node's mint: unit, keyset storage, listeners, auto-accept |
| `merchant.yaml` | `merchantd` | Prices, accepted payment, the market endpoints, the wallet |

The search paths below apply to each file by name. A price never appears in `tollgate.yaml`.

---

## Configuration Loading

### Search Paths

When started without `-c`, TollGate searches for `tollgate.yaml` in these locations (lowest to highest priority):

| Priority | Path | Purpose |
|----------|------|---------|
| 1 (lowest) | `/etc/tollgate/tollgate.yaml` | System-wide defaults (OpenWrt, Linux) |
| 2 | `~/.config/tollgate/tollgate.yaml` | User preferences (XDG) |
| 3 | `./tollgate.yaml` | Deployment-specific overrides |

All found files are loaded and merged in priority order. Values from higher priority files override lower ones.

### CLI Option

```
tollgated -c /path/to/tollgate.yaml
```

When `-c` is specified, only that file is loaded.

### OpenWrt

On OpenWrt, the primary config path is `/etc/tollgate/tollgate.yaml`. UCI integration is a future consideration — initially TollGate uses YAML directly.

---

## YAML Structure

```yaml
identity:    # Node identity (keypair)
network:     # Where the TollGate protocol listens
forwarding:  # What actually delivers the resource: loopback, nftables or fips
mint:        # Where this node's mint (mintd) is, and what it issues
merchant:    # Where merchantd is: upstream funding and foreign proceeds
vouchers:    # Which mints this node takes payment in, and per-peer traffic terms
access:      # Minimum flow allowance
channels:    # Spilman channel parameters
grants:      # Bounds on what a payer may buy in one purchase
peers:       # Static peer overrides
```

---

## Identity

```yaml
identity:
  # Path to file containing the secp256k1 secret key (hex or bech32)
  # If not specified, a new keypair is generated and saved to default location
  secret_key_file: "/etc/tollgate/identity.key"
```

The node's public key is derived from the secret key. This pubkey is used in:
- TollGate Announce messages
- Spilman channel creation (sender/receiver keys)
- Peer identification

---

## Network

```yaml
network:
  listen: "0.0.0.0:4747"      # control plane; the data plane is the next port up
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `listen` | `"0.0.0.0:4747"` | Control-plane listen address. The data plane listens on the next port up. Under `forwarding.mode: fips` this must be somewhere mesh peers reach — the node's own `fips0` address, or `[::]` — because a connection from anywhere else cannot prove whose key it announces and is refused |

---

## Forwarding

What actually delivers the resource, and so where access and rate are enforced ([peering-ip.md](../network-peering/peering-ip.md), [peering-fips.md](../network-peering/peering-fips.md)).

```yaml
forwarding:
  mode: loopback              # loopback, nftables or fips
  interface: "eth0"           # nftables only: the interface facing the peers
  fips_socket: ""             # fips only: the FIPS control socket; empty = FIPS's own default
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `mode` | `loopback` | `loopback` shapes and meters a socket of its own and forwards nobody's traffic — for demos and tests, and it runs anywhere. `nftables` gates and shapes the kernel's forwarding path, which is what sells transit; it needs Linux with `CAP_NET_ADMIN`. `fips` sells transit across a FIPS mesh, leaving enforcement to the FIPS node over its control socket — and, because a mesh address names a key, it is the only mode that checks a peer's announced identity rather than believing it |
| `interface` | `"eth0"` | Where the peers' `tc` classes live. Only `nftables` uses it |
| `fips_socket` | *(FIPS's default path)* | Only `fips` uses it |

The default is `loopback` because it runs everywhere and gates nothing it does not own: a node that installed firewall rules because a config line was missing would be a nasty surprise.

---

## Mint

A node that sells its own capacity runs its own mint and issues vouchers against it ([tollgate-vouchers.md](tollgate-vouchers.md)). **The block is optional**: a node without one issues nothing, runs no `mintd`, and takes payment only in other mints' vouchers — a pass-through relay, or a leaf that only buys. The mint is `mintd`, a separate daemon; this block tells `tollgated` where it is. `tollgated` uses it like any Cashu client — to settle channels and to burn what it was paid — and has no privilege there.

```yaml
mint:
  url: "https://gateway.example.com/mint"   # advertised in Offer
  local: "http://127.0.0.1:3338"            # how tollgated reaches mintd
  unit: "byte"                              # quantity unit for this resource
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `url` | *(none: this node issues nothing)* | Mint URL advertised to peers. Required if the block is present |
| `local` | `url` | Where `tollgated` reaches `mintd` itself, when that differs from what peers are told |
| `unit` | `"byte"` | Quantity unit — `byte`, `wh`, `ml` |

How the mint issues — auto-accept, its limits, its database — is `mintd`'s configuration, in [`mint.yaml`](#mintd).

The unit is fixed by the resource and must match across every node selling it. There is no per-direction unit: what a peer pays to have its outgoing traffic carried is the received multiplier, not a second keyset ([tollgate-vouchers.md](tollgate-vouchers.md)).

---

## Vouchers

Which mints this node will take payment in, and how welcome each peer's
outgoing traffic is.

```yaml
vouchers:
  accepted_mints:               # most preferred first; at least one
    - "https://gateway.example.com/mint"        # our own: always burned
    - url: "https://upstream.example.com/mint"
      settle: keep              # worth something: deposit with merchantd
    - url: "https://neighbor.example.com/mint"
      settle: burn              # a courtesy: destroy after settling

  received_multiplier: 0        # surcharge default; 0 = none
```

There are **no prices here.** Delivery is one voucher per unit, and a mint is
either accepted or it is not — what an issuer's paper is worth is expressed in
what you pay for it on the market, not in a discount applied at settlement
([tollgate-vouchers.md](tollgate-vouchers.md)).

No entry need be this node's own mint. A pure pass-through relay can list its
upstream's mint alone, take payment in vouchers it can spend directly, and
never issue any of its own. Order is a preference a payer should honor when it
can fund in more than one of them.

`settle` says what `tollgated` does with another mint's vouchers once a
channel funded in them has been settled
([tollgate-daemons.md](tollgate-daemons.md#other-mints-keep-or-burn)):
**keep** swaps them at their mint — so the payer can no longer spend them —
and deposits the fresh proofs with `merchantd`, for a mint whose paper this
node can spend, sell or be reimbursed for; **burn** melts them at their mint, for one
accepted as a courtesy. This node's own mint is always burned — the resource
has been delivered — and a bare URL means `keep` for any other.

`keep` needs somewhere to keep them: **`tollgated` refuses to start** if any
accepted mint resolves to `keep` and no `merchant.socket` is configured,
rather than silently destroying what it would have kept.

`received_multiplier` is an unsigned surcharge on what a peer pushes at us, on
top of that peer already being paid for delivering it. Netted out:

| Value | Net effect per unit that peer uploads |
|---|---|
| `0` (default) | We pay them 1× — we want the traffic |
| `1` | Nets to zero — their upload is free |
| `2` | They pay 1× — upload costs the same as download |
| `11` | They pay 10× — matches a 10:1 backhaul |

The net rate is `m − 1`, so to charge uploads at `k` times the download rate,
set `received_multiplier = k + 1`. Setting `10` gives 9×, not 10×.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `accepted_mints` | *(required)* | Mints this node will take payment in, best first; at least one. Each one taken on is that issuer's credit risk |
| `accepted_mints[].settle` | `keep` | `keep` or `burn`, for mints other than this node's own. Ignored for its own, which is always burned. `keep` without a `merchant` block is a startup error |
| `received_multiplier` | `0` | No surcharge; each side simply pays for what it received. Per-peer overrides in the `peers` section |

---

## Merchant

Where `tollgated` gets the upstream vouchers it funds channels with, and where
it hands the proceeds of channels settled in mints it **keeps**
([tollgate-daemons.md](tollgate-daemons.md#between-tollgated-and-merchantd)).

```yaml
merchant:
  socket: "/run/tollgate/merchantd.sock"   # local socket merchantd serves
  prefetch: 0                              # next channel fundings held per paid upstream; 0 = fetch on demand
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `socket` | *(platform state dir)* | Where `merchantd` listens. Absent `merchantd`, this node cannot fund channels to paid upstreams |
| `prefetch` | `0` | Fetch funding when a channel opens or rolls over. Counted per upstream peer, each sized to that peer's next channel capacity — upstreams differ in mint and capacity, so no single amount fits. Raise it only if measured round trips come near the rollover safety margin. Per-peer override in `peers` |

---

## mintd

`mint.yaml`. A NUT-compliant Cashu mint with no Lightning backend; see
[tollgate-daemons.md](tollgate-daemons.md#mintd) for what it does and does not
implement. It serves the same NUT API on two listeners, which differ only in
how a mint quote gets paid.

```yaml
unit: "byte"
seed_file: "/etc/tollgate/mint.seed"    # this mint's own seed; keysets derive from it
file: ""                                # mint.sqlite in the state directory

public:
  listen: "0.0.0.0:3338"                # anyone: swap, melt (burn), state check, …
private:
  socket: "/run/tollgate/mintd.sock"    # merchantd only: mint quotes paid on creation

auto_accept: true                       # public mint quotes paid on creation too
issue_rate_bytes_per_sec: 125000000     # ...but no faster than this
issue_burst_bytes: 4000000000
issue_quotes_per_minute: 60
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `unit` | `"byte"` | Must match `mint.unit` in `tollgate.yaml` |
| `seed_file` | *(generated on first start)* | The mint's own secret, separate from the node's identity key. Keysets are derived from it |
| `public.listen` | `"0.0.0.0:3338"` | The mint as peers and wallets see it. Mint quotes here are never paid unless auto-accept is on |
| `private.socket` | *(platform state dir)* | Mint quotes here are paid on creation. Whoever can open it can print this node's vouchers, so only `merchantd` should |
| `auto_accept` | `true` | Report every NUT-04 mint quote on the public listener paid, so a peer mints what it needs for free |
| `issue_rate_bytes_per_sec` | `125000000` | Vouchers an auto-accepting mint issues per second, across all askers; `0` = unlimited |
| `issue_burst_bytes` | `4000000000` | Issued at once before the rate applies; never less than one channel's initial capacity |
| `issue_quotes_per_minute` | `60` | Mint quotes created per minute, a minute's worth at once; `0` = unlimited |
| `file` | `mint.sqlite` in the state directory | The mint database: the spent-proof set and the mint quotes issued |

There is no issuance ceiling on what `merchantd` issues: how much to sell is its decision. The `issue_*` limits apply to auto-accept alone.

While the market is deferred, `auto_accept` is how a peer comes to hold this
node's vouchers: a buyer funds a channel by minting at the seller's mint, with
nothing paid. **Service is then free to any peer that can reach the mint.**
With `auto_accept: false` the public listener serves no mint quotes, so peers
can only pay with vouchers they bought from `merchantd` or came by some other
way.

Free is not unlimited. The `issue_*` keys ration how fast the mint gives
vouchers away, so its quote endpoint cannot be used to make the node sign and
store without end. The limit is node-wide, because the mint cannot tell one
asker from another, and a quote over it is refused when it is asked for. The
defaults sit well above honest use: 1 Gbit/s of vouchers is more than the node
could deliver, 4 GB at once is four default-size channels opening together,
and one quote a second is far more than a buyer needs, since it asks for one
per channel it opens.

The keyset is derived from `seed_file` — `mintd`'s own seed, not the node's identity key, so the mint's secret never has to be shared with `tollgated` — and survives a restart without the database. Losing the seed retires every voucher outstanding: the keyset cannot be rebuilt. The spent-proof set does not: losing `file` makes every voucher this node has already redeemed redeemable again.

The state directory is the first of `/var/lib/tollgate` and `/usr/local/var/lib/tollgate` that exists or can be created, then `$XDG_DATA_HOME/tollgate`, and `/tmp` as a last resort; `merchantd`'s wallet follows the same rule. A file under `/tmp` does not survive a reboot, and two nodes on one host that both leave `file` empty share one database, so set it explicitly anywhere but a dedicated host.

---

## merchantd

`merchant.yaml`. **Independent of everything above.** It configures what
this node sells its capacity for, which mints it takes as payment, the market
endpoints it serves them on — a separate protocol under a separate path, see
[market-protocol.md](../market/market-protocol.md) — and the wallet behind
them. A node that runs no `merchantd` still delivers and gets paid; it just
cannot sell or buy.

```yaml
listen: "0.0.0.0:3340"                  # HTTP: the market endpoints
socket: "/run/tollgate/merchantd.sock"  # fund / deposit, for tollgated only
control: "/run/tollgate/merchantd-control.sock"   # prices and accepts at runtime

market:
  enabled: false                        # serve the market endpoints at all
  path: "/tollgate/market/v1"           # prefix on `listen`, or an external URL to
                                        #   delegate to a third-party market

mint:
  private: "/run/tollgate/mintd.sock"   # where this node's vouchers are issued

price:                                  # default, for accepts entries without their own
  unit: "usd"                           # usd, eur or sat
  per_mbit: 0.00001                     # ≈ $0.08 per GB

rates:                                  # BTC price, tried in order; only fetched
                                        #   when price and payment units differ
  sources:                              # {CUR} = USD / EUR, {cur} = usd / eur
    - url: "https://mempool.space/api/v1/prices"
      path: "/{CUR}"
    - url: "https://api.coinbase.com/v2/prices/BTC-{CUR}/spot"
      path: "/data/amount"
    - url: "https://api.kraken.com/0/public/Ticker?pair=XBT{CUR}"
      path: "/result/XXBTZ{CUR}/c/0"
    - url: "https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies={cur}"
      path: "/bitcoin/{cur}"
  refresh_seconds: 300

accepts:                                # tokens we swap for our own vouchers
  - mint: "https://mint.minibits.cash/Bitcoin"
    unit: "sat"
    price: { unit: "sat", per_mbit: 0.8 }        # this issuer's own price
  - mint: "https://usd-mint.example.com"
    unit: "usd"                                  # no price: the default applies

cross_mint_swap: false                  # trade one mint's vouchers for another's

wallet:
  file: "/var/lib/tollgate/merchant-wallet.sqlite"
```

**Price** is per Mbit — one quantity to reason about — in `usd`, `eur` or
`sat`. A Mbit is 125 000 bytes. When a sale is paid in the price's own unit,
no rate is involved: `bytes per sat = 125 000 / per_mbit` for a `sat` price,
and likewise per cent for a `usd` or `eur` price paid in tokens of that unit
(Cashu counts both in cents). Otherwise the sale converts through the BTC
price in whichever fiat currency is involved, e.g. a `eur` price paid in sats
is `bytes per sat = 125 000 × btc_eur / (per_mbit × 10⁸)`, and a `usd` price
paid in `eur` tokens goes through both. With a `sat` price and only sat mints
accepted, `rates` is never fetched.

**Rates** are fetched per currency that needs one, `USD` and `EUR` by
default; `{CUR}` and `{cur}` in a source's `url` and `path` are replaced with
the currency code in upper and lower case. They come from each source in order; a source that fails, times out, or
answers zero is skipped for the next. `path` is a JSON pointer to the number
in the response. If every source fails, the last good rate stays in use.

There is **no list of mints to buy from.** `merchantd` buys whatever
`tollgated` asks it to fund — any upstream the node peers with, in whichever
mint that upstream takes. The need comes from the peerings, which change
while the node runs, so a static list could only ever be wrong.

**Accepts** is the list of mints, and the unit in each, whose tokens
`merchantd` will swap for this node's vouchers. Tokens from anything not on
it are refused rather than guessed at. Each entry is that issuer's credit
risk until `merchantd` spends or redeems what it took.

**Each entry may carry its own price**, in the same shape as the default:
`unit` and `per_mbit`. That is how an operator prices issuer risk — a sat
from a mint it trusts buys more than a sat from one it does not
([voucher-price-signal.md](../market/voucher-price-signal.md)). An entry
priced in its own token unit never needs a rate; one priced in another
currency converts like the default does. Entries without a price use
`price`.

There is **no quote step**: `merchantd` publishes the price in force for each
entry in its `info`, and a swap whose outputs no longer match it — because
the rate moved — is refused and retried by the buyer
([market-protocol.md](../market/market-protocol.md)). A price change takes
effect on the next swap.

`market.path` may be an external URL, in which case this node advertises
somebody else's market rather than running one. A node can offer swaps
without being a market maker, and a market maker can operate without
forwarding a byte. The design assumes a local market: only traffic to this
node's own daemons passes the gate unpaid, so a delegated market is the
operator's to make reachable for peers that cannot yet pay. Letting a named
external market through the gate is a possible later option.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `listen` | `"0.0.0.0:3340"` | Where the market endpoints are served over HTTP. Not 3339, which the speedtest uses |
| `socket` | *(platform state dir)* | Local socket for `fund` and `deposit`. Must match `merchant.socket` in `tollgate.yaml`; only `tollgated` should be able to open it |
| `control` | *(platform state dir)* | Local control socket: price, rate sources and `accepts` changed at runtime |
| `market.enabled` | `false` | Off by default; a node without a market still delivers and gets paid |
| `market.path` | `"/tollgate/market/v1"` | Local prefix, or an external URL to delegate to |
| `mint.private` | *(platform state dir)* | `mintd`'s private listener |
| `price.unit` | `"usd"` | `usd`, `eur` or `sat`. Decides whether a rate is needed at all |
| `price.per_mbit` | *(required unless every entry has its own)* | Default price. An entry with neither its own price nor a default is not sold against |
| `accepts[].price` | *(none: `price` applies)* | This issuer's own price, `unit` and `per_mbit` |
| `rates.sources` | the four above | Public, keyless APIs, templated per currency; replace or reorder freely. Unused unless price and payment units differ |
| `rates.refresh_seconds` | `300` | How often the rate is fetched |
| `accepts` | `[]` | Empty means nothing is sold |
| `cross_mint_swap` | `false` | No atomic implementation exists yet |
| `wallet.file` | *(platform state dir)* | Bearer tokens: the file *is* the balance |

---

## Access

```yaml
access:
  minimum_flow:
    enabled: false
    bytes_per_second: 0
```

A small amount of traffic every peer gets free, so a new peer can acquire vouchers before it can pay for anything, and so basic things work regardless. It is given away, so it can be farmed — see Minimum Flow Allowance in [tollgate-vouchers.md](tollgate-vouchers.md) for what bounds it and what does not.

It is a **rate**, and it is the floor of the shaper. A peer whose grant has expired falls back to it rather than to silence, which is what leaves it able to send the TopUp that buys the next grant.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `minimum_flow.enabled` | `false` | Off by default; it is traffic given away |
| `minimum_flow.bytes_per_second` | `0` | Keep small — resale value is bounded economically, not cryptographically. Being a rate, it cannot be accumulated |

---

## Channel Parameters

```yaml
channels:
  min_capacity: 134217728                  # smallest channel this node funds, bytes (128 MiB)
  max_capacity: 17179869184                # largest channel this node funds, bytes (16 GiB)
  initial_capacity: 1073741824             # first channel to a new peer, bytes (1 GiB)
  capacity_growth_factor: 2.0              # multiply capacity after each rollover forced by use
  ttl_seconds: 3600                        # channel expiry (default: 1 hour)
  rollover_threshold_pct: 80               # rollover at 80% capacity used
  safety_margin_seconds: 60                # floor of the margin before expiry
  stale_timeout_seconds: 60                # close a session whose peer has been silent this long, and hold it as long again for a return
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `min_capacity` | `134217728` | Smallest channel this node funds, in bytes (128 MiB) |
| `max_capacity` | `17179869184` | Largest channel this node funds, in bytes (16 GiB). Also the most this node's market and mint issue in one swap, since a peer paying us funds a channel of up to this much |
| `initial_capacity` | `1073741824` | First channel to a new peer, in bytes (1 GiB) |
| `capacity_growth_factor` | `2.0` | Multiplier applied to a channel this node funds when it is replaced for filling up. A channel replaced because it neared expiry keeps its size. At least `1.0` |
| `ttl_seconds` | `3600` | Lifetime of a channel this node funds (1 hour). As a receiver, this node refuses a channel expiring sooner than half its own TTL |
| `rollover_threshold_pct` | `80` | Trigger rollover at 80% exhaustion |
| `safety_margin_seconds` | `60` | The floor of the safety margin, which is `max(safety_margin_seconds, 2 × max_window_ms)` — see [Safety Margin](tollgate-payment-channels.md#safety-margin) |
| `stale_timeout_seconds` | `60` | Session closed if the peer sends nothing for this long. Also how long a session that ended without a Disconnect is held, so a peer that reconnects can resume its channels; then its incoming channels are settled, as is any held channel that reaches its settle point first. `0` disables both: silence never closes a session, and a disconnect settles at once |

Capacities must satisfy `0 < min_capacity ≤ initial_capacity ≤ max_capacity`, and `ttl_seconds` must be at least twice the safety margin, so a channel is never born inside its own margin.

`capacity_growth_factor` rewards a peer relationship that has proven stable across rollovers. It applies to the channels this node funds — the peer's revenue channels — since only the funder chooses a channel's size. Every channel is funded by the party that owes, so growth always tracks a paying relationship and has nothing to run away on; `max_capacity` bounds it. Growth follows the channel in use: a peer that reconnects within the grace period resumes its channels, so the next rollover grows from where it was, while one that starts a fresh session starts again from `initial_capacity`.

---

## Grants

What this node will accept when a peer buys capacity. A grant is a quantity of units paired with a window to spend it in, and the rate it buys is one divided by the other ([tollgate-vouchers.md](tollgate-vouchers.md)).

```yaml
grants:
  window_range_ms: [200, 30000]      # payer picks any window in this range, per grant
  max_rate: null                     # units/second this node will commit across all buyers; null = link capacity
```

`window_range_ms` is advertised in the Offer, and the two ends do different jobs:

- **Upper bound** caps how far ahead capacity can be bought, which is what stops a buyer accumulating off-peak claims and presenting them at peak. Long windows also mean a large forfeit when a payer raises its rate early, so a high ceiling is not a favor to the payer.
- **Lower bound** caps how many grants can arrive per second, and therefore how many signature verifications a peer can impose. On an ESP32 that is the binding constraint, not bandwidth. Raise it on constrained hardware.

There is no minimum grant size. A short window already bounds message rate, and a small grant is cheap to serve.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `window_range_ms` | `[200, 30000]` | Payer chooses per grant. Five verifications per second worst case, and no claim held longer than 30 s |
| `max_rate` | `null` | Total rate this node will commit across all buyers at once. A TopUp that would exceed it is refused with the rate still available attached |

There is no transit-loss tolerance. Counters are not exchanged, so there is no second number to disagree with — see [tollgate-metering.md](tollgate-metering.md).

---

## Peer Overrides

```yaml
peers:
  # Do not charge this peer (operator's own nodes, friends)
  "02abc...":
    no_charge: true

  # Charge this peer 10× for what it pushes at us (net rate is m − 1)
  "03def...":
    received_multiplier: 11

  # Block a peer entirely
  "04ghi...":
    blocked: true

  # Hold the next funding for this upstream ahead of time
  "05mno...":
    prefetch: 1

  # Static peer endpoint (IP peering only)
  "05jkl...":
    endpoint: "192.168.1.1:4747"
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `no_charge` | `false` | Do not charge this peer, and tell it so in the Offer, so it funds no channel toward this node. One-sided — whether the peer charges back is its own decision |
| `received_multiplier` | *(from `vouchers.received_multiplier`)* | Unsigned surcharge on what this peer pushes at us, on top of it being paid for delivering it |
| `blocked` | `false` | Refuse all service to this peer |
| `prefetch` | *(from `merchant.prefetch`)* | Channel fundings held ahead for this upstream |
| `endpoint` | *(none)* | Static endpoint for IP peering |

There is no `price_multiplier`. Favoring a peer means selling it vouchers more cheaply, which happens outside the protocol. `no_charge` is **not transitive** — it means free for that peer's own traffic, never free for anything that peer is nominally the beneficiary of.

---

## Full Example

```yaml
identity:
  secret_key_file: "/etc/tollgate/identity.key"

mint:
  url: "https://gateway.example.com/mint"
  unit: "byte"

merchant:
  socket: "/run/tollgate/merchantd.sock"

vouchers:
  accepted_mints:
    - "https://gateway.example.com/mint"                # our own: burned
    - url: "https://upstream.example.com/mint"          # spend these onward
      settle: keep
  received_multiplier: 2                                # uploads cost the same as downloads

access:
  minimum_flow:
    enabled: true
    bytes_per_second: 4096

channels:
  initial_capacity: 1073741824
  max_capacity: 8589934592
  ttl_seconds: 3600
  rollover_threshold_pct: 80

grants:
  window_range_ms: [200, 30000]

peers:
  "02abc...":
    no_charge: true
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting `tollgated`:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Received multiplier | Yes | Sent as a revised Offer; takes effect on the peer's next grant, never on one already bought |
| Minimum flow allowance | Yes | Applies immediately — it is a shaping rate, not a budget |
| Grant window range | Yes | Sent as a revised Offer; applies to the next grant |
| Max rate | Yes | Lowering it does not revoke a grant already sold; it refuses the next one |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Prefetch | Yes | Applies from the next funding |
| Accepted mints | No | A channel is funded in a specific mint, so dropping one would strand it. New sessions only |
| Channel parameters | No | Applies to new channels only |
| Own mint URL and unit | No | Requires restart; changing them invalidates outstanding vouchers |
| Identity | No | Requires restart |

`merchantd` has its own runtime changes:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Price, default and per mint | Yes | Applies to the next swap |
| Rate sources | Yes | Applies from the next refresh |
| Accepted payment mints | Yes | Applies to the next swap |

And `tollgated`'s `settle` policy per mint applies from the next settlement.

`mint.yaml` has no runtime changes: any change, auto-accept included, takes a `mintd` restart.

Each daemon watches its own config file for changes and applies runtime-changeable parameters without interrupting active sessions or sales.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Format | YAML | Follows FIPS pattern, human-readable, supports comments |
| Loading | Cascading multi-file with priority | System defaults + user overrides + deployment specifics |
| Defaults | Every parameter has a sensible default | Minimal config for simple deployments |
| Mint block | Optional. Where `mintd` is, and its unit; no privilege there | Issuing sits in its own daemon. A node that issues nothing — a pass-through relay, a buying leaf — needs no `mintd` at all |
| Merchant block | A socket to `merchantd`, and `prefetch` counted in fundings per upstream | `tollgated` holds nothing of value, so it asks for upstream vouchers when it needs them; upstreams differ in mint and capacity, so no single amount fits |
| Grants block | Replaces the metering block | There is no interval to negotiate and no drift tolerance to set. What is left is a bound on what a payer may buy in one purchase |
| Window range | Advertised in the Offer; payer picks per grant | Upper end bounds buying off-peak for peak, lower end bounds signature verifications per second |
| Minimum flow allowance | A rate, not a per-interval quantity | It is the floor of the shaper and what a peer falls back to when its grant expires. A rate cannot be accumulated |
| Transit-loss settings | Removed | Counters are no longer exchanged, so there is no second number to disagree with |
| Accepted mints | One ordered list, at least one entry, no prices | Accept or refuse is binary; what an issuer's paper is worth belongs on the market |
| Received multiplier | Unsigned, per peer | Prices scarce uplink and signals how welcome a peer's traffic is, without any signed number in the protocol |
| Market services | Own daemon (`merchantd`), own file, own endpoints, own protocol, disabled by default | Buying and swapping is not part of paying for delivery. `path` may point at a third party, so a node can offer swaps without running a market |
| One file per executable | `tollgate.yaml`, `mint.yaml`, `merchant.yaml` | Each daemon can be restarted, replaced or run by someone else without touching the others' settings. Prices never reach the protocol daemon |
| Other mints after settlement | Per-mint `settle: keep \| burn` in `tollgate.yaml`; `keep` without `merchantd` refuses to start | Whether another issuer's paper is worth anything depends on the relationship; value is never destroyed silently |
| Quote step | None: swap at the price in force | A moved rate costs a retry. Quotes can be added later |
| Selling price | Per Mbit, in `usd`, `eur` or `sat`; a default `price`, overridable per `accepts` entry | One quantity to reason about, in the operator's unit, and priced per issuer because that is how issuer risk is priced. A rate is fetched only when price and payment units differ; zero answers are skipped, and with every source down the last good rate is used |
| mintd privilege | A second listener serving the same NUT API, where mint quotes are paid on creation | No custom endpoints: the privilege is the address, not the call |
| Per-peer favoritism | Sell that peer vouchers cheaper, outside the protocol | Same capability, no multiplier machinery |
| Free peering | Per-peer `no_charge` flag, one-sided, not transitive | It is a decision about a relationship rather than a price; transitivity would launder free transit for others |
| Capacity growth | Applies to every channel | Every channel is funded by the party that owes, so growth always tracks a paying relationship |
