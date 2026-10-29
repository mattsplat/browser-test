// Canvas 2D, the same in every pack (canvas.dart, canvas.py). The cross-pack test runs all
// three and compares their output and their pixels.
const out = document.getElementById('out');
const lines = [];
const add = (line) => {
  lines.push(line);
  out.textContent = lines.join('|');
};

const canvas = document.getElementById('c');
const ctx = canvas.getContext('2d');
add(`size:${canvas.width},${canvas.height},${canvas.getContext('webgl')}`);
ctx.fillStyle = 'red';
ctx.fillRect(0, 0, 20, 10);
ctx.strokeStyle = 'rgb(0, 128, 0)';
ctx.lineWidth = 4;
ctx.strokeRect(25, 2, 10, 10);
ctx.fillStyle = 'blue';
ctx.beginPath();
ctx.arc(10, 25, 6, 0, Math.PI * 2);
ctx.fill();
ctx.beginPath();
ctx.moveTo(25, 20);
ctx.lineTo(39, 20);
ctx.lineTo(39, 34);
ctx.closePath();
ctx.fillStyle = '#ff00ff';
ctx.fill();
ctx.clearRect(0, 0, 5, 5);
ctx.fillStyle = 'black';
ctx.font = '14px sans-serif';
ctx.fillText('Hi', 42, 14);
add(`style:${ctx.fillStyle},${ctx.strokeStyle},${ctx.lineWidth},${ctx.font}`);
add(`url:${canvas.toDataURL().startsWith('data:image/png;base64,')}`);
