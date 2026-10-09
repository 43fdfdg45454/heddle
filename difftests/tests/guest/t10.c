// t10: el cargador como el enlazador de bionic en ARM64 (casos de tests/dlfcn_test.cpp y elftls_dl_test.cpp de
// bionic cuando se puede).
//  * orden de busqueda de simbolos (SymbolLookupList, dlsym_handle_lookup, dlsym_linear_lookup): grupo local en
//    anchura con la raiz primero (interposicion), RTLD_LOCAL de otros grupos invisible para RTLD_DEFAULT (salvo
//    targetSdkVersion < 23), RTLD_GLOBAL, DF_1_GLOBAL, RTLD_NEXT y los textos de dlerror.
//  * TLS dinamico (elftls_dl_test.cpp): valores por hilo, bloque reservado en el primer acceso, simbolo debil sin
//    definir, valores iniciales tras dlclose, identificador de modulo reutilizado, dlsym/dladdr/dl_iterate_phdr.
//  * colocacion (linker_phdr.cpp, find_libraries): alineacion de 2 MiB con p_align de 2 MiB, paginas enormes
//    transparentes y targetSdkVersion >= 31; dependencias nuevas mapeadas en orden aleatorio.
//  * simbolos como bionic: un indefinido que no existe en ninguna parte hace fallar el dlopen (uno que el NDK exporta
//    pero el puente no sirve queda como fallo perezoso), versiones (dlvsym, DT_VERNEED), dlsym con el handle de una
//    biblioteca del sistema (solo ella) y RTLD_DEFAULT en el orden de carga (las del sistema antes que las guest).
//  * android_dlopen_ext (dlext_test.cpp): banderas no validas, ANDROID_DLEXT_USE_LIBRARY_FD(_OFFSET) con sus errores,
//    FORCE_LOAD, entradas de APK no alineadas, region reservada (RESERVED_ADDRESS, HINT, RECURSIVE) y RELRO
//    compartido (WRITE_RELRO/USE_RELRO).
//  * targetSdkVersion y DT_SONAME: sin DT_SONAME, con targetSdkVersion >= 23 una biblioteca solo se reconoce por su
//    archivo (inodo); por debajo de 23 tambien por el nombre de su archivo (prelink_image).
// Se ejecuta con distintos HEDDLE_TARGET_SDK (run-all.sh).
typedef unsigned long size_t;
extern int printf(const char *, ...);
extern int snprintf(char *, size_t, const char *, ...);
extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern void *dlvsym(void *, const char *, const char *);
extern int dlclose(void *);
extern char *dlerror(void);
extern char *strstr(const char *, const char *);
extern char *strrchr(const char *, int);
extern char *strcpy(char *, const char *);
extern int strcmp(const char *, const char *);
extern int android_get_application_target_sdk_version(void);
typedef struct { const char *fname; void *fbase; const char *sname; void *saddr; } dl_info;
extern int dladdr(const void *, dl_info *);

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
#define RTLD_NOW 2
#define RTLD_NOLOAD 4
#define RTLD_GLOBAL 0x100
#define SYM(h, T, n) ((T)dlsym(h, n))

static int sdk;
static char dir[512];   // directorio de esta biblioteca (build/)

static void find_dir(void) {
    dl_info info;
    CHECK(dladdr((void *)find_dir, &info) != 0);
    strcpy(dir, info.fname);
    char *s = strrchr(dir, '/');
    if (s) *s = 0;
}

// ---- DT_SONAME y targetSdkVersion (bionic: find_loaded_library_by_soname / _by_inode, prelink_image) ----
static void soname_rules(void) {
    char alt[600];
    snprintf(alt, sizeof alt, "%s/alt/libt10noso.so", dir);
    void *a = dlopen(alt, RTLD_NOW);
    CHECK(a != 0);
    if (!a) { printf("dlopen %s: %s\n", alt, dlerror()); return; }
    // por nombre: >= 23 no hay soname (se busca el archivo en las rutas: otro inodo, otra carga); < 23 el nombre del
    // archivo hace de soname
    void *n = dlopen("libt10noso.so", RTLD_NOW | RTLD_NOLOAD);
    if (sdk >= 23) {
        CHECK(n == 0);
        const char *e = dlerror();
        CHECK(e && strstr(e, "dlopen failed: library \"libt10noso.so\" wasn't loaded and RTLD_NOLOAD prevented it"));
    } else {
        CHECK(n == a);
        if (n) dlclose(n);
    }
    void *b = dlopen("libt10noso.so", RTLD_NOW);
    CHECK(b != 0);
    if (sdk >= 23) CHECK(b != a && SYM(b, void *, "noso_id") != SYM(a, void *, "noso_id"));
    else CHECK(b == a);
    // un enlace simbolico al mismo archivo es la misma biblioteca (inodo); con < 23 el archivo de build/ aun no estaba
    // cargado (b era la copia de alt/), asi que es una carga nueva
    void *c = dlopen("libt10link.so", RTLD_NOW);
    CHECK(c != 0);
    if (sdk >= 23) CHECK(c == b);
    else CHECK(c != a);
    if (c) dlclose(c);
    if (b) dlclose(b);
    dlclose(a);
    // con DT_SONAME el nombre basta siempre
    snprintf(alt, sizeof alt, "%s/alt/libt10so.so", dir);
    a = dlopen(alt, RTLD_NOW);
    CHECK(a != 0);
    b = dlopen("libt10so.so", RTLD_NOW);
    CHECK(b == a);
    if (b) dlclose(b);
    if (a) dlclose(a);
    CHECK(dlopen("libt10so.so", RTLD_NOW | RTLD_NOLOAD) == 0);
}

