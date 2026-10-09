// Anfitrion x86-64 que simula ART: JNIEnv/JavaVM falsos + carga del native bridge por NativeBridgeItf.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <signal.h>
#include <unistd.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct { const char *name, *sig; void *fn; } NM;
typedef union { uint8_t z; int32_t i; int64_t j; float f; double d; void *l; } jvalue;
typedef struct { char shorty[32]; } MID;

static void *(*table[240])(void);
static const void *envp = table;      // JNIEnv = puntero a tabla
static void *vmt[8];
static const void *vmp = vmt;
static NM registered[16]; static int nreg;
static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)

static const char *sig_to_shorty(const char *sig, char *out) {
    int n = 0; const char *p = sig + 1; char ret = 0;
    char tmp[32];
    while (*p != ')') {
        if (*p == 'L') { tmp[n++] = 'L'; while (*p != ';') p++; p++; }
        else if (*p == '[') { tmp[n++] = 'L'; while (*p == '[') p++; if (*p == 'L') while (*p != ';') p++; p++; }
        else tmp[n++] = *p++;
    }
    p++;
    ret = (*p == '[') ? 'L' : (*p == 'L' ? 'L' : *p);
    out[0] = ret; memcpy(out + 1, tmp, n); out[n + 1] = 0;
    return out;
}

static void *m_FindClass(void *env, const char *n) { return strdup(n); }
static void *m_GetMethodID(void *env, void *cls, const char *name, const char *sig) {
    MID *m = calloc(1, sizeof *m); sig_to_shorty(sig, m->shorty); return m;
}
static void *m_NewStringUTF(void *env, const char *s) { return strdup(s); }
static const char *m_GetStringUTFChars(void *env, void *s, void *c) { return (const char *)s; }

static int cb_result(int a, double d, const char *s, long long l) { return a + (int)(d * 10) + (int)strlen(s) + (int)(l / 1000000000000LL); }

static int m_CallIntMethod(void *env, void *obj, MID *m, ...) {
    va_list ap; va_start(ap, m);
    int a = va_arg(ap, int); double d = va_arg(ap, double); const char *s = va_arg(ap, const char *); long long l = va_arg(ap, long long);
    va_end(ap);
    return cb_result(a, d, s, l);
}
static int m_CallIntMethodV(void *env, void *obj, MID *m, va_list ap) {
    int a = va_arg(ap, int); double d = va_arg(ap, double); const char *s = va_arg(ap, const char *); long long l = va_arg(ap, long long);
    return cb_result(a, d, s, l);
}
static int m_CallIntMethodA(void *env, void *obj, MID *m, const jvalue *v) {
    return cb_result(v[0].i, v[1].d, (const char *)v[2].l, v[3].j);
}
static int m_RegisterNatives(void *env, void *cls, const NM *m, int n) {
    for (int i = 0; i < n; i++) registered[nreg++] = m[i];
    return 0;
}
static int m_GetJavaVM(void *env, void **vm) { *vm = (void *)&vmp; return 0; }
static int vm_GetEnv(void *vm, void **env, int ver) { *env = (void *)&envp; return 0; }
static int vm_Attach(void *vm, void **env, void *args) { *env = (void *)&envp; return 0; }
static int unimpl(void) { printf("JNI no implementada en el mock\n"); return -1; }

static const char *rc_getMethodShorty(void *env, MID *m) { return m->shorty; }
static uint32_t rc_count(void *e, void *c) { return 0; }
static uint32_t rc_methods(void *e, void *c, void *m, uint32_t n) { return 0; }
static void *runtime_cbs[3] = { rc_getMethodShorty, rc_count, rc_methods };

typedef struct {
    uint32_t version;
    int (*initialize)(const void *, const char *, const char *);
    void *(*loadLibrary)(const char *, int);
    void *(*getTrampoline)(void *, const char *, const char *, uint32_t);
    int (*isSupported)(const char *);
    const void *(*getAppEnv)(const char *);
    int (*isCompatibleWith)(uint32_t);
    void *(*getSignalHandler)(int);
    int (*unloadLibrary)(void *);
    const char *(*getError)(void);
    int (*isPathSupported)(const char *);
    int (*initAnon)(const char *, const char *);
    void *(*createNamespace)(const char *, const char *, const char *, uint64_t, const char *, void *);
    int (*linkNamespaces)(void *, void *, const char *);
    void *(*loadLibraryExt)(const char *, int, void *);
    void *(*getVendorNamespace)(void);
    void *(*getExportedNamespace)(const char *);
    void (*preZygoteFork)(void);
    void *(*getTrampolineJNI)(void *, const char *, const char *, uint32_t, int);
    void *(*getTrampolineFP)(const void *, const char *, uint32_t, int);
    int (*isNBFP)(const void *);
} NBCB;

