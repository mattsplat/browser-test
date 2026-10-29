# Browser experiment: design

An experimental browser where JavaScript is no longer the default language of the web.
Dart is first-class; JS is just one more language pack, off by default.

Status: experiment. Current plan: "Roadmap (2026-10-28)". Phases 0–3 built in `browser/` (started from Blitz's reference
browser): the gate, the Dart pack (with dev mode and source maps), the JS pack, and the
per-site policy screen. Phase A is done, Phase B is done apart from breakpoints and DAP
(the Inspector, AI access and AI control; see "Step 7 notes"), and Phase C is done apart
from bookmarks (see "Phase C notes"). Phase D is done: the pack contract is written down in
`docs/packs.md` and Python is a third pack (see "Phase D notes"). Run it with `cd browser && cargo run --release -- <url>`. Dart demo:
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
- **What a pack implements and what the host guarantees** (loading, imports, DOM bindings,
  callbacks, memory, errors, the time limit, pause and eval, shutdown, isolation) is in
  `docs/packs.md`. As built, a page runs one pack: the first of Dart, Python and JS.

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
  (built as "AI control": eval, click, reload; see "Step 7 notes")
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

The original phases below are done. The current plan is "Roadmap (2026-10-28)" after this
table.

| Phase | Goal | Proves |
|---|---|---|
| 0 | Blitz shell that renders a URL, plus the network chokepoint with block-all mode and a live request log | Existing pages render; security works from day one |
| 1 | Pack interface, host dispatcher with the boundary tracer, and a Dart pack with a minimal DOM (query, create, text, events) | Dart runs as a first-class language |
| 2 | JS pack (Boa compiled to Wasm) on the same interface | JS is "just a pack" |
| 3 | Per-site policy UI: allowed packs and allowed domains | The security and language model works end to end |
| Later | Python pack, reload by replay, shared runtime cache, MCP server | |

## Roadmap (2026-10-28)

Adopted from a plan drafted with ChatGPT, checked against the code and corrected where it
didn't match how things work. Corrections are marked **Correction**; open questions are
marked **Decision needed**.

### Architecture

- Many languages, one browser API. JS stays for compatibility but isn't privileged; Wasm is
  a first-class execution environment. Packs handle language runtimes, never duplicate
  browser functionality, and adding a language doesn't touch the rendering engine. (Already
  true: Blitz is unmodified; Dart and JS share `Host`.)
- Web IDL is the reference for DOM behaviour. (Already true in practice: Dart uses
  `package:web`, generated from Web IDL, and the host uses the same member names for JS.)
- **Correction (WIT):** WIT / the component model can't be *the* common host interface,
  because dart2wasm can't produce components: it emits core Wasm whose imports are JS
  snippets. The common interface stays the internal `Host` (global/get/set/call with Web
  IDL names). WIT can be one binding *onto* it for languages that compile to components
  (Rust, Python via componentize-py).
- Don't compromise JS compatibility to promote other languages.

### Order

**Phase A: reliability and compatibility (now)**

Done so far (`page_document.rs` cross-pack test `events_and_timers_behave_the_same_in_dart_and_js`
runs the same page in Dart and JS and expects identical output):
- Timers (`setTimeout`, `setInterval` (clamped to ≥ 4 ms), `clearTimeout`/`clearInterval`,
  `requestAnimationFrame`/`cancelAnimationFrame`) live in the host; the page document runs due
  timers on `poll` and wakes the event loop at the next deadline.
- Listener identity: a function has one callback id for its lifetime, so `removeEventListener`
  works (Dart's `StreamSubscription.cancel` too); duplicate `addEventListener` is ignored.
- Event objects carry mouse (`clientX`/`Y`, `pageX`/`Y`, `screenX`/`Y`, `button`, `buttons`)
  and keyboard (`key`, `code`, `repeat`) details plus modifier keys, `currentTarget` and
  `defaultPrevented`; `preventDefault` cancels Blitz's default action; `stopPropagation`
  and `stopImmediatePropagation` work.
- Leak fixed: one event object per dispatch, released when it ends; handles are never reused
  (a stale one errors instead of aliasing); callbacks nothing refers to are released in the
  pack; the JS prelude holds wrappers weakly.
- `window` and `console` (log/info/warn/error/debug) globals; `Date.now` from the host.
- Error reporting with types, messages and stacks in both packs:
  - **Dart:** `build.sh` compiles through a generated entry point (`.pack_entry_<name>.dart`,
    deleted after) that registers a function formatting errors with Dart's own `toString`
    (via a `globalThis` assignment; dart2wasm forbids `wasm:export` in user code). Uncaught
    errors read `Uncaught Dart error: Bad state: boom from a click` / `at throws.dart:9:3`.
    Pages built without it fall back to the strings inside the error object.
  - **JS:** Boa already reports the type, message and a stack with function names; scripts
    are now named (their URL, or `page (inline script N)`) so locations read `app.js:1:25`.
    `error.stack` works in page code.
  - The dev-mode compile cache now also goes stale when the build tooling changes.
- ES modules (`<script type="module">`, inline or `src`): classic scripts run first, then
  module scripts in document order. Imports resolve relative to the importing module and are
  fetched **through the gate** as requests from the page (so third-party rules apply). The
  host loads each graph before it runs: a scratch Boa context with no page access reports
  missing imports, the host fetches them, and repeats; then the page's context links and
  evaluates it, each module once. Not supported yet: import maps (bare specifiers like
  `import "lodash"` error clearly), `import()` from classic scripts. Demo:
  `examples/js-modules`.
- JS permission (`js_permission.rs`, rules tested in `decide`): a global default (**Ask**, Off
  or Allow; shipped as Ask), per-site "Always allow" and "Keep off" (saved), and "Allow this
  visit" (until the browser quits). With Ask, a page with scripts renders without JS at once
  and a non-blocking bar offers Allow this visit / Always allow / Keep off / ×; Keep off and ×
  stop it reappearing. The Site panel shows the site's state (cycle: default → always allowed
  → kept off) and the global default.