// ---- orden de busqueda (dlfcn_test: dlopen_check_order_dlsym, dlopen_check_order_reloc_siblings, dlsym_df_1_global,
// dlopen_check_rtld_local/global, dlsym_with_dependencies, RTLD_NEXT) ----
typedef int (*ifn)(void);
// llama a la funcion si existe (-1 si no): un simbolo que falta no tumba la prueba
static int call(void *f) { return f ? ((ifn)f)() : -1; }
static void lookup_order(void) {
    void *r = dlopen("libt10o_root.so", RTLD_NOW);
    CHECK(r != 0);
    if (!r) { printf("dlopen libt10o_root: %s\n", dlerror()); return; }
    // relocacion con el grupo local en anchura: root, a, b, c. t10_deep: la de b (nivel 1) antes que la de c (nivel 2)
    CHECK(call(dlsym(r, "t10_root_deep")) == 2);
    // a llama a t10_who: la define la raiz, que va primero en el grupo (interposicion), no la suya
    CHECK(call(dlsym(r, "t10_a_calls_who")) == 0);
    // c resuelve su propia referencia con el grupo de la raiz: la de su hermana b
    CHECK(call(dlsym(r, "t10_c_deep")) == 2);
    // dlsym(handle): el arbol en anchura desde el handle
    CHECK(call(dlsym(r, "t10_deep")) == 2);
    CHECK(call(dlsym(r, "t10_who")) == 0);
    void *a = dlopen("libt10o_a.so", RTLD_NOW | RTLD_NOLOAD);
    CHECK(a != 0);
    if (a) {
        CHECK(call(dlsym(a, "t10_deep")) == 3);   // desde a: a, c
        CHECK(call(dlsym(a, "t10_who")) == 1);
        CHECK(dlsym(a, "t10_root_deep") == 0);   // la raiz no es dependencia de a
        // RTLD_NEXT desde a: lo que va despues de a en su grupo local (b, c)
        void *(*next)(const char *) = SYM(a, void *(*)(const char *), "t10_a_next");
        CHECK(next && call(next("t10_who")) == 2);
        CHECK(next && call(next("t10_only_c")) == 3);
        CHECK(next && next("t10_a_calls_who") == 0);
        dlclose(a);
    }
    // RTLD_DEFAULT desde esta biblioteca (otro grupo): lo RTLD_LOCAL de otros grupos no se ve (>= 23)
    if (sdk >= 23) CHECK(dlsym(0, "t10_only_c") == 0);
    else CHECK(dlsym(0, "t10_only_c") != 0 && call(dlsym(0, "t10_only_c")) == 3);
    // errores de dlsym
    CHECK(dlsym(r, "t10_no_existe") == 0);
    const char *e = dlerror();
    CHECK(e && strcmp(e, "undefined symbol: t10_no_existe") == 0);
    CHECK(dlsym((void *)0x12345, "t10_who") == 0);
    e = dlerror();
    CHECK(e && strcmp(e, "dlsym failed: invalid handle: 0x12345") == 0);
    // las bibliotecas del sistema estan en el grupo de quien las necesita (aqui ninguna: dlsym(handle) no las ve)
    CHECK(dlsym(r, "strlen") == 0);
    dlclose(r);
    CHECK(dlopen("libt10o_root.so", RTLD_NOW | RTLD_NOLOAD) == 0);
    // RTLD_GLOBAL: visible para RTLD_DEFAULT desde cualquier grupo (y no se descarga)
    void *g = dlopen("libt10o_g.so", RTLD_NOW | RTLD_GLOBAL);
    CHECK(g != 0);
    CHECK(dlsym(0, "t10_glob") != 0 && call(dlsym(0, "t10_glob")) == 40);
    if (g) dlclose(g);
    CHECK(dlopen("libt10o_g.so", RTLD_NOW | RTLD_NOLOAD) == g);
    // DF_1_GLOBAL: en el grupo global de su espacio; lo que se enlaza despues resuelve contra ella sin DT_NEEDED
    void *d = dlopen("libt10o_df1.so", RTLD_NOW);
    CHECK(d != 0);
    void *u = dlopen("libt10o_user.so", RTLD_NOW);
    CHECK(u != 0);
    if (u) {
        CHECK(call(dlsym(u, "t10_user_df1")) == 50);
        // t10_only_c (de un grupo ya descargado) es debil y no esta en su lista: 0
        CHECK(call(dlsym(u, "t10_user_has_c")) == 0);
        dlclose(u);
    }
    // una biblioteca del sistema: handle propio, dlsym por ella, dlclose sin efecto
    void *lc = dlopen("libc.so", RTLD_NOW);
    CHECK(lc != 0 && lc != g && dlsym(lc, "strlen") != 0);
    if (lc) CHECK(dlclose(lc) == 0);
    void *self = dlopen(0, RTLD_NOW);
    CHECK(self != 0 && dlsym(self, "t10_glob") != 0);
}


