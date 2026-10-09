typedef unsigned long size_t;
typedef int jint; typedef long jlong; typedef double jdouble; typedef float jfloat; typedef unsigned char jboolean;
typedef void *jobject; typedef void *jclass; typedef void *jstring; typedef void *jmethodID;
typedef void *JNIEnv;   // puntero a puntero a tabla
typedef void *JavaVM;
typedef union { jboolean z; jint i; jlong j; jfloat f; jdouble d; jobject l; } jvalue;
typedef struct { const char *name; const char *sig; void *fn; } JNINativeMethod;
typedef __builtin_va_list va_list;
extern int printf(const char *, ...);
extern size_t strlen(const char *);

#define TBL(env) (*(void ***)(env))
#define VMT(vm) (*(void ***)(vm))

static JavaVM *g_vm;
static int ctor_done;
__attribute__((constructor)) static void ctor(void) { ctor_done = 1; }

static jlong n_add(JNIEnv *env, jobject thiz, jint a, jlong b) { return a + b; }
static jdouble n_mix(JNIEnv *env, jobject thiz, jint a, jdouble d, jfloat f, jlong l) { return a * 1000.0 + d * 10.0 + f + (double)l; }
static jint n_many(JNIEnv *env, jobject thiz, jint a, jint b, jint c, jint d, jint e, jint f, jint g, jint h) {
    return a + 2*b + 3*c + 4*d + 5*e + 6*f + 7*g + 8*h;
}
static jboolean n_bool(JNIEnv *env, jobject thiz, jboolean x) { return !x; }
static jfloat n_float(JNIEnv *env, jobject thiz, jfloat x, jfloat y) { return x * y; }

static jint call_v(JNIEnv *env, jobject obj, jmethodID mid, ...) {
    va_list ap;
    __builtin_va_start(ap, mid);
    jint (*f)(JNIEnv *, jobject, jmethodID, va_list) = (void *)TBL(env)[51 + 0 * 0 + 1 - 1 + 1 - 1 + 1 - 1];
    (void)f;
    // CallIntMethodV es el indice 50
    jint (*fv)(JNIEnv *, jobject, jmethodID, va_list) = (void *)TBL(env)[50];
    jint r = fv(env, obj, mid, ap);
    __builtin_va_end(ap);
    return r;
}

static jint n_callback(JNIEnv *env, jobject thiz, jobject target) {
    jclass cls = ((jclass (*)(JNIEnv *, const char *))TBL(env)[6])(env, "com/x/Target");
    jmethodID mid = ((jmethodID (*)(JNIEnv *, jclass, const char *, const char *))TBL(env)[33])(env, cls, "cb", "(IDLjava/lang/String;J)I");
    jstring s = ((jstring (*)(JNIEnv *, const char *))TBL(env)[167])(env, "hello");
    // variadica: CallIntMethod (indice 49)
    jint r1 = ((jint (*)(JNIEnv *, jobject, jmethodID, ...))TBL(env)[49])(env, target, mid, 5, 2.5, s, 1000000000000L);
    // va_list: CallIntMethodV (indice 50)
    jint r2 = call_v(env, target, mid, 6, 3.5, s, 2000000000000L);
    // jvalue[]: CallIntMethodA (indice 51)
    jvalue jv[4]; jv[0].i = 7; jv[1].d = 4.5; jv[2].l = s; jv[3].j = 3000000000000L;
    jint r3 = ((jint (*)(JNIEnv *, jobject, jmethodID, const jvalue *))TBL(env)[51])(env, target, mid, jv);
    printf("callbacks: %d %d %d\n", r1, r2, r3);
    return r1 * 10000 + r2 * 100 + r3;
}


// ---- bajo nivel (los casos del banco): fallos recuperables, codigo propio, mascaras y firma mezclada ----
typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa;   // struct sigaction de bionic LP64
extern int sigaction(int, const gsa *, gsa *);
extern int sigsetjmp(long *, int) __attribute__((returns_twice));
extern void siglongjmp(long *, int) __attribute__((noreturn));
extern int pthread_sigmask(int, const unsigned long *, unsigned long *);
extern int pthread_kill(unsigned long, int);
extern unsigned long pthread_self(void);
extern int sigpending(unsigned long *);
extern void *mmap(void *, size_t, int, int, int, long);
extern int mprotect(void *, size_t, int);
extern int munmap(void *, size_t);

static long salto[33];
static volatile int vistos, por_pc;
static volatile unsigned long dir_vista;
static volatile unsigned long malo = 0x10;

static void h_salto(int s, void *si, void *uc) { vistos++; dir_vista = *(unsigned long *)((char *)si + 16); siglongjmp(salto, 1); }
static void __attribute__((noinline)) recuperacion(void) { por_pc = 1; siglongjmp(salto, 2); }
// uc->uc_mcontext.pc (aarch64): ucontext + 176 + 264
static void h_pc(int s, void *si, void *uc) { vistos++; *(unsigned long *)((char *)uc + 440) = (unsigned long)recuperacion; }
static int __attribute__((noinline)) provocar(void) { return *(volatile int *)malo; }

