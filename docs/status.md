# Status (2026-10-29)

Where the browser stands after Phases B–E and both web-platform parity passes. Design
reasoning and per-feature notes live in `DESIGN.md`; the pack contract is in `docs/packs.md`; what page code can call is in `docs/api.md`.

Every phase was built, tested, code-reviewed by a separate agent, fixed, and re-tested.
183 tests pass (`cd browser && cargo test --release`, ~2.8 s; the JS, Python and PHP packs must be
built first); release build has no warnings.

## Build and run

```sh
packs/js/build.sh                 # Boa JS pack → packs/js/target/wasm32-wasip1/release/js_pack.wasm
packs/python/build.sh             # RustPython pack → packs/python/target/.../python_pack.wasm
packs/php/build.sh                # PHP pack (downloads wasi-sdk + php-src) → packs/php/target/php_pack.wasm
packs/dart/build.sh app.dart      # a Dart page program (wasm with embedded imports + source map)
cd browser && cargo build --release && ./target/release/unscripted [--profile <name>] [url]
```

`vendor/blitz` is a submodule on the fork `mattsplat/blitz`, branch `browser-test`.
`vendor/anyrender_vello_hybrid` is a patched copy of Blitz's GPU renderer (0.11.0, used
through `[patch.crates-io]`): the original never frees cached images, so a canvas animation
filled its atlas and crashed the app within seconds. The changes are marked `browser-test`.

**First launch of this build:** existing `settings.json` and history move into
`profiles/Default`, and the JS and Python engines recompile once (cache keys changed).

## What's built

| Area | Delivered |
|---|---|
| Phase A | Timers, listener identity, event details, error types and stacks, ES modules, JS permission bar (default Ask) |
| Phase B | Inspector (Elements with an element picker, Console with eval, Exceptions, Handles, Profiler, Recording); shared debug layer (`debug.rs`); pause on exceptions; read-only AI access (MCP) and AI control as separate switches; 5 s time limit on pack code |
| Phase C | Command palette (Cmd+K / Cmd+Shift+P), vertical tabs, pinned tabs, idle auto-close (12 h default), reopen closed (Cmd+Shift+T, 50 kept across restarts), session restore, spaces, profiles (one process + data folder each) |
| Phase D | Pack contract (`docs/packs.md`); Python pack; 512 MB memory cap per page; crashed packs stop cleanly |
| Phase E | Per-site CSS, Split View, dev-mode hot reload with state restore, Dart compiled-code cache (per site and profile), time-travel replay |
| Parity 1 | `document.cookie` (per-profile jar, persisted, SameSite), `fetch` (gated per hop, CORS, bounded), local/sessionStorage, location/history, window basics |
| Parity 2 | DOM breadth and form events, WebSocket, IndexedDB subset, canvas 2D subset, async XHR |
| PHP pack | php-src 8.4 (embed SAPI) in wasm; `<script type="text/php">`, per-site block in the Site panel, `examples/php-counter` |

Every web API works identically in Dart, JS and Python; cross-pack tests enforce it. PHP has
its own tests (`php_*` in `page_document.rs`) but isn't in the cross-pack ones yet.

## Security fixes found by the reviews

- A `file:` page could read any file on disk via `fetch` (encoded `../`, symlinks).
- AI control could act on the old page while a tab navigated.
- Secrets reaching AI views: console input, JSON fields, error text, keys typed into fields.
- Cross-site cookies on POST navigations (CSRF), Secure cookies overwritable from http, TRACE exposing HttpOnly cookies, `pushState` userinfo spoofing.
- Canvas attribute sizes and huge `arc()` radii could crash or freeze the browser.
- Paused pages busy-looped the UI thread; damaged `session.json`/`settings.json` wiped tabs or permissions.

## Known gaps

- **No true breakpoints.** Packs run synchronously on the UI thread; stopping inside one would freeze the browser. Pause works at the event-loop level.
- **Dart can't release host handles** (dart2wasm has no collection hook), so long-running Dart pages can hit the response or NodeList caps until reload.
- **Partial APIs:** WebSocket is text only; IndexedDB has no indexes, cursors or rollback; canvas has no transforms, gradients or `drawImage`; XHR is async only; no Python `await` for fetch; no `storage` event between tabs.
- **Cookies:** no Lax-by-default for cookies without SameSite; third-party cookies aren't partitioned; the DNS-rebinding check covers `fetch` and WebSocket, not images or CSS.
- **Not started:** WebGL, audio/video, a draggable Split View divider, hot reload for http(s) pages, saving recordings to disk.
- **Python:** no stdlib (`json`, `math`, `re`), no importing page files.
- **PHP:** integers are 32-bit (wasm32 pointers), so big products silently become floats.
- **Dart:** string concatenation copies the whole string each time, so building a long one
  (`StringBuffer`, `join`) is quadratic; the benchmark's 20,000-string task takes ~0.6 s.

## Hand checks (not yet done; the agents couldn't drive the GUI)

- [ ] **Dev mode:** prompt on `examples/counter/dev.html`; every Site panel toggle; JS bar on `examples/js-counter` (Allow this visit / Always / Keep off).
- [ ] **Inspector:** Elements highlight follows selection; Toggle DevTools, then click an element: it's selected in Elements; Console eval on JS and Python pages; Dart shows the "can't evaluate" note; pause on exceptions, then Resume.
- [ ] **AI access:** turn on, connect Claude Code (command below); cookies, storage and form values show `(hidden)`; AI control appears only while access is on.
- [ ] **Tabs and profiles:** Cmd+K; vertical tabs layout (traffic lights, panel offset); restart restores tabs; Cmd+Shift+T; new profile opens a second window; existing history appears under Default.
- [ ] **Python:** `examples/python-counter`; a `while True: pass` handler stops after ~5 s.
- [ ] **PHP:** `examples/php-counter`; Console eval (`Unscripted::document()->querySelector('h1')->textContent`); block PHP in the Site panel and reload.
- [ ] **Phase E:** site CSS (`body{background:yellow}`) applies at once; Split View; hot reload by editing `main.dart`; Record → Verify → replay to a step.
- [ ] **Real sites:** cookies survive restart; `pushState` on an SPA; `localStorage`; a WebSocket echo; a canvas animation (watch memory for a minute).

Connecting Claude Code to AI access (token is new each time access is turned on):

```sh
claude mcp add --transport http unscripted http://127.0.0.1:7807/mcp \
  --header "Authorization: Bearer $(jq -r .token ~/Library/Application\ Support/dev.mattsplat.unscripted/profiles/Default/ai-access.json)"
```
