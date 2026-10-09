#include <stdint.h>
static int (*g_cb)(int, int, void *); static void *g_data; static int g_fd;
int ALooper_addFd(void *l, int fd, int ident, int ev, int (*cb)(int, int, void *), void *data) { g_cb = cb; g_data = data; g_fd = fd; return 1; }
int mock_fire(void) { return g_cb(g_fd, 3, g_data); }
int host_twice(int x) { return x * 2; }
void *eglGetProcAddress(const char *n) { return (void *)host_twice; }
static void *g_finished; static unsigned g_flags_add, g_flags_rm;
void ANativeActivity_finish(void *a) { g_finished = a; }
void ANativeActivity_setWindowFlags(void *a, unsigned add, unsigned rm) { if (a == g_finished) { g_flags_add = add; g_flags_rm = rm; } }
void *mock_finished(void) { return g_finished; }
unsigned mock_flags(void) { return g_flags_add * 16 + g_flags_rm; }

/* JNI simulado para leer el manifiesto como NativeActivity: PackageManager.getPackageInfo(pkg, GET_ACTIVITIES |
 * GET_META_DATA) -> activities[i].metaData.getString("android.app.func_name" / "android.app.lib_name"). Los objetos
 * son punteros a estructuras propias; los metodos y campos, punteros a su nombre. */
#include <stdlib.h>
#include <string.h>
struct JObj { int kind; const char *s; const struct JObj *md; };
enum { J_STR = 1, J_APP, J_PM, J_PKGINFO, J_ARR, J_ACT, J_BUNDLE };
static const struct JObj md0 = {J_BUNDLE, "MiEntrada|t4"}, md2 = {J_BUNDLE, "OtraEntrada|otra"};
static const struct JObj acts[3] = {{J_ACT, 0, &md0}, {J_ACT, 0, 0}, {J_ACT, 0, &md2}};
static const struct JObj japp = {J_APP}, jpm = {J_PM}, jpkginfo = {J_PKGINFO}, jarr = {J_ARR};
static const char *jclasses[] = {"android/app/ActivityThread", "android/content/Context", "android/content/pm/PackageManager",
                                 "android/content/pm/PackageInfo", "android/content/pm/ActivityInfo", "android/os/Bundle"};
static int jexc, jreads, jbad;
static const struct JObj *jstr(const char *s) { struct JObj *o = calloc(1, sizeof *o); o->kind = J_STR; o->s = strdup(s); return o; }
static void *j_findclass(void *e, const char *n) {
    for (unsigned i = 0; i < sizeof jclasses / sizeof *jclasses; i++)
        if (!strcmp(n, jclasses[i])) return (void *)jclasses[i];
    jexc = 1;
    return 0;
}
static void *j_methodid(void *e, void *c, const char *n, const char *sig) { return c ? strdup(n) : 0; }
static void *j_call_static(void *e, void *c, const char *m, const uint64_t *a) {
    return !strcmp(m, "currentApplication") && c == jclasses[0] ? (void *)&japp : 0;
}
static void *j_call(void *e, const struct JObj *o, const char *m, const uint64_t *a) {
    if (o == &japp && !strcmp(m, "getPackageManager")) return (void *)&jpm;
    if (o == &japp && !strcmp(m, "getPackageName")) return (void *)jstr("org.ejemplo.app");
    if (o == &jpm && !strcmp(m, "getPackageInfo")) {
        const struct JObj *p = (const void *)a[0];
        if (a[1] != (1 | 128) || strcmp(p->s, "org.ejemplo.app")) jbad++;
        return (void *)&jpkginfo;
    }
    if (o->kind == J_BUNDLE && !strcmp(m, "getString")) {
        const struct JObj *k = (const void *)a[0];
        const char *bar = strchr(o->s, '|');
        if (!strcmp(k->s, "android.app.func_name")) return (void *)jstr(strndup(o->s, bar - o->s));
        if (!strcmp(k->s, "android.app.lib_name")) return (void *)jstr(bar + 1);
        return 0;
    }
    jexc = 1;
    return 0;
}
static void *j_fieldid(void *e, void *c, const char *n, const char *sig) { return strdup(n); }
static void *j_getfield(void *e, const struct JObj *o, const char *f) {
    if (o == &jpkginfo && !strcmp(f, "activities")) return (void *)&jarr;
    if (o->kind == J_ACT && !strcmp(f, "metaData")) return (void *)o->md;
    jexc = 1;
    return 0;
}
static int j_len(void *e, const struct JObj *a) { return a == &jarr ? 3 : 0; }
static void *j_elem(void *e, const struct JObj *a, int i) { return a == &jarr && i >= 0 && i < 3 ? (void *)&acts[i] : 0; }
static void *j_newstr(void *e, const char *s) { return (void *)jstr(s); }
static const char *j_chars(void *e, const struct JObj *s, void *copy) { return s && s->kind == J_STR ? s->s : 0; }
static void j_release(void *e, void *s, const char *c) {}
static int j_push(void *e, int n) { return 0; }
static void *j_pop(void *e, void *r) { return 0; }
static int j_exccheck(void *e) { return jexc; }
static void j_excclear(void *e) { jexc = 0; }
static void *mtbl[240];
static const void *menv = mtbl;
static int vm_getenv(void *vm, void **env, int v) { *env = (void *)&menv; return 0; }
static void *mvmt[8] = {0, 0, 0, 0, 0, 0, (void *)vm_getenv};
static const void *mvm = mvmt;
int JNI_GetCreatedJavaVMs(void **vms, int n, int *count) {
    jreads++;
    mtbl[6] = j_findclass; mtbl[17] = j_excclear; mtbl[19] = j_push; mtbl[20] = j_pop; mtbl[33] = j_methodid;
    mtbl[36] = j_call; mtbl[94] = j_fieldid; mtbl[95] = j_getfield; mtbl[113] = j_methodid; mtbl[116] = j_call_static;
    mtbl[167] = j_newstr; mtbl[169] = j_chars; mtbl[170] = j_release; mtbl[171] = j_len; mtbl[173] = j_elem; mtbl[228] = j_exccheck;
    if (n >= 1) vms[0] = (void *)&mvm;
    *count = 1;
    return 0;
}
/* lecturas del manifiesto (una por proceso) y peticiones incorrectas */
int mock_manifest(void) { return jreads * 16 + jbad; }
