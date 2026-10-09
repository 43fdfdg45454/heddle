// t10 (mockart): biblioteca de una app en un espacio de nombres aislado: DT_NEEDED libc.so (la del sistema, por el
// enlace con el espacio "system") y libt10nsdep.so (en otro espacio, por su enlace).
typedef unsigned long size_t;
extern size_t strlen(const char *);
extern void *dlopen(const char *, int);
extern char *dlerror(void);
extern char *strstr(const char *, const char *);
int t10nsdep_val(void);
int Java_t10ns_len(void *env, void *cls, const char *s) { return (int)strlen(s); }
int Java_t10ns_dep(void *env, void *cls) { return t10nsdep_val(); }
// dlopen desde la biblioteca: en el espacio de quien llama. 1 = como se esperaba.
int Java_t10ns_open(void *env, void *cls) {
    // libt10so.so esta en build/, fuera de las rutas del espacio
    if (dlopen("libt10so.so", 2) != 0) return 0;
    const char *e = dlerror();
    if (!e || !strstr(e, "dlopen failed: library \"libt10so.so\" not found")) return 0;
    // libt10nsdep.so: por el enlace (ya cargada en el otro espacio)
    return dlopen("libt10nsdep.so", 2) != 0;
}
// RTLD_DEFAULT en un espacio de una app (bionic dlsym_linear_lookup): las bibliotecas del sistema no son de este
// espacio (se alcanzan por el enlace), asi que una RTLD_GLOBAL de la app con el nombre de una funcion de libc va
// antes; lo que solo esta en libc se encuentra por el grupo local de quien llama (DT_NEEDED libc.so). 1 = bien.
extern void *dlsym(void *, const char *);
int Java_t10ns_order(void *env, void *cls) {
    if (!dlopen("libt10nsg.so", 2 | 0x100)) return 0;
    int (*a)(int) = (int (*)(int))dlsym(0, "abs");
    return a && a(5) == -41 && dlsym(0, "strlen") == (void *)strlen;
}
