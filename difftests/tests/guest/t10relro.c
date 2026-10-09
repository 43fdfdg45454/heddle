// t10: biblioteca con datos RELRO relocados (tabla de punteros constante) para ANDROID_DLEXT_WRITE_RELRO/USE_RELRO.
static int a = 1, b = 2;
static int *const tab[] = { &a, &b };
int relro_get(int i) { return *tab[i]; }
