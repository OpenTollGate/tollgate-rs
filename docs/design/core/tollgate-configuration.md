# TollGate Configuration

This document specifies the configuration schema for TollGate — the YAML format, parameter hierarchy, defaults, and platform-specific paths.

## Overview

TollGate uses YAML-based configuration following the same pattern as FIPS. Every parameter has a sensible default — a minimal config only specifies what differs. Configuration is organized into logical sections covering products, pricing, channels, peers, and operator identity.

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
products:    # What this node sells
pricing:     # Dynamic pricing rules
channels:    # Spilman channel parameters
metering:    # Metering interval and drift tolerance
bootstrap:   # Bootstrap token parameters
mints:       # Accepted mints
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

## Products

Each product defines what the node sells and at what price. Multiple products can be offered — the peer chooses one.

```yaml
products:
  - name: "standard"                       # human-readable name (not sent over protocol)
    pricing_scale: 1000                    # sub-unit precision divisor
    pricing:
      - mint_url: "https://mint.example.com"
        price_per_second: 0                # scaled integer
        price_per_unit: 10                 # scaled integer (0.01 sat/unit with scale=1000)
        mint_unit: "sat"

      - mint_url: "https://mint.eu"
        price_per_second: 0
        price_per_unit: 8                  # discount for preferred mint
        mint_unit: "sat"

    # Implementation-specific fields (opaque to core, included in product_id hash)
    extensions:
      bandwidth_limit: 0                   # network: bytes/sec, 0 = unlimited

  - name: "always-on"
    pricing_scale: 1000
    pricing:
      - mint_url: "https://mint.example.com"
        price_per_second: 100              # 0.1 sat/sec
        price_per_unit: 0
        mint_unit: "sat"
    extensions:
      bandwidth_limit: 10000              # network: 10 KB/s cap
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `pricing_scale` | `1000` | Sub-unit precision divisor |
| `extensions` | `{}` | Implementation-specific product fields (opaque to core) |

If no products are defined, the node operates as a **consumer only** — it pays peers but does not sell. A node with no products and no funds is effectively passive (zero-price peering with any peer that allows it).

---

## Dynamic Pricing

```yaml
pricing:
  enabled: false                           # enable dynamic price adjustments

  # Formula expression — core evaluates against opaque metrics from the implementation.
  # Core doesn't know what the metric keys mean; it just plugs values into the formula.
  # Available: base (base price), metric('key') (lookup from implementation metrics)
  formula: "fixed"                         # "fixed" = use base prices as-is

  # Examples (set by implementation):
  # Network:     "base * metric('etx') * (1 + metric('srtt_ms') / 100)"
  # Electricity: "base * (1 + metric('demand_ratio'))"
  # Water:       "base * metric('scarcity_index')"

  # Price bounds (applied after formula computation)
  price_floor_multiplier: 0.1            # never below 10% of base
  price_ceiling_multiplier: 10.0         # never above 10x base
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `pricing.enabled` | `false` | Dynamic pricing disabled by default |
| `pricing.formula` | `"fixed"` | Use base prices as-is |
| `price_floor_multiplier` | `0.1` | Min price = 10% of base |
| `price_ceiling_multiplier` | `10.0` | Max price = 10x base |

---

## Channel Parameters

```yaml
channels:
  min_capacity: 10                         # minimum Spilman channel capacity (sats)
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
| `min_capacity` | `10` | Minimum channel funding (sats) |
| `max_capacity` | `10000` | Maximum channel funding (sats) |
| `initial_capacity` | `10` | First channel capacity for new peers |
| `capacity_growth_factor` | `2.0` | Capacity multiplier per successful rollover |
| `ttl_seconds` | `3600` | Channel lifetime (1 hour) |
| `rollover_threshold` | `0.80` | Trigger rollover at 80% exhaustion |
| `safety_margin_seconds` | `60` | Emergency rollover window before expiry |
| `stale_timeout_seconds` | `60` | Session closed if rollover blocked this long |

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

## Bootstrap

```yaml
bootstrap:
  enabled: true                            # accept bootstrap tokens
  min_token_value: 10                      # minimum token value to accept (sats)
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `bootstrap.enabled` | `true` | Accept bootstrap tokens |
| `min_token_value` | `10` | Reject tokens below this value |

