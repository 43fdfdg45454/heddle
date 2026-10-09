// t11: otro modulo TLS (para el caso de bloque sin reservar con la generacion del DTV al dia).
__thread long t11b_v = 77;
long *t11b_addr(void) { return &t11b_v; }
