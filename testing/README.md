# Integration tests

Docker-based integration harnesses for `tollgate-net`. The workspace is compiled
**once** into `tollgate-test:latest` (see `docker/Dockerfile` — a multi-stage
build whose first stage runs `cargo build`); every topology runs that same image
with different configs and commands. Nothing is rebuilt per container.

## Prerequisites

- Docker with the Compose plugin (`docker compose`)
- The daemon running (`docker info` succeeds)

## Build the image

```sh
testing/scripts/build.sh
```

Re-run after changing Rust code. Topologies can then reuse it with
`SKIP_BUILD=1`.

## Tests

| Test | Topology | Asserts |
|------|----------|---------|
| `detect/` | gateway (parent) ↔ client (child) | the two nodes detect each other (mutual Announce) |
| `bootstrap/` | fake mint ← gateway ← client | child pays a bootstrap token; gateway verifies it with the mint and grants access |
| `exhaust/` | fake mint ← gateway ← client | a paid peer that stops topping up is suspended when its balance runs out |
| `metering/` | fake mint ← gateway ← client | the client's `consume` loop tops up before exhaustion and stays Active |
| `drift/` | fake mint ← gateway ← (lying) client → upstream | a peer that under-reports what it received is warned each interval and cut off after 3 consecutive over-tolerance intervals |

The `bootstrap/` mint is a stock `python:3-slim` running `fake-mint.py` — a NUT-07
check-state stub that reports every proof UNSPENT. It exercises the provider's
real verification path without a full Cashu mint; swap in `cdk-spilman-test-mint`
later for payment-correctness fidelity.

Run one:

```sh
testing/detect/test.sh          # builds, runs, asserts, cleans up
SKIP_BUILD=1 testing/detect/test.sh   # reuse an existing image
```

## Adding a test

1. Create `testing/<name>/docker-compose.yml` using `image: tollgate-test:latest`.
2. Add per-role `*.yaml` node configs.
3. Write `testing/<name>/test.sh` that brings the topology up and asserts on
   container logs / exit codes. Source the same build step.

The `detect/` harness is the seed; later milestones (bootstrap payment,
metering, suspension) extend the same parent-child shape.
