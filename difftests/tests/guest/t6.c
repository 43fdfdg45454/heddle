// Senales: mascaras como libsigchain + bionic en ARM y entrega de senales asincronas.
typedef unsigned long size_t;
typedef int pid_t;
extern int printf(const char *, ...);
extern int sigaction(int, const void *, void *);
extern int pthread_sigmask(int, const unsigned long *, unsigned long *);
extern int sigprocmask(int, const unsigned long *, unsigned long *);
extern int sigpending(unsigned long *);
extern int sigsetjmp(long *, int) __attribute__((returns_twice));
extern void siglongjmp(long *, int) __attribute__((noreturn));
extern int raise(int);
extern pid_t fork(void);
extern pid_t waitpid(pid_t, int *, int);
extern void _exit(int) __attribute__((noreturn));
extern int __libc_current_sigrtmin(void);

typedef struct { int flags; void *h; unsigned long mask; void *restorer; } gsa; // struct sigaction de bionic LP64

static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
#define B(s) (1ul << ((s) - 1))

static long sc4(long n, long a, long b, long c, long d) {
    register long x8 __asm__("x8") = n;
    register long x0 __asm__("x0") = a;
    register long x1 __asm__("x1") = b;
    register long x2 __asm__("x2") = c;
    register long x3 __asm__("x3") = d;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8), "r"(x1), "r"(x2), "r"(x3) : "memory");
    return x0;
}
static long rt_sigprocmask(int how, const unsigned long *s, unsigned long *o) { return sc4(135, how, (long)s, (long)o, 8); }
static unsigned long mascara(void) {
    unsigned long m = 0;
    pthread_sigmask(0, 0, &m);
    return m;
}

static volatile int vistos;
static volatile unsigned long uc_mask;
static void h_cuenta(int s, void *si, void *uc) {
    vistos++;
    uc_mask = *(unsigned long *)((char *)uc + 40); // uc_sigmask (aarch64)
}
static void h_no_debe(int s, void *si, void *uc) { _exit(1); }

static void mascaras(void) {
    unsigned long old = 0, pend = 0;
    CHECK(pthread_sigmask(2, 0, &old) == 0);
    // libsigchain quita SEGV y BUS de SIG_BLOCK; USR2 si se bloquea; la consulta es la mascara real
    unsigned long pide = B(11) | B(7) | B(12);
    CHECK(pthread_sigmask(0, &pide, 0) == 0);
    CHECK((mascara() & pide) == B(12));
    CHECK(sigprocmask(2, &pide, 0) == 0);
    CHECK((mascara() & pide) == B(12));
    // SIG_UNBLOCK no se filtra; `how` invalido con conjunto: EINVAL; sin conjunto no se comprueba
    CHECK(pthread_sigmask(1, &pide, 0) == 0 && (mascara() & pide) == 0);
    CHECK(pthread_sigmask(7, &pide, 0) == 22);
    CHECK(pthread_sigmask(7, 0, &pend) == 0);
    // uc_sigmask: lo bloqueado en el punto interrumpido (sin la propia senal)
    gsa sa = {4 /* SA_SIGINFO */, (void *)h_cuenta, 0, 0}, viejo;
    sigaction(10, &sa, &viejo);
    unsigned long u2 = B(12);
    pthread_sigmask(0, &u2, 0);
    vistos = 0;
    raise(10);
    CHECK(vistos == 1 && (uc_mask & B(12)) && !(uc_mask & B(10)));
    // SIGILL si se puede bloquear: queda pendiente y llega una vez al desbloquear
    sigaction(4, &sa, 0);
    unsigned long ill = B(4);
    vistos = 0;
    pthread_sigmask(0, &ill, 0);
    raise(4);
    sigpending(&pend);
    CHECK(vistos == 0 && (pend & ill));
    pthread_sigmask(1, &ill, 0);
    CHECK(vistos == 1);
    sigaction(4, &viejo, 0);
    sigaction(10, &viejo, 0);
    // la llamada directa al sistema no se filtra: SEGV queda bloqueada y la consulta lo dice
    unsigned long segv = B(11);
    CHECK(rt_sigprocmask(0, &segv, 0) == 0);
    CHECK((mascara() & segv) == segv);
    CHECK(rt_sigprocmask(7, 0, &pend) == 0 && rt_sigprocmask(7, &segv, 0) == -22);
    // sigsetjmp guarda la mascara real; siglongjmp la repone con sigprocmask64, que en el dispositivo pasa por
    // libsigchain: SEGV no vuelve a quedar bloqueada (USR2 si)
    static long jb[33];
    unsigned long nada = 0;
    pthread_sigmask(0, &u2, 0);
    if (sigsetjmp(jb, 1) == 0) {
        pthread_sigmask(2, &nada, 0);
        CHECK((mascara() & (segv | u2)) == 0);
        siglongjmp(jb, 1);
    }
    CHECK((mascara() & (segv | u2)) == u2);
    CHECK(rt_sigprocmask(0, &segv, 0) == 0);
    // un fallo con SEGV bloqueada termina el proceso con SIGSEGV, sin llegar al manejador (force_sig del kernel)
    pid_t p = fork();
    if (p == 0) {
        gsa sb = {4, (void *)h_no_debe, 0, 0};
        sigaction(11, &sb, 0);
        *(volatile int *)0x10 = 1;
        _exit(2);
    }
    int st = 0;
    CHECK(p > 0 && waitpid(p, &st, 0) == p);
    CHECK((st & 0x7f) == 11);
    CHECK(pthread_sigmask(2, &old, 0) == 0 && (mascara() & (segv | B(12))) == 0);
    CHECK(__libc_current_sigrtmin() >= 34 && __libc_current_sigrtmin() <= 64);
}

