# Language packs

A language pack runs a page's code in one language. There are three: **Dart** (dart2wasm
output), **JS** (Boa compiled to `wasm32-wasip1`) and **Python** (RustPython compiled to
`wasm32-wasip1`). They all run in wasmtime on the UI thread and reach the page only through
the host (`browser/src/host.rs`). This document says what a pack implements and what the
host guarantees in return. The code is the reference; the module named in each section is
where to look.

Two kinds of pack exist today:

- **Compiled** (Dart): the page ships Wasm. The pack is a Rust adapter between that Wasm's
  imports and the host (`dart_pack.rs`).
- **Interpreter** (JS, Python): the page ships source. The pack is an interpreter compiled to
  Wasm with a fixed ABI, and one shared Rust runtime (`interpreter.rs`) loads any such
  interpreter. Adding an interpreted language is a new crate under `packs/` and a
  `Language` static; nothing in the runtime changes.

## The contract in one place

`browser/src/page_document.rs` is the contract. Nothing in it knows which language a pack
is.

- `PackRuntime`: the trait a pack's runtime implements (`fire_callback`, `release_callback`,
  `can_eval`, `eval`, `stopped`).
- `PackDocument::starting(doc, name)` returns the page's `PageHost` (what the pack talks to)
  and a `Startup` (what start-up errors are reported to). `Startup::finish(runtime)` returns
  the running `PackDocument`.
- `engine()`, `TIME_LIMIT_TICKS`, `RAN_TOO_LONG`, `STOPPED` and `interrupted`: the shared
  wasmtime engine and the time limit.

A pack module provides one `start` function, which the loader calls:

```rust
// dart_pack.rs
pub fn start(doc: BaseDocument, wasm: &[u8]) -> Result<PackDocument>
// js_pack.rs (async: module imports are fetched through the gate)
pub async fn start(doc: BaseDocument, scripts: &[Script], fetch: F) -> Result<PackDocument>
// python_pack.rs
pub fn start(doc: BaseDocument, scripts: &[Script]) -> Result<PackDocument>
```

Every `start` has the same shape:

```rust
let (host, mut startup) = PackDocument::starting(doc, "python");
let mut runtime = Interpreter::load(&PYTHON, Box::new(host))?;
runtime.run_scripts(scripts, &mut startup);   // errors go to startup.uncaught(...)
Ok(startup.finish(Box::new(runtime)))
```

## Initialisation and loading

- **Choosing a pack** (`document_loader.rs`, `build_page_document`). A page runs one pack:
  the first of Dart (`<script type="application/dart" src="app.wasm">`), Python
  (`<script type="text/python">`, inline or `src`), JS (a plain `<script>` or
  `type="module"`). Other script types are ignored.
- **Permission** is checked before anything loads:
  - A page's scripts for packs that don't run are traced as skipped, e.g. `skipped 2 JS
    script(s): this page runs Python`. Script types compare ignoring ASCII case.
  - Dart and Python run by default and can be blocked per site (`dart_blocked`,
    `python_blocked` in settings, toggled in the Site panel).
  - JS follows `js_permission::decide`: a global default (Ask), and per-site "always allow",
    "keep off" and "allow this visit".
  - A skipped pack is traced as a `load` entry saying why.
- **Fetching:** the page's Wasm or script sources are fetched through the gate as requests
  from the page (`fetch_for_page`), so the third-party rules apply. An external script that
  fails to load is traced and skipped, as a browser would.
- **Compiling:** the wasmtime `Module` is compiled once per process.
  - **Interpreters:** the module is also cached on disk as
    `<cache dir>/<prefix>-engine-<hash>.cwasm`, keyed by a hash of the `.wasm` bytes and
    wasmtime's `precompile_compatibility_hash`. It is written with
    `profile::write_atomic`, because another profile's process may have it memory-mapped.
    Once a new file is saved, that pack's older `.cwasm` files are deleted.
  - **Dart:** a page's program is cached the same way, keyed by its wasm, wasmtime's
    compatibility hash **and the page's origin**, in the profile's `compiled-dart/` folder
    (at most 32 entries, oldest pruned). It's partitioned by site and profile because a hit
    loads measurably faster, which a site could time. Only compiled code is shared; each
    page still gets its own instance.
