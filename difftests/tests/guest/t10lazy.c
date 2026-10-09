// t10: referencia a una funcion que el NDK exporta (libc) pero el puente no sirve (tdestroy recibe un callback y no
// esta implementada): el dlopen funciona y el fallo queda para cuando se llame.
extern void tdestroy(void *, void (*)(void *));
void *t10lazy(void) { return (void *)tdestroy; }