// ---- senales asincronas: entrega en frontera de instruccion, ucontext escribible, reinicio de llamadas ----
extern int pthread_create(unsigned long *, const void *, void *(*)(void *), void *);
extern int pthread_join(unsigned long, void **);
extern int pthread_kill(unsigned long, int);
extern unsigned long pthread_self(void);
extern int usleep(unsigned int);
extern int pipe2(int *, int);
extern long write(int, const void *, size_t);
extern int close(int);

// bucles sin llamadas (el codigo traducido solo sale en el salto hacia atras)
__asm__(".text\n"
        ".globl lazo_ldp\n lazo_ldp:\n"      // x0 = par de valores, x1 = &bandera
        "  mov x2, #0\n  mov x3, #1\n"
        ".globl lazo_ldp_ini\nlazo_ldp_ini:\n"
        "  ldp x2, x3, [x0]\n  ldr w4, [x1]\n  cbz w4, lazo_ldp_ini\n"
        ".globl lazo_ldp_fin\nlazo_ldp_fin:\n"
        "  ret\n"
        ".globl lazo_inf\n lazo_inf:\n"      // nunca termina: solo un manejador que cambie el pc lo saca
        "  mov x10, #0\n"
        "1: add x9, x9, #1\n  b 1b\n"
        ".globl lazo_salida\nlazo_salida:\n"
        "  mov x0, x10\n  ret\n"
        ".globl lazo_svc\n lazo_svc:\n"      // read(x0, x1, 1) con svc directo; devuelve x0
        "  mov x2, #1\n  mov x8, #63\n"
        ".globl lazo_svc_insn\nlazo_svc_insn:\n"
        "  svc #0\n  ret\n");
extern void lazo_ldp(const unsigned long *, volatile int *);
extern char lazo_ldp_ini[], lazo_ldp_fin[], lazo_salida[], lazo_svc_insn[];
extern long lazo_inf(void);
extern long lazo_svc(long fd, void *buf);

#define UC_X(uc, i) (*(unsigned long *)((char *)(uc) + 184 + 8 * (i)))
#define UC_PC(uc) (*(unsigned long *)((char *)(uc) + 440))
#define UC_MASK(uc) (*(unsigned long *)((char *)(uc) + 40))
#define MAGIC 0x5a5a1234deadbeeful

