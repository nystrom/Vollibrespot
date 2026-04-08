#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v docker >/dev/null 2>&1; then
  echo "docker is required"
  exit 1
fi

if ! docker info >/dev/null 2>&1; then
  echo "docker daemon is not running"
  exit 1
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "cargo is required"
  exit 1
fi

if ! command -v cross >/dev/null 2>&1; then
  cargo install cross --locked
fi

if [ "$(uname -s)" = "Darwin" ] && [ "$(uname -m)" = "arm64" ]; then
  export CROSS_CONTAINER_OPTS="--platform linux/amd64"
  export CROSS_BUILD_OPTS="--platform linux/amd64"
fi

cross build --release --target armv7-unknown-linux-gnueabihf --manifest-path "Cargo.toml"
cross build --release --target aarch64-unknown-linux-gnu --manifest-path "Cargo.toml"

tar czf "$ROOT_DIR/vollibrespot-armv7l.tar.xz" -C "$ROOT_DIR/target/armv7-unknown-linux-gnueabihf/release" vollibrespot
tar czf "$ROOT_DIR/vollibrespot-aarch64.tar.xz" -C "$ROOT_DIR/target/aarch64-unknown-linux-gnu/release" vollibrespot
