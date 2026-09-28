#!/usr/bin/env bash
# Stage both engines' WebAssembly builds next to bench-worker.mjs:
#   glider/  - glider built for wasm32-unknown-unknown, with its JS wrapper (ts/)
#   sqlite/  - SQLite's official WebAssembly build (@sqlite.org/sqlite-wasm)
# Then: node node-test.mjs   (runs every benchmark once, checks answers)
set -euo pipefail
cd "$(dirname "$0")"
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
(cd ../.. && cargo build --release --target wasm32-unknown-unknown --lib)
(cd ../../ts && npx tsc -p tsconfig.json)
mkdir -p glider sqlite
cp ../../target/wasm32-unknown-unknown/release/glider.wasm glider/
cp ../../ts/dist/index.js ../../ts/dist/types.js glider/
tmp=$(mktemp -d)
(cd "$tmp" && npm pack --silent @sqlite.org/sqlite-wasm >/dev/null && tar xzf ./*.tgz)
cp "$tmp/package/dist/index.mjs" "$tmp/package/dist/sqlite3.wasm" sqlite/
rm -r "$tmp"
echo "staged: $(du -sh glider sqlite | tr '\n' ' ')"
