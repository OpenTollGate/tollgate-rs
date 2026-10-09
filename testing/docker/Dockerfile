# syntax=docker/dockerfile:1
# One build of the TollGate binaries, reused by every topology.
#
# Stage 1 compiles the workspace; stage 2 is a slim runtime carrying the two
# binaries and the tools a node needs at run time. All compose roles run this
# same image with different configs — nothing is rebuilt per role.
#
# Build context is the repo root (see scripts/build.sh).
#
# Caching: BuildKit cache mounts persist the cargo registry, the git checkouts
# and target/ across builds, so a source edit recompiles only what changed. The
# `cp` out of target/ happens in the same RUN because cache-mount contents do
# not survive into the image layer.

# ---- Stage 1: build ----
FROM rust:1.90-slim-trixie AS builder

WORKDIR /src

# `git` because the Cashu dependencies are git dependencies, and
# `protobuf-compiler` because cdk's signatory reaches gRPC through prost, whose
# build script shells out to protoc.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        pkg-config git ca-certificates protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin tollgated --bin tolltop --bin mintd --bin minttop \
        --bin merchantd --bin merchanttop && \
    cargo build --release -p tollgate-protocol --example stub_gate && \
    cp target/release/tollgated /tollgated && \
    cp target/release/tolltop /tolltop && \
    cp target/release/mintd /mintd && \
    cp target/release/minttop /minttop && \
    cp target/release/merchantd /merchantd && \
    cp target/release/merchanttop /merchanttop && \
    cp target/release/examples/stub_gate /stub-gate

# ---- Stage 2: runtime ----
FROM debian:trixie-slim

# nftables and iproute2 are what the `ip` enforcer gates and shapes with. curl
# reaches the mint and market over HTTP; the control socket speaks raw JSON
# rather than HTTP, so netcat reads that one. jq does the assertions.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        nftables iproute2 iputils-ping curl ca-certificates jq netcat-openbsd && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /tollgated /usr/local/bin/tollgated
COPY --from=builder /tolltop /usr/local/bin/tolltop
COPY --from=builder /mintd /usr/local/bin/mintd
COPY --from=builder /minttop /usr/local/bin/minttop
COPY --from=builder /merchantd /usr/local/bin/merchantd
COPY --from=builder /merchanttop /usr/local/bin/merchanttop
# A gate that enforces nothing, for the `external` topology: it speaks the gate
# protocol and reports made-up counts. Not a node binary; nothing else runs it.
COPY --from=builder /stub-gate /usr/local/bin/stub-gate
COPY testing/docker/tollgate-node.sh /usr/local/bin/tollgate-node

# A node is the mint and the protocol daemon together; see tollgate-node.sh.
ENTRYPOINT ["tollgate-node"]
