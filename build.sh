#!/usr/bin/env bash
# Build the static musl binary into dist/. Requires podman or docker.
set -euo pipefail

cd "$(dirname "$0")"

# Pinned digest of rust:1-alpine (rustc 1.99.1). The CI release build uses the
# same image so the published asset is reproducible from a local build.
IMAGE=${IMAGE:-docker.io/library/rust@sha256:8975e9b19af39614f3fab1b03444f39e4a77dfea9a619a51dcfa54c5257785db}
TARGET=x86_64-unknown-linux-musl
CARGO_CACHE=${CARGO_CACHE:-/tmp/xone-relay-cargo}
RUSTFLAGS=${RUSTFLAGS:--C link-arg=-Wl,--build-id=none}

if [ -n "${ENGINE:-}" ]; then
    engine=$ENGINE
elif command -v podman > /dev/null; then
    engine=podman
elif command -v docker > /dev/null; then
    engine=docker
else
    echo "build: install podman or docker" >&2
    exit 1
fi

if [ "$engine" = podman ]; then
    userns=(--userns=keep-id)
    volume_suffix=:Z
else
    userns=()
    volume_suffix=
fi

mkdir -p "$CARGO_CACHE" dist

"$engine" run --rm "${userns[@]}" \
    --user "$(id -u):$(id -g)" \
    -e HOME=/tmp \
    -e CARGO_HOME=/tmp/cargo \
    -e RUSTFLAGS="$RUSTFLAGS" \
    -v "$PWD":/src$volume_suffix \
    -v "$CARGO_CACHE":/tmp/cargo$volume_suffix \
    -w /src \
    "$IMAGE" \
    sh -c "cargo test --release --target $TARGET && cargo build --release --target $TARGET"

install -m 0755 "target/$TARGET/release/xone-relay" dist/xone-relay
echo
file dist/xone-relay
stat -c 'size: %s bytes' dist/xone-relay
sha256sum dist/xone-relay