// ---- simbolos como bionic (linker_relocate.cpp, do_dlsym, dlsym_linear_lookup, versiones) ----
static void symbols_bionic(void) {
    // un simbolo que no existe en ninguna parte: el dlopen falla con el texto de bionic
    CHECK(dlopen("libt10undef.so", RTLD_NOW) == 0);
    const char *e = dlerror();
    CHECK(e && strstr(e, "dlopen failed: cannot locate symbol \"t10_no_existe_en_ninguna_parte\" referenced by \"") == e &&
          strstr(e, "/libt10undef.so\"..."));
    if (e && !strstr(e, "libt10undef.so\"...")) printf("dlerror: %s\n", e);
    // exportado por el NDK (libc) pero sin servir: carga (fallo perezoso al llamarlo)
    void *lz = dlopen("libt10lazy.so", RTLD_NOW);
    CHECK(lz != 0);
    if (lz) dlclose(lz);
    // dlvsym con las versiones de bionic
    CHECK(dlvsym(0, "malloc", "LIBC") != 0 && dlvsym(0, "malloc", "LIBC") == dlsym(0, "malloc"));
    CHECK(dlvsym(0, "malloc", "LIBC_N") == 0);
    e = dlerror();
    CHECK(e && strcmp(e, "undefined symbol: malloc@LIBC_N") == 0);
    CHECK(dlvsym(0, "dlvsym", "LIBC_N") != 0 && dlvsym(0, "dlvsym", "LIBC") == 0);
    // versiones de una biblioteca guest: la oculta solo con dlvsym; dlsym da la de por defecto
    void *v = dlopen("libt10ver.so", RTLD_NOW);
    CHECK(v != 0);
    if (v) {
        CHECK(call(dlvsym(v, "t10_ver", "T10_1")) == 1);
        CHECK(call(dlvsym(v, "t10_ver", "T10_2")) == 2);
        CHECK(call(dlsym(v, "t10_ver")) == 2);
        CHECK(dlvsym(v, "t10_ver", "T10_3") == 0);
        e = dlerror();
        CHECK(e && strcmp(e, "undefined symbol: t10_ver@T10_3") == 0);
        dlclose(v);
    }
    // DT_VERNEED: malloc@LIBC de libc.so se resuelve; malloc@LIBC_N no existe en libc (cannot locate symbol)
    void *vn = dlopen("libt10vneed.so", RTLD_NOW);
    CHECK(vn != 0);
    if (vn) {
        CHECK(SYM(vn, void *(*)(void), "t10_vneed_malloc")() == dlsym(0, "malloc"));
        dlclose(vn);
    } else printf("dlopen libt10vneed: %s\n", dlerror());
    CHECK(dlopen("libt10vbad.so", RTLD_NOW) == 0);
    e = dlerror();
    CHECK(e && strstr(e, "cannot locate symbol \"malloc\" referenced by \"") && strstr(e, "libt10vbad.so\"..."));
    // dlsym con el handle de una biblioteca del sistema (bionic `dlsym_handle_lookup`): ella y su arbol de DT_NEEDED en
    // anchura (libm -> libc -> libdl; libdl solo necesita ld-android)
    void *lm = dlopen("libm.so", RTLD_NOW), *ldl = dlopen("libdl.so", RTLD_NOW), *lc = dlopen("libc.so", RTLD_NOW);
    CHECK(lm && ldl && lc && lm != ldl && lm != lc);
    CHECK(dlsym(lm, "sqrt") != 0 && dlsym(lm, "malloc") == dlsym(lc, "malloc") && dlsym(lc, "malloc") != 0);
    CHECK(dlsym(ldl, "dlopen") != 0 && dlsym(lc, "dlopen") == dlsym(ldl, "dlopen") && dlsym(lm, "dlopen") != 0);
    CHECK(dlsym(ldl, "malloc") == 0 && dlsym(lc, "sqrt") == 0);
    // dladdr de una funcion del sistema: la ruta de su biblioteca en el dispositivo y el simbolo (bionic)
    dl_info si;
    void *fm = dlsym(lc, "malloc"), *fs = dlsym(lm, "sqrt");
    CHECK(fm && dladdr(fm, &si) != 0 && si.fname && strcmp(si.fname, "/apex/com.android.runtime/lib64/bionic/libc.so") == 0 &&
          si.sname && strcmp(si.sname, "malloc") == 0 && si.saddr == fm);
    CHECK(fs && dladdr(fs, &si) != 0 && si.fname && strcmp(si.fname, "/apex/com.android.runtime/lib64/bionic/libm.so") == 0 &&
          si.sname && strcmp(si.sname, "sqrt") == 0 && si.saddr == fs);
    void *ll = dlopen("liblog.so", RTLD_NOW), *fl = ll ? dlsym(ll, "__android_log_print") : 0;
    CHECK(fl && dladdr(fl, &si) != 0 && si.fname && strcmp(si.fname, "/system/lib64/liblog.so") == 0);
    // un simbolo del sistema que la biblioteca no declara en DT_NEEDED (enlazada sin libc.so): no esta en su lista de
    // busqueda y el dlopen falla como en bionic
    CHECK(dlopen("libt10noneed.so", RTLD_NOW) == 0);
    e = dlerror();
    CHECK(e && strstr(e, "cannot locate symbol \"strlen\" referenced by \"") && strstr(e, "/libt10noneed.so\"..."));
    // RTLD_DEFAULT en el orden de carga: libc (cargada al arrancar) antes que una RTLD_GLOBAL de la app
    void *g = dlopen("libt10o_g.so", RTLD_NOW | RTLD_GLOBAL);
    CHECK(g != 0);
    if (g) {
        int (*gabs)(int) = SYM(g, int (*)(int), "abs");
        int (*dabs)(int) = SYM(0, int (*)(int), "abs");
        CHECK(gabs && gabs(1) == -40);
        CHECK(dabs && dabs != gabs && dabs(-3) == 3);
    }
}

// ---- TLS dinamico (bionic tests/elftls_dl_test.cpp) ----
typedef unsigned long pthread_t;
extern int pthread_create(pthread_t *, const void *, void *(*)(void *), void *);
extern int pthread_join(pthread_t, void **);
extern void *__tls_get_addr(void *);
typedef struct { unsigned long p_type_flags, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_align; } phdr_t;
typedef struct {
    unsigned long addr; const char *name; const phdr_t *phdr; unsigned short phnum;
    unsigned long long adds, subs; size_t tls_modid; void *tls_data;
} phdr_info;
extern int dl_iterate_phdr(int (*)(phdr_info *, size_t, void *), void *);
typedef struct { int found; size_t modid; void *data; size_t memsz; } tls_info;
static int tls_cb(phdr_info *i, size_t sz, void *d) {
    tls_info *t = d;
    const char *b = strrchr(i->name, '/');
    if (strcmp(b ? b + 1 : i->name, "libt10tls.so") != 0) return 0;
    t->found = 1; t->modid = i->tls_modid; t->data = i->tls_data;
    for (int k = 0; k < i->phnum; k++)
        if ((unsigned)i->phdr[k].p_type_flags == 7 /* PT_TLS */) t->memsz = i->phdr[k].p_memsz;
    return 1;
}
static tls_info get_tls_info(void) { tls_info t = {0}; dl_iterate_phdr(tls_cb, &t); return t; }

static void *tls_lib;
static void *bump_in_thread(void *a) { return (void *)(long)SYM(tls_lib, int (*)(void), "bump_local_vars")(); }
static void *weak_in_thread(void *a) { return SYM(tls_lib, int *(*)(void), "missing_weak_dyn_tls_addr")(); }
static void *dlsym_in_thread(void *a) {
    char *v = dlsym(tls_lib, "large_tls_var");
    return (void *)(long)(v != 0 && v == SYM(tls_lib, char *(*)(void), "get_large_tls_var_addr")() && v != (char *)a);
}
static long run_thread(void *(*f)(void *), void *arg) {
    pthread_t t; void *r = 0;
    if (pthread_create(&t, 0, f, arg) != 0) return -12345;
    pthread_join(t, &r);
    return (long)r;
}

