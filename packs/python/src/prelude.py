# Runs before any page script, in its own namespace. Turns host handles into Python objects:
# attribute reads and writes and method calls on them become host requests. Pages see the
# same web platform names the Dart and JS packs do (document.querySelector, textContent...),
# and nothing more. `host_request` (from lib.rs) is the only way out.
import builtins

# Members that are methods; every other member is a property. Keep in step with the methods
# browser/src/host.rs implements (packs/js/src/prelude.js has the same list).
METHODS = {
    'querySelector', 'createElement', 'appendChild', 'remove',
    'setAttribute', 'getAttribute', 'removeAttribute', 'addEventListener', 'removeEventListener',
    'preventDefault', 'stopPropagation', 'stopImmediatePropagation',
    'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'requestAnimationFrame', 'cancelAnimationFrame', 'log', 'info', 'warn', 'error', 'debug', 'random',
}

# Callbacks by id, and each function's id, so passing the same function again (e.g. to
# removeEventListener) gives the same id. The host says when an id can be released.
callbacks = {}
callback_ids = {}
next_callback = 0


def encode(v):
    """A value as it crosses to the host: None → null, host object → {o: h}, function → {cb: id}."""
    global next_callback
    if isinstance(v, HostObject):
        return {'o': v._handle}
    if v is None or isinstance(v, (bool, int, float, str)):
        return v
    if callable(v):
        id = callback_ids.get(v)
        if id is None:
            id = next_callback
            next_callback += 1
            callback_ids[v] = id
            callbacks[id] = v
        return {'cb': id}
    raise TypeError(f"a {type(v).__name__} can't be passed to the page yet")


def decode(v):
    if isinstance(v, dict):
        if 'u' in v:
            return None
        if 'o' in v:
            return HostObject(v['o'])
    # The host's numbers are doubles; whole ones read as ints, as they print in JS.
    if isinstance(v, float) and v.is_integer() and abs(v) < 2 ** 53:
        return int(v)
    return v


def request(message, error=RuntimeError):
    reply = host_request(message)
    if 'err' in reply:
        raise error(reply['err'])
    return decode(reply['ok'])


class HostObject:
    """A web platform object (window, document, an element, an event) the host holds."""
    __slots__ = ('_handle',)

    def __init__(self, handle):
        object.__setattr__(self, '_handle', handle)

    def __getattr__(self, name):
        # Only called for names the class doesn't have, i.e. the page's members.
        if name.startswith('_'):
            raise AttributeError(name)
        h = self._handle
        if name in METHODS:
            return lambda *args: request({'op': 'call', 'target': h, 'name': name, 'args': [encode(a) for a in args]})
        return request({'op': 'get', 'target': h, 'name': name}, AttributeError)

    def __setattr__(self, name, value):
        h = self._handle
        request({'op': 'set', 'target': h, 'name': name, 'value': encode(value)})

    # Wrappers aren't cached (so none outlive their use); the same object compares equal.
    def __eq__(self, other):
        return isinstance(other, HostObject) and other._handle == self._handle

    def __hash__(self):
        return hash(self._handle)

    def __repr__(self):
        return describe(self)


window = request({'op': 'global', 'name': 'window'})
document = request({'op': 'global', 'name': 'document'})
console = request({'op': 'global', 'name': 'console'})
Math = request({'op': 'global', 'name': 'Math'})


# Timers come from the host, as for every pack. Extra arguments are passed to the callback,
# as in browsers.
def setTimeout(f, ms=0, *args):
    return window.setTimeout((lambda: f(*args)) if args else f, ms)


def setInterval(f, ms=0, *args):
    return window.setInterval((lambda: f(*args)) if args else f, ms)


def clearTimeout(handle):
    window.clearTimeout(handle)


def clearInterval(handle):
    window.clearInterval(handle)


def requestAnimationFrame(f):
    return window.requestAnimationFrame(f)


def cancelAnimationFrame(handle):
    window.cancelAnimationFrame(handle)


def print(*args, sep=' ', end='\n', file=None, flush=False):
    console.log(sep.join(str(a) for a in args))


for name in ('window', 'document', 'console', 'Math', 'setTimeout', 'setInterval', 'clearTimeout',
             'clearInterval', 'requestAnimationFrame', 'cancelAnimationFrame', 'print'):
    setattr(builtins, name, globals()[name])

KNOWN = {window: 'Window', document: 'Document', console: 'Console', Math: 'Math'}


def describe(v):
    """A host object for the console: `<p id="out">`, or what it is. Elements cost two host calls."""
    if v in KNOWN:
        return KNOWN[v]
    try:
        id = v.id
        return f'<{v.tagName.lower()}' + (f' id="{id}"' if id else '') + '>'
    except Exception:
        return f'<host object #{v._handle}>'


# Called by the host (through lib.rs) to call a listener or timer, and to release one it no
# longer uses.
def fire(id, args):
    callbacks[id](*[decode(a) for a in args])


def release(id):
    f = callbacks.pop(id, None)
    if f is not None and callback_ids.get(f) == id:
        del callback_ids[f]