- **Instantiating:** each page gets a fresh `Store` and `Instance`, so no pack state is ever
  shared between pages or tabs. Every store has a memory cap of 512 MB
  (`page_document::MEMORY_LIMIT`, applied with `Store::limiter`). A page that allocates
  without end fails inside its pack (Python raises `MemoryError`; Boa traps, which stops the
  pack) and doesn't take the browser's memory. An interpreter's `_initialize` (the wasip1 reactor start) runs
  at instantiation; its interpreter context is created lazily on the first call.

## Running the page's code

- **Dart:** `$invokeMain`, then the microtask queue. An error out of `main` fails the start:
  the loader shows it in place of the page (with a snapshot if pause on exceptions is on).
- **Interpreters:** each classic script runs in document order (`Interpreter::run_scripts`).
  An error in one is reported through `Startup::uncaught` and the next still runs. All of a
  page's scripts share one global namespace, as in a browser.
- **JS modules** (JS only, `js_pack.rs`): module scripts run after classic scripts.
  - Before a module graph runs, `discover` reports its missing imports. The host fetches them
    through the gate and repeats until the graph is complete.
  - `run_module` then links and evaluates it. A failed import fetch is `Startup::report`ed
    (traced, no snapshot).
- **Python imports:** there are none from the page. Only the modules built into the
  interpreter exist (`sys`, `time`, `itertools`, `_io`...). There is no standard library and
  no importing of other page files (see "Python pack" below).
- **Script names:** a script is named by its URL, or `<page> (inline script N)` for inline
  ones. The name appears in error locations.

## Imports: what a pack's Wasm may import

- **Dart:** only what dart2wasm emits:
  - `dart2wasm` helpers, recognised by their snippet in the embedded import table and
    routed to the host by shape (global, get, set, call);
  - `wasm:js-string` builtins;
  - string constants;
  - the exception tag.

  Any other import fails to load.
- **Interpreters:** `host.request` and a fixed set of `wasi_snapshot_preview1` functions,
  all implemented in `interpreter.rs`:
  - `random_get` and `clock_time_get` come from the host (principle 4), through
    `Host::random_u64` and `Host::clock`; `clock_res_get` is a constant.
  - `fd_write` to stdout goes to the trace as console output; stderr (only ever the
    interpreter's own, e.g. a Rust panic) goes to the browser's log. One write sends at
    most 64 KiB, and one `random_get` fills at most 64 KiB.
  - Every pointer and length the pack hands a shim is checked against its memory before
    the host reads or allocates anything.
  - `environ_*` and `args_*` report empty lists. `sched_yield` does nothing,
    `poll_oneoff` fails, and `proc_exit` traps.
  - **Files:** `fd_prestat_get` fails, so WASI libc sees no preopened directory and can
    resolve no path. `path_open`, `fd_read`, `fd_seek` and the other file calls fail with
    EBADF.
  - There are no sockets: WASI preview 1 has no socket calls, and none are provided.

  An import outside this list fails instantiation. JS imports 8 of these functions; Python
  imports 21 (RustPython's standard Rust library links the file calls even though nothing
  can succeed).

## DOM bindings: the host operations

`host::Host` is the whole interface:

| Operation | Web meaning |
|---|---|
| `global(name)` | `globalThis.<name>`: `window`, `document`, `Math`, `console` |
| `get(target, prop)` | `target.prop` |
| `set(target, prop, value)` | `target.prop = value` |
| `call(target, method, args)` | `target.method(...args)` |
| `console(text)` | output with no console object (Dart's `print`, a pack's stdout) |
| `now()` | `Date.now()` |
| `clock(id)` | an interpreter's WASI clock: real time, or monotonic from the page's start |
| `random_u64()` | an interpreter's WASI random bytes |
| `object(fields)`, `array(items)` | a plain object or array literal of data (`{method: "POST"}`) |
| `is_array(value)` | `value instanceof Array` (dart2wasm asks) |

Every nondeterministic value a pack reads (`now`, `Math.random`, `clock`, `random_u64`)
passes through the page's `Tape` (`replay.rs`), so a recording can replay it. A pack must
not read time or randomness any other way.

- **Names** are Web IDL's (`querySelector`, `textContent`, `addEventListener`), so Dart's
  `package:web`, JS and Python all use the same ones. A member the host doesn't implement
  fails with "`Element.foo` is not supported yet". "Web platform parity" in DESIGN.md lists
  what's missing.
- **Values** are `host::HostValue`:
  - `Str`, `Num` (always a double), `Bool`, `Null`, `Undefined`;
  - `Object(handle)` for a web object;
  - `CallbackId(id)` for a function the page passed;
  - `Opaque` for a Dart object passed through untouched.
- **Interpreter wire format:** `host.request` carries JSON
  `{"op": "global"|"get"|"set"|"call"|"console", "target": handle, "name": ..., "value": ...,
  "args": [...]}`. The reply is `{"ok": value}` or `{"err": message}`. Values are JSON
  scalars, plus three tagged objects:
  - `{"u": 1}` is undefined;
  - `{"o": handle}` is a web object;
  - `{"cb": id}` is a callback;
  - `{"obj": {name: value}}` is a plain object of data, which the host turns into a host
    object (`Host::object`).

  Arrays can't cross from an interpreter yet.
- **Promises:** host methods that return a promise (`fetch`, `Response.text`, `json`) return
  a host `Promise` object with `then(onFulfilled, onRejected)`. Its callbacks run later, as
  zero-delay timers. The JS prelude wraps them in real Promises; Python's in a `Promise` class
  with `then`; Dart's `toDart` calls `then` itself.
- **What a page reaches beyond its DOM** comes in `PageEnv`, which the loader passes to each
  pack's `start`: its gate (for `fetch`), and its tab. Tests and replays pass the default:
  no network, no tab.
- **Wrapping:** each interpreter's prelude (`packs/js/src/prelude.js`,
  `packs/python/src/prelude.py`) wraps handles so page code reads naturally: a JS `Proxy`, or a
  Python `HostObject` with `__getattr__`/`__setattr__`.
  - The prelude's `METHODS` list says which member names are methods (called), as opposed
    to properties (read). Keep it in step with `host.rs`.
  - The prelude also defines the globals: `window`, `document`, `console`, the timer
    functions, `Math.random` for JS and `Math` for Python.

