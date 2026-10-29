// XMLHttpRequest (async), the same in every pack (xhr.dart, xhr.py). The cross-pack test
// runs all three against a local server and expects identical output.
const out = document.getElementById('out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

const x = new XMLHttpRequest();
x.open('GET', 'data.json');
x.onreadystatechange = () => add(`state:${x.readyState}`);
// The body holds a secret the page never shows: AI views must not see it.
x.onload = () => add(`load:${x.status},${x.getResponseHeader('content-type')},${x.responseText.includes('"name":"web"')}`);
x.onloadend = () => {
  const y = new XMLHttpRequest();
  y.open('POST', 'echo');
  y.setRequestHeader('X-A', 'b');
  y.onload = () => {
    add(`echo:${y.responseText}`);
    const z = new XMLHttpRequest();
    z.open('GET', 'http://blocked.example/x');
    z.onerror = () => add(`blocked:${z.status},${z.readyState}`);
    z.addEventListener('loadend', () => add('end'));
    z.send();
  };
  y.send('hello');
};
x.send();
