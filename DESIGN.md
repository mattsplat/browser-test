# Browser experiment: design

An experimental browser where JavaScript is no longer the default language of the web.
Dart is first-class; JS is just one more language pack, off by default.

Status: experiment. Phases 0–3 built in `browser/` (started from Blitz's reference
browser): the gate, the Dart pack (with dev mode and source maps), the JS pack, and the
per-site policy screen. Run it with `cd browser && cargo run --release -- <url>`. Dart demo:
`packs/dart/build.sh examples/counter/main.dart`, then open
`file://<repo>/examples/counter/index.html`.

## Principles

1. **JS gets nothing extra.** It is a pack like any other, off by default. A parity test suite
   enforces this: a feature that works for JS but not Dart does not ship.
2. **Existing pages still render.** HTML/CSS render with no scripting at all. Pages that need
   JS can run the JS pack if the user allows it for that site.
3. **One host dispatcher.** Every pack, and the AI interface, reaches the DOM, network, clock
   and randomness only through host functions. Nothing has a side door.
4. **No input bypasses the host.** Time, randomness, events and network responses all come
   through host calls. This is what makes the network chokepoint, the tracer and
   time-travel replay possible, so it is a hard rule from day one.
5. **Source is primary, Wasm is how it runs.** The console, debugger and stack traces show
   the real source, never raw Wasm.
6. **Wasm can do everything JS can do on the web.** Anything a page's JS can do in a normal
   browser, a Wasm pack (e.g. Dart via `package:web`) can do here, through the same host and
   subject to the same policy (the gate, per-site settings). Principle 1 stops JS getting
   more; this one stops Wasm getting less. See "Web platform parity" for what's missing.

## Architecture

```
┌─────────────────────────────────────────────┐
│  Shell (tabs, URL bar, policy + dev UI)     │
├─────────────────────────────────────────────┤
│  Blitz: HTML/CSS parse → style → layout →   │
│  paint. No scripting.                       │
├──────────────┬──────────────────────────────┤
│  DOM + Web   │  Language pack interface     │
│  API host    │  (host functions generated   │
│              │   from WebIDL)               │
│              ├──────┬──────┬───────┬────────┤
│              │ Dart │  JS  │Python │  ...   │
│              │ pack │ pack │ pack  │        │
├──────────────┴──────┴──────┴───────┴────────┤
│  Network chokepoint (Blitz NetProvider):    │
│  every request from loader and packs        │
└─────────────────────────────────────────────┘
```

### Rendering engine: Blitz

Chosen after a spike comparing Servo, Blitz and Gosub against Chrome on 12 real pages with
JS removed (see `spike/`, comparison images in `spike/out/compare/`).

- **Why Blitz:** its `NetProvider` trait is the only way it fetches anything, and we
  implement it, so blocking is the default rather than an add-on. It has no JS engine to
  remove, builds without changes, and was the fastest (median 330 ms).
- **Known gaps:** layout bugs with floats (Wikipedia infobox), table spacing (Hacker News)
  and multi-column layouts (simonwillison.net sidebar missing).
- **Not chosen:** Servo (JS engine wired into its core; request hook only covers HTTP(S)
  and lets unhandled requests through; one page hung). Gosub (best layout and the closest
  design fit, but no way for an embedder to block requests, and its macOS build needed two
  config fixes).
- **Escape hatch:** if Blitz rendering holds the project back, revisit Gosub, starting with
  whether a request-blocking hook can be added to it.

### Language packs

- Every pack runs in Wasm. Compiled languages (Dart) ship their code as Wasm. For
  interpreted languages (JS, Python), the interpreter is compiled to Wasm and the page
  ships source.
- Packs reach the DOM only through host functions generated from WebIDL, using **handles**
  to DOM nodes. Packs never hold each other's objects; two languages on one page talk only
  through the DOM and events. Handles are reclaimed on navigation.
- Pages declare a pack by script type, e.g. `<script type="application/dart" src="app.wasm">`.
  A plain `<script>` goes to the JS pack, which runs only if the site's policy allows JS.
