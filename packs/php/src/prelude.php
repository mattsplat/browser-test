// Runs before any page script. Turns host handles into PHP objects: property reads and writes
// and method calls on them become host requests, so pages use the same web platform names the
// Dart, JS and Python packs do (Unscripted::document()->querySelector('#count')->textContent). `__host`
// (from pack.c) is the only way out. No `<?php` tag: pack.c compiles this as code.
//
// Unlike JS and Python, PHP's syntax says whether a member is read (`$el->focus`, __get) or
// called (`$el->focus()`, __call), so there is no list of method names to keep in step with
// browser/src/host.rs.

// Callbacks by id, and each closure's id (by object id), so passing the same closure again
// (to removeEventListener) gives the same id. The host says when an id can be released.
$GLOBALS['__callbacks'] = [];
$GLOBALS['__callback_ids'] = [];
$GLOBALS['__next_callback'] = 0;
// Live wrappers per handle the host marked releasable: when the last goes, the host frees it.
$GLOBALS['__held'] = [];

/** A value as it crosses to the host: null, scalars, a host object as {o: h}, a closure as
 * {cb: id}, a list as {arr: [...]}, any other array as {obj: {...}}. */
function __to_host(mixed $v): mixed {
	if ($v instanceof HostObject) {
		return ['o' => $v->__handle()];
	}
	if ($v instanceof Closure) {
		$key = spl_object_id($v);
		$id = $GLOBALS['__callback_ids'][$key] ?? null;
		if ($id === null) {
			$id = $GLOBALS['__next_callback']++;
			$GLOBALS['__callback_ids'][$key] = $id;
			$GLOBALS['__callbacks'][$id] = $v;
		}
		return ['cb' => $id];
	}
	if (is_array($v)) {
		return array_is_list($v)
			? ['arr' => array_map('__to_host', $v)]
			: ['obj' => (object) array_map('__to_host', $v)];
	}
	if ($v === null || is_scalar($v)) {
		return $v;
	}
	throw new TypeError('a ' . get_debug_type($v) . " can't be passed to the page yet");
}

function __from_host(mixed $v): mixed {
	if (is_array($v)) {
		if (array_key_exists('u', $v)) {
			return null;
		}
		if (array_key_exists('o', $v)) {
			return new HostObject($v['o'], array_key_exists('r', $v));
		}
	}
	// The host's numbers are doubles; whole ones read as ints, as they print in JS.
	if (is_float($v) && floor($v) === $v && abs($v) < 2 ** 53) {
		return (int) $v;
	}
	return $v;
}

function __request(array $message): mixed {
	$reply = json_decode(__host(json_encode($message, JSON_UNESCAPED_SLASHES | JSON_PRESERVE_ZERO_FRACTION)), true);
	if (array_key_exists('err', $reply)) {
		throw new Error($reply['err']);
	}
	return __from_host($reply['ok']);
}

/** A web platform object (window, document, an element, an event) the host holds. Lists (a
 * NodeList, `dataset`) work with `[$i]` / `['name']` and foreach; their size is ->length. */
final class HostObject implements ArrayAccess, IteratorAggregate {
	public function __construct(private int $h, private bool $r = false) {
		if ($r) {
			$GLOBALS['__held'][$h] = ($GLOBALS['__held'][$h] ?? 0) + 1;
		}
	}

	public function __destruct() {
		if (!$this->r) {
			return;
		}
		$left = ($GLOBALS['__held'][$this->h] ?? 1) - 1;
		if ($left) {
			$GLOBALS['__held'][$this->h] = $left;
		} else {
			unset($GLOBALS['__held'][$this->h]);
			__request(['op' => 'release', 'target' => $this->h]);
		}
	}

	public function __handle(): int {
		return $this->h;
	}

	public function __get(string $name): mixed {
		return __request(['op' => 'get', 'target' => $this->h, 'name' => $name]);
	}

	public function __set(string $name, mixed $value): void {
		__request(['op' => 'set', 'target' => $this->h, 'name' => $name, 'value' => __to_host($value)]);
	}

	public function __isset(string $name): bool {
		return $this->__get($name) !== null;
	}

	public function __call(string $name, array $args): mixed {
		return __request(['op' => 'call', 'target' => $this->h, 'name' => $name, 'args' => array_map('__to_host', $args)]);
	}

	public function offsetGet(mixed $key): mixed {
		return $this->__get((string) $key);
	}

	public function offsetSet(mixed $key, mixed $value): void {
		$this->__set((string) $key, $value);
	}

	public function offsetExists(mixed $key): bool {
		return $this->offsetGet($key) !== null;
	}

