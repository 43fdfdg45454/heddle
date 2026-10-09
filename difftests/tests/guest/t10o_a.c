// t10: libt10o_a (necesita libt10o_c). Define t10_who, pero la raiz de su grupo tambien (gana la raiz).
int t10_who(void) { return 1; }
int t10_a_calls_who(void) { return t10_who(); }
extern void *dlsym(void *, const char *);
// RTLD_NEXT desde esta biblioteca (sin llamada de cola: bionic toma al llamador de la direccion de retorno)
__attribute__((disable_tail_calls)) void *t10_a_next(const char *n) { return dlsym((void *)-1L, n); }
