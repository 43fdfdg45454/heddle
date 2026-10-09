// t8: libt8a.so <-> libt8b.so (DT_NEEDED en ciclo; ambas necesitan tambien libt8log.so).
extern void t8_note(int);
extern int b_value(void);
int a_data = 1234;
int a_func(void) { return 77; }
int a_value(void) { return 1; }
int a_calls_b(void) { return b_value() * 10 + a_value(); }
__attribute__((constructor)) static void a_ctor(void) { t8_note(1); }
__attribute__((destructor)) static void a_dtor(void) { t8_note(-1); }
