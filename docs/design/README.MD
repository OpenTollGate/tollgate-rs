# TollGate Design Documents

Protocol design specifications for TollGate — a permissionless payment
layer for network forwarding, built on Cashu ecash and Spilman channels.

## Reading Order

Start with the introduction for the full architecture overview, then read
the core protocol documents in order. Peering documents are
network-specific and can be read independently.

### Core Protocol

| Document | Description |
| -------- | ----------- |
| [tollgate-intro.md](core/tollgate-intro.md) | Introduction: goals, architecture, payment model, security |
| [tollgate-pricing.md](core/tollgate-pricing.md) | Pricing: dual dimensions (time + bytes), products, dynamic adjustment |
| [tollgate-protocol.md](core/tollgate-protocol.md) | Wire protocol: CBOR messages, settlement flow, negotiation |
| [tollgate-payment-channels.md](core/tollgate-payment-channels.md) | Spilman channels: lifecycle, rollover, netting, wallet trait |
| [tollgate-bootstrap.md](core/tollgate-bootstrap.md) | Bootstrap tokens: verification, pay-only mode, upgrade path |
| [tollgate-access-control.md](core/tollgate-access-control.md) | Access control: forwarding gates, metering, drift resolution |

### Configuration

| Document | Description |
| -------- | ----------- |
| [tollgate-configuration.md](core/tollgate-configuration.md) | YAML configuration reference: products, pricing, channels, peers |

### Network Integration (Peering)

| Document | Description |
| -------- | ----------- |
| [peering-ip.md](peering/peering-ip.md) | Traditional IP networks: open access, HTTP/WebSocket, firewall rules |
| [peering-fips.md](peering/peering-fips.md) | FIPS mesh networks: native integration, MMP metrics, bloom filters |

### Migration

| Document | Description |
| -------- | ----------- |
| [FIPS_FEATURE_REQUESTS.md](FIPS_FEATURE_REQUESTS.md) | Required FIPS changes for TollGate integration |
