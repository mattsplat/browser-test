#!/bin/sh
# Build the Python interpreter the browser's Python pack runs: RustPython compiled to WebAssembly.
# Output: packs/python/target/wasm32-wasip1/release/python_pack.wasm
set -e
cd "$(dirname "$0")"
cargo build --release --target wasm32-wasip1
echo "built $(pwd)/target/wasm32-wasip1/release/python_pack.wasm"