- Timers: `setTimeout`/`clearTimeout`/`setInterval`/`clearInterval` (plus
  `requestAnimationFrame`), host-owned so time comes from the host (principle 4).
- `removeEventListener` with correct listener identity and cleanup.
- Keyboard and mouse event properties.
- Fix the event-object leak: release handles when the pack no longer holds them.
- Error types, messages and stack traces. (Done: see above.)
- JS ES modules (`<script type="module">`, imports, execution order). (Done: see above.)
- JS permission prompt and default policy (see below).
- Tests show the same DOM behaviour across packs (one test, run against Dart and JS).

**Phase B: developer tooling and AI**
- Stabilize the Trace panel. **Done:** every entry records its page URL and kind (call,
  console, error, load); the panel shows the active tab's site by default (or all tabs) and
  filters to Console or Errors. Pack start failures now reach the trace, not only the page.
- Read-only MCP server (AI access), on the same inspection infrastructure as the dev tools.
  **Done (first version):** `browser/src/ai_access.rs`. Menu → "AI access" (off at every
  launch). MCP over HTTP at `http://127.0.0.1:7807/mcp`; a new token each time it's turned
  on, written with the URL to `ai-access.json` (mode 0600) next to settings.json. Requests
  with an `Origin` header or another `Host` are refused (web pages, DNS rebinding). Tools:
  `list_tabs`, `read_page` (outline; no form values, scripts or hidden inputs), `console`,
  `trace`, `requests`. URLs lose credentials, query values and fragments. A bar shows while
  it's on, with "Turn off"; AI calls appear in the trace as pack "ai".
- A shared internal debugging protocol, consumed by the browser UI and MCP; DAP for VS Code.
  **Done:** `browser/src/debug.rs`, used by the Inspector, the Trace panel and AI access.
  **Not done:** DAP (see "Step 7 notes" for what it needs).
- Inspector (Elements, Console, Exceptions; Network is the Requests panel). **Done**, see
  "Step 7 notes".
- **Correction (breakpoints):** real breakpoints are high risk here. wasmtime's guest
  debugging is limited and Boa has few debugger hooks. Start with source-mapped tracing and
  pause-on-exception, then breakpoints. **Done:** pause on exceptions. **Not done:**
  breakpoints; "Step 7 notes" says what they'd need.
- AI control (eval, click, reload) as a separate permission. **Done (first version)**, see
  "Step 7 notes".

**Phase C: browser productivity**
Command palette (keyboard navigation), vertical tabs, automatic tab management (pinned and
temporary tabs), spaces and profiles. Profiles get genuinely separate cookies, storage,
sessions and permissions; spaces only organise.
**Done**, see "Phase C notes". **Not done:** bookmarks (the page is still a stub, so the
palette has no bookmark results), and per-tab back/forward lists across restarts.

**Phase D: language extensibility**
Document exactly what a pack implements (initialisation, module loading, imports, DOM
bindings, callbacks, memory ownership, error reporting, shutdown), then add Python to prove
a third language needs no core changes. Validate isolation and lifecycle.
**Done**, see "Phase D notes" and `docs/packs.md`.

**Phase E: advanced**
Split View, per-site CSS (stored per site, no JS needed), hot reload and state restoration,
handle inspector and profiler, shared compiled-runtime cache, time-travel replay.
- **Correction (hot reload):** "hot module replacement where the runtime supports it" doesn't
  apply to Dart: dart2wasm has no HMR. For Dart it's restart plus state restoration.
- Replay must record timers, network responses, nondeterministic values and external events,
  not just user input (principle 4 already routes all of these through the host).
