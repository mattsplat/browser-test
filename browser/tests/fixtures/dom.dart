// DOM breadth and form events, the same in every pack (dom.js, dom.py), through
// package:web. The cross-pack test runs all three and expects identical output.
import 'dart:js_interop';
import 'package:web/web.dart';

void main() {
  final out = document.getElementById('out')!;
  final lines = <String>[];
  void add(String line) {
    lines.add(line);
    out.textContent = lines.join('|');
  }

  String joined(NodeList list, String Function(Node) f) => [for (var i = 0; i < list.length; i++) f(list.item(i)!)].join('+');
  String joinedElements(HTMLCollection list, String Function(Element) f) => [for (var i = 0; i < list.length; i++) f(list.item(i)!)].join('+');

  // Queries: a NodeList by index, item() and iteration.
  final all = document.querySelectorAll('#root p');
  add('query:${all.length},${(all.item(0) as Element).id},${(all.item(1) as Element).id},${all.item(9)},${joined(all, (p) => p.textContent!)},${document.getElementById('nope')}');

  // The tree.
  final root = document.getElementById('root')!;
  final p1 = document.getElementById('p1') as HTMLElement;
  final p2 = document.getElementById('p2')!;
  final s = document.getElementById('s')!;
  add('tree:${root.children.length},${root.children.item(2)!.tagName},${p1.parentElement!.id},${(root.firstChild!.nextSibling as Element).id},${(s.previousSibling as Element).id},${root.lastChild!.textContent},${p1.firstChild!.nodeType},${root.parentElement!.tagName}');

  // Moving, replacing, removing and cloning nodes.
  root.insertBefore(s, p1);
  final em = document.createElement('em');
  em.textContent = 'new';
  final replaced = root.replaceChild(em, p2) as Element;
  root.removeChild(s);
  final copy = p1.cloneNode(true) as Element;
  copy.id = 'p3';
  root.appendChild(copy);
  final shallow = p1.cloneNode(false);
  add('mutate:${joinedElements(root.children, (c) => c.id.isEmpty ? c.tagName : c.id)},${replaced.id},${shallow.childNodes.length},${copy.textContent},${root.contains(copy)},${root.contains(s)}');

  // classList, dataset and style.
  final list = p1.classList;
  list.add('c');
  list.remove('a');
  final b = list.toggle('b');
  final d = list.toggle('d', true);
  add('class:$b,$d,${list.contains('c')},${list.contains('a')},${list.length},${p1.className}');
  p1.dataset['fooBar'] = 'x';
  add('data:${p1.dataset['userId']},${p1.getAttribute('data-foo-bar')}');
  p1.style.setProperty('background-color', 'blue');
  p1.style.marginTop = '4px';
  final css = p1.style.cssText;
  final color = p1.style.getPropertyValue('color');
  final removed = p1.style.removeProperty('color');
  final after = p1.style.color;
  p1.style.cssText = 'width: 10px';
  add('style:$css,$color,$removed,$after,${p1.style.width},${p1.getAttribute('style')}');

  // innerHTML is parsed; the script in it never runs. outerHTML serialises.
  root.innerHTML = '<b class="x">bold &amp; <i>it</i></b><script>document.getElementById("out").textContent = "RAN"</script>'.toJS;
  add('html:${root.children.length},${(root.innerHTML as JSString).toDart},${(root.firstChild as Element).outerHTML}');

  // Form fields: value, checked, disabled.
  final name = document.getElementById('name') as HTMLInputElement;
  final agree = document.getElementById('agree') as HTMLInputElement;
  final off = document.getElementById('off') as HTMLInputElement;
  final initial = name.value;
  name.value = 'typed';
  agree.checked = true;
  final wasDisabled = off.disabled;
  off.disabled = false;
  add('form:$initial,${name.value},${name.getAttribute('value')},$wasDisabled,${agree.checked},${off.disabled},${off.hasAttribute('disabled')}');

  // A password's value is read, never shown: AI views must not see it.
  final pw = document.getElementById('pw') as HTMLInputElement;
  add('secret:${pw.value.length},${(document.getElementById('form')!.innerHTML as JSString).toDart.isNotEmpty}');

  // Form events: focus, input, change, blur and submit (cancelled).
  name.addEventListener('focus', ((Event e) => add('focus')).toJS);
  name.addEventListener('blur', ((Event e) => add('blur')).toJS);
  name.addEventListener('input', ((Event e) => add('input:${(e.target as HTMLInputElement).value}')).toJS);
  name.addEventListener('change', ((Event e) => add('change:${name.value}')).toJS);
  agree.addEventListener('change', ((Event e) => add('toggle:${agree.checked}')).toJS);
  document.getElementById('form')!.addEventListener('submit', ((Event e) {
    e.preventDefault();
    add('submit');
  }).toJS);

  // The box's layout, once laid out.
  final box = document.getElementById('box')!;
  box.addEventListener('click', ((Event e) {
    final r = box.getBoundingClientRect();
    add('rect:${r.x.toInt()},${r.y.toInt()},${r.width.toInt()},${r.height.toInt()},${r.right.toInt()},${r.bottom.toInt()}');
  }).toJS);
  name.focus();
}
