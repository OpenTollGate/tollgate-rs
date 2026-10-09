# TollGate Configuration

This document specifies the configuration schema for TollGate — the YAML format, parameter hierarchy, defaults, and platform-specific paths.

## Overview

TollGate uses YAML-based configuration following the same pattern as FIPS. Every parameter has a sensible default — a minimal config only specifies what differs.

Delivery has no price to configure: one voucher buys one unit ([tollgate-vouchers.md](tollgate-vouchers.md)). What a unit costs in money is decided where vouchers are sold — by `merchantd`, not in the protocol's configuration.

A node runs as three daemons ([tollgate-daemons.md](tollgate-daemons.md)), and each reads its own file:

| File | Daemon | Configures |
|------|--------|------------|
| `tollgate.yaml` | `tollgated` | The protocol: identity, accepted mints, channels, accounting, grants, buying, peers. **Most of this document** |
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

```
tollgated -c /etc/tollgate/fips-exit.yaml --instance fips-exit
```

`--instance` names the instance ([Instance](#instance)). It wins over
`instance` in the file. The init script passes both.

### OpenWrt

On OpenWrt, the primary config path is `/etc/tollgate/tollgate.yaml`. UCI integration is a future consideration — initially TollGate uses YAML directly.

---

## YAML Structure

```yaml
instance:        # This instance's name: logs, service, runtime directory
control_socket:  # Where tolltop and local clients reach this instance
identity:    # Node identity (keypair)
network:     # Where the TollGate protocol listens
enforcer:    # What enforces delivery: loopback, ip, fips or external
mint:        # Where this node's mint (mintd) is, and what it issues
merchant:    # Where merchantd is: upstream funding and foreign proceeds
vouchers:    # Which mints this node takes payment in, and per-peer traffic terms
access:      # Minimum flow allowance
channels:    # Spilman channel parameters
accounting:  # Superseding or accumulative, and the bounds of each
grants:      # Bounds on what a payer may buy in one purchase
buying:      # How this node buys from its peers
peers:       # Static peer overrides
```

---

## Instance

One machine may run several `tollgated` at once, one per task: say one selling
internet access on the LAN, one selling FIPS peering, and one selling a FIPS
exit. Each is an **instance**, and `instance` is its name.

```yaml
instance: "ip"            # unset = "default"
control_socket: ""        # empty = /run/tollgate-<instance>/control.sock
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `instance` | `default` | The instance's name. Letters, digits and `-` only, because it becomes part of a path. The logs show it, and the init script lists instances by it. `--instance` on the command line wins over it |
| `control_socket` | `/run/tollgate-<instance>/control.sock` | The local socket `tolltop` reads, and trusted local clients such as a proxy talk to. Set it only to put the socket somewhere else |

### Instances and Their Sockets

**Each instance has its own runtime directory, `/run/tollgate-<instance>/`.**
The init script creates it before the instance starts, owned by the user the
instance runs as. Only that user can create files in it. Who may then connect
to a socket in it is set by the socket's own file permissions.

The directory holds the instance's sockets, under fixed names:

| Socket | Who listens | Path |
|--------|-------------|------|
| Control | `tollgated` | `/run/tollgate-<instance>/control.sock` |
| Enforcer | The external enforcer, only with `enforcer.kind: external` | `/run/tollgate-<instance>/enforcer.sock` |

So a router running three instances has three directories:

| Instance | Service | Runtime directory |
|----------|---------|-------------------|
| `ip` | `tollgate-ip` | `/run/tollgate-ip/` |
| `fips` | `tollgate-fips` | `/run/tollgate-fips/` |
| `fips-exit` | `tollgate-fips-exit` | `/run/tollgate-fips-exit/` |

The `fips-exit` instance runs as the user `exitd`, the same user as its
enforcer, the exit proxy `exitd`. So `exitd` can create `enforcer.sock` in
`/run/tollgate-fips-exit/`, and no other program can put a socket there in its
place.

**Clients find the sockets from the instance's name.** A proxy working with
the instance `ip` connects to `/run/tollgate-ip/control.sock` without being
told where it is. `tolltop` finds every running instance by listing
`/run/tollgate-*/control.sock`.

**An instance with no name is called `default`,** and keeps the same layout:
its control socket is `/run/tollgate-default/control.sock`. One rule then
covers every node, and `tolltop` finds an unnamed node the same way it finds
the others.

`/run` is where these live on Linux and OpenWrt. The macOS package uses
`/usr/local/var/run` instead, and a node a person runs by hand uses
`$XDG_RUNTIME_DIR`. Only that first part changes. If the instance's directory
is missing, `tollgated` creates it, if it can.

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
| `listen` | `"0.0.0.0:4747"` | Control-plane listen address. The data plane listens on the next port up. Under `enforcer.identity: pubkey` this must be somewhere mesh peers reach — the node's own `fips0` address, or `[::]` — because a connection from anywhere else cannot prove whose key it announces and is refused |

---

## Enforcer

The **enforcer** is what applies `tollgated`'s decisions to real traffic: it lets a paying peer's traffic through, shapes it to the rate it bought, and counts what it carried. Three enforcers are built into `tollgated`. The fourth kind, `external`, is a separate program that `tollgated` drives over a local socket ([tollgate-enforcer-protocol.md](tollgate-enforcer-protocol.md)). For the built-in ones, see [peering-ip.md](../network-peering/peering-ip.md) and [peering-fips.md](../network-peering/peering-fips.md).

```yaml
enforcer:
  kind: loopback              # loopback, ip, fips or external
  interface: "eth0"           # ip only: the interface facing the peers
  fips_socket: ""             # fips only: the FIPS control socket; empty = FIPS's own default
  socket: ""                  # external only: the enforcer's Unix socket; empty = the instance's default
  identity: null              # pubkey or address; null = the kind's default
```

| Parameter | Default | Description |
|-----------|---------|-------------|
| `kind` | `loopback` | `loopback` shapes and meters a socket of its own and forwards nobody's traffic — for demos and tests, and it runs anywhere. `ip` gates and shapes the kernel's forwarding path with nftables and `tc`, which is what sells transit; it needs Linux with `CAP_NET_ADMIN`. `fips` sells transit across a FIPS mesh, leaving enforcement to the FIPS node over its control socket. `external` hands enforcement to a separate program, over the enforcer protocol |
| `interface` | `"eth0"` | Where the peers' `tc` classes live. Only `ip` uses it |
| `fips_socket` | *(FIPS's default path)* | Only `fips` uses it |
| `socket` | `/run/tollgate-<instance>/enforcer.sock` | The Unix socket the external enforcer listens on. Only `external` uses it. Set it only if the enforcer listens somewhere else ([Instances and Their Sockets](#instances-and-their-sockets)) |
| `identity` | *(by kind, below)* | Who a connecting peer is: `pubkey` or `address` |

The default is `loopback` because it runs everywhere and gates nothing it does not own: a node that installed firewall rules because a config line was missing would be a nasty surprise.

### Identity of a Peer

Every peer announces a public key when it connects. `identity` decides what that key is worth, and so who the peer is:

- **`pubkey`**: the peer is its public key, and the network the connection came over proves it — only whoever holds the key could have made that connection. Today only FIPS proves keys. `tollgated` refuses to start with `pubkey` unless its peers arrive over FIPS, and drops any connection that does not come from the FIPS address of the key it announces. `pubkey` never means "the key the peer announced".
- **`address`**: the peer is the address its connection came from. Whoever uses that address is that payer. The key it announces is not checked; it only names the payer's account.

The name `pubkey` is not tied to FIPS on purpose. Another network that proves keys, such as a tunnel keyed by the TollGate key, could use it later.

| `kind` | Default `identity` |
|--------|--------------------|
| `ip` | `address` |
| `fips` | `pubkey` |
| `loopback` | `address` |
| `external` | *(none: it must be written)* |

`external` has no default because `tollgated` cannot tell what the external enforcer matches, or which network its peers arrive over. The enforcer states the identity it was built for when it connects. That is only a check: if it differs from this setting, `tollgated` refuses to start and names both ([tollgate-enforcer-protocol.md](tollgate-enforcer-protocol.md#identity)). It also names the unit it counts in, which must be this node's [`mint.unit`](#mint), checked the same way ([Units](tollgate-enforcer-protocol.md#units)).

This is not the top-level [`identity`](#identity) block, which holds this node's own key.

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

`keep` needs somewhere to keep them: `merchantd`, at `merchant.socket`. If
any accepted mint other than this node's own resolves to `keep` and nothing is
listening there when `tollgated` starts, it **logs a warning** and starts
anyway — `merchantd` may simply start after it. A deposit that finds nobody
there fails, and is retried with the settlement it came from, so what would
have been kept is held back rather than silently destroyed.

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
| `accepted_mints[].settle` | `keep` | `keep` or `burn`, for mints other than this node's own. Ignored for its own, which is always burned. `keep` with no `merchantd` at `merchant.socket` is a startup warning; the deposit is retried with its settlement |
| `received_multiplier` | `0` | No surcharge; each side simply pays for what it received. Per-peer overrides in the `peers` section |

---

## Merchant

Where `tollgated` gets the upstream vouchers it funds channels with, and where
it hands the proceeds of channels settled in mints it **keeps**
([tollgate-daemons.md](tollgate-daemons.md#between-tollgated-and-merchantd)).

```yaml
merchant:
  socket: "/run/merchantd.sock"   # local socket merchantd serves
  prefetch: 0                              # next channel fundings held per paid upstream; 0 = fetch on demand
```

Not implemented yet: `prefetch` — every funding is fetched on demand.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `socket` | `merchantd.sock` in the temp directory | Where `merchantd` listens. Absent `merchantd`, this node cannot fund channels to paid upstreams |
| `prefetch` | `0` | Fetch funding when a channel opens or rolls over. Counted per upstream peer, each sized to that peer's next channel capacity — upstreams differ in mint and capacity, so no single amount fits. Raise it only if measured round trips come near the rollover safety margin. Per-peer override in `peers` |

---

## mintd

`mint.yaml`. A NUT-compliant Cashu mint with no Lightning backend; see
[tollgate-daemons.md](tollgate-daemons.md#mintd) for what it does and does not
implement. It serves the same NUT API on two listeners, which differ only in
how a mint quote gets paid.

```yaml
unit: "byte"
url: "http://192.168.1.1:3338"          # as tollgate.yaml advertises it
seed_file: "/etc/tollgate/mint.seed"    # this mint's own seed; keysets derive from it
file: ""                                # mint.sqlite in the state directory
max_amount: 1099511627776               # largest single quote (1 TiB); at least the largest channel

public:
  listen: "0.0.0.0:3338"                # anyone: swap, melt (burn), state check, …
private:
  listen: "127.0.0.1:3337"              # merchantd only: mint quotes paid on creation

auto_accept: true                       # public mint quotes paid on creation too
issue_quotes_per_minute: 60             # ...but no more quotes than this
control_socket: ""                      # what minttop reads
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `unit` | `"byte"` | Must match `mint.unit` in `tollgate.yaml` |
| `url` | `"http://127.0.0.1:3338"` | Must match `mint.url` in `tollgate.yaml`; the mint names itself by it |
| `seed_file` | `mint.seed` in the state directory, generated on first start | The mint's own secret, separate from the node's identity key. Keysets are derived from it |
| `max_amount` | `1099511627776` | Largest amount one quote may be for (1 TiB). At least the largest channel `tollgated` funds (`channels.max_capacity`), and at least what the market sells at once. Read at every start: it overrides the copy cdk keeps in the database |
| `public.listen` | `"0.0.0.0:3338"` | The mint as peers and wallets see it. Mint quotes here are never paid unless auto-accept is on |
| `private.listen` | `"127.0.0.1:3337"` | Mint quotes here are paid on creation. Whoever reaches it can print this node's vouchers, so it stays on loopback, for `merchantd` |
| `auto_accept` | `true` | Report every NUT-04 mint quote on the public listener paid, so a peer mints what it needs for free |
| `issue_quotes_per_minute` | `60` | Mint quotes created per minute, a minute's worth at once; `0` = unlimited |
| `file` | `mint.sqlite` in the state directory | The mint database: the spent-proof set and the mint quotes issued |
| `control_socket` | `mintd.sock` in the temp directory | Local socket `minttop` reads: listeners, auto-accept, quotes served on each listener |

There is no issuance ceiling, on what `merchantd` sells or on what auto-accept gives away: auto-accept is free or it is off.

While the market is deferred, `auto_accept` is how a peer comes to hold this
node's vouchers: a buyer funds a channel by minting at the seller's mint, with
nothing paid. **Service is then free to any peer that can reach the mint.**
With `auto_accept: false` the public listener serves no mint quotes, so peers
can only pay with vouchers they bought from `merchantd` or came by some other
way.

What is rationed is **requests, not value**. `issue_quotes_per_minute` caps
how many quotes the public listener serves, so its quote endpoint cannot be
used to make the mint sign and store without end; how much a quote is for is
not capped. The limit is node-wide, because the mint cannot tell one asker
from another, and a quote over it is refused when it is asked for. One a
second is far more than a buyer needs, since it asks for one per channel it
opens. Auto-accept exists for testing and for handing free vouchers to third
parties; a node that sells turns it off.

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
socket: "/run/merchantd.sock"           # fund / deposit, for tollgated only
control: "/run/merchantd-control.sock"  # what merchanttop reads and changes
market: true                            # serve the market endpoints at all

mint:
  url: "http://192.168.1.1:3338"        # this node's mint, as buyers reach it
  private: "http://127.0.0.1:3337"      # where what is sold gets issued
  unit: "byte"
  max_amount: 1099511627776             # the most one sale may be for (1 TiB)

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

wallet:
  file: "/etc/tollgate/wallet.sqlite"   # bearer tokens: the file *is* the money
  seed_file: "/etc/tollgate/wallet.seed"   # its own secret, created on first start
  mint: "https://mint.minibits.cash/Bitcoin"   # where it is topped up over Lightning
  unit: "sat"
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

A node can also leave its market to somebody else's `merchantd` rather than
running one, and a market maker can operate without forwarding a byte. The
design assumes a local market: only traffic to this
node's own daemons passes the gate unpaid, so a delegated market is the
operator's to make reachable for peers that cannot yet pay. Letting a named
external market through the gate is a possible later option.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `listen` | `"0.0.0.0:3340"` | Where the market endpoints are served over HTTP. Not 3339, which the speedtest uses |
| `socket` | `merchantd.sock` in the temp directory | Local socket for `fund` and `deposit`. Must match `merchant.socket` in `tollgate.yaml`; only `tollgated` should be able to open it |
| `control` | `merchantd-control.sock` in the temp directory | Local control socket `merchanttop` reads: prices and the wallet, changed at runtime |
| `market` | `true` | Serve the market endpoints. With no price or no `accepts`, nothing is sold anyway |
| `mint.url` | `"http://127.0.0.1:3338"` | This node's mint, as buyers reach it: what their vouchers are issued by |
| `mint.private` | `"http://127.0.0.1:3337"` | `mintd`'s private listener |
| `mint.unit` | `"byte"` | The unit this node's vouchers denominate in |
| `mint.max_amount` | `1099511627776` | The most one sale may be for. The market sells no more than `mintd` will issue in one quote either (its NUT-06 `nut04` `max_amount`), and publishes the lower of the two as `max_amount` in `info`; a buyer paying for more splits the payment into several sales |
| `price.unit` | `"usd"` | `usd`, `eur` or `sat`. Decides whether a rate is needed at all |
| `price.per_mbit` | *(required unless every entry has its own)* | Default price. An entry with neither its own price nor a default is not sold against |
| `accepts[].price` | *(none: `price` applies)* | This issuer's own price, `unit` and `per_mbit` |
| `rates.sources` | the four above | Public, keyless APIs, templated per currency; replace or reorder freely. Unused unless price and payment units differ |
| `rates.refresh_seconds` | `300` | How often the rate is fetched |
| `accepts` | `[]` | Empty means nothing is sold |
| `wallet.file` | `wallet.sqlite` in the state directory | Bearer tokens: the file *is* the balance |
| `wallet.seed_file` | `wallet.seed` in the state directory, created on first start | The wallet's own secret |
| `wallet.mint`, `wallet.unit` | minibits, `sat` | Where the wallet is topped up over Lightning, from `merchanttop` |

Trading one mint's vouchers for another's is not offered: there is no atomic
cross-mint swap to build it on.

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
| `max_capacity` | `17179869184` | Largest channel this node funds, in bytes (16 GiB). The market and mint must issue at least this much in one swap, since a peer paying us funds a channel of up to this much |
| `initial_capacity` | `1073741824` | First channel to a new peer, in bytes (1 GiB) |
| `capacity_growth_factor` | `2.0` | Multiplier applied to a channel this node funds when it is replaced for filling up. A channel replaced because it neared expiry keeps its size. At least `1.0` |
| `ttl_seconds` | `3600` | Lifetime of a channel this node funds (1 hour). As a receiver, this node refuses a channel expiring sooner than half its own TTL |
| `rollover_threshold_pct` | `80` | Trigger rollover at 80% exhaustion |
| `safety_margin_seconds` | `60` | The floor of the safety margin, which is `max(safety_margin_seconds, 2 × max_window_ms)` for a superseding receiver and the floor alone for an accumulative one — see [Safety Margin](tollgate-payment-channels.md#safety-margin) |
| `stale_timeout_seconds` | `60` | Session closed if the peer sends nothing for this long. A node that has sent a peer nothing for a third of it sends that peer the Offer it last sent again, unchanged, as a keepalive (a third of 60 s when this is `0`), so a peer is only silent when it is gone — see Keepalive in [tollgate-protocol.md](tollgate-protocol.md#raw-tcp). Also how long a session that ended without a Disconnect is held, so a peer that reconnects can resume its channels; then its incoming channels are settled, as is any held channel that reaches its settle point first. `0` disables both: silence never closes a session, and a disconnect settles at once |

Capacities must satisfy `0 < min_capacity ≤ initial_capacity ≤ max_capacity`, and `ttl_seconds` must be at least twice the safety margin, so a channel is never born inside its own margin.

`capacity_growth_factor` rewards a peer relationship that has proven stable across rollovers. It applies to the channels this node funds — the peer's revenue channels — since only the funder chooses a channel's size. Every channel is funded by the party that owes, so growth always tracks a paying relationship and has nothing to run away on; `max_capacity` bounds it. Growth follows the channel in use: a peer that reconnects within the grace period resumes its channels, so the next rollover grows from where it was, while one that starts a fresh session starts again from `initial_capacity`.

---

## Accounting

How this node keeps its accounts with the peers that pay it. In both
**accounting modes** a payment is a grant with a window, and whatever is left
at the window's deadline is forfeit
([tollgate-vouchers.md](tollgate-vouchers.md#two-accounting-modes)). The mode
decides two things:

- **`superseding`**, the default. The next payment replaces the one in force,
  and what was left of it is forfeit. The speed is the grant divided by its
  window. This sells speed.
- **`accumulative`**. The next payment adds to what is left, and moves the
  deadline to now plus its window. The speed is `rate_cap`. This sells volume.

```yaml
accounting:
  mode: superseding              # superseding or accumulative
  # The rest applies only in accumulative mode:
  rate_cap: null                 # fastest a peer is carried, units/s; null = no cap
  max_budget: 1073741824         # most a peer may hold unspent, units (1 GiB); null = no limit
  min_topup_gap_ms: 1000         # shortest time between two TopUps from one peer
```

The mode and the three bounds are sent to each peer in the Offer
([tollgate-protocol.md](tollgate-protocol.md#0x01-offer)), beside
`grants.window_range_ms`, so a payer knows before it buys how fast it will be
carried and how much it may hold. In superseding mode the bounds are not sent
and not used.

**An accumulative node accepts much longer windows.** It sets the upper end
of `grants.window_range_ms` to a month or a year — for example
`[60000, 31536000000]` — where a superseding node keeps seconds. Nothing else
is needed: the window is how long a budget lasts, and the bound means the same
in both modes, the longest window this node accepts. Accumulative windows do
not count toward the channel safety margin, so `channels.ttl_seconds` does not
grow with them ([Safety Margin](tollgate-payment-channels.md#safety-margin)).

**Pick `superseding` unless buyers really want volume.** It keeps capacity
perishable on a scale of seconds, which is what stops buyers stockpiling
off-peak and spending at peak
([tollgate-hazards.md](tollgate-hazards.md#unspent-capacity-must-expire)).
Pick `accumulative` for buyers who pay for a quantity and want to use it at
their own pace — a phone on a data pack — and read
[When the Window Can Be Long](tollgate-hazards.md#when-the-window-can-be-long)
first. The two bounds are what make long windows safe enough:

- **`rate_cap`** is the speed every accumulative peer is shaped to while it
  has budget. The window no longer sets a rate, so the operator does. Size it
  against the link: at the busiest hour every peer holding a budget may want
  its cap at once, and the node has already sold the units. `null` leaves the
  link as the only limit.
- **`max_budget`** is the most one peer may hold unspent. It bounds how much a
  peer can save up for the busiest hour, and how much this node owes a peer at
  once. A TopUp that would pass it is refused before any money moves.

**Keep `max_budget` modest.** Frequent small top-ups are better than a few
large ones: a buyer has less paid in advance, so it loses less if this node
fails or disappears, and this node owes less at its busiest hour. A TopUp is
one message, so buying often costs little. A proxy that buys for many users
moves larger volumes on their behalf; give that one peer a larger
`max_budget` in `peers`.

`min_topup_gap_ms` does for accumulative mode what the lower end of
`grants.window_range_ms` does for superseding: it bounds how many signature
checks a peer can impose. One a second is plenty for a buyer adding hundreds
of megabytes at a time.

A budget is kept in this node's state and written to disk beside the channel
backups, so it survives a reconnect, a restart of this node, a channel
rollover and a channel settlement, until its deadline. Past the deadline it is
forfeit. Nothing is refunded: the units were paid for when they were bought.

**Selling time is not a mode.** An operator selling "an hour at 1 MB/s" uses
`superseding`. The buyer holds one rate and renews it every window for as
long as it has vouchers, so the hour is in what it bought, not in the window
([Selling Time](tollgate-vouchers.md#selling-time)).

**Enforcers are unaffected.** In either mode `tollgated` sends an enforcer one
rate per payer, and reads its counters
([tollgate-enforcer-protocol.md](tollgate-enforcer-protocol.md)). Which mode
produced the rate is invisible to it.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `mode` | `superseding` | `superseding` or `accumulative`. Also the only mode this node buys in. Per-peer override in `peers`, for selling |
| `rate_cap` | `null` | Accumulative only. Units per second each peer is shaped to while it has budget; `null` = no cap. Near zero budget the peer is also slowed so it cannot overrun what is left. Per-peer override in `peers` |
| `max_budget` | `1073741824` | Accumulative only. Most units one peer may hold unspent (1 GiB); `null` = no limit. Per-peer override in `peers` |
| `min_topup_gap_ms` | `1000` | Accumulative only. A TopUp sooner than this after the peer's last is refused, to be sent again |

---

## Grants

What this node will accept when a peer buys capacity. A grant is a quantity of units paired with a window to spend it in. In superseding mode the rate it buys is one divided by the other ([tollgate-vouchers.md](tollgate-vouchers.md)); in accumulative mode the window is how long the budget lasts. `max_rate` counts only the peers buying in superseding mode.

```yaml
grants:
  window_range_ms: [200, 30000]      # payer picks any window in this range, per grant
  max_rate: null                     # units/second this node will commit across all buyers; null = link capacity
```

`window_range_ms` is advertised in the Offer, and the two ends do different jobs:

- **Upper bound** is the longest window this node accepts. In superseding mode it caps how far ahead capacity can be bought, which is what stops a buyer accumulating off-peak claims and presenting them at peak. Long windows also mean a large forfeit when a payer raises its rate early, so a high ceiling is not a favor to the payer. Selling a longer interval, such as an hour, does not need a longer window: the buyer renews inside it ([Selling Time](tollgate-vouchers.md#selling-time)). Raising it is allowed, but the safety margin is twice the longest superseding window and `channels.ttl_seconds` must be at least twice the margin, so a one-hour window needs channels of four hours or more. In accumulative mode it is set to a month or a year, and the channel rule does not apply ([Accounting](#accounting)).
- **Lower bound** caps how many grants can arrive per second, and therefore how many signature verifications a peer can impose. On an ESP32 that is the binding constraint, not bandwidth. Raise it on constrained hardware.

There is no minimum grant size. A short window already bounds message rate, and a small grant is cheap to serve.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `window_range_ms` | `[200, 30000]` | Payer chooses per grant, in milliseconds. Five verifications per second worst case, and no claim held longer than 30 s. An accumulative node raises the upper end to a month or a year. Per-peer override in `peers` |
| `max_rate` | `null` | Total rate this node will commit across all superseding buyers at once. A TopUp that would exceed it is refused with the rate still available attached. Accumulative peers are not counted; leave room for their `rate_cap` by hand |

There is no transit-loss tolerance. Counters are not exchanged, so there is no second number to disagree with — see [tollgate-metering.md](tollgate-metering.md).

---

## Buying

How this node buys from the peers it pays. It buys only in its own
`accounting.mode`, from the peers whose Offer is in that mode, and skips the
others with a line in its log
([How a Buyer Buys](tollgate-vouchers.md#how-a-buyer-buys)).

```yaml
buying:
  demand: 0                       # units/s to want from every peer regardless; 0 = only what is observed
  renew_lead_ms: 1200             # renew this long before the deadline
  # Superseding:
  headroom_pct: 125               # buy this share of observed demand
  window_ms: 4000                 # window to ask for, clamped to the peer's range
  raise_threshold_pct: 150        # buy early only if demand rose this much
  cap_hold_ms: 10000              # respect a rate a peer named for this long
  min_rate: 0                     # never buy below this rate
  # max_rate:                    # never buy above this rate; unset = no ceiling
  # Accumulative:
  low_water: 67108864             # top up when the budget falls below this (64 MiB)
  top_up: 268435456               # by this much (256 MiB)
  budget_window_ms: 2592000000    # window to ask for (30 days), clamped to the peer's range
```

**Superseding.** The buyer buys a rate for a window and renews it
`renew_lead_ms` before the deadline. It only buys again earlier when demand
has risen past `raise_threshold_pct`, because buying early forfeits what is
left of the grant in force. `min_rate` and `max_rate` bound the rate it buys;
set them equal to hold one rate whatever the demand, which is how a buyer
buys time ([Selling Time](tollgate-vouchers.md#selling-time)).

**Accumulative.** Buying early forfeits nothing. The buyer tops up by
`top_up`, with a window of `budget_window_ms`, whenever its remaining budget
falls below `low_water`, and `renew_lead_ms` before the deadline, in both cases
only while something wants the link (observed demand, or `demand`). It never
asks for more than the peer's `max_budget` leaves room for. It counts the
budget itself, from what it signed and what it measured, and takes the
provider's Balance message as the truth whenever one arrives.

Set `low_water` to cover what the link moves while a top-up is on its way: a
few seconds at the peer's rate cap is plenty. `top_up` trades message count
against money paid ahead, which the peer holds until it is used; keep it
small next to the peer's `max_budget`.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `demand` | `0` | Units per second to want from every peer whether or not anything asks. A standing order that spends money. `0` buys only for observed demand |
| `headroom_pct` | `125` | Superseding. Buy this percentage of observed demand, so a rising flow is not shaped before the next purchase lands |
| `window_ms` | `4000` | Superseding. Window to ask for, clamped to the peer's `window_range_ms` |
| `renew_lead_ms` | `1200` | Renew this long before the deadline. Below 1000 a renewal under load lands late and the flow stalls |
| `raise_threshold_pct` | `150` | Superseding. Buy before the deadline only if the wanted rate exceeds the one in force by this much |
| `cap_hold_ms` | `10000` | Superseding. How long to respect a rate a peer named in a TopUpReject before trying higher again |
| `min_rate`, `max_rate` | `0`, unset (no ceiling) | Superseding. Bounds on the rate bought. Equal values pin it |
| `low_water` | `67108864` | Accumulative. Top up when the remaining budget falls below this (64 MiB) |
| `top_up` | `268435456` | Accumulative. Units added per top-up (256 MiB), clamped to what the peer's `max_budget` allows |
| `budget_window_ms` | `2592000000` | Accumulative. Window asked for with each top-up (30 days), clamped to the peer's `window_range_ms` |

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

  # Sell this peer volume rather than speed: a proxy buying for its users
  "06pqr...":
    accounting:
      mode: accumulative
      rate_cap: 2500000          # 20 Mbit/s
      max_budget: 10737418240    # 10 GiB
    grants:
      window_range_ms: [60000, 2592000000]   # up to 30 days
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `no_charge` | `false` | Do not charge this peer, and tell it so in the Offer, so it funds no channel toward this node. One-sided — whether the peer charges back is its own decision |
| `received_multiplier` | *(from `vouchers.received_multiplier`)* | Unsigned surcharge on what this peer pushes at us, on top of it being paid for delivering it |
| `blocked` | `false` | Refuse all service to this peer |
| `prefetch` | *(from `merchant.prefetch`)* | Channel fundings held ahead for this upstream |
| `accounting.mode`, `accounting.rate_cap`, `accounting.max_budget` | *(from the `accounting` block)* | The accounting mode this node sells to this peer in, and its accumulative bounds. Any not given come from the node-wide block. What this node buys from the peer still follows the node-wide `accounting.mode` |
| `grants.window_range_ms` | *(from `grants.window_range_ms`)* | The windows this peer may ask for. A peer sold to in accumulative mode on a superseding node needs a longer upper end |
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
  socket: "/run/merchantd.sock"

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

accounting:
  mode: superseding

grants:
  window_range_ms: [200, 30000]

peers:
  "02abc...":
    no_charge: true
  "06pqr...":
    accounting:
      mode: accumulative
      rate_cap: 2500000
    grants:
      window_range_ms: [60000, 2592000000]
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting `tollgated`:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Received multiplier | Yes | Sent as a revised Offer; takes effect on the peer's next grant, never on one already bought |
| Minimum flow allowance | Yes | Applies immediately — it is a shaping rate, not a budget |
| Grant window range | Yes | Sent as a revised Offer; applies to the next grant |
| Accounting mode and its bounds | New sessions only | The mode is fixed for a session. A peer moved from accumulative to superseding keeps its budget on disk until its deadline, and gets it back if it is moved back in time |
| Max rate | Yes | Lowering it does not revoke a grant already sold; it refuses the next one |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Prefetch | Yes | Applies from the next funding |
| Accepted mints | No | A channel is funded in a specific mint, so dropping one would strand it. New sessions only |
| Channel parameters | No | Applies to new channels only |
| Own mint URL and unit | No | Requires restart; changing them invalidates outstanding vouchers |
| Identity | No | Requires restart |
| Instance name, control socket, enforcer socket | No | Requires restart |

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
| Instances | One `tollgated` for every task, run once per task; each instance is named by `instance` or `--instance` | The name ties together what belongs to one instance: its service, its logs and its runtime directory |
| Runtime directory | One per instance, `/run/tollgate-<instance>/`, created by the init script and owned by the instance's user. The control and enforcer sockets sit in it under fixed names, and their keys are set only to deviate | Ownership: only the instance's user can create a socket there, so no other program can take the place of the enforcer or of `tollgated`. Discovery: a client finds an instance from its name alone, and `tolltop` finds them all with one listing. One place per instance: two instances never share a path, and everything of one instance is in one directory |
| Unset instance | Named `default`, with the same layout | One rule for every node. Keeping the old plain paths for an unnamed node would give clients a second place to look, and `tolltop`'s listing would miss it |
| Mint block | Optional. Where `mintd` is, and its unit; no privilege there | Issuing sits in its own daemon. A node that issues nothing — a pass-through relay, a buying leaf — needs no `mintd` at all |
| Merchant block | A socket to `merchantd`, and `prefetch` counted in fundings per upstream | `tollgated` holds nothing of value, so it asks for upstream vouchers when it needs them; upstreams differ in mint and capacity, so no single amount fits |
| Grants block | Replaces the metering block | There is no interval to negotiate and no drift tolerance to set. What is left is a bound on what a payer may buy in one purchase |
| Window range | Advertised in the Offer; payer picks per grant | Upper end bounds buying off-peak for peak, lower end bounds signature verifications per second |
| Accounting mode | `accounting.mode`, `superseding` by default, overridable per peer for selling; fixed for a session | Speed and volume are different products, and the operator knows which its buyers want. Superseding is the default because its capacity expires in seconds and so cannot be stockpiled |
| Accumulative windows | No separate setting: the upper end of `grants.window_range_ms` is set to a month or a year; exempt from the channel-lifetime rule | The window means the same in both modes, the longest one accepted. The budget lives in node state rather than in a channel, so a long window does not need a long channel |
| Accumulative bounds | A per-peer `rate_cap` and `max_budget`, with defaults of no cap and 1 GiB | The window no longer limits how fast a peer can draw, and a long window lets it hold a lot, so the operator states both. A modest default keeps buyers topping up small and often: less paid ahead is less lost if a provider fails, and less owed at peak |
| Buying mode | A node buys only in its own `accounting.mode` and skips providers in the other | A relay cannot hedge buying one mode and selling the other, and one buyer algorithm per node is simpler. Bridging the two is possible with two nodes and a balancer, or a custom implementation |
| Selling time | Not a mode: superseding with a buyer whose `min_rate` equals its `max_rate` | A fixed speed for a fixed time is a rate renewed every window; the length of time is how many vouchers the buyer holds |
| Minimum flow allowance | A rate, not a per-interval quantity | It is the floor of the shaper and what a peer falls back to when its grant expires. A rate cannot be accumulated |
| Transit-loss settings | Removed | Counters are no longer exchanged, so there is no second number to disagree with |
| Accepted mints | One ordered list, at least one entry, no prices | Accept or refuse is binary; what an issuer's paper is worth belongs on the market |
| Received multiplier | Unsigned, per peer | Prices scarce uplink and signals how welcome a peer's traffic is, without any signed number in the protocol |
| Market services | Own daemon (`merchantd`), own file, own endpoints, own protocol, disabled by default | Buying and swapping is not part of paying for delivery. `path` may point at a third party, so a node can offer swaps without running a market |
| One file per executable | `tollgate.yaml`, `mint.yaml`, `merchant.yaml` | Each daemon can be restarted, replaced or run by someone else without touching the others' settings. Prices never reach the protocol daemon |
| Other mints after settlement | Per-mint `settle: keep \| burn` in `tollgate.yaml`; `keep` without `merchantd` warns at startup and retries the deposit | Whether another issuer's paper is worth anything depends on the relationship; value is never destroyed silently |
| Quote step | None: swap at the price in force | A moved rate costs a retry. Quotes can be added later |
| Selling price | Per Mbit, in `usd`, `eur` or `sat`; a default `price`, overridable per `accepts` entry | One quantity to reason about, in the operator's unit, and priced per issuer because that is how issuer risk is priced. A rate is fetched only when price and payment units differ; zero answers are skipped, and with every source down the last good rate is used |
| mintd privilege | A second listener serving the same NUT API, where mint quotes are paid on creation | No custom endpoints: the privilege is the address, not the call |
| Per-peer favoritism | Sell that peer vouchers cheaper, outside the protocol | Same capability, no multiplier machinery |
| Free peering | Per-peer `no_charge` flag, one-sided, not transitive | It is a decision about a relationship rather than a price; transitivity would launder free transit for others |
| Capacity growth | Applies to every channel | Every channel is funded by the party that owes, so growth always tracks a paying relationship |
