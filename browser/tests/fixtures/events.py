# The same behaviour as events.dart and events.js. The cross-pack test runs all three and
# expects identical output.
out = document.querySelector('#out')
log = []


def add(entry):
    log.append(entry)
    out.textContent = ','.join(log)


def js_bool(b):
    return 'true' if b else 'false'


button = document.querySelector('#button')


# A listener that removes itself, added twice (the duplicate is ignored).
def once(e):
    add('once')
    button.removeEventListener('click', once)


button.addEventListener('click', once)
button.addEventListener('click', once)
button.addEventListener('click', lambda e: add(f'click:{e.clientX},{e.clientY},{e.button},{js_bool(e.shiftKey)}'))

# stopPropagation keeps the click from reaching #outer.
document.querySelector('#outer').addEventListener('click', lambda e: add('outer'))


def stop(e):
    e.stopPropagation()
    add('stop')


document.querySelector('#stop').addEventListener('click', stop)


# Keyboard details, and preventDefault.
def key(e):
    e.preventDefault()
    add(f'key:{e.key},{e.code},{js_bool(e.ctrlKey)}')


document.querySelector('#field').addEventListener('keydown', key)

# The equivalent of Dart's subscription cancel: remove the listener after the first event.
stream = document.querySelector('#stream')


def on_stream(e):
    add('stream')
    stream.removeEventListener('click', on_stream)


stream.addEventListener('click', on_stream)

# Timers.
setTimeout(lambda: add('timeout'), 1)
clearTimeout(setTimeout(lambda: add('cancelled!'), 1))
ticks = 0


def tick():
    global ticks
    ticks += 1
    if ticks == 3:
        clearInterval(interval)
        add('interval:3')


interval = setInterval(tick, 1)
requestAnimationFrame(lambda t: add('frame'))


# Nondeterminism, for the replay test: Math.random and the clock, now and from a timer.
import time
noise = document.querySelector('#noise')


def later():
    noise.textContent = noise.textContent + f' t{int(Math.random() * 1e9)}'


def on_noise(e):
    noise.textContent = noise.textContent + f' {int(Math.random() * 1e9)}:{int(time.time() * 1000)}'
    setTimeout(later, 1)


noise.addEventListener('click', on_noise)
