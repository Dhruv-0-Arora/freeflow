#!/usr/bin/env bash
# Verification gate for the Linux tree. Mirrors what `make check` does for the
# macOS app: formatting, lints as errors, and the full test suite.
#
# Excluded here because they need hardware or a session, and are run
# deliberately instead:
#   cargo test -p freeflow-llm --test live_harness -- --ignored   (real claude)
#   cargo test -p freeflow-asr --test live_whisper -- --ignored   (whisper-server)
set -euo pipefail
cd "$(dirname "$0")"

echo "==> cargo fmt --check"
cargo fmt --all -- --check

echo "==> cargo clippy -D warnings"
cargo clippy --workspace --all-targets --all-features -- -D warnings

echo "==> cargo test"
cargo test --workspace

# The Home Manager module can only break during someone's rebuild otherwise.
if command -v nix >/dev/null 2>&1 && [ -z "${FREEFLOW_SKIP_NIX:-}" ]; then
  echo "==> nix flake check (home-manager module)"
  nix build "$(dirname "$PWD")#checks.x86_64-linux.home-manager-module" --no-link
fi

echo "OK"
