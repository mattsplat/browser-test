// The web platform APIs beyond the DOM, the same in every pack (web_api.js, web_api.py),
// through package:web. The cross-pack test runs all three and expects identical output.
import 'dart:js_interop';
import 'dart:js_interop_unsafe';
import 'package:web/web.dart';

Future<void> main() async {
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

  // Storage: localStorage per origin (kept), sessionStorage per tab. Values are strings.
  final local = window.localStorage, session = window.sessionStorage;
  local.clear();
  local.setItem('k', 'v');
  local.setItem('n', '2');
  add('storage:${local.getItem('k')},${local.getItem('n')},${local.getItem('missing')},${local.length},${local.key(0)}');
  local.removeItem('k');
  session.setItem('s', 'tab');
  add('storage:${local.length},${local.key(0)},${session.getItem('s')},${session.length}');

  // Location and history: pushState and replaceState change the URL without a load; a new
  // fragment queues popstate and hashchange; back and other navigations go to the tab.
  final loc = window.location, hist = window.history;
  add('location:${loc.protocol},${loc.pathname},${loc.search},${loc.hash},${loc.href == loc.origin + loc.pathname}');
  window.addEventListener('popstate', ((PopStateEvent e) => add('popstate:${loc.hash},${e.state}')).toJS);
  window.addEventListener('hashchange', ((HashChangeEvent e) => add('hashchange:${e.newURL.endsWith('#next')}')).toJS);
  hist.pushState({'page': 2}.jsify(), '', 'two?x=1');
  add('push:${loc.pathname},${loc.search},${(hist.state as JSObject).getProperty('page'.toJS)},${hist.length}');
  hist.replaceState('three'.toJS, '', '#h');
  add('replace:${loc.pathname}${loc.search}${loc.hash},${hist.state},${hist.length}');
  loc.hash = 'next';
  hist.back();
  loc.assign('/elsewhere');

  // fetch, through the gate: same-origin (with cookies), a POST, an error status, a redirect,
  // a blocked third party, and cross-origin reads that CORS allows (without cookies) or
  // refuses.
  final cross = out.getAttribute('data-cross')!;
  String errorName(Object e) => '$e'.split(':').first;
  final r = await window.fetch('data.json'.toJS).toDart;
  add('fetch:${r.status},${r.ok},${r.headers.get('content-type')},${r.headers.get('set-cookie')}');
  final data = (await r.json().toDart) as JSObject;
  add('json:${data.getProperty('name'.toJS)},${data.getProperty('n'.toJS)}');
  add('cookie:${document.cookie}');
  final init = RequestInit(method: 'POST', headers: {'X-A': 'b'}.jsify() as HeadersInit, body: 'hello'.toJS);
  final echo = await window.fetch('echo'.toJS, init).toDart;
  add('echo:${(await echo.text().toDart).toDart}');
  final missing = await window.fetch('missing'.toJS).toDart;
  add('missing:${missing.status},${missing.ok},${(await missing.text().toDart).toDart}');
  final moved = await window.fetch('redirect'.toJS).toDart;
  add('redirect:${moved.status},${moved.redirected}');
  try {
    await window.fetch('http://blocked.example/x'.toJS).toDart;
    add('blocked:no');
  } catch (e) {
    add('blocked:${errorName(e)}');
  }
  final cors = await window.fetch('${cross}cors'.toJS).toDart;
  add('cors:${(await cors.text().toDart).toDart}');
  try {
    await window.fetch('${cross}nocors'.toJS).toDart;
    add('nocors:read');
  } catch (e) {
    add('nocors:${errorName(e)}');
  }
}
