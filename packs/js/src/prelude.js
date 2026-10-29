// Runs before any page script. Turns host handles into ordinary-looking JS objects:
// property reads and writes and method calls on them become host requests. Pages see the
// same web platform names the Dart pack does, and nothing more.
(() => {
  const HANDLE = Symbol('host handle');
  // Callbacks by id, and each function's id, so passing the same function again (e.g. to
  // removeEventListener) gives the same id. The host says when an id can be released.
  const callbacks = new Map();
  const callbackIds = new Map();
  let nextCallback = 0;

  // Members that are methods; every other member is a property. Keep in step with the
  // methods browser/src/host.rs implements.
  const METHODS = new Set([
    'querySelector', 'createElement', 'appendChild', 'remove',
    'setAttribute', 'getAttribute', 'removeAttribute', 'addEventListener', 'removeEventListener',
    'preventDefault', 'stopPropagation', 'stopImmediatePropagation',
    'setTimeout', 'setInterval', 'clearTimeout', 'clearInterval',
    'requestAnimationFrame', 'cancelAnimationFrame', 'log', 'info', 'warn', 'debug', 'random',
    'fetch', 'text', 'json', 'get', 'has',
    'pushState', 'replaceState', 'back', 'forward', 'go', 'assign', 'replace', 'reload',
    'scrollTo', 'scroll', 'scrollBy',
    'getElementById', 'querySelectorAll', 'createTextNode', 'item', 'insertBefore', 'replaceChild',
    'removeChild', 'cloneNode', 'hasAttribute', 'contains', 'getBoundingClientRect', 'focus', 'blur',
    'add', 'toggle', 'setProperty', 'getPropertyValue', 'removeProperty',
    'WebSocket', 'send', 'close',
    // IndexedDB. (`transaction` is a method of databases; requests' and stores'
    // `transaction` properties aren't supported, as one name can't be both.)
    'open', 'deleteDatabase', 'createObjectStore', 'deleteObjectStore', 'transaction', 'objectStore',
    'put', 'delete', 'getAll', 'count', 'clear',
    'getContext', 'toDataURL', 'fillRect', 'strokeRect', 'clearRect', 'beginPath', 'moveTo', 'lineTo',
    'arc', 'rect', 'closePath', 'fill', 'stroke', 'fillText', 'save', 'restore',
  ]);
  // Storage's methods. Kept apart because `key` is also a property (of keyboard events).
  const STORAGE_METHODS = new Set(['getItem', 'setItem', 'removeItem', 'clear', 'key']);
  const STORAGE = new Set(['localStorage', 'sessionStorage']);
  // The console's methods: `error` is a method there, but a property of an IndexedDB request.
  const CONSOLE_METHODS = new Set([...METHODS, 'error']);
  // Methods that return a host promise, which becomes a real JS Promise.
  const PROMISES = new Set(['fetch', 'text']);

  // Values cross as JSON: undefined → {u:1}, host object → {o:h}, function → {cb:id}, a
  // plain object (a fetch init, a history state) → {obj: {...}}, an array → {arr: [...]}.
  const encode = (v) => {
    if (v === undefined) return { u: 1 };
    if (typeof v === 'function') {
      if (!callbackIds.has(v)) {
        callbackIds.set(v, nextCallback);
        callbacks.set(nextCallback, v);
        nextCallback++;
      }
      return { cb: callbackIds.get(v) };
    }
    if (v !== null && typeof v === 'object') {
      if (v[HANDLE] !== undefined) return { o: v[HANDLE] };
      if (Array.isArray(v)) return { arr: v.map(encode) };
      const proto = Object.getPrototypeOf(v);
      if (proto === Object.prototype || proto === null) {
        return { obj: Object.fromEntries(Object.entries(v).map(([k, x]) => [k, encode(x)])) };
      }
    }
    return v;
  };
  // The host rejects with a message such as "TypeError: Failed to fetch ...": that error.
  const toError = (reason) => {
    const m = /^(\w*Error): ([\s\S]*)$/.exec(String(reason));
    const Type = m && typeof globalThis[m[1]] === 'function' ? globalThis[m[1]] : Error;
    return new Type(m ? m[2] : String(reason));
  };
  const promised = (p) => new Promise((resolve, reject) => {
    request({ op: 'call', target: p[HANDLE], name: 'then', args: [resolve, (reason) => reject(toError(reason))].map(encode) });
  });
  const decode = (v) => {
    if (v === null || typeof v !== 'object') return v;
    if (v.u) return undefined;
    if (v.o !== undefined) return wrap(v.o, METHODS, v.r === 1);
    return v;
  };
  const request = (message) => {
    const reply = JSON.parse(__host(JSON.stringify(message)));
    if (reply.err !== undefined) throw new Error(reply.err);
    return decode(reply.ok);
  };

  // One wrapper per handle, so the same element is `===` itself. Held weakly: once page code
  // drops a wrapper (e.g. an event object after its dispatch), it can be collected.
  const wrappers = new Map();
  // A wrapper collected: forget it, and tell the host to free what it made for the page
  // (a response, parsed data), marked releasable (`r`).
  const forget = new FinalizationRegistry(({ h, releasable }) => {
    if (wrappers.get(h)?.deref() !== undefined) return;
    wrappers.delete(h);
    if (releasable) __host(JSON.stringify({ op: 'release', target: h }));
  });
  const wrap = (h, methods = METHODS, releasable = false) => {
    const existing = wrappers.get(h)?.deref();
    if (existing !== undefined) return existing;
    const proxy = new Proxy({}, {
      // Method closures hold the wrapper (`self`), so it isn't collected, and released, before
      // they're called.
      get(_, prop, self) {
        if (prop === HANDLE) return h;
        // Lists (a NodeList, an array of data) iterate by index, as in browsers.
        if (prop === Symbol.iterator) {
          return function* () {
            const n = self.length;
            for (let i = 0; i < n; i++) yield self[i];
          };
        }
        // Not thenable: resolving a JS promise with a host object mustn't call it.
        if (typeof prop !== 'string' || prop === 'then') return undefined;
        // Response.json() parses in JS, so the page gets ordinary objects and arrays.
        if (prop === 'json') return () => promised(request({ op: 'call', target: self[HANDLE], name: 'text', args: [] })).then((t) => JSON.parse(t));
        if (prop === 'forEach' && !methods.has(prop)) {
          return (f) => {
            const n = self.length;
            for (let i = 0; i < n; i++) f(self[i], i, self);
          };
        }
        if (STORAGE.has(prop)) {
          // Wrapped with Storage's methods (not by `decode`, which would wrap it generically).
          const reply = JSON.parse(__host(JSON.stringify({ op: 'get', target: h, name: prop })));
          if (reply.err !== undefined) throw new Error(reply.err);
          return wrap(reply.ok.o, STORAGE_METHODS);
        }
        if (methods.has(prop)) {
          return (...args) => {
            const result = request({ op: 'call', target: self[HANDLE], name: prop, args: args.map(encode) });
            return PROMISES.has(prop) ? promised(result) : result;
          };
        }
        return request({ op: 'get', target: h, name: prop });
      },
      set(_, prop, value) {
        request({ op: 'set', target: h, name: prop, value: encode(value) });
        return true;
      },
    });
    wrappers.set(h, new WeakRef(proxy));
    forget.register(proxy, { h, releasable });
    return proxy;
  };

  const window = request({ op: 'global', name: 'window' });
  globalThis.window = window;
  globalThis.document = request({ op: 'global', name: 'document' });
  globalThis.console = wrap(JSON.parse(__host(JSON.stringify({ op: 'global', name: 'console' }))).ok.o, CONSOLE_METHODS);
  const math = request({ op: 'global', name: 'Math' });
  // Randomness and timers come from the host, as for every pack.
  Math.random = () => math.random();
  // Extra arguments to setTimeout/setInterval are passed to the callback, as in browsers.
  const withArgs = (f, args) => (args.length ? () => f(...args) : f);
  globalThis.setTimeout = (f, ms = 0, ...args) => window.setTimeout(withArgs(f, args), ms);
  globalThis.setInterval = (f, ms = 0, ...args) => window.setInterval(withArgs(f, args), ms);
  globalThis.clearTimeout = (handle) => window.clearTimeout(handle);
  globalThis.clearInterval = (handle) => window.clearInterval(handle);
  globalThis.requestAnimationFrame = (f) => window.requestAnimationFrame(f);
  globalThis.cancelAnimationFrame = (handle) => window.cancelAnimationFrame(handle);
  // The network, through the page's gate.
  globalThis.fetch = (input, init) => window.fetch(String(input), init);
  globalThis.location = window.location;
  globalThis.navigator = window.navigator;
  // The window's size and scroll, read from the host each time.
  for (const name of ['innerWidth', 'innerHeight', 'devicePixelRatio', 'scrollX', 'scrollY', 'pageXOffset', 'pageYOffset']) {
    Object.defineProperty(globalThis, name, { get: () => window[name], configurable: true });
  }
  globalThis.scrollTo = (...args) => window.scrollTo(...args);
  globalThis.scrollBy = (...args) => window.scrollBy(...args);
  globalThis.history = window.history;
  // `new WebSocket(url, protocols)` makes a host socket (the constructor returns it).
  globalThis.WebSocket = class WebSocket {
    constructor(url, protocols) {
      return window.WebSocket(String(url), protocols);
    }
  };
  Object.assign(globalThis.WebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });
  globalThis.localStorage = window.localStorage;
  globalThis.sessionStorage = window.sessionStorage;
  globalThis.indexedDB = window.indexedDB;

  // Console results, formatted for reading: strings quoted, objects and arrays shown a few
  // levels deep, host objects by what they are. Element names cost two host calls (traced).
  const knownHosts = new Map([[window, 'Window'], [globalThis.document, 'Document'], [globalThis.console, 'Console'], [math, 'Math']]);
  const describeHost = (v) => {
    if (knownHosts.has(v)) return knownHosts.get(v);
    try {
      const id = v.id;
      return `<${v.tagName.toLowerCase()}${id ? ` id="${id}"` : ''}>`;
    } catch {
      return `[host object #${v[HANDLE]}]`;
    }
  };
  const MAX_ITEMS = 20;
  const inspect = (v, depth, seen) => {
    switch (typeof v) {
      case 'string': return JSON.stringify(v);
      case 'undefined': return 'undefined';
      case 'bigint': return `${v}n`;
      case 'symbol': return v.toString();
      case 'function': return `ƒ ${v.name || '(anonymous)'}()`;
      case 'object': break;
      default: return String(v);
    }
    if (v === null) return 'null';
    if (v[HANDLE] !== undefined) return describeHost(v);
    if (v instanceof Error) return v.stack || `${v.name}: ${v.message}`;
    if (v instanceof Date) return v.toISOString();
    if (v instanceof RegExp) return String(v);
    if (v instanceof Promise) return 'Promise {…}';
    if (seen.has(v)) return '[circular]';
    if (depth > 2) return Array.isArray(v) ? '[…]' : '{…}';
    seen.add(v);
    const more = (n) => (n > MAX_ITEMS ? [`… ${n - MAX_ITEMS} more`] : []);
    let out;
    if (Array.isArray(v)) {
      out = `[${[...v.slice(0, MAX_ITEMS).map((x) => inspect(x, depth + 1, seen)), ...more(v.length)].join(', ')}]`;
    } else if (v instanceof Map) {
      const items = [...v].slice(0, MAX_ITEMS).map(([k, x]) => `${inspect(k, depth + 1, seen)} => ${inspect(x, depth + 1, seen)}`);
      out = `Map(${v.size}) {${[...items, ...more(v.size)].join(', ')}}`;
    } else if (v instanceof Set) {
      const items = [...v].slice(0, MAX_ITEMS).map((x) => inspect(x, depth + 1, seen));
      out = `Set(${v.size}) {${[...items, ...more(v.size)].join(', ')}}`;
    } else {
      const keys = Object.keys(v);
      const items = keys.slice(0, MAX_ITEMS).map((k) => `${/^[A-Za-z_$][\w$]*$/.test(k) ? k : JSON.stringify(k)}: ${inspect(v[k], depth + 1, seen)}`);
      const name = v.constructor && v.constructor !== Object ? `${v.constructor.name} ` : '';
      out = `${name}{${[...items, ...more(keys.length)].join(', ')}}`;
    }
    seen.delete(v);
    return out;
  };
  globalThis.__inspect = (v) => inspect(v, 0, new Set());

  // Called by the host to call a listener or timer, and to release one it no longer uses.
  globalThis.__fire = (id, args) => callbacks.get(id)(...JSON.parse(args).map(decode));
  globalThis.__release = (id) => {
    const f = callbacks.get(id);
    callbacks.delete(id);
    if (f !== undefined && callbackIds.get(f) === id) callbackIds.delete(f);
  };
})();
