/* Every POSIX call WASI preview 1 lacks, failing (see posix_stubs.h). */
#include <errno.h>
#include <stdarg.h>
#include "posix_stubs.h"
#include "netdb.h"
#include <sys/mman.h>

int h_errno;

void *dlopen(const char *f, int m) { (void)f; (void)m; return NULL; }
void *dlsym(void *h, const char *n) { (void)h; (void)n; return NULL; }
int dlclose(void *h) { (void)h; return 0; }
char *dlerror(void) { return "dynamic loading is not available"; }

struct passwd *getpwnam(const char *n) { (void)n; return NULL; }
struct passwd *getpwuid(uid_t u) { (void)u; return NULL; }
struct group *getgrnam(const char *n) { (void)n; return NULL; }
struct group *getgrgid(gid_t g) { (void)g; return NULL; }
uid_t getuid(void) { return 0; }
uid_t geteuid(void) { return 0; }
gid_t getgid(void) { return 0; }
gid_t getegid(void) { return 0; }
int getgroups(int n, gid_t l[]) { (void)n; (void)l; return 0; }
pid_t getppid(void) { return 0; }
int chown(const char *p, uid_t u, gid_t g) { (void)p; (void)u; (void)g; errno = ENOSYS; return -1; }
int lchown(const char *p, uid_t u, gid_t g) { (void)p; (void)u; (void)g; errno = ENOSYS; return -1; }
mode_t umask(mode_t m) { (void)m; return 022; }
FILE *popen(const char *c, const char *m) { (void)c; (void)m; errno = ENOSYS; return NULL; }
int pclose(FILE *f) { (void)f; errno = ENOSYS; return -1; }
int getdtablesize(void) { return 1024; }
/* Only a hint to the kernel (zend_alloc frees huge pages with it); nothing to do. */
int madvise(void *a, size_t l, int advice) { (void)a; (void)l; (void)advice; return 0; }
char *mktemp(char *t) { if (t) t[0] = 0; errno = ENOSYS; return t; }

void openlog(const char *i, int o, int f) { (void)i; (void)o; (void)f; }
void syslog(int p, const char *f, ...) { (void)p; (void)f; }
void closelog(void) {}

int socket(int d, int t, int p) { (void)d; (void)t; (void)p; errno = ENOSYS; return -1; }
int bind(int s, const struct sockaddr *a, socklen_t l) { (void)s; (void)a; (void)l; errno = ENOSYS; return -1; }
int listen(int s, int b) { (void)s; (void)b; errno = ENOSYS; return -1; }
int connect(int s, const struct sockaddr *a, socklen_t l) { (void)s; (void)a; (void)l; errno = ENOSYS; return -1; }
ssize_t sendto(int s, const void *b, size_t n, int f, const struct sockaddr *a, socklen_t l) { (void)s; (void)b; (void)n; (void)f; (void)a; (void)l; errno = ENOSYS; return -1; }
ssize_t recvfrom(int s, void *b, size_t n, int f, struct sockaddr *a, socklen_t *l) { (void)s; (void)b; (void)n; (void)f; (void)a; (void)l; errno = ENOSYS; return -1; }
int getsockname(int s, struct sockaddr *a, socklen_t *l) { (void)s; (void)a; (void)l; errno = ENOSYS; return -1; }
int getpeername(int s, struct sockaddr *a, socklen_t *l) { (void)s; (void)a; (void)l; errno = ENOSYS; return -1; }
int setsockopt(int s, int v, int o, const void *x, socklen_t l) { (void)s; (void)v; (void)o; (void)x; (void)l; errno = ENOSYS; return -1; }
int getsockopt(int s, int v, int o, void *x, socklen_t *l) { (void)s; (void)v; (void)o; (void)x; (void)l; errno = ENOSYS; return -1; }
int socketpair(int d, int t, int p, int v[2]) { (void)d; (void)t; (void)p; (void)v; errno = ENOSYS; return -1; }

struct hostent *gethostbyname(const char *n) { (void)n; h_errno = NO_RECOVERY; return NULL; }
struct hostent *gethostbyaddr(const void *a, socklen_t l, int t) { (void)a; (void)l; (void)t; h_errno = NO_RECOVERY; return NULL; }
struct servent *getservbyname(const char *n, const char *p) { (void)n; (void)p; return NULL; }
struct servent *getservbyport(int port, const char *p) { (void)port; (void)p; return NULL; }
struct protoent *getprotobyname(const char *n) { (void)n; return NULL; }
struct protoent *getprotobynumber(int n) { (void)n; return NULL; }
int getaddrinfo(const char *n, const char *s, const struct addrinfo *h, struct addrinfo **r) { (void)n; (void)s; (void)h; *r = NULL; return EAI_FAIL; }
void freeaddrinfo(struct addrinfo *a) { (void)a; }
const char *gai_strerror(int e) { (void)e; return "name resolution is not available"; }
int getnameinfo(const struct sockaddr *a, socklen_t l, char *h, socklen_t hl, char *s, socklen_t sl, int f) { (void)a; (void)l; (void)h; (void)hl; (void)s; (void)sl; (void)f; return EAI_FAIL; }
