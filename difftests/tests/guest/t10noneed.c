// t10: usa strlen sin declarar libc.so en DT_NEEDED (enlazada sin los stubs del NDK): bionic no la carga
extern unsigned long strlen(const char *);
unsigned long t10_noneed(const char *s) { return strlen(s); }