Bootstrap tokens are always verified with the mint before service is granted. If the mint is unreachable the token is rejected outright — there is no pending / unverified buffer. See [tollgate-bootstrap.md](tollgate-bootstrap.md).

---

## Accepted Mints

```yaml
mints:
  - url: "https://mint.example.com"
    mint_units: ["sat", "msat"]
  - url: "https://mint.eu"
    mint_units: ["sat", "eur"]
```

Only mints listed here are accepted for both bootstrap tokens and Spilman channel funding. If a peer offers a product priced in a mint not on this list, the node rejects it.

### Multi-Mint Resilience (Future)

Operators are encouraged to maintain overlapping channels across at least two mints (three preferred) so that a single mint outage doesn't block all channel funding, rollover, and settlement. The current design lists multiple mints in this section but does not specify:

- How channels are distributed across mints (round-robin? capacity-weighted? per-peer?)
- How a node responds when a mint becomes unreachable mid-session (drain to other mints? wait?)
- How funds are rebalanced between mints (manual today; automated inter-mint transfer is future work)

For v1, operators configure multiple mints and manually ensure channels are spread across them. Automated mint distribution and rebalancing policy is **future work**.

---

## Peer Overrides

```yaml
peers:
  # Zero-price peering (operator's own nodes, friends)
  "02abc...":
    price_multiplier: 0.0

  # Discount for a specific peer
  "03def...":
    price_multiplier: 0.5

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
| `price_multiplier` | `1.0` | Multiply base price for this peer |
| `blocked` | `false` | Refuse all service to this peer |
| `endpoint` | *(none)* | Static endpoint for IP peering |

---

## Full Example

```yaml
identity:
  secret_key_file: "/etc/tollgate/identity.key"

products:
  - name: "standard"
    pricing_scale: 1000
    pricing:
      - mint_url: "https://mint.example.com"
        price_per_second: 0
        price_per_unit: 10
        mint_unit: "sat"
    extensions:
      bandwidth_limit: 0

  - name: "budget"
    pricing_scale: 1000
    pricing:
      - mint_url: "https://mint.example.com"
        price_per_second: 50
        price_per_unit: 0
        mint_unit: "sat"
    extensions:
      bandwidth_limit: 50000

pricing:
  enabled: true
  formula: "base * metric('etx') * (1 + metric('srtt_ms') / 100)"
  price_floor_multiplier: 0.1
  price_ceiling_multiplier: 10.0

channels:
  initial_capacity: 10
  max_capacity: 5000
  ttl_seconds: 3600
  rollover_threshold: 0.80

metering:
  interval_range: [3000, 10000]
  default_interval_ms: 5000
  transit_loss_tolerance: 0.05

bootstrap:
  enabled: true
  min_token_value: 10

mints:
  - url: "https://mint.example.com"
    mint_units: ["sat"]

peers:
  "02abc...":
    price_multiplier: 0.0
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting the node:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Product pricing | Yes | New prices take effect at next metering interval |
| Dynamic pricing rules | Yes | Strategy and factors can be updated |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Channel parameters | No | Applies to new channels only |
| Metering interval | No | Applies to new sessions only |
| Identity | No | Requires restart |
| Accepted mints | No | Requires restart (affects channel validity) |

The implementation watches the config file for changes and applies runtime-changeable parameters without interrupting active sessions.

---

## Design Decisions

| Decision | Resolution | Rationale |
|----------|-----------|-----------|
| Format | YAML | Follows FIPS pattern, human-readable, supports comments |
| Loading | Cascading multi-file with priority | System defaults + user overrides + deployment specifics |
| Defaults | Every parameter has a sensible default | Minimal config for simple deployments |
| Products | Array of named products | Multiple offerings per node |
| No products | Node is consumer-only | Pay peers but don't sell |
| Pricing | Formula expression evaluated against opaque metrics | Core doesn't interpret metrics — implementation sets policy, core executes |
| Product extensions | Opaque CBOR blob for implementation-specific fields | Core hashes but doesn't interpret |
| Peer overrides | By pubkey | Per-peer pricing, blocking, zero-price |
| Runtime changes | Pricing and peer overrides are hot-reloadable | Operator can adjust without downtime |
| OpenWrt | YAML directly, UCI integration future | Keep it simple initially |
