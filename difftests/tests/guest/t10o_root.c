// t10: raiz del grupo local libt10o_root -> (libt10o_a -> libt10o_c), libt10o_b (orden de busqueda de bionic).
int t10_deep(void);
// interposicion: la raiz va primero en su grupo local, tambien para las referencias de sus dependencias
int t10_who(void) { return 0; }
int t10_root_deep(void) { return t10_deep(); }
