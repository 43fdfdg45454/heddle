// Kernels AArch64 nativos (sin libc) equivalentes a los de arm_xlat_suite.rs.
// Cada uno ejecuta las instrucciones ARM reales que el traductor tiene que emular.
#ifndef ARM_KERNELS_H
#define ARM_KERNELS_H
#include <stdint.h>
#include <arm_neon.h>

static inline uint64_t xs_next(uint64_t *s) {
    uint64_t x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    return x;
}

// A. flags: 2 xorshift + subs + 2 csel + acumulador, por iteracion
static inline uint64_t k_flags(uint64_t iters, uint64_t seed) {
    uint64_t x = seed, x5 = 0;
    for (uint64_t i = 0; i < iters; i++) {
        uint64_t a = xs_next(&x), b = xs_next(&x), x2, x3, x4;
        __asm__ volatile(
            "subs %0, %3, %4\n\t"
            "csel %1, %3, %4, hs\n\t"
            "csel %2, %3, %4, gt\n\t"
            : "=&r"(x2), "=&r"(x3), "=&r"(x4)
            : "r"(a), "r"(b)
            : "cc");
        x5 = (x5 + x3) ^ x4;
    }
    return x5;
}

// C. SIMD: uqadd v.4s sobre n vectores
static inline void k_uqadd(uint32_t (*acc)[4], const uint32_t (*d)[4], uint64_t n) {
    for (uint64_t i = 0; i < n; i++) {
        uint32x4_t a = vld1q_u32(acc[i]);
        uint32x4_t b = vld1q_u32(d[i]);
        vst1q_u32(acc[i], vqaddq_u32(a, b));
    }
}

// D. stores: WORK xorshift entre stores, str normal
static inline uint64_t k_stores(volatile uint64_t *mem, uint64_t n, uint64_t rounds16, int work) {
    uint64_t x = 0x9E3779B97F4A7C15ull, rounds = 0;
    for (uint64_t r = 0; r < rounds16; r++) {
        for (int k = 0; k < 16; k++) {
            for (uint64_t i = 0; i < n; i++) {
                for (int w = 0; w < work; w++) xs_next(&x);
                mem[i] = x ^ rounds;
            }
        }
        rounds += 16;
    }
    return x;
}

// B. exclusivas: ldxr / add / stxr / cbnz
static inline void k_llsc_inc(volatile uint64_t *p) {
    uint64_t v;
    uint32_t s;
    __asm__ volatile(
        "1: ldxr %0, [%2]\n\t"
        "add %0, %0, #1\n\t"
        "stxr %w1, %0, [%2]\n\t"
        "cbnz %w1, 1b\n\t"
        : "=&r"(v), "=&r"(s)
        : "r"(p)
        : "memory");
}

// ARMv8.1 LSE (referencia: lo que haria un guest compilado para v8.1+)
static inline void k_lse_inc(volatile uint64_t *p) {
    __asm__ volatile(
        ".arch_extension lse\n\t"
        "stadd %1, [%0]\n\t"
        :
        : "r"(p), "r"((uint64_t)1)
        : "memory");
}

#endif