## Callbacks and their lifetimes

- **Ids:** the pack gives each function it passes to the host an id. The same function
  always gets the same id, so `removeEventListener` works, and ids are never reused.
  - **Dart:** the slot index of the `.toJS` trampoline and closure.
  - **JS and Python:** the prelude's counter; maps go id → function and function → id.
- **Who refers to an id:** the host refers to an id from the page's listeners
  (`Objects::listeners`, by node and event type) and timers.
  - When nothing refers to an id any more (a listener removed, a one-shot timer fired or
    cleared), the host queues it.
  - After every dispatch, timer batch and eval, the document calls
    `PackRuntime::release_callback` for each queued id. The pack then drops its reference
    to the function.
- **Calling:** `fire_callback(id, args)` calls a listener with one event object, a timer with
  nothing, and `requestAnimationFrame` with a timestamp. Python is strict about argument
  counts, so a Python listener must take the event (`def on_click(e)`).

## Memory ownership

- **The host owns:**
  - the DOM (`Rc<RefCell<BaseDocument>>`);
  - every web object (`Objects`: handles → `Obj`), listeners and timers.
- **A pack holds handles, never nodes:**
  - Element handles live as long as the page; one per node, so the same element compares
    equal.
  - Event objects are released when their dispatch ends.
  - Handles are never reused, so a stale one fails ("no longer available") instead of
    aliasing.
- **The pack owns everything in its `Store`:**
  - its linear memory or GC heap;
  - its callbacks;
  - its wrappers. JS holds them in a `WeakRef` map. Python creates one per lookup and
    compares by handle (`==`, not `is`), so none outlive their use.
- **Strings across the interpreter ABI:**
  - The host allocates strings it passes in with the pack's `<prefix>_alloc`, and the pack
    frees them.
  - Strings the pack returns (a packed `ptr << 32 | len`) are read and then freed by the
    host with `<prefix>_free`.
  - `host.request`'s reply is allocated by the host with `<prefix>_alloc` and freed by the
    pack. Its request buffer stays the pack's.
- **Dart:** values travel as externrefs holding `HostValue`s; Dart objects the host keeps
  are `OwnedRooted` (rooted until the host drops them).

## Errors and stacks

