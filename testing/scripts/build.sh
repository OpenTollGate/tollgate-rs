#!/usr/bin/env bash
# Build the single tollgate test image, reused by all integration harnesses.
#
# The image is built once and tagged `tollgate-test:latest`; every compose
# topology runs that same image. Re-run this after changing Rust code.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTING_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
PROJECT_ROOT="$(cd "$TESTING_DIR/.." && pwd)"

if [ ! -f "$PROJECT_ROOT/Cargo.toml" ]; then
    echo "Error: Cannot find Cargo.toml at $PROJECT_ROOT" >&2
    exit 1
fi

# BuildKit is required for the Dockerfile's cache mounts (persisted cargo
# registry + target/ across builds). Default in modern Docker; forced here so
# incremental rebuilds are fast regardless of daemon config.
export DOCKER_BUILDKIT=1

echo "Building tollgate-test:latest (workspace compiled once inside the image)..."
docker build \
    -t tollgate-test:latest \
    -f "$TESTING_DIR/docker/Dockerfile" \
    "$PROJECT_ROOT"

echo "Done. Image: tollgate-test:latest"
