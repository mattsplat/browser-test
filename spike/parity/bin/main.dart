import 'dart:async';
import 'dart:js_interop';
import 'package:web/web.dart';

Future<void> main() async {
  document.cookie = 'a=1';
  print(document.cookie);
  Timer(const Duration(milliseconds: 10), () => print('timer'));
  Timer.periodic(const Duration(milliseconds: 10), (t) => t.cancel());
  window.requestAnimationFrame(((num t) => print('frame $t')).toJS);
  final items = document.querySelectorAll('li');
  for (var i = 0; i < items.length; i++) { print(items.item(i)); }
  final el = document.querySelector('#x') as HTMLElement;
  el.classList.add('on'); el.classList.remove('off'); el.classList.toggle('t'); print(el.classList.contains('on'));
  el.style.color = 'red'; el.style.setProperty('margin', '4px');
  el.innerHTML = '<b>hi</b>'.toJS;
  print(el.children.length); print(el.parentElement);
  final input = document.querySelector('input') as HTMLInputElement;
  input.value = 'v'; print(input.value);
  final sub = el.onClick.listen((e) { e.preventDefault(); print('${e.clientX} ${e.button}'); });
  await sub.cancel();
  document.body!.onKeyDown.listen((e) => print(e.key));
  input.onInput.listen((e) => print(input.value));
  final resp = await window.fetch('/data.json'.toJS).toDart;
  print((await resp.text().toDart).toDart);
  window.localStorage.setItem('k', 'v'); print(window.localStorage.getItem('k'));
  window.sessionStorage.removeItem('k');
  print(window.location.href); window.history.pushState(null, '', '/next');
  print(window.innerWidth);
  console.log('logged'.toJS);
}
