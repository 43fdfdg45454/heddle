// t10 (mockart): RTLD_GLOBAL en el espacio de la app con el nombre de una funcion de libc.
int abs(int x) { (void)x; return -41; }