static void tls_dynamic(void) {
    void *lib = dlopen("libt10tls.so", RTLD_NOW);
    CHECK(lib != 0);
    if (!lib) { printf("dlopen libt10tls: %s\n", dlerror()); return; }
    tls_lib = lib;
    // dl_iterate_phdr: identificador de modulo y, antes del primer acceso de este hilo, sin bloque
    tls_info ti = get_tls_info();
    CHECK(ti.found && ti.modid >= 1 && ti.data == 0 && ti.memsz >= 4 * 1024 * 1024);
    // bump_local_vars
    int *(*v1addr)(void) = SYM(lib, int *(*)(void), "get_local_var1_addr");
    CHECK(v1addr && v1addr() == v1addr());
    CHECK(call(dlsym(lib, "get_local_var2")) == 25 && call(dlsym(lib, "get_local_var1")) == 15);
    CHECK(call(dlsym(lib, "bump_local_vars")) == 42);
    CHECK(run_thread(bump_in_thread, 0) == 42);       // otro hilo: sus propios valores iniciales
    CHECK(call(dlsym(lib, "bump_local_vars")) == 44);
    // tlsdesc_missing_weak
    CHECK(SYM(lib, int *(*)(void), "missing_weak_dyn_tls_addr")() == 0);
    CHECK(run_thread(weak_in_thread, 0) == 0);
    // dlsym_dynamic_tls: la copia de cada hilo
    char *big = dlsym(lib, "large_tls_var");
    CHECK(big != 0 && big == SYM(lib, char *(*)(void), "get_large_tls_var_addr")());
    CHECK(run_thread(dlsym_in_thread, big) == 1);
    // dladdr_on_tls_var / dladdr_skip_tls_symbol
    dl_info info;
    CHECK(dladdr(big, &info) == 0);
    void *la = SYM(lib, void *(*)(void), "get_local_addr")();
    CHECK(dladdr(la, &info) != 0 && strstr(info.fname, "libt10tls.so") && info.sname == 0 && info.saddr == 0);
    // dl_iterate_phdr tras el primer acceso: el bloque de este hilo cubre la variable; __tls_get_addr lo mismo
    ti = get_tls_info();
    CHECK(ti.found && ti.data != 0 && (char *)big >= (char *)ti.data && (char *)big < (char *)ti.data + ti.memsz);
    size_t idx[2] = { ti.modid, 0 };
    CHECK(__tls_get_addr(idx) == ti.data);
    size_t modid = ti.modid;
    CHECK(dlclose(lib) == 0);
    // dlclose_resets_values y dlclose_removes_entry: valores iniciales otra vez y el mismo identificador de modulo
    for (int round = 0; round < 8; round++) {
        lib = dlopen("libt10tls.so", RTLD_NOW);
        CHECK(lib != 0);
        if (!lib) return;
        CHECK(call(dlsym(lib, "bump_local_vars")) == 42);
        CHECK(call(dlsym(lib, "bump_local_vars")) == 44);
        CHECK(get_tls_info().modid == modid);
        CHECK(dlclose(lib) == 0);
    }
}


// ---- rutas reales en dladdr y dl_iterate_phdr (bionic: realpath; dentro de un APK, "base.apk!/lib/...") ----
typedef struct { const char *want; int found; } name_info;
static int name_cb(phdr_info *i, size_t sz, void *d) {
    name_info *n = d;
    if (strcmp(i->name, n->want) == 0) n->found = 1;
    return 0;
}
static int iterate_has(const char *name) { name_info n = { name, 0 }; dl_iterate_phdr(name_cb, &n); return n.found; }
static void real_paths(void) {
    char p[600], want[600];
    dl_info info;
    // por un enlace simbolico: la ruta del archivo real
    snprintf(p, sizeof p, "%s/libt10link.so", dir);
    snprintf(want, sizeof want, "%s/libt10noso.so", dir);
    void *h = dlopen(p, RTLD_NOW);
    CHECK(h != 0);
    if (h) {
        CHECK(dladdr(dlsym(h, "noso_id"), &info) != 0 && strcmp(info.fname, want) == 0);
        CHECK(iterate_has(want) && !iterate_has(p));
        dlclose(h);
    }
    // dentro de un APK sin comprimir
    snprintf(p, sizeof p, "%s/t10.apk!/lib/arm64-v8a/libt10apk.so", dir);
    h = dlopen(p, RTLD_NOW);
    CHECK(h != 0);
    if (!h) { printf("dlopen %s: %s\n", p, dlerror()); return; }
    CHECK(call(dlsym(h, "t10apk_id")) == 61);
    CHECK(dladdr(dlsym(h, "t10apk_id"), &info) != 0 && strcmp(info.fname, p) == 0);
    CHECK(iterate_has(p));
    dlclose(h);
}

// ---- colocacion (bionic ReserveAddressSpace y shuffle de find_libraries) ----
extern int open(const char *, int, ...);
extern long read(int, void *, unsigned long);
extern int close(int);
static int thp_supported(void) {
    char b[128] = {0};
    int fd = open("/sys/kernel/mm/transparent_hugepage/enabled", 0);
    if (fd < 0) return 0;
    long n = read(fd, b, sizeof b - 1);
    close(fd);
    return n > 0 && !strstr(b, "[never]");
}
static unsigned long base_of(void *h, const char *sym) {
    dl_info i;
    void *f = h ? dlsym(h, sym) : 0;
    return f && dladdr(f, &i) ? (unsigned long)i.fbase : 0;
}
static void placement(void) {
    int thp = thp_supported(), all_huge = 1, all_plain = 1;
    for (int r = 0; r < 8; r++) {
        void *h = dlopen("libt10huge.so", RTLD_NOW);
        unsigned long b = base_of(h, "t10huge_id");
        CHECK(b != 0);
        if (b & 0x1fffff) all_huge = 0;
        if (h) dlclose(h);
        // una biblioteca normal nunca se alinea a 2 MiB a proposito (inicio aleatorio por paginas)
        h = dlopen("libt10so.so", RTLD_NOW);
        b = base_of(h, "so_id");
        if (b & 0x1fffff) all_plain = 0;
        if (h) dlclose(h);
    }
    if (thp && sdk >= 31) CHECK(all_huge);
    else CHECK(!all_huge);
    CHECK(!all_plain);
    // orden de mapeo de las dependencias nuevas: aleatorio en cada carga
    char first[8] = {0}, cur[8];
    int varied = 0;
    for (int r = 0; r < 12 && !varied; r++) {
        void *h = dlopen("libt10p_root.so", RTLD_NOW);
        CHECK(h != 0);
        if (!h) return;
        unsigned long b[6];
        char sym[16];
        for (int i = 0; i < 6; i++) {
            snprintf(sym, sizeof sym, "t10p_%d", i + 1);
            b[i] = base_of(h, sym);
            CHECK(b[i] != 0);
        }
        // rango de cada una por direccion
        for (int i = 0; i < 6; i++) {
            int k = 0;
            for (int j = 0; j < 6; j++) k += b[j] < b[i];
            cur[i] = (char)('0' + k);
        }
        cur[6] = 0;
        if (r == 0) strcpy(first, cur);
        else if (strcmp(first, cur) != 0) varied = 1;
        dlclose(h);
    }
    CHECK(varied);
}


