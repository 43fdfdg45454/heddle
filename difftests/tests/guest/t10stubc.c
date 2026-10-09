// t10: libc.so de mentira para enlazar (DT_NEEDED libc.so): en ejecucion la sirve el puente.
unsigned long strlen(const char *s) { unsigned long n = 0; while (s[n]) n++; return n; }
