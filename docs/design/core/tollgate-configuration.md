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
settlement:  # Settlement interval and drift tolerance
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
    bandwidth_limit: 0                     # bytes/sec, 0 = unlimited
    pricing_scale: 1000                    # sub-unit precision divisor
    pricing:
      - mint: "https://mint.example.com"
        price_per_second: 0                # scaled integer
        price_per_byte: 10                 # scaled integer (0.01 sat/byte with scale=1000)
        unit: "sat"

      - mint: "https://mint.eu"
        price_per_second: 0
        price_per_byte: 8                  # discount for preferred mint
        unit: "sat"

  - name: "always-on"
    bandwidth_limit: 10000                 # 10 KB/s cap
    pricing_scale: 1000
    pricing:
      - mint: "https://mint.example.com"
        price_per_second: 100              # 0.1 sat/sec
        price_per_byte: 0
        unit: "sat"
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `bandwidth_limit` | `0` (unlimited) | Max bytes/sec for this product |
| `pricing_scale` | `1000` | Sub-unit precision divisor |

If no products are defined, the node operates as a **consumer only** — it pays peers but does not sell forwarding. A node with no products and no funds is effectively passive (zero-price peering with any peer that allows it).

---

## Dynamic Pricing

```yaml
pricing:
  enabled: false                           # enable dynamic price adjustments
  strategy: "fixed"                        # pricing strategy name

  # Strategy: "cost_plus" — scale by link quality metrics
  # price = base x etx x (1 + srtt_ms / 100)
  cost_plus:
    etx_weight: 1.0
    latency_weight: 0.01

  # Strategy: "demand" — scale by peer count
  # price = base x (1 + active_peers / max_peers)
  demand:
    max_peers: 10

  # Strategy: "quality_tiered" — discrete tiers based on metrics
  quality_tiered:
    premium_multiplier: 2.0               # loss < 1%, SRTT < 10ms
    standard_multiplier: 1.0              # loss < 5%, SRTT < 50ms
    economy_multiplier: 0.5               # loss < 10%, SRTT < 200ms
    degraded_multiplier: 0.1              # everything else

  # Price bounds (applied after strategy computation)
  price_floor_multiplier: 0.1            # never below 10% of base
  price_ceiling_multiplier: 10.0         # never above 10x base
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `pricing.enabled` | `false` | Dynamic pricing disabled by default |
| `pricing.strategy` | `"fixed"` | Use base prices as-is |
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

## Settlement

```yaml
settlement:
  interval_range: [3000, 10000]           # acceptable settlement interval range [min_ms, max_ms]
  default_interval_ms: 5000               # preferred interval (used if peer accepts)
  drift_tolerance: 0.05                   # 5% metering drift tolerance
  drift_max_consecutive: 3                # close after this many consecutive over-tolerance intervals
  drift_unacceptable: 0.50               # immediately close if drift exceeds this (50%)
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `interval_range` | `[3000, 10000]` | Acceptable interval in ms |
| `default_interval_ms` | `5000` | Preferred settlement interval |
| `drift_tolerance` | `0.05` | 5% drift tolerance |
| `drift_max_consecutive` | `3` | Close after 3 consecutive over-tolerance intervals |
| `drift_unacceptable` | `0.50` | Immediately close if drift exceeds 50% |

---

## Bootstrap

```yaml
bootstrap:
  enabled: true                            # accept bootstrap tokens
  min_token_value: 10                      # minimum token value to accept (sats)
  max_pending_tokens: 5                    # max unverified tokens in buffer (if offline)
```

### Defaults

| Parameter | Default | Description |
|-----------|---------|-------------|
| `bootstrap.enabled` | `true` | Accept bootstrap tokens |
| `min_token_value` | `10` | Reject tokens below this value |
| `max_pending_tokens` | `5` | Limit buffered unverified tokens |

---

## Accepted Mints

```yaml
mints:
  - url: "https://mint.example.com"
    units: ["sat", "msat"]
  - url: "https://mint.eu"
    units: ["sat", "eur"]
```

Only mints listed here are accepted for both bootstrap tokens and Spilman channel funding. If a peer offers a product priced in a mint not on this list, the node rejects it.

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
    bandwidth_limit: 0
    pricing_scale: 1000
    pricing:
      - mint: "https://mint.example.com"
        price_per_second: 0
        price_per_byte: 10
        unit: "sat"

  - name: "budget"
    bandwidth_limit: 50000
    pricing_scale: 1000
    pricing:
      - mint: "https://mint.example.com"
        price_per_second: 50
        price_per_byte: 0
        unit: "sat"

pricing:
  enabled: true
  strategy: "cost_plus"
  cost_plus:
    etx_weight: 1.0
    latency_weight: 0.01
  price_floor_multiplier: 0.1
  price_ceiling_multiplier: 10.0

channels:
  initial_capacity: 10
  max_capacity: 5000
  ttl_seconds: 3600
  rollover_threshold: 0.80

settlement:
  interval_range: [3000, 10000]
  default_interval_ms: 5000
  drift_tolerance: 0.05

bootstrap:
  enabled: true
  min_token_value: 10

mints:
  - url: "https://mint.example.com"
    units: ["sat"]

peers:
  "02abc...":
    price_multiplier: 0.0
```

---

## Runtime Changes

Some parameters can be changed at runtime without restarting the node:

| Parameter | Runtime changeable? | Notes |
|-----------|-------------------|-------|
| Product pricing | Yes | New prices take effect at next settlement |
| Dynamic pricing rules | Yes | Strategy and factors can be updated |
| Peer overrides | Yes | Add/remove/modify peer policies |
| Channel parameters | No | Applies to new channels only |
| Settlement interval | No | Applies to new sessions only |
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
| Pricing | Separate from products, applied as multiplier | Dynamic pricing doesn't change product structure |
| Peer overrides | By pubkey | Per-peer pricing, blocking, zero-price |
| Runtime changes | Pricing and peer overrides are hot-reloadable | Operator can adjust without downtime |
| OpenWrt | YAML directly, UCI integration future | Keep it simple initially |
