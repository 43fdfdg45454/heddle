// t10: protecciones de los segmentos como bionic (texto y solo lectura sin escritura, RELRO protegido tras relocar,
// datos escribibles) y extension de segmentos con la nota NT_ANDROID_TYPE_PAD_SEGMENT (se compila tambien como
// libt10pad.so con -DPAD y p_align de 16 KiB).
#ifdef PAD
__asm__(".section .note.android.pad_segment,\"a\",%note\n"
        ".balign 4\n.long 8\n.long 4\n.long 4\n.asciz \"Android\"\n.balign 4\n.long 1\n.balign 4\n.text\n");
#endif
const char seg_ro[64] = "solo lectura";
static int v = 7;
int *const seg_relro = &v;   // RELRO relocado
int seg_rw = 5;
const char *seg_ro_addr(void) { return seg_ro; }
int *const *seg_relro_addr(void) { return &seg_relro; }
int *seg_rw_addr(void) { return &seg_rw; }
void *seg_code_addr(void) { return (void *)seg_ro_addr; }
