double faddloop(long n) { double s = 0, x = 1.5; for (; n; n--) __asm__ volatile("fadd %d0,%d0,%d1" : "+w"(s) : "w"(x)); return s; }
double fdivloop(long n) { double s = 1.0e300, x = 1.0000001; for (; n; n--) __asm__ volatile("fdiv %d0,%d0,%d1" : "+w"(s) : "w"(x)); return s; }
long intloop(long n) { long s = 0; for (; n; n--) __asm__ volatile("add %0,%0,#3" : "+r"(s)); return s; }
double cvtloop(long n) { double s = 0; for (long i = 0; i < n; i++) __asm__ volatile("scvtf %d0,%1" : "=w"(s) : "r"(i)); return s; }
double fdivexact(long n) { double s = 1.0e300, x = 1.0; for (; n; n--) __asm__ volatile("fdiv %d0,%d0,%d1" : "+w"(s) : "w"(x)); return s; }
double fmulloop(long n) { double s = 1.0, x = 1.0000001; for (; n; n--) __asm__ volatile("fmul %d0,%d0,%d1" : "+w"(s) : "w"(x)); return s; }
double fmaddloop(long n) { double s = 1.0, x = 1.0000001, y = 0.5; for (; n; n--) __asm__ volatile("fmadd %d0,%d0,%d1,%d2" : "+w"(s) : "w"(x), "w"(y)); return s; }
long vaddloop(long n) { __asm__ volatile("movi v0.4s,#1\n movi v1.4s,#2" ::: "v0","v1"); for (; n; n--) __asm__ volatile("add v0.4s,v0.4s,v1.4s\n eor v1.16b,v1.16b,v0.16b" ::: "v0","v1"); return 0; }
long vfmlaloop(long n) { __asm__ volatile("fmov v0.4s,#1.0\n fmov v1.4s,#1.5\n fmov v2.4s,#0.25" ::: "v0","v1","v2"); for (; n; n--) __asm__ volatile("fmla v0.4s,v1.4s,v2.4s\n fmul v1.4s,v1.4s,v2.4s\n fadd v1.4s,v1.4s,v0.4s" ::: "v0","v1","v2"); return 0; }
