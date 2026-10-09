// Dependencia de libt5ctor.so: su estado tambien debe volver a la imagen inicial cuando se descarga con ella.
int dep_inits;          // .bss
int dep_data = 100;     // .data
__attribute__((constructor)) static void dep_ctor(void) { dep_inits++; dep_data++; }
int dep_value(void) { return dep_data; }