// Como sigchain: el manejador del proceso para los fallos pregunta primero al native bridge (getSignalHandler) y,
// si este no consume la senal, termina. Mascara completa y pila alternativa de 16 KiB, como un hilo de bionic.
static int (*nb_senal)(int, siginfo_t *, void *);
static void cadena(int sig, siginfo_t *si, void *uc) {
    if (nb_senal && nb_senal(sig, si, uc)) return;
    static const char m[] = "mockart: senal de fallo no consumida por el bridge\n";
    if (write(1, m, sizeof m - 1) < 0) {}
    _exit(3);
}
static char alt[3][16 << 10];   // [0] y [2]: margen para ver un desbordamiento de la del medio
static void instalar_cadena(void *(*get)(int)) {
    memset(alt, 0xA5, sizeof alt);
    stack_t ss = { .ss_sp = alt[1], .ss_size = sizeof alt[1], .ss_flags = 0 };
    sigaltstack(&ss, 0);
    nb_senal = (int (*)(int, siginfo_t *, void *))get(SIGSEGV);
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_sigaction = cadena;
    sa.sa_flags = SA_SIGINFO | SA_ONSTACK | SA_RESTART;
    sigfillset(&sa.sa_mask);
    sigaction(SIGSEGV, &sa, 0);
    sigaction(SIGBUS, &sa, 0);
}

