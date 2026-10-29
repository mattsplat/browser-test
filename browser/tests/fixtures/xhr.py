# XMLHttpRequest (async), the same in every pack (xhr.dart, xhr.js). The cross-pack test
# runs all three against a local server and expects identical output.
out = document.getElementById('out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


x = XMLHttpRequest()
x.open('GET', 'data.json')
x.onreadystatechange = lambda e: add(f'state:{x.readyState}')
# The body holds a secret the page never shows: AI views must not see it.
NAME = '"name":"web"'
x.onload = lambda e: add(f"load:{x.status},{x.getResponseHeader('content-type')},{'true' if NAME in x.responseText else 'false'}")


def after_x(e):
    y = XMLHttpRequest()
    y.open('POST', 'echo')
    y.setRequestHeader('X-A', 'b')

    def on_echo(e):
        add(f'echo:{y.responseText}')
        z = XMLHttpRequest()
        z.open('GET', 'http://blocked.example/x')
        z.onerror = lambda e: add(f'blocked:{z.status},{z.readyState}')
        z.addEventListener('loadend', lambda e: add('end'))
        z.send()
    y.onload = on_echo
    y.send('hello')


x.onloadend = after_x
x.send()
