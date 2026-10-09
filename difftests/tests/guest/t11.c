// t11: llamadas TLSDESC con la ruta rapida en linea del JIT (jit.rs, "TLSDESC en linea"). La sonda "fast" usa la
// secuencia del ABI (ldr x1, [x0]; add x0, x0, #0; blr x1), que el JIT reconoce; la "slow" mete un nop en medio: el
// JIT no la reconoce y salta al resolutor, que se traduce instruccion a instruccion (la referencia, el codigo ARM de
// bionic). Las dos deben dar lo mismo: resultado, registros preservados, NZCV, sp, x30, y en un fallo el mismo pc
// (dentro del resolutor) y los mismos registros. Rutas: rapida, generacion vieja, bloque sin reservar, punteros con
// etiqueta TBI (ignorada en ARM; en x86 no son canonicos) y descriptor con un argumento invalido (SIGSEGV).
typedef unsigned long size_t;
typedef unsigned long pthread_t;
extern int printf(const char *, ...);
extern int sigaction(int, const void *, void *);
extern int sigsetjmp(long *, int) __attribute__((returns_twice));
extern void siglongjmp(long *, int) __attribute__((noreturn));
extern int pthread_create(pthread_t *, const void *, void *(*)(void *), void *);
extern int pthread_join(pthread_t, void **);

typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa; // struct sigaction de bionic LP64
static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
#define N 32

__thread long tv = 5;
extern long *t11b_addr(void); // libt11b.so: otro modulo TLS

// out: [0] x0, [1] NZCV, [2] sp despues - sp antes, [3] x30 - retorno, [4 + n - 1] xn (n = 1..28)
extern void probe_fast(long *desc, long *out);
extern void probe_slow(long *desc, long *out);
__asm__(
    ".macro TLSPROBE name, gap\n"
    ".globl \\name\n"
    ".type \\name, %function\n"
    "\\name:\n"
    "  stp x29, x30, [sp, #-112]!\n"
    "  stp x19, x20, [sp, #16]\n"
    "  stp x21, x22, [sp, #32]\n"
    "  stp x23, x24, [sp, #48]\n"
    "  stp x25, x26, [sp, #64]\n"
    "  stp x27, x28, [sp, #80]\n"
    "  str x1, [sp, #96]\n"
    "  mov x29, sp\n"
    "  mov x2, #0x202\n  mov x3, #0x303\n  mov x4, #0x404\n  mov x5, #0x505\n  mov x6, #0x606\n  mov x7, #0x707\n"
    "  mov x8, #0x808\n  mov x9, #0x909\n  mov x10, #0xa0a\n  mov x11, #0xb0b\n  mov x12, #0xc0c\n  mov x13, #0xd0d\n"
    "  mov x14, #0xe0e\n  mov x15, #0xf0f\n  mov x16, #0x1010\n  mov x17, #0x1111\n  mov x19, #0x1313\n"
    "  mov x20, #0x1414\n  mov x21, #0x1515\n  mov x22, #0x1616\n  mov x23, #0x1717\n  mov x24, #0x1818\n"
    "  mov x25, #0x1919\n  mov x26, #0x1a1a\n  mov x27, #0x1b1b\n  mov x28, #0x1c1c\n"
    "  mov x1, #0x90000000\n  msr nzcv, x1\n"
    "  ldr x1, [x0]\n"
    "  .if \\gap\n  nop\n  .endif\n"
    "  add x0, x0, #0\n"
    "  blr x1\n"
    "\\name\\()_ret:\n"
    "  str x0, [x29, #104]\n"
    "  ldr x0, [x29, #96]\n"
    "  stp x1, x2, [x0, #32]\n  stp x3, x4, [x0, #48]\n  stp x5, x6, [x0, #64]\n  stp x7, x8, [x0, #80]\n"
    "  stp x9, x10, [x0, #96]\n  stp x11, x12, [x0, #112]\n  stp x13, x14, [x0, #128]\n  stp x15, x16, [x0, #144]\n"
    "  stp x17, x18, [x0, #160]\n  stp x19, x20, [x0, #176]\n  stp x21, x22, [x0, #192]\n  stp x23, x24, [x0, #208]\n"
    "  stp x25, x26, [x0, #224]\n  stp x27, x28, [x0, #240]\n"
    "  mrs x1, nzcv\n  str x1, [x0, #8]\n"
    "  mov x1, sp\n  sub x1, x1, x29\n  str x1, [x0, #16]\n"
    "  adr x1, \\name\\()_ret\n  sub x1, x30, x1\n  str x1, [x0, #24]\n"
    "  ldr x1, [x29, #104]\n  str x1, [x0]\n"
    "  ldp x19, x20, [sp, #16]\n  ldp x21, x22, [sp, #32]\n  ldp x23, x24, [sp, #48]\n"
    "  ldp x25, x26, [sp, #64]\n  ldp x27, x28, [sp, #80]\n"
    "  ldp x29, x30, [sp], #112\n"
    "  ret\n"
    ".endm\n"
    "TLSPROBE probe_fast, 0\n"
    "TLSPROBE probe_slow, 1\n");

static long *desc_tv(void) {
    long *d;
    __asm__("adrp %0, :tlsdesc:tv\n\tadd %0, %0, :tlsdesc_lo12:tv" : "=r"(d));
    return d;
}
static unsigned long tp(void) {
    unsigned long t;
    __asm__ volatile("mrs %0, tpidr_el0" : "=r"(t));
    return t;
}
#define TAG(p) ((long *)((unsigned long)(p) | (0x2bul << 56)))

