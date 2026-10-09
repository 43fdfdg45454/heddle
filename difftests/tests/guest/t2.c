typedef unsigned long size_t;
typedef long ssize_t;
typedef int pid_t;
extern int printf(const char *, ...);
extern int snprintf(char *, size_t, const char *, ...);
extern int sscanf(const char *, const char *, ...);
extern size_t strlen(const char *);
extern int strcmp(const char *, const char *);
extern int raise(int);
extern int kill(pid_t, int);
extern pid_t getpid(void);
extern pid_t fork(void);
extern pid_t waitpid(pid_t, int *, int);
extern void _exit(int) __attribute__((noreturn));
extern void exit(int) __attribute__((noreturn));
extern int sigaction(int, const void *, void *);
extern void (*signal(int, void (*)(int)))(int);
extern int pipe2(int *, int);
extern int epoll_create1(int);
extern int epoll_ctl(int, int, int, void *);
extern int epoll_wait(int, void *, int, int);
extern ssize_t write(int, const void *, size_t);
extern ssize_t read(int, void *, size_t);
extern int uname(void *);
extern unsigned long getauxval(unsigned long);
extern int dl_iterate_phdr(int (*)(void *, size_t, void *), void *);
extern long double sqrtl(long double);
extern int __cxa_atexit(void (*)(void *), void *, void *);
extern long fact(long);
extern int fcntl(int, int, ...);
extern int open(const char *, int, ...);
extern int close(int);
extern int fstat(int, void *);
extern void *mmap(void *, size_t, int, int, int, long);
extern int munmap(void *, size_t);
extern void *fopen(const char *, const char *);
extern char *fgets(char *, int, void *);
extern int fclose(void *);
extern int __errno_dummy;

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)

static volatile int got_usr1, got_sig;
static void h1(int s) { got_usr1 = s; }
struct ksa { int flags; void (*h)(int, void *, void *); unsigned long mask; void *restorer; } ;
static void h2(int s, void *info, void *uc) { got_sig = s; }

static int found_self;
static int phdr_cb(void *info, size_t sz, void *data) {
    const char *name = *(const char **)((char *)info + 8);
    int n = (int)strlen(name);
    if (n >= 8 && name[n-8]=='l' && name[n-7]=='i' && name[n-6]=='b' && name[n-5]=='t' && name[n-4]=='2') found_self = 1;
    (void)sz; (void)data;
    return 0;
}

static void ax(void *arg) { printf("cxa_atexit con arg=%ld\n", (long)arg); }

int run_all(void) {
    char buf[200];
    signal(10, h1);
    raise(10);
    CHECK(got_usr1 == 10);
    got_usr1 = 0;
    kill(getpid(), 10);
    CHECK(got_usr1 == 10);
    struct ksa sa = {0};
    sa.flags = 4;  // SA_SIGINFO
    sa.h = h2;
    CHECK(sigaction(12, &sa, 0) == 0);
    raise(12);
    CHECK(got_sig == 12);

    dl_iterate_phdr(phdr_cb, 0);
    CHECK(found_self == 1);

    // epoll con struct epoll_event arm64 (16 bytes)
    int p[2];
    CHECK(pipe2(p, 0x80000) == 0);   // O_CLOEXEC
    int ep = epoll_create1(0);
    CHECK(ep >= 0);
    struct { unsigned ev; unsigned pad; unsigned long data; } e = { 1, 0, 0x1122334455667788UL }, out[4];
    CHECK(epoll_ctl(ep, 1, p[0], &e) == 0);
    write(p[1], "x", 1);
    int n = epoll_wait(ep, out, 4, 1000);
    CHECK(n == 1 && out[0].data == 0x1122334455667788UL && (out[0].ev & 1));

    char un[390];
    uname(un);
    CHECK(strcmp(un + 4 * 65, "aarch64") == 0);
    CHECK((getauxval(16) & 1) != 0);

    snprintf(buf, sizeof buf, "%Lf %.3Lf", (long double)1.5L, sqrtl(2.0L));
    CHECK(strcmp(buf, "1.500000 1.414") == 0);
    int a, b; char w[20];
    CHECK(sscanf("12 -7 hola", "%d %d %19s", &a, &b, w) == 3);
    CHECK(a == 12 && b == -7 && strcmp(w, "hola") == 0);
    CHECK(fact(5) == 120);

    // O_DIRECTORY/O_NOFOLLOW (arm64: 0x4000/0x8000) convertidos a x86
    int fd = open("/tmp", 0x4000 | 0x20000);  // O_DIRECTORY|O_LARGEFILE
    CHECK(fd >= 0);
    int fl = fcntl(fd, 3);
    CHECK(fl >= 0 && (fl & 0x4000));
    close(fd);
    CHECK(open("/etc/hostname", 0x4000) < 0);   // no es directorio

    // mmap anonima
    char *m = mmap(0, 8192, 3, 0x22, -1, 0);
    CHECK(m != (char *)-1);
    m[8191] = 5;
    CHECK(munmap(m, 8192) == 0);

    void *fpp = fopen("/proc/self/stat", "r");
    CHECK(fpp != 0);
    CHECK(fgets(buf, 100, fpp) != 0);
    fclose(fpp);

    // fork
    pid_t c = fork();
    if (c == 0) _exit(3);
    int st = 0;
    waitpid(c, &st, 0);
    CHECK(((st >> 8) & 0xff) == 3);

    __cxa_atexit(ax, (void *)99, 0);
    printf("t2 run_all: %d fallos\n", fails);
    return fails;
}

static void segv_h(int s, void *i, void *u) { printf("SIGSEGV capturado en guest, addr=%p\n", *(void **)((char *)i + 16)); _exit(77); }
int segv_test(void) {
    struct ksa sa = {0};
    sa.flags = 4;
    sa.h = segv_h;
    sigaction(11, &sa, 0);
    volatile int *p = (volatile int *)0x10;
    return *p;
}
int exit_test(void) { __cxa_atexit(ax, (void *)7, 0); exit(5); }
