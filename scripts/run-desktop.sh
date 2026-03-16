#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

if cargo run --offline "$@"; then
  exit 0
fi

echo "Offline run failed; retrying with network-enabled Cargo resolution..." >&2
cargo run "$@"
