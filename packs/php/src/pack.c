/* The PHP interpreter for the browser's PHP pack: php-src's embed SAPI, compiled to
 * wasm32-wasip1 (see build.sh).
 *
 * The browser runs this in wasmtime with no files, network or environment. Everything the
 * page can reach goes through one import, `host.request`, which takes a JSON request and
 * returns a JSON reply, the same shapes as the JS and Python packs' (see prelude.php).
 *
 * Exports, the same ABI as the other interpreters with a `php_` prefix: `php_alloc`/`php_free`
 * (memory for strings), `php_run` (run a page script), `php_fire` (call a listener or timer)
 * and `php_release` (forget one). `php_run` and `php_fire` return 0 on success, or a packed
 * (ptr << 32 | len) error message the host must `php_free`. `php_eval` runs console input and
 * always returns a packed JSON reply. */

#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "php.h"
#include "php_main.h"
#include "php_embed.h"
#include "SAPI.h"
#include "zend_exceptions.h"
#include "zend_smart_str.h"
#include "ext/json/php_json.h"

#include "prelude.h" /* prelude_php, prelude_php_len: generated from prelude.php by build.sh */

__attribute__((import_module("host"), import_name("request")))
extern uint64_t host_request(const char *json, uint32_t len);

#define EXPORT(name) __attribute__((export_name(#name)))

/* The host API this pack was built for (packs/HOST_API, passed in by build.sh); the browser
 * refuses a mismatch. */
EXPORT(php_host_api) uint32_t php_host_api(void) { return HOST_API; }

EXPORT(php_alloc) void *php_alloc(uint32_t len) { return malloc(len ? len : 1); }
EXPORT(php_free) void php_free(void *ptr, uint32_t len) { (void)len; free(ptr); }

static uint64_t pack(const char *s, size_t len) {
	char *out = php_alloc(len);
	memcpy(out, s, len);
	return ((uint64_t)(uintptr_t)out << 32) | (uint32_t)len;
}

/* Output (`echo`, `print`, warnings) collects here and goes to the console when a call into
 * the pack ends, as one entry. */
static smart_str output;

static size_t ub_write(const char *str, size_t len) {
	smart_str_appendl(&output, str, len);
	return len;
}

static void flush_output(void) {
	if (!output.s || !ZSTR_LEN(output.s)) {
		return;
	}
	/* {"op":"console","text":...}, with the text JSON-escaped by PHP's own encoder. */
	zval text;
	ZVAL_STR(&text, smart_str_extract(&output));
	smart_str req = {0};
	smart_str_appends(&req, "{\"op\":\"console\",\"text\":");
	php_json_encode(&req, &text, PHP_JSON_UNESCAPED_UNICODE | PHP_JSON_UNESCAPED_SLASHES | PHP_JSON_INVALID_UTF8_SUBSTITUTE);
	smart_str_appendc(&req, '}');
	smart_str_0(&req);
	uint64_t reply = host_request(ZSTR_VAL(req.s), ZSTR_LEN(req.s));
	php_free((void *)(uintptr_t)(reply >> 32), (uint32_t)reply);
	smart_str_free(&req);
	zval_ptr_dtor(&text);
}

/* The first fatal error (a parse error, an uncaught exception, running out of memory) in the
 * current call, returned to the host. Warnings and notices go to the console instead. */
static zend_string *fatal;
static void (*php_error_cb_orig)(int, zend_string *, const uint32_t, zend_string *);

static void error_cb(int type, zend_string *file, const uint32_t line, zend_string *message) {
	const int fatal_types = E_ERROR | E_PARSE | E_CORE_ERROR | E_COMPILE_ERROR | E_USER_ERROR | E_RECOVERABLE_ERROR;
	if (type & fatal_types) {
		if (!fatal) {
			fatal = file ? zend_strpprintf(0, "%s in %s:%u", ZSTR_VAL(message), ZSTR_VAL(file), line)
			             : zend_string_copy(message);
		}
	} else {
		const char *kind = (type & (E_WARNING | E_USER_WARNING | E_CORE_WARNING | E_COMPILE_WARNING)) ? "Warning"
			: (type & (E_DEPRECATED | E_USER_DEPRECATED)) ? "Deprecated" : "Notice";
		smart_str_append_printf(&output, "%s: %s in %s:%u\n", kind, ZSTR_VAL(message), file ? ZSTR_VAL(file) : "-", line);
	}
	/* display_errors is off, so this only records error_get_last() and, for fatal errors,
	 * bails out to the zend_try in `guarded`. */
	php_error_cb_orig(type, file, line, message);
}

