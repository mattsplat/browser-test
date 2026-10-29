// The web platform APIs beyond the DOM, the same in every pack (web_api.js, web_api.py),
// through package:web. The cross-pack test runs all three and expects identical output.
import 'package:web/web.dart';

void main() {
  final out = document.querySelector('#out')!;
  final lines = <String>[];
  void add(String line) {
    lines.add(line);
    out.textContent = lines.join('|');
  }

  // Cookies: the page's own, never HttpOnly ones (the test set one from the "server").
  document.cookie = 'a=1';
  document.cookie = 'b=2; Path=/';
  document.cookie = 'sneaky=1; HttpOnly';
  add('cookie:${document.cookie}');
  document.cookie = 'a=; Max-Age=0';
  add('cookie:${document.cookie}');
}
