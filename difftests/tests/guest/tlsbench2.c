// Microbanco TLS (dependencia de libtlsbench.so): otro modulo TLS, para calentar en cada hilo la traduccion del
// resolutor (sus rutas rapida y lenta) antes de medir la ruta lenta del modulo principal.
__thread long tls_o = 3;
__attribute__((noinline)) long *addr_o(void) { return &tls_o; }
