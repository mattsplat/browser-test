// Canvas 2D, the same in every pack (canvas.js, canvas.py), through package:web. The
// cross-pack test runs all three and compares their output and their pixels.
import 'dart:js_interop';
import 'dart:math' as math;
import 'package:web/web.dart';

void main() {
  final out = document.getElementById('out')!;
  final lines = <String>[];
  void add(String line) {
    lines.add(line);
    out.textContent = lines.join('|');
  }

  final canvas = document.getElementById('c') as HTMLCanvasElement;
  final ctx = canvas.getContext('2d') as CanvasRenderingContext2D;
  add('size:${canvas.width},${canvas.height},${canvas.getContext('webgl')}');
  ctx.fillStyle = 'red'.toJS;
  ctx.fillRect(0, 0, 20, 10);
  ctx.strokeStyle = 'rgb(0, 128, 0)'.toJS;
  ctx.lineWidth = 4;
  ctx.strokeRect(25, 2, 10, 10);
  ctx.fillStyle = 'blue'.toJS;
  ctx.beginPath();
  ctx.arc(10, 25, 6, 0, math.pi * 2);
  ctx.fill();
  ctx.beginPath();
  ctx.moveTo(25, 20);
  ctx.lineTo(39, 20);
  ctx.lineTo(39, 34);
  ctx.closePath();
  ctx.fillStyle = '#ff00ff'.toJS;
  ctx.fill();
  ctx.clearRect(0, 0, 5, 5);
  ctx.fillStyle = 'black'.toJS;
  ctx.font = '14px sans-serif';
  ctx.fillText('Hi', 42, 14);
  add('style:${(ctx.fillStyle as JSString).toDart},${(ctx.strokeStyle as JSString).toDart},${ctx.lineWidth.toInt()},${ctx.font}');
  add('url:${canvas.toDataURL().startsWith('data:image/png;base64,')}');
}
