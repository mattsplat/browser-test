/* Force-included (-include) into every PHP source file. WASI preview 1 has no users, groups,
 * processes, syslog or sockets; these declarations let PHP's standard extension compile, and
 * posix_stubs.c makes every call fail, so page PHP reaches nothing outside the browser. */
#ifndef PHP_PACK_POSIX_STUBS_H
#define PHP_PACK_POSIX_STUBS_H
#ifdef __wasi__
#include <sys/types.h>
#include <stdio.h>
#include <sys/socket.h>

/* PHP's bailout (zend_try) uses sigsetjmp; Wasm exception handling lowers only setjmp and
 * longjmp, and there are no signal masks to save. */
#include <setjmp.h>
#define sigsetjmp(env, save) setjmp(env)
#define siglongjmp(env, val) longjmp(env, val)

/* No dynamic loading: dl() and extensions loaded at runtime fail. */
void *dlopen(const char *, int);
void *dlsym(void *, const char *);
int dlclose(void *);
char *dlerror(void);

/* No UNIX-domain sockets: PHP compiles its unix:// transport out when AF_UNIX is undefined. */
#undef AF_UNIX
#undef PF_UNIX

struct passwd { char *pw_name, *pw_passwd; uid_t pw_uid; gid_t pw_gid; char *pw_gecos, *pw_dir, *pw_shell; };
struct group { char *gr_name, *gr_passwd; gid_t gr_gid; char **gr_mem; };
struct passwd *getpwnam(const char *);
struct passwd *getpwuid(uid_t);
struct group *getgrnam(const char *);
struct group *getgrgid(gid_t);
uid_t getuid(void);
uid_t geteuid(void);
gid_t getgid(void);
gid_t getegid(void);
int getgroups(int, gid_t[]);
pid_t getppid(void);
int chown(const char *, uid_t, gid_t);
int lchown(const char *, uid_t, gid_t);
mode_t umask(mode_t);
FILE *popen(const char *, const char *);
int pclose(FILE *);
int getdtablesize(void);

#define LOG_EMERG 0
#define LOG_ALERT 1
#define LOG_CRIT 2
#define LOG_ERR 3
#define LOG_WARNING 4
#define LOG_NOTICE 5
#define LOG_INFO 6
#define LOG_DEBUG 7
#define LOG_PID 0x01
#define LOG_CONS 0x02
#define LOG_ODELAY 0x04
#define LOG_NDELAY 0x08
#define LOG_NOWAIT 0x10
#define LOG_PERROR 0x20
#define LOG_KERN (0<<3)
#define LOG_USER (1<<3)
#define LOG_MAIL (2<<3)
#define LOG_DAEMON (3<<3)
#define LOG_AUTH (4<<3)
#define LOG_SYSLOG (5<<3)
#define LOG_LPR (6<<3)
#define LOG_NEWS (7<<3)
#define LOG_UUCP (8<<3)
#define LOG_CRON (9<<3)
#define LOG_AUTHPRIV (10<<3)
#define LOG_LOCAL0 (16<<3)
#define LOG_LOCAL1 (17<<3)
#define LOG_LOCAL2 (18<<3)
#define LOG_LOCAL3 (19<<3)
#define LOG_LOCAL4 (20<<3)
#define LOG_LOCAL5 (21<<3)
#define LOG_LOCAL6 (22<<3)
#define LOG_LOCAL7 (23<<3)
void openlog(const char *, int, int);
void syslog(int, const char *, ...);
void closelog(void);

int socket(int, int, int);
int bind(int, const struct sockaddr *, socklen_t);
int listen(int, int);
int connect(int, const struct sockaddr *, socklen_t);
ssize_t sendto(int, const void *, size_t, int, const struct sockaddr *, socklen_t);
ssize_t recvfrom(int, void *, size_t, int, struct sockaddr *, socklen_t *);
int getsockname(int, struct sockaddr *, socklen_t *);
int getpeername(int, struct sockaddr *, socklen_t *);
int setsockopt(int, int, int, const void *, socklen_t);
int getsockopt(int, int, int, void *, socklen_t *);
int socketpair(int, int, int, int[2]);
char *mktemp(char *);
#ifndef MSG_OOB
#define MSG_OOB 0x0001
#endif
#ifndef SO_ERROR
#define SO_ERROR 4
#endif
#ifndef SOL_IP
#define SOL_IP 0
#endif
#endif
#endif
