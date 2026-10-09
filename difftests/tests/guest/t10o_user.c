// t10: usa t10_df1_sym (de la biblioteca DF_1_GLOBAL, sin DT_NEEDED) y t10_only_c (de otro grupo, invisible: debil).
int t10_df1_sym(void);
__attribute__((weak)) int t10_only_c(void);
int t10_user_df1(void) { return t10_df1_sym(); }
int t10_user_has_c(void) { return t10_only_c != 0; }
