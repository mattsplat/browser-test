# Runs before any page script, in its own namespace. Turns host handles into Python objects:
# attribute reads and writes and method calls on them become host requests. Pages see the
# same web platform names the Dart and JS packs do (document.querySelector, textContent...),
# and nothing more. `host_request` (from lib.rs) is the only way out.
import builtins
import sys
import time

# Members that are methods; every other member is a property. Keep in step with the methods
# browser/src/host.rs implements (packs/js/src/prelude.js has the same list).
METHODS = {
    'querySelector', 'createElement', 'appendChild', 'remove',
    'setAttribute', 'getAttribute', 'removeAttribute', 'addEventListener', 'removeEventListener',
    'preventDefault', 'stopPropagation', 'stopImmediatePropagation',
    'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'requestAnimationFrame', 'cancelAnimationFrame', 'log', 'info', 'warn', 'debug', 'random',
    'fetch', 'then', 'text', 'json', 'get', 'has',
    'pushState', 'replaceState', 'back', 'forward', 'go', 'assign', 'replace', 'reload',
    'scrollTo', 'scroll', 'scrollBy',
    'getElementById', 'querySelectorAll', 'createTextNode', 'item', 'insertBefore', 'replaceChild',
    'removeChild', 'cloneNode', 'hasAttribute', 'contains', 'getBoundingClientRect', 'focus', 'blur',
    'add', 'toggle', 'setProperty', 'getPropertyValue', 'removeProperty',
    'WebSocket', 'send', 'close',
    # IndexedDB. (`transaction` is a method of databases; requests' and stores'
    # `transaction` properties aren't supported, as one name can't be both.)
    'open', 'deleteDatabase', 'createObjectStore', 'deleteObjectStore', 'transaction', 'objectStore',
    'put', 'delete', 'getAll', 'count', 'clear',
    'getContext', 'toDataURL', 'fillRect', 'strokeRect', 'clearRect', 'beginPath', 'moveTo', 'lineTo',
    'arc', 'rect', 'closePath', 'fill', 'stroke', 'fillText', 'save', 'restore',
}
# Storage's methods. Kept apart because `key` is also a property (of keyboard events).
STORAGE_METHODS = {'getItem', 'setItem', 'removeItem', 'clear', 'key'}
# Methods that return a host promise, wrapped in `Promise` (below).
PROMISES = {'fetch', 'text', 'json'}

# Callbacks by id, and each function's id, so passing the same function again (e.g. to
# removeEventListener) gives the same id. The host says when an id can be released.
callbacks = {}
callback_ids = {}
next_callback = 0


def encode(v):
    """A value as it crosses to the host: None → null, host object → {o: h}, function → {cb: id},
    a dict (a fetch init, a history state) → {obj: {...}}, a list or tuple → {arr: [...]}."""
    global next_callback
    if isinstance(v, HostObject):
        return {'o': v._handle}
    if isinstance(v, Promise):
        return {'o': v._host._handle}
    if isinstance(v, dict):
        return {'obj': {str(k): encode(x) for k, x in v.items()}}
    if isinstance(v, (list, tuple)):
        return {'arr': [encode(x) for x in v]}
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
            return HostObject(v['o'], 'r' in v)
    # The host's numbers are doubles; whole ones read as ints, as they print in JS.
    if isinstance(v, float) and v.is_integer() and abs(v) < 2 ** 53:
        return int(v)
    return v


def request(message, error=RuntimeError):
    reply = host_request(message)
    if 'err' in reply:
        raise error(reply['err'])
    return decode(reply['ok'])


# Live wrappers per handle the host marked releasable (a response, parsed data): when the
# last goes, the host frees the object.
held = {}


