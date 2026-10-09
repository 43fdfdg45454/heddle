// t10: libt10o_c (nivel 2): su propia referencia a t10_deep se resuelve con el grupo de la raiz (la de libt10o_b).
int t10_deep(void) { return 3; }
int t10_only_c(void) { return 3; }
int t10_c_deep(void) { return t10_deep(); }
