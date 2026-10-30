# Page API reference

What a page's code can use, in any language. Every pack reaches the page through one host
(`browser/src/host.rs`), so this list is the same for Dart, JavaScript, Python, PHP and Rust;
only the syntax differs. Names are the standard web ones (`querySelector`, `textContent`).

Anything not listed here fails with a clear error, such as
`Element.click() is not supported yet` or `Document.title is not supported yet`. The code is
the reference: if this page and `host.rs` disagree, `host.rs` wins (and this page has a bug).

- [Calling the API from each language](#calling-the-api-from-each-language)
- [Globals](#globals)
- [Document](#document) · [Element](#element) · [classList, style, dataset](#classlist-style-dataset) · [NodeList](#nodelist)
- [Events](#events)
- [Timers](#timers)
- [Network: fetch, XMLHttpRequest, WebSocket](#network)
- [Storage: localStorage, sessionStorage, cookies, IndexedDB](#storage)
- [Window, location, history, navigator](#window-location-history-navigator)
- [Canvas 2D](#canvas-2d)
- [Console and Math](#console-and-math)
- [Not supported](#not-supported) · [Limits](#limits)

## Calling the API from each language

The same click handler in each:

```js
// JavaScript: <script> (or type="module")
document.querySelector('#go').addEventListener('click', (event) => {
  document.querySelector('#out').textContent = `clicked at ${event.clientX}`;
});
```

```python
# Python: <script type="text/python">
def on_click(event):
    document.querySelector('#out').textContent = f'clicked at {event.clientX}'

document.querySelector('#go').addEventListener('click', on_click)
```

```php
<?php // PHP: <script type="text/php"> (inline, or src="main.php")
use Unscripted as U;

U::document()->querySelector('#go')->addEventListener('click', function ($event) {
    U::document()->querySelector('#out')->textContent = "clicked at {$event->clientX}";
});
```

```rust
// Rust: a crate on packs/rust, built with packs/rust/build.sh; <script type="application/rust" src="app.wasm">
use unscripted::{document, Object};

fn start() {
    document().query("#go").unwrap().on("click", |event: Object| {
        let x = event.get("clientX").ok().and_then(|v| v.as_f64()).unwrap_or(0.0);
        document().query("#out").unwrap().set_text(&format!("clicked at {x}"));
    });
}

unscripted::main!(start);
```

```dart
// Dart: package:web, compiled with packs/dart/build.sh; <script type="application/dart" src="main.wasm">
import 'package:web/web.dart';

void main() {
  document.querySelector('#go')!.onClick.listen((event) {
    document.querySelector('#out')!.textContent = 'clicked at ${event.clientX}';
  });
}
```

How each language sees the API:

| | JavaScript | Python | PHP | Dart | Rust |
|---|---|---|---|---|---|
| Globals | `document`, `window`, ... | `document`, `window`, ... | static methods: `Unscripted::document()`, `Unscripted::fetch(...)`, ... (`use Unscripted as U;` to shorten); nothing else takes a name | `package:web`'s `document`, `window` | `unscripted::document()`, `window()`, `fetch(...)` |
| Members | `el.textContent`, `el.focus()` | `el.textContent`, `el.focus()` | `$el->textContent`, `$el->focus()` | `el.textContent`, `el.focus()` | `el.get("textContent")`, `el.call("focus", &[])`, or helpers (`el.text()`, `el.set_text(..)`) |
| Lists | `for...of`, spread, `forEach`, `[i]` | `for`, `len()`, `[i]` | `foreach`, `[$i]`, `->length` | `.item(i)`, `.length` | `el.query_all(sel)` and `list.items()` give a `Vec` |
| Promises (`fetch`, `text()`, `json()`) | real `Promise`: `await` or `.then` | `.then(on_value, on_error)`; nest, don't chain; no `await` | `->then($ok, $err)` | `.toDart` and `await` | `.then(ok, err)` closures |
| Plain data (`fetch` options) | object literal | `dict` | array (`['method' => 'POST']`) | `JSObject` / `.jsify()` | `serde_json::json!({...})` |
| `print`/`echo` | — | `print()` goes to the console | `echo` goes to the console | `print()` goes to the console | `println!` goes to the console |
| Console eval (Inspector, AI) | yes | yes | yes | no (compiled ahead of time) | no (compiled ahead of time) |

Numbers from the host are doubles; Python and PHP read whole ones as ints.

### Editor support

Keep page code in its own file (`<script type="text/python" src="main.py">`,
`<script type="text/php" src="main.php">`): editors don't analyze code inside HTML. Then
open the repo root in the editor.

- **Rust:** the `unscripted` crate is ordinary Rust, so rust-analyzer completes it. Members
  are named as strings (`get("textContent")`), so a typo shows at runtime.
- **Dart:** `package:web` is fully typed. It also has APIs this browser lacks; those fail at
  runtime.
- **JavaScript:** editors include the browser's DOM types. Like Dart, they show more than
  this browser has.
- **Python:** Pyright and Pylance read `typings/__builtins__.pyi`, so `document`, `window`,
  `fetch` and the rest are known without imports. Annotate a handler's parameter with a
  string (`def on_click(event: 'Event')`): the type exists only for the editor.
  `querySelector` returns `Element | None`, so strict checking asks you to handle `None`.
- **PHP:** Intelephense and PhpStorm index `typings/browser.stub.php` (the web interfaces) and
  the PHP pack's prelude (the `Unscripted` class, whose methods name those interfaces as
  their return types). So `Unscripted::document()->` completes anywhere, closures included,
  with nothing to declare.

The stubs list only what's in this reference, so a typo or an unsupported member shows in
the editor instead of at runtime.

## Globals

| Global | Notes |
|---|---|
| `window` | see [Window](#window-location-history-navigator) |
| `document` | see [Document](#document) |
| `console` | `log`, `info`, `warn`, `error`, `debug` |
| `location`, `history`, `navigator`, `performance` | also on `window` |
| `localStorage`, `sessionStorage` | also on `window` |
| `indexedDB` | |
| `setTimeout`, `setInterval`, `clearTimeout`, `clearInterval`, `requestAnimationFrame` | extra `setTimeout`/`setInterval` arguments are passed to the callback |
| `cancelAnimationFrame` | |
| `fetch(url, options)` | see [Network](#network) |
| `WebSocket`, `XMLHttpRequest` | JS `new WebSocket(url)`; Python `WebSocket(url)`; PHP `Unscripted::WebSocket($url)`, `Unscripted::XMLHttpRequest()` |
| `Math` | JS's own `Math` (with `random` from the host); Python's `Math.random()` |
| `scrollTo`, `scrollBy` | JS; elsewhere on `window` |

## Document

| | |
|---|---|
| **Read** | `body`, `head`, `documentElement`, `cookie`, `location` |
| **Write** | `cookie`, `location` (navigates) |
| **Methods** | `querySelector(sel)`, `querySelectorAll(sel)`, `getElementById(id)`, `createElement(tag)`, `createTextNode(text)`, `addEventListener(type, f)`, `removeEventListener(type, f)` |

## Element

Elements and text nodes share one interface.

| | |
|---|---|
| **Read** | `id`, `className`, `tagName`, `textContent`, `innerHTML`, `outerHTML`, `value`, `checked`, `disabled`, `classList`, `style`, `dataset`, `children`, `childNodes`, `childElementCount`, `parentElement`, `parentNode`, `firstChild`, `lastChild`, `firstElementChild`, `lastElementChild`, `nextSibling`, `previousSibling`, `nextElementSibling`, `previousElementSibling`, `nodeType`, `nodeName`, `nodeValue`, `data`; for `<canvas>`, `width` and `height` |
| **Write** | `id`, `className`, `textContent`, `innerHTML`, `value`, `checked`, `disabled`; for `<canvas>`, `width` and `height` (clears it) |
| **Tree** | `appendChild`, `insertBefore`, `replaceChild`, `removeChild`, `remove`, `cloneNode(deep)`, `contains(node)` |
| **Attributes** | `getAttribute`, `setAttribute`, `removeAttribute`, `hasAttribute` |
| **Queries** | `querySelector`, `querySelectorAll` (within the element) |
| **Layout and focus** | `getBoundingClientRect()` (`x`, `y`, `width`, `height`, `top`, `left`, `right`, `bottom`), `focus()`, `blur()` |
| **Events** | `addEventListener`, `removeEventListener` |
| **Canvas** | `getContext('2d')`, `toDataURL()`; see [Canvas 2D](#canvas-2d) |

`value` and `checked` read what the user typed or ticked, not just the attribute.

## classList, style, dataset

| Object | Members |
|---|---|
| `classList` | `add`, `remove`, `toggle(name, force?)`, `contains`, `item(i)`, `length`, `value` (read and write) |
| `style` | any property in camelCase (`el.style.backgroundColor = 'red'`), `cssText`, `length`, `getPropertyValue`, `setProperty`, `removeProperty` |
| `dataset` | `el.dataset.userId` reads and writes `data-user-id` (Python `el.dataset['userId']`, PHP `$el->dataset['userId']` or `->userId`) |

## NodeList

From `querySelectorAll`, `children` and `childNodes`: a snapshot, not live. `length`, `[i]`,
`item(i)`, and iteration in every language.

## Events

**Listening:** `addEventListener(type, f)` on `window`, `document` and elements;
`removeEventListener` with the same function removes it. Events bubble.
`on<event> = f` works on `WebSocket`, `XMLHttpRequest` and IndexedDB objects only;
elements need `addEventListener`.

**Events the browser fires:**

| Kind | Types |
|---|---|
| Mouse and pointer | `click`, `dblclick`, `contextmenu`, `mousedown`, `mouseup`, `mousemove`, `mouseover`, `mouseout`, `mouseenter`, `mouseleave`, `pointer*` |
| Keyboard | `keydown`, `keyup`, `keypress` |
| Forms | `input`, `change`, `submit` (cancellable with `preventDefault`), `focus`, `blur`, `focusin`, `focusout` |
| Window | `resize`, `popstate`, `hashchange` |
| Network and storage | WebSocket `open`/`message`/`error`/`close`; XHR `readystatechange`/`load`/`error`/`loadend`; IndexedDB `success`/`error`/`upgradeneeded`/`complete`/`abort` |

**Event object:**

| | |
|---|---|
| All | `type`, `target`, `currentTarget`, `defaultPrevented`, `preventDefault()`, `stopPropagation()`, `stopImmediatePropagation()` |
| Mouse | `clientX`/`x`, `clientY`/`y`, `pageX`, `pageY`, `screenX`, `screenY`, `button`, `buttons`, `altKey`, `ctrlKey`, `shiftKey`, `metaKey` |
| Keyboard | `key`, `code`, `repeat`, `altKey`, `ctrlKey`, `shiftKey`, `metaKey` |
| `popstate` | `state` |
| `hashchange` | `oldURL`, `newURL` |
| WebSocket | `data` (message), `code`, `reason`, `wasClean` (close) |

## Timers

`setTimeout(f, ms, ...args)`, `setInterval(f, ms, ...args)`, `clearTimeout(id)`,
`clearInterval(id)`, `requestAnimationFrame(f)` (`f` gets a timestamp),
`cancelAnimationFrame(id)`. They're the host's, so they show in the Inspector's Handles view
and are recorded for replay.

Page code runs on the browser's UI thread and must never block: Python's `time.sleep` and
PHP's `sleep` don't work. A handler that runs longer than 5 seconds is stopped.

## Network

Every request goes through the browser's gate: it's checked against the site's settings,
logged in the Requests panel, and blocked from reaching private addresses from public
pages.

**`fetch(url, options)`** returns a promise of a Response.

- Options: `method`, `headers` (an object), `body` (text), `credentials` (`omit`,
  `same-origin`, `include`).
- Cross-origin requests follow CORS.
- **Response:** `ok`, `status`, `statusText`, `url`, `redirected`, `type`, `bodyUsed`,
  `headers` (`get(name)`, `has(name)`), `text()` and `json()` (promises).
- JSON data comes back as a host object you read like any other: `data.name`, `data[0]`,
  `data.length`.

```python
def show(data):
    document.querySelector('#out').textContent = data.name

fetch('https://api.github.com/repos/mattsplat/unscripted').then(lambda r: r.json().then(show))
```

```php
<?php
use Unscripted as U;

U::fetch('https://api.github.com/repos/mattsplat/unscripted')->then(function ($r) {
    $r->json()->then(fn($data) => U::document()->querySelector('#out')->textContent = $data->name);
});
```

**`XMLHttpRequest`** (async only):

- Members: `open(method, url)`, `setRequestHeader`, `send(body)`, `readyState`, `status`,
  `statusText`, `responseText`/`response` (text), `responseURL`, `getResponseHeader`,
  `getAllResponseHeaders`, and the state constants.
- `responseType` may only be `''` or `'text'`.

**`WebSocket(url)`** (text messages only):

- Members: `send(text)`, `close()`, `url`, `readyState`, `protocol`, `bufferedAmount`, and
  the state constants.
- Events: `open`, `message` (`event.data`), `error`, `close`.

## Storage

All storage is per site, kept in the browser profile.

**`localStorage`** (kept across restarts) and **`sessionStorage`** (cleared when the tab
closes):
- `getItem`, `setItem`, `removeItem`, `clear`, `key(i)`, `length`.
- Values are strings.

**`document.cookie`:**
- Reads `name=value; ...`.
- Writing sets one cookie, with the usual attributes (`path`, `max-age`, `expires`,
  `SameSite`, `Secure`).
- `HttpOnly` cookies aren't visible.

**IndexedDB** (a subset):

| Object | Members |
|---|---|
| `indexedDB` | `open(name, version)`, `deleteDatabase(name)` |
| request | `result`, `error`, `readyState`; events `success`, `error`, `upgradeneeded` |
| database | `name`, `version`, `objectStoreNames`, `createObjectStore(name, {keyPath, autoIncrement})`, `deleteObjectStore`, `transaction(stores, mode)`, `close()` |
| transaction | `objectStore(name)`, `mode`; events `complete`, `error`, `abort` |
| object store | `put`, `add`, `get`, `delete`, `getAll`, `count`, `clear`, `name`, `keyPath`, `autoIncrement` |

IndexedDB has no indexes, cursors, key ranges or rollback.

## Window, location, history, navigator

| Object | Members |
|---|---|
| `window` | `performance`, `innerWidth`, `innerHeight`, `devicePixelRatio`, `scrollX`/`pageXOffset`, `scrollY`/`pageYOffset`, `scrollTo(x, y)`, `scroll`, `scrollBy(dx, dy)`, plus the globals above |
| `location` | `href`, `protocol`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`, `origin`, `toString()`; setting `href` or `hash` navigates; `assign(url)`, `replace(url)`, `reload()` |
| `history` | `length`, `state`, `pushState(state, '', url)`, `replaceState(...)`, `back()`, `forward()`, `go(n)`; `pushState` URLs must stay on the page's origin |
| `navigator` | `userAgent`, `language` |
| `performance` | `now()` (milliseconds since the page started, never goes back), `timeOrigin` (the wall time it started); recorded for replay. No `mark`/`measure` or timing entries |

## Canvas 2D

`canvas.getContext('2d')` returns a context drawn into the canvas's image.

| | |
|---|---|
| **Properties** | `fillStyle`, `strokeStyle` (CSS colors), `lineWidth`, `font`, `canvas` |
| **Rectangles** | `fillRect`, `strokeRect`, `clearRect`, `rect` |
| **Paths** | `beginPath`, `moveTo`, `lineTo`, `arc(x, y, r, start, end, ccw)`, `closePath`, `fill`, `stroke` |
| **Text** | `fillText(text, x, y)` |
| **State** | `save`, `restore` |
| **Canvas element** | `width`, `height`, `toDataURL()` |

Canvas 2D has no transforms, gradients, patterns, `drawImage`, `getImageData`, `strokeText`
or `measureText`.

## Console and Math

- `console.log`, `info`, `warn`, `error` and `debug` take any arguments. Objects show by
  handle, not expanded.
- Output appears in the Inspector's Console and the Trace panel.
- `Math.random()` comes from the host, so a recording replays it.
- The rest of JS's `Math` is Boa's own. Python and PHP use their own math (Python has no
  stdlib `math` module; PHP's math functions all work).

## Not supported

Each of these errors with "... is not supported yet":

- **Document:**
  - `document.title`;
  - `getElementsByClassName`/`TagName`;
  - `createDocumentFragment`;
  - `activeElement`.
- **Element:**
  - `click()`;
  - `onclick =` and other `on<event>` properties (use `addEventListener`);
  - `matches`, `closest`;
  - `insertAdjacentHTML`;
  - `scrollIntoView`;
  - `attributes`;
  - setting `outerHTML`.
- **APIs:**
  - `URL`, `URLSearchParams`, `FormData`, `Blob`, `FileReader`;
  - `crypto`, and `performance` beyond `now()` and `timeOrigin`;
  - `MutationObserver`, `IntersectionObserver`, `ResizeObserver`;
  - `matchMedia`, `getComputedStyle`;
  - `alert`, `confirm`, `prompt`;
  - `postMessage`, `CustomEvent`/`dispatchEvent`.
- **Platform:** WebGL, audio and video, Web Workers, service workers, Notifications,
  geolocation, clipboard.
- **Language gaps:**
  - Python has no stdlib (`json`, `re`, `math`, `datetime`), no `await` on fetch, and can't
    import other page files.
  - PHP has no Fibers, no `sleep`, no `exec` family, and no files or sockets of its own.
    Its integers are 32-bit (`PHP_INT_MAX` is 2147483647): php-src sizes them by pointer, and
    WebAssembly's are 32-bit. A result past that becomes a float and loses precision.
  - Dart strings built by repeated concatenation (a growing `StringBuffer`, a big
    `join`) slow down with their length: each step copies the string so far.
  - Dart can't release host objects, so very long-running Dart pages can hit the limits
    below until reload.

## Limits

| | |
|---|---|
| Running time | a handler or script is stopped after 5 s |
| Memory | 512 MB per page's pack |
| Response bodies | 64 MB each; 20 redirects |
| Canvases | 256 with a context, 64 M pixels in all |
| WebSockets | 64 open at once; 1 MB per message |
| XMLHttpRequests | 65,536 held at once |
