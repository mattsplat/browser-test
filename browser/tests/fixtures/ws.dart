// WebSocket through the gate, the same in every pack (ws.js, ws.py), through package:web.
// The cross-pack test runs all three against a local server and expects identical output.
import 'dart:js_interop';
import 'package:web/web.dart';

void main() {
  final out = document.getElementById('out')!;
  final lines = <String>[];
  void add(String line) {
    lines.add(line);
    out.textContent = lines.join('|');
  }

  final ws = WebSocket(out.getAttribute('data-ws')!);
  add('state:${ws.readyState},${ws.url.endsWith('/socket')}');
  ws.onopen = ((Event e) {
    add('open:${ws.readyState},${ws.protocol}');
    ws.send('ping'.toJS);
  }).toJS;
  ws.addEventListener('message', ((MessageEvent e) {
    final data = (e.data as JSString).toDart;
    add('message:${data.startsWith('hello') ? data : data.length}');
    if (data.endsWith('PONG')) ws.send('bye'.toJS);
  }).toJS);
  ws.onclose = ((CloseEvent e) {
    add('close:${e.code},${e.reason},${e.wasClean},${ws.readyState}');
    // A third party the page isn't allowed: it fails, as a connection that went nowhere.
    final blocked = WebSocket('ws://blocked.example/x');
    blocked.onerror = ((Event e) => add('blocked:error')).toJS;
    blocked.onclose = ((CloseEvent e) => add('blocked:close:${e.code},${e.wasClean}')).toJS;
  }).toJS;
}
