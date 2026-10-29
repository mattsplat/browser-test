// The web platform APIs beyond the DOM, the same in every pack (web_api.dart, web_api.py).
// The cross-pack test runs all three and expects identical output.
const out = document.querySelector('#out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

// Cookies: the page's own, never HttpOnly ones (the test set one from the "server").
document.cookie = 'a=1';
document.cookie = 'b=2; Path=/';
document.cookie = 'sneaky=1; HttpOnly';
add(`cookie:${document.cookie}`);
document.cookie = 'a=; Max-Age=0';
add(`cookie:${document.cookie}`);

// fetch, through the gate: same-origin (with cookies), a POST, an error status, a redirect, a
// blocked third party, and cross-origin reads that CORS allows (without cookies) or refuses.
const cross = out.getAttribute('data-cross');
const errorName = (e) => String(e).split(':')[0];
(async () => {
  const r = await fetch('data.json');
  add(`fetch:${r.status},${r.ok},${r.headers.get('content-type')},${r.headers.get('set-cookie')}`);
  const data = await r.json();
  add(`json:${data.name},${data.n}`);
  add(`cookie:${document.cookie}`);
  const echo = await fetch('echo', { method: 'POST', headers: { 'X-A': 'b' }, body: 'hello' });
  add(`echo:${await echo.text()}`);
  const missing = await fetch('missing');
  add(`missing:${missing.status},${missing.ok},${await missing.text()}`);
  const moved = await fetch('redirect');
  add(`redirect:${moved.status},${moved.redirected}`);
  try {
    await fetch('http://blocked.example/x');
    add('blocked:no');
  } catch (e) {
    add(`blocked:${errorName(e)}`);
  }
  const cors = await fetch(`${cross}cors`);
  add(`cors:${await cors.text()}`);
  try {
    await fetch(`${cross}nocors`);
    add('nocors:read');
  } catch (e) {
    add(`nocors:${errorName(e)}`);
  }
})();