class HostObject:
    """A web platform object (window, document, an element, an event) the host holds."""
    __slots__ = ('_handle', '_releasable')
    _methods = METHODS

    def __init__(self, handle, releasable=False):
        object.__setattr__(self, '_handle', handle)
        object.__setattr__(self, '_releasable', releasable)
        if releasable:
            held[handle] = held.get(handle, 0) + 1

    def __del__(self):
        if self._releasable:
            h = self._handle
            left = held.get(h, 1) - 1
            if left:
                held[h] = left
            else:
                held.pop(h, None)
                host_request({'op': 'release', 'target': h})

    def __getattr__(self, name):
        # Only called for names the class doesn't have, i.e. the page's members.
        if name.startswith('_'):
            raise AttributeError(name)
        h = self._handle
        if name in ('localStorage', 'sessionStorage'):
            return Storage(request({'op': 'get', 'target': h, 'name': name})._handle)
        if name in self._methods:
            def call(*args):
                # Through `self`, so the object stays alive (unreleased) until the call.
                result = request({'op': 'call', 'target': self._handle, 'name': name, 'args': [encode(a) for a in args]})
                return Promise(result) if name in PROMISES else result
            return call
        return request({'op': 'get', 'target': h, 'name': name}, AttributeError)

    def __setattr__(self, name, value):
        h = self._handle
        request({'op': 'set', 'target': h, 'name': name, 'value': encode(value)})

    # Lists (a NodeList, an array of data) have a length and items; a DOMStringMap (`dataset`)
    # has items by name.
    def __len__(self):
        return self.length

    def __getitem__(self, key):
        value = request({'op': 'get', 'target': self._handle, 'name': str(key)})
        if value is None and isinstance(key, int) and not 0 <= key < len(self):
            raise IndexError(key)
        return value

    def __setitem__(self, key, value):
        request({'op': 'set', 'target': self._handle, 'name': str(key), 'value': encode(value)})

    def __iter__(self):
        for i in range(len(self)):
            yield self[i]

    # A host object is always true, whether or not it has a length.
    def __bool__(self):
        return True

    # Wrappers aren't cached (so none outlive their use); the same object compares equal.
    def __eq__(self, other):
        return isinstance(other, HostObject) and other._handle == self._handle

    def __hash__(self):
        return hash(self._handle)

    def __repr__(self):
        return describe(self)


class Console(HostObject):
    """The console: `error` is a method there, but a property of an IndexedDB request."""
    __slots__ = ()
    _methods = METHODS | {'error'}


class Storage(HostObject):
    """localStorage or sessionStorage."""
    __slots__ = ()
    _methods = STORAGE_METHODS


class Promise:
    """A host promise (from fetch, Response.text(), Response.json()). `then(on_value, on_error)`
    calls one of them later, from the event loop, never during the call; it returns None, so
    nest calls rather than chaining them. A rejection's reason is a message such as
    "TypeError: Failed to fetch ...". `then` may be called more than once."""
    __slots__ = ('_host', '_waiting', '_outcome')

    def __init__(self, host):
        self._host = host
        self._waiting = []
        self._outcome = None

    def then(self, on_value=None, on_error=None):
        if self._outcome is not None:
            ok, value = self._outcome
            f = on_value if ok else on_error
            if f is not None:
                setTimeout(lambda: f(value), 0)
            return
        self._waiting.append((on_value, on_error))
        # The host's promise takes one `then`; later ones wait here.
        if len(self._waiting) == 1:
            self._host.then(lambda v: self._settle(True, v), lambda e: self._settle(False, e))

    def _settle(self, ok, value):
        self._outcome = (ok, value)
        waiting, self._waiting = self._waiting, []
        for on_value, on_error in waiting:
            f = on_value if ok else on_error
            if f is not None:
                f(value)

    def __repr__(self):
        return 'Promise'


window = request({'op': 'global', 'name': 'window'})
document = request({'op': 'global', 'name': 'document'})
console = Console(request({'op': 'global', 'name': 'console'})._handle)
Math = request({'op': 'global', 'name': 'Math'})
location = window.location
navigator = window.navigator
history = window.history
localStorage = window.localStorage
sessionStorage = window.sessionStorage
indexedDB = window.indexedDB


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


def fetch(input, init=None):
    """The network, through the page's gate: a Promise of a Response."""
    return window.fetch(input, init)


def WebSocket(url, protocols=None):
    """A WebSocket through the page's gate. Set `onopen`, `onmessage`... or use
    addEventListener; handlers take the event."""
    return window.WebSocket(url, protocols)


def print(*args, sep=' ', end='\n', file=None, flush=False):
    console.log(sep.join(str(a) for a in args))


# The page runs on the browser's UI thread, so it must never block.
def sleep(seconds):
    raise RuntimeError('time.sleep would block the page; use setTimeout')


time.sleep = sleep

# Deeper recursion than this overflows the interpreter's Wasm stack, which crashes the pack
# (measured: about 5,000 simple frames fit), so a page can't raise the limit past it.
MAX_RECURSION = 3000
set_recursion_limit = sys.setrecursionlimit


def setrecursionlimit(limit):
    set_recursion_limit(min(limit, MAX_RECURSION))


sys.setrecursionlimit = setrecursionlimit


for name in ('window', 'document', 'console', 'Math', 'setTimeout', 'setInterval', 'clearTimeout',
             'clearInterval', 'requestAnimationFrame', 'cancelAnimationFrame', 'print', 'fetch', 'Promise', 'localStorage', 'sessionStorage', 'location', 'history', 'navigator', 'WebSocket', 'indexedDB'):
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
