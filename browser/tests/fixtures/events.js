// The same behaviour as events.dart. The cross-pack test runs both and expects identical output.
const out = document.querySelector('#out');
const log = [];
const add = (entry) => {
  log.push(entry);
  out.textContent = log.join(',');
};

const button = document.querySelector('#button');
// A listener that removes itself, added twice (the duplicate is ignored).
const once = (e) => {
  add('once');
  button.removeEventListener('click', once);
};
button.addEventListener('click', once);
button.addEventListener('click', once);
button.addEventListener('click', (e) => add(`click:${e.clientX},${e.clientY},${e.button},${e.shiftKey}`));

// stopPropagation keeps the click from reaching #outer.
document.querySelector('#outer').addEventListener('click', () => add('outer'));
document.querySelector('#stop').addEventListener('click', (e) => {
  e.stopPropagation();
  add('stop');
});

// Keyboard details, and preventDefault.
document.querySelector('#field').addEventListener('keydown', (e) => {
  e.preventDefault();
  add(`key:${e.key},${e.code},${e.ctrlKey}`);
});

// The equivalent of Dart's subscription cancel: remove the listener after the first event.
const stream = document.querySelector('#stream');
const onStream = () => {
  add('stream');
  stream.removeEventListener('click', onStream);
};
stream.addEventListener('click', onStream);

// Timers.
setTimeout(() => add('timeout'), 1);
clearTimeout(setTimeout(() => add('cancelled!'), 1));
let ticks = 0;
const interval = setInterval(() => {
  ticks++;
  if (ticks === 3) {
    clearInterval(interval);
    add('interval:3');
  }
}, 1);
requestAnimationFrame(() => add('frame'));

// Nondeterminism, for the replay test: Math.random and the clock, now and from a timer.
const noise = document.querySelector('#noise');
noise.addEventListener('click', () => {
  noise.textContent += ` ${Math.floor(Math.random() * 1e9)}:${Date.now()}`;
  setTimeout(() => { noise.textContent += ` t${Math.floor(Math.random() * 1e9)}`; }, 1);
});