// Espacios de nombres como los crea ART por classloader (bionic dlext_test: android_create_namespace, isolated,
// shared, links): rutas, accesibilidad, enlaces con la lista de bibliotecas, bibliotecas del sistema por el enlace a
// "system" y textos de error con el nombre del espacio.
#define NS_ISOLATED 1
#define NS_SHARED 2
static int err_has(NBCB *nb, const char *want) {
    const char *e = nb->getError();
    if (e && strstr(e, want)) return 1;
    printf("  error: \"%s\"\n  esperado: \"%s\"\n", e ? e : "(nulo)", want);
    return 0;
}
static void namespaces(NBCB *nb) {
    char nsdir[4096], dep[4200], alt[4096], lib[4200], altlib[4200], want[9000];
    if (!realpath("build/ns", nsdir) || !realpath("build/alt", alt)) { CHECK(0); return; }
    snprintf(dep, sizeof dep, "%s/dep", nsdir);
    snprintf(lib, sizeof lib, "%s/libt10ns.so", nsdir);
    snprintf(altlib, sizeof altlib, "%s/libt10so.so", alt);
    void *sys = nb->getExportedNamespace("system");
    CHECK(sys != 0 && nb->getExportedNamespace("system") == sys);
    CHECK(nb->getExportedNamespace("default") != 0);
    void *iso = nb->createNamespace("clns-iso", nsdir, "", NS_ISOLATED, "", 0);
    CHECK(iso != 0);
    // sin enlaces, libc.so no existe en el espacio
    CHECK(nb->loadLibraryExt("libt10ns.so", RTLD_NOW, iso) == 0);
    snprintf(want, sizeof want, "dlopen failed: library \"libc.so\" not found: needed by %s in namespace clns-iso", lib);
    CHECK(err_has(nb, want));
    CHECK(!nb->linkNamespaces(iso, sys, ""));
    CHECK(err_has(nb, "error linking namespaces \"clns-iso\"->\"system\": the list of shared libraries is empty."));
    CHECK(nb->linkNamespaces(iso, sys, "libc.so:libm.so:libdl.so"));
    // libc.so por el enlace; libt10nsdep.so no esta en las rutas del espacio
    CHECK(nb->loadLibraryExt("libt10ns.so", RTLD_NOW, iso) == 0);
    snprintf(want, sizeof want, "dlopen failed: library \"libt10nsdep.so\" not found: needed by %s in namespace clns-iso", lib);
    CHECK(err_has(nb, want));
    // otro espacio con la dependencia, enlazado solo para ella
    void *nsa = nb->createNamespace("clns-dep", dep, "", NS_ISOLATED, "", 0);
    CHECK(nsa != 0 && nb->linkNamespaces(iso, nsa, "libt10nsdep.so"));
    void *h = nb->loadLibraryExt("libt10ns.so", RTLD_NOW, iso);
    if (!h) { printf("  loadLibraryExt: %s\n", nb->getError()); CHECK(0); return; }
    int (*len)(void *, void *, const char *) = nb->getTrampoline(h, "Java_t10ns_len", "IL", 2);
    CHECK(len && len((void *)&envp, 0, "hola!") == 5);
    int (*depv)(void *, void *) = nb->getTrampoline(h, "Java_t10ns_dep", "I", 1);
    CHECK(depv && depv((void *)&envp, 0) == 77);
    // dlopen desde la biblioteca: en su espacio (libt10so.so no esta en sus rutas; libt10nsdep.so por el enlace)
    int (*op)(void *, void *) = nb->getTrampoline(h, "Java_t10ns_open", "I", 1);
    CHECK(op && op((void *)&envp, 0) == 1);
    // RTLD_DEFAULT en el espacio de la app: su RTLD_GLOBAL antes que libc (que no es de este espacio)
    int (*ord)(void *, void *) = nb->getTrampoline(h, "Java_t10ns_order", "I", 1);
    CHECK(ord && ord((void *)&envp, 0) == 1);
    // la dependencia se cargo en su espacio: desde ahi se encuentra por soname
    CHECK(nb->loadLibraryExt("libt10nsdep.so", RTLD_NOW | RTLD_NOLOAD, nsa) != 0);
    // fuera de las rutas de un espacio aislado: no accesible (texto de bionic con el nombre del espacio)
    CHECK(nb->loadLibraryExt(altlib, RTLD_NOW, iso) == 0);
    snprintf(want, sizeof want, "dlopen failed: library \"%s\" needed or dlopened by \"(unknown)\" is not accessible for the namespace \"clns-iso\"", altlib);
    CHECK(err_has(nb, want));
    // ... salvo bajo sus rutas permitidas
    void *perm = nb->createNamespace("clns-perm", nsdir, "", NS_ISOLATED, alt, 0);
    void *hp = perm ? nb->loadLibraryExt(altlib, RTLD_NOW, perm) : 0;
    CHECK(hp != 0);
    if (hp) nb->unloadLibrary(hp);
    // SHARED: hereda rutas, enlaces y las bibliotecas ya cargadas del padre (la misma)
    void *sh = nb->createNamespace("clns-shared", "", "", NS_SHARED | NS_ISOLATED, "", iso);
    CHECK(sh != 0 && nb->loadLibraryExt("libt10ns.so", RTLD_NOW, sh) == h);
    // sin SHARED, otro espacio con la misma ruta carga su propia copia
    void *other = nb->createNamespace("clns-other", nsdir, "", NS_ISOLATED, "", 0);
    CHECK(other && nb->linkNamespaces(other, sys, "libc.so:libdl.so") && nb->linkNamespaces(other, nsa, "libt10nsdep.so"));
    void *h2 = nb->loadLibraryExt("libt10ns.so", RTLD_NOW, other);
    CHECK(h2 != 0 && h2 != h);
    int (*len2)(void *, void *, const char *) = h2 ? nb->getTrampoline(h2, "Java_t10ns_len", "IL", 2) : 0;
    CHECK(len2 && len2 != len && len2((void *)&envp, 0, "abc") == 3);
}


