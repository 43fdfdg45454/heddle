/* Funciones de long double que en bionic salen de builtins.cpp (intrinsecos del compilador). */
long double fabsl(long double x) { return __builtin_fabsl(x); }
long double copysignl(long double x, long double y) { return __builtin_copysignl(x, y); }
int __signbitl(long double x) { return __builtin_signbitl(x); }
/* Igual que bionic: FreeBSD no tiene tgammal de 128 bits; se calcula en double. */
double tgamma(double);
long double tgammal(long double x) { return tgamma(x); }
