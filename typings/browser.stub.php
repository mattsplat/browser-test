<?php
// Editor types for PHP page code (<script type="text/php">). Intelephense and PhpStorm index
// this file; it never runs (the browser's PHP pack has its own prelude,
// packs/php/src/prelude.php, where every host object is a HostObject). It lists only what
// the browser supports: docs/api.md is the reference, and browser/src/host.rs is the truth.
//
// The page's globals are superglobals at runtime, but IDEs don't know that. Name their types
// at the top of a page's file:
//
//   /** @var Document $document */
//   /** @var Console $console */
//
// The globals are $window, $document, $console, $location, $history, $navigator,
// $performance, $localStorage and $sessionStorage. The functions (setTimeout, fetch, ...) are in the prelude.

/** A host promise: `then` calls one callback later. */
class Promise {
    public function then(?callable $onValue = null, ?callable $onError = null): void {}
}

/** Plain data from the host (Response::json(), IndexedDB): `$data->name`, `$data[0]`, foreach. */
class Data implements ArrayAccess, IteratorAggregate {
    public int $length;
    public function __get(string $name): mixed {}
    public function offsetGet(mixed $key): mixed {}
    public function offsetSet(mixed $key, mixed $value): void {}
    public function offsetExists(mixed $key): bool {}
    public function offsetUnset(mixed $key): void {}
    public function getIterator(): Iterator {}
}

class Event {
    public string $type;
    public Element|Window|Document|WebSocket|XMLHttpRequest $target;
    public Element|Window|Document|WebSocket|XMLHttpRequest $currentTarget;
    public bool $defaultPrevented;
    // Mouse and pointer events.
    public float $clientX;
    public float $clientY;
    public float $x;
    public float $y;
    public float $pageX;
    public float $pageY;
    public float $screenX;
    public float $screenY;
    public int $button;
    public int $buttons;
    // Keyboard events (code is an int for a WebSocket close).
    public string $key;
    public string|int $code;
    public bool $repeat;
    public bool $altKey;
    public bool $ctrlKey;
    public bool $shiftKey;
    public bool $metaKey;
    // popstate, hashchange, WebSocket message and close.
    public mixed $state;
    public string $oldURL;
    public string $newURL;
    public string $data;
    public string $reason;
    public bool $wasClean;
    public function preventDefault(): void {}
    public function stopPropagation(): void {}
    public function stopImmediatePropagation(): void {}
}

abstract class EventTarget {
    /** @param callable(Event): void $listener */
    public function addEventListener(string $type, callable $listener): void {}
    /** @param callable(Event): void $listener */
    public function removeEventListener(string $type, callable $listener): void {}
}

class DOMRect {
    public float $x;
    public float $y;
    public float $width;
    public float $height;
    public float $top;
    public float $left;
    public float $right;
    public float $bottom;
}

class DOMTokenList {
    public int $length;
    public string $value;
    public function add(string ...$tokens): void {}
    public function remove(string ...$tokens): void {}
    public function toggle(string $token, ?bool $force = null): bool {}
    public function contains(string $token): bool {}
    public function item(int $index): ?string {}
}

/** Any CSS property in camelCase: `$el->style->backgroundColor = 'red'`. */
class CSSStyleDeclaration {
    public string $cssText;
    public int $length;
    public function getPropertyValue(string $name): string {}
    public function setProperty(string $name, ?string $value): void {}
    public function removeProperty(string $name): string {}
    public function __get(string $name): string {}
    public function __set(string $name, string $value): void {}
}

/** data-* attributes: `$el->dataset->userId` or `$el->dataset['userId']` is data-user-id. */
class DOMStringMap implements ArrayAccess {
    public function __get(string $name): ?string {}
    public function __set(string $name, string $value): void {}
    public function offsetGet(mixed $key): ?string {}
    public function offsetSet(mixed $key, mixed $value): void {}
    public function offsetExists(mixed $key): bool {}
    public function offsetUnset(mixed $key): void {}
}

/**
 * A snapshot (not live) of nodes: `$list[0]`, `$list->length`, foreach.
 * @implements IteratorAggregate<int, Element>
 */
class NodeList implements ArrayAccess, IteratorAggregate {
    public int $length;
    public function item(int $index): ?Element {}
    public function offsetGet(mixed $key): Element {}
    public function offsetSet(mixed $key, mixed $value): void {}
    public function offsetExists(mixed $key): bool {}
    public function offsetUnset(mixed $key): void {}
    /** @return Iterator<int, Element> */
    public function getIterator(): Iterator {}
}