// Lista de excepciones de bionic (is_exempt_lib, b/26394120): con targetSdkVersion < 24, un espacio que ART crea con
// ANDROID_NAMESPACE_TYPE_EXEMPT_LIST_ENABLED busca libssl.so (y las demas de la lista) tambien en el espacio por
// defecto. Desde la API 24, o sin la bandera, o con otro nombre, no.
#define NS_EXEMPT_LIST_ENABLED 0x08000000
static void exempt(NBCB *nb) {
    char nsdir[4096], ex[4096];
    if (!realpath("build/ns", nsdir) || !realpath("build/exempt", ex)) { CHECK(0); return; }
    setenv("HEDDLE_LIBPATH", ex, 1);   // las rutas del espacio por defecto
    void *sys = nb->getExportedNamespace("system");
    void *on = nb->createNamespace("clns-exempt", nsdir, "", NS_ISOLATED | NS_EXEMPT_LIST_ENABLED, "", 0);
    void *off = nb->createNamespace("clns-noexempt", nsdir, "", NS_ISOLATED, "", 0);
    CHECK(on && off && nb->linkNamespaces(on, sys, "libc.so") && nb->linkNamespaces(off, sys, "libc.so"));
    setenv("HEDDLE_TARGET_SDK", "24", 1);
    CHECK(nb->loadLibraryExt("libssl.so", RTLD_NOW, on) == 0 && err_has(nb, "dlopen failed: library \"libssl.so\" not found"));
    setenv("HEDDLE_TARGET_SDK", "23", 1);
    CHECK(nb->loadLibraryExt("libssl.so", RTLD_NOW, off) == 0 && err_has(nb, "dlopen failed: library \"libssl.so\" not found"));
    CHECK(nb->loadLibraryExt("libt10noexempt.so", RTLD_NOW, on) == 0 && err_has(nb, "dlopen failed: library \"libt10noexempt.so\" not found"));
    void *h = nb->loadLibraryExt("libssl.so", RTLD_NOW, on);
    CHECK(h != 0);
    int (*f)(void *, void *) = h ? nb->getTrampoline(h, "t10_exempt", "I", 1) : 0;
    CHECK(f && f((void *)&envp, 0) == 24);
    // se cargo en el espacio por defecto: desde ahi ya esta cargada
    void *def = nb->getExportedNamespace("default");
    CHECK(nb->loadLibraryExt("libssl.so", RTLD_NOW | RTLD_NOLOAD, def) == h);
    if (h) nb->unloadLibrary(h);
    unsetenv("HEDDLE_TARGET_SDK");
    unsetenv("HEDDLE_LIBPATH");
}

