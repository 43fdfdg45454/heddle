// Estado de salida de heddle-run y diagnostico de instrucciones no soportadas (lo comprueba scripts/run-all.sh):
//  ret42 -> 42 (x0 & 0xff), exit7 -> 7 (exit del guest), sm4e/udf/smstart/mrs_rndr -> SIGILL con el mnemonico en el
//  mensaje, flagm -> 10 * ID_AA64ISAR0_EL1.TS (leido con MRS: lo anunciado por el modelo) + 1 (CFINV invierte C) si el
//  modelo anuncia FlagM; si no (A78, A53), SIGILL "ausente en el modelo": se ejecuta lo anunciado e implementado.
//  retaa -> SIGILL en todos los modelos (sin PAuth), cpu_files -> 0 (/proc/cpuinfo, /proc/self/auxv y midr_el1
//  coherentes con MRS y getauxval, por open, fopen y svc openat), chain_segv/chain_abort -> 139/134 (manejador que
//  encadena al de debuggerd), ssbs_signal -> 0 con max (PSTATE.SSBS en el marco de una senal) y SIGILL sin SSBS2
extern void exit(int);
int ret42(void) { return 42; }
int ret300(void) { return 300; } // 300 & 0xff = 44
void exit7(void) { exit(7); }
int sm4e(void) {
    __asm__ volatile(".inst 0xcec08400"); // SM4E v0.4s, v0.4s (FEAT_SM4)
    return 0;
}
int udf(void) {
    __asm__ volatile(".inst 0x00000000"); // UDF #0
    return 0;
}
int smstart(void) {
    __asm__ volatile(".inst 0xd503477f"); // SMSTART (FEAT_SME, no anunciada)
    return 0;
}
int mrs_rndr(void) {
    unsigned long v;
    __asm__ volatile(".inst 0xd53b2400\n\tmov %0, x0" : "=r"(v) : : "x0"); // MRS x0, RNDR (FEAT_RNG, no anunciada)
    return (int)v;
}
int flagm(void) {
    unsigned long isar0, cc;
    __asm__ volatile("mrs %0, ID_AA64ISAR0_EL1" : "=r"(isar0));
    // cmp 0, 0 deja C=1; CFINV lo pone a 0; cset cc -> 1
    __asm__ volatile("cmp xzr, xzr\n\t.inst 0xd500401f\n\tcset %0, cc" : "=r"(cc) : : "cc");
    return (int)(((isar0 >> 52) & 0xf) * 10 + cc);
}

int retaa(void) {
    __asm__ volatile(".inst 0xd65f0bff"); // RETAA (FEAT_PAuth, ningun modelo la anuncia)
    return 0;
}

// --- archivos de la CPU ------------------------------------------------------------------------------------------
typedef struct FILE FILE;
extern int open(const char*, int, ...);
extern long read(int, void*, unsigned long);
extern int close(int);
extern FILE* fopen(const char*, const char*);
extern unsigned long fread(void*, unsigned long, unsigned long, FILE*);
extern int fclose(FILE*);
extern unsigned long getauxval(unsigned long);

static char buf[65536];

static long rd(int fd) {
    long n = 0, r;
    if (fd < 0) return -1;
    while (n < (long)sizeof(buf) - 1 && (r = read(fd, buf + n, sizeof(buf) - 1 - n)) > 0) n += r;
    close(fd);
    buf[n] = 0;
    return n;
}
static long svc_openat(const char* p, long flags) {
    register long x0 __asm__("x0") = -100;
    register long x1 __asm__("x1") = (long)p;
    register long x2 __asm__("x2") = flags;
    register long x3 __asm__("x3") = 0;
    register long x8 __asm__("x8") = 56;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x1), "r"(x2), "r"(x3), "r"(x8) : "memory");
    return x0;
}
static int has(const char* h, const char* n) {
    for (; *h; h++) {
        const char *a = h, *b = n;
        while (*b && *a == *b) a++, b++;
        if (!*b) return 1;
    }
    return 0;
}
static void hex(char* o, unsigned long v, int digits) {
    for (int i = digits - 1; i >= 0; i--, v >>= 4) o[i] = "0123456789abcdef"[v & 15];
    o[digits] = 0;
}
static int same(const char* a, const char* b) {
    while (*a && *a == *b) a++, b++;
    return *a == *b;
}
static char copy[65536];

