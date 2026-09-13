#!/usr/bin/env bash
# Build the wasm module and the TypeScript, then stage both into dist/.
#
# Requires the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
set -euo pipefail

# rustup installs to ~/.cargo/bin, which is not always on PATH in npm scripts.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
cd "$(dirname "$0")"

echo "==> cargo build --target wasm32-unknown-unknown"
(cd .. && cargo build --release --target wasm32-unknown-unknown --lib)

echo "==> tsc"
npx tsc -p tsconfig.json

WASM=../target/wasm32-unknown-unknown/release/glider.wasm
cp "$WASM" dist/glider.wasm
echo "==> dist/glider.wasm $(du -h dist/glider.wasm | cut -f1)"
