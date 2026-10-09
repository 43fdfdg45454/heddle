// t10: libc.so de mentira con versiones (para enlazar; en ejecucion la sirve el puente): malloc en la version que
// diga el guion de enlazado (LIBC, la de bionic, o LIBC_N, que bionic no le da).
void *malloc(unsigned long n) { (void)n; return 0; }