- Reference for wiring a script engine to Blitz's DOM: `blitz-vibey-script` (Boa-based JS)
  in the Blitz repo.

### Network chokepoint

All requests go through one Rust layer wrapping Blitz's `NetProvider`:
- **Block-all mode** (nothing leaves the machine)
- Default-deny for third-party domains, with a per-site allowlist
- A live request log, including blocked requests
- A per-site choice of which packs may run

The spike showed why this has to be the enforcement point: even with `<script>` tags
removed, pages still try to fetch JS through preload links (112 attempts on GitHub).

## Dev mode

- **Global dev setting, off by default.** Off means source scripts are never compiled and
  never prompt; the page runs only precompiled Wasm, and the console shows one line saying a
  source script was skipped.
- **On:** the browser asks once per origin before compiling source (Allow for this site /
  Allow once / Block). Works with Herd's `.test` domains; there is no localhost special case.
- A granted site gets compile-on-load, hot reload and AI write actions. Grants are listed
  and revocable in settings; turning dev mode off disables them without deleting them.
- The prompt is browser UI, never page content. A blocked site stays blocked until changed
  in settings.
- Compilation uses the **locally installed Dart SDK**, sandboxed with no network. The
  compiler is not bundled with the browser.
- Production pages ship precompiled Wasm, optionally with source or source maps for the
  console.

## Hot reload

1. Start with **hot restart**: recompile and restart, losing state.
2. Aim for **reload by replay**: recompile, start a fresh instance, and replay the recorded
   host inputs to rebuild state. Fall back to a restart if the replay diverges.

