# Single build of the `tollgate` binary, reused by every test container.
#
# Stage 1 (builder) compiles the workspace once. Stage 2 (runtime) is a slim
# image carrying just that one binary plus the tools a node needs at run time
# (nftables for the IP adapter). All compose roles — gateway, client — run this
# same image with different commands/configs; nothing is rebuilt per role.
#
# Build context is the repo root (see build.sh), so the whole workspace is
# available to the builder.

# ---- Stage 1: build ----
FROM rust:1.94-slim-trixie AS builder

WORKDIR /src

# Build dependencies for secp256k1 (cc) and TLS (pkg-config/openssl via rustls
# we avoid openssl, but cc + perl are needed by some -sys crates).
RUN apt-get update && \
    apt-get install -y --no-install-recommends pkg-config && \
    rm -rf /var/lib/apt/lists/*

# Copy the full workspace and build the release binary in one layer.
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --bin tollgate && \
    cp target/release/tollgate /tollgate

# ---- Stage 2: runtime ----
FROM debian:trixie-slim

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        nftables iproute2 iputils-ping curl ca-certificates && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /tollgate /usr/local/bin/tollgate

# Default command is overridden per role in docker-compose.
ENTRYPOINT ["tollgate"]
