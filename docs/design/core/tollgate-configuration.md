# TollGate Configuration

This document specifies the configuration schema for TollGate — the YAML format, parameter hierarchy, defaults, and platform-specific paths.

## Overview

TollGate uses YAML-based configuration following the same pattern as FIPS. Every parameter has a sensible default — a minimal config only specifies what differs.

Delivery has no price to configure: one voucher buys one unit ([tollgate-vouchers.md](tollgate-vouchers.md)). What a unit costs in money is decided where vouchers are sold, which is not the node's protocol configuration.

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
mint:        # This node's own mint — what it issues vouchers against
vouchers:    # Which mints' vouchers this node takes as payment
market:      # Optional: buying and swapping services (separate protocol)
access:      # Minimum flow allowance
channels:    # Spilman channel parameters
metering:    # Metering interval and drift tolerance
subsidy:     # Negative delivery prices (conserved resources only)
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

## Mint

Every node runs its own mint and issues vouchers against its own capacity ([tollgate-vouchers.md](tollgate-vouchers.md)).

```yaml
mint:
  url: "https://gateway.example.com/mint"   # advertised in Offer
  unit: "byte"                              # quantity unit for this resource
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `url` | *(required)* | Mint URL advertised to peers |
| `unit` | `"byte"` | Quantity unit — `byte`, `wh`, `ml` |

The unit is fixed by the resource and must match across every node selling it. There is no per-direction unit: what a peer pays to have its own outgoing traffic carried is the acceptance price on its vouchers, not a second keyset ([tollgate-vouchers.md](tollgate-vouchers.md)).

---

## Vouchers

Which mints' vouchers this node takes as payment, and at what settlement ratio. These are the only prices in the protocol — see Accepted Mints in [tollgate-vouchers.md](tollgate-vouchers.md). Selling or swapping vouchers is configured separately; see [market-protocol.md](../market/market-protocol.md).

```yaml
vouchers:
  price_scale: 1000            # divisor for the prices below

  accept:                      # mints whose vouchers this node takes
    - url: "https://upstream.example.com/mint"
      price: 1000              # par — this is our upstream, we can spend these
    - url: "https://neighbor.example.com/mint"
      price: 950               # 5% haircut
    - url: "https://hub.example.com/mint"
      price: 1000              # widely held
```

This node's own mint is accepted at par by definition and does not appear in
`accept`. An empty `accept` list means own vouchers only, which is the
default and the conservative choice — see Accepted Mints in
[tollgate-vouchers.md](tollgate-vouchers.md) for what accepting a foreign
mint costs.

Each `price` is signed and crosses zero:

| Value | Meaning |
|---|---|
| `> price_scale` | Premium — we want these more than face value |
| `= price_scale` | Par |
| `0 < price < scale` | Haircut — we take them at a discount |
| `0` | Even swap |
| `< 0` | The holder pays us to take them — paid acceptance |
| absent | Refused |

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `price_scale` | `1000` | Sub-unit precision divisor |
| `accept` | `[]` | Own vouchers only; a foreign mint costs local verification and free settlement |

---

## Market

**Independent of everything above.** These settings configure the market
endpoints, which are a separate protocol served under a separate path — see
[market-protocol.md](../market/market-protocol.md). Disabling the whole
section changes nothing about paying for delivery.

```yaml
market:
  enabled: false                        # serve the market endpoints at all
  path: "/tollgate/market/v1"           # local prefix, or an external URL to
                                        #   delegate to a third-party market

  sat_swap: false                       # trade vouchers against sat tokens
  cross_mint_swap: false                # trade one mint's vouchers for another's
  quote_ttl_seconds: 30                 # how long a quote stays honorable

  sells:                                # mints whose vouchers we will sell
    - "https://gateway.example.com/mint"      # our own
  buys:                                 # mints whose vouchers we will buy
    - "https://neighbor.example.com/mint"
