// long double (binary128) en stdio/stdlib: strtold, wcstold, printf %L*, wprintf %L*, scanf %L* y wscanf %L*.
// Cada caso produce una linea; se comparan con tld_ref.h, generado con la bionic real de arm64:
//   difftests/tools/tld_ref.sh  (compila este archivo con -DTLD_REF estatico contra el libc.a del NDK y lo ejecuta
//   con qemu-aarch64)
typedef unsigned long size_t;
typedef __builtin_va_list va_list;
#define va_start(a, b) __builtin_va_start(a, b)
#define va_end(a) __builtin_va_end(a)
typedef __WCHAR_TYPE__ wchar_t_;
typedef struct FILE FILE;
extern int printf(const char *, ...);
extern int snprintf(char *, size_t, const char *, ...);
extern int vsnprintf(char *, size_t, const char *, va_list);
extern int asprintf(char **, const char *, ...);
extern int fprintf(FILE *, const char *, ...);
extern int sscanf(const char *, const char *, ...);
extern int vsscanf(const char *, const char *, va_list);
extern int fscanf(FILE *, const char *, ...);
extern int vfscanf(FILE *, const char *, va_list);
extern int swprintf(wchar_t_ *, size_t, const wchar_t_ *, ...);
extern int vswprintf(wchar_t_ *, size_t, const wchar_t_ *, va_list);
extern int fwprintf(FILE *, const wchar_t_ *, ...);
extern int swscanf(const wchar_t_ *, const wchar_t_ *, ...);
extern int vswscanf(const wchar_t_ *, const wchar_t_ *, va_list);
extern int fwscanf(FILE *, const wchar_t_ *, ...);
extern long double strtold(const char *, char **);
extern long double wcstold(const wchar_t_ *, wchar_t_ **);
extern FILE *fmemopen(void *, size_t, const char *);
extern int fclose(FILE *);
extern FILE *fdopen(int, const char *);
extern int pipe(int *);
extern int close(int);
extern long write(int, const void *, size_t);
extern long read(int, void *, size_t);
extern int fgetc(FILE *);
extern unsigned fgetwc(FILE *);
extern int fflush(FILE *);
extern int strcmp(const char *, const char *);
extern size_t strlen(const char *);
extern void free(void *);
extern int *__errno(void);
#define errno (*__errno())
#define ERANGE 34

// --- salida -----------------------------------------------------------------------------------------------------
#define MAXL 900
static char lines[MAXL][160];
static char *big[48];  // lineas largas (LDBL_MAX con %Lf...)
static int nl, nbig;
static void put(const char *s) {
    if (nl >= MAXL) return;
    if (strlen(s) >= sizeof(lines[0])) {
        if (nbig < 48) asprintf(&big[nbig++], "%s", s);
        snprintf(lines[nl++], sizeof(lines[0]), "<larga %d:%u>", nbig - 1, (unsigned)strlen(s));
        return;
    }
    snprintf(lines[nl++], sizeof(lines[0]), "%s", s);
}
static char tmp[8192];
static unsigned long long hi(long double x) { unsigned long long w[2]; __builtin_memcpy(w, &x, 16); return w[1]; }
static unsigned long long lo(long double x) { unsigned long long w[2]; __builtin_memcpy(w, &x, 16); return w[0]; }
static long double from_bits(unsigned long long h, unsigned long long l) {
    unsigned long long w[2] = {l, h};
    long double x;
    __builtin_memcpy(&x, w, 16);
    return x;
}
static void wide_to(char *d, const wchar_t_ *s, int n) {
    int i = 0;
    for (; s[i] && i < n - 1; i++) d[i] = s[i] < 128 ? (char)s[i] : '?';
    d[i] = 0;
}
static void widen(wchar_t_ *d, const char *s) {
    int i = 0;
    for (; s[i]; i++) d[i] = (unsigned char)s[i];
    d[i] = 0;
}

