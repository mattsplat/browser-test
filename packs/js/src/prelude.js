// Runs before any page script. Turns host handles into ordinary-looking JS objects:
// property reads and writes and method calls on them become host requests. Pages see the
// same web platform names the Dart pack does, and nothing more.
(() => {
  const HANDLE = Symbol('host handle');
  const callbacks = [];

  // Members that are methods; every other member is a property. Keep in step with the
  // methods browser/src/host.rs implements.
  const METHODS = new Set([
    'querySelector', 'createElement', 'appendChild', 'remove',
    'setAttribute', 'getAttribute', 'removeAttribute', 'addEventListener', 'random',
  ]);

  // Values cross as JSON: undefined → {u:1}, host object → {o:h}, function → {cb:id}.
  const encode = (v) => {
    if (v === undefined) return { u: 1 };
    if (typeof v === 'function') { callbacks.push(v); return { cb: callbacks.length - 1 }; }
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

  const wrappers = new Map();
  const wrap = (h) => {
    if (!wrappers.has(h)) {
      wrappers.set(h, new Proxy({}, {
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
      }));
    }
    return wrappers.get(h);
  };

  globalThis.document = request({ op: 'global', name: 'document' });
  const math = request({ op: 'global', name: 'Math' });
  // Randomness comes from the host, as for every pack.
  Math.random = () => math.random();
  globalThis.console = {
    log: (...parts) => request({ op: 'console', text: parts.map(String).join(' ') }),
  };
  globalThis.console.error = globalThis.console.warn = globalThis.console.info = globalThis.console.log;

  // Called by the host to deliver an event to a listener.
  globalThis.__fire = (id, args) => callbacks[id](...JSON.parse(args).map(decode));
})();