Running the native Dart VM in dev mode (for Dart's own hot reload) was considered and
dropped: it would bypass the network chokepoint and break replay.

## Debugging tools

- **Source-level debugging** through the Debug Adapter Protocol: one adapter per pack, so the
  built-in dev tools and VS Code both work.
- **Boundary tracer:** every host call logged with pack, arguments, timing and source line.
  Comes almost for free from the single dispatcher.
- **Time-travel replay:** record every host call result; replay a session exactly, step
  backwards, share recordings as bug reports.
- **Handle and heap inspector:** which pack holds which DOM nodes; WasmGC objects shown with
  field names.
- **Profiler:** flame graph per pack, with time spent crossing into the host shown separately.

## AI access

A built-in MCP server so an agent can read and debug the browser: DOM and accessibility
tree, screenshots, console, boundary trace, network log, breakpoints, recordings.

- Off by default, local socket only, requires a token
- Read-only, except write actions (eval, reload, input) on sites with a dev-mode grant
- Never sees cookies, passwords or payment fields
- Its calls appear in the tracer like any other client

## Proposed (not yet decided)

- **Shell language:** Rust (proposed), versus a Flutter UI over the Rust engine.
- **Wasm engine for packs:** wasmtime if its WasmGC support holds up in a spike; otherwise V8
  used purely as a Wasm engine, never exposed as JS.
- **Enabling JS:** a per-site "this page needs JS" prompt, or settings only. A prompt makes
  JS easier to turn on, which may conflict with principle 1.
- **AI toggle:** its own setting (proposed), separate from dev mode, so an AI can read
  production sites without enabling compilation.
- **Herd shortcut:** an "allow all `*.test`" dev-mode rule. Left out unless per-site prompts
  get annoying.

## Unverified assumptions

- dart2wasm source map quality
- dart2wasm producing identical output from identical input (needed for "verified source")

## Verified: Dart runs in wasmtime with no JS engine

Spike in `spike/dart-host` (wasmtime 50, Dart 3.13.7):
- dart2wasm emits **legacy** exception instructions, which wasmtime does not implement. Fix:
  run the SDK's own `wasm-opt` with `--translate-to-exnref` and an explicit feature list
  (`--all-features` lets it emit "exact" reference types, which wasmtime rejects).
- Dart treats strings and JS values as opaque `externref`s. The host supplies 11
  `wasm:js-string` operations and string constants (import field name = the string).
- `@JS('host.name')` externals compile to `globalThis.host.name(...)` snippets in the
  generated `.mjs`, so one rule routes every host call to the dispatcher. The `.mjs` is
  read as an import table only, never executed.
- Small ints cross as `i31ref`; Dart objects cross as opaque references; callbacks arrive
  as a trampoline plus closure the host can call later.

## Phases

| Phase | Goal | Proves |
|---|---|---|
| 0 | Blitz shell that renders a URL, plus the network chokepoint with block-all mode and a live request log | Existing pages render; security works from day one |
| 1 | Pack interface, host dispatcher with the boundary tracer, and a Dart pack with a minimal DOM (query, create, text, events) | Dart runs as a first-class language |
| 2 | JS pack (Boa compiled to Wasm) on the same interface | JS is "just a pack" |
| 3 | Per-site policy UI: allowed packs and allowed domains | The security and language model works end to end |
| Later | Python pack, reload by replay, shared runtime cache, MCP server | |

## Feature list

Candidate features beyond the phases. Value and complexity are as given; the notes say how
each relates to what's built.

| Feature | Value to users | Engineering complexity | Notes |
|---|---|---|---|
| Vertical tabs | High | Low–medium | Browser UI only; the tab strip is Dioxus in `browser/src/tab_strip.rs` |
| Spaces and profiles | Very high | Medium–high | Needs separate cookies, storage, history and settings per profile; today all of these are global |
| Command palette | High | Low | Browser UI only; could reuse the URL bar's suggestion list (`url_suggestions.rs`) |
| Split View | High | Medium–high | Two page documents side by side in one window; each already gets its own gate view and pack |
| Automatic tab management | High | Low–medium | Browser UI over the existing tab store (e.g. grouping, closing idle tabs) |
| Website CSS customization | Medium | Medium | Per-site user stylesheets; would sit alongside the Site panel's per-site settings |
| Rust/WASM scripting | Specialized | Very high | Partly built: the Dart pack already runs Wasm on the host dispatcher. A Rust pack would need its own way to describe host calls (Rust has no dart2wasm-style import table) |
| `document.cookie` for Wasm and JS | High | Low–medium | See "Web platform parity": expose the existing cookie jar per origin, hiding `HttpOnly` cookies |
| Replaceable scripting engine | Specialized | Extremely high | Largely built: language packs (Dart, JS) behind one `Host` interface and `PackRuntime` trait. Swapping or adding an engine is a new pack, not an engine change |

## Web platform parity

What JS can do in a normal browser that the packs can't do here yet (principle 6). Each is
implemented once in the host (`browser/src/host.rs`), so Dart and JS get it together.

| Capability | Status | Notes |
|---|---|---|
| `document.cookie` (read and write) | Missing | The network layer already keeps a cookie jar; expose it per page origin, hiding `HttpOnly` cookies from page code as browsers do, and applying third-party rules |
| Timers: `setTimeout`, `setInterval`, `requestAnimationFrame` | Missing | Needs the event loop to wake the pack; time comes from the host (principle 4) |
| Network from page code: `fetch`, `XMLHttpRequest`, `WebSocket` | Missing | Must go through the gate with the page's URL (`fetch_for_page`), so third-party rules apply |
| Storage: `localStorage`, `sessionStorage`, IndexedDB | Missing | Per origin, and per profile once profiles exist |
| Events: `removeEventListener`, mouse/keyboard details, `preventDefault`, input/change on form fields | Partly | Listeners and `event.type`/`target` work; details and form events don't |
| DOM breadth: `querySelectorAll`, `classList`, `style`, `innerHTML`, `children`/`parentElement`, form `value` | Partly | `querySelector`, create/append/remove, `textContent`, attributes, `id`, `className` work |
| `window` and `location`: URL, navigation, `history`, viewport size | Missing | |
| `console.*` with objects, `print` to a console view | Partly | Both packs log to the Trace panel as text |
| Canvas, WebGL, audio, media | Missing | Large; depends on what Blitz can render |

## Phase 0 notes

- `browser/src/gate.rs` is the network chokepoint. It replaces the real provider everywhere
  the app used it (page loads, subresources, favicons). Its tests cover the block-all rule.
- `browser/src/request_panel.rs` is the Requests panel (toolbar button): the request log and
  the block-all switch.
- The Boa-based JS path from the original app was removed; scripts come back as packs.
- The browser UI's own CSS and icons load from `browser/assets/` via `file://` URLs
  (`local_asset!`), because Dioxus's `asset!` needs the Dioxus CLI. These UI files are not
  page traffic, so they bypass the gate.

## Phase 1 notes

- `browser/src/dart_pack.rs`: the Dart runtime (wasmtime). Knows nothing about the DOM; it
  talks to the page only through the `Host` trait.
- `browser/src/host.rs`: the host dispatcher (`PageHost`) and the boundary tracer. Packs hold
  small integer **handles** to typed web objects (`Document`, `Element`, `Event`, `Math`),
  never Blitz node ids. The host API is the web platform's own names: `global`, `get`, `set`
  and `call` (e.g. `Document.querySelector`, `Element.textContent =`). Unimplemented members
  fail with "`Element.foo` is not supported yet".
- `browser/src/dart_document.rs`: delivers DOM events to `addEventListener` listeners, in
  bubbling order, each with an event object.
- `browser/src/trace_panel.rs`: the Trace panel (toolbar button) showing every host call.
- **Dart pages use the standard `package:web` API.** Its generated bindings compile to four
  snippet shapes (read a global, call a method, get or set a property), which the pack routes to
  the host by name, so the API surface comes from WebIDL without writing a generator. Supported
  so far: `document`, `Math.random`, `querySelector`, `createElement`, `appendChild`, `remove`,
  `textContent`, `id`, `className`, `tagName`, `body`, attributes, `addEventListener`
  (any event type Blitz reports), `event.type`, `event.target`. Dart streams work through a
  microtask queue the pack drains after every entry into Dart.
- `packs/dart/build.sh`: compile plus the exnref step.
- Pages opt in with `<script type="application/dart" src="app.wasm">`, fetched through the
  gate. A page ships only the `.wasm`: `build.sh` moves dart2wasm's import table into a
  `dart2wasm-imports` custom section (`packs/dart/embed_imports.dart`) and deletes the
  generated `.mjs` and `.support.js`.
- Known gaps: no `removeEventListener`; event objects are never freed (one handle per
  delivery); no mouse/keyboard event details; Dart's `print` goes to the trace, not a console view.

## Step 3 notes: dev mode

- Menu → **Dev mode** toggles it (saved in `settings.json` in the app's config folder, along
  with per-site grants). History moved to the app's own folder too.
- A page opts in by pointing its Dart script at source: `<script type="application/dart"
  src="main.dart">` (see `examples/counter/dev.html`).
- Rules (`dev_mode_decision` in `document_loader.rs`, tested): off → never compile, never ask;
  remembered Block → skip; remembered Allow → compile; no answer yet → the prompt bar asks
  (Allow for this site / Allow once / Block). Skips are recorded in the trace.
- `dev_compile.rs` runs `packs/dart/build.sh` in `sandbox-exec` with outbound network denied,
  using the local Dart SDK. `file://` pages compile in their own folder (multi-file apps and
  their pubspec work); `http(s)` pages compile as a single file fetched through the gate.
  Output is cached and rebuilt when any `.dart` file or the pubspec is newer, so editing
  and reloading is the "hot restart" level from the design.
- Compile errors are shown on the page. Packages resolve with `dart pub get --offline`, so a
  project needs `dart pub get` run once with network.
- Not yet: stateful hot reload; multi-file apps over http.

## Step 4 notes: source maps

- `build.sh` passes dart2wasm's source map through `wasm-opt` (so offsets match the rewritten
  binary) and embeds it as a `dart2wasm-sourcemap` custom section. Pages still ship one file.
