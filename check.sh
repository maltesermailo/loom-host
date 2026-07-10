#!/usr/bin/env bash
# CI-style gate for loom-host. Runs the three checks that define "done":
#   1. cargo test  --workspace                     (unit tests)
#   2. cargo clippy --workspace --all-targets -Dwarnings
#   3. vector-check <vector-adapter> spec/vectors  (all conformance vectors)
#
# The conformance harness and the vectors both come from the pinned spec
# submodule, so this script is self-contained within the host repo.
set -euo pipefail
cd "$(dirname "$0")"

echo "== [1/3] cargo test --workspace =="
cargo test --workspace

echo "== [2/3] cargo clippy --workspace --all-targets -D warnings =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== [3/3] conformance vectors =="
cargo build --quiet -p loom-proto --bin vector-adapter
ADAPTER="$(pwd)/target/debug/vector-adapter"

# Build the harness from the pinned spec submodule (its own standalone crate).
( cd spec/vector-check && cargo build --quiet )
spec/vector-check/target/debug/vector-check "$ADAPTER" spec/vectors

echo
echo "ALL CHECKS PASSED"
