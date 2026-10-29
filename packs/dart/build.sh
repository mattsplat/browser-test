#!/bin/sh
# Compile a Dart entry point for this browser's Dart pack.
#
# usage: packs/dart/build.sh path/to/main.dart [out.wasm]   (default: path/to/main.wasm)
#
# dart2wasm emits legacy wasm exception instructions, which wasmtime does not implement,
# so the output is passed through the SDK's own wasm-opt with --translate-to-exnref.
# The feature list is explicit: --all-features lets wasm-opt emit "exact" reference
# types, which wasmtime rejects.
set -e

src="$1"
[ -n "$src" ] || { echo "usage: $0 path/to/main.dart [out.wasm]" >&2; exit 2; }
out="${2:-${src%.dart}.wasm}"

sdk="$(dirname "$(dirname "$(readlink -f "$(command -v dart)")")")"
wasm_opt="$sdk/bin/utils/wasm-opt"
[ -x "$wasm_opt" ] || { echo "wasm-opt not found in the Dart SDK at $sdk" >&2; exit 1; }

# Compile through a small generated entry point beside the source (so relative imports and
# the package config resolve). It hands the browser a function that formats uncaught errors
# with Dart's own toString, then runs the page's main. It is deleted afterwards.
name="$(basename "$src" .dart)"
entry="$(dirname "$src")/.pack_entry_$name.dart"
trap 'rm -f "$entry"' EXIT
cat > "$entry" <<DART
import 'dart:js_interop';
import '$name.dart' as app;

@JS('__dartPackDescribeError')
external set _describeError(JSFunction f);

void main() {
  _describeError = ((ExternalDartReference error) => error.toDartObject.toString().toJS).toJS;
  (app.main as dynamic)();
}
DART

dart compile wasm "$entry" -o "$out"
"$wasm_opt" "$out" \
  --enable-gc --enable-reference-types --enable-exception-handling \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-mutable-globals --enable-multivalue --enable-tail-call \
  --translate-to-exnref \
  --input-source-map "$out.map" --output-source-map "$out.map" \
  -o "$out"

# Move the import table and source map into the .wasm and drop the loader files:
# pages ship one file. (wasm-opt above kept the map's offsets in step with its rewrite.)
dart run "$(dirname "$0")/embed_imports.dart" "$out" "${out%.wasm}.mjs" "$out.map"
rm -f "${out%.wasm}.mjs" "${out%.wasm}.support.js" "$out.map"
echo "built $out"
