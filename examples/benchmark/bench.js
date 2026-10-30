// The benchmark in JavaScript. bench.py, bench.php and bench.dart do the same work the same way,
// so the chart compares the languages, not the algorithms.

const LANG = 'js';
const LANGUAGES = [['js', 'JavaScript', '#D4A72C'], ['python', 'Python', '#3B82F6'], ['php', 'PHP', '#8B5CF6'], ['dart', 'Dart', '#14B8A6']];
const TASKS = [
  ['fib', 'fib(25), recursive'],
  ['sieve', 'Primes below 1,000,000'],
  ['strings', 'Join and split 20,000 strings'],
  ['dom', 'Create and remove 1,000 elements'],
  ['canvas', 'Draw 3,000 rectangles'],
];

function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

// Each task returns a check value, the same in every language.
const RUN = {
  fib: () => fib(25),
  sieve: () => {
    const n = 1000000;
    const composite = new Uint8Array(n + 1);
    let count = 0;
    for (let i = 2; i <= n; i++) {
      if (!composite[i]) {
        count++;
        for (let j = i * i; j <= n; j += i) composite[j] = 1;
      }
    }
    return count;
  },
  strings: () => {
    const parts = [];
    for (let i = 0; i < 20000; i++) parts.push('item ' + i);
    return parts.join(',').split(',').length;
  },
  dom: () => {
    const list = document.querySelector('#dom');
    for (let i = 0; i < 1000; i++) {
      const item = document.createElement('li');
      item.textContent = 'item ' + i;
      list.appendChild(item);
    }
    const count = list.children.length;
    list.textContent = '';
    return count;
  },
  canvas: () => {
    // A Park-Miller sequence, stepped with Schrage's method so no product passes 2^31 (PHP's integers are 32-bit here): every language draws the same rectangles in the same colors.
    const ctx = document.querySelector('#scratch').getContext('2d');
    let seed = 1;
    const next = () => {
      seed = 48271 * (seed % 44488) - 3399 * Math.floor(seed / 44488);
      if (seed <= 0) seed += 2147483647;
      return seed;
    };
    for (let i = 0; i < 3000; i++) {
      const x = next() % 300;
      const y = next() % 150;
      const c = next();
      ctx.fillStyle = `rgb(${c % 256}, ${(c >> 8) % 256}, ${(c >> 16) % 256})`;
      ctx.fillRect(x, y, 10, 10);
    }
    return seed;
  },
};

const status = (text) => (document.querySelector('#status').textContent = text);
let results = {};

function addRow(label, ms, check) {
  const row = document.createElement('tr');
  for (const [text, cls] of [[label, ''], [ms.toFixed(1), 'ms'], [String(check), '']]) {
    const cell = document.createElement('td');
    cell.textContent = text;
    if (cls) cell.className = cls;
    row.appendChild(cell);
  }
  document.querySelector('#rows').appendChild(row);
}

// Each task runs in its own timer callback, so the page repaints between them and no single
// callback comes near the 5 s limit.
function step(i) {
  if (i === TASKS.length) {
    localStorage.setItem('bench:' + LANG, TASKS.map(([key]) => `${key}=${results[key].toFixed(2)}`).join(';'));
    status('Done. Run the other languages to compare them.');
    drawChart();
    return;
  }
  const [key, label] = TASKS[i];
  status(`Running: ${label}…`);
  setTimeout(() => {
    const start = performance.now();
    const check = RUN[key]();
    const ms = performance.now() - start;
    results[key] = ms;
    addRow(label, ms, check);
    step(i + 1);
  }, 0);
}

// Every language's saved results, as {lang: {task: ms}}.
function savedResults() {
  const all = {};
  for (const [code] of LANGUAGES) {
    const saved = localStorage.getItem('bench:' + code);
    if (!saved) continue;
    all[code] = {};
    for (const pair of saved.split(';')) {
      const [key, ms] = pair.split('=');
      all[code][key] = Number(ms);
    }
  }
  return all;
}

// One group of bars per task, one bar per language; the slowest fills the width.
function drawChart() {
  const ctx = document.querySelector('#chart').getContext('2d');
  const all = savedResults();
  ctx.fillStyle = '#ffffff';
  ctx.fillRect(0, 0, 640, 380);
  ctx.font = '12px sans-serif';
  let y = 16;
  for (const [key, label] of TASKS) {
    ctx.fillStyle = '#222222';
    ctx.fillText(label, 8, y);
    y += 6;
    const max = Math.max(1e-9, ...Object.values(all).map((r) => r[key] || 0));
    for (const [code, name, color] of LANGUAGES) {
      if (!all[code]) continue;
      const ms = all[code][key];
      const width = Math.max(1, (ms / max) * 440);
      ctx.fillStyle = color;
      ctx.fillRect(90, y, width, 11);
      ctx.fillStyle = '#444444';
      ctx.fillText(name, 8, y + 10);
      ctx.fillText(ms.toFixed(1) + ' ms', 96 + width, y + 10);
      y += 13;
    }
    y += 14;
  }
  if (Object.keys(all).length === 0) {
    ctx.fillStyle = '#888888';
    ctx.fillText('No results yet: press Run.', 8, y);
  }
}

document.querySelector('#run').addEventListener('click', () => {
  results = {};
  document.querySelector('#rows').textContent = '';
  step(0);
});
document.querySelector('#clear').addEventListener('click', () => {
  for (const [code] of LANGUAGES) localStorage.removeItem('bench:' + code);
  document.querySelector('#rows').textContent = '';
  drawChart();
});
status('JavaScript is ready.');
drawChart();
