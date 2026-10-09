typedef unsigned long size_t;
typedef long ssize_t;
extern int printf(const char *, ...);
extern int snprintf(char *, size_t, const char *, ...);
extern int sprintf(char *, const char *, ...);
extern void *malloc(size_t);
extern void free(void *);
extern void *memcpy(void *, const void *, size_t);
extern void *memset(void *, int, size_t);
extern size_t strlen(const char *);
extern int strcmp(const char *, const char *);
extern void qsort(void *, size_t, size_t, int (*)(const void *, const void *));
extern void *bsearch(const void *, const void *, size_t, size_t, int (*)(const void *, const void *));
extern int setjmp(void *) __attribute__((returns_twice));
extern void longjmp(void *, int) __attribute__((noreturn));
extern double sin(double), pow(double, double), sqrt(double);
extern float sqrtf(float), powf(float, float);
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);
extern int pthread_mutex_lock(void *), pthread_mutex_unlock(void *);
extern int atexit(void (*)(void));
extern long strtol(const char *, char **, int);
extern double strtod(const char *, char **);
extern int open(const char *, int, ...);
extern ssize_t read(int, void *, size_t);
extern int close(int);
extern int stat(const char *, void *);
extern void *dlsym(void *, const char *);
extern void *dlopen(const char *, int);
extern int __android_log_print(int, const char *, const char *, ...);
extern int fprintf(void *, const char *, ...);
extern void *stderr;

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)

static int ctor_ran;
__attribute__((constructor)) static void ctor(void) { ctor_ran = 1; }

__thread int tls_counter = 5;
__thread char tls_buf[100];

static int cmp_int(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }

static void *thr(void *arg) {
    tls_counter += (int)(long)arg;
    return (void *)(long)(tls_counter * 2);
}

static int lock[10];
static int shared;
static void *thr_inc(void *arg) {
    for (int i = 0; i < 1000; i++) { pthread_mutex_lock(lock); shared++; pthread_mutex_unlock(lock); }
    return 0;
}

static int atexit_ran;
static void at_exit(void) { printf("atexit del guest ejecutado\n"); }

double dsum(double a, double b, int n) { return a + b + n; }
long fact(long n) { return n <= 1 ? 1 : n * fact(n - 1); }

int run_all(void) {
    char buf[128];
    CHECK(ctor_ran == 1);
    // printf-family con enteros, cadenas y floats
    int n = snprintf(buf, sizeof buf, "%d %s %5.2f %ld %x %c|%-4d|%05d", -42, "hola", 3.14159, 123456789012L, 0xbeef, 'Z', 7, 33);
    CHECK(n == (int)strlen(buf));
    CHECK(strcmp(buf, "-42 hola  3.14 123456789012 beef Z|7   |00033") == 0);
    // muchos argumentos (pila)
    snprintf(buf, sizeof buf, "%d %d %d %d %d %d %d %d %d %d", 1, 2, 3, 4, 5, 6, 7, 8, 9, 10);
    CHECK(strcmp(buf, "1 2 3 4 5 6 7 8 9 10") == 0);
    snprintf(buf, sizeof buf, "%f %f %f %f %f %f %f %f %f %f", 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.5, 10.25);
    CHECK(strcmp(buf, "1.000000 2.000000 3.000000 4.000000 5.000000 6.000000 7.000000 8.000000 9.500000 10.250000") == 0);
    snprintf(buf, sizeof buf, "%s %s %s %s %s %s %s %s", "a", "b", "c", "d", "e", "f", "g", "h");
    CHECK(strcmp(buf, "a b c d e f g h") == 0);
    // malloc/memcpy
    char *p = malloc(64);
    memset(p, 'x', 63); p[63] = 0;
    CHECK(strlen(p) == 63);
    free(p);
    // qsort / bsearch con callbacks guest
    int v[] = {9, 3, 7, 1, 8, 2, 6, 4, 5, 0, 15, 11, 13, 12, 14, 10, 19, 17, 18, 16};
    qsort(v, 20, sizeof(int), cmp_int);
    for (int i = 0; i < 20; i++) CHECK(v[i] == i);
    int key = 13;
    int *f = bsearch(&key, v, 20, sizeof(int), cmp_int);
    CHECK(f && *f == 13 && f == &v[13]);
    key = 99;
    CHECK(bsearch(&key, v, 20, sizeof(int), cmp_int) == 0);
    // setjmp/longjmp
    long jb[32];
    volatile int stage = 0;
    int r = setjmp(jb);
    if (r == 0) { stage = 1; longjmp(jb, 7); }
    CHECK(r == 7 && stage == 1);
    // math: host libm con flotantes en v0..v7
    CHECK(sqrt(2.0) > 1.41421356 && sqrt(2.0) < 1.41421357);
    CHECK(pow(2.0, 10.0) == 1024.0);
    CHECK(sqrtf(16.0f) == 4.0f);
    CHECK(powf(2.0f, 3.0f) == 8.0f);
    double s = sin(1.0);
    CHECK(s > 0.8414 && s < 0.8415);
    // strtol / strtod
    CHECK(strtol("  -1234xyz", 0, 10) == -1234);
    CHECK(strtod("2.5e3", 0) == 2500.0);
    // TLS (TLSDESC) y hilos
    CHECK(tls_counter == 5);
    tls_buf[0] = 'q';
    unsigned long t1, t2;
    void *rv1, *rv2;
    pthread_create(&t1, 0, thr, (void *)10);
    pthread_join(t1, &rv1);
    pthread_create(&t2, 0, thr, (void *)20);
    pthread_join(t2, &rv2);
    CHECK((long)rv1 == 30);   // 5+10 en el TLS del hilo, *2
    CHECK((long)rv2 == 50);   // 5+20 (cada hilo parte de la imagen inicial), *2
    CHECK(tls_counter == 5 && tls_buf[0] == 'q');
    // mutex con varios hilos
    unsigned long th[4];
    for (int i = 0; i < 4; i++) pthread_create(&th[i], 0, thr_inc, 0);
    for (int i = 0; i < 4; i++) pthread_join(th[i], 0);
    CHECK(shared == 4000);
    // E/S: open (flags convertidos), read, stat (struct convertida)
    int fd = open("/proc/self/status", 0);
    CHECK(fd >= 0);
    char sb[32];
    CHECK(read(fd, sb, 5) == 5);
    close(fd);
    long st[18];
    CHECK(stat("/", st) == 0);
    unsigned mode = *(unsigned *)((char *)st + 16);   // st_mode en el layout arm64
    CHECK((mode & 0170000) == 0040000);
    // dlsym del propio guest y de HLE
    void *fp = dlsym(0, "fact");
    CHECK(fp != 0 && ((long (*)(long))fp)(10) == 3628800);
    void *sp = dlsym(0, "strlen");
    CHECK(sp != 0 && ((size_t (*)(const char *))sp)("abcd") == 4);
    atexit(at_exit);
    printf("run_all: %d fallos\n", fails);
    return fails;
}
