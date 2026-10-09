// Importa variables TLS definidas en otra biblioteca con el modelo general-dynamic (R_AARCH64_TLSDESC). Con
// initial-exec (R_AARCH64_TLS_TPREL64) bionic rechaza la carga porque libt5tls.so se abre con dlopen (ver t8); solo
// vale para una variable debil sin definir.
extern __thread int tv_ie;
extern __thread int tv_gd;   // -fPIC: TLSDESC
extern __thread char tv_buf[64];
extern int mid_marker(void);
int *use_ie_addr(void) { return &tv_ie; }
int *use_gd_addr(void) { return &tv_gd; }
int use_ie_get(void) { return tv_ie; }
int use_gd_get(void) { return tv_gd; }
void use_set(int a, int b) { tv_ie = a; tv_gd = b; }
char use_buf0(void) { return tv_buf[0]; }
int use_mid(void) { return mid_marker(); }
// TLS debil sin definir en ningun modulo (bionic): con TLSDESC la direccion es NULL; con initial-exec el
// desplazamiento queda a 0 y la direccion es la del propio tp (no NULL)
extern __thread int tv_weak_ie __attribute__((weak, tls_model("initial-exec")));
extern __thread int tv_weak_gd __attribute__((weak));
int use_weak_null(void) {
    int *a = &tv_weak_ie, *b = &tv_weak_gd;
    __asm__("" : "+r"(a), "+r"(b));   // el compilador supone que una direccion TLS nunca es NULL
    return (a != 0) + 2 * (b == 0);
}