// --- strtold / wcstold ------------------------------------------------------------------------------------------
static const char *num[] = {
    "0.1", "1", "-0", "+0.0e-99999", "3.14159265358979323846264338327950288419716939937510582097494459",
    "1e4932", "1.18973149535723176508575932662800702e4932", "1.18973149535723176508575932662800703e4932", "1.2e4932",
    "1e999999999", "1e-999999999", "0e999999999",
    // minimo normal y subnormales
    "3.3621031431120935062626778173217526e-4932", "3.3621031431120935062626778173217525e-4932",
    "6.4751751194380251109244389582276465525e-4966", "1e-4966", "4e-4966",
    "3.2375875597190125554622194791138232762497846690173405048449421945985197700620596855088357456383249701279390707384240598382936099431912710233425550359863089915213963553756674672083673128192358701197243e-4966",
    "3.2375875597190125554622194791138232762497846690173405048449421945985197700620596855088357456383249701279390707384240598382936099431912710233425550359863089915213963553756674672083673128192358701197244e-4966",
    // redondeo al medio: 1 + 2^-113 (al par: 1), 1 + 3*2^-113 (al par: hacia arriba) y un poco por encima/debajo
    "1.00000000000000000000000000000000009629649721936179265279889712924636592690508241076940976199693977832794189453125",
    "1.00000000000000000000000000000000009629649721936179265279889712924636592690508241076940976199693977832794189453126",
    "1.00000000000000000000000000000000009629649721936179265279889712924636592690508241076940976199693977832794189453124",
    "1.00000000000000000000000000000000028888949165808537795839669138773909778071524723230822928599081933498382568359375",
    // hexadecimal
    "0x1.0000000000000000000000000001p0", "0x1.00000000000000000000000000008p0", "0x1.00000000000000000000000000018p0",
    "0x1.000000000000000000000000000080000001p0", "0x1p-16494", "0x1p-16495", "0x1.8p-16495", "0x1.ffffffffffffffffffffffffffffp16383",
    "0X1P+16384", "0x.8p1", "-0xABCDEF.123p-7", "0x1p-16382", "0x0.ffffffffffffffffffffffffffffp-16382",
    // inf y nan
    "inf", "-Infinity", "infinit", "+INF", "nan", "-nan", "nan(0x123)", "nan(123)", "NAN(abc_9)", "nan(", "nan()",
    "nan(0xffffffffffffffffffffffffffffffff)",
    // invalidos y finales
    "", "  ", "x", ".", "e5", "0x", "0x.p1", "-.e1", "1e", "1e+", "1.5e+x", "  \t\n+12.5e+3xyz", "1.", ".5", "00012",
    "0x1p", "0x1p+", "1_000",
    // muchas cifras
    "123456789012345678901234567890123456789012345678901234567890123456789012345678901234567890.123456789e-50",
    "0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001",
};

static void t_strtold(void) {
    for (unsigned i = 0; i < sizeof num / sizeof *num; i++) {
        char *e;
        errno = 0;
        long double x = strtold(num[i], &e);
        int er = errno == ERANGE;
        snprintf(tmp, sizeof tmp, "strtold %u %016llx%016llx %d %d", i, hi(x), lo(x), (int)(e - num[i]), er);
        put(tmp);
        wchar_t_ w[400], *we;
        widen(w, num[i]);
        errno = 0;
        x = wcstold(w, &we);
        er = errno == ERANGE;
        snprintf(tmp, sizeof tmp, "wcstold %u %016llx%016llx %d %d", i, hi(x), lo(x), (int)(we - w), er);
        put(tmp);
    }
}

// --- printf -----------------------------------------------------------------------------------------------------
static const char *ffmt[] = {
    "%Lf", "%.0Lf", "%.40Lf", "%Le", "%.35Le", "%.0Le", "%#.0Le", "%Lg", "%.36Lg", "%#Lg", "%La", "%LA", "%.0La",
    "%.3La", "%+Le", "% Lf", "[%-40.10Le]", "[%040.10Le]", "[%+040La]", "%'Lf", "%LF", "%LE", "%LG", "%.50Lg", "%#.3Lg",
    "[%10Lf]", "%.2Lf", "%.112La",
};