- Shared cache: share compiled modules and immutable assets only, never mutable state across
  origins. (Already the JS engine's model: one compiled module, a fresh instance per page.)

### JS permission

- When a page has scripts and JS isn't allowed, it renders without JS immediately and a
  **non-blocking** bar offers: allow for this visit, always allow for this site, keep off.
  (Unlike the dev-mode prompt, it never holds up the page.) Choices persist and are editable
  in the Site panel.
- A global default JS policy.
  **Decided:** ships as **Ask** (configurable: Off, Ask, Allow). "Ask" on every site with scripts would interrupt constantly, since
  almost every site has scripts, so the bar must be quiet and dismissible.
- **Correction (third-party):** JS permission is per *page* origin. Scripts from other sites
  are already governed by the third-party request rule, so allowing JS for a page never
  unblocks a third-party site that's blocked. That stays true.

### AI access (MCP)

- A separate permission from dev mode; off by default; read-only first (tabs, URLs, runtime
  info, DOM, console and exceptions, requests with secrets redacted, traces, packs and their
  status). Control (navigate, click, type, run code) later, as a separate permission.
- Never exposes cookies, authorization headers, passwords or credentials by default; visibly
  on and easy to revoke.
- **Correction (transport):** a GUI browser the user launches can't offer stdio to an agent
  directly. Use authenticated loopback (localhost + token), with an optional small stdio shim
  that connects to it, for MCP clients that only speak stdio.
- **Note:** the gate logs requests, not responses. Showing responses is new work.

### Manual verification

Before adding features: dev-mode prompts, every Site panel toggle, JS after permission,
reload and persistence, third-party script blocking, and representative real sites.
Automate where possible: event-level tests can drive pages headlessly; the browser's own
Dioxus UI is harder to drive.

## Feature list

Candidate features beyond the phases. Value and complexity are as given; the notes say how
each relates to what's built.

| Feature | Value to users | Engineering complexity | Notes |
|---|---|---|---|
| Vertical tabs | High | Low–medium | **Done** (Phase C): Settings → Tab layout; `tab_strip.rs` |
| Spaces and profiles | Very high | Medium–high | **Done** (Phase C): spaces in `tab_manager.rs`; a profile is its own data folder and process (`profile.rs`) |
| Command palette | High | Low | **Done** (Phase C): `command_palette.rs`, Cmd+K / Cmd+Shift+P |
| Split View | High | Medium–high | Two page documents side by side in one window; each already gets its own gate view and pack |
| Automatic tab management | High | Low–medium | **Done** (Phase C): pinned tabs, closing idle tabs, reopen closed, session restore (`tab_manager.rs`) |
| Website CSS customization | Medium | Medium | Per-site user stylesheets; would sit alongside the Site panel's per-site settings |
| Rust/WASM scripting | Specialized | Very high | Partly built: the Dart pack already runs Wasm on the host dispatcher. A Rust pack would need its own way to describe host calls (Rust has no dart2wasm-style import table) |
| `document.cookie` for Wasm and JS | High | Low–medium | See "Web platform parity": expose the existing cookie jar per origin, hiding `HttpOnly` cookies |
| Replaceable scripting engine | Specialized | Extremely high | Largely built: language packs (Dart, JS, Python) behind one `Host` interface and `PackRuntime` trait. Adding Python changed no core behaviour (see "Phase D notes") |

## Web platform parity

What JS can do in a normal browser that the packs can't do here yet (principle 6). Each is
implemented once in the host (`browser/src/host.rs`), so every pack gets it together.

| Capability | Status | Notes |
|---|---|---|
| `document.cookie` (read and write) | Missing | The network layer already keeps a cookie jar; expose it per page origin, hiding `HttpOnly` cookies from page code as browsers do, and applying third-party rules |
| Timers: `setTimeout`, `setInterval`, `requestAnimationFrame` | Missing | Needs the event loop to wake the pack; time comes from the host (principle 4) |
| Network from page code: `fetch`, `XMLHttpRequest`, `WebSocket` | Missing | Must go through the gate with the page's URL (`fetch_for_page`), so third-party rules apply |
| Storage: `localStorage`, `sessionStorage`, IndexedDB | Missing | Per origin, and per profile: keep it under `profile::dir()` |
| Events: `removeEventListener`, mouse/keyboard details, `preventDefault`, input/change on form fields | Partly | Listeners and `event.type`/`target` work; details and form events don't |
| DOM breadth: `querySelectorAll`, `classList`, `style`, `innerHTML`, `children`/`parentElement`, form `value` | Partly | `querySelector`, create/append/remove, `textContent`, attributes, `id`, `className` work |
| `window` and `location`: URL, navigation, `history`, viewport size | Missing | |
| `console.*` with objects, `print` to a console view | Partly | Both packs log as text, with levels, to the Inspector's Console and the Trace panel; objects are shown by handle, not expanded |
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

## Step 7 notes: the Inspector, the debug protocol and AI control (Phase B)

**Debug protocol** (`browser/src/debug.rs`). One module the browser's tools read through:
tabs (`TabInfo`, with the page's `PageRef`), DOM nodes (`node`, with `Privacy::Full` for
the Inspector and `Privacy::Redacted` for AI), `describe`, `box_model`, `computed_styles`,
the redacted `outline`, console lines with levels (`console`), the trace and requests for a
site, and exception snapshots. Redaction rules live here, with their tests. It's plain
functions over the existing stores (the trace, the gate log, the tabs), not a message
protocol: there is one consumer process, so a wire format would only add a layer. DAP
would be a translation onto these functions.

**Inspector** (toolbar "Inspect", `browser/src/inspector.rs`), for the active tab:
- **Elements:** the DOM as a tree (`<html>` and `<body>` open; whitespace text hidden), with
  every attribute. Selecting a node shows its attributes, its box from Blitz's last layout
  (page position, size, margin, border, padding) and key computed styles
  (`resolved_style_value`), and highlights it on the page with Blitz's own
  `devtools.highlight_node` overlay; no Blitz changes were needed. The tree re-reads the DOM
  twice a second (the DOM has no change signal the UI can watch).
- **Console:** logs and errors for the tab's site, from the trace, filterable (All, Errors,
  Warnings, Logs), with errors' source-mapped Dart or JS stacks as they already come. Console
  levels now travel in the trace entry (`call` is `console.warn` etc.). Newest first with the
  input line on top, like the other panels, because Blitz can't yet keep a list scrolled to
  its bottom. "Clear" clears the trace (the console is a view of it).
