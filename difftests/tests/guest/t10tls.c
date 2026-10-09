// t10: TLS dinamico de una biblioteca abierta con dlopen (libtest_elftls_dynamic.cpp de bionic, en C).
// Variable TLS grande exportada (dladdr y dlsym no deben confundirla con una direccion del modulo).
__thread char large_tls_var[4 * 1024 * 1024];
char *get_large_tls_var_addr(void) { return large_tls_var; }
// direccion de .bss sin entrada en la tabla de simbolos que "solapa" el valor del simbolo TLS grande
void *get_local_addr(void) { static char buf[1024]; return &buf[512]; }
// el modulo actual: relocaciones TLSDESC sin simbolo
static __thread int local_var_1 = 15;
static __thread int local_var_2 = 25;
int bump_local_vars(void) { return ++local_var_1 + ++local_var_2; }
int get_local_var1(void) { return local_var_1; }
int *get_local_var1_addr(void) { return &local_var_1; }
int get_local_var2(void) { return local_var_2; }
// TLSDESC hacia un simbolo debil sin definir: NULL
__attribute__((weak)) extern __thread int missing_weak_dyn_tls;
int *missing_weak_dyn_tls_addr(void) { return &missing_weak_dyn_tls; }