static void t_printf(void) {
    long double v[] = {
        0.1L, 1.0L / 3, 3.14159265358979323846264338327950288L, 0.5L, 2.5L, 1.5L, 0.125L, -0.0L, 1e4000L, 1e-4000L,
        123456789.987654321L, 9.999999999999999999999999999999999e99L,
        from_bits(0x7ffeffffffffffffULL, 0xffffffffffffffffULL),  // LDBL_MAX
        from_bits(0x0001000000000000ULL, 0),                         // LDBL_MIN
        from_bits(0, 1),                                             // LDBL_TRUE_MIN
        from_bits(0x0000800000000000ULL, 0x123),                     // subnormal
        from_bits(0x7fff000000000000ULL, 0),                         // inf
        from_bits(0xffff000000000000ULL, 0),                         // -inf
        from_bits(0x7fff800000000000ULL, 0),                         // nan
        from_bits(0xffff800000000000ULL, 0x55),                      // -nan con carga
    };
    for (unsigned i = 0; i < sizeof v / sizeof *v; i++)
        for (unsigned j = 0; j < sizeof ffmt / sizeof *ffmt; j++) {
            char b[6000];
            int n = snprintf(b, sizeof b, ffmt[j], v[i]);
            snprintf(tmp, sizeof tmp, "printf %u %u %d %s", i, j, n, b);
            put(tmp);
        }
    // LDBL_MAX entero (4933 cifras)
    {
        static char b[6000];
        int n = snprintf(b, sizeof b, "%.0Lf", v[12]);
        snprintf(tmp, sizeof tmp, "printf max %d ", n);
        put(tmp);
        put(b);
    }
    // mezcla con otros argumentos y con mas de 8 argumentos de coma flotante (los ultimos van en la pila)
    {
        char b[600];
        int n = snprintf(b, sizeof b, "%d|%Lf|%s|%f|%Le|%c|%*.*Lf|%lld|%La", 7, 0.1L, "hola", 2.5, -1e300L, 'x', 12, 3, 1.0L / 7, -5LL,
                         v[15]);
        snprintf(tmp, sizeof tmp, "mezcla %d %s", n, b);
        put(tmp);
        n = snprintf(b, sizeof b, "%Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %g %Lg %d", 1.0L, 2.0L, 3.0L, 4.0L, 5.0L, 6.0L, 7.0L,
                     8.0L, 9.0L, 10.0, 11.0L, 12);
        snprintf(tmp, sizeof tmp, "pila %d %s", n, b);
        put(tmp);
        static volatile size_t ocho = 8;
        n = snprintf(b, ocho, "%Lf", 0.1L);  // truncado
        snprintf(tmp, sizeof tmp, "trunc %d %s", n, b);
        put(tmp);
        char *a = 0;
        n = asprintf(&a, "<%.20Le>", 2.0L / 3);
        snprintf(tmp, sizeof tmp, "asprintf %d %s", n, a ? a : "(nulo)");
        put(tmp);
        free(a);
        n = snprintf(b, sizeof b, "%.5Lf|%d|%5s|%Lf", 1.0L / 9, 3, "ab", 1.0L);
        snprintf(tmp, sizeof tmp, "varios %d %s", n, b);
        put(tmp);
    }
}

static int vs(char *b, size_t n, const char *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vsnprintf(b, n, f, ap);
    va_end(ap);
    return r;
}
static int vsw(wchar_t_ *b, size_t n, const wchar_t_ *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vswprintf(b, n, f, ap);
    va_end(ap);
    return r;
}

static void t_vprintf(void) {
    char b[600];
    int n = vs(b, sizeof b, "%Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %g %La %s %Le", 1.0L, 2.0L, 3.0L, 4.0L, 5.0L, 6.0L, 7.0L, 8.0L,
               9.0L, 10.0, 0.1L, "fin", 1e-4950L);
    snprintf(tmp, sizeof tmp, "vsnprintf %d %s", n, b);
    put(tmp);
    // fprintf a un FILE en memoria
    static char mb[256];
    FILE *f = fmemopen(mb, sizeof mb, "w");
    if (f) {
        n = fprintf(f, "[%Lf|%5.1Le|%d]", 1.0L / 3, 12345.678L, 9);
        fflush(f);
        snprintf(tmp, sizeof tmp, "fprintf %d %s", n, mb);
        put(tmp);
        fclose(f);
    }
}

