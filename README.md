# browser-test

An experimental web browser where JavaScript isn't the language of the web, just one of several.

Pages can be written in **Dart, JavaScript, Python or PHP**. Each language is a *pack*: a
WebAssembly module running in [wasmtime](https://wasmtime.dev) that reaches the page only
through one host. HTML and CSS are rendered by [Blitz](https://github.com/DioxusLabs/blitz),
with no scripting engine built in.

```html
<button id="increment">Add one</button>
<span id="count">0</span>

<script type="text/python">
clicks = 0

def on_click(event):
    global clicks
    clicks += 1
    document.querySelector("#count").textContent = str(clicks)

document.querySelector("#increment").addEventListener("click", on_click)
</script>
```

The same page works with `type="text/php"`, a plain `<script>` (JS) or a compiled Dart program,
using the same web platform names in each.

> **Status: experiment.** It's built and tested on macOS (Apple Silicon). Expect missing web
> APIs and rough edges; see [`docs/status.md`](docs/status.md).

## Why

- **JS gets nothing extra.** It's a pack like the others, and off by default (each site asks
  first). Cross-pack tests make sure every web API works identically in Dart, JS and Python.
- **One way out.** Every pack, and the optional AI access, reaches the DOM, network, clock
  and randomness only through host calls. There are no side doors: no filesystem, no
  sockets, no processes.
- **Everything goes through the host**, so one gate sees every network request, a tracer
  sees every call, and a page's run can be recorded and replayed step by step.
- **Source first.** The console and stack traces show your source, not raw Wasm.

The reasoning behind these, and per-feature notes, are in [`DESIGN.md`](DESIGN.md).

## What's in it

- **Four language packs.**
  - Dart: dart2wasm output.
  - JS: [Boa](https://boajs.dev).
  - Python: [RustPython](https://rustpython.github.io).
  - PHP: php-src 8.4, embed SAPI.

  JS, Python and PHP share one runtime; adding another interpreted language is a new crate
  under `packs/`. See [`docs/packs.md`](docs/packs.md).
- **Web platform.**
  - DOM and events, timers and `fetch` (gated and CORS-checked).
  - Cookies, `localStorage`/`sessionStorage` and `location`/`history`.
  - WebSocket, XHR, a subset of IndexedDB and a basic Canvas 2D.
- **Inspector.**
  - Elements, with an element picker.
  - A console that evaluates in the page's own language.
  - Pause on exceptions, Handles, Profiler, and time-travel Recording.
- **Browser.**
  - Vertical or horizontal tabs, pinned tabs, spaces and profiles.
  - A command palette (Cmd+K), session restore, Split View and per-site CSS.
  - Hot reload for local Dart pages.
- **Per-site controls.** Allow or block each language per site, plus a 5 s time limit and a
  512 MB memory cap per page.
- **AI access (opt-in).**
  - A local MCP server lets an AI agent read the page, and separately control it.
  - Off by default and revocable.
  - Cookies, storage and form values are hidden from it.

## Build and run

You need:

- Rust (stable, 2024 edition) with the Wasm target: `rustup target add wasm32-wasip1`.
- The [Dart SDK](https://dart.dev/get-dart), only for Dart pages.

```sh
git clone --recurse-submodules https://github.com/mattsplat/browser-test
cd browser-test

packs/js/build.sh        # JS pack (Boa)
packs/python/build.sh    # Python pack (RustPython)
packs/php/build.sh       # PHP pack (downloads wasi-sdk and php-src into packs/php/.build; macOS arm64)

cd browser
cargo run --release -- file://$PWD/../examples/python-counter/index.html
```

A Dart page is compiled first, then opened:

```sh
packs/dart/build.sh examples/counter/main.dart
cd browser && cargo run --release -- file://$PWD/../examples/counter/index.html
```

Other flags: `--profile <name>` runs a separate profile, with its own data folder.

Tests: `cd browser && cargo test --release`. They need the JS, Python and PHP packs built.

## Examples

| Page | Language |
|---|---|
| [`examples/counter`](examples/counter) | Dart (`dev.html` turns on hot reload) |
| [`examples/js-counter`](examples/js-counter) | JavaScript |
| [`examples/js-modules`](examples/js-modules) | JavaScript, ES modules |
| [`examples/python-counter`](examples/python-counter) | Python |
| [`examples/php-counter`](examples/php-counter) | PHP, from a separate `main.php` |

These pages only come alive in this browser. Other browsers ignore `text/python` and
`text/php` scripts and show the static HTML.

## Layout

```
browser/    the browser: shell, host, packs' Rust side, inspector, gate
packs/      the language packs (each builds to a .wasm the browser loads)
examples/   demo pages, one per language
docs/       pack contract (packs.md) and current status (status.md)
spike/      the rendering-engine spike that picked Blitz over Servo and Gosub
vendor/     Blitz (git submodule, fork with a few changes)
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.

`browser/` started from Blitz's reference browser (also MIT/Apache-2.0). The PHP pack, when
built, includes PHP, which is under the [PHP License](https://www.php.net/license/). PHP's
source isn't stored in this repo; `packs/php/build.sh` downloads it.