- **What a pack returns:** a call that fails returns an `anyhow::Error` whose text is ready
  to read: the error's type and message, then its stack in source terms.
  - **Dart:** `Uncaught Dart error: Bad state: boom` then `at throws.dart:9:3`. This comes
    from Dart's own `toString` (via `build.sh`'s entry point) and the embedded source map.
  - **JS:** `Uncaught TypeError: ...` then `at inner (app.js:1:25)`, from Boa.
  - **Python:** `Uncaught ValueError: boom`, then Python's traceback
    (`File "app.py", line 2, in inner`).
    - The prelude's frames are left out.
    - Runaway recursion is Python's `RecursionError`, its repeated frames summarised.
    - A `SyntaxError` shows the line and a caret.
- **Errors the page can catch:** a failed host call raises in the page's own language (a JS
  `Error`; a Python `RuntimeError`, or `AttributeError` for a property read, so `hasattr`
  and `getattr(x, name, default)` work).
- **What the document does with an uncaught error:**
  - traces it under the pack's name, with what was running (`click listener on button#go`);
  - with pause on exceptions on, it keeps a snapshot and pauses the page.

## The time limit and interruption

- **The limit:** every pack runs on `page_document::engine()`, which has epoch
  interruption. A clock thread ticks it every 100 ms.
  - Before **every** call into the pack, the pack sets its store's deadline to
    `TIME_LIMIT_TICKS` (about 5 s; 2 s in tests). This includes scripts, listeners, timers,
    eval, imports and Dart's `main`.
  - The interpreters do it in `Interpreter::enter`, Dart in `DartRuntime::enter`.
- **When a call runs over:** wasmtime traps with `Trap::Interrupt` (`interrupted(err)`).
  - The pack reports `script ran too long and was stopped (<Language>)` and sets `stopped`.
  - From then on it refuses every call with `STOPPED` ("this page's script was stopped (it
    ran too long or crashed); reload the page to run it again"). The interpreter or Dart
    runtime was interrupted mid-update, so it can't safely run again.
- **A call that traps** is treated the same way: a guest panic, a stack overflow, or
  running out of memory.
  - An interpreter's call that doesn't return normally sets `stopped` and reports `the
    <Language> pack crashed (<trap>); reload the page to run it again`. The wasm backtrace
    goes to the browser's log, not the page's console.
  - Dart does the same for traps, but not for Dart exceptions, which unwind normally.
  - So a crash gives one error, not one per timer tick.
- **What the document does:** it treats a stopped pack as idle: no events, timers or input.
  Reloading runs a fresh instance.

## Pause and eval

- **Pause is the document's:** a paused page gets no listener calls or timers until
  "Resume". Blitz's default actions still happen. The pack is not involved.
- **Eval:** `PackRuntime::eval(source)` evaluates console input in the page's running
  program and returns its result formatted to read.
  - **JS:** the page's realm, formatted by the prelude's `__inspect`.
  - **Python:** the page's `__main__` namespace. The last statement's value is shown with
    `repr`, so an assignment shows `None`.
  - **Dart:** `can_eval` is false, because a page ships no compiler. The Inspector says so
    instead of showing an input line.
- **Caller attribution:** while eval runs, `Objects::caller` traces host calls and logs as
  the caller's ("console" or "ai"), so the AI's redacted views leave the user's console
  input out.
- **AI control's `eval`** uses the same path, for JS and Python pages.

## Shutdown

- **There is no shutdown call.** Closing a tab or navigating drops the `PackDocument`, which
  drops:
  - the `RunningPack`;
  - its runtime and with it the wasmtime `Store`: the instance's memory and every pack
    object;
  - the `PageHost`, `Objects` and DOM.
- **The timer thread** exits when its channel's sender goes with the `RunningPack`.
- **The browser's tools** hold the page only weakly (`PageRef`), so they don't keep it alive.
  `a_dropped_page_releases_its_pack_and_dom` checks this for all three packs.
- **A pack must not hold anything outside its store:** no threads, files or global state in
  the host.
- **What outlives pages:** only compiled code (`Module`s, and their `.cwasm` files), which is
  immutable. The interpreters' is shared by every page; a Dart program's only by pages of
  the same site in the same profile.

## Isolation

- Each page has its own `Store`, `PageHost` and `Objects`. Two pages, even in the same
  language, share nothing but compiled code. `each_page_gets_a_fresh_instance` checks this.
