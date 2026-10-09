// Microbanco de acceso a TLS de una biblioteca abierta con dlopen (TLSDESC, como compila clang -fPIC en arm64):
//  * acceso: ns por acceso en la ruta rapida (bucle con la direccion recalculada en cada vuelta)
//  * primer acceso de un hilo nuevo (ruta lenta: en bionic reserva el bloque del modulo para ese hilo): la primera
//    llamada a addr (traduccion + ruta lenta) menos la primera a addr2 (traduccion + ruta rapida, mismo codigo). Antes,
//    un acceso al TLS de libtlsbench2.so deja traducido en el hilo el codigo del resolutor (rapido y lento).
// Uso: heddle-run build/libtlsbench.so run_bench (los resultados de referencia, en difftests/README.md)
typedef unsigned long size_t;
struct timespec { long tv_sec, tv_nsec; };
extern int clock_gettime(int, struct timespec *);
extern int printf(const char *, ...);
typedef unsigned long pthread_t;
extern int pthread_create(pthread_t *, const void *, void *(*)(void *), void *);
extern int pthread_join(pthread_t, void **);

long *addr_o(void);   // libtlsbench2.so
__thread long tls_v = 1;
__thread long tls_w = 2;
__thread char tls_big[256];

static long now(void) { struct timespec t; clock_gettime(1, &t); return t.tv_sec * 1000000000L + t.tv_nsec; }

// el asm volatile impide que el compilador saque la llamada del bucle (sin el, clang la considera pura y tls_loop
// medía solo el bucle, sin ningun acceso TLS)
__attribute__((noinline)) long *addr(void) { __asm__ volatile(""); return &tls_v; }
// misma forma que addr (misma traduccion), otra variable del mismo modulo
__attribute__((noinline)) long *addr2(void) { __asm__ volatile(""); return &tls_w; }

// referencia sin TLS: misma forma (llamada por la PLT, prologo, retorno), devuelve una variable global
long glob_v = 1;
__attribute__((noinline)) long *addr_g(void) { __asm__ volatile(""); return &glob_v; }

long glob_loop(long n) {
    long s = 0;
    for (long i = 0; i < n; i++) {
        long *p = addr_g();
        __asm__ volatile("" : "+r"(p));
        s += ++*p;
    }
    return s;
}

long tls_loop(long n) {
    long s = 0;
    for (long i = 0; i < n; i++) {
        long *p = addr();
        __asm__ volatile("" : "+r"(p));
        s += ++*p;
    }
    return s;
}

static long first_ns[64];
static void *first(void *arg) {
    long i = (long)arg;
    long *o = addr_o();          // calienta el resolutor en este hilo: ruta lenta (reserva el bloque de libtlsbench2)
    o = addr_o();                // ... y rapida
    long t0 = now();
    long *p = addr();            // primer acceso del hilo al modulo
    long t1 = now();
    long *q = addr2();           // primero a otra variable del mismo modulo (bloque ya reservado)
    long t2 = now();
    first_ns[i] = (t1 - t0) - (t2 - t1);
    return (void *)(long)(*p + *q + *o + tls_big[0]);
}

static double best_ns(long (*f)(long)) {
    f(1000000);   // calentamiento (traduccion)
    long best = 1L << 62;
    for (int r = 0; r < 5; r++) {
        long n = 20000000, t0 = now();
        f(n);
        long d = now() - t0;
        if (d < best) best = d;
    }
    return (double)best / 20000000.0;
}

int run_bench(void) {
    double t = best_ns(tls_loop), g = best_ns(glob_loop);
    printf("tls: acceso (ruta rapida) %.2f ns por vuelta; sin TLS (misma llamada a una global) %.2f ns; TLS %.2f ns\n", t, g, t - g);
    long sum = 0;
    for (int i = 0; i < 64; i++) {
        pthread_t th;
        pthread_create(&th, 0, first, (void *)(long)i);
        pthread_join(th, 0);
        sum += first_ns[i];
    }
    // mediana (la traduccion en frio mete ruido)
    for (int i = 0; i < 64; i++)
        for (int j = i + 1; j < 64; j++)
            if (first_ns[j] < first_ns[i]) { long t = first_ns[i]; first_ns[i] = first_ns[j]; first_ns[j] = t; }
    printf("tls: primer acceso de un hilo (ruta lenta sobre la rapida) %ld ns de mediana (media %.0f)\n", first_ns[32], (double)sum / 64.0);
    return 0;
}