// Llamada de una sonda con el resultado esperado (direccion de tv en este hilo) y comprobaciones propias.
static void one(int fast, long *desc, long *out, const char *que) {
    if (fast) probe_fast(desc, out);
    else probe_slow(desc, out);
    if ((unsigned long)out[0] + tp() != (unsigned long)&tv) { printf("FALLO %s %s: direccion\n", que, fast ? "fast" : "slow"); fails++; }
    CHECK(out[2] == 0 && out[3] == 0);
    for (int n = 2; n <= 28; n++)
        if (n != 18 && out[4 + n - 1] != n * 0x101) { printf("FALLO %s: x%d = %#lx\n", que, n, out[4 + n - 1]); fails++; }
}

// Secuencia por hilo: 0 primer acceso (generacion vieja), 1 bloque sin reservar (antes, otro modulo), 2 rapida,
// 3 descriptor con etiqueta, 4 argumento con etiqueta.
struct run { int fast; long r[5][N]; };
static void *seq(void *p) {
    struct run *r = p;
    long *d = desc_tv();
    one(r->fast, d, r->r[0], "generacion vieja");
    return 0;
}
static void *seq_null(void *p) {
    struct run *r = p;
    long *d = desc_tv();
    long *o = t11b_addr(); // otro modulo: la generacion del DTV queda al dia, el bloque de tv sin reservar
    CHECK(*o == 77);
    one(r->fast, d, r->r[1], "bloque sin reservar");
    one(r->fast, d, r->r[2], "rapida");
    one(r->fast, TAG(d), r->r[3], "descriptor con etiqueta");
    long fake[2] = {d[0], (long)TAG(d[1])};
    one(r->fast, fake, r->r[4], "argumento con etiqueta");
    return 0;
}
static void compara(struct run *a, struct run *b, int k, const char *que) {
    for (int i = 1; i < N; i++)
        if (a->r[k][i] != b->r[k][i]) { printf("FALLO %s: [%d] %#lx frente a %#lx\n", que, i, a->r[k][i], b->r[k][i]); fails++; }
}

static long salto[33];
static volatile unsigned long f_addr, f_pc, f_sp, f_x[23];
static void h_segv(int s, void *si, void *uc) {
    f_addr = *(unsigned long *)((char *)si + 16);
    f_pc = *(unsigned long *)((char *)uc + 440);
    f_sp = *(unsigned long *)((char *)uc + 432);
    for (int i = 0; i < 23; i++) f_x[i] = *(unsigned long *)((char *)uc + 184 + 8 * i);
    siglongjmp(salto, 1);
}
// SIGSEGV dentro del resolutor (argumento invalido): pc, direccion y registros como en el resolutor de ARM
static void fallo(int fast, unsigned long *pc, unsigned long *addr, unsigned long *x, long *sp_rel) {
    long *d = desc_tv();
    long fake[2] = {d[0], 0x10};
    long out[N];
    gsa sa = {4 /*SA_SIGINFO*/, (void *)h_segv, 0, 0}, old;
    sigaction(11, &sa, &old);
    unsigned long sp0;
    __asm__ volatile("mov %0, sp" : "=r"(sp0));
    f_pc = 0;
    if (sigsetjmp(salto, 1) == 0) {
        if (fast) probe_fast(fake, out);
        else probe_slow(fake, out);
        printf("FALLO: la sonda %s con argumento invalido no fallo\n", fast ? "fast" : "slow");
        fails++;
    }
    sigaction(11, &old, 0);
    *pc = f_pc - (unsigned long)d[0];
    *addr = f_addr;
    for (int i = 0; i < 23; i++) x[i] = f_x[i];
    *sp_rel = (long)(f_sp - sp0);
}

int run_all(void) {
    struct run a = {1}, b = {0};
    pthread_t t;
    pthread_create(&t, 0, seq, &a); pthread_join(t, 0);
    pthread_create(&t, 0, seq, &b); pthread_join(t, 0);
    pthread_create(&t, 0, seq_null, &a); pthread_join(t, 0);
    pthread_create(&t, 0, seq_null, &b); pthread_join(t, 0);
    const char *que[5] = {"generacion vieja", "bloque sin reservar", "rapida", "descriptor con etiqueta", "argumento con etiqueta"};
    for (int k = 0; k < 5; k++) compara(&a, &b, k, que[k]);
    // NZCV de la ruta rapida: cmp de generaciones iguales o mayor (C = 1)
    CHECK(a.r[2][1] & (1ul << 29));
    unsigned long pa, pb, aa, ab, xa[23], xb[23];
    long sa, sb;
    fallo(1, &pa, &aa, xa, &sa);
    fallo(0, &pb, &ab, xb, &sb);
    CHECK(pa == 24 && pb == 24); // ldr x22, [x0]: el acceso al argumento
    CHECK(aa == 0x10 && ab == 0x10);
    CHECK(sa == sb);
    CHECK(xa[0] == 0x10 && xa[19] == tp());
    for (int i = 0; i < 23; i++)
        if (xa[i] != xb[i]) { printf("FALLO fallo: x%d %#lx frente a %#lx\n", i, xa[i], xb[i]); fails++; }
    printf("t11 run_all: %d fallos\n", fails);
    return fails;
}
