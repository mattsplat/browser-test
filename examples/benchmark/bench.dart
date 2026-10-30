// The benchmark in Dart. bench.js, bench.py and bench.php do the same work the same way, so the
// chart compares the languages, not the algorithms. Build: packs/dart/build.sh examples/benchmark/bench.dart
import 'dart:async';
import 'dart:js_interop';
import 'dart:typed_data';

import 'package:web/web.dart';

const lang = 'dart';
const languages = [('js', 'JavaScript', '#D4A72C'), ('python', 'Python', '#3B82F6'), ('php', 'PHP', '#8B5CF6'), ('dart', 'Dart', '#14B8A6')];
const tasks = [
  ('fib', 'fib(25), recursive'),
  ('sieve', 'Primes below 1,000,000'),
  ('strings', 'Join and split 20,000 strings'),
  ('dom', 'Create and remove 1,000 elements'),
  ('canvas', 'Draw 3,000 rectangles'),
];

int fib(int n) => n < 2 ? n : fib(n - 1) + fib(n - 2);

CanvasRenderingContext2D context(String selector) =>
    (document.querySelector(selector) as HTMLCanvasElement).getContext('2d') as CanvasRenderingContext2D;

// Each task returns a check value, the same in every language.
int runTask(String key) {
  switch (key) {
    case 'fib':
      return fib(25);
    case 'sieve':
      const n = 1000000;
      final composite = Uint8List(n + 1);
      var count = 0;
      for (var i = 2; i <= n; i++) {
        if (composite[i] == 0) {
          count++;
          for (var j = i * i; j <= n; j += i) {
            composite[j] = 1;
          }
        }
      }
      return count;
    case 'strings':
      final parts = <String>[];
      for (var i = 0; i < 20000; i++) {
        parts.add('item $i');
      }
      return parts.join(',').split(',').length;
    case 'dom':
      final list = document.querySelector('#dom')!;
      for (var i = 0; i < 1000; i++) {
        final item = document.createElement('li');
        item.textContent = 'item $i';
        list.appendChild(item);
      }
      final count = list.children.length;
      list.textContent = '';
      return count;
    case 'canvas':
      // A Park-Miller sequence, stepped with Schrage's method so no product passes 2^31 (PHP's integers are 32-bit here): every language draws the same rectangles in the same colors.
      final ctx = context('#scratch');
      var seed = 1;
      int next() {
        seed = 48271 * (seed % 44488) - 3399 * (seed ~/ 44488);
        if (seed <= 0) seed += 2147483647;
        return seed;
      }
      for (var i = 0; i < 3000; i++) {
        final x = next() % 300;
        final y = next() % 150;
        final c = next();
        ctx.fillStyle = 'rgb(${c % 256}, ${(c >> 8) % 256}, ${(c >> 16) % 256})'.toJS;
        ctx.fillRect(x, y, 10, 10);
      }
      return seed;
  }
  throw ArgumentError('no task $key');
}

final results = <String, double>{};

void status(String text) => document.querySelector('#status')!.textContent = text;

void addRow(String label, double ms, int check) {
  final row = document.createElement('tr');
  for (final (text, cls) in [(label, ''), (ms.toStringAsFixed(1), 'ms'), ('$check', '')]) {
    final cell = document.createElement('td');
    cell.textContent = text;
    if (cls.isNotEmpty) cell.className = cls;
    row.appendChild(cell);
  }
  document.querySelector('#rows')!.appendChild(row);
}

// Each task runs in its own timer callback, so the page repaints between them and no single
// callback comes near the 5 s limit.
void step(int i) {
  if (i == tasks.length) {
    window.localStorage.setItem('bench:$lang', tasks.map((t) => '${t.$1}=${results[t.$1]!.toStringAsFixed(2)}').join(';'));
    status('Done. Run the other languages to compare them.');
    drawChart();
    return;
  }
  final (key, label) = tasks[i];
  status('Running: $label…');
  Timer(Duration.zero, () {
    final start = window.performance.now();
    final check = runTask(key);
    final ms = window.performance.now() - start;
    results[key] = ms;
    addRow(label, ms, check);
    step(i + 1);
  });
}

/// Every language's saved results, as {lang: {task: ms}}.
Map<String, Map<String, double>> savedResults() {
  final all = <String, Map<String, double>>{};
  for (final (code, _, _) in languages) {
    final saved = window.localStorage.getItem('bench:$code');
    if (saved == null || saved.isEmpty) continue;
    all[code] = {
      for (final pair in saved.split(';')) pair.split('=')[0]: double.parse(pair.split('=')[1]),
    };
  }
  return all;
}

/// One group of bars per task, one bar per language; the slowest fills the width.
void drawChart() {
  final ctx = context('#chart');
  final all = savedResults();
  ctx.fillStyle = '#ffffff'.toJS;
  ctx.fillRect(0, 0, 640, 380);
  ctx.font = '12px sans-serif';
  var y = 16.0;
  for (final (key, label) in tasks) {
    ctx.fillStyle = '#222222'.toJS;
    ctx.fillText(label, 8, y);
    y += 6;
    final biggest = all.values.map((r) => r[key] ?? 0.0).fold(1e-9, (a, b) => a > b ? a : b);
    for (final (code, name, color) in languages) {
      final ms = all[code]?[key];
      if (ms == null) continue;
      final width = (ms / biggest * 440).clamp(1.0, 440.0);
      ctx.fillStyle = color.toJS;
      ctx.fillRect(90, y, width, 11);
      ctx.fillStyle = '#444444'.toJS;
      ctx.fillText(name, 8, y + 10);
      ctx.fillText('${ms.toStringAsFixed(1)} ms', 96 + width, y + 10);
      y += 13;
    }
    y += 14;
  }
  if (all.isEmpty) {
    ctx.fillStyle = '#888888'.toJS;
    ctx.fillText('No results yet: press Run.', 8, y);
  }
}

void main() {
  document.querySelector('#run')!.onClick.listen((_) {
    results.clear();
    document.querySelector('#rows')!.textContent = '';
    step(0);
  });
  document.querySelector('#clear')!.onClick.listen((_) {
    for (final (code, _, _) in languages) {
      window.localStorage.removeItem('bench:$code');
    }
    document.querySelector('#rows')!.textContent = '';
    drawChart();
  });
  status('Dart is ready.');
  drawChart();
}