class CanvasRenderingContext2D {
    public string $fillStyle;
    public string $strokeStyle;
    public float $lineWidth;
    public string $font;
    public Element $canvas;
    public function fillRect(float $x, float $y, float $w, float $h): void {}
    public function strokeRect(float $x, float $y, float $w, float $h): void {}
    public function clearRect(float $x, float $y, float $w, float $h): void {}
    public function rect(float $x, float $y, float $w, float $h): void {}
    public function beginPath(): void {}
    public function moveTo(float $x, float $y): void {}
    public function lineTo(float $x, float $y): void {}
    public function arc(float $x, float $y, float $radius, float $start, float $end, bool $counterclockwise = false): void {}
    public function closePath(): void {}
    public function fill(): void {}
    public function stroke(): void {}
    public function fillText(string $text, float $x, float $y): void {}
    public function save(): void {}
    public function restore(): void {}
}

/** An element or a text node. */
class Element extends EventTarget {
    public string $id;
    public string $className;
    public string $tagName;
    public string $textContent;
    public string $innerHTML;
    /** Read only. */
    public string $outerHTML;
    public string $value;
    public bool $checked;
    public bool $disabled;
    public DOMTokenList $classList;
    public CSSStyleDeclaration $style;
    public DOMStringMap $dataset;
    public NodeList $children;
    public NodeList $childNodes;
    public int $childElementCount;
    public ?Element $parentElement;
    public Element|Document|null $parentNode;
    public ?Element $firstChild;
    public ?Element $lastChild;
    public ?Element $firstElementChild;
    public ?Element $lastElementChild;
    public ?Element $nextSibling;
    public ?Element $previousSibling;
    public ?Element $nextElementSibling;
    public ?Element $previousElementSibling;
    public int $nodeType;
    public string $nodeName;
    public ?string $nodeValue;
    public string $data;
    /** <canvas> only. */
    public int $width;
    /** <canvas> only. */
    public int $height;
    public function querySelector(string $selectors): ?Element {}
    public function querySelectorAll(string $selectors): NodeList {}
    public function appendChild(Element $child): Element {}
    public function insertBefore(Element $child, ?Element $before): Element {}
    public function replaceChild(Element $new, Element $old): Element {}
    public function removeChild(Element $child): Element {}
    public function remove(): void {}
    public function cloneNode(bool $deep = false): Element {}
    public function contains(?Element $other): bool {}
    public function getAttribute(string $name): ?string {}
    public function setAttribute(string $name, string $value): void {}
    public function removeAttribute(string $name): void {}
    public function hasAttribute(string $name): bool {}
    public function getBoundingClientRect(): DOMRect {}
    public function focus(): void {}
    public function blur(): void {}
    /** @param '2d' $kind */
    public function getContext(string $kind): ?CanvasRenderingContext2D {}
    public function toDataURL(): string {}
}

class Document extends EventTarget {
    public ?Element $body;
    public ?Element $head;
    public ?Element $documentElement;
    public string $cookie;
    public Location $location;
    public function querySelector(string $selectors): ?Element {}
    public function querySelectorAll(string $selectors): NodeList {}
    public function getElementById(string $id): ?Element {}
    public function createElement(string $tag): Element {}
    public function createTextNode(string $text): Element {}
}

class Headers {
    public function get(string $name): ?string {}
    public function has(string $name): bool {}
}

class Response {
    public bool $ok;
    public int $status;
    public string $statusText;
    public string $url;
    public bool $redirected;
    public string $type;
    public bool $bodyUsed;
    public Headers $headers;
    /** Settles with a string. */
    public function text(): Promise {}
    /** Settles with Data (or a scalar). */
    public function json(): Promise {}
}

class Storage {
    public int $length;
    public function getItem(string $key): ?string {}
    public function setItem(string $key, string $value): void {}
    public function removeItem(string $key): void {}
    public function clear(): void {}
    public function key(int $index): ?string {}
}

class Location {
    public string $href;
    public string $protocol;
    public string $host;
    public string $hostname;
    public string $port;
    public string $pathname;
    public string $search;
    public string $hash;
    public string $origin;
    public function assign(string $url): void {}
    public function replace(string $url): void {}
    public function reload(): void {}
    public function toString(): string {}
}

class History {
    public int $length;
    public mixed $state;
    public function pushState(mixed $state, string $unused, ?string $url = null): void {}
    public function replaceState(mixed $state, string $unused, ?string $url = null): void {}
    public function back(): void {}
    public function forward(): void {}
    public function go(int $delta = 0): void {}
}

class Navigator {
    public string $userAgent;
    public string $language;
}