```

Selling this node's **own** vouchers for sats needs none of this — that is a
plain Cashu mint operation (NUT-04) against `mint.url`. The endpoints here
exist for what Cashu has no answer to: quoting and exchanging vouchers across
mints.

`path` may be an external URL, in which case this node advertises somebody
else's market rather than running one. A node can offer swaps without being a
market maker, and a market maker can operate without forwarding a byte.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `enabled` | `false` | Off by default; a node without a market is fully functional |
| `path` | `"/tollgate/market/v1"` | Local prefix, or an external URL to delegate to |
| `sat_swap` | `false` | Trade vouchers against sat-denominated tokens |
| `cross_mint_swap` | `false` | Trade one mint's vouchers for another's — no atomic implementation exists yet |
| `quote_ttl_seconds` | `30` | Quote validity window |
| `sells` | `[]` | Empty means own mint only, via plain Cashu |
| `buys` | `[]` | Empty means we buy nothing |

---

## Access

```yaml
access:
  minimum_flow:
    enabled: false
    bytes_per_interval: 0
```

A small allowance every peer gets without paying, so a new peer can acquire vouchers before it can pay for anything. It is a subsidy and can be farmed — see Minimum Flow Allowance in [tollgate-vouchers.md](tollgate-vouchers.md) for what bounds it and what does not.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `minimum_flow.enabled` | `false` | Off by default; it is a subsidy |
| `minimum_flow.bytes_per_interval` | `0` | Keep small — resale value is bounded economically, not cryptographically |

---

## Channel Parameters

```yaml
channels:
  min_capacity: 10                         # minimum Spilman channel capacity (vouchers)
  max_capacity: 10000                      # maximum channel capacity
  initial_capacity: 10                     # starting capacity for new peers
  capacity_growth_factor: 2.0             # multiply capacity after each successful rollover
  ttl_seconds: 3600                        # channel expiry (default: 1 hour)
  rollover_threshold: 0.80                 # rollover at 80% capacity used
  safety_margin_seconds: 60               # begin emergency rollover this long before expiry
  stale_timeout_seconds: 60               # close session if rollover can't complete within this time
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `min_capacity` | `10` | Minimum channel funding |
| `max_capacity` | `10000` | Maximum channel funding |
| `initial_capacity` | `10` | First channel capacity for new peers |
| `capacity_growth_factor` | `2.0` | Capacity multiplier per successful rollover — **incoming (revenue) channels only** |
| `ttl_seconds` | `3600` | Channel lifetime (1 hour) |
| `rollover_threshold` | `0.80` | Trigger rollover at 80% exhaustion |
| `safety_margin_seconds` | `60` | Emergency rollover window before expiry |
| `stale_timeout_seconds` | `60` | Session closed if rollover blocked this long |

`capacity_growth_factor` rewards a peer relationship that has proven stable across rollovers. That is the right incentive on a channel the *peer* funds. On a channel this node funds because it owes the peer, the same rule rewards whichever peer drains it fastest, and combined with automatic rollover it drains the wallet unattended, limited only by the balance:

```
10 → 20 → 40 → 80 → … → max_capacity, then refilled indefinitely
```

Growth is therefore not applied to channels funded under a negative price, and their rollover is bounded by `subsidy` below.

---

## Metering

```yaml
metering:
  interval_range: [3000, 10000]           # acceptable metering interval range [min_ms, max_ms]
  default_interval_ms: 5000               # preferred interval (used if peer accepts)
  transit_loss_tolerance: 0.05                   # 5% transit loss tolerance
  transit_loss_max_consecutive: 3                # close after this many consecutive over-tolerance intervals (transit loss)
  transit_loss_unacceptable: 0.50               # immediately close if transit loss exceeds this (50%)
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `interval_range` | `[3000, 10000]` | Acceptable interval in ms |
| `default_interval_ms` | `5000` | Preferred metering interval |
| `transit_loss_tolerance` | `0.05` | 5% transit loss tolerance |
| `transit_loss_max_consecutive` | `3` | Close after 3 consecutive over-tolerance intervals (transit loss) |
| `transit_loss_unacceptable` | `0.50` | Immediately close if transit loss exceeds 50% |

---

## Subsidy Limits

Negative *delivery* prices — paying a peer to take a resource off your hands — survive only for surplus disposal on a conserved, physically metered resource ([tollgate-hazards.md](tollgate-hazards.md)). They do not apply to network forwarding.

Where they are used, money leaves the node and no counterparty's willingness to pay bounds the total, so the bound has to be configured.

```yaml
subsidy:
  enabled: false                     # negative delivery prices refused unless true
  max_per_peer_per_hour: 0           # absolute cap per peer
  max_total_per_hour: 0              # aggregate cap across all peers
  require_conserved_resource: true   # only where delivery is physically metered
  on_budget_exhausted: "close"       # close | stop_paying
