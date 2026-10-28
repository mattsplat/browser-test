#!/bin/sh
# Compile a Dart entry point for this browser's Dart pack.
#
# usage: packs/dart/build.sh path/to/main.dart   ->  path/to/main.wasm + main.mjs
#
# dart2wasm emits legacy wasm exception instructions, which wasmtime does not implement,
# so the output is passed through the SDK's own wasm-opt with --translate-to-exnref.
# The feature list is explicit: --all-features lets wasm-opt emit "exact" reference
# types, which wasmtime rejects.
set -e

src="$1"
[ -n "$src" ] || { echo "usage: $0 path/to/main.dart" >&2; exit 2; }
out="${src%.dart}.wasm"

sdk="$(dirname "$(dirname "$(readlink -f "$(command -v dart)")")")"
wasm_opt="$sdk/bin/utils/wasm-opt"
[ -x "$wasm_opt" ] || { echo "wasm-opt not found in the Dart SDK at $sdk" >&2; exit 1; }

dart compile wasm "$src" -o "$out"
"$wasm_opt" "$out" \
  --enable-gc --enable-reference-types --enable-exception-handling \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-multivalue --enable-tail-call \
  --translate-to-exnref -o "$out"
echo "built $out (+ ${out%.wasm}.mjs, read by the browser as an import table only)"
