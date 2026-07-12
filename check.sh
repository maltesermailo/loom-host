#!/usr/bin/env bash
# CI-style gate for loom-host. Runs the four checks that define "done":
#   1. cargo fmt   --all --check                    (formatting)
#   2. cargo test  --workspace                      (unit tests)
#   3. cargo clippy --workspace --all-targets -Dwarnings
#   4. vector-check <vector-adapter> spec/vectors   (all conformance vectors)
#
# The conformance harness and the vectors both come from the pinned spec
# submodule, so this script is self-contained within the host repo.
set -euo pipefail
cd "$(dirname "$0")"

echo "== [1/4] cargo fmt --all --check =="
cargo fmt --all --check

echo "== [2/4] cargo test --workspace =="
cargo test --workspace

echo "== [3/4] cargo clippy --workspace --all-targets -D warnings =="
cargo clippy --workspace --all-targets -- -D warnings

echo "== [4/4] conformance vectors =="
cargo build --quiet -p loom-proto --bin vector-adapter
ADAPTER="$(pwd)/target/debug/vector-adapter"

# Build the harness from the pinned spec submodule (its own standalone crate).
( cd spec/vector-check && cargo build --quiet )
spec/vector-check/target/debug/vector-check "$ADAPTER" spec/vectors

echo
echo "ALL CHECKS PASSED"
