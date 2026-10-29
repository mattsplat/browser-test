// XMLHttpRequest (async), the same in every pack (xhr.js, xhr.py), through package:web.
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

  final x = XMLHttpRequest();
  x.open('GET', 'data.json');
  x.onreadystatechange = ((Event e) => add('state:${x.readyState}')).toJS;
  // The body holds a secret the page never shows: AI views must not see it.
  x.onload = ((Event e) => add('load:${x.status},${x.getResponseHeader('content-type')},${x.responseText.contains('"name":"web"')}')).toJS;
  x.onloadend = ((Event e) {
    final y = XMLHttpRequest();
    y.open('POST', 'echo');
    y.setRequestHeader('X-A', 'b');
    y.onload = ((Event e) {
      add('echo:${y.responseText}');
      final z = XMLHttpRequest();
      z.open('GET', 'http://blocked.example/x');
      z.onerror = ((Event e) => add('blocked:${z.status},${z.readyState}')).toJS;
      z.addEventListener('loadend', ((Event e) => add('end')).toJS);
      z.send();
    }).toJS;
    y.send('hello'.toJS);
  }).toJS;
  x.send();
}
