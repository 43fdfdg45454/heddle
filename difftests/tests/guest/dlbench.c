// Microbanco del cargador (no forma parte de run-all.sh): dlopen + dlclose repetidos de
//  * libt10p_root.so: siete bibliotecas nuevas por carga (lectura, reserva, mapeo, enlazado y descarga)
//  * libdlbench2.so: una biblioteca con ~150 importaciones de libc/libm (busqueda de simbolos al relocar)
// y dlsym con RTLD_DEFAULT y con el handle. Uso: heddle-run build/libdlbench.so run_bench (HEDDLE_LIBPATH=build)
typedef unsigned long size_t;
struct timespec { long tv_sec, tv_nsec; };
extern int clock_gettime(int, struct timespec *);
extern int printf(const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern int dlclose(void *);
extern char *dlerror(void);

static long now(void) { struct timespec t; clock_gettime(1, &t); return t.tv_sec * 1000000000L + t.tv_nsec; }

static long cycle(const char *lib, int n) {
    long best = 1L << 62;
    for (int r = 0; r < 3; r++) {
        long t0 = now();
        for (int i = 0; i < n; i++) {
            void *h = dlopen(lib, 2);
            if (!h) { printf("dlopen %s: %s\n", lib, dlerror()); return -1; }
            dlclose(h);
        }
        long d = (now() - t0) / n;
        if (d < best) best = d;
    }
    return best;
}

int run_bench(void) {
    printf("dl: dlopen+dlclose libt10p_root.so (7 bibliotecas) %ld us\n", cycle("libt10p_root.so", 100) / 1000);
    printf("dl: dlopen+dlclose libdlbench2.so (150 importaciones) %ld us\n", cycle("libdlbench2.so", 200) / 1000);
    void *h = dlopen("libdlbench2.so", 2);
    const char *names[] = {"dlbench2", "malloc", "sqrt", "no_existe_xyz"};
    for (int k = 0; k < 4; k++) {
        long best = 1L << 62;
        for (int r = 0; r < 3; r++) {
            long t0 = now();
            for (int i = 0; i < 20000; i++) dlsym(h, names[k]);
            long d = now() - t0;
            if (d < best) best = d;
        }
        long best0 = 1L << 62;
        for (int r = 0; r < 3; r++) {
            long t0 = now();
            for (int i = 0; i < 20000; i++) dlsym((void *)0, names[k]);
            long d = now() - t0;
            if (d < best0) best0 = d;
        }
        printf("dl: dlsym(handle, %s) %ld ns, dlsym(RTLD_DEFAULT) %ld ns\n", names[k], best / 20000, best0 / 20000);
    }
    dlclose(h);
    return 0;
}