// ---- android_dlopen_ext (bionic dlext_test.cpp): descriptor, desplazamiento, FORCE_LOAD, region reservada, RELRO ----
typedef struct {
    unsigned long flags; void *reserved_addr; size_t reserved_size; int relro_fd; int library_fd;
    long library_fd_offset; void *library_namespace;
} dlextinfo;
#define DLEXT_RESERVED_ADDRESS 0x1
#define DLEXT_RESERVED_ADDRESS_HINT 0x2
#define DLEXT_WRITE_RELRO 0x4
#define DLEXT_USE_RELRO 0x8
#define DLEXT_USE_LIBRARY_FD 0x10
#define DLEXT_USE_LIBRARY_FD_OFFSET 0x20
#define DLEXT_FORCE_LOAD 0x40
#define DLEXT_USE_NAMESPACE 0x200
#define DLEXT_RESERVED_ADDRESS_RECURSIVE 0x400
extern void *android_dlopen_ext(const char *, int, const dlextinfo *);
extern long pread(int, void *, unsigned long, long);
extern long lseek(int, long, int);
extern void *mmap(void *, unsigned long, int, int, int, long);
extern int munmap(void *, unsigned long);
extern int mprotect(void *, unsigned long, int);
extern int getpagesize(void);
static int err_is(const char *want) {
    const char *e = dlerror();
    if (e && strstr(e, want)) return 1;
    printf("  dlerror: \"%s\"\n  esperado: \"%s\"\n", e ? e : "(nulo)", want);
    return 0;
}
static int ends_with(const char *s, const char *suf) {
    unsigned long a = 0, b = 0;
    while (s && s[a]) a++;
    while (suf[b]) b++;
    return s && a >= b && strcmp(s + a - b, suf) == 0;
}
static int maps_has(const char *what) {
    static char buf[1 << 16];
    int fd = open("/proc/self/maps", 0), found = 0;
    if (fd < 0) return 0;
    long n, keep = 0;
    while ((n = read(fd, buf + keep, sizeof buf - 1 - keep)) > 0) {
        buf[keep + n] = 0;
        if (strstr(buf, what)) found = 1;
        // conserva la ultima linea a medias
        char *nl = strrchr(buf, '\n');
        keep = nl ? (long)(keep + n - (nl + 1 - buf)) : 0;
        if (nl) { char *d = buf, *q = nl + 1; while (*q) *d++ = *q++; }
    }
    close(fd);
    return found;
}
static void dlext(void) {
    char p[600], want[900];
    dlextinfo e;
    dl_info info;
    long pg = getpagesize();
    // validacion de las banderas (do_dlopen)
    e = (dlextinfo){ .flags = 0x80 };
    CHECK(android_dlopen_ext("libt10ext.so", RTLD_NOW, &e) == 0 && err_is("dlopen failed: invalid extended flags to android_dlopen_ext: 0x80"));
    e = (dlextinfo){ .flags = DLEXT_USE_LIBRARY_FD_OFFSET };
    CHECK(android_dlopen_ext("libt10ext.so", RTLD_NOW, &e) == 0 && err_is("invalid extended flag combination (ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET without ANDROID_DLEXT_USE_LIBRARY_FD): 0x20"));
    e = (dlextinfo){ .flags = DLEXT_USE_NAMESPACE };
    CHECK(android_dlopen_ext("libt10ext.so", RTLD_NOW, &e) == 0 && err_is("ANDROID_DLEXT_USE_NAMESPACE is set but extinfo->library_namespace is null"));
    // por descriptor: cualquier nombre; la ruta real es la del descriptor; despues, el mismo archivo por ruta es la
    // misma biblioteca (inodo) y con FORCE_LOAD otra copia
    snprintf(p, sizeof p, "%s/libt10ext.so", dir);
    int fd = open(p, 0);
    CHECK(fd >= 0);
    e = (dlextinfo){ .flags = DLEXT_USE_LIBRARY_FD, .library_fd = fd };
    void *h = android_dlopen_ext("libt10ext_por_fd.so", RTLD_NOW, &e);
    CHECK(h != 0);
    if (!h) { printf("  %s\n", dlerror()); return; }
    CHECK(call(dlsym(h, "t10ext_id")) == 62);
    CHECK(dladdr(dlsym(h, "t10ext_id"), &info) && strcmp(info.fname, p) == 0);
    void *h2 = dlopen(p, RTLD_NOW);
    CHECK(h2 == h);
    void *h3 = android_dlopen_ext("libt10ext_forzada.so", RTLD_NOW, &(dlextinfo){ .flags = DLEXT_USE_LIBRARY_FD | DLEXT_FORCE_LOAD, .library_fd = fd });
    CHECK(h3 != 0 && h3 != h && dlsym(h3, "t10ext_id") != dlsym(h, "t10ext_id"));
    if (h3) dlclose(h3);
    dlclose(h2);
    dlclose(h);
    close(fd);
    // descriptor no valido
    e = (dlextinfo){ .flags = DLEXT_USE_LIBRARY_FD, .library_fd = 9999 };
    CHECK(android_dlopen_ext("libt10ext.so", RTLD_NOW, &e) == 0 && err_is("unable to stat file for the library \"libt10ext.so\": Bad file descriptor"));
    // dentro de un APK por descriptor y desplazamiento: la ruta real es la del APK
    snprintf(p, sizeof p, "%s/t10.apk", dir);
    fd = open(p, 0);
    long off = -1, size = lseek(fd, 0, 2);
    for (long o = 0; o < size; o += pg) {
        char m[4];
        if (pread(fd, m, 4, o) == 4 && m[0] == 0x7f && m[1] == 'E' && m[2] == 'L' && m[3] == 'F') { off = o; break; }
    }
    CHECK(off > 0);
    e = (dlextinfo){ .flags = DLEXT_USE_LIBRARY_FD | DLEXT_USE_LIBRARY_FD_OFFSET, .library_fd = fd, .library_fd_offset = off };
    h = android_dlopen_ext("libt10apk.so", RTLD_NOW, &e);
    CHECK(h != 0);
    if (h) {
        CHECK(call(dlsym(h, "t10apk_id")) == 61);
        CHECK(dladdr(dlsym(h, "t10apk_id"), &info) && strcmp(info.fname, p) == 0);
        dlclose(h);
    }
    e.library_fd_offset = 100;
    CHECK(android_dlopen_ext("libt10apk.so", RTLD_NOW, &e) == 0 && err_is("file offset for the library \"libt10apk.so\" is not page-aligned: 100"));
    e.library_fd_offset = -pg;
    snprintf(want, sizeof want, "file offset for the library \"libt10apk.so\" is negative: %ld", -pg);
    CHECK(android_dlopen_ext("libt10apk.so", RTLD_NOW, &e) == 0 && err_is(want));
    e.library_fd_offset = 1L << 30;
    snprintf(want, sizeof want, "file offset for the library \"libt10apk.so\" >= file size: %ld >= %ld", 1L << 30, size);
    CHECK(android_dlopen_ext("libt10apk.so", RTLD_NOW, &e) == 0 && err_is(want));
    close(fd);
    // entrada de APK no alineada a pagina: como si no existiera (open_library_in_zipfile)
    snprintf(p, sizeof p, "%s/t10bad.apk!/lib/arm64-v8a/libt10apk.so", dir);
    snprintf(want, sizeof want, "dlopen failed: library \"%s\" not found", p);
    CHECK(dlopen(p, RTLD_NOW) == 0 && err_is(want));
    // region reservada por el llamador: la biblioteca va al principio; al cerrarla la region sigue reservada
    unsigned long rsz = 8 << 20;
    char *r = mmap(0, rsz, 0, 0x22 /* MAP_PRIVATE|MAP_ANONYMOUS */, -1, 0);
    CHECK(r != (char *)-1);
    snprintf(p, sizeof p, "%s/libt10ext.so", dir);
    e = (dlextinfo){ .flags = DLEXT_RESERVED_ADDRESS, .reserved_addr = r, .reserved_size = rsz };
    h = android_dlopen_ext(p, RTLD_NOW, &e);
    CHECK(h != 0 && base_of(h, "t10ext_id") == (unsigned long)r && call(dlsym(h, "t10ext_id")) == 62);
    if (h) dlclose(h);
    CHECK(mprotect(r, pg, 1) == 0 && *(volatile int *)r == 0 && mprotect(r, pg, 0) == 0);
    e.reserved_size = pg;
    CHECK(android_dlopen_ext(p, RTLD_NOW, &e) == 0);
    const char *er = dlerror();
    CHECK(er && strstr(er, "dlopen failed: reserved address space ") && strstr(er, " bytes needed for \"") && strstr(er, "/libt10ext.so\""));
    // HINT: si no cabe, donde decida el kernel; si cabe, en la region
    e.flags = DLEXT_RESERVED_ADDRESS_HINT;
    h = android_dlopen_ext(p, RTLD_NOW, &e);
    CHECK(h != 0 && base_of(h, "t10ext_id") != (unsigned long)r);
    if (h) dlclose(h);
    e.reserved_size = rsz;
    h = android_dlopen_ext(p, RTLD_NOW, &e);
    CHECK(h != 0 && base_of(h, "t10ext_id") == (unsigned long)r);
    if (h) dlclose(h);
    // RECURSIVE: tambien las dependencias, en orden de carga y sin barajar; sin el, solo la pedida
    snprintf(p, sizeof p, "%s/libt10p_root.so", dir);
    for (int rec = 0; rec < 2; rec++) {
        e = (dlextinfo){ .flags = DLEXT_RESERVED_ADDRESS | (rec ? DLEXT_RESERVED_ADDRESS_RECURSIVE : 0), .reserved_addr = r, .reserved_size = rsz };
        h = android_dlopen_ext(p, RTLD_NOW, &e);
        CHECK(h != 0);
        if (!h) { printf("  %s\n", dlerror()); continue; }
        CHECK(base_of(h, "t10p_root") == (unsigned long)r);
        unsigned long prev = (unsigned long)r;
        char sym[16];
        for (int i = 1; i <= 6; i++) {
            snprintf(sym, sizeof sym, "t10p_%d", i);
            unsigned long b = base_of(h, sym);
            int inside = b > (unsigned long)r && b < (unsigned long)r + rsz;
            CHECK(rec ? inside && b > prev : !inside);
            prev = b;
        }
        dlclose(h);
    }
    // RELRO compartido (WebView): escrito en un archivo y mapeado desde el; otra carga en la misma direccion lo usa
    char relro[600];
    snprintf(p, sizeof p, "%s/libt10relro.so", dir);
    snprintf(relro, sizeof relro, "%s/t10relro.bin", dir);
    fd = open(relro, 0102 | 01000 /* O_RDWR|O_CREAT|O_TRUNC */, 0644);
    CHECK(fd >= 0);
    e = (dlextinfo){ .flags = DLEXT_RESERVED_ADDRESS | DLEXT_WRITE_RELRO, .reserved_addr = r, .reserved_size = rsz, .relro_fd = fd };
    h = android_dlopen_ext(p, RTLD_NOW, &e);
    CHECK(h != 0);
    if (!h) printf("  %s\n", dlerror());
    long written = lseek(fd, 0, 2);
    CHECK(written > 0 && written % pg == 0);
    CHECK(maps_has("/t10relro.bin"));
    if (h) {
        CHECK(((int (*)(int))dlsym(h, "relro_get"))(1) == 2);
        dlclose(h);
    }
    CHECK(!maps_has("/t10relro.bin"));
    e.flags = DLEXT_RESERVED_ADDRESS | DLEXT_USE_RELRO;
    h = android_dlopen_ext(p, RTLD_NOW, &e);
    CHECK(h != 0);
    CHECK(maps_has("/t10relro.bin"));
    if (h) {
        CHECK(((int (*)(int))dlsym(h, "relro_get"))(0) == 1);
        dlclose(h);
    }
    close(fd);
    e = (dlextinfo){ .flags = DLEXT_WRITE_RELRO, .relro_fd = -1 };
    snprintf(want, sizeof want, "failed serializing GNU RELRO section for \"%s\": Bad file descriptor", p);
    CHECK(android_dlopen_ext(p, RTLD_NOW, &e) == 0 && err_is(want));
    munmap(r, rsz);
}

