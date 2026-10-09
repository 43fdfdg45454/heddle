// t10: importa malloc con version (DT_VERNEED de libc.so): con LIBC se resuelve; con LIBC_N, que libc no le da a
// malloc, el dlopen falla como en bionic.
extern void *malloc(unsigned long);
void *t10_vneed_malloc(void) { return (void *)malloc; }
