# TollGate Design Documents

Protocol design specifications for TollGate — a permissionless payment
layer for metered resource delivery, built on Cashu ecash and Spilman
channels.

The core protocol is **resource-agnostic** — it works for network
forwarding (bytes), electricity (watt-hours), fluid delivery
(milliliters), or any metered resource. Network-specific integration
details live in the network-peering section.

## Reading Order

Start with the introduction for the full architecture overview, then read
the core protocol documents in order. Network-peering documents are
specific to `tollgate-net` and can be read independently.

### Core Protocol (Resource-Agnostic)

| Document | Description |
| -------- | ----------- |
| [tollgate-intro.md](core/tollgate-intro.md) | Introduction: goals, architecture, payment model, security |
| [tollgate-pricing.md](core/tollgate-pricing.md) | Pricing: dual dimensions (time + units), products, dynamic adjustment |
| [tollgate-protocol.md](core/tollgate-protocol.md) | Wire protocol: CBOR messages, interval flow, negotiation |
| [tollgate-payment-channels.md](core/tollgate-payment-channels.md) | Spilman channels: lifecycle, rollover, netting, wallet trait |
| [tollgate-bootstrap.md](core/tollgate-bootstrap.md) | Bootstrap tokens: verification, bootstrap-only mode, upgrade path |
| [tollgate-access-control.md](core/tollgate-access-control.md) | Access control: delivery gates, access levels, FIPS bloom filter visibility |
| [tollgate-metering.md](core/tollgate-metering.md) | Metering: counters, calibration, transit loss resolution, peer metrics |

### Configuration

| Document | Description |
| -------- | ----------- |
| [tollgate-configuration.md](core/tollgate-configuration.md) | YAML configuration reference: products, pricing, channels, peers |

### Network Integration (tollgate-net)

| Document | Description |
| -------- | ----------- |
| [peering-ip.md](network-peering/peering-ip.md) | Traditional IP networks: open access, HTTP/WebSocket, firewall rules |
| [peering-fips.md](network-peering/peering-fips.md) | FIPS mesh networks: native integration, MMP metrics, bloom filters |
| [peering-multi-egress.md](network-peering/peering-multi-egress.md) | Per-interface instances: traffic class segmentation on multi-homed routers |

### FIPS Changes

| Document | Description |
| -------- | ----------- |
| [FIPS_FEATURE_REQUESTS.md](FIPS_FEATURE_REQUESTS.md) | Required FIPS changes for TollGate integration |