// ---- tamano de pagina (HEDDLE_PAGE_SIZE=16384: como un dispositivo arm64 con paginas de 16 KiB) ----
extern char *getenv(const char *);
extern long sysconf(int);
extern unsigned long getauxval(unsigned long);
extern int *__errno(void);
extern int setenv(const char *, const char *, int);
extern int unsetenv(const char *);
extern int madvise(void *, unsigned long, int);
extern int msync(void *, unsigned long, int);
static void page_size(void) {
    const char *e = getenv("HEDDLE_PAGE_SIZE");
    long want = e && strcmp(e, "16384") == 0 ? 16384 : 4096;
    long pg = getpagesize();
    CHECK(pg == want && sysconf(39 /* _SC_PAGESIZE */) == want && sysconf(40 /* _SC_PAGE_SIZE */) == want);
    CHECK((long)getauxval(6 /* AT_PAGESZ */) == want);
    // mmap como su kernel: direcciones alineadas a la pagina, longitudes redondeadas, desplazamiento alineado
    for (int i = 0; i < 8; i++) {
        char *m = mmap(0, 100, 3, 0x22, -1, 0);
        CHECK(m != (char *)-1 && (unsigned long)m % pg == 0);
        m[pg - 1] = 1;   // toda la pagina es accesible
        if (pg > 4096) CHECK(munmap(m + 4096, 1) == -1);   // dentro de la pagina: no alineado
        CHECK(munmap(m, 1) == 0);
    }
    char *m = mmap(0, 4 * pg, 3, 0x22, -1, 0);
    CHECK(mprotect(m + 4096, 4096, 1) == (pg > 4096 ? -1 : 0));
    CHECK(madvise(m + 4096, 4096, 4 /* MADV_DONTNEED */) == (pg > 4096 ? -1 : 0));
    CHECK(msync(m + 4096, 4096, 4 /* MS_SYNC */) == (pg > 4096 ? -1 : 0) || pg == 4096);
    CHECK(mprotect(m + pg, 1, 1) == 0);
    munmap(m, 4 * pg);
    int fd = open("/proc/self/exe", 0);
    m = mmap(0, 4096, 1, 2 /* MAP_PRIVATE */, fd, 4096);
    CHECK(pg > 4096 ? m == (char *)-1 && *__errno() == 22 : m != (char *)-1);
    close(fd);
    if (pg == 4096) return;
    // cargador: p_align de 4 KiB solo en el modo de compatibilidad de bionic (Android 15+)
    char p[600], want_err[900];
    snprintf(p, sizeof p, "%s/libt10p4k.so", dir);
    snprintf(want_err, sizeof want_err, "dlopen failed: \"%s\" program alignment (4096) cannot be smaller than system page size (16384)", p);
    CHECK(dlopen(p, RTLD_NOW) == 0 && err_is(want_err));
    setenv("HEDDLE_PAGE_COMPAT", "1", 1);
    void *h = dlopen(p, RTLD_NOW);
    CHECK(h != 0);
    if (!h) printf("  %s\n", dlerror());
    if (h) {
        // codigo, RELRO relocado, datos y bss
        CHECK(((int (*)(int))dlsym(h, "p4k_get"))(1) == 20);
        int (*bump)(void) = (int (*)(void))dlsym(h, "p4k_bump");
        CHECK(bump && bump() == 1 && bump() == 2);
        // la region RELRO de compatibilidad es RX en bionic: tambien se ejecuta lo que hay en solo lectura
        int (*ro_code)(void) = SYM(h, int (*)(void), "p4k_ro_code");
        CHECK(ro_code && ro_code() == 42);
        dlclose(h);
    }
    unsetenv("HEDDLE_PAGE_COMPAT");
}

