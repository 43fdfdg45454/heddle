// t8: define una variable TLS que libt8ie.so importa con el modelo initial-exec.
__thread int t8_tls_var = 9;
int tls_get(void) { return t8_tls_var; }