/* `__host(string $json): string`, the prelude's one way out. */
ZEND_BEGIN_ARG_WITH_RETURN_TYPE_INFO_EX(arginfo_host, 0, 1, IS_STRING, 0)
	ZEND_ARG_TYPE_INFO(0, json, IS_STRING, 0)
ZEND_END_ARG_INFO()

static PHP_FUNCTION(__host) {
	zend_string *req;
	ZEND_PARSE_PARAMETERS_START(1, 1)
		Z_PARAM_STR(req)
	ZEND_PARSE_PARAMETERS_END();
	uint64_t reply = host_request(ZSTR_VAL(req), ZSTR_LEN(req));
	char *ptr = (char *)(uintptr_t)(reply >> 32);
	RETVAL_STRINGL(ptr, (uint32_t)reply);
	php_free(ptr, (uint32_t)reply);
}

static const zend_function_entry pack_functions[] = {
	ZEND_FE(__host, arginfo_host)
	ZEND_FE_END
};

/* Lower precedence than an INI file (there is none): errors aren't printed (error_cb reports
 * them), blocking calls are disabled, and PHP's own memory limit sits below the browser's per-page cap so running out is a
 * PHP fatal error rather than a trap. */
static void ini_defaults(HashTable *ini) {
	zval v;
	ZVAL_STRING(&v, "0"); zend_hash_str_update(ini, "display_errors", sizeof("display_errors") - 1, &v);
	ZVAL_STRING(&v, "0"); zend_hash_str_update(ini, "log_errors", sizeof("log_errors") - 1, &v);
	ZVAL_STRING(&v, "256M"); zend_hash_str_update(ini, "memory_limit", sizeof("memory_limit") - 1, &v);
	/* Blocking would freeze the browser's UI thread; the rest can't work without processes. */
	ZVAL_STRING(&v, "sleep,usleep,time_nanosleep,time_sleep_until,exec,shell_exec,system,passthru,proc_open,popen");
	zend_hash_str_update(ini, "disable_functions", sizeof("disable_functions") - 1, &v);
}

static bool started;
static zend_string *prelude_error;

static void start(void) {
	if (started) {
		return;
	}
	started = true;
	php_embed_module.ub_write = ub_write;
	php_embed_module.ini_defaults = ini_defaults;
	php_embed_module.phpinfo_as_text = 1;
	php_embed_init(0, NULL);
	zend_register_functions(NULL, pack_functions, NULL, MODULE_PERSISTENT);
	php_error_cb_orig = zend_error_cb;
	zend_error_cb = error_cb;
	zend_try {
		zend_eval_stringl_ex((const char *)prelude_php, prelude_php_len, NULL, "<prelude>", 1);
	} zend_end_try();
	/* Without the prelude no page can work: every call reports why. */
	prelude_error = fatal;
	fatal = NULL;
}

/* Run `body` with fatal errors caught; 0, or the packed error. */
static uint64_t guarded(void (*body)(void *), void *arg) {
	start();
	if (prelude_error) {
		return pack(ZSTR_VAL(prelude_error), ZSTR_LEN(prelude_error));
	}
	if (fatal) {
		zend_string_release(fatal);
		fatal = NULL;
	}
	zend_try {
		body(arg);
		if (EG(exception)) {
			zend_exception_error(EG(exception), E_ERROR);
		}
	} zend_end_try();
	flush_output();
	if (!fatal) {
		return 0;
	}
	uint64_t packed = pack(ZSTR_VAL(fatal), ZSTR_LEN(fatal));
	zend_string_release(fatal);
	fatal = NULL;
	return packed;
}

/* A page script may start with `<?php` (and end with `?>`); a script tag's code needn't. */
static void skip_open_tag(const char **src, size_t *len) {
	while (*len && (**src == ' ' || **src == '\t' || **src == '\n' || **src == '\r')) {
		(*src)++, (*len)--;
	}
	if (*len >= 5 && !strncasecmp(*src, "<?php", 5)) {
		*src += 5, *len -= 5;
	}
}

struct source { const char *code; size_t len; const char *name; zval *result; };

static void run_source(void *arg) {
	struct source *s = arg;
	zend_eval_stringl(s->code, s->len, s->result, s->name);
}

EXPORT(php_run) uint64_t php_run(char *src, uint32_t len, char *name, uint32_t name_len) {
	start(); /* before estrndup: PHP's allocator starts with the engine */
	char *name_z = estrndup(name, name_len);
	const char *code = src;
	size_t code_len = len;
	skip_open_tag(&code, &code_len);
	struct source s = {code, code_len, name_z, NULL};
	uint64_t status = guarded(run_source, &s);
	efree(name_z);
	php_free(src, len);
	php_free(name, name_len);
	return status;
}

