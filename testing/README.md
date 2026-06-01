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