- **Console input** (JS pages): evaluated in the page's own Boa context (`js_eval`), so it
  sees the page's globals; DOM access goes through the host like page code, and the input
  and result are traced (pack "console"). Results are formatted by the prelude's
  `__inspect`: strings quoted, objects/arrays/Maps/Sets three levels deep, cycles marked,
  functions as `ƒ name()`, host objects as `<p id="out">` or `Document` (naming an element
  costs two traced host calls). Console input is the user's own action, so it needs no
  permission. The AI's redacted views leave console input out, since the user may type
  anything there.
- **Decided: Dart pages can't evaluate code.** dart2wasm compiles ahead of time and a page
  ships no compiler, so there's nothing to evaluate Dart with. Evaluating JS against a Dart
  page's DOM was rejected: it would run the JS engine on a page where JS may be off
  (principle 1), and JS callbacks would share the host's listener table with Dart's callback
  ids. A small expression language over host objects was rejected as a new language to
  maintain for little gain over Elements. The console says this instead of an input line.
- **Network** opens the Requests panel rather than duplicating it.

**Pause on exceptions** (Inspector → Exceptions; off at every launch). When on, an error a
pack doesn't catch (a listener, a timer, a script or module, Dart's `main`) records a
snapshot: what was running (`click listener on button#go`), the error with its
source-mapped stack, the page's last 12 host calls, what the pack holds (handles, listeners,
timers) and the redacted DOM outline. The page then **pauses**: its listeners don't run and
its timers wait until "Resume" (Blitz's own default actions still happen). That's pause at
event-loop granularity, not mid-function: the error has already unwound, so locals are gone.
A timer batch that pauses partway still finishes, because its one-shot timers are already
out of the table. Scripts during page load keep running and the page pauses after them.
The console still works while paused.

**What breakpoints would need** (not built):
- **The blocker for both packs:** packs run synchronously on the UI thread, holding the DOM.
  Stopping inside a pack call means not returning from it, which freezes the browser UI.
  Breakpoints need the pack to run on its own thread (DOM calls marshalled to the UI thread,
  which today are direct borrows) or to be resumable from a suspended state; wasmtime's
  async support (fibers, `call_async`) could suspend a guest at a host call and resume it
  later on the same thread. That restructuring is the real cost.
- **JS (Boa):** Boa has no debugger API. We build Boa ourselves (`packs/js`), so a hook in
  its bytecode loop that checks a (source, line) breakpoint table and calls a host import
  `host.paused` is feasible; Boa's bytecode carries source positions (it builds stack traces
  from them). Locals would need Boa's environments exposed through that import.
- **Dart (wasmtime):** dart2wasm emits source maps, not DWARF, so native debuggers can't
  help. Breakpoints would mean instrumenting the wasm (insert a host call at each mapped
  statement offset, as the exnref pass already rewrites it) and resolving source lines with
  `source_map.rs`. Reading Dart locals means walking WasmGC structs, which have no field
  names without a name section.
- **DAP:** an adapter on a loopback socket could map `threads` to tabs, `evaluate` to
  `RunningPack::eval`, `setExceptionBreakpoints` to pause on exceptions, `continue` to resume
  and `stackTrace` to a snapshot's stack. Without real stops that's a thin layer over what
  the Inspector shows, so it waits for breakpoints.

**AI control** (`ai_access.rs`). A second switch, separate from read-only access: off at
every launch and whenever access is turned off; toggled from the menu (shown while access is
on) or the AI bar, which says when control is on and offers "Turn off control". Rules
(`control_decision`, tested): control on, dev mode on, and a dev-mode Allow grant for the
tab's origin; anything else is refused with the reason, and the refusal is traced as an
error. Tools:
- `eval`: JS pages only, through `RunningPack::eval`, traced as pack "ai". Its output isn't
  redacted: control is only for sites the user has granted dev mode, and eval can reach only
  what the host exposes (no cookies; no form field values yet). **It shares the page's realm
  with console input**, so globals the user declared in the console (`let token = ...`) are
  readable by AI eval on a granted site.
