// t8: la otra mitad del ciclo libt8a.so <-> libt8b.so.
extern void t8_note(int);
extern int a_value(void);
int b_value(void) { return 2; }
int b_calls_a(void) { return a_value() * 10 + b_value(); }
__attribute__((constructor)) static void b_ctor(void) { t8_note(2); }
__attribute__((destructor)) static void b_dtor(void) { t8_note(-2); }