// 1 = siglongjmp desde el manejador; 10 = el manejador cambia el pc del contexto y vuelve (SA_NODEFER)
static jint n_fallos(JNIEnv *env, jclass c) {
    gsa sa = {4 /* SA_SIGINFO */, (void *)h_salto, 0, 0}, viejo;
    int r = 0;
    vistos = 0;
    sigaction(11, &sa, &viejo);
    if (sigsetjmp(salto, 1) == 0) provocar(); else r += vistos == 1 && dir_vista == 0x10;
    vistos = 0;
    por_pc = 0;
    sa.h = (void *)h_pc;
    sa.flags = 4 | 0x40000000; // SA_NODEFER
    sigaction(11, &sa, 0);
    if (sigsetjmp(salto, 1) == 0) provocar(); else r += 10 * (vistos == 1 && por_pc);
    sigaction(11, &viejo, 0);
    return r;
}

// escribir, hacer ejecutable, invalidar la cache de instrucciones y llamar, tres veces en la misma direccion
static jint n_codigo(JNIEnv *env, jclass c) {
    unsigned int *m = mmap(0, 4096, 3, 0x22, -1, 0);
    unsigned int esperado[3] = {0x12345678u, 0x0badcafeu, 7u};
    int ok = 0;
    for (int i = 0; i < 3; i++) {
        mprotect(m, 4096, 3);
        unsigned int v = esperado[i];
        m[0] = 0x52800000u | ((v & 0xffff) << 5);
        m[1] = 0x72a00000u | ((v >> 16) << 5);
        m[2] = 0xd65f03c0u;
        mprotect(m, 4096, 5);
        __asm__ volatile("dc cvau, %0\n dsb ish\n ic ivau, %0\n dsb ish\n isb" ::"r"(m) : "memory");
        if (((unsigned int (*)(void))(void *)m)() == v) ok++;
    }
    munmap(m, 4096);
    return ok;
}

static volatile int usr2;
static void h_usr2(int s) { usr2++; }

// 1 = el guest la ve bloqueada; 10 = pendiente sin entregar; 100 = una sola entrega al desbloquear
static jint n_mascara(JNIEnv *env, jclass c) {
    gsa sa = {0, (void *)h_usr2, 0, 0}, viejo;
    sigaction(12, &sa, &viejo);
    unsigned long b = 1ul << 11, antes = 0, ahora = 0, pend[16] = {0};
    usr2 = 0;
    pthread_sigmask(0, &b, &antes);
    pthread_sigmask(0, 0, &ahora);
    int bloqueada = (ahora & b) != 0;
    pthread_kill(pthread_self(), 12);
    sigpending(pend);
    int pendiente = (pend[0] & b) != 0 && usr2 == 0;
    pthread_sigmask(2, &antes, 0);
    int entregada = usr2 == 1;
    sigaction(12, &viejo, 0);
    return bloqueada + 10 * pendiente + 100 * entregada;
}

static int a_texto(char *o, long v) {
    char t[24];
    int n = 0, k = 0;
    if (v < 0) { o[k++] = '-'; v = -v; }
    do { t[n++] = '0' + v % 10; v /= 10; } while (v);
    while (n) o[k++] = t[--n];
    o[k] = 0;
    return k;
}

// registrado con RegisterNatives: enteros y coma flotante intercalados, mas de los que caben en registros
static jstring n_mezcla(JNIEnv *env, jclass c, jint a, jlong b, jfloat f, jdouble d, jint e, jfloat g, jdouble h, jlong i,
                        jfloat j, jdouble k, jint l, jfloat m2, jdouble n, jlong o, jfloat p, jdouble q, jint r, jdouble s) {
    double suma = a + (double)b + f + d + e + g + h + (double)i + j + k + l + m2 + n + (double)o + p + q + r + s;
    int ok = a == 1 && b == 2000000000000L && f == 3.5f && d == 4.25 && e == -5 && g == 6.5f && h == 7.125 && i == -8 &&
             j == 9.5f && k == 10.75 && l == 11 && m2 == 12.5f && n == 13.25 && o == 14 && p == 15.5f && q == 16.125 && r == 17 &&
             s == 18.5;
    char out[64];
    const char *pre = ok ? "OK " : "FALLO ";
    int z = 0;
    while (pre[z]) { out[z] = pre[z]; z++; }
    a_texto(out + z, (long)(suma * 1000));
    return ((jstring (*)(JNIEnv *, const char *))TBL(env)[167])(env, out);
}

jint Java_com_x_Test_direct(JNIEnv *env, jobject thiz, jint x) { return x * 2; }

jint JNI_OnLoad(JavaVM *vm, void *reserved) {
    g_vm = vm;
    JNIEnv *env = 0;
    jint (*getenv)(JavaVM *, void **, jint) = (void *)VMT(vm)[6];
    if (getenv(vm, (void **)&env, 0x10006) != 0) return -1;
    jclass cls = ((jclass (*)(JNIEnv *, const char *))TBL(env)[6])(env, "com/x/Test");
    JNINativeMethod m[] = {
        {"nativeAdd", "(IJ)J", n_add},
        {"nativeMix", "(IDFJ)D", n_mix},
        {"nativeMany", "(IIIIIIII)I", n_many},
        {"nativeBool", "(Z)Z", n_bool},
        {"nativeFloat", "(FF)F", n_float},
        {"nativeCallback", "(Ljava/lang/Object;)I", n_callback},
        {"fallos", "()I", n_fallos},
        {"codigo", "()I", n_codigo},
        {"mascara", "()I", n_mascara},
        {"mezcla", "(IJFDIFDJFDIFDJFDID)Ljava/lang/String;", n_mezcla},
    };
    jint (*reg)(JNIEnv *, jclass, const JNINativeMethod *, jint) = (void *)TBL(env)[215];
    if (reg(env, cls, m, 10) != 0) return -2;
    return ctor_done ? 0x10006 : -3;
}
