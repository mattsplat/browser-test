#!/bin/sh
# Build a Rust page program: a crate (crate-type cdylib) that depends on packs/rust and calls
# `unscripted::main!`. Output: <crate>/app.wasm, beside its index.html.
#
# usage: packs/rust/build.sh examples/rust-counter
set -e
crate="${1:?usage: $0 path/to/crate}"
cd "$crate"
name="$(sed -n 's/^name *= *"\(.*\)"/\1/p' Cargo.toml | head -1 | tr - _)"
cargo build --release --target wasm32-wasip1
cp "target/wasm32-wasip1/release/$name.wasm" app.wasm
echo "built $(pwd)/app.wasm ($(du -h app.wasm | cut -f1))"
