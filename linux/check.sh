#!/usr/bin/env bash
# Verification gate for the Linux tree. Mirrors what `make check` does for the
# macOS app: formatting, lints as errors, and the full test suite.
set -euo pipefail
cd "$(dirname "$0")"
echo "==> cargo fmt --check"; cargo fmt --all -- --check
echo "==> cargo clippy -D warnings"; cargo clippy --workspace --all-targets --all-features -- -D warnings
echo "==> cargo test"; cargo test --workspace
echo "OK"