- `browser/src/source_map.rs` decodes it; `dart_pack.rs` uses it in two places:
  - **Dart's own stack traces** (`StackTrace.current`, and the trace captured when an error is
    thrown): the stack helper returns the wasmtime backtrace mapped to Dart lines, shaped like
    a JS engine's `Error().stack` so dart2wasm's trimming works unchanged.
  - **Uncaught errors** out of `main`, event listeners or microtasks: the message strings held
    by the thrown Dart error, then its stack, e.g.
    `Uncaught Dart error: boom from a click` / `at throws.dart:9:3`. They appear in the Trace
    panel (listeners) or on the page (`main`).
- The message is best effort: the host collects strings inside the error object, since it
  can't call Dart's `toString`, so the class name (e.g. `StateError`) isn't shown.
- Compiler-generated wrappers have no source and show as `wasm-function[N]:0x...`.

## Step 5 notes: the JS pack (Phase 2)

- **Engine:** Boa, compiled to `wasm32-wasip1` from `packs/js` (`packs/js/build.sh`, ~5 MB).
  It runs in wasmtime with the **same `Host`** as the Dart pack, so JS gets the same web API
  names and nothing more (principle 1). Compiled engine code is cached on disk (first JS page
  ~7 s once, then ~1 s).
- **Bridge:** one import, `host.request`, carrying JSON (`global`/`get`/`set`/`call`/`console`).
  `packs/js/src/prelude.js` wraps host handles in Proxies so pages write ordinary JS
  (`document.querySelector('#x').textContent = ...`, `addEventListener`).
