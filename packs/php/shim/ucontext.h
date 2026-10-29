/* WASI has no ucontext. PHP only uses it for fibers, which this pack doesn't support:
 * zend_fiber_init_context is patched to fail first (see patches/), so these are never reached. */
#ifndef PHP_PACK_UCONTEXT_H
#define PHP_PACK_UCONTEXT_H
#include <stddef.h>

typedef struct { void *ss_sp; int ss_flags; size_t ss_size; } php_pack_stack_t;
typedef struct ucontext_t { struct ucontext_t *uc_link; php_pack_stack_t uc_stack; } ucontext_t;

static inline int getcontext(ucontext_t *u) { (void)u; __builtin_trap(); }
static inline void makecontext(ucontext_t *u, void (*f)(void), int argc, ...) { (void)u; (void)f; (void)argc; __builtin_trap(); }
static inline int swapcontext(ucontext_t *a, const ucontext_t *b) { (void)a; (void)b; __builtin_trap(); }
#endif
