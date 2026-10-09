// Version NATIVA AArch64 de las pruebas de arm_xlat_suite.rs.
// Mide, en un ARM64 real, cuanto cuesta el codigo ORIGINAL; la salida alimenta las columnas
// "ARM ns" / "x/ARM" de la suite.
//
//   Compilar (Termux / Linux ARM64 / Android NDK):  clang -O2 -pthread arm_native.c -o arm_native
//                                          o:       gcc   -O2 -pthread arm_native.c -o arm_native
//   Ejecutar:  ./arm_native > arm_results.txt      (~15 s)
//   Luego, en el x86:  ./arm_xlat_suite --arm arm_results.txt
//
// Cada linea de salida: "<clave> <ns por operacion>". Mismas definiciones de ns/op que la suite.
// Conviene fijar el reloj (modo avion, cargador, sin otras apps); en big.LITTLE los resultados
// dependen del nucleo donde caiga el hilo: use taskset si puede (p. ej. un nucleo grande).
#define _GNU_SOURCE
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "arm_kernels.h"

#ifndef __aarch64__
#error "Esto es para AArch64"
#endif

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec * 1e-9;
}

#define DUR 0.5

static void emit(const char *key, double ns) { printf("%s %.4f\n", key, ns); fflush(stdout); }

static volatile uint64_t sink;

static void bench_flags(void) {
    uint64_t iters = 0, acc = 0;
    double t0 = now();
    do {
        acc ^= k_flags(1024, 0x9E3779B97F4A7C15ull + iters);
        iters += 1024;
    } while (now() - t0 < DUR);
    sink = acc;
    emit("flags.block", (now() - t0) * 1e9 / iters);
}

#define NVEC 1024
static uint32_t acc_v[NVEC][4], dat_v[NVEC][4];

static void bench_simd(void) {
    uint64_t x = 0xDEADBEEFCAFEF00Dull;
    for (int i = 0; i < NVEC; i++)
        for (int l = 0; l < 4; l++) {
            acc_v[i][l] = (uint32_t)xs_next(&x) >> (xs_next(&x) % 32);
            dat_v[i][l] = (uint32_t)xs_next(&x) >> (xs_next(&x) % 32);
        }
    uint64_t passes = 0;
    double t0 = now();
    do {
        for (int k = 0; k < 16; k++) k_uqadd(acc_v, (const uint32_t(*)[4])dat_v, NVEC);
        passes += 16;
    } while (now() - t0 < DUR);
    sink = acc_v[0][0];
    emit("simd.uqadd", (now() - t0) * 1e9 / (double)(passes * NVEC));
}

static uint64_t mem_st[4096] __attribute__((aligned(64)));

static void bench_stores(void) {
    for (int w = 0; w <= 1; w++) {
        uint64_t rounds = 0, r = 0;
        double t0 = now();
        do {
            r ^= k_stores(mem_st, 4096, 1, w);
            rounds += 16;
        } while (now() - t0 < DUR);
        sink = r;
        char key[32];
        snprintf(key, sizeof key, "store.work%d", w);
        emit(key, (now() - t0) * 1e9 / (double)(rounds * 4096));
    }
}

static volatile uint64_t cell __attribute__((aligned(64)));
static atomic_int stop_flag;
static atomic_ullong total_ops;
static int use_lse;

static void *worker(void *arg) {
    (void)arg;
    uint64_t n = 0;
    for (;;) {
        for (int i = 0; i < 4096; i++) {
            if (use_lse) k_lse_inc(&cell); else k_llsc_inc(&cell);
        }
        n += 4096;
        if (atomic_load_explicit(&stop_flag, memory_order_relaxed)) break;
    }
    atomic_fetch_add(&total_ops, n);
    return NULL;
}

static void bench_excl(int threads, int lse) {
    pthread_t th[8];
    cell = 0;
    use_lse = lse;
    atomic_store(&stop_flag, 0);
    atomic_store(&total_ops, 0);
    double t0 = now();
    for (int i = 0; i < threads; i++) pthread_create(&th[i], NULL, worker, NULL);
    struct timespec ts = {0, 600 * 1000 * 1000};
    nanosleep(&ts, NULL);
    atomic_store(&stop_flag, 1);
    for (int i = 0; i < threads; i++) pthread_join(th[i], NULL);
    double secs = now() - t0;
    char key[32];
    snprintf(key, sizeof key, "excl.%dt.%s", threads, lse ? "lse" : "llsc");
    emit(key, secs * 1e9 / (double)total_ops);
    if (cell != total_ops) fprintf(stderr, "AVISO: contador %llu != %llu\n", (unsigned long long)cell, (unsigned long long)total_ops);
}

// Rotacion sobre ws granulos de 64 B (equivale a B' de la suite). En ARM no hay armado ni enfriamiento.
struct pad { uint64_t v; char p[56]; };

static void bench_rot(int ws, const char *key) {
    struct pad *c = aligned_alloc(64, sizeof(struct pad) * ws);
    memset(c, 0, sizeof(struct pad) * ws);
    uint64_t n = 0;
    double t0 = now();
    do {
        for (int i = 0; i < ws; i++) k_llsc_inc(&c[i].v);
        n += ws;
    } while (now() - t0 < DUR);
    emit(key, (now() - t0) * 1e9 / (double)n);
    free(c);
}

int main(void) {
    fprintf(stderr, "arm_native: midiendo (~15 s)...\n");
    bench_flags();
    bench_simd();
    bench_stores();
    bench_excl(1, 0);
    bench_excl(2, 0);
    bench_excl(1, 1);
    bench_excl(2, 1);
    bench_rot(64, "excl.rot64");
    bench_rot(8192, "excl.rot8192");
    bench_rot(200000, "excl.rot200k");
    return 0;
}
