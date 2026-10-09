// t10: biblioteca con relocaciones de texto (DT_TEXTREL): bionic en LP64 no la carga.
__asm__(".text\n.globl t10textrel_ptr\nt10textrel_ptr:\n.quad t10textrel_ptr\n");
int t10textrel_id(void) { return 1; }
