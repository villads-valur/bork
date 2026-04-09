#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

echo "=== cargo fmt --check ==="
cargo fmt -- --check

echo "=== cargo clippy ==="
cargo clippy

echo "=== cargo test ==="
cargo test

echo "=== All checks passed ==="