- `click`: runs the page's click listeners on the element a selector matches, like
  `element.click()`, with no default action (a link isn't followed). Typing isn't supported.
- `reload`: reloads the tab (dev-mode Dart source recompiles).

The tools are always listed (MCP clients cache the list) and say they need control.
eval and click run in the tab's *loaded* page, which during a navigation is still the
previous one, so the grant is checked against that page's own URL and they're refused
unless it's the site the tab shows (`control_origin`, tested). About pages drop the tab's
page reference.

**All `file://` pages share one origin** (`settings::origin_of`), so one dev-mode Allow grant
for a local file covers every local file, for compiling and for AI control. A future fix is
to key `file://` grants by directory; not done, since dev-mode compile grants share the key.

**Console input stays the user's.** While `RunningPack::eval` runs, `Objects::caller` makes
the host trace its calls and logs under the caller ("console" or "ai") instead of the page's
pack, so the AI's redacted views leave out the console's input and everything it caused.

**Limitations:**
- **Time limit:** packs run on the UI thread, so code that never returns would hang the
  browser. The shared wasmtime engine uses epoch interruption (a clock thread ticks every
  100 ms) and each call into a pack (script, listener, timer, console or AI eval, Dart's
  `main`) gets about 5 s. A call over it is stopped and traced as "script ran too long and
  was stopped". The pack is then marked stopped and gets no more events, timers or input
  (reload to run it again): an interrupted Boa call leaves its context borrowed, and
  Dart's runtime may be mid-update, so neither can safely continue. The compiled-JS-engine
  cache is invalidated once by the config change (it recompiles and re-saves).
- A paused or stopped page doesn't arm the timer thread; resuming arms it, as do console
  eval and AI click, so timers they start run without waiting for another event.
- Elements selection is keyed by document address; after a reload the tree starts fresh.
- Snapshots are kept in memory (the last 20, all tabs) and lost on quit.

## Phase C notes: palette, tabs, spaces and profiles

**Command palette** (`browser/src/command_palette.rs`). Cmd+K or Cmd+Shift+P (Ctrl
elsewhere) opens one list: open tabs (every space's), commands, recently closed tabs and,
once you type, history. Arrows move, Enter runs, Esc or a click outside closes. Commands:
new/close/reload/pin tab, close other tabs, reopen closed, each panel (Inspect, Trace,
Requests, Site), dev mode, AI access, JavaScript for this site (one step of the Site
panel's cycle, default → always allowed → kept off, via the same `js_permission::cycle_site`,
then a reload), history, bookmarks, settings, tab layout, switch/move to/remove a space,
and open a profile. Typing a name also offers "New space “name”" and "New profile “name”".
- Matching is nucleo's fuzzy matcher, as in the URL bar, run synchronously: the list is at
  most ~1,100 items (history is capped at 1,000), which scores in well under a frame.
  `rank` (tested) orders by score, then tabs, commands, closed tabs, history; history rows
  are capped at 8 and pages already open in a tab are left out. It reuses the URL bar's
  input and suggestion-row styles.
- It runs only what the menu and panels already run (settings, `ai_access`, the tab
  manager), so it adds no new permission paths. Shortcuts are read from winit window events
  (`use_window_event`), like Cmd+T/W before, so they work while a page has focus.
- **Bookmarks don't exist yet** (about:bookmarks is a stub), so there are no bookmark
  results. When they're built they belong in the profile's folder and the palette list.

**Tabs** (`browser/src/tab_manager.rs`, `tab_strip.rs`). `TabManager` (a context of Copy
signals over the tab store) is the one place tabs are activated, opened and closed; the
rules are pure functions (`display_order`, `next_active_after_close`, `idle_tabs`), tested,
plus a test that drives the real store.
- **Vertical tabs:** Settings → Tab layout (or the palette) switches between the top strip
  and a 220px sidebar; saved as `vertical_tabs` in settings.json. Both show the same items:
  favicon, title, close, active state, new tab, pinned tabs and the space switcher. Side
  panels now start below `--chrome-top`, since the toolbar sits higher without the strip.
- **Pinned tabs:** first (left or top), favicon only (or the title's initial), no close
  button, shown in every space, skipped by "close other tabs" and by idle closing. Pin or
  unpin from the palette (there's no tab context menu).
- **Idle ("temporary") tabs:** an unpinned tab nobody has looked at for
  `close_idle_tabs_hours` (default 12; Settings cycles 1h, 4h, 12h, 1d, 3d, 7d, never, and
  back to 12h) is closed by a once-a-minute check. A tab's idle time starts when you leave
  it; the tab on screen is never idle. Closed tabs (by hand or idle, last 50) reopen with
  Cmd+Shift+T or from the palette, pinned again if they were; the list is saved with the
  session, so tabs closed while you were away can be found after a restart.
- **Session restore:** open tabs (URL, title, pin, space, last-left time), the spaces and
  the current space are written to the profile's `session.json` whenever they change
  (write-then-rename) and reopened at startup. Tabs come back at their current page
  **without their back/forward list**, and every restored tab loads at startup (no lazy
  loading). Tabs that went idle past the limit while the browser was closed go straight to
  recently closed without loading; the tab on screen at quit is saved as just looked at, so
  it stays. A URL on the command line opens in a new tab, or focuses an open tab already
  showing it.
- **A damaged session.json** is moved aside as `session.json.bad` (logged) and the browser
  starts with a new tab, so the next save can't overwrite it; a single unreadable tab is
  skipped on its own, and impossible times (overflowing or in the future) read as now.
  `settings.json` gets the same treatment (`settings.json.bad`), so a parse error can't
  turn into every saved permission being reset on the next change.

**Spaces** only organise: each tab has a space name, the window shows the current space's
tabs plus pinned ones, and switching shows that space's most recently used tab (or a new
tab page). Selecting a tab from another space (palette) switches to its space. Removing a
space closes its tabs. Cookies, settings and history are shared between spaces.

**Profiles** (`browser/src/profile.rs`) are genuinely separate. **Decided: one profile per
process.** `browser --profile <name>` runs a profile (default "Default"); about:settings and
the palette list profiles and open or create one, which starts a new process.
- **Why a process, not in-process separation:** settings (`settings::get()`), the gate and
  its request log, the trace, the JS "allow this visit" list, AI access and the cookie jar
  (reqwest's, inside blitz-net's one `Provider`, created by dioxus-native) are all
  process-wide. Making each per-profile would mean threading a profile through the gate,
  every settings read, the loader and the packs, plus a second dioxus-native provider, and
  any missed global would leak between profiles. A process boundary makes leaks impossible
  by construction and needed no changes to those modules. The cost: profiles are separate
  windows in separate processes (more memory; no dragging tabs between them), and opening
  an already-open profile does nothing (it's shown as "Open in another window").
- **What's separate:** each profile's folder (`<data dir>/profiles/<name>/`) holds
  `settings.json` (dev mode, per-site JS/Dart/dev-mode/third-party grants, tab settings),
  `history.sqlite3`, `session.json`, `ai-access.json` and `http-cache/`. Cookies are in the
  process's memory, so per profile (they still aren't saved across restarts at all; that
  predates profiles). Web storage doesn't exist yet; it should live in the profile folder.
- **HTTP cache:** blitz-net kept one cache for everything under Blitz's own folder, which
  would serve one profile's cached (possibly personalised) responses to another. The fork
  gained `blitz_net::set_cache_dir` (vendor/blitz, branch `browser-test`) and each profile
  sets its own. The old shared cache folder is simply no longer used.
- **Still shared:** the compiled JS engine (compiled code keyed by a hash of the engine's
  bytes and wasmtime's version and configuration; no user data) and the dev-mode Dart
  compile cache. **The Dart cache can hold user data:** for an http(s) page it stores the
  fetched source and its compiled wasm under `dart-dev/http/<hash>/`, and source a server
  sends may be personalised for whoever fetched it. Entries are keyed by a hash of the
  exact bytes, so a profile only ever gets a hit for bytes it fetched itself, but the files
  sit in a folder every profile can read and nothing deletes them. **Future:** move the
  http part into the profile folder (or delete entries after the compile) and clean up old
  entries. Every shared cache file is replaced whole (`profile::write_atomic`: a temp file
  named for the process, then a rename), never rewritten in place, because another
  profile's process may have it open or memory-mapped (wasmtime maps the compiled engine;
  rewriting it underneath could crash that process).
- **Lock:** a `lock` file (`File::try_lock`) stops two processes using one profile, which
  would otherwise overwrite each other's session and settings. The OS releases it on exit;
  startup retries for about 200 ms in case a window of that profile is just closing. Only
  "would block" means "already open"; any other locking error is reported as itself.
- **AI access** uses a fixed port, so only one profile can have it on at a time; the other
  reports the port is in use. Each profile writes its own token file.
- **Migration:** on the first run, after creating and locking the folder, `settings.json`
  (from the config folder) and `history.sqlite3` (from the data folder) move into
  `profiles/Default/`. A file only moves when the profile doesn't already have one, so it
  never overwrites, and an interrupted move completes on the next start (tested). The
  `-wal`/`-shm`/`-journal` files move only along with their database (now or in an earlier
  run), never next to a different one.
- Profile names become folder names, so they're limited to letters, digits, spaces, `-` and
  `_` (1–32 characters, no leading or trailing space). They're compared ignoring case
  (macOS folders ignore case), so "work" opens the existing "Work". Which profiles are open
  is checked once when the palette or Settings opens (it takes each one's lock briefly), not
  on every keystroke. Window titles name non-default profiles. There's no rename or delete yet (delete the folder by hand).

**Testing note:** Dioxus swallows panics during a render, so a test that asserts inside a
`VirtualDom` app passes even when the assertion fails. `tab_manager`'s store test catches
the panic and checks the outcome after `rebuild_in_place`; the older
`tab_lens_survives_webview_props_replacement` test in `tab.rs` asserts inside the render
and so can't fail.

## Phase D notes: the pack contract and a Python pack

**The contract** is written down in `docs/packs.md`. The code now matches it. Each pack has
one `start` (`dart_pack::start`, `js_pack::start`, `python_pack::start`), built from
`PackDocument::starting` and `Startup` in `page_document.rs`, which knows no pack by name.

**A Python pack** (`packs/python`, `browser/src/python_pack.rs`):
- **What it is:** RustPython 0.6 compiled to `wasm32-wasip1`, on the same bridge as JS (one
  `host.request` JSON import, the same ABI under a `py_` prefix).
- **The prelude:** `prelude.py` wraps host handles in a `HostObject` with attribute access
  and method calls. It defines `window`, `document`, `console`, `Math`, the timer functions
  and `print`.
- **Pages:** `<script type="text/python">`, inline (dedented) or `src`. All scripts share
  one namespace.
- **What works:** errors read as Python tracebacks with file and line. Console eval works
  (the last statement's `repr`), and so do AI eval, the time limit, pause on exceptions and
  the trace.
- **Demo:** `examples/python-counter`.

**Size and speed:**
- The pack is 13.4 MB of Wasm (Boa is 5.4 MB). Compiled, it's a 28 MB cache file, written
  atomically and keyed by wasmtime's compatibility hash, like JS's.
- Compiling it the first time took about 0.5 s on this Mac. After that, a page's
  interpreter starts in about 25 ms.

**What had to change in the core, and why.** No behaviour changed for Dart or JS, and no
host operation or dispatch rule changed (`host.rs`'s `PageHost` is untouched). The changes
are generalisations:
- **`page_document.rs`:**
  - `start_dart` and `start_js` became `PackDocument::starting` and `Startup` (`uncaught`,
    `report`, `snapshot`, `finish`), and each pack's start moved into its own module. Before,
    a new language meant a new `start_<lang>` in the document code; now it means none.
  - `PackRuntime` gained `can_eval` (default true; Dart false). It replaces the Inspector's
    `pack.name == "js"` check, which would have hidden Python's console.
  - The wasmtime engine and time limit moved here from `dart_pack.rs`, since every pack uses
    them.
- **`host.rs`:** `Host` and `HostValue` moved here from `dart_pack.rs`, unchanged.
- **`interpreter.rs`** (new) is the JS pack's bridge, WASI and module cache, taken out of
  `js_pack.rs` and parameterised by a `Language` (label, export prefix, path). Only two
  packs share it, so this isn't "abstract on the third use". It's justified by the size:
  copying it for Python would have duplicated about 300 lines that must stay in step (the
  wire format, the WASI table, the cache).
  - The WASI table gained deny-only functions RustPython's standard Rust library links:
    `args_*`, `clock_res_get`, and file calls that fail with EBADF, with no preopened
    directory. They grant nothing; JS doesn't import them.
- **Registering Python** (outside the core): a branch and script type in
  `document_loader.rs`, `python_blocked` in settings, a Site panel row, and the Inspector's
  language name.

**Decisions:**
- **Python is first-class, like Dart:** it runs by default and is blocked per site in the
  Site panel ("Python: Allowed/Blocked"). There's no prompt. Principle 1 keeps JS off by
  default so that the web's incumbent language gets no privilege. Python has no existing
  pages to run unexpectedly: a page runs Python only because its author wrote
  `type="text/python"`.
- **RustPython, not CPython's WASI build:** it builds from crates.io with the same toolchain
  as the JS pack (no downloaded binaries). It needs no WASI files to start, since it has no
  stdlib on disk, and links into the same single-import shape. The cost is that RustPython
  isn't CPython: it's slower, and some behaviour differs.
- **One pack per page:** Dart first, then Python, then JS. Pages mixing languages would need
  the host to separate callback ids per pack. Nothing needs it yet.
- **The interpreter ABI keeps per-language export prefixes** (`js_run`, `py_run`), so the
  JS engine didn't need rebuilding.
- **No wrapper cache in Python:** host objects compare with `==`, so nothing holds wrappers
  alive. JS needs `WeakRef` for the same.
- **Strict callback arity:** Python listeners must accept the event argument. Python reports
  a mismatch rather than ignoring it as JS does.

**Isolation and lifecycle**, now tested for every pack:
- each page has its own instance (`each_page_gets_a_fresh_instance`);
- Python has no network, files, environment or processes (`python_has_no_side_doors`; it
  can't import `os`, `socket`, `subprocess` or `ctypes`, and `open` fails);
- dropping a page drops its DOM, host objects and pack instance
  (`a_dropped_page_releases_its_pack_and_dom`, for Dart, JS and Python).

**Limitations and deferred:**
- **No Python standard library** (`json`, `math`, `random`, `re`...): only the interpreter's
  built-in modules (`sys`, `time`, `itertools`). Use `Math.random()` for randomness. Adding
  RustPython's native stdlib crate would cost size and needs auditing for host access.
- **No imports between page files, and no `async`/`await` tied to the event loop.**
- **Values:** lists and dicts can't be passed to the host (as for JS). Members use Web IDL
  camelCase names. The prelude's method list must track `host.rs` (as for JS).
- **Stopped packs:** a pack the time limit interrupts is stopped for good, as before. So now
  is a pack that **traps** (a guest panic, a stack overflow, running out of memory).
  - The interpreter's state is half-updated after a trap, so every later call would trap
    again: before this, a crashing `setInterval` logged an error on every tick.
  - The page sees one error, `the Python pack crashed (<trap>); reload the page to run it
    again`. The wasm backtrace goes to the browser's log, and so does a guest panic's
    stderr, which no longer appears as page console output.
  - Dart traps stop the Dart pack the same way. Dart exceptions are not traps and don't.
  - Two known triggers are now page errors instead: Python's `time.sleep` raises "would
    block the page; use setTimeout", and `sys.setrecursionlimit` is capped at 3,000. About
    5,000 simple frames fit in the Wasm stack.
- **Memory cap:** every pack's store (Dart, JS, Python) has a 512 MB memory limit
  (`page_document::MEMORY_LIMIT`). A page that allocates without end gets an error inside
  its pack (Python: `MemoryError`; JS: the pack crashes) instead of taking the browser's
  memory.
- **JS messages that changed:**
  - A stopped pack now says `this page's script was stopped (it ran too long or crashed);
    reload the page to run it again`. It used to say "stopped after running too long".
  - A JS engine trap now reads `the JS pack crashed (...); reload the page to run it again`
    and stops the pack. It used to be the raw wasm error, and the pack kept going.
  - A missing build says `JS pack not built: run packs/js/build.sh`.
- **The WASI shims check what the pack hands them:**
  - pointers and lengths are checked against its memory before the host allocates;
  - iovec arithmetic is checked;
  - one `fd_write` sends at most 64 KiB to the trace;
  - one `random_get` fills at most 64 KiB.
- **Script types** compare ignoring ASCII case (`Text/Python`, `MODULE`).
- **One pack per page:** the scripts of packs that don't run are traced. For example,
  `skipped 2 JS script(s): this page runs Python`.
- **Builds:** `packs/*/build.sh` build with `--locked`. There is no `rust-toolchain.toml`:
  pinning a toolchain would change how the existing builds pick theirs, for little gain.
- **Stale compiled packs:** after saving a newly compiled pack, older
  `<prefix>-engine-*.cwasm` files of the same pack are deleted.
- **Pinned dependencies:** RustPython's ruff fork crates are held at 0.16.5, because their
  0.16.10 patch release breaks `rustpython-codegen` 0.6.0. Revisit when RustPython 0.6.1 or
  later ships.

## Phase E notes

**Per-site CSS** (`browser/src/site_css.rs`, Site panel → "Custom CSS for this site").
- **Stored** in the profile's `settings.json` as `site_css` (origin → `{css, enabled}`), so
  each profile has its own. An empty sheet removes the entry.
- **Applied** by the loader right after parsing, before any pack runs, so it needs no script
  and works with JS off. "Apply" and the On/Off switch also update every open page of that
  site at once, without a reload.
- **How:** the sheet is the text of a `<style>` element that is created but never inserted
  in the page's tree, registered with Blitz's `upsert_stylesheet_for_node` (no Blitz
  changes). It's an author sheet created after the page's own were parsed, so it comes
  after them in the cascade and wins ties without `!important`.
- **Isolation:** keyed by origin, so it never reaches another site. The element isn't in the
  tree, so page code (which can only search the tree) can't find, read or remove it, the
  Inspector's tree and the AI outline don't show it, and the browser UI is a separate
  document it never touches. Any `url()` or `@import` in it is fetched through the page's
  gate, under the page's third-party rules.
- **Limitations:**
  - All `file://` pages share one origin, so they share one sheet (as they share grants).
  - A `<style>` a page script creates later comes after the user sheet, and so does a
    page sheet if Blitz reuses a lower node id for it. Stylus-style precedence isn't
    guaranteed against late page styles; `!important` still wins.
  - No preview while typing: it applies on "Apply".

## Known local setup notes

- Building cloned repos under `vendor/` needs your approval each time (auto mode blocks it).
- Intel Homebrew Python on this Apple Silicon Mac breaks native builds that call `xcrun`;
  use `/usr/bin/python3` or an arm64 Python.
- Your Herd apps (`vector-workflow.test`, `pallet-pal.test`) are Inertia apps that render
  blank without JS; they need the JS pack or Inertia server-side rendering.
