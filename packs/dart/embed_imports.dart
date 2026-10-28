/// Moves dart2wasm's import table from the generated `.mjs` into the `.wasm` itself, as a
/// custom section named `dart2wasm-imports`, so a page ships one file and no JS. With a
/// source map, it is embedded too, as `dart2wasm-sourcemap`.
///
/// usage: dart run packs/dart/embed_imports.dart main.wasm main.mjs [main.wasm.map]
library;

import 'dart:convert';
import 'dart:io';
import 'dart:typed_data';

const importsSection = 'dart2wasm-imports';
const sourceMapSection = 'dart2wasm-sourcemap';

void main(List<String> args) {
  if (args.length < 2 || args.length > 3) {
    stderr.writeln('usage: embed_imports.dart <app.wasm> <app.mjs> [app.wasm.map]');
    exit(2);
  }
  final wasmFile = File(args[0]);
  final mjs = File(args[1]).readAsStringSync();

  // The table is the `const dart2wasm = { ... };` object literal in the loader.
  final start = mjs.indexOf('const dart2wasm = {');
  final end = start < 0 ? -1 : mjs.indexOf('\n    };', start);
  if (end < 0) {
    stderr.writeln('no dart2wasm import table found in ${args[1]}');
    exit(1);
  }
  final table = mjs.substring(start, end + '\n    };'.length);

  final sections = [..._customSection(importsSection, utf8.encode(table))];
  if (args.length == 3) {
    sections.addAll(_customSection(sourceMapSection, File(args[2]).readAsBytesSync()));
  }
  // Appended after the code, so code offsets (and the source map) are unchanged.
  wasmFile.writeAsBytesSync(Uint8List.fromList([...wasmFile.readAsBytesSync(), ...sections]));
}

/// A custom section is: id 0, payload size, name length, name, payload.
List<int> _customSection(String name, List<int> data) {
  final nameBytes = utf8.encode(name);
  final payload = [..._leb128(nameBytes.length), ...nameBytes, ...data];
  return [0, ..._leb128(payload.length), ...payload];
}

List<int> _leb128(int value) {
  final out = <int>[];
  do {
    var byte = value & 0x7f;
    value >>= 7;
    if (value != 0) byte |= 0x80;
    out.add(byte);
  } while (value != 0);
  return out;
}
