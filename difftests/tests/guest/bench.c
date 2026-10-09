long fib(long n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
unsigned long lcg(unsigned long n) { unsigned long x = 1; for (unsigned long i = 0; i < n; i++) x = x * 6364136223846793005UL + 1442695040888963407UL ^ (x >> 7); return x; }
double dot(long n) { double s = 0; for (long i = 1; i <= n; i++) s += 1.0 / (double)(i * i); return s; }
