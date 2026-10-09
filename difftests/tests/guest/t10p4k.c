// t10: biblioteca con p_align de 4 KiB (-z max-page-size=4096) para el modo de compatibilidad de 16 KiB de bionic.
static int a = 10, b = 20;
static int *const tab[] = { &a, &b };   // RELRO relocado
static int counter;                      // bss
int p4k_get(int i) { return *tab[i]; }
int p4k_bump(void) { return ++counter; }
// codigo maquina en .rodata (mov w0, #42; ret): en el modo de compatibilidad de 16 KiB toda la region es RX
const unsigned int p4k_ro_code[2] __attribute__((aligned(8))) = { 0x52800540, 0xd65f03c0 };
