// Runs before any page script. Turns host handles into PHP objects: property reads and writes
// and method calls on them become host requests, so pages use the same web platform names the
// Dart, JS and Python packs do ($document->querySelector('#count')->textContent). `__host`
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

// The page's globals. pack.c makes these superglobals, so functions and closures see them
// without `global` or `use`.
$window = __request(['op' => 'global', 'name' => 'window']);
$document = __request(['op' => 'global', 'name' => 'document']);
$console = __request(['op' => 'global', 'name' => 'console']);
$location = $window->location;
$navigator = $window->navigator;
$history = $window->history;
$localStorage = $window->localStorage;
$sessionStorage = $window->sessionStorage;

// Timers and the network come from the host, as for every pack. Extra arguments are passed to
// the callback, as in browsers.
function setTimeout(callable $f, int|float $ms = 0, mixed ...$args): int {
	return $GLOBALS['window']->setTimeout($args ? fn() => $f(...$args) : Closure::fromCallable($f), $ms);
}

function setInterval(callable $f, int|float $ms = 0, mixed ...$args): int {
	return $GLOBALS['window']->setInterval($args ? fn() => $f(...$args) : Closure::fromCallable($f), $ms);
}

function clearTimeout(?int $handle): void {
	$GLOBALS['window']->clearTimeout($handle);
}

function clearInterval(?int $handle): void {
	$GLOBALS['window']->clearInterval($handle);
}

function requestAnimationFrame(callable $f): int {
	return $GLOBALS['window']->requestAnimationFrame(Closure::fromCallable($f));
}

/** The network, through the page's gate: a host Promise of a Response; `->then($ok, $err)`
 * calls one of them later, from the event loop. */
function fetch(string $input, ?array $init = null): HostObject {
	return $GLOBALS['window']->fetch($input, $init);
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
