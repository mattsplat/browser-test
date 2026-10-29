import { formatElapsed } from './format.js';

const clock = document.querySelector('#clock');
const started = Date.now();
const tick = () => { clock.textContent = `Running for ${formatElapsed(Date.now() - started)}`; };
tick();
setInterval(tick, 1000);