static volatile int bandera, anid, n_h;
static volatile unsigned long pc_visto, x2_visto, x3_visto;
static unsigned long hilo_main;
static const unsigned long par[2] = {MAGIC, MAGIC};

struct envio { int sig; int veces; int espera_ms; int fd; };
static void *enviar(void *p) {
    struct envio *e = p;
    usleep(e->espera_ms * 1000);
    for (int i = 0; i < e->veces; i++) {
        pthread_kill(hilo_main, e->sig);
        usleep(2000);
    }
    if (e->fd >= 0) {
        usleep(e->espera_ms * 1000);
        write(e->fd, "x", 1);
    }
    return 0;
}
// vigilante: si la senal no llega, el bucle no termina nunca
static volatile int listo;
static void *vigilar(void *p) {
    for (int i = 0; i < 500 && !listo; i++) usleep(10000);
    if (!listo) { printf("FALLO: la senal asincrona no llego (%s)\n", (const char *)p); _exit(3); }
    return 0;
}

static void h_ldp(int s, void *si, void *uc) {
    n_h++;
    pc_visto = UC_PC(uc);
    x2_visto = UC_X(uc, 2);
    x3_visto = UC_X(uc, 3);
    bandera = 1;
}
static void h_salir(int s, void *si, void *uc) {
    n_h++;
    UC_PC(uc) = (unsigned long)lazo_salida;
    UC_X(uc, 10) = 1234;
    UC_MASK(uc) |= B(12); // la mascara al volver tambien se aplica
}
static void h_anidada(int s, void *si, void *uc) { anid++; }
static void h_espera(int s, void *si, void *uc) {
    n_h++;
    while (!anid) {} // bucle del manejador: la senal anidada llega igual
}
static long salto2[33];
static void h_longjmp(int s, void *si, void *uc) { n_h++; siglongjmp(salto2, 7); }
static void h_svc(int s, void *si, void *uc) { n_h++; pc_visto = UC_PC(uc); x2_visto = UC_X(uc, 0); }

static void con_envio(struct envio *e, const char *que, unsigned long *t, unsigned long *v) {
    listo = 0;
    pthread_create(v, 0, vigilar, (void *)que);
    pthread_create(t, 0, enviar, e);
}
static void fin_envio(unsigned long t, unsigned long v) {
    listo = 1;
    pthread_join(t, 0);
    pthread_join(v, 0);
}

