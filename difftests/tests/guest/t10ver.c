// t10: versiones de simbolos (como glibc/bionic): t10_ver@T10_1 (oculta) y t10_ver@@T10_2 (por defecto).
__asm__(".symver t10_ver_1, t10_ver@T10_1");
__asm__(".symver t10_ver_2, t10_ver@@T10_2");
int t10_ver_1(void) { return 1; }
int t10_ver_2(void) { return 2; }