class Performance {
    public float $timeOrigin;
    /** Milliseconds since the page started; never goes back. */
    public function now(): float {}
}

class Console {
    public function log(mixed ...$args): void {}
    public function info(mixed ...$args): void {}
    public function warn(mixed ...$args): void {}
    public function error(mixed ...$args): void {}
    public function debug(mixed ...$args): void {}
}

/** Text messages only. Events: open, message ($event->data), error, close. */
class WebSocket extends EventTarget {
    public string $url;
    public int $readyState;
    public string $protocol;
    public string $extensions;
    public int $bufferedAmount;
    public string $binaryType;
    public ?Closure $onopen;
    public ?Closure $onmessage;
    public ?Closure $onerror;
    public ?Closure $onclose;
    public int $CONNECTING;
    public int $OPEN;
    public int $CLOSING;
    public int $CLOSED;
    public function send(string $data): void {}
    public function close(?int $code = null, ?string $reason = null): void {}
}

/** Async only; text responses only. */
class XMLHttpRequest extends EventTarget {
    public int $readyState;
    public int $status;
    public string $statusText;
    public string $responseText;
    public string $response;
    public string $responseURL;
    /** '' or 'text' */
    public string $responseType;
    public ?Closure $onreadystatechange;
    public ?Closure $onload;
    public ?Closure $onerror;
    public ?Closure $onloadend;
    public function open(string $method, string $url): void {}
    public function setRequestHeader(string $name, string $value): void {}
    public function send(?string $body = null): void {}
    public function getResponseHeader(string $name): ?string {}
    public function getAllResponseHeaders(): string {}
}

/** Events: success, error (and upgradeneeded for open). */
class IDBRequest extends EventTarget {
    public mixed $result;
    public mixed $error;
    public string $readyState;
    public ?Closure $onsuccess;
    public ?Closure $onerror;
    public ?Closure $onupgradeneeded;
}

class IDBObjectStore {
    public string $name;
    public ?string $keyPath;
    public bool $autoIncrement;
    public function put(mixed $value, mixed $key = null): IDBRequest {}
    public function add(mixed $value, mixed $key = null): IDBRequest {}
    public function get(mixed $key): IDBRequest {}
    public function delete(mixed $key): IDBRequest {}
    public function getAll(): IDBRequest {}
    public function count(): IDBRequest {}
    public function clear(): IDBRequest {}
}

/** Events: complete, error, abort. */
class IDBTransaction extends EventTarget {
    public string $mode;
    public ?Closure $oncomplete;
    public ?Closure $onerror;
    public ?Closure $onabort;
    public function objectStore(string $name): IDBObjectStore {}
}

class IDBDatabase extends EventTarget {
    public string $name;
    public int $version;
    public Data $objectStoreNames;
    /** @param array{keyPath?: string, autoIncrement?: bool}|null $options */
    public function createObjectStore(string $name, ?array $options = null): IDBObjectStore {}
    public function deleteObjectStore(string $name): void {}
    /** @param 'readonly'|'readwrite' $mode */
    public function transaction(string|array $stores, string $mode = 'readonly'): IDBTransaction {}
    public function close(): void {}
}

class IDBFactory {
    public function open(string $name, ?int $version = null): IDBRequest {}
    public function deleteDatabase(string $name): IDBRequest {}
}

class Window extends EventTarget {
    public Document $document;
    public Console $console;
    public Location $location;
    public History $history;
    public Navigator $navigator;
    public Performance $performance;
    public Storage $localStorage;
    public Storage $sessionStorage;
    public IDBFactory $indexedDB;
    public int $innerWidth;
    public int $innerHeight;
    public float $devicePixelRatio;
    public float $scrollX;
    public float $scrollY;
    public float $pageXOffset;
    public float $pageYOffset;
    public function scrollTo(float $x, float $y): void {}
    public function scroll(float $x, float $y): void {}
    public function scrollBy(float $x, float $y): void {}
    public function setTimeout(callable $f, int|float $ms = 0): int {}
    public function setInterval(callable $f, int|float $ms = 0): int {}
    public function clearTimeout(?int $handle): void {}
    public function clearInterval(?int $handle): void {}
    public function requestAnimationFrame(callable $f): int {}
    public function cancelAnimationFrame(?int $handle): void {}
    /** @param array{method?: string, headers?: array<string, string>, body?: string, credentials?: string}|null $init */
    public function fetch(string $input, ?array $init = null): Promise {}
    public function WebSocket(string $url, string|array|null $protocols = null): WebSocket {}
    public function XMLHttpRequest(): XMLHttpRequest {}
}