static void asincronas(void) {
    hilo_main = pthread_self();
    unsigned long t, v, old = 0;
    pthread_sigmask(2, 0, &old);
    gsa sa = {4 /* SA_SIGINFO */, (void *)h_ldp, 0, 0}, viejo;
    sigaction(10, &sa, &viejo);

    // 1) bucle con LDP: pc exacto dentro del bucle y nunca un LDP a medias
    struct envio e1 = {10, 1, 20, -1};
    bandera = 0; n_h = 0;
    con_envio(&e1, "lazo ldp", &t, &v);
    lazo_ldp(par, &bandera);
    fin_envio(t, v);
    CHECK(n_h == 1);
    CHECK(pc_visto >= (unsigned long)lazo_ldp_ini && pc_visto < (unsigned long)lazo_ldp_fin);
    CHECK((x2_visto == MAGIC && x3_visto == MAGIC) || (x2_visto == 0 && x3_visto == 1));

    // 2) el manejador cambia pc, x10 y uc_sigmask de una senal asincrona: se aplican
    sa.h = (void *)h_salir;
    sigaction(10, &sa, 0);
    n_h = 0;
    con_envio(&e1, "lazo infinito", &t, &v);
    long r = lazo_inf();
    fin_envio(t, v);
    CHECK(r == 1234 && n_h == 1);
    CHECK((mascara() & B(12)) == B(12));
    pthread_sigmask(2, &old, 0);

    // 3) senal anidada mientras el manejador gira (interprete)
    gsa sb = {4, (void *)h_anidada, 0, 0}, viejo12;
    sigaction(12, &sb, &viejo12);
    sa.h = (void *)h_espera;
    sigaction(10, &sa, 0);
    struct envio e3 = {12, 1, 40, -1};
    anid = 0; n_h = 0;
    con_envio(&e3, "anidada", &t, &v);
    raise(10);
    fin_envio(t, v);
    CHECK(n_h == 1 && anid == 1);
    sigaction(12, &viejo12, 0);

    // 4) siglongjmp desde el manejador de una senal que interrumpio un bucle
    sa.h = (void *)h_longjmp;
    sigaction(10, &sa, 0);
    n_h = 0;
    con_envio(&e1, "siglongjmp", &t, &v);
    int j = sigsetjmp(salto2, 1);
    if (j == 0) lazo_inf();
    fin_envio(t, v);
    CHECK(j == 7 && n_h == 1);

    // 5) svc read bloqueado: con SA_RESTART se reinicia (el manejador ve el pc en el svc); sin el, -EINTR (pc despues)
    int p[2];
    char b = 0;
    CHECK(pipe2(p, 0) == 0);
    sa.h = (void *)h_svc;
    sa.flags = 4 | 0x10000000; // SA_RESTART
    sigaction(10, &sa, 0);
    struct envio e5 = {10, 1, 50, p[1]};
    n_h = 0;
    con_envio(&e5, "svc con SA_RESTART", &t, &v);
    r = lazo_svc(p[0], &b);
    fin_envio(t, v);
    CHECK(r == 1 && b == 'x' && n_h == 1);
    CHECK(pc_visto == (unsigned long)lazo_svc_insn);
    sa.flags = 4;
    sigaction(10, &sa, 0);
    n_h = 0;
    con_envio(&e5, "svc sin SA_RESTART", &t, &v);
    r = lazo_svc(p[0], &b);
    fin_envio(t, v);
    CHECK(r == -4 && n_h == 1);
    CHECK(pc_visto == (unsigned long)lazo_svc_insn + 4 && x2_visto == (unsigned long)-4);
    CHECK(lazo_svc(p[0], &b) == 1); // el byte que escribio el hilo
    close(p[0]);
    close(p[1]);
    sigaction(10, &viejo, 0);
}

// ---- suspension al estilo del GC de Boehm: el manejador se queda en sigsuspend hasta la senal de reanudar ----
extern int sigsuspend(const unsigned long *);
static volatile int suspendidos, reanudado, parar;
static void h_suspender(int s, void *si, void *uc) {
    unsigned long m = 0;
    pthread_sigmask(2, 0, &m);
    m &= ~B(12);
    __atomic_add_fetch(&suspendidos, 1, __ATOMIC_SEQ_CST);
    while (!reanudado) sigsuspend(&m);
    __atomic_sub_fetch(&suspendidos, 1, __ATOMIC_SEQ_CST);
}
static void h_reanudar(int s) {}
static void *gira(void *p) {
    volatile unsigned long n = 0;
    while (!parar) {
        if (p) usleep(100); else n++;
    }
    return 0;
}
static void suspension(void) {
    gsa s1 = {4, (void *)h_suspender, B(12), 0}, s2 = {0, (void *)h_reanudar, 0, 0}, o1, o2;
    sigaction(10, &s1, &o1);
    sigaction(12, &s2, &o2);
    for (int modo = 0; modo < 2; modo++) {
        unsigned long h[3];
        parar = 0;
        for (int i = 0; i < 3; i++) pthread_create(&h[i], 0, gira, modo ? (void *)1 : 0);
        for (int ronda = 0; ronda < 20; ronda++) {
            reanudado = 0;
            for (int i = 0; i < 3; i++) pthread_kill(h[i], 10);
            int k = 0;
            while (suspendidos != 3 && k++ < 2000) usleep(1000);
            CHECK(suspendidos == 3);
            reanudado = 1;
            for (int i = 0; i < 3; i++) pthread_kill(h[i], 12);
            k = 0;
            while (suspendidos != 0 && k++ < 2000) usleep(1000);
            CHECK(suspendidos == 0);
        }
        parar = 1;
        for (int i = 0; i < 3; i++) pthread_join(h[i], 0);
    }
    sigaction(10, &o1, 0);
    sigaction(12, &o2, 0);
}