```

Both caps are enforced. A per-peer cap alone is defeated by creating more peer identities, which are free; an aggregate cap alone lets one peer consume the whole budget.

`require_conserved_resource` restricts negative delivery prices to resources the ResourceAdapter declares physically metered and conserved. For resources that can be silently discarded — network bytes — a peer can accept, bill, and drop, and no meter can tell the difference.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `subsidy.enabled` | `false` | Refused unless explicitly enabled |
| `max_per_peer_per_hour` | `0` | Absolute per-peer cap |
| `max_total_per_hour` | `0` | Absolute aggregate cap |
| `require_conserved_resource` | `true` | Restrict to physically metered resources |
| `on_budget_exhausted` | `"close"` | Close the session rather than silently continue |

---

## Peer Overrides

```yaml
peers:
  # Do not charge this peer (operator's own nodes, friends)
  "02abc...":
    no_charge: true

  # Override the price for this peer's own mint
  "03def...":
    voucher_price: -50           # scaled; peer pays us to hold its vouchers

  # Block a peer entirely
  "04ghi...":
    blocked: true

  # Static peer endpoint (IP peering only)
  "05jkl...":
    endpoint: "192.168.1.1:4747"
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `no_charge` | `false` | Do not charge this peer. One-sided — whether the peer charges back is its own decision |
| `voucher_price` | *(from `vouchers.accept`, else refused)* | Signed price for this peer's own mint, overriding the global entry |
| `blocked` | `false` | Refuse all service to this peer |
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

vouchers:
  price_scale: 1000
  accept:
    - url: "https://upstream.example.com/mint"
      price: 1000               # our upstream — we spend these onward

access:
  minimum_flow:
    enabled: true
    bytes_per_interval: 4096

channels:
  initial_capacity: 10
  max_capacity: 5000
  ttl_seconds: 3600
  rollover_threshold: 0.80

metering:
  interval_range: [3000, 10000]
  default_interval_ms: 5000
  transit_loss_tolerance: 0.05

peers:
  "02abc...":
    no_charge: true
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting the node:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Accepted mints and their prices | Yes | Changes take effect at the next metering interval |
| Minimum flow allowance | Yes | Applies to the next interval |
| Subsidy limits | Yes | Lowering a cap applies immediately; already-spent budget is not refunded |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Channel parameters | No | Applies to new channels only |
| Metering interval | No | Applies to new sessions only |
| Own mint URL and unit | No | Requires restart; changing them invalidates outstanding vouchers |
| Identity | No | Requires restart |

The implementation watches the config file for changes and applies runtime-changeable parameters without interrupting active sessions.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Format | YAML | Follows FIPS pattern, human-readable, supports comments |
| Loading | Cascading multi-file with priority | System defaults + user overrides + deployment specifics |
| Defaults | Every parameter has a sensible default | Minimal config for simple deployments |
| Mint block | Added — every node issues its own vouchers | The node is the mint for its own capacity |
| Accepted mints | A set per node, own mint implicitly at par, empty by default | One unit of account network-wide makes any mint's vouchers usable; accepting an upstream's mint lets a relay spend what it receives without converting |
| Market services | Own config section, own endpoints, own protocol, disabled by default | Buying and swapping is not part of paying for delivery. `market.path` may point at a third party, so a node can offer swaps without running a market |
| Per-peer favoritism | Sell that peer vouchers cheaper, outside the protocol | Same capability, no multiplier machinery |
| Free peering | Per-peer `no_charge` flag, one-sided, not transitive | It is a decision about a relationship rather than a price; transitivity would launder free transit for others |
| Negative delivery prices | Absolute caps, per peer and aggregate, conserved resources only | No counterparty bounds outbound spend; per-peer caps alone are defeated by free identities |
| Capacity growth | Revenue channels only | On a subsidy channel it rewards the fastest drain |
