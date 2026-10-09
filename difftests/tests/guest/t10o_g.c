// t10: se abre con RTLD_GLOBAL: visible para dlsym(RTLD_DEFAULT) desde cualquier grupo.
int t10_glob(void) { return 40; }
// el mismo nombre que una funcion de libc: con RTLD_DEFAULT gana la de libc (cargada antes en el espacio por defecto)
int abs(int x) { (void)x; return -40; }