// ---- protecciones de segmentos (bionic LoadSegments con PFLAGS_TO_PROT; RELRO tras relocar), nota PAD_SEGMENT y
//      relocaciones de texto ----
extern int pipe(int *);
extern long write(int, const void *, unsigned long);
static int probe_fd[2] = { -1, -1 };
// legible: write desde la direccion (EFAULT si no); escribible: read hacia ella
static int readable(const void *a) {
    if (probe_fd[0] < 0) pipe(probe_fd);
    char c;
    if (write(probe_fd[1], a, 1) != 1) return 0;
    read(probe_fd[0], &c, 1);
    return 1;
}
static int writable(void *a) {
    if (probe_fd[0] < 0) pipe(probe_fd);
    char c = *(volatile char *)a;
    write(probe_fd[1], &c, 1);
    return read(probe_fd[0], a, 1) == 1;
}
static void segments(void) {
    char p[600], want[900];
    snprintf(p, sizeof p, "%s/libt10seg.so", dir);
    void *h = dlopen(p, RTLD_NOW);
    CHECK(h != 0);
    if (h) {
        const char *ro = SYM(h, const char *(*)(void), "seg_ro_addr")();
        int *const *relro = SYM(h, int *const *(*)(void), "seg_relro_addr")();
        int *rw = SYM(h, int *(*)(void), "seg_rw_addr")();
        void *code = SYM(h, void *(*)(void), "seg_code_addr")();
        CHECK(readable(ro) && !writable((void *)ro));
        CHECK(readable(code) && !writable(code));
        CHECK(**relro == 7 && readable(relro) && !writable((void *)relro));
        CHECK(*rw == 5 && writable(rw));
        dlclose(h);
    }
    // DT_TEXTREL: "has text relocations" (LP64)
    snprintf(p, sizeof p, "%s/libt10textrel.so", dir);
    snprintf(want, sizeof want, "dlopen failed: \"%s\" has text relocations", p);
    CHECK(dlopen(p, RTLD_NOW) == 0 && err_is(want));
    // nota PAD_SEGMENT (con migracion de tamano de pagina): el primer PT_LOAD se extiende hasta el siguiente, sin
    // hueco inaccesible entre los dos (solo con paginas de 4 KiB: p_align 16 KiB > pagina)
    if (getpagesize() == 4096) {
        snprintf(p, sizeof p, "%s/libt10pad.so", dir);
        h = dlopen(p, RTLD_NOW);
        CHECK(h != 0);
        if (h) {
            dl_info info;
            CHECK(dladdr(dlsym(h, "seg_ro_addr"), &info) != 0);
            const char *gap = (const char *)info.fbase + 0x2000;   // entre el fin del primero y el inicio del segundo
            const char *e = getenv("HEDDLE_PGSIZE_MIGRATION");
            int pad = e && strcmp(e, "1") == 0;
            CHECK(readable(gap) == pad);
            if (pad) CHECK(!writable((void *)gap));
            dlclose(h);
        }
    }
}

