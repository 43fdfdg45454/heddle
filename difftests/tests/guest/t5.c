// t5: el cargador como bionic. dlclose hasta 0 referencias descarga la biblioteca (destructores en orden inverso) y
// un dlopen posterior la carga de nuevo: constructores otra vez y datos/bss con su imagen inicial. Y variables TLS
// importadas de otra biblioteca (TLSDESC, dependencia transitiva; initial-exec hacia una biblioteca de dlopen lo
// rechaza bionic, ver t8).
typedef unsigned long size_t;
extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern int dlclose(void *);
extern char *dlerror(void);
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
#define RTLD_NOW 2
#define RTLD_NOLOAD 4
#define RTLD_NODELETE 0x1000

static int fini_log[8], nfini;
static void fini_cb(int k) { if (nfini < 8) fini_log[nfini++] = k; }

#define SYM(h, T, n) ((T)dlsym(h, n))

static void ctor_reload(void) {
    void *h = dlopen("libt5ctor.so", RTLD_NOW);
    CHECK(h != 0);
    if (!h) { printf("dlopen: %s\n", dlerror()); return; }
    int *runs = SYM(h, int *, "ctor_runs"), *data = SYM(h, int *, "g_data"), *norder = SYM(h, int *, "norder");
    int *order = SYM(h, int *, "order"), *dep_at = SYM(h, int *, "dep_at_ctor");
    long *bss = SYM(h, long *, "g_bss");
    void (**onf)(int) = SYM(h, void (**)(int), "on_fini");
    int (*val)(void) = SYM(h, int (*)(void), "ctor_value");
    CHECK(runs && data && norder && order && bss && onf && val && dep_at);
    if (!(runs && data && norder && order && bss && onf && val && dep_at)) return;
    CHECK(*runs == 1 && *data == 8 && val() == 8);
    CHECK(*norder == 2 && order[0] == 1 && order[1] == 2);
    CHECK(*dep_at == 101);   // la dependencia se inicializo antes
    // segunda referencia: dlclose de una no descarga
    void *h2 = dlopen("libt5ctor.so", RTLD_NOW);
    CHECK(h2 == h);
    CHECK(*runs == 1);       // ya cargada: no se repiten constructores
    *data = 99; bss[63] = 12345; *onf = fini_cb; *runs = 50; *norder = 3; order[0] = 9;
    CHECK(dlclose(h2) == 0);
    CHECK(nfini == 0);
    // ultima referencia: destructores en orden inverso (dtor_b, luego dtor_a)
    CHECK(dlclose(h) == 0);
    CHECK(nfini == 2 && fini_log[0] == 2 && fini_log[1] == 1);
    // handle viejo: ya no es valido
    CHECK(dlsym(h, "ctor_runs") == 0);
    CHECK(dlclose(h) == -1);
    CHECK(dlopen("libt5ctor.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    CHECK(dlopen("libt5dep.so", RTLD_NOW | RTLD_NOLOAD) == 0);   // la dependencia tambien
    // recarga: constructores de nuevo y datos con su imagen inicial (no los valores viejos)
    h = dlopen("libt5ctor.so", RTLD_NOW);
    CHECK(h != 0);
    if (!h) return;
    runs = SYM(h, int *, "ctor_runs"); data = SYM(h, int *, "g_data"); norder = SYM(h, int *, "norder");
    order = SYM(h, int *, "order"); bss = SYM(h, long *, "g_bss"); onf = SYM(h, void (**)(int), "on_fini");
    val = SYM(h, int (*)(void), "ctor_value"); dep_at = SYM(h, int *, "dep_at_ctor");
    CHECK(runs && data && norder && bss && onf && val && dep_at);
    if (!(runs && data && norder && bss && onf && val && dep_at)) return;
    CHECK(*runs == 1);
    CHECK(*data == 8 && val() == 8);
    CHECK(*norder == 2 && order[0] == 1 && order[1] == 2);
    CHECK(bss[63] == 0 && *onf == 0);
    CHECK(*dep_at == 101);   // la dependencia tambien se descargo y se volvio a inicializar
    CHECK(nfini == 2);
    CHECK(dlopen("libt5ctor.so", RTLD_NOW | RTLD_NOLOAD) == h);
    dlclose(h);
    // RTLD_NODELETE en una biblioteca ya cargada se ignora (bionic, dlopen_nodelete_on_second_dlopen)
    *onf = fini_cb;
    CHECK(dlopen("libt5ctor.so", RTLD_NOW | RTLD_NODELETE) == h);
    dlclose(h);
    dlclose(h);
    CHECK(nfini == 4);
    CHECK(dlopen("libt5ctor.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    // en una carga nueva si vale: el ultimo dlclose no la descarga
    h = dlopen("libt5ctor.so", RTLD_NOW | RTLD_NODELETE);
    CHECK(h != 0);
    if (!h) return;
    data = SYM(h, int *, "g_data"); onf = SYM(h, void (**)(int), "on_fini"); runs = SYM(h, int *, "ctor_runs");
    *data = 55; *onf = fini_cb;
    dlclose(h);
    CHECK(nfini == 4);
    h = dlopen("libt5ctor.so", RTLD_NOW);
    CHECK(h != 0 && *data == 55 && *runs == 1);
    dlclose(h);
}

static int (*g_ie_get)(void), (*g_gd_get)(void);
static void *tls_thr(void *arg) {
    (void)arg;
    return (void *)(long)(g_ie_get() * 10000 + g_gd_get());
}

static void tls_import(void) {
    void *h = dlopen("libt5use.so", RTLD_NOW);
    CHECK(h != 0);
    if (!h) { printf("dlopen libt5use: %s\n", dlerror()); return; }
    void *ht = dlopen("libt5tls.so", RTLD_NOW);
    CHECK(ht != 0);
    if (!ht) return;
    int *(*ie)(void) = SYM(h, int *(*)(void), "use_ie_addr"), *(*gd)(void) = SYM(h, int *(*)(void), "use_gd_addr");
    int *(*die)(void) = SYM(ht, int *(*)(void), "tls_ie_addr"), *(*dgd)(void) = SYM(ht, int *(*)(void), "tls_gd_addr");
    g_ie_get = SYM(h, int (*)(void), "use_ie_get"); g_gd_get = SYM(h, int (*)(void), "use_gd_get");
    void (*set)(int, int) = SYM(h, void (*)(int, int), "use_set");
    char (*b0)(void) = SYM(h, char (*)(void), "use_buf0");
    int (*mid)(void) = SYM(h, int (*)(void), "use_mid");
    CHECK(ie && gd && die && dgd && g_ie_get && g_gd_get && set && b0 && mid);
    if (!(ie && gd && die && dgd && g_ie_get && g_gd_get && set && b0 && mid)) return;
    CHECK(mid() == 5);
    int (*weak)(void) = SYM(h, int (*)(void), "use_weak_null");
    CHECK(weak && weak() == 3);
    // la misma variable vista desde la biblioteca que la define y desde la que la importa
    CHECK(ie() == die());
    CHECK(gd() == dgd());
    CHECK(g_ie_get() == 42 && g_gd_get() == 1000 && b0() == 't');
    set(7, 8);
    CHECK(*die() == 7 && *dgd() == 8);
    // otro hilo parte de la imagen inicial
    unsigned long t;
    void *rv = 0;
    pthread_create(&t, 0, tls_thr, 0);
    pthread_join(t, &rv);
    CHECK((long)rv == 42 * 10000 + 1000);
    CHECK(g_ie_get() == 7 && g_gd_get() == 8);
    dlclose(ht);
    dlclose(h);
}

int run_all(void) {
    ctor_reload();
    tls_import();
    printf("t5 run_all: %d fallos\n", fails);
    return fails;
}
