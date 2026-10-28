import 'dart:js_interop';

@JS('host.domQuery')
external int domQuery(String selector);

@JS('host.domSetText')
external void domSetText(int node, String text);

@JS('host.domOnClick')
external void domOnClick(int node, JSFunction callback);

void main() {
  final node = domQuery('h1');
  domSetText(node, 'Hello from Dart');
  var clicks = 0;
  domOnClick(node, (() {
    clicks++;
    domSetText(node, 'Clicked $clicks times');
  }).toJS);
}
