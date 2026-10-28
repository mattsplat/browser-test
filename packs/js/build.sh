#!/bin/sh
# Build the JS engine the browser's JS pack runs: Boa compiled to WebAssembly.
# Output: packs/js/target/wasm32-wasip1/release/js_pack.wasm
set -e
cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip1
echo "built $(pwd)/target/wasm32-wasip1/release/js_pack.wasm"