int cpu_files(void) {
    unsigned long midr;
    __asm__ volatile("mrs %0, MIDR_EL1" : "=r"(midr));
    // /proc/cpuinfo por open
    if (rd(open("/proc/cpuinfo", 0)) <= 0) return 1;
    if (!has(buf, "processor\t: ") || !has(buf, "BogoMIPS\t: ") || !has(buf, "Features\t: fp asimd evtstrm")) return 2;
    char part[32] = "CPU part\t: 0x";
    hex(part + 13, (midr >> 4) & 0xfff, 3);
    if (!has(buf, part) || !has(buf, "CPU architecture: 8\n")) return 3;
    if (has(buf, "x86") || has(buf, "GenuineIntel") || has(buf, "AuthenticAMD") || has(buf, "flags\t")) return 4;
    // la misma por svc openat y por fopen
    for (int i = 0; buf[i] || (copy[i] = 0); i++) copy[i] = buf[i];
    if (rd((int)svc_openat("/proc/cpuinfo", 0)) <= 0 || !same(buf, copy)) return 5;
    FILE* f = fopen("/proc/cpuinfo", "re");
    if (!f) return 6;
    unsigned long n = fread(buf, 1, sizeof(buf) - 1, f);
    buf[n] = 0;
    fclose(f);
    if (!same(buf, copy)) return 7;
    // solo lectura
    if (open("/proc/cpuinfo", 1) >= 0 || svc_openat("/proc/cpuinfo", 2) != -13 || fopen("/proc/cpuinfo", "w")) return 8;
    // /proc/self/auxv: AT_HWCAP/AT_HWCAP2 iguales a getauxval, sin AT_SYSINFO_EHDR
    long m = rd(open("/proc/self/auxv", 0));
    if (m <= 0 || m % 16) return 9;
    unsigned long* e = (unsigned long*)buf;
    int seen = 0;
    for (long i = 0; i < m / 8; i += 2) {
        if (e[i] == 16 && e[i + 1] == getauxval(16)) seen |= 1;
        if (e[i] == 26 && e[i + 1] == getauxval(26)) seen |= 2;
        if (e[i] == 33) return 10;
    }
    if (seen != 3) return 11;
    // midr_el1 de sysfs = MRS MIDR_EL1
    char want[24] = "0x";
    hex(want + 2, midr, 16);
    want[18] = '\n';
    want[19] = 0;
    if (rd(open("/sys/devices/system/cpu/cpu0/regs/identification/midr_el1", 0)) != 19 || !same(buf, want)) return 12;
    return 0;
}

// Manejador de fallos que encadena al anterior, como Unity, Crashlytics o Breakpad: el anterior debe ser el
// debuggerd_signal_handler de bionic (SA_SIGINFO, no SIG_DFL). Al encadenarlo, el proceso muere por la senal (139 o
// 134); si el manejador se vuelve a ejecutar mas de 3 veces (el fallo se repite sin terminar), sale con 9.
extern int sigaction(int, const void *, void *);
extern void abort(void);
typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa7; // struct sigaction de bionic LP64
static gsa7 prev_segv, prev_abrt;
static volatile int veces;
static void encadena(int sig, void *info, void *uc) {
    gsa7 *p = sig == 11 ? &prev_segv : &prev_abrt;
    if (++veces > 3) exit(9);
    if ((unsigned long)p->h > 1) ((void (*)(int, void *, void *))p->h)(sig, info, uc);
}
static int instala(int sig, gsa7 *prev) {
    gsa7 sa = {0};
    sa.flags = 4 | 0x08000000; // SA_SIGINFO | SA_ONSTACK
    sa.h = (void *)encadena;
    sigaction(sig, &sa, prev);
    return (prev->flags & 4) && (unsigned long)prev->h > 1;
}
int chain_segv(void) {
    if (!instala(11, &prev_segv)) return 3;
    return *(volatile int *)0;
}
int chain_abort(void) {
    if (!instala(6, &prev_abrt)) return 3;
    abort();
    return 4;
}

// FEAT_SSBS2 (modelo max): el marco de una senal lleva PSTATE.SSBS en pstate (bit 12) y al volver se repone el del
// ucontext. 0 = bien.
static volatile unsigned long ssbs_visto;
static void ssbs_h(int sig, void *info, void *uc) {
    unsigned long *pc = (unsigned long *)((char *)uc + 176 + 264), *pstate = pc + 1;
    ssbs_visto = *pstate & (1ul << 12);
    *pstate |= 1ul << 12;
    *pc += 4; // salta el UDF
}
int ssbs_signal(void) {
    unsigned long v;
    __asm__ volatile("mrs %0, s3_3_c4_c2_6" : "=r"(v));
    if (v != 1ul << 12) return 1; // Linux arranca con SSBS = 1
    __asm__ volatile(".inst 0xd503403f"); // msr ssbs, #0
    __asm__ volatile("mrs %0, s3_3_c4_c2_6" : "=r"(v));
    if (v != 0) return 2;
    gsa7 sa = {0};
    sa.flags = 4;
    sa.h = (void *)ssbs_h;
    sigaction(4, &sa, 0);
    __asm__ volatile(".inst 0x00000000"); // UDF #0: fallo sincrono, ucontext escribible
    if (ssbs_visto != 0) return 3;
    __asm__ volatile("mrs %0, s3_3_c4_c2_6" : "=r"(v));
    return v == 1ul << 12 ? 0 : 4;
}