// ---- paginas de 16 KiB: lo que el host hace a 4 KiB se ve como en un kernel de 16 KiB (cola de ceros de un mapeo
//      de archivo, /proc/self/maps y smaps, mincore, mlock y brk) ----
extern int mincore(void *, unsigned long, unsigned char *);
extern int mlock(const void *, unsigned long);
extern int munlock(const void *, unsigned long);
extern int brk(void *);
extern void *sbrk(long);
typedef struct FILE FILE;
extern FILE *fopen(const char *, const char *);
extern char *fgets(char *, int, FILE *);
extern int fclose(FILE *);
extern int unlink(const char *);
extern unsigned long strtoul(const char *, char **, int);
static char big[1 << 20];
static long read_all(const char *p) {
    int fd = open(p, 0);
    long n = 0, r;
    if (fd < 0) return -1;
    while (n < (long)sizeof big - 1 && (r = read(fd, big + n, sizeof big - 1 - n)) > 0) n += r;
    big[n] = 0;
    close(fd);
    return n;
}
static unsigned long vm_lck(void) {
    if (read_all("/proc/self/status") <= 0) return ~0ul;
    char *l = strstr(big, "VmLck:");
    return l ? strtoul(l + 6, 0, 10) : ~0ul;
}
static void page16(void) {
    if (getpagesize() != 16384) return;
    // mapeo de un archivo de 5000 bytes: la pagina de 16 KiB tiene ceros tras el final (sin SIGBUS) y en maps es un
    // solo VMA de 16 KiB
    char p[600];
    snprintf(p, sizeof p, "%s/t10tail.tmp", dir);
    int fd = open(p, 0102 | 01000, 0644);   // O_RDWR | O_CREAT | O_TRUNC
    CHECK(fd >= 0);
    char a5[5000];
    for (int i = 0; i < 5000; i++) a5[i] = 'a';
    CHECK(write(fd, a5, 5000) == 5000);
    char *m = mmap(0, 16384, 3, 2 /* MAP_PRIVATE */, fd, 0);
    CHECK(m != (char *)-1);
    if (m != (char *)-1) {
        CHECK(m[4999] == 'a' && m[5000] == 0 && m[8191] == 0 && m[8192] == 0 && m[16383] == 0);
        m[16383] = 7;
        CHECK(m[16383] == 7);
        FILE *f = fopen("/proc/self/maps", "re");
        CHECK(f != 0);
        char line[512];
        unsigned long prev = 0;
        int lines = 0, bad = 0, mine = 0;
        while (f && fgets(line, sizeof line, f)) {
            char *e;
            unsigned long s0 = strtoul(line, &e, 16), e0 = strtoul(e + 1, 0, 16);
            lines++;
            if (s0 % 16384 || e0 % 16384 || s0 < prev || e0 <= s0) bad++;
            if (s0 <= (unsigned long)m && (unsigned long)m < e0) {
                mine++;
                CHECK(s0 == (unsigned long)m && e0 == (unsigned long)m + 16384 && strstr(line, "t10tail.tmp"));
            }
            prev = e0;
        }
        if (f) fclose(f);
        CHECK(lines > 10 && bad == 0 && mine == 1);
        munmap(m, 16384);
    }
    close(fd);
    unlink(p);
    // smaps por open: paginas del kernel de 16 KiB
    CHECK(read_all("/proc/self/smaps") > 0 && strstr(big, "KernelPageSize:       16 kB") && !strstr(big, "KernelPageSize:        4 kB"));
    // mincore: un byte por pagina de 16 KiB, inicio alineado
    char *a = mmap(0, 4 * 16384, 3, 0x22, -1, 0);
    unsigned char v[4] = { 9, 9, 9, 9 };
    a[0] = 1;
    CHECK(mincore(a, 4 * 16384, v) == 0 && v[0] == 1 && v[1] == 0 && v[3] == 0);
    CHECK(mincore(a + 4096, 4096, v) == -1 && *__errno() == 22);
    CHECK(mincore(a, 1, v) == 0 && v[0] == 1);
    // mlock bloquea la pagina de 16 KiB entera
    unsigned long l0 = vm_lck();
    CHECK(mlock(a + 100, 10) == 0 && vm_lck() == l0 + 16);
    CHECK(munlock(a + 100, 10) == 0 && vm_lck() == l0);
    munmap(a, 4 * 16384);
    // brk: el inicio alineado a 16 KiB, el guest ve el valor pedido y es accesible hasta el final de su pagina
    char *b0 = sbrk(0);
    CHECK(((unsigned long)b0 % 16384) == 0);
    CHECK(sbrk(100) == b0 && sbrk(0) == b0 + 100);
    b0[16383] = 1;
    CHECK(brk(b0) == 0 && sbrk(0) == b0);
}

// ---- modo de compatibilidad de 16 KiB automatico (Android 16: manifiesto android:pageSizeCompat y alineacion de
//      las bibliotecas de la app). scripts/run-all.sh da la app (HEDDLE_APK, HEDDLE_NATIVE_LIB_DIR o la ruta de la
//      biblioteca) y lo esperado (T10_COMPAT=1/0); la biblioteca es libt10p4k.so (p_align de 4 KiB) o T10_COMPAT_LIB ----
int compat_auto(void) {
    find_dir();
    const char *w = getenv("T10_COMPAT"), *l = getenv("T10_COMPAT_LIB");
    int want = w && w[0] == '1';
    char p[600];
    if (l) snprintf(p, sizeof p, "%s", l);
    else snprintf(p, sizeof p, "%s/libt10p4k.so", dir);
    void *h = dlopen(p, RTLD_NOW);
    CHECK((h != 0) == want);
    if (h) {
        CHECK(((int (*)(int))dlsym(h, "p4k_get"))(1) == 20);
        dlclose(h);
    } else if (want) {
        printf("  %s\n", dlerror());
    } else {
        // sin el modo: el error de bionic
        const char *e = dlerror();
        CHECK(e && strstr(e, "program alignment (4096) cannot be smaller than system page size (16384)"));
    }
    printf("t10 compat_auto: %d fallos\n", fails);
    return fails;
}

int run_all(void) {
    sdk = android_get_application_target_sdk_version();
    find_dir();
    soname_rules();
    lookup_order();
    symbols_bionic();
    tls_dynamic();
    real_paths();
    placement();
    dlext();
    page_size();
    page16();
    segments();
    printf("t10 (targetSdkVersion %d) run_all: %d fallos\n", sdk, fails);
    return fails;
}
