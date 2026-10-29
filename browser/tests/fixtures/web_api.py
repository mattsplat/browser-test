# The web platform APIs beyond the DOM, the same in every pack (web_api.dart, web_api.js).
# The cross-pack test runs all three and expects identical output.
out = document.querySelector('#out')
lines = []


def add(line):
    lines.append(line)
    out.textContent = '|'.join(lines)


def js(v):
    """A value as JS would print it."""
    if v is None:
        return 'null'
    if isinstance(v, bool):
        return 'true' if v else 'false'
    return str(v)


# Cookies: the page's own, never HttpOnly ones (the test set one from the "server").
document.cookie = 'a=1'
document.cookie = 'b=2; Path=/'
document.cookie = 'sneaky=1; HttpOnly'
add(f'cookie:{document.cookie}')
document.cookie = 'a=; Max-Age=0'
add(f'cookie:{document.cookie}')

# Storage: localStorage per origin (kept), sessionStorage per tab. Values are strings.
localStorage.clear()
localStorage.setItem('k', 'v')
localStorage.setItem('n', 2)
add(f"storage:{localStorage.getItem('k')},{localStorage.getItem('n')},{js(localStorage.getItem('missing'))},{localStorage.length},{localStorage.key(0)}")
localStorage.removeItem('k')
sessionStorage.setItem('s', 'tab')
add(f"storage:{localStorage.length},{localStorage.key(0)},{sessionStorage.getItem('s')},{sessionStorage.length}")

# Location and history: pushState and replaceState change the URL without a load; a new
# fragment queues popstate and hashchange; back and other navigations go to the tab.
add(f'location:{location.protocol},{location.pathname},{location.search},{location.hash},{js(location.href == location.origin + location.pathname)}')
window.addEventListener('popstate', lambda e: add(f'popstate:{location.hash},{js(e.state)}'))
window.addEventListener('hashchange', lambda e: add(f"hashchange:{js(e.newURL.endswith('#next'))}"))
history.pushState({'page': 2}, '', 'two?x=1')
add(f'push:{location.pathname},{location.search},{history.state.page},{history.length}')
history.replaceState('three', '', '#h')
add(f'replace:{location.pathname}{location.search}{location.hash},{history.state},{history.length}')
location.hash = 'next'
history.back()
location.assign('/elsewhere')

# Window basics: size, pixel ratio, scrolling, navigator. (The test's window is 800×600
# physical pixels at 2×.)
window.scrollTo(0, 50)
window.scrollBy({'top': 10})
add(f"window:{window.innerWidth},{window.innerHeight},{window.devicePixelRatio},{window.scrollX},{window.scrollY},{js(navigator.userAgent.startswith('Mozilla/5.0'))},{navigator.language}")
window.addEventListener('resize', lambda e: add(f'resize:{window.innerWidth}'))

# fetch, through the gate: same-origin (with cookies), a POST, an error status, a redirect, a
# blocked third party, and cross-origin reads that CORS allows (without cookies) or refuses.
# Python gets callbacks: `then(on_value, on_error)`.
cross = out.getAttribute('data-cross')


def error_name(e):
    return str(e).split(':')[0]


def on_data(r):
    add(f"fetch:{r.status},{js(r.ok)},{r.headers.get('content-type')},{js(r.headers.get('set-cookie'))}")
    r.json().then(on_json)


def on_json(data):
    add(f'json:{data.name},{data.n}')
    add(f'cookie:{document.cookie}')
    fetch('echo', {'method': 'POST', 'headers': {'X-A': 'b'}, 'body': 'hello'}).then(lambda r: r.text().then(on_echo))


def on_echo(text):
    add(f'echo:{text}')
    fetch('missing').then(on_missing)


def on_missing(r):
    def with_text(text):
        add(f'missing:{r.status},{js(r.ok)},{text}')
        fetch('redirect').then(on_redirect)
    r.text().then(with_text)


def on_redirect(r):
    add(f'redirect:{r.status},{js(r.redirected)}')

    def blocked(e):
        add(f'blocked:{error_name(e)}')
        fetch(cross + 'cors').then(lambda r: r.text().then(on_cors))
    fetch('http://blocked.example/x').then(lambda r: add('blocked:no'), blocked)


def on_cors(text):
    add(f'cors:{text}')
    fetch(cross + 'nocors').then(lambda r: add('nocors:read'), lambda e: add(f'nocors:{error_name(e)}'))


fetch('data.json').then(on_data)
