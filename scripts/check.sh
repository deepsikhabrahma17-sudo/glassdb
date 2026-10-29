#!/usr/bin/env bash
# Run the same checks as CI. Use this before every commit.
#   ./scripts/check.sh
set -euo pipefail
cd "$(dirname "$0")/.."

step() { printf '\n\033[1;34m==> %s\033[0m\n' "$1"; }

step "Formatting (cargo fmt)"
cargo fmt --all --check

step "Lints (cargo clippy, warnings are errors)"
cargo clippy --workspace --all-targets -- -D warnings

step "Tests (cargo test)"
cargo test --workspace

printf '\n\033[1;32m✔ All checks passed.\033[0m\n'
