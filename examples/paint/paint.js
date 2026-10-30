// Mouse drawing on a canvas. A stroke is a run of line segments from mousedown to mouseup;
// each point also gets a dot, since the canvas has no round line caps.

const COLORS = ['#222222', '#E53935', '#FB8C00', '#FDD835', '#43A047', '#1E88E5', '#8E24AA', '#FFFFFF'];
const pad = document.querySelector('#pad');
const ctx = pad.getContext('2d');
const state = { color: COLORS[0], size: 6, drawing: false, last: null, strokes: 0 };

function clear() {
  ctx.fillStyle = '#FFFFFF';
  ctx.fillRect(0, 0, 640, 400);
}

// Where the mouse is, in the canvas's own coordinates.
function point(event) {
  const box = pad.getBoundingClientRect();
  return { x: event.clientX - box.left, y: event.clientY - box.top };
}

function dot(p) {
  ctx.fillStyle = state.color;
  ctx.beginPath();
  ctx.arc(p.x, p.y, state.size / 2, 0, Math.PI * 2);
  ctx.fill();
}

function segment(from, to) {
  ctx.strokeStyle = state.color;
  ctx.lineWidth = state.size;
  ctx.beginPath();
  ctx.moveTo(from.x, from.y);
  ctx.lineTo(to.x, to.y);
  ctx.stroke();
  dot(to);
}

pad.addEventListener('mousedown', (event) => {
  state.drawing = true;
  state.last = point(event);
  dot(state.last);
});
pad.addEventListener('mousemove', (event) => {
  if (!state.drawing) return;
  const p = point(event);
  segment(state.last, p);
  state.last = p;
});
const stop = () => {
  if (state.drawing) {
    state.strokes++;
    document.querySelector('#status').textContent = `${state.strokes} stroke${state.strokes === 1 ? '' : 's'}.`;
  }
  state.drawing = false;
};
pad.addEventListener('mouseup', stop);
pad.addEventListener('mouseleave', stop);

// The palette: one swatch per color, the chosen one ringed.
const palette = document.querySelector('#colors');
for (const color of COLORS) {
  const swatch = document.createElement('div');
  swatch.className = color === state.color ? 'swatch on' : 'swatch';
  swatch.style.backgroundColor = color;
  swatch.dataset.color = color;
  if (color === '#FFFFFF') swatch.style.borderColor = '#ccc';
  palette.appendChild(swatch);
}
palette.addEventListener('click', (event) => {
  const color = event.target.dataset.color;
  if (!color) return;
  state.color = color;
  for (const swatch of palette.children) swatch.classList.toggle('on', swatch.dataset.color === color);
});

for (const button of document.querySelectorAll('.size')) {
  button.addEventListener('click', () => {
    state.size = Number(button.dataset.size);
    for (const other of document.querySelectorAll('.size')) other.classList.toggle('on', other === button);
  });
}
document.querySelector('#clear').addEventListener('click', clear);
document.querySelector('#save').addEventListener('click', () => {
  document.querySelector('#saved').setAttribute('src', pad.toDataURL());
});

clear();
document.querySelector('#status').textContent = 'Ready: draw on the canvas.';
