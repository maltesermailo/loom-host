#!/usr/bin/env bash
# Build and launch loomd (release) for the loopback demo. Extra args pass through
# to loomd (e.g. --port, --width, --height, --drop-percent). Runs in the
# foreground; the superrepo scripts/demo.sh backgrounds it. --insecure-dev is
# always supplied (loopback dev only; pinning is M7).
set -euo pipefail

cd "$(dirname "$0")/.."  # host repo root

cargo build --release -p loomd

exec target/release/loomd --insecure-dev "$@"
