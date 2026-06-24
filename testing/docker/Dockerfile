# syntax=docker/dockerfile:1
# Single build of the `tollgate` binary, reused by every test container.
#
# Stage 1 (builder) compiles the workspace once. Stage 2 (runtime) is a slim
# image carrying just that one binary plus the tools a node needs at run time
# (nftables for the IP adapter). All compose roles — gateway, client — run this
# same image with different commands/configs; nothing is rebuilt per role.
#
# Build context is the repo root (see build.sh), so the whole workspace is
# available to the builder.
#
# Caching: BuildKit cache mounts persist the cargo registry and the target/
# directory *across* builds, so a source edit recompiles only what changed
# rather than every dependency from scratch. The `cp` out of target/ happens in
# the same RUN because cache-mount contents do not survive into the image layer.

# ---- Stage 1: build ----
FROM rust:1.94-slim-trixie AS builder

WORKDIR /src

RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config && \
    rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin tollgate --bin tolltop && \
    cp target/release/tollgate /tollgate && \
    cp target/release/tolltop /tolltop

# ---- Stage 2: runtime ----
FROM debian:trixie-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        nftables iproute2 iputils-ping curl ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /tollgate /usr/local/bin/tollgate
COPY --from=builder /tolltop /usr/local/bin/tolltop

# Default command is overridden per role in docker-compose.
ENTRYPOINT ["tollgate"]