int main(int argc, char **argv) {
    for (int i = 0; i < 240; i++) table[i] = (void *)unimpl;
    table[6] = (void *)m_FindClass; table[33] = (void *)m_GetMethodID; table[167] = (void *)m_NewStringUTF;
    table[169] = (void *)m_GetStringUTFChars; table[49] = (void *)m_CallIntMethod; table[50] = (void *)m_CallIntMethodV;
    table[51] = (void *)m_CallIntMethodA; table[215] = (void *)m_RegisterNatives; table[219] = (void *)m_GetJavaVM;
    vmt[4] = vm_Attach; vmt[6] = vm_GetEnv;

    void *h = dlopen(argv[1], RTLD_NOW);
    if (!h) { printf("dlopen bridge: %s\n", dlerror()); return 1; }
    NBCB *nb = dlsym(h, "NativeBridgeItf");
    CHECK(nb && nb->version == 8);
    CHECK(nb->isCompatibleWith(8));
    CHECK(nb->initialize(runtime_cbs, "/tmp", "arm64"));
    CHECK(nb->isSupported(argv[2]));
    CHECK(!nb->isSupported(argv[1]));    // el propio bridge es x86-64
    CHECK(nb->isPathSupported("/data/app/x/lib/arm64"));
    void *ns = nb->createNamespace("app", "tests/guest", "tests/guest", 0, "", 0);
    CHECK(ns != 0);
    // como ART (LibraryNamespaces::Create): el espacio de la app se enlaza con el del sistema por la lista de
    // bibliotecas publicas (public.libraries.txt; aqui, las del NDK que usan las pruebas)
    CHECK(nb->linkNamespaces(ns, nb->getExportedNamespace("system"), "libc.so:libm.so:libdl.so:liblog.so:libandroid.so"));
    void *lib = nb->loadLibraryExt(argv[2], 2, ns);
    if (!lib) { printf("loadLibrary: %s\n", nb->getError()); return 1; }
    int (*onload)(void *, void *) = nb->getTrampoline(lib, "JNI_OnLoad", "IL", 2);
    CHECK(onload != 0);
    int v = onload((void *)&vmp, 0);
    CHECK(v == 0x10006);
    CHECK(nreg == 10);
    void *thiz = (void *)0x1234;
    for (int i = 0; i < nreg; i++) {
        char sh[32]; sig_to_shorty(registered[i].sig, sh);
        void *t = nb->getTrampolineFP(registered[i].fn, sh, strlen(sh), 1);   // ya es un trampolin: se devuelve tal cual o uno nuevo
        (void)t;
    }
    #define NATIVE(i) (registered[i].fn)
    CHECK(nb->isNBFP((void*)0) == 0);
    int64_t (*nadd)(void *, void *, int, int64_t) = NATIVE(0);
    CHECK(nadd((void *)&envp, thiz, -5, 10000000000LL) == 9999999995LL);
    double (*nmix)(void *, void *, int, double, float, int64_t) = NATIVE(1);
    CHECK(nmix((void *)&envp, thiz, 3, 1.5, 0.25f, 7) == 3000.0 + 15.0 + 0.25 + 7.0);
    int (*nmany)(void *, void *, int, int, int, int, int, int, int, int) = NATIVE(2);
    CHECK(nmany((void *)&envp, thiz, 1, 2, 3, 4, 5, 6, 7, 8) == 1 + 4 + 9 + 16 + 25 + 36 + 49 + 64);
    uint8_t (*nbool)(void *, void *, uint8_t) = NATIVE(3);
    CHECK(nbool((void *)&envp, thiz, 0) == 1 && nbool((void *)&envp, thiz, 1) == 0);
    float (*nfloat)(void *, void *, float, float) = NATIVE(4);
    CHECK(nfloat((void *)&envp, thiz, 1.5f, 4.0f) == 6.0f);
    int (*ncb)(void *, void *, void *) = NATIVE(5);
    int r = ncb((void *)&envp, thiz, (void *)"target");
    // cb_result: 5+25+5+1=36, 6+35+5+2=48, 7+45+5+3=60
    CHECK(r == 36 * 10000 + 48 * 100 + 60);
    // bajo nivel (los casos del banco), con el bridge en modo ART: los fallos llegan por getSignalHandler
    instalar_cadena(nb->getSignalHandler);
    CHECK(nb_senal != 0);
    int (*nf)(void *, void *) = NATIVE(6);
    int rf = nf((void *)&envp, thiz);
    printf("fallos: %d\n", rf);
    CHECK(rf == 11);
    CHECK(nf((void *)&envp, thiz) == 11);   // y otra vez: el estado de senales quedo limpio
    int (*nc)(void *, void *) = NATIVE(7);
    CHECK(nc((void *)&envp, thiz) == 3);
    int (*nm)(void *, void *) = NATIVE(8);
    int rm = nm((void *)&envp, thiz);
    printf("mascara: %d\n", rm);
    CHECK(rm == 111);
    const char *(*nz)(void *, void *, int, int64_t, float, double, int, float, double, int64_t, float, double, int, float,
                      double, int64_t, float, double, int, double) = NATIVE(9);
    const char *rz = nz((void *)&envp, thiz, 1, 2000000000000LL, 3.5f, 4.25, -5, 6.5f, 7.125, -8, 9.5f, 10.75, 11, 12.5f,
                        13.25, 14, 15.5f, 16.125, 17, 18.5);
    printf("mezcla: %s\n", rz ? rz : "(nulo)");
    int libre = 0;
    while (libre < (int)sizeof alt[1] && (unsigned char)alt[1][libre] == 0xA5) libre++;
    printf("pila alternativa usada: %d de %d bytes\n", (int)sizeof alt[1] - libre, (int)sizeof alt[1]);
    CHECK(libre > 2048 && (unsigned char)alt[0][sizeof alt[0] - 1] == 0xA5);
    CHECK(rz && strcmp(rz, "OK 2000000000147500") == 0);
    int (*direct)(void *, void *, int) = nb->getTrampoline(lib, "Java_com_x_Test_direct", "ILI", 3);
    CHECK(direct && direct((void *)&envp, thiz, 21) == 42);
    CHECK(nb->getTrampoline(lib, "no_existe", "V", 1) == 0);
    namespaces(nb);
    exempt(nb);
    printf("mockart: %d fallos\n", fails);
    return fails;
}
