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
    'requestAnimationFrame', 'cancelAnimationFrame', 'log', 'info', 'warn', 'error', 'debug', 'random',
  ]);

  // Values cross as JSON: undefined → {u:1}, host object → {o:h}, function → {cb:id}.
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
    if (v !== null && typeof v === 'object' && v[HANDLE] !== undefined) return { o: v[HANDLE] };
    return v;
  };
  const decode = (v) => {
    if (v === null || typeof v !== 'object') return v;
    if (v.u) return undefined;
    if (v.o !== undefined) return wrap(v.o);
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
  const forget = new FinalizationRegistry((h) => {
    if (wrappers.get(h)?.deref() === undefined) wrappers.delete(h);
  });
  const wrap = (h) => {
    const existing = wrappers.get(h)?.deref();
    if (existing !== undefined) return existing;
    const proxy = new Proxy({}, {
      get(_, prop) {
        if (prop === HANDLE) return h;
        if (typeof prop !== 'string') return undefined;
        if (METHODS.has(prop)) return (...args) => request({ op: 'call', target: h, name: prop, args: args.map(encode) });
        return request({ op: 'get', target: h, name: prop });
      },
      set(_, prop, value) {
        request({ op: 'set', target: h, name: prop, value: encode(value) });
        return true;
      },
    });
    wrappers.set(h, new WeakRef(proxy));
    forget.register(proxy, h);
    return proxy;
  };

  const window = request({ op: 'global', name: 'window' });
  globalThis.window = window;
  globalThis.document = request({ op: 'global', name: 'document' });
  globalThis.console = request({ op: 'global', name: 'console' });
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

  // Called by the host to call a listener or timer, and to release one it no longer uses.
  globalThis.__fire = (id, args) => callbacks.get(id)(...JSON.parse(args).map(decode));
  globalThis.__release = (id) => {
    const f = callbacks.get(id);
    callbacks.delete(id);
    if (f !== undefined && callbackIds.get(f) === id) callbackIds.delete(f);
  };
})();
