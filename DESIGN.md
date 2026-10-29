# Browser experiment: design

An experimental browser where JavaScript is no longer the default language of the web.
Dart is first-class; JS is just one more language pack, off by default.

Status: experiment. Current plan: "Roadmap (2026-10-28)". Phases 0–3 built in `browser/` (started from Blitz's reference
browser): the gate, the Dart pack (with dev mode and source maps), the JS pack, and the
per-site policy screen. Phase A is done, Phase B is done apart from breakpoints and DAP
(the Inspector, AI access and AI control; see "Step 7 notes"), and Phase C is done apart
from bookmarks (see "Phase C notes"). Phase D is done: the pack contract is written down in
`docs/packs.md` and Python is a third pack (see "Phase D notes"). Phase E is done: per-site
CSS, Split View, hot reload with state restoration, the Handles and Profiler views, a Dart
compiled-code cache and time-travel replay (see "Phase E notes"). Web platform parity part 1
is done: `document.cookie`, `fetch`, `localStorage`/`sessionStorage`, `location`/`history`
and window basics in all three packs (see "Web platform parity notes"). Part 2 adds DOM
breadth and form events, `WebSocket` and a subset of IndexedDB (see "Web platform
parity part 2 notes"). Run it with `cd browser && cargo run --release -- <url>`. Dart demo:
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
**Done**, see "Phase E notes". Not done: a draggable split divider, reload by replay, and
saving recordings to share.

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
| Split View | High | Medium–high | **Done** (Phase E): palette "Split with tab"; fixed 50/50 (`tab_manager.rs`) |
| Automatic tab management | High | Low–medium | **Done** (Phase C): pinned tabs, closing idle tabs, reopen closed, session restore (`tab_manager.rs`) |
| Website CSS customization | Medium | Medium | **Done** (Phase E): Site panel → Custom CSS (`site_css.rs`) |
| Rust/WASM scripting | Specialized | Very high | Partly built: the Dart pack already runs Wasm on the host dispatcher. A Rust pack would need its own way to describe host calls (Rust has no dart2wasm-style import table) |
| `document.cookie` for Wasm and JS | High | Low–medium | See "Web platform parity": expose the existing cookie jar per origin, hiding `HttpOnly` cookies |
| Replaceable scripting engine | Specialized | Extremely high | Largely built: language packs (Dart, JS, Python) behind one `Host` interface and `PackRuntime` trait. Adding Python changed no core behaviour (see "Phase D notes") |

## Web platform parity

What JS can do in a normal browser that the packs can't do here yet (principle 6). Each is
implemented once in the host (`browser/src/host.rs`), so every pack gets it together.

| Capability | Status | Notes |
|---|---|---|
| `document.cookie` (read and write) | **Done** | One jar per profile, shared with the network stack and saved in the profile (`cookies.rs`); see "Web platform parity notes" |
| Timers: `setTimeout`, `setInterval`, `requestAnimationFrame` | **Done** | Phase A: host-owned timers, woken by the event loop |
| Network from page code: `fetch`, `XMLHttpRequest`, `WebSocket` | Partly | **`fetch` done** (text and JSON bodies, through the gate, basic CORS; see "Web platform parity notes"). **`WebSocket` done** (text messages, through the gate; see "Web platform parity part 2 notes"). `XMLHttpRequest` deferred |
| Storage: `localStorage`, `sessionStorage`, IndexedDB | **Done** (subset) | **`localStorage` and `sessionStorage`** (`storage.rs`; see "Web platform parity notes"); **IndexedDB, a useful subset** (`idb.rs`: stores, transactions, put/add/get/delete/getAll/count/clear; no indexes or cursors; see "Web platform parity part 2 notes") |
| Events: `removeEventListener`, mouse/keyboard details, `preventDefault`, input/change on form fields | **Done** (part 2) | Listeners, details, `preventDefault`, propagation, `popstate`/`hashchange`/`resize`, and form events: `input`, `change`, `submit` (cancellable), `focus`/`blur`; see "Web platform parity part 2 notes" |
| DOM breadth: `querySelectorAll`, `classList`, `style`, `innerHTML`, `children`/`parentElement`, form `value` | **Done** (part 2) | Queries, tree walking and editing, `classList`, `dataset`, `style`, `innerHTML`/`outerHTML`, `getBoundingClientRect`, `focus`/`blur`, form `value`/`checked`/`disabled`; see "Web platform parity part 2 notes" |
| `window` and `location`: URL, navigation, `history`, viewport size | **Done** (part 1) | `location`, `history`, `innerWidth`/`innerHeight`, `devicePixelRatio`, scroll, `navigator.userAgent`/`language`, `resize`; see "Web platform parity notes" |
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
- **Values:** lists and dicts of data can be passed to the host (part 2; before, they couldn't), but not ones holding functions or host objects. Members use Web IDL
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
  document it never touches. The browser finds the carrier by an attribute whose name is
  random per process (`site_css::mark`), so a page can't plant a lookalike element to
  receive or read back the user's CSS (tested). Any `url()` or `@import` in it is fetched
  through the page's gate, under the page's third-party rules.
- **Limitations:**
  - All `file://` pages share one origin, so they share one sheet (as they share grants).
  - A `<style>` a page script creates later comes after the user sheet, and so does a
    page sheet if Blitz reuses a lower node id for it. Stylus-style precedence isn't
    guaranteed against late page styles; `!important` still wins.
  - No preview while typing: it applies on "Apply".

**Split View** (`tab_manager.rs`: `split`, `shown_tabs`, `split_shown`; `tab.rs`; palette).
- Palette → "Split with tab: <title>" puts another tab of the current space beside the
  active one; "Close split view" ends it. The two are a pair of real tabs, each with its own
  document, gate view and pack, as before; nothing about loading changed.
- The pair shows while either of them is the active tab. The active side has focus, a blue
  top border and the toolbar, URL bar and panels (they already follow the active tab).
  Clicking in the other side, or its tab in the strip, makes it active. Selecting a third
  tab shows it alone; going back to either of the pair shows the split again.
- **Fixed 50/50:** both panes are `flex: 1` in a row (`.tab-area`). No draggable divider:
  Blitz has no drag-resize primitive in the UI document and a fixed split covers the use.
- Panes keep tab-store order (left is the tab opened first), so sides don't swap when focus
  moves. Closing either tab ends the split. The split isn't saved with the session.

**Hot reload with state restoration** (`browser/src/hot_reload.rs`, dev mode only).
- **When:** dev mode on, a `file://` page, and a dev-mode Allow grant for its origin
  (`watching`, tested). Anything else is never watched. Every 500 ms the browser polls the
  modification times of the page's folder (not subfolders) for `.html .js .mjs .py .dart
  .css .wasm` files, plus the whole tree when the folder is a Dart project (has a
  `pubspec.yaml`; the same walk `dev_compile` uses to decide a recompile, which skips hidden
  folders, `build` and `node_modules` and doesn't follow symlinks). Only the tabs on screen
  (the active one and its split partner) are polled; one out of sight is checked when it's
  shown again. No watcher crate: polling is a few `stat`s and needs no new dependency.
- **What happens:** a newer file reloads the tab (traced as `hot reload` under pack "dev").
  Dart source recompiles because `dev_compile`'s cache is now stale; JS and Python refetch.
  Per the correction, there's no hot module replacement for any pack: it's a restart with
  a fresh pack instance, and the page's in-memory state is lost.
- **What's restored:** the viewport scroll position, and text inputs, textareas and
  checkboxes, matched by tag, id and name (repeats by order). Passwords and hidden inputs
  are never captured. The URL and its hash are kept because it's a reload of the same URL.
  Restoration happens when the new page is committed, after its scripts ran, as a
  browser's form restoration would.
- **Only into the page it came from:** the saved state lives on the tab's loader
  (`DocumentLoader::restore`) with the URL it was taken from. A later save replaces it, the
  tab's next commit takes it, and it's restored only if that commit is the same URL
  (`restore_for`, tested). Navigating elsewhere during a slow recompile drops it, so one
  site's form values can't land in another's page.
- **Limitations:** http(s) pages aren't watched, even with a grant (there are no local files
  to poll; a Herd site would need a dev-server signal). Scripts outside the page's folder
  aren't watched. A compile error page keeps being watched (it's the same URL), so fixing
  the error reloads it. Reload by replay (DESIGN "Hot reload" step 2) would restore the
  page's own state too; see time-travel replay below for what that needs.

**Handle inspector and profiler** (Inspector → Handles, Profiler; `RunningPack::handles`,
`debug::profile`).
- **Handles** reads the page's `Objects` through `RunningPack::handles`: every handle the
  pack holds and what it is (`Element p#out`, `Document`), listeners per node and event
  type with each callback's id and call count, pending timers (kind, due in, callback), the
  number of distinct callbacks the host refers to, and total calls into the pack. Refreshed
  twice a second like the other views. WasmGC objects with field names (the original
  "heap inspector" idea) aren't shown: dart2wasm ships no name section to read them with.
- **Profiler:** each call into a pack (a listener, a timer or animation frame, and the
  pack's `start`: loading plus the page's scripts, less time spent fetching JS module
  imports) is now traced as a `TraceKind::Pack`
  entry whose `micros` is the time **inside the pack**: the elapsed time less the host time
  it caused, which `PageHost::traced` adds up in `Objects::host_micros`. Host operations
  keep their own entries. `debug::profile` (tested) groups both by label, slowest first,
  and the view shows pack time against host time with a bar per row.
- It's a sorted table with bars, not a flame graph: calls into a pack don't nest (host calls
  can't call back into the pack), so there's no stack to draw beyond pack → host.
- **Limitations:** it covers what's in the trace (the last 2000 entries, all tabs), and
  "Clear" clears the trace (as the console's does). Console and AI input are left out.
  Pack time includes the pack's own work in Rust (string conversion in the bridge), which
  is part of crossing the boundary but can't be separated without timing inside each pack.

**Shared compiled-runtime cache** (`page_document::load_or_compile`, `code_key`).
- **What existed:** the JS and Python interpreters' compiled code was already cached on disk
  (`<cache dir>/<prefix>-engine-<hash>.cwasm`, keyed by the `.wasm` and wasmtime's
  `precompile_compatibility_hash`, written with `write_atomic`, older builds deleted). That
  logic moved into `page_document.rs` (where the engine is) so Dart can use it; behaviour
  for the interpreters is unchanged.
- **New: Dart page programs** are cached too (`dart_pack::compiled`), as
  `<profile>/compiled-dart/<key>.cwasm`, key = SHA-256 of the wasm bytes, the compatibility
  hash and the page's origin (`sha2` was already in the dependency tree; a cryptographic
  hash means a page can't craft bytes that collide with another program's entry). The
  interpreters' cache files use the same key, so they recompiled once after this change. Reloads (including hot reload), other tabs and later runs of the same site skip
  Cranelift. At most 32 are kept per profile, newest first.
- **Decided: Dart's cache is partitioned by site and profile; the interpreters' isn't.** Every
  browser has the same interpreters, so a hit on them reveals nothing. A Dart program is
  the site's own: a cached one starts measurably faster, and a page can time that (it can
  start an image request from `main` and the server can measure the gap), which would let a
  site learn whether this machine ran another site's program, or ran it in another
  profile. Keying by origin and keeping it in the profile folder closes that, at the cost of
  compiling the same bytes again for another site or profile.
- **Never shared:** stores, instances, memories, `Objects`, callbacks: each page gets its own
  (`each_page_gets_a_fresh_instance`). Only immutable compiled code is reused.
- The dev-mode Dart compile cache (`dart-dev/`) is unchanged; its http part still sits in the
  shared cache folder (see "Phase C notes").

**Time-travel replay** (`browser/src/replay.rs`; Inspector → Recording).
- **What's recorded**, per page, everything that crosses the host boundary into the pack:
  - **steps**, in order: events delivered to the page's listeners (type, target as a path
    of child indexes, mouse and key details), timer batches (which timer handles fired,
    with an animation frame's timestamp), and console or AI eval input;
  - **the tape**: every nondeterministic value the pack read, in order: `Date.now()`,
    `Math.random`, and the interpreters' WASI clock and random bytes; plus whether each
    uncaught error paused the page (pause on exceptions is a setting that can change at
    any time), with "Resume" recorded as a step, so a replay pauses and resumes where the
    recording did (`a_pause_on_an_exception_replays_as_recorded`);
  - **the start**: the page's HTML, its site CSS, and its program (Dart's wasm, or the
    interpreter's scripts plus every JS module import fetched).
- **The fix this needed (principle 4):** the interpreters' WASI `clock_time_get` and
  `random_get` had their own clock and generator in `interpreter.rs`, a side path around the
  host. They now ask the host (`Host::clock`, `Host::random_u64`), and every such read goes
  through the page's `Tape` (`Objects::tape`: off, recording or replaying). Dart's
  `DateTime.now()` also failed before (its `Date.now` import wasn't recognised); it now
  reads `Host::now`.
- **Replaying** parses the recorded HTML, applies the same site CSS (so node ids line up),
  starts a fresh pack on the recorded program (nothing is fetched), answers reads from the
  tape and applies the steps through the same dispatch and timer code as live
  (`RunningPack::dispatch`, `run_timers`). Timers fire by handle, not by the clock. Then the
  page goes live. A read of the wrong kind, a missing target or a timer that isn't pending
  is reported as divergence (`Outcome::diverged`).
- **Event targets are paths, not node ids:** Blitz allocates anonymous layout nodes in the
  same slab, so an element the page creates after the first layout gets a different id in a
  replay that never lays out. Child-index paths over the DOM tree are the same.
- **Proved by tests:** `a_recorded_session_replays_to_the_same_dom_in_every_pack` records the
  cross-pack events page (clicks, keys, timeouts, an interval, an animation frame, clicks
  that print `Math.random` and the clock now and from a timer, console input for JS and
  Python) in Dart, JS and Python, replays each in a fresh pack and compares the whole DOM;
  replaying to step 1 stops there. `replay_without_the_tape_differs_and_divergence_is_reported`
  shows the tape is what makes it match, and that a bad step is reported.
- **UI:** Inspector → Recording. "Record" sets the tab's `DocumentLoader::record_next` and
  reloads (a page's earlier state can't be replayed, so a recording starts with a load).
  The flag is taken by the tab's next load whatever URL it ends at; the loader arms the
  recording just before the pack starts, and clears it after, so a recording never waits
  around for a later visit; "Stop" keeps it. For a
  recording: ⏮ ◀ ▶ ⏭ (or click a step) pick step N, "Replay to step N in this tab" swaps a
  fresh replayed page into the tab (scrubbing back replays from the start), "Verify"
  replays all of it off screen and compares the DOM with the one at "Stop", "Forget"
  drops it.
- **Privacy:** recordings hold what the page received, including console input, so they stay
  in memory (the last 5) and are never written to disk or reachable by AI access.
- **Limitations:**
  - Blitz's own default actions aren't replayed (typing into a field, toggling a checkbox,
    following a link): only what reached the pack. Pages can't read field values yet, so
    this doesn't change what a pack sees today; it will once `value` is supported.
  - Leaving or reloading the page without "Stop" loses the recording.
  - A recording is limited by size (about 64 MB, counting each step's timers, input text
    and path, and each tape value). Going over, it stops and drops the step under way with
    its reads, so every step it keeps replays (tested). Going over during start-up keeps
    nothing replayable, and a replay says it diverged.
  - No saving or sharing recordings as bug reports yet: they'd need a file format and the
    same redaction review as AI views.
  - "Reload by replay" for hot reload (replaying a session into recompiled code) isn't
    built: a code change usually changes which values are read, so the tape diverges; it
    would need a rule for when to fall back to a plain reload.

## Web platform parity notes

Part 1 of principle 6: cookies, `fetch`, storage, location and history, and window basics,
the same in Dart, JS and Python. Each is a host member under its Web IDL name, so every pack
gets it at once; the cross-pack test `web_apis_behave_the_same_in_dart_js_and_python` runs
one page (`tests/fixtures/web_api.*`) in all three and expects identical output.

**Cookies** (`browser/src/cookies.rs`, `document.cookie`).
- **One jar per profile**, shared by requests and page code. blitz-net used reqwest's own
  jar, which nothing else can read; the fork gained `blitz_net::set_cookie_stores` (like
  `set_cache_dir`), and `main` gives it views of the profile's jar before the provider
  exists (one per `SameSite` scope, below).
- **Rules:** `cookie_store` (already in the tree through reqwest) parses and matches (RFC 6265
  domain and path matching, expiry, `Secure` only over https). On top, as browsers do: page
  code never reads, sets or overwrites an `HttpOnly` cookie; `Secure` can only be set from
  https; nothing is set for a public suffix (`Domain=co.uk`, checked with `psl`, which the
  gate already uses); from an insecure URL nothing may shadow a `Secure` cookie of the same
  name (RFC 6265bis), by `Set-Cookie` or page code. Order is longest path first, then name.
- **Limits:** 4 KB per cookie (name plus value), 180 per domain and 3000 in all; past them
  the oldest go (the jar keeps the order cookies were set in).
- **SameSite:** requests carry a `CookieScope` (a field the fork added to blitz's `Request`),
  and blitz-net keeps a client per scope, each with a view of the jar. A link or form from
  another site (`BrowserNavProvider`, comparing the current page's site, scheme included)
  sends no `Strict` cookies, and if it isn't a GET no `Lax` ones either, so a cross-site form
  can't ride the user's session. A page's subresources to another site send neither (only
  cookies without `SameSite`, or `None`). What the user types is same-site. Cookies without
  the attribute are still sent cross-site (browsers' newer Lax-by-default isn't applied).
- **Persistence:** cookies with `Expires`/`Max-Age` are saved to the profile's
  `cookies.json`, replaced whole (`write_atomic`), whenever a change adds, replaces or
  removes one (including replacing it with a session cookie, so an old token can't come
  back). The profile's jar writes on a thread of its own half a second after changes settle,
  never on the UI thread, and flushes when the browser exits. Session cookies stay in memory,
  so they're gone after a restart. A damaged file is
  kept as `cookies.json.bad`, like settings.json.
- **Replay:** every `document.cookie` read is on the tape (text reads: `Tape::read_text`,
  `Recording::texts`), and a replay never writes to the real jar.
- **AI:** `debug::holds_secrets` marks host calls whose arguments or results carry secrets
  (`Document.cookie`, storage, fetch, responses and headers, plain objects and arrays,
  location and history, and `popstate`/`hashchange` details); redacted trace views show
  `(hidden)` for both, and the redacted console hides the arguments of such a call that
  failed (a quota error). The cross-pack test checks that secrets the page reads but never
  shows appear nowhere in the redacted trace or console, in every pack. What a page itself writes into its DOM or console is the page's output and isn't
  hidden: AI access sees the page as the user does.
- **Site panel → Site data:** the cookie count for the page's site (registrable domain) and
  "Clear cookies".
- **Limitations:**
  - `file://` and other pages without a host have no cookies (`document.cookie` is empty),
    as in Chrome.
  - `__Secure-`/`__Host-` prefixes aren't enforced.
  - **Third-party cookies** without `SameSite` (or with `None`) still go with a page's
    subresource requests to sites the user allowed for it (third-party requests are refused
    by default), and responses from them can set cookies; there's no partitioning.
    `fetch` (below) sends cookies only to its own origin.
  - A crash, or quitting within half a second of a change, can lose that change.
  - Saving writes the whole file; fine for normal jars.

**fetch** (`Window.fetch`, `gate::fetch_from_page`).
- **Through the gate, as a request from the page.** `Gate::fetch_from_page` checks every hop
  with the same `allow` as everything else (logged, third-party default-deny, block-all), and
  follows redirects itself so a same-site URL can't redirect to a blocked site unchecked.
  blitz-net gained `Provider::fetch_once` (one request, any status, its headers, no redirects,
  no HTTP cache), with a cookie-carrying client and a credentialless one.
- **Same origin by default; basic CORS for the rest.** Cookies go only to the page's own
  origin (`credentials: "omit"` sends none; `"include"` is treated as same-origin, so cookies
  never go cross-origin). A cross-origin response is readable only if
  `Access-Control-Allow-Origin` is `*` or the page's origin; its headers are cut to the
  CORS-safelisted ones plus `Access-Control-Expose-Headers`. There's **no preflight**: a
  cross-origin request that would need one (another method, custom headers) is refused
  before anything is sent. `Set-Cookie` is never exposed. Forbidden request headers
  (`Cookie`, `Host`, `Origin`, `Sec-*`...) set by page code are dropped.
- **Schemes:** http(s); `data:`; `file:` only from a file page and only in its folder or below
  it: the URL is decoded to a path and both it and the folder are resolved on disk
  (`canonicalize`, so encoded `..` and symlinks can't lead out), and the resolved path is
  what's read. Anything else rejects. `CONNECT`, `TRACE` and `TRACK` are refused (TRACE would
  echo HttpOnly cookies back).
- **Bounded:** a body is read in chunks up to 64 MB (a larger `Content-Length` is refused up
  front), each hop has a 60 s timeout, a fetch stops when its page goes (the load's abort
  signal), and fetches share the per-host limit of 6 with other requests. A page holds at
  most 256 MB of response bodies at once; past that, fetches reject.
- **DNS rebinding / private network:** a page on a public site (not a file, `localhost`,
  a private IP, or a `.test`/`.local`/`.localhost`/`.internal`/`.home.arpa` name) can't
  fetch private or local addresses: IP literals are refused, and names are resolved by a
  resolver that drops loopback, private, link-local and similar addresses (and fails if none
  are left), so a name can't be rebound to the local network between check and connect.
  **Gap:** this covers `fetch` only; a public page's images and CSS can still name a local
  address if the user allowed that site as a third party.
- **Rejections** are `TypeError: Failed to fetch <url>: <why>` (blocked, CORS, network
  error). JS gets a real `TypeError`.
- **Async, in every pack.** The host has a small promise (`Obj::Promise`, `Objects::settle`):
  `then(onFulfilled, onRejected)` callbacks run later as zero-delay "promise" timers with the
  value, so they go through the same dispatch, release and replay machinery as timers. The
  network task pushes its result to `Objects::fetched` and wakes the page's event loop;
  `RunningPack::deliver_fetches` (from `poll`) settles the promise on the UI thread.
  - **JS:** a real `Promise` (the prelude wraps host promises; `await` works). `json()`
    parses with `JSON.parse`, so the page gets ordinary objects. Host objects aren't
    thenable, so resolving with a `Response` doesn't call it.
  - **Dart:** `package:web`'s `window.fetch(...).toDart` works unchanged: dart2wasm's
    `p.then(s, (e) => f(e, e === undefined))` snippet is recognised (`PromiseThen`), and the
    second argument is added for its rejection handler.
  - **Python:** callbacks, `fetch(url).then(on_value, on_error)` (a `Promise` wrapper);
    `then` returns None, so nest rather than chain. Chosen as the simplest that works with
    RustPython without asyncio (there's no stdlib); `await` would need a small coroutine
    driver in the prelude and is deferred.
- **Plain objects can cross now** (needed for `fetch`'s `init` and headers): `Host::object`
  (and `array`, `is_array` for Dart's `String.split`) makes an `Obj::Json`; the interpreter
  wire format gained `{"obj": {...}}`; Dart object literals (`({method: x0, ...})`, `({})`
  with `o[p] = v`) are recognised. Only data crosses (strings, numbers, booleans, null, other
  plain objects), not functions or host objects inside them.
- **Replay:** a finished fetch is a step (`Step::Fetch`, with the response), and replaying
  sends nothing: the page's `fetch` makes a promise and the step settles it. A bad URL
  rejects at once in both; "no network" is delivered like a finished fetch, so it's a step
  too and a replay matches.
- **AI:** `Window.fetch`, `Response.*`, `Headers.*` and `Promise.*` calls are hidden in
  redacted trace views; bodies are never traced (they reach the pack as callback arguments).
- **Tested** against a local server (`serve` in `page_document.rs` tests): same-origin GET with
  a `Set-Cookie` that `document.cookie` then sees, POST with a header and body, a 404, a
  redirect, a blocked third party, a CORS-allowed cross-origin read (no cookies sent) and a
  refused one, identical in all three packs and replayed.
- **Limitations and deferred:**
  - Bodies are text only: no `arrayBuffer()`, `blob()`, `formData()`, streams, or binary
    request bodies; invalid UTF-8 is replaced.
  - No `Request`/`Headers` constructors, `AbortController`, or `mode: "no-cors"` (opaque
    responses); `bodyUsed` is always false and a body can be read more than once.
  - No HTTP cache for page fetches; no third-party cookie partitioning (none are sent).
  - **Handles:** a promise's state goes once its callbacks are queued (each pack calls
    `then` once; Python's `Promise` fans out further `then`s itself). Responses, their one
    `Headers` object and parsed data are marked releasable on the wire (`{"o": h, "r": 1}`),
    and the pack sends `release` when it drops them: Python as its wrappers are freed
    (counted per handle), JS when its garbage collector finalizes them (so later, and not
    deterministically). **Dart doesn't release them** (dart2wasm gives no hook when a JS
    value is collected), so a Dart page's responses live until it goes, within the 256 MB
    bound. `many_fetches_dont_pile_up_host_objects` checks Python and JS.
  - A replayed page that then goes live has no network (it has no gate).
  - **XMLHttpRequest deferred:** it isn't cheap on top of fetch (synchronous mode can't work
    on the UI thread, and its event model is its own); new code uses fetch.

**Storage** (`browser/src/storage.rs`, `Obj::Storage`).
- **`localStorage`:** per profile and origin, one file per origin in the profile's
  `local-storage/` folder (named by a SHA-256 of the origin, holding the origin to check
  against), replaced whole on every change. Limit: 5 Mi UTF-16 code units of keys plus values
  per origin; going over throws `QuotaExceededError` and stores nothing. All `file://` pages
  share one origin, as for every other per-origin setting. Pages with an opaque origin
  (about:blank, data:) get `SecurityError`.
- **`sessionStorage`:** per tab and origin, in memory: the tab's `DocumentLoader` owns it and
  passes it to each page in `PageEnv`, so it survives reloads and navigations within the tab
  and goes with the tab. Not restored with the session after a restart.
- `getItem`, `setItem` (values become strings), `removeItem`, `clear`, `key`, `length`. Keys
  are kept sorted, so `key(n)` is stable (the order is up to the browser in the spec).
- **Prelude change:** `key` is a Storage method but an event property, so a single
  "which names are methods" list couldn't serve both. Storage objects get their own method
  list in the JS and Python preludes (`STORAGE_METHODS`); everything else is unchanged.
- **Replay:** every read (`getItem`, `key`, `length`) is on the tape as text; a replay never
  writes to real storage.
- **AI:** `Storage.*` calls are hidden in redacted views.
- **Site panel → Site data:** the origin's local storage size, and "Clear site data", which
  clears the site's cookies, the origin's local storage and the active tab's session storage.
- **Limitations and deferred:** no `storage` event between tabs; no named-property access
  (`localStorage.foo`; use `getItem`); IndexedDB and the Cache API deferred; a Dart page
  can't catch host errors (a pre-existing gap: they aren't thrown as Dart exceptions), so
  `QuotaExceededError` ends its current task there.

**Location and history** (`Obj::Location`, `Obj::History`, `history.rs`, `TabWebView`).
- **The tab's back/forward list now knows documents.** Each entry has a document id and a
  `history.state` (JSON text). `pushState`, `replaceState` and fragment navigations
  (`#section`, by link, `location.hash` or `assign`) make or change entries that share the
  current document; anything else gets a new one. The tab loads when the current entry's
  document changes (a memo), not when its URL does, so moving between entries of one
  document doesn't reload. Instead the page gets them through `RunningPack::traverse`:
  the entry's URL (the document's base URL is updated) and state, then `popstate`, and
  `hashchange` if the fragment changed. A page with no pack just takes the URL.
  (`same_document` and the entry rules are tested.)
- **`location`:** `href`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`,
  `hash`, `origin`; setting `href` or `hash`, `assign`, `replace`, `reload`. A navigation
  goes into the tab's history like a link click (`TabNav::navigate`), so the tab loads it
  through the normal path and the gate. Only http(s), file and about URLs can be navigated to
  (no `javascript:`). `window.location = url` and `document.location` work too.
- **`history`:** `pushState`/`replaceState` (the spec's rewrite rule: same scheme,
  username, password, host and port, so `https://bank.test@evil.test/` is refused; a file
  page may change only its query and fragment; `SecurityError` otherwise; state must be data,
  as `fetch` init is), `state`, `length`, `back`, `forward`, `go`.
- **How the page reaches its tab:** `PageEnv::nav`, a `host::TabNav` (`history::TabHistory`
  over the tab's history store and reload counter). Tests use a recording stand-in; replays
  and pages outside a tab have none (navigation does nothing).
- **Events the page causes** (`popstate` and `hashchange` after it sets a fragment) are queued
  (`Objects::queued_events`) and dispatched on `window` after its code returns, from `poll`.
- **Replay:** `history.state` (first read) and `history.length` are on the tape; a traversal
  is a step (`Step::Traverse`) and its `popstate` another; a replay never navigates.
- **AI:** `Location.*` and `History.*` calls are hidden in redacted views (full URLs can carry
  tokens; AI access sees URLs only redacted).
- **Limitations:** the tab's list still isn't saved with the session (as before), so state
  and pushed entries are lost on restart; `history.scrollRestoration`, `location.ancestorOrigins`
  and `URL`/`URLSearchParams` aren't there; a global assignment `location = "..."` in JS or
  Python rebinds the name instead of navigating (use `location.href`); Blitz doesn't scroll
  to a fragment's target.

**Window basics.**
- `innerWidth`/`innerHeight` (the viewport in CSS pixels, from Blitz's `Viewport`),
  `devicePixelRatio` (its scale: hidpi times zoom), `scrollX`/`scrollY` (and
  `pageXOffset`/`pageYOffset`), `scrollTo`/`scroll`/`scrollBy` with `(x, y)` or `{left, top}`
  (clamped at 0; Blitz clamps to the content when it lays out). All reads are on the tape.
- `navigator.userAgent` is blitz-net's `USER_AGENT`, the one every request sends;
  `navigator.language` is a fixed `en-US`, so it adds nothing to fingerprinting.
- **`resize`:** Blitz has no resize callback for a sub-document, so the page's `poll` compares
  the viewport size with the last one it saw and dispatches `resize` on `window` when it
  changed. Polls follow redraws, so a resize arrives with the next frame.
- JS gets `innerWidth` etc., `navigator`, `scrollTo` and `scrollBy` as globals too; Python
  reads them from `window` (it has no global getters), plus `navigator`.

**Profile files are private:** `write_atomic` creates its file with mode 0600 and the
profile folder is 0700, since it holds cookies, storage and history.

**Cross-cutting decisions:**
- **`PageEnv`** is the one explicit hand-off of what a page reaches beyond its DOM (gate, the
  tab's session storage and history), passed to each pack's `start` rather than through
  globals, so tests and replays visibly get none.
- **Events the host causes** for `window` are dispatched from `poll` (queued, a resize, a
  traversal), through the same `dispatch` as input, so they're recorded and paused alike.
- **Prelude method lists** remain hand-kept (`METHODS` in both preludes), now with
  `STORAGE_METHODS` for the one name clash (`key`). A per-interface table generated from the
  host would remove that upkeep; three packs share it today, so it waits.

## Web platform parity part 2 notes

Part 2 of principle 6: DOM breadth and form events, `WebSocket`, a subset of IndexedDB and
a basic canvas, the same in Dart, JS and Python. As in part 1, each is a host member under
its Web IDL name, and each has a cross-pack test that runs one page in all three packs and
expects identical output, then replays it.

**DOM breadth** (`host.rs`; cross-pack test `the_dom_and_form_events_behave_the_same_in_dart_js_and_python`,
fixtures `tests/fixtures/dom.*`).
- **Queries:** `getElementById`, `querySelectorAll` (on documents and elements). A `NodeList`
  has `length`, `item(i)` and indexing; JS iterates it (`for...of`, spread, `forEach`) and
  Python too (`len`, `[i]`, `for`); Dart uses `length`/`item`. `children` and `childNodes`
  are NodeLists too. **They're snapshots**, not live collections (a `querySelectorAll`
  result is static in browsers anyway; `children` and `childNodes` are live there).
- **The tree:** `parentElement`/`parentNode`, `first`/`last`/`next`/`previousSibling` and their
  `...ElementChild`/`...ElementSibling` forms, `childElementCount`, `nodeType`, `nodeName`,
  `nodeValue`/`data` for text, `documentElement`, `head`, `createTextNode`, `insertBefore`,
  `replaceChild`, `removeChild`, `cloneNode(deep)`, `contains`, `hasAttribute`, `className`
  (read). Inserting a node into itself or its own subtree throws `HierarchyRequestError`
  (`appendChild` too, which before could make the tree a loop and hang the browser);
  removing or replacing a node that isn't a child throws `NotFoundError`.
- **`classList`** (`add`, `remove`, `toggle(token, force)`, `contains`, `item`, `length`,
  `value`), **`dataset`** (`el.dataset.userId` ↔ `data-user-id`; Dart's `dataset['userId']`)
  and **`style`** (`setProperty` with priority, `getPropertyValue`, `removeProperty`,
  `cssText`, `length`, and camelCase properties such as `style.marginTop`). Each is a view of
  the element's attribute, one object per element. `style` reads and writes the `style`
  attribute as written: values aren't normalised as browsers do (`#f00` stays `#f00`), and a
  `;` inside a quoted string or `url()` would split a declaration.
- **`innerHTML`** (get and set) and **`outerHTML`** (get). Setting parses with Blitz's HTML
  parser (`DocumentHtmlParser::parse_inner_html_into_mutator`). **Nothing in it runs:** a page
  picks its pack and runs its scripts once, at load, so an inserted `<script>` is inert, as
  in browsers (tested), and pack selection and permissions are untouched. What it loads
  (images, stylesheets) goes through the page's gate like any subresource. Serialising is
  the host's own (void elements, escaping, raw-text elements), since Blitz's writes empty
  elements as `<p />`.
- **Dropped nodes:** replacing an element's `textContent` or `innerHTML` drops its old
  children for good. Blitz reuses node ids, so their handles, views and listeners are now
  forgotten (`Objects::forget_nodes`): a stale handle fails ("this object is no longer
  available") instead of reaching whatever new node gets the id. (Before, `textContent`
  could alias a stale handle onto a new node.)
- **Form fields:** `value` (text fields' text; other inputs' `value` attribute), `checked`,
  `disabled`. Setting `value` changes the field's text, not its attribute, and puts the
  caret at the end. Blitz makes a field's editing state when it first lays the field out;
  page code can run before that, so the host makes it the same way first
  (`ensure_field_state`). Reads of `value` and `checked` are on the tape (`Read::Field`):
  the user changes them by typing and clicking, Blitz's default actions, which a replay
  doesn't repeat. `<select>` values aren't supported.
- **`getBoundingClientRect`** returns a plain object (`x`, `y`, `width`, `height`, `top`,
  `left`, `right`, `bottom`) from **the layout Blitz last computed** (the last frame), less
  the scroll. No layout is forced, unlike browsers: right after a change, or during the
  page's first scripts (before the first layout), it's out of date or zero. Forcing one
  means calling Blitz's `resolve` with an animation time the host doesn't know (the shell
  owns it), which would restart CSS transitions; reading the last frame is the safe
  choice. On the tape (`Read::Layout`): layout depends on the window and the fonts.
- **`focus()`/`blur()`** move Blitz's focus (only to focusable elements) and queue `blur`,
  `focusout`, `focus` and `focusin`, dispatched once the page's code returns (browsers
  dispatch them synchronously; the host can't re-enter a pack mid-call).
- **Form events.** Blitz already reports `input` (text fields, checkboxes, radios), `focus`,
  `blur`, `focusin` and `focusout`. The page document adds what browsers fire around them
  (`RunningPack::form_events`), with no fork change:
  - `change`: when a text field loses focus with a value other than the one it had when it
    got it (before `blur`, as in browsers), and after `input` on a checkbox or radio;
  - `submit` on the form, before a click on an enabled submit button or Enter in a one-line
    text field would submit it. `preventDefault` on `submit` cancels Blitz's default action
    (the click's or the key's), so the form isn't submitted (tested, with a navigation
    provider that records submissions). Enter fires `submit` whenever the field has a form,
    without the spec's implicit-submission conditions (Blitz checks those itself, so an
    Enter can fire `submit` where Blitz then doesn't submit). `form.submit()` and
    `requestSubmit()` aren't supported.
  - Each is dispatched (and recorded) like any other event, so a replay gets them as steps.
- **AI:** `Element.value`, `Element.checked`, `Element.innerHTML` and `Element.outerHTML` calls
  are hidden in redacted views (`debug::holds_secrets`). `innerHTML`/`outerHTML` are hidden
  on every element, not only forms: any element's HTML can contain a field's value. The
  cross-pack test plants a password field's value, reads it in every pack, and checks it
  never reaches the redacted trace or console.
- **Handles:** NodeLists are releasable (JS and Python free them as their wrappers go) and
  bounded: a page holds at most 4 Mi listed nodes (Dart never releases). `classList`,
  `style` and `dataset` objects are one per element, like elements.
- **Dart:** `package:web`'s `operator []` and `operator []=` (`dataset`) compile to
  `(x0,x1) => x0[x1]` and `(x0,x1,x2) => { x0[x1] = x2 }`, now recognised. Dart can't
  catch host errors (part 1's gap), so the cross-pack fixture avoids them; `dom_edge_cases`
  tests them in JS.
- **Wire format:** arrays now cross from the interpreters (`{"arr": [...]}`), for data such
  as IndexedDB values.

**WebSocket** (`browser/src/websocket.rs`; cross-pack test `web_sockets_behave_the_same_in_dart_js_and_python`,
fixtures `tests/fixtures/ws.*`, against a tungstenite server on 127.0.0.1).
- `new WebSocket(url, protocols)`, `send`, `close(code, reason)`, `readyState`, `url`,
  `protocol`, the `CONNECTING`...`CLOSED` constants, and `open`, `message`, `error` and
  `close` events through `on<event>` handlers or `addEventListener`. Message events have
  `data`; close events `code`, `reason` and `wasClean`. JS gets a `WebSocket` class (its
  constructor returns the host object); Python a `WebSocket(url, protocols=None)` function;
  Dart's `package:web` constructor snippet (`x0 => new WebSocket(x0)`) is recognised as a
  new shape, `Construct`, which the host has as a method of `window`.
- **Through the gate:** the URL (`http(s)` becomes `ws(s)`, relative to the page) is checked
  with the gate's own rule for the page (`Gate::allow`: logged, block-all, third-party
  default-deny). A refused one fails like a connection that went nowhere: `error`, then
  `close` with 1006. An https page can't open a `ws:` socket (`SecurityError`, as browsers'
  mixed-content rule). A public page's sockets connect only to public addresses: IP
  literals that aren't public are refused, and names resolve to public addresses only, which
  are the ones connected to (`websocket::addresses`, tested), so DNS can't rebind them.
- **Handshake:** the page's `Origin`, the browser's User-Agent, and the cookies the jar would
  send to the matching `http(s)` URL under `SameSite` (all of them same-site; no `Strict` or
  `Lax` ones cross-site). The test checks a `SameSite=Strict` cookie arrives. `Set-Cookie`
  on the handshake response is **ignored** (browsers store it), to keep the socket path out
  of the cookie rules' write side.
- **How it runs:** each connection is a thread (`websocket::connect`) with tungstenite (sync,
  already in the tree through Dioxus's devtools) over a `TcpStream`, or native-tls (already
  in the tree through reqwest) for `wss:`. Reads wait 20 ms at a time so the page's sends
  get a turn. What arrives goes to the page's `Inbox`, which wakes its event loop;
  `RunningPack::deliver_sockets` (from `poll`) applies it to the socket's state and
  dispatches the event, so delivery is async, on the UI thread, like fetch's. A thread
  per socket was chosen over an async client because tokio-tungstenite isn't in the tree
  and a page has at most 64 open; the cost is a thread each.
- **Bounded:** 64 open sockets per page and 16,384 made in all; messages up to 16 MB either
  way; at most 64 MB received and not yet delivered per page (past that a connection stops
  reading until the page catches up, so a paused page holds no more). Sockets aren't
  releasable handles (they get events while the page holds no wrapper); a closed one keeps
  only its URL, its listeners and handlers released.
- **When the page goes**, its socket states and their command queues go with it; each
  thread sees its queue disconnect, sends a close frame and ends
  (`sockets_close_when_the_page_goes`).
- **Replay:** each event is a step (`Step::Socket`); a replay makes the page's socket objects
  but opens nothing (the test counts the server's connections) and the steps deliver what
  the recording got.
- **AI:** `WebSocket.*`, `Window.WebSocket` (its URL), `Event.data` and `Event.reason` are
  hidden in redacted views; the test plants a message the page never shows and checks no
  redacted view has it.
- **Limitations:** text messages only (a binary one is dropped and logged; there's no
  `ArrayBuffer`/`Blob` in the host), so `binaryType` is ignored; `bufferedAmount` is
  always 0; no extensions (no compression); closing while still connecting waits for the
  connection and closes it then. Events go to non-node objects through a small generic
  mechanism (`Objects::target_listeners` and `handlers`, `RunningPack::dispatch
**IndexedDB, a useful subset** (`browser/src/idb.rs`; cross-pack test
`indexed_db_behaves_the_same_in_dart_js_and_python`, fixtures `tests/fixtures/idb.*`, and
`indexed_db_refuses_what_it_should`).
- `indexedDB.open(name, version)` with `upgradeneeded` then `success` (or `error`:
  `VersionError` for an older version), `deleteDatabase`; a database's `name`, `version`,
  `objectStoreNames`, `createObjectStore(name, {keyPath, autoIncrement})` and
  `deleteObjectStore` (only during `upgradeneeded`: `InvalidStateError` otherwise),
  `transaction(stores, mode)` and `close`; a transaction's `objectStore`, `mode` and
  `complete` event; a store's `name`, `keyPath`, `autoIncrement`, `put`, `add`
  (`ConstraintError` for an existing key), `get`, `delete`, `getAll`, `count` and `clear`,
  each returning a request with `result`, `error` (`name`, `message`), `readyState` and
  `success`/`error` events, through `on<event>` or `addEventListener`.
- **Storage:** per profile and origin, one JSON file per origin in the profile's
  `indexeddb/` folder (named by a SHA-256 of the origin), replaced whole on every change, as
  local storage is; in memory with no profile folder. A change that would take an origin
  over 10 MB (its JSON; twice local storage's limit) fails with `QuotaExceededError` and
  changes nothing. SQLite (the `persistence` crate) was considered: it would make big
  databases cheaper to change, but adds a schema, migrations and a connection per profile
  for what is today a small subset; whole-file writes keep it the same as local storage and
  are fine at this size. **Tradeoff:** every write rewrites the origin's file.
- **Values** are JSON data: strings, numbers, booleans, null, arrays and plain objects, not
  the whole structured-clone algorithm (no `Date`, `Map`, `Set`, `Blob`, typed arrays or
  cycles). **Keys** are numbers or strings, numbers first; key paths are a name or a dotted
  path (`autoIncrement` fills only a top-level one); no array, date or binary keys, and no
  compound key paths.
- **How it runs:** each operation runs at once, on the UI thread, and its outcome is kept in
  its request; the request's `success` or `error` is queued (`Objects::queued_object_events`)
  and dispatched once the page's code returns, like any task, so handlers set after the call
  still run. A transaction's `complete` is queued when it's made and waits until its
  requests' events have gone. So transactions don't isolate or roll back: each operation is
  committed as it runs, and `abort()` isn't supported. A request's `error` doesn't bubble to
  its transaction or database. There are no `versionchange` or `blocked` events between
  tabs. Dart's `jsify` of maps with lists needed three more dart2wasm snippets (`() => []`,
  `(a, i) => a.push(i)`, `o => [o]` and longer array literals) and a null classifier.
- **Replay:** every operation's outcome is on the tape (`Read::Idb`): another tab may change
  the databases at any time. A replay changes nothing (tested: the test changes a record
  after recording, and the replay still matches while the record stays changed), and the
  queued events are steps (`Step::ObjectEvent`).
- **AI:** every `IDB*` call is hidden in redacted views, as are the plain objects values
  arrive as; the test stores a secret, reads it back without showing it, and checks no
  redacted view has it.
- **Handles:** requests, transactions and stores are releasable, but a request or
  transaction the pack lets go of before its events have been delivered stays until they
  have. A request's data `result` is copied out on each read, so releasing one read's copy
  can't free what a later read gets. Connections aren't releasable (a request's `result`
  holds one). A page holds at most 256 Ki requests, transactions and connections.
- **One name, two meanings:** `transaction` is a database method but a property of requests
  and stores, and `error` a console method but a request's property. The JS and Python
  preludes now keep `error` to the console's own method list (as `key` is kept to
  Storage's); requests' and stores' `transaction` properties aren't supported.
- **Not supported:** indexes (`createIndex`, `index()`), cursors (`openCursor`), key
  ranges (`IDBKeyRange`), `getAllKeys`, `getKey`, `put` with a key range, and
  `indexedDB.databases()`.
_object`),
  which IndexedDB uses too.

## Known local setup notes

- Building cloned repos under `vendor/` needs your approval each time (auto mode blocks it).
- Intel Homebrew Python on this Apple Silicon Mac breaks native builds that call `xcrun`;
  use `/usr/bin/python3` or an arm64 Python.
- Your Herd apps (`vector-workflow.test`, `pallet-pal.test`) are Inertia apps that render
  blank without JS; they need the JS pack or Inertia server-side rendering.
