#!/usr/bin/env bash
# Build the single TollGate test image, reused by every topology.
#
# Tagged `tollgate-test:latest`. Re-run after changing Rust code; topologies can
# then reuse it with SKIP_BUILD=1. `IMAGE_TAG` builds another tag instead, so two
# checkouts building at once do not overwrite each other's image; a topology
# that names `tollgate-test:${IMAGE_TAG:-latest}` runs it.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="$(cd "$TESTING_DIR/.." && pwd)"

[[ -f "$ROOT/Cargo.toml" ]] || { echo "no Cargo.toml at $ROOT" >&2; exit 1; }

# BuildKit is required for the Dockerfile's cache mounts. Default in modern
# Docker; forced here so incremental rebuilds are fast regardless of config.
export DOCKER_BUILDKIT=1

IMAGE="tollgate-test:${IMAGE_TAG:-latest}"

echo "building $IMAGE ..."
docker build \
    -t "$IMAGE" \
    -f "$TESTING_DIR/docker/Dockerfile" \
    "$ROOT"

echo "done: $IMAGE"