// Anidamiento profundo. Un manejador con SA_NODEFER que vuelve a lanzar su senal (raise: llega dentro de una HLE)
// hasta 10 veces: hasta 8 niveles corren anidados y el resto se aplaza y se entrega al volver uno. Las 10
// entregas tienen que ocurrir.
static volatile int prof_n, prof_actual, prof_max;
// Con los 8 niveles ocupados, 5 sigqueue de una senal de tiempo real con valores 1..5 se aplazan; cada una tiene
// que llegar con su valor y en orden (no se funden en una).
extern int sigqueue(int, int, long);
extern int getpid(void);
extern int __libc_current_sigrtmin(void);
static volatile int rt_n, rt_vals[8], rt_enviar;
static void h_rt(int s, void *si, void *uc) {
    if (rt_n < 8) rt_vals[rt_n] = *(int *)((char *)si + 24); // si_value (sival_int)
    rt_n++;
}
static void h_profundo(int s, void *si, void *uc) {
    prof_actual++;
    if (prof_actual > prof_max) prof_max = prof_actual;
    if (prof_actual == 8 && rt_enviar) {
        rt_enviar = 0;
        for (int i = 1; i <= 5; i++) sigqueue(getpid(), __libc_current_sigrtmin() + 3, i);
    }
    if (++prof_n < 10) raise(12);
    prof_actual--;
}
static void anidamiento(void) {
    gsa sa = {0}, viejo;
    sa.flags = 4 | 0x40000000; // SA_SIGINFO | SA_NODEFER
    sa.h = (void *)h_profundo;
    sigaction(12, &sa, &viejo);
    gsa sr = {0}, viejo_rt;
    sr.flags = 4;
    sr.h = (void *)h_rt;
    int rt = __libc_current_sigrtmin() + 3;
    sigaction(rt, &sr, &viejo_rt);
    prof_n = prof_actual = prof_max = rt_n = 0;
    rt_enviar = 1;
    raise(12);
    for (volatile int i = 0; i < 1000 && (prof_n < 10 || rt_n < 5); i++) {}
    CHECK(prof_n == 10);
    CHECK(prof_max == 8);
    CHECK(rt_n == 5);
    for (int i = 0; i < 5 && i < rt_n; i++) CHECK(rt_vals[i] == i + 1);
    sigaction(12, &viejo, 0);
    sigaction(rt, &viejo_rt, 0);
}

// Un siglongjmp desde un manejador que interrumpio una llamada bloqueada en el host (read de un pipe vacio): la
// llamada se abandona y la ejecucion sigue en sigsetjmp (read no sigue bloqueado).
extern int pipe(int *);
extern long read(int, void *, unsigned long);
extern unsigned alarm(unsigned);
extern int close(int);
extern int sigsetjmp(long *, int);
extern void siglongjmp(long *, int);
static long salto[64];
static volatile int salto_h;
static void h_salta(int s, void *si, void *uc) {
    salto_h++;
    siglongjmp(salto, 7);
}
static void hle_abandonable(void) {
    int fd[2];
    CHECK(pipe(fd) == 0);
    gsa sa = {0}, viejo;
    sa.flags = 4;
    sa.h = (void *)h_salta;
    sigaction(14, &sa, &viejo); // SIGALRM
    salto_h = 0;
    int r = sigsetjmp(salto, 1);
    if (r == 0) {
        alarm(1);
        char c;
        read(fd[0], &c, 1); // bloquea en el host hasta la senal
        CHECK(0 && "read no debio volver");
    }
    CHECK(r == 7);
    CHECK(salto_h == 1);
    alarm(0);
    sigaction(14, &viejo, 0);
    close(fd[0]);
    close(fd[1]);
}

int run_all(void) {
    hle_abandonable();
    anidamiento();
    mascaras();
    suspension();
    asincronas();
    printf("t6 run_all: %d fallos\n", fails);
    return fails;
}
