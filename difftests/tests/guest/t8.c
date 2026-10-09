// t8: el cargador como el enlazador de bionic en ARM64.
//  * descarga real: tras el ultimo dlclose la biblioteca se desmapea; un puntero viejo (dato o funcion) da SIGSEGV
//    que el manejador del guest captura, y mprotect sobre su pagina falla (ENOMEM), como en dlfcn_test de bionic;
//  * una recarga es una carga nueva donde decida el enlazador (direccion aleatoria como ReserveWithAlignmentPadding);
//  * TLS initial-exec en una biblioteca abierta con dlopen: la carga falla con el texto de bionic;
//  * dependencias en ciclo (libt8a <-> libt8b): orden de constructores y destructores de bionic, cuenta de
//    referencias en la raiz del grupo;
//  * __cxa_thread_atexit_impl retiene la biblioteca hasta que el hilo ejecuta el destructor.
typedef unsigned long size_t;
extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern int dlclose(void *);
extern char *dlerror(void);
extern char *strstr(const char *, const char *);
extern int strncmp(const char *, const char *, size_t);
extern int mprotect(void *, size_t, int);
extern int *__errno(void);
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);
extern int sched_yield(void);
typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa; // struct sigaction de bionic LP64
extern int sigaction(int, const gsa *, gsa *);
extern int sigsetjmp(long *, int) __attribute__((returns_twice));
extern void siglongjmp(long *, int) __attribute__((noreturn));

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
#define RTLD_NOW 2
#define RTLD_NOLOAD 4
#define RTLD_GLOBAL 0x100
#define SYM(h, T, n) ((T)dlsym(h, n))

extern int t8_log[64], t8_n;   // libt8log.so (DT_NEEDED de esta biblioteca)

static int log_is(const int *want, int n) {
    if (t8_n != n) return 0;
    for (int i = 0; i < n; i++) if (t8_log[i] != want[i]) return 0;
    return 1;
}
static void log_print(const char *what) {
    printf("%s:", what);
    for (int i = 0; i < t8_n; i++) printf(" %d", t8_log[i]);
    printf("\n");
}

// ---- SIGSEGV capturado por el manejador guest ----
static long jb[33];
static volatile int seen;
static volatile unsigned long seen_addr;
static void h_segv(int s, void *si, void *uc) { (void)s; (void)uc; seen++; seen_addr = *(unsigned long *)((char *)si + 16); siglongjmp(jb, 1); }

// 1 si leer *p da SIGSEGV con si_addr == p
static int __attribute__((noinline)) read_faults(volatile int *p) {
    gsa sa = {4 /* SA_SIGINFO */, (void *)h_segv, 0, 0}, old;
    seen = 0;
    sigaction(11, &sa, &old);
    int r = 0;
    if (sigsetjmp(jb, 1) == 0) {
        (void)*p;
    } else {
        r = seen == 1 && seen_addr == (unsigned long)p;
    }
    sigaction(11, &old, 0);
    return r;
}

// 1 si llamar a f da SIGSEGV con si_addr == f (el pc)
static int __attribute__((noinline)) call_faults(int (*f)(void)) {
    gsa sa = {4, (void *)h_segv, 0, 0}, old;
    seen = 0;
    sigaction(11, &sa, &old);
    int r = 0;
    if (sigsetjmp(jb, 1) == 0) {
        f();
    } else {
        r = seen == 1 && seen_addr == (unsigned long)f;
    }
    sigaction(11, &old, 0);
    return r;
}

static void unload_for_real(void) {
    void *h = dlopen("libt8a.so", RTLD_NOW);
    CHECK(h != 0);
    if (!h) { printf("dlopen libt8a: %s\n", dlerror()); return; }
    volatile int *data = SYM(h, volatile int *, "a_data");
    int (*fn)(void) = SYM(h, int (*)(void), "a_func");
    CHECK(data && fn);
    if (!data || !fn) return;
    CHECK(*data == 1234 && fn() == 77);
    CHECK(read_faults(data) == 0);
    CHECK(dlclose(h) == 0);
    // desmapeada: el dato y el codigo viejos fallan como en un dispositivo
    CHECK(read_faults(data));
    CHECK(call_faults(fn));
    unsigned long page = (unsigned long)data & ~4095ul;
    CHECK(mprotect((void *)page, 4096, 0) == -1 && *__errno() == 12 /* ENOMEM */);
    // el handle viejo ya no vale
    CHECK(dlsym(h, "a_data") == 0);
    CHECK(dlclose(h) == -1);
    const char *e = dlerror();
    CHECK(e && strncmp(e, "dlclose failed: invalid handle", 30) == 0);
}

static void new_address(void) {
    // cada carga es nueva (direccion aleatoria dentro del relleno de alineacion, como bionic): alguna de varias
    // recargas tiene que caer en otro sitio, y cada una parte de la imagen inicial con un handle nuevo
    unsigned long first = 0;
    void *first_h = 0;
    int moved = 0, fresh = 1, newh = 1;
    for (int k = 0; k < 6; k++) {
        void *h = dlopen("libt8a.so", RTLD_NOW);
        CHECK(h != 0);
        if (!h) return;
        int *data = SYM(h, int *, "a_data");
        unsigned long a = (unsigned long)SYM(h, void *, "a_func");
        fresh &= *data == 1234;
        *data = 99;
        if (k == 0) { first = a; first_h = h; }
        else { moved |= a != first; newh &= h != first_h; }
        dlclose(h);
    }
    CHECK(moved);
    CHECK(fresh);
    CHECK(newh);
}

