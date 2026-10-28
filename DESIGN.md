# Browser experiment: design

An experimental browser where JavaScript is no longer the default language of the web.
Dart is first-class; JS is just one more language pack, off by default.

Status: experiment. Phases 0 and 1 built in `browser/` (started from Blitz's reference
browser). Run it with `cd browser && cargo run --release -- <url>`. Dart demo:
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
| 2 | JS pack (QuickJS compiled to Wasm) on the same interface | JS is "just a pack" |
| 3 | Per-site policy UI: allowed packs and allowed domains | The security and language model works end to end |
| Later | Python pack, reload by replay, shared runtime cache, MCP server | |

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
  small integer **handles**, never Blitz node ids. Host API so far: `domQuery`, `domCreate`,
  `domAppend`, `domSetText`, `domOnClick`.
- `browser/src/dart_document.rs`: wraps the page so clicks fire Dart listeners, in bubbling
  order.
- `browser/src/trace_panel.rs`: the Trace panel (toolbar button) showing every host call.
- `packs/dart/dom.dart`: the Dart-side API (`query`, `create`, `Element.text`,
  `append`, `onClick`). `packs/dart/build.sh`: compile plus the exnref step.
- Pages opt in with `<script type="application/dart" src="app.wasm">`, fetched through the
  gate. A page ships only the `.wasm`: `build.sh` moves dart2wasm's import table into a
  `dart2wasm-imports` custom section (`packs/dart/embed_imports.dart`) and deletes the
  generated `.mjs` and `.support.js`.
- Known gaps: only click events; no way to remove
  listeners; Dart's `print` goes to the trace, not a console view; the source map is not
  used yet, so errors show wasm function numbers rather than Dart lines.

## Known local setup notes

- Building cloned repos under `vendor/` needs your approval each time (auto mode blocks it).
- Intel Homebrew Python on this Apple Silicon Mac breaks native builds that call `xcrun`;
  use `/usr/bin/python3` or an arm64 Python.
- Your Herd apps (`vector-workflow.test`, `pallet-pal.test`) are Inertia apps that render
  blank without JS; they need the JS pack or Inertia server-side rendering.
