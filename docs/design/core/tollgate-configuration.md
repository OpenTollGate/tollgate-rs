# TollGate Configuration

This document specifies the configuration schema for TollGate — the YAML format, parameter hierarchy, defaults, and platform-specific paths.

## Overview

TollGate uses YAML-based configuration following the same pattern as FIPS. Every parameter has a sensible default — a minimal config only specifies what differs.

The config is considerably smaller than it used to be. Delivery has no price to configure: one voucher buys one unit, so products, pricing scales, floors, ceilings, per-peer multipliers and dynamic pricing formulas are all gone ([tollgate-pricing.md](tollgate-pricing.md)). What a unit costs in money is decided where vouchers are sold, which is not the node's protocol configuration.

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
vouchers:    # How this node treats other nodes' vouchers
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
  classes: ["up", "down"]                   # one keyset per direction class
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `url` | *(required)* | Mint URL advertised to peers |
| `unit` | `"byte"` | Quantity unit — `byte`, `wh`, `ml` |
| `classes` | `["up", "down"]` | Direction classes; a single-class resource lists one |

The unit is fixed by the resource and must match across every node selling it. Classes are defined by the ResourceAdapter; the core neither enumerates nor interprets them.

---

## Vouchers

How this node treats vouchers issued by *other* nodes. This is the only price in the protocol — see Paid Acceptance in [tollgate-vouchers.md](tollgate-vouchers.md).

```yaml
vouchers:
  accept_foreign: false        # refuse other nodes' vouchers by default
  price_scale: 1000            # divisor for the prices below
  default_price: 0             # scaled; applies to peers with no override
```

`default_price` is signed and crosses zero:

| Value | Meaning |
|---|---|
| `> 0` | We buy the peer's vouchers — we want what they deliver |
| `0` | Even swap |
| `< 0` | The peer pays us to hold them — paid acceptance |
| `accept_foreign: false` | Refused; the peering runs one-way |

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `accept_foreign` | `false` | Refusing is the safe default; otherwise a node accumulates vouchers it cannot redeem |
| `price_scale` | `1000` | Sub-unit precision divisor |
| `default_price` | `0` | Even swap when acceptance is enabled at all |

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

Negative *delivery* prices — paying a peer to take a resource off your hands — survive only for surplus disposal on a conserved, physically metered resource ([tollgate-pricing.md](tollgate-pricing.md)). They do not apply to network forwarding.

Where they are used, money leaves the node and no counterparty's willingness to pay bounds the total, so the bound has to be configured.

```yaml
subsidy:
  enabled: false                     # negative delivery prices refused unless true
  max_per_peer_per_hour: 0           # absolute cap per peer
  max_total_per_hour: 0              # aggregate cap across all peers
  require_conserved_resource: true   # only where delivery is physically metered
  on_budget_exhausted: "close"       # close | zero_price
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
  # Zero-price peering (operator's own nodes, friends)
  "02abc...":
    zero_price: true

  # Accept this peer's vouchers at a specific price
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
| `zero_price` | `false` | Skip payment entirely with this peer |
| `voucher_price` | *(from `vouchers.default_price`)* | Signed price for this peer's vouchers |
| `blocked` | `false` | Refuse all service to this peer |
| `endpoint` | *(none)* | Static endpoint for IP peering |

There is no `price_multiplier`. Favoring a peer means selling it vouchers more cheaply, which happens outside the protocol. `zero_price` is **not transitive** — it means free for that peer's own traffic, never free for anything that peer is nominally the beneficiary of.

---

## Full Example

```yaml
identity:
  secret_key_file: "/etc/tollgate/identity.key"

mint:
  url: "https://gateway.example.com/mint"
  unit: "byte"
  classes: ["up", "down"]

vouchers:
  accept_foreign: true
  price_scale: 1000
  default_price: -20            # we charge peers to hold their vouchers

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
    zero_price: true
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting the node:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Voucher prices | Yes | New price takes effect at next metering interval |
| Minimum flow allowance | Yes | Applies to the next interval |
| Subsidy limits | Yes | Lowering a cap applies immediately; already-spent budget is not refunded |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Channel parameters | No | Applies to new channels only |
| Metering interval | No | Applies to new sessions only |
| Mint URL, unit, classes | No | Requires restart; changing them invalidates outstanding vouchers |
| Identity | No | Requires restart |

The implementation watches the config file for changes and applies runtime-changeable parameters without interrupting active sessions.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Format | YAML | Follows FIPS pattern, human-readable, supports comments |
| Loading | Cascading multi-file with priority | System defaults + user overrides + deployment specifics |
| Defaults | Every parameter has a sensible default | Minimal config for simple deployments |
| Products, pricing scales, floors, ceilings, multipliers, dynamic formulas | Removed | Delivery is one voucher per unit; money prices are set where vouchers are sold |
| Bootstrap block | Removed | The mechanism is gone — see [voucher-acquisition.md](../market/voucher-acquisition.md) |
| Mint block | Added — every node issues its own vouchers | The node is the mint for its own capacity |
| Foreign vouchers | Refused by default | Otherwise a node accumulates vouchers it cannot redeem |
| Per-peer favoritism | Sell that peer vouchers cheaper, outside the protocol | Same capability, no multiplier machinery |
| Zero-price peering | Per-peer flag, not transitive | Transitivity would launder free transit for others |
| Negative delivery prices | Absolute caps, per peer and aggregate, conserved resources only | No counterparty bounds outbound spend; per-peer caps alone are defeated by free identities |
| Capacity growth | Revenue channels only | On a subsidy channel it rewards the fastest drain |