	public function offsetUnset(mixed $key): void {
		throw new Error("host objects' members can't be unset");
	}

	public function getIterator(): Generator {
		for ($i = 0, $n = $this->length; $i < $n; $i++) {
			yield $i => $this[$i];
		}
	}
}

/**
 * The browser, for page code: its objects and functions as static methods, so nothing takes a
 * name in the page's own code. `use Unscripted as U;` shortens it to `U::document()`.
 *
 * Return types are `object` (every one is a HostObject); the docblocks name the web interface,
 * for editors (typings/browser.stub.php declares them).
 */
final class Unscripted {
	/** @var array<string, HostObject> */
	private static array $objects = [];

	private static function get(string $name): HostObject {
		return self::$objects[$name] ??= match ($name) {
			'window', 'document', 'console' => __request(['op' => 'global', 'name' => $name]),
			default => self::window()->$name,
		};
	}

	/** @return Window */
	public static function window(): object { return self::get('window'); }
	/** @return Document */
	public static function document(): object { return self::get('document'); }
	/** @return Console */
	public static function console(): object { return self::get('console'); }
	/** @return Location */
	public static function location(): object { return self::get('location'); }
	/** @return History */
	public static function history(): object { return self::get('history'); }
	/** @return Navigator */
	public static function navigator(): object { return self::get('navigator'); }
	/** @return Performance */
	public static function performance(): object { return self::get('performance'); }
	/** @return Storage */
	public static function localStorage(): object { return self::get('localStorage'); }
	/** @return Storage */
	public static function sessionStorage(): object { return self::get('sessionStorage'); }
	/** @return IDBFactory */
	public static function indexedDB(): object { return self::get('indexedDB'); }

	// Timers and the network come from the host, as for every pack. Extra arguments are passed
	// to the callback, as in browsers.
	public static function setTimeout(callable $f, int|float $ms = 0, mixed ...$args): int {
		return self::window()->setTimeout($args ? fn() => $f(...$args) : Closure::fromCallable($f), $ms);
	}

	public static function setInterval(callable $f, int|float $ms = 0, mixed ...$args): int {
		return self::window()->setInterval($args ? fn() => $f(...$args) : Closure::fromCallable($f), $ms);
	}

	public static function clearTimeout(?int $handle): void {
		self::window()->clearTimeout($handle);
	}

	public static function clearInterval(?int $handle): void {
		self::window()->clearInterval($handle);
	}

	/** @param callable(float): void $f */
	public static function requestAnimationFrame(callable $f): int {
		return self::window()->requestAnimationFrame(Closure::fromCallable($f));
	}

	public static function cancelAnimationFrame(?int $handle): void {
		self::window()->cancelAnimationFrame($handle);
	}

	/**
	 * The network, through the page's gate: a host Promise of a Response; `->then($ok, $err)`
	 * calls one of them later, from the event loop.
	 * @param array{method?: string, headers?: array<string, string>, body?: string, credentials?: string}|null $init
	 * @return Promise
	 */
	public static function fetch(string $input, ?array $init = null): object {
		return self::window()->fetch($input, $init);
	}

	/** Text messages only. @return WebSocket */
	public static function WebSocket(string $url, string|array|null $protocols = null): object {
		return self::window()->WebSocket($url, $protocols);
	}

	/** Async only. @return XMLHttpRequest */
	public static function XMLHttpRequest(): object {
		return self::window()->XMLHttpRequest();
	}
}

// The page runs on the browser's UI thread, so it must never block: sleep() and its kin are
// disabled (pack.c's ini_defaults).

/** Console input's value, for the inspector. */
function __show(mixed $v): string {
	if ($v instanceof HostObject) {
		try {
			$tag = strtolower($v->tagName ?? '');
			if ($tag !== '') {
				$id = $v->id;
				return "<$tag" . ($id ? " id=\"$id\"" : '') . '>';
			}
		} catch (Throwable) {
		}
		return '<host object #' . $v->__handle() . '>';
	}
	return var_export($v, true);
}

// Called by the host (through pack.c) to call a listener or timer, and to release one it no
// longer uses.
function __fire(int $id, string $args): void {
	$f = $GLOBALS['__callbacks'][$id] ?? null;
	if ($f !== null) {
		$f(...array_map('__from_host', json_decode($args, true)));
	}
}

function __release(int $id): void {
	$f = $GLOBALS['__callbacks'][$id] ?? null;
	unset($GLOBALS['__callbacks'][$id]);
	if ($f !== null && ($GLOBALS['__callback_ids'][spl_object_id($f)] ?? null) === $id) {
		unset($GLOBALS['__callback_ids'][spl_object_id($f)]);
	}
}
