# WebSocket through the gate, the same in every pack (ws.dart, ws.js). The cross-pack test
# runs all three against a local server and expects identical output.
out = document.getElementById('out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


def js(v):
    return ('true' if v else 'false') if isinstance(v, bool) else str(v)


ws = WebSocket(out.getAttribute('data-ws'))
add(f"state:{ws.readyState},{js(ws.url.endswith('/socket'))}")


def on_open(e):
    add(f'open:{ws.readyState},{ws.protocol}')
    ws.send('ping')


def on_message(e):
    add(f"message:{e.data if e.data.startswith('hello') else len(e.data)}")
    if e.data.endswith('PONG'):
        ws.send('bye')


def on_close(e):
    add(f'close:{e.code},{e.reason},{js(e.wasClean)},{ws.readyState}')
    # A third party the page isn't allowed: it fails, as a connection that went nowhere.
    blocked = WebSocket('ws://blocked.example/x')
    blocked.onerror = lambda e: add('blocked:error')
    blocked.onclose = lambda e: add(f'blocked:close:{e.code},{js(e.wasClean)}')


ws.onopen = on_open
ws.addEventListener('message', on_message)
ws.onclose = on_close
