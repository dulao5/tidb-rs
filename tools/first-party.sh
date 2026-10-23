#!/usr/bin/env bash
# Print `-p <package>` for every first-party crate (crates/*). Vendored code in
# third_party/ is excluded from formatting and linting:
#   cargo fmt --check $(tools/first-party.sh)
#   cargo clippy --no-deps --all-targets $(tools/first-party.sh) -- -D warnings
set -euo pipefail
cd "$(dirname "$0")/.."
args=()
for manifest in crates/*/Cargo.toml; do
    name=$(sed -n 's/^name = "\(.*\)"$/\1/p' "$manifest" | head -n 1)
    args+=(-p "$name")
done
echo "${args[@]}"
