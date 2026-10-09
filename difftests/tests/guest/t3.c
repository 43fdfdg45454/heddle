// atomicos / exclusivos LL-SC y LSE entre hilos, sobre el JIT con el monitor estricto
typedef unsigned long size_t;
extern int printf(const char *, ...);
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);
static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)

static long counter_llsc, counter_lse, counter_cas;
static unsigned char pad[128];
static volatile int spin;

static inline long llsc_add(long *p, long v) {
    long old, tmp; unsigned st;
    __asm__ volatile("1: ldxr %0, [%3]\n add %1, %0, %4\n stxr %w2, %1, [%3]\n cbnz %w2, 1b"
                     : "=&r"(old), "=&r"(tmp), "=&r"(st) : "r"(p), "r"(v) : "memory");
    return old;
}
static inline long lse_add(long *p, long v) {
    long old;
    __asm__ volatile(".arch_extension lse\n ldaddal %2, %0, [%1]" : "=&r"(old) : "r"(p), "r"(v) : "memory");
    return old;
}
static void *worker(void *arg) {
    for (int i = 0; i < 20000; i++) {
        llsc_add(&counter_llsc, 1);
        lse_add(&counter_lse, 2);
        long o;
        do { o = __atomic_load_n(&counter_cas, __ATOMIC_RELAXED); }
        while (!__atomic_compare_exchange_n(&counter_cas, &o, o + 3, 0, __ATOMIC_SEQ_CST, __ATOMIC_RELAXED));
    }
    return 0;
}
int run_all(void) {
    unsigned long t[4];
    for (int i = 0; i < 4; i++) pthread_create(&t[i], 0, worker, 0);
    for (int i = 0; i < 4; i++) pthread_join(t[i], 0);
    CHECK(counter_llsc == 80000);
    CHECK(counter_lse == 160000);
    CHECK(counter_cas == 240000);
    printf("t3 run_all: %d fallos\n", fails);
    return fails;
}
