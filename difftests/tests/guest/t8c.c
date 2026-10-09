// t8: destructor de thread_local registrado con __cxa_thread_atexit_impl: la biblioteca no se descarga hasta que
// el hilo lo ejecuta (bionic cuenta una referencia por dso_handle).
extern void t8_note(int);
extern int __cxa_thread_atexit_impl(void (*)(void *), void *, void *);
static char marker;   // sin crtbegin no hay __dso_handle: vale cualquier direccion de la biblioteca
int c_data = 5;
static void c_dtor(void *p) { t8_note((int)(long)p); }
void c_register(int v) { __cxa_thread_atexit_impl(c_dtor, (void *)(long)v, &marker); }
