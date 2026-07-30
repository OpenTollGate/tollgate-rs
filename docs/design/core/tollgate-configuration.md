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
vouchers:    # Which mints this node takes payment in, and per-peer traffic terms
market:      # Optional: buying and swapping services (separate protocol)
access:      # Minimum flow allowance
channels:    # Spilman channel parameters
metering:    # Metering interval and drift tolerance
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

The unit is fixed by the resource and must match across every node selling it. There is no per-direction unit: what a peer pays to have its outgoing traffic carried is the received multiplier, not a second keyset ([tollgate-vouchers.md](tollgate-vouchers.md)).

---

## Vouchers

Which mints this node will take payment in, and how welcome each peer's
outgoing traffic is.

```yaml
vouchers:
  preferred_mint: "https://gateway.example.com/mint"   # pay me in this
  also_accept:                                         # optional
    - "https://upstream.example.com/mint"
    - "https://hub.example.com/mint"

  received_multiplier: 0        # surcharge default; 0 = none
```

There are **no prices here.** Delivery is one voucher per unit, and a mint is
either accepted or it is not — what an issuer's paper is worth is expressed in
what you pay for it on the market, not in a discount applied at settlement
([tollgate-vouchers.md](tollgate-vouchers.md)).

`preferred_mint` need not be this node's own. A pure pass-through relay can
name its upstream's mint, take payment in vouchers it can spend directly, and
never issue any of its own.

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
| `preferred_mint` | *(required)* | Mint this node wants to be paid in |
| `also_accept` | `[]` | Other mints accepted; each one taken on is that issuer's credit risk |
| `received_multiplier` | `0` | No surcharge; each side simply pays for what it received. Per-peer overrides in the `peers` section |

---

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

A small amount of traffic every peer gets free, so a new peer can acquire vouchers before it can pay for anything, and so basic things work regardless. It is given away, so it can be farmed — see Minimum Flow Allowance in [tollgate-vouchers.md](tollgate-vouchers.md) for what bounds it and what does not.

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `minimum_flow.enabled` | `false` | Off by default; it is traffic given away |
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

`capacity_growth_factor` rewards a peer relationship that has proven stable across rollovers. Every channel is funded by the party that owes, so growth always tracks a paying relationship and has nothing to run away on.

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

  # Static peer endpoint (IP peering only)
  "05jkl...":
    endpoint: "192.168.1.1:4747"
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `no_charge` | `false` | Do not charge this peer. One-sided — whether the peer charges back is its own decision |
| `received_multiplier` | *(from `vouchers.received_multiplier`)* | Unsigned surcharge on what this peer pushes at us, on top of it being paid for delivering it |
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
  preferred_mint: "https://upstream.example.com/mint"   # spend these onward
  received_multiplier: 2                                # uploads cost the same as downloads

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
| Accepted mints | One preferred mint plus an optional accepted set, no prices | Accept or refuse is binary; what an issuer's paper is worth belongs on the market |
| Received multiplier | Unsigned, per peer | Prices scarce uplink and signals how welcome a peer's traffic is, without any signed number in the protocol |
| Market services | Own config section, own endpoints, own protocol, disabled by default | Buying and swapping is not part of paying for delivery. `market.path` may point at a third party, so a node can offer swaps without running a market |
| Per-peer favoritism | Sell that peer vouchers cheaper, outside the protocol | Same capability, no multiplier machinery |
| Free peering | Per-peer `no_charge` flag, one-sided, not transitive | It is a decision about a relationship rather than a price; transitivity would launder free transit for others |
| Negative delivery prices | Absolute caps, per peer and aggregate, conserved resources only | No counterparty bounds outbound spend; per-peer caps alone are defeated by free identities |
| Capacity growth | Applies to every channel | Every channel is funded by the party that owes, so growth always tracks a paying relationship |