static void cycle(void) {
    t8_n = 0;
    void *ha = dlopen("libt8a.so", RTLD_NOW);
    CHECK(ha != 0);
    if (!ha) { printf("dlopen ciclo: %s\n", dlerror()); return; }
    // constructores de bionic: en profundidad desde la raiz, cada uno una vez: B (hijo de A) antes que A
    static const int ctors_a[] = {2, 1};
    CHECK(log_is(ctors_a, 2));
    int (*acb)(void) = SYM(ha, int (*)(void), "a_calls_b");
    int (*bca)(void) = SYM(ha, int (*)(void), "b_calls_a");   // dlsym(handle) busca en el grupo en anchura
    CHECK(acb && bca && acb() == 21 && bca() == 12);
    // libt8b ya esta cargada (en el grupo de libt8a): dlopen sube la cuenta de la raiz
    void *hb = dlopen("libt8b.so", RTLD_NOW);
    CHECK(hb != 0 && hb != ha);
    CHECK(t8_n == 2);
    CHECK(dlclose(ha) == 0);
    CHECK(t8_n == 2);                       // aun la retiene la referencia de hb
    CHECK(dlsym(ha, "a_calls_b") != 0);
    CHECK(dlclose(hb) == 0);
    // destructores en el orden del recorrido de la descarga: la raiz (A) y luego B
    static const int all_a[] = {2, 1, -1, -2};
    CHECK(log_is(all_a, 4));
    if (!log_is(all_a, 4)) log_print("ciclo desde A");
    CHECK(dlopen("libt8a.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    {
        const char *e = dlerror();
        CHECK(e && strstr(e, "dlopen failed: library \"libt8a.so\" wasn't loaded and RTLD_NOLOAD prevented it"));
    }
    CHECK(dlopen("libt8b.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    // desde el otro lado: A antes que B
    t8_n = 0;
    hb = dlopen("libt8b.so", RTLD_NOW);
    CHECK(hb != 0);
    static const int ctors_b[] = {1, 2};
    CHECK(log_is(ctors_b, 2));
    if (!log_is(ctors_b, 2)) log_print("ciclo desde B");
    CHECK(dlclose(hb) == 0);
    static const int all_b[] = {1, 2, -2, -1};
    CHECK(log_is(all_b, 4));
    // RTLD_GLOBAL: como bionic, el grupo ya no se descarga
    t8_n = 0;
    ha = dlopen("libt8a.so", RTLD_NOW | RTLD_GLOBAL);
    CHECK(ha != 0 && t8_n == 2);
    CHECK(dlclose(ha) == 0);
    CHECK(t8_n == 2);
    CHECK(dlopen("libt8a.so", RTLD_NOW | RTLD_NOLOAD) == ha);
}

static void tls_ie(void) {
    CHECK(dlopen("libt8ie.so", RTLD_NOW) == 0);
    const char *e = dlerror();
    CHECK(e != 0);
    if (!e) return;
    CHECK(strncmp(e, "dlopen failed: TLS symbol \"t8_tls_var\" in dlopened \"", 52) == 0);
    CHECK(strstr(e, "/libt8tls.so\" referenced from \"") != 0);
    CHECK(strstr(e, "/libt8ie.so\" using IE access model") != 0);
    if (!strstr(e, "using IE access model")) printf("dlerror: %s\n", e);
    // la carga fallida no deja nada cargado (tampoco la dependencia)
    CHECK(dlopen("libt8ie.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    CHECK(dlopen("libt8tls.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    // IE hacia el propio TLS: tambien
    CHECK(dlopen("libt8ieself.so", RTLD_NOW) == 0);
    e = dlerror();
    CHECK(e && strstr(e, "TLS symbol \"own_tls\" in dlopened \"") && strstr(e, "/libt8ieself.so\" using IE access model"));
    // la misma variable con TLSDESC (modelo dinamico) si carga
    void *h = dlopen("libt8tls.so", RTLD_NOW);
    CHECK(h != 0);
    int (*g)(void) = h ? SYM(h, int (*)(void), "tls_get") : 0;
    CHECK(g && g() == 9);
    if (h) dlclose(h);
}

static void (*g_reg)(int);
static volatile int thr_ready, thr_go;
static void *thr(void *arg) {
    (void)arg;
    g_reg(100);
    thr_ready = 1;
    while (!thr_go) sched_yield();
    return 0;
}

static void thread_atexit(void) {
    void *h = dlopen("libt8c.so", RTLD_NOW);
    CHECK(h != 0);
    if (!h) { printf("dlopen libt8c: %s\n", dlerror()); return; }
    g_reg = SYM(h, void (*)(int), "c_register");
    CHECK(g_reg != 0);
    if (!g_reg) return;
    t8_n = 0;
    unsigned long t;
    pthread_create(&t, 0, thr, 0);
    while (!thr_ready) sched_yield();
    CHECK(dlclose(h) == 0);
    CHECK(dlsym(h, "c_data") != 0);         // retenida por el destructor pendiente del hilo
    thr_go = 1;
    pthread_join(t, 0);
    CHECK(t8_n == 1 && t8_log[0] == 100);
    CHECK(dlsym(h, "c_data") == 0);         // al terminar el hilo se descargo
    CHECK(dlopen("libt8c.so", RTLD_NOW | RTLD_NOLOAD) == 0);
}

int run_all(void) {
    unload_for_real();
    new_address();
    cycle();
    tls_ie();
    thread_atexit();
    printf("t8 run_all: %d fallos\n", fails);
    return fails;
}