static void t_wprintf(void) {
    wchar_t_ w[600];
    char b[600];
    int n = swprintf(w, 600, L"%Lf|%ls|%d|%La|%s|%f|%c", 0.1L, L"ancho", 42, 1.0L / 3, "estrecho", 0.25, 'z');
    wide_to(b, w, sizeof b);
    snprintf(tmp, sizeof tmp, "swprintf %d %s", n, b);
    put(tmp);
    n = swprintf(w, 600, L"%-30.20Le|%+.3LA|%#Lg", 2.0L / 3, from_bits(0xffff800000000000ULL, 0), 100.0L);
    wide_to(b, w, sizeof b);
    snprintf(tmp, sizeof tmp, "swprintf %d %s", n, b);
    put(tmp);
    w[0] = 7;
    n = swprintf(w, 5, L"%Lf", 0.1L);  // no cabe: -1
    wide_to(b, w, sizeof b);
    snprintf(tmp, sizeof tmp, "swprintf corto %d %s", n, b);
    put(tmp);
    n = vsw(w, 600, L"%Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %Lg %g %Le|%d", 1.0L, 2.0L, 3.0L, 4.0L, 5.0L, 6.0L, 7.0L, 8.0L, 9.0L,
            10.0, 1e4000L, -3);
    wide_to(b, w, sizeof b);
    snprintf(tmp, sizeof tmp, "vswprintf %d %s", n, b);
    put(tmp);
    n = vsw(w, 600, L"sin L: %d %s %5.2f", 5, "x", 1.5);
    wide_to(b, w, sizeof b);
    snprintf(tmp, sizeof tmp, "vswprintf %d %s", n, b);
    put(tmp);
    static char mb[256];
    int pf[2];
    FILE *f = pipe(pf) ? 0 : fdopen(pf[1], "w");
    if (f) {
        n = fwprintf(f, L"<%Lf %d>", 1.0L / 8, 3);
        fclose(f);
        long k = read(pf[0], mb, sizeof mb - 1);
        mb[k > 0 ? k : 0] = 0;
        close(pf[0]);
        snprintf(tmp, sizeof tmp, "fwprintf %d %s", n, mb);
        put(tmp);
    }
}

// --- scanf ------------------------------------------------------------------------------------------------------
static void scan1(const char *in, const char *fmt) {
    long double a = -7, b = -7;
    int n1 = -1, n2 = -1, d = -1;
    char ch = '?';
    int r;
    // formatos con hasta dos long double, un int, un char y dos %n en orden fijo segun el caso
    if (!strcmp(fmt, "%La,%d") || !strcmp(fmt, "%Lf %d"))
        r = sscanf(in, fmt, &a, &d);
    else if (!strcmp(fmt, "%d %Lf") || !strcmp(fmt, "%d%Lf"))
        r = sscanf(in, fmt, &d, &a);
    else if (!strcmp(fmt, "%d%Lf%d%n"))
        r = sscanf(in, fmt, &d, &a, &n1, &n2);
    else if (!strcmp(fmt, "%LG%c"))
        r = sscanf(in, fmt, &a, &ch);
    else if (!strcmp(fmt, "%Lf%n") || !strcmp(fmt, "%n%Lf"))
        r = !strcmp(fmt, "%Lf%n") ? sscanf(in, fmt, &a, &n1) : sscanf(in, fmt, &n1, &a);
    else if (!strcmp(fmt, "%Lf %n%Lf%n") || !strcmp(fmt, "%Lg %n %Le%n"))
        r = sscanf(in, fmt, &a, &n1, &b, &n2);
    else if (!strcmp(fmt, "%*Lf %Lf") || !strcmp(fmt, "x%Lf") || !strcmp(fmt, "%Lf") || !strcmp(fmt, "%5Lf") ||
             !strcmp(fmt, "%LA") || !strcmp(fmt, " %Lf"))
        r = sscanf(in, fmt, &a);
    else
        r = sscanf(in, fmt, &a, &b);
    snprintf(tmp, sizeof tmp, "sscanf [%s] [%s] %d %016llx%016llx %016llx%016llx %d %d %d %c", in, fmt, r, hi(a), lo(a), hi(b),
             lo(b), n1, n2, d, ch);
    put(tmp);
}

