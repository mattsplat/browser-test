// DOM breadth and form events, the same in every pack (dom.dart, dom.py). The cross-pack
// test runs all three and expects identical output.
const out = document.getElementById('out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

// Queries: a NodeList by index, item() and iteration.
const all = document.querySelectorAll('#root p');
add(`query:${all.length},${all.item(0).id},${all[1].id},${all.item(9)},${[...all].map((p) => p.textContent).join('+')},${document.getElementById('nope')}`);

// The tree.
const root = document.getElementById('root');
const p1 = document.getElementById('p1');
const p2 = document.getElementById('p2');
const s = document.getElementById('s');
add(`tree:${root.children.length},${root.children.item(2).tagName},${p1.parentElement.id},${root.firstChild.nextSibling.id},${s.previousSibling.id},${root.lastChild.textContent},${p1.firstChild.nodeType},${root.parentElement.tagName}`);

// Moving, replacing, removing and cloning nodes.
root.insertBefore(s, p1);
const em = document.createElement('em');
em.textContent = 'new';
const replaced = root.replaceChild(em, p2);
root.removeChild(s);
const copy = p1.cloneNode(true);
copy.id = 'p3';
root.appendChild(copy);
const shallow = p1.cloneNode(false);
add(`mutate:${[...root.children].map((c) => c.id || c.tagName).join('+')},${replaced.id},${shallow.childNodes.length},${copy.textContent},${root.contains(copy)},${root.contains(s)}`);

// classList, dataset and style.
const list = p1.classList;
list.add('c');
list.remove('a');
add(`class:${list.toggle('b')},${list.toggle('d', true)},${list.contains('c')},${list.contains('a')},${list.length},${p1.className}`);
p1.dataset.fooBar = 'x';
add(`data:${p1.dataset.userId},${p1.getAttribute('data-foo-bar')}`);
p1.style.setProperty('background-color', 'blue');
p1.style.marginTop = '4px';
const css = p1.style.cssText;
const color = p1.style.getPropertyValue('color');
const removed = p1.style.removeProperty('color');
const after = p1.style.color;
p1.style.cssText = 'width: 10px';
add(`style:${css},${color},${removed},${after},${p1.style.width},${p1.getAttribute('style')}`);

// innerHTML is parsed; the script in it never runs. outerHTML serialises.
root.innerHTML = '<b class="x">bold &amp; <i>it</i></b><script>document.getElementById("out").textContent = "RAN"</script>';
add(`html:${root.children.length},${root.innerHTML},${root.firstChild.outerHTML}`);

// Form fields: value, checked, disabled.
const name = document.getElementById('name');
const agree = document.getElementById('agree');
const off = document.getElementById('off');
const initial = name.value;
name.value = 'typed';
agree.checked = true;
const wasDisabled = off.disabled;
off.disabled = false;
add(`form:${initial},${name.value},${name.getAttribute('value')},${wasDisabled},${agree.checked},${off.disabled},${off.hasAttribute('disabled')}`);

// A password's value is read, never shown: AI views must not see it.
const pw = document.getElementById('pw');
add(`secret:${pw.value.length},${document.getElementById('form').innerHTML.length > 0}`);

// Form events: focus, input, change, blur and submit (cancelled).
name.addEventListener('focus', () => add('focus'));
name.addEventListener('blur', () => add('blur'));
name.addEventListener('input', (e) => add(`input:${e.target.value}`));
name.addEventListener('change', () => add(`change:${name.value}`));
agree.addEventListener('change', () => add(`toggle:${agree.checked}`));
document.getElementById('form').addEventListener('submit', (e) => {
  e.preventDefault();
  add('submit');
});

// The box's layout, once laid out.
const box = document.getElementById('box');
box.addEventListener('click', () => {
  const r = box.getBoundingClientRect();
  add(`rect:${r.x},${r.y},${r.width},${r.height},${r.right},${r.bottom}`);
});
name.focus();
