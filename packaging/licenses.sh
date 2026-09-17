#!/usr/bin/env bash
# Regenerate THIRD-PARTY-LICENSES.md from Cargo.lock.
#   cargo install --locked cargo-about
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
cargo about generate --locked --offline \
    -c packaging/about.toml packaging/about.hbs \
    -o THIRD-PARTY-LICENSES.md
echo "wrote THIRD-PARTY-LICENSES.md"