static void t_scanf(void) {
    static const char *c[][2] = {
        {"3.25", "%Lf"}, {"  -1e-4950 rest", "%Lf%n"}, {"0x1.8p3,7", "%La,%d"}, {"12345678", "%3Lf%Lf"},
        {"nan(abc) inf", "%Lg %Le"}, {"1e+x", "%Lf%n"}, {"", "%Lf"}, {"   ", "%Lf"}, {"abc", "%Lf"}, {"5 x", "%d %Lf"},
        {"5", "%d %Lf"}, {"1.5 2.5", "%*Lf %Lf"}, {"1 2.5 3", "%d%Lf%d%n"}, {"x1.5", "x%Lf"}, {"y1.5", "x%Lf"},
        {"0X1P-16494", "%LA"}, {"-.5e-1z", "%LG%c"}, {"1.0000000000000000000000000000000000962964972193617926527988971292463659269", "%Lf"},
        {" 12  34.5", "%Lf %n%Lf%n"}, {"7 \t 8", "%Lg %n %Le%n"}, {"123456", "%5Lf"}, {"infinity", "%Lf"},
        {"-INFx", "%Lf"}, {"nan(", "%Lf"}, {"0x", "%Lf"}, {"1e", "%Lf%n"}, {"9", "%n%Lf"}, {"2.5 3", "%Lf %d"},
        {"4 5.5", "%d%Lf"}, {"\n 6.25", " %Lf"}, {"1.25 2", "%Lf%Lf"}, {"0.1 0.2", "%Lf,%Lf"},
    };
    for (unsigned i = 0; i < sizeof c / sizeof *c; i++) scan1(c[i][0], c[i][1]);
    // 600 cifras: la anchura se limita a 512 caracteres
    {
        static char in[700];
        for (int i = 0; i < 600; i++) in[i] = '1' + i % 9;
        in[600] = 0;
        long double a = 0, b = 0;
        int r = sscanf(in, "%Lf%Lf", &a, &b);
        snprintf(tmp, sizeof tmp, "sscanf 600 %d %016llx%016llx %016llx%016llx", r, hi(a), lo(a), hi(b), lo(b));
        put(tmp);
    }
    // FILE: lo que parsefloat devuelve a la entrada se vuelve a leer
    static const char *fc[] = {"1e+x", "  2.5e-3 9", "nan(12)", "0x1.ffp+zz", "-", "inf"};
    for (unsigned i = 0; i < sizeof fc / sizeof *fc; i++) {
        static char mb[64];
        __builtin_memcpy(mb, fc[i], strlen(fc[i]) + 1);
        FILE *f = fmemopen(mb, strlen(fc[i]), "r");
        if (!f) continue;
        long double a = -7;
        int d = -1;
        int r = fscanf(f, "%Lf%d", &a, &d);
        int rest[4];
        for (int k = 0; k < 4; k++) rest[k] = fgetc(f);
        snprintf(tmp, sizeof tmp, "fscanf %u %d %016llx%016llx %d %d %d %d %d", i, r, hi(a), lo(a), d, rest[0], rest[1], rest[2],
                 rest[3]);
        put(tmp);
        fclose(f);
    }
}

static int vss(const char *s, const char *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vsscanf(s, f, ap);
    va_end(ap);
    return r;
}
static int vsws(const wchar_t_ *s, const wchar_t_ *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vswscanf(s, f, ap);
    va_end(ap);
    return r;
}

static void t_vscanf(void) {
    long double a = 0, b = 0;
    int d = 0, n = 0;
    int r = vss("8 1.75e2 0x1p-1", "%d %Lf%n %La", &d, &a, &n, &b);
    snprintf(tmp, sizeof tmp, "vsscanf %d %d %d %016llx%016llx %016llx%016llx", r, d, n, hi(a), lo(a), hi(b), lo(b));
    put(tmp);
    a = b = 0;
    d = n = 0;
    r = swscanf(L"  3.5  x  -0x1.8p1", L"%Lf x %n%La", &a, &n, &b);
    snprintf(tmp, sizeof tmp, "swscanf %d %d %016llx%016llx %016llx%016llx", r, n, hi(a), lo(a), hi(b), lo(b));
    put(tmp);
    a = b = 0;
    r = swscanf(L"", L"%Lf", &a);
    snprintf(tmp, sizeof tmp, "swscanf vacio %d", r);
    put(tmp);
    r = swscanf(L"abc", L"%Lf", &a);
    snprintf(tmp, sizeof tmp, "swscanf abc %d", r);
    put(tmp);
    r = swscanf(L"5", L"%d %Lf", &d, &a);
    snprintf(tmp, sizeof tmp, "swscanf corto %d %d", r, d);
    put(tmp);
    a = b = 0;
    r = vsws(L"1e-4966 7 inf", L"%Lf %d %Lg", &a, &d, &b);
    snprintf(tmp, sizeof tmp, "vswscanf %d %d %016llx%016llx %016llx%016llx", r, d, hi(a), lo(a), hi(b), lo(b));
    put(tmp);
    a = 0;
    r = vsws(L"12 13", L"%d %d", &d, &n);
    snprintf(tmp, sizeof tmp, "vswscanf sin L %d %d %d", r, d, n);
    put(tmp);
    // FILE ancho: una tuberia (el fmemopen de glibc no admite E/S ancha)
    int pf[2];
    FILE *f = pipe(pf) ? 0 : (write(pf[1], "  0.75e1rest", 12), close(pf[1]), fdopen(pf[0], "r"));
    if (f) {
        a = 0;
        r = fwscanf(f, L"%Lf", &a);
        snprintf(tmp, sizeof tmp, "fwscanf %d %016llx%016llx %c", r, hi(a), lo(a), (char)fgetwc(f));
        put(tmp);
        fclose(f);
    }
}

