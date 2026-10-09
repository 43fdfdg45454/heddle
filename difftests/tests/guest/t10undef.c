// t10: referencia a un simbolo que no existe en ninguna biblioteca: el dlopen falla como en bionic
// ("cannot locate symbol ... referenced by ...").
extern int t10_no_existe_en_ninguna_parte(void);
int t10undef(void) { return t10_no_existe_en_ninguna_parte(); }
