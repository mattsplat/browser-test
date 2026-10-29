// WebSocket through the gate, the same in every pack (ws.dart, ws.py). The cross-pack test
// runs all three against a local server and expects identical output.
const out = document.getElementById('out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

const ws = new WebSocket(out.getAttribute('data-ws'));
add(`state:${ws.readyState},${ws.url.endsWith('/socket')}`);
ws.onopen = () => {
  add(`open:${ws.readyState},${ws.protocol}`);
  ws.send('ping');
};
ws.addEventListener('message', (e) => {
  add(`message:${e.data.startsWith('hello') ? e.data : e.data.length}`);
  if (e.data.endsWith('PONG')) ws.send('bye');
});
ws.onclose = (e) => {
  add(`close:${e.code},${e.reason},${e.wasClean},${ws.readyState}`);
  // A third party the page isn't allowed: it fails, as a connection that went nowhere.
  const blocked = new WebSocket('ws://blocked.example/x');
  blocked.onerror = () => add('blocked:error');
  blocked.onclose = (e) => add(`blocked:close:${e.code},${e.wasClean}`);
};