- **No side doors:** the engine's only other imports are 8 WASI functions, implemented in
  `js_pack.rs` (not a WASI runtime): clock and randomness come from the host, there is no
  environment, no files, no network. `fetch`, `XMLHttpRequest`, `require` etc. don't exist
  (tested). `Math.random` goes through the host, as for Dart.
- **Off by default:** a page's classic `<script>`s (inline or `src`, fetched through the gate)
  run only if its origin is in `js_sites`; otherwise the trace says how many were skipped.
  One script failing doesn't stop the next. Module scripts are not supported yet.
- `page_document.rs` (was `dart_document.rs`) serves both packs through a small
  `PackRuntime` trait, so event delivery is identical for Dart and JS.
- Known gaps: the prelude needs a list of which member names are methods (kept in step with
  `host.rs`), so calling an unknown method reports it as an unknown property; no timers
  (`setTimeout`), no `window`; a page runs one pack (Dart wins if both are present).

## Step 6 notes: per-site policy (Phase 3)

- **Site** panel (toolbar) for the page in the active tab: Dart allowed/blocked, JavaScript
  off/allowed, the dev-mode grant (with "Ask again"), and every third-party site the page
  tried to reach, each with Allow. Changes apply on reload ("Reload to apply").
- **Third-party requests are blocked by default**, as designed: a page's requests to another
  site (registrable domain via the public suffix list, so `cdn.example.com` is first-party on
  `www.example.com`) are refused unless allowed for that page's origin. A global switch in
  the panel turns the rule off. Navigations are never third-party; `file:` pages and local
  schemes are exempt.
- The gate now knows which page asked: each page document gets a `PageGate` carrying its
  URL, and scripts, wasm and dev-mode source are fetched with `fetch_for_page`. The
  Requests panel shows why a request was blocked.
- **Cost of default-deny:** many sites load images, fonts or CSS from another domain (BBC
  News from `bbci.co.uk`, Wikipedia images from `wikimedia.org`), so they render with gaps
  until those sites are allowed in the Site panel. Turn the global switch off to browse
  without the rule.
- Policy logic is a pure function (`gate::decide`), tested without touching saved settings.

## Known local setup notes

- Building cloned repos under `vendor/` needs your approval each time (auto mode blocks it).
- Intel Homebrew Python on this Apple Silicon Mac breaks native builds that call `xcrun`;
  use `/usr/bin/python3` or an arm64 Python.
- Your Herd apps (`vector-workflow.test`, `pallet-pal.test`) are Inertia apps that render
  blank without JS; they need the JS pack or Inertia server-side rendering.