/* Call a prelude function with an id and an optional string. */
struct prelude_call { const char *fn; uint32_t id; const char *text; size_t len; };

static void call_prelude(void *arg) {
	struct prelude_call *c = arg;
	zval fn, ret, params[2];
	ZVAL_STRING(&fn, c->fn);
	ZVAL_LONG(&params[0], c->id);
	ZVAL_STRINGL(&params[1], c->text ? c->text : "", c->len);
	ZVAL_UNDEF(&ret);
	call_user_function(NULL, NULL, &fn, &ret, c->text ? 2 : 1, params);
	zval_ptr_dtor(&ret);
	zval_ptr_dtor(&params[1]);
	zval_ptr_dtor(&fn);
}

EXPORT(php_fire) uint64_t php_fire(uint32_t id, char *args, uint32_t len) {
	struct prelude_call c = {"__fire", id, args, len};
	uint64_t status = guarded(call_prelude, &c);
	php_free(args, len);
	return status;
}

EXPORT(php_release) void php_release(uint32_t id) {
	struct prelude_call c = {"__release", id, NULL, 0};
	uint64_t status = guarded(call_prelude, &c);
	if (status) {
		php_free((void *)(uintptr_t)(status >> 32), (uint32_t)status);
	}
}

/* Console input: an expression's value is shown (as the prelude's __show formats it); input
 * that isn't an expression runs as statements, like `php -a`. */
struct eval_input { const char *code; size_t len; zend_string *shown; };

static void eval_input(void *arg) {
	struct eval_input *e = arg;
	zval value;
	ZVAL_UNDEF(&value);
	/* Try it as an expression first, quietly: a parse error just means it's statements. */
	zend_string *expr = zend_strpprintf(0, "__show(%.*s);", (int)e->len, e->code);
	zend_op_array *op = NULL;
	zend_try {
		op = zend_compile_string(expr, "console", ZEND_COMPILE_POSITION_AFTER_OPEN_TAG);
	} zend_end_try();
	if (EG(exception)) {
		zend_clear_exception();
	}
	if (fatal) {
		zend_string_release(fatal);
		fatal = NULL;
	}
	if (op) {
		destroy_op_array(op);
		efree_size(op, sizeof(zend_op_array));
		zend_eval_stringl(ZSTR_VAL(expr), ZSTR_LEN(expr), &value, "console");
	} else {
		/* Statements: put back the `;` dropped for the expression attempt. */
		zend_string *stmts = zend_strpprintf(0, "%.*s;", (int)e->len, e->code);
		zend_eval_stringl(ZSTR_VAL(stmts), ZSTR_LEN(stmts), NULL, "console");
		zend_string_release(stmts);
	}
	zend_string_release(expr);
	if (Z_TYPE(value) == IS_STRING) {
		e->shown = zend_string_copy(Z_STR(value));
	}
	zval_ptr_dtor(&value);
}

EXPORT(php_eval) uint64_t php_eval(char *src, uint32_t len) {
	struct eval_input e = {src, len, NULL};
	skip_open_tag(&e.code, &e.len);
	/* Statements end with `;`; an expression shouldn't, so drop one trailing `;`. */
	while (e.len && strchr(" \t\r\n;", e.code[e.len - 1])) {
		e.len--;
	}
	uint64_t status = guarded(eval_input, &e);
	php_free(src, len);
	zval reply, value;
	array_init(&reply);
	if (status) {
		ZVAL_STRINGL(&value, (char *)(uintptr_t)(status >> 32), (uint32_t)status);
		php_free((void *)(uintptr_t)(status >> 32), (uint32_t)status);
		add_assoc_zval(&reply, "err", &value);
	} else {
		ZVAL_STR(&value, e.shown ? e.shown : ZSTR_EMPTY_ALLOC());
		add_assoc_zval(&reply, "ok", &value);
	}
	smart_str json = {0};
	php_json_encode(&json, &reply, PHP_JSON_UNESCAPED_UNICODE | PHP_JSON_UNESCAPED_SLASHES | PHP_JSON_INVALID_UTF8_SUBSTITUTE);
	zval_ptr_dtor(&reply);
	uint64_t packed = pack(ZSTR_VAL(json.s), ZSTR_LEN(json.s));
	smart_str_free(&json);
	return packed;
}