- A page runs one pack, so packs on one page can't see each other.
- No pack reaches the network except through the host: `fetch` is a host call that goes
  through the page's gate (third-party rules, logging, block-all). There is no
  `XMLHttpRequest` or socket. Requests a page causes (scripts, imports, Dart programs) go
  through the gate too.
- No pack can reach files, the environment or processes. `js_has_no_side_doors` and
  `python_has_no_side_doors` check this.

## Adding a language

**An interpreter** that compiles to `wasm32-wasip1`:

1. **Build it.** Create a crate under `packs/<lang>` that exports the interpreter ABI below
   under a new prefix and imports only `host.request` and the WASI functions above.
2. **Write a prelude** in the language that wraps handles and defines the globals.
3. **Register it with the browser:**
   - a `Language` static and a `start` in `browser/src/<lang>_pack.rs` (about 30 lines; see
     `python_pack.rs`);
   - a script type and a branch in `document_loader.rs`;
   - a per-site block list in `settings.rs` and a row in `site_panel.rs`;
   - its display name in `inspector.rs`'s `language`.

**A compiled language** needs its own adapter, like `dart_pack.rs`: something that maps its
Wasm's imports onto `Host`. It also needs its own `PackRuntime` implementation, since its
callbacks are its own.

**The interpreter ABI** (`<p>` is the prefix; strings are UTF-8; `packed` is `ptr << 32 | len`):

| Export | Meaning |
|---|---|
| `<p>_alloc(len) -> ptr`, `<p>_free(ptr, len)` | memory for strings crossing the boundary |
| `<p>_run(src, src_len, name, name_len) -> status` | run a page script; 0, or a packed error message |
| `<p>_fire(id, args, args_len) -> status` | call callback `id` with a JSON array of values |
| `<p>_release(id)` | forget callback `id` |
| `<p>_eval(src, src_len) -> packed` | console input; packed JSON `{"ok": text}` or `{"err": message}` |
| `memory` | the linear memory |
| `_initialize` (optional) | the wasip1 reactor start |

The pack imports `host.request(ptr, len) -> packed`, which takes a JSON request and returns a
packed JSON reply.

## Python pack

- **Interpreter:** RustPython 0.6 (`rustpython-vm` with `compiler`, `gc` and `serde` only).
  It has no `host_env` (files, sockets, ctypes), no `wasm-bindgen` and no stdlib.
  - Built with `packs/python/build.sh`.
  - The five crates of RustPython's ruff fork are pinned to 0.16.5. Their 0.16.10 patch
    release changed the AST and breaks `rustpython-codegen` 0.6.0.
- **Size and speed** (this Mac, release build):
  - `python_pack.wasm` is 13.4 MB, against Boa's 5.4 MB. Compiled and cached, it's 28 MB.
  - Compiling it the first time takes about 0.5 s. After that, starting a page's
    interpreter takes about 25 ms (about 55 ms for the first page in a process, which loads
    the cached code).
- **Pages:**
  - `<script type="text/python">` inline or with `src`. The loader dedents inline scripts
    (`python_pack::dedent`), so they can be indented to match the HTML; line numbers are
    kept. A `.py` file from `src` is used as it is.
  - `time.sleep` raises `RuntimeError: time.sleep would block the page; use setTimeout`.
    The page runs on the UI thread, and the WASI shims have no way to wait.
  - `sys.setrecursionlimit` is capped at 3,000. Deeper recursion would overflow the Wasm
    stack (about 5,000 simple frames fit), which crashes the pack. The default stays 1,000.
  - The globals are `window`, `document`, `console`, `Math`, `setTimeout`, `setInterval`,
    `clearTimeout`, `clearInterval`, `requestAnimationFrame` and `cancelAnimationFrame`.
    `print` goes to the console.
  - Members use Web IDL's camelCase names (`document.querySelector`, `el.textContent`).
  - Numbers from the host that are whole come back as `int`, so `e.clientX` prints `10`,
    not `10.0`.
  - The demo is `examples/python-counter`.
- **Not supported yet:**
  - The standard library (`json`, `math`, `random`, `re`...). RustPython's native stdlib
    crate, or its frozen pure-Python `Lib`, would add both size and surface; it's
    deferred until a page needs it.
  - Importing other page files.
  - `async`/`await` with the browser's event loop.
  - Host object identity with `is` (use `==`).
  - Passing lists or dicts to the host (as for JS).
  - Snake_case aliases for Web IDL names.