#ifdef TLD_REF
// Referencia: imprime las lineas como tld_ref.h
int main(void) {
    t_strtold();
    t_printf();
    t_vprintf();
    t_wprintf();
    t_scanf();
    t_vscanf();
    printf("// generado por difftests/tools/tld_ref.sh con la bionic de arm64 del NDK (no editar)\n");
    printf("static const char *const REF[] = {\n");
    for (int i = 0; i < nl; i++) {
        printf("    \"");
        for (const char *p = lines[i]; *p; p++) {
            if (*p == '"' || *p == '\\') printf("\\");
            if (*p == '\n') printf("\\n");
            else if (*p == '\t') printf("\\t");
            else printf("%c", *p);
        }
        printf("\",\n");
    }
    printf("};\nstatic const char *const REF_BIG[] = {\n");
    for (int i = 0; i < nbig; i++) printf("    \"%s\",\n", big[i]);
    printf("};\n");
    return 0;
}
#else
#include "tld_ref.h"
extern char *getenv(const char *);
int run_all(void) {
    int fails = 0;
    // sin biblioteca guest incrustada, heddle aproxima en double: la prueba no aplica (salvo HEDDLE_REQUIRE_GUEST)
    if (lo(strtold("0.1", 0)) != 0x999999999999999aULL) {
        int req = getenv("HEDDLE_REQUIRE_GUEST") != 0;
        printf("tld run_all: %d fallos (sin biblioteca guest: long double aproximado)\n", req);
        return req;
    }
    t_strtold();
    t_printf();
    t_vprintf();
    t_wprintf();
    t_scanf();
    t_vscanf();
    int nref = sizeof REF / sizeof *REF;
    if (nref != nl) {
        printf("FALLO: %d lineas, se esperaban %d\n", nl, nref);
        fails++;
    }
    for (int i = 0; i < nl && i < nref; i++)
        if (strcmp(lines[i], REF[i])) {
            printf("FALLO\n  obtenido: %s\n  esperado: %s\n", lines[i], REF[i]);
            fails++;
        }
    for (int i = 0; i < nbig; i++)
        if (i >= (int)(sizeof REF_BIG / sizeof *REF_BIG) || strcmp(big[i], REF_BIG[i])) {
            printf("FALLO linea larga %d\n", i);
            fails++;
        }
    printf("tld run_all: %d fallos\n", fails);
    return fails;
}
#endif

// --- %n en la familia printf: bionic aborta con "FORTIFY: %n not allowed on Android" (SIGABRT). run-all.sh
// ejecuta cada una aparte y comprueba la senal y el mensaje en stderr. (Fuera de -DTLD_REF: con qemu y el libc.a del
// NDK se comprueba igual llamandolas desde main.)
#pragma clang diagnostic ignored "-Wformat"
extern int __snprintf_chk(char *, size_t, int, size_t, const char *, ...);
extern int wprintf(const wchar_t_ *, ...);
static int nn;
static int vsn(char *b, size_t n, const char *f, ...) {
    va_list ap;
    va_start(ap, f);
    int r = vsnprintf(b, n, f, ap);
    va_end(ap);
    return r;
}
int n_printf(void) { return printf("a%nb\n", &nn); }
int n_snprintf(void) { char b[16]; return snprintf(b, sizeof b, "%d%n", 1, &nn); }
int n_snprintf_ld(void) { char b[64]; return snprintf(b, sizeof b, "%Lf%hhn", 1.5L, &nn); }
int n_vsnprintf(void) { char b[16]; return vsn(b, sizeof b, "%5ln", &nn); }
int n_snprintf_chk(void) { char b[16]; return __snprintf_chk(b, sizeof b, 0, sizeof b, "x%n", &nn); }
int n_swprintf(void) { wchar_t_ b[16]; return swprintf(b, 16, L"%d%n", 1, &nn); }
int n_swprintf_ld(void) { wchar_t_ b[64]; return swprintf(b, 64, L"%Lf%n", 1.5L, &nn); }
int n_wprintf(void) { return wprintf(L"a%n\n", &nn); }
// sin %n no aborta (%%n es un literal)
int n_ninguno(void) { char b[16]; return snprintf(b, sizeof b, "%%n%d", 7) == 3 ? 5 : 6; }
