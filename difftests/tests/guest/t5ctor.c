// Biblioteca con constructores (DT_INIT_ARRAY) y destructores (DT_FINI_ARRAY) para t5: dlopen/dlclose/dlopen.
extern int dep_value(void);
int ctor_runs;              // .bss: lo incrementa el constructor
int g_data = 7;             // .data: el constructor le suma 1
long g_bss[64];             // .bss grande
int *g_ptr = &g_data;       // relocacion R_AARCH64_RELATIVE en .data
int order[4], norder;       // orden de los constructores de esta biblioteca y su dependencia
void (*on_fini)(int);       // aviso al conductor desde el destructor
int dep_at_ctor;

__attribute__((constructor(101))) static void ctor_a(void) { ctor_runs++; g_data++; order[norder++] = 1; dep_at_ctor = dep_value(); }
__attribute__((constructor(102))) static void ctor_b(void) { order[norder++] = 2; }
__attribute__((destructor(101))) static void dtor_a(void) { if (on_fini) on_fini(1); }
__attribute__((destructor(102))) static void dtor_b(void) { if (on_fini) on_fini(2); }

int ctor_value(void) { return *g_ptr; }
