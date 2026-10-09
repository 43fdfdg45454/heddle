typedef unsigned long size_t;
extern int printf(const char *, ...);
extern int ALooper_addFd(void *, int, int, int, int (*)(int, int, void *), void *);
extern void *eglGetProcAddress(const char *);
extern void ANativeActivity_finish(void *);
extern void ANativeActivity_setWindowFlags(void *, unsigned, unsigned);
static int fired, started, saved, version, resumed, focus = -1, destroyed;
static void *created_act;
static int cb(int fd, int ev, void *d) { fired = fd * 100 + ev + (int)(long)d; return 1; }
int get_fired(void) { return fired; }
int get_started(void) { return started; }
int get_saved(void) { return saved; }
int get_version(void) { return version; }
int get_resumed(void) { return resumed; }
int get_focus(void) { return focus; }
int get_destroyed(void) { return destroyed; }
int t4_run(void) {
    ALooper_addFd((void *)1, 7, 0, 1, cb, (void *)5);
    int (*f)(int) = eglGetProcAddress("host_twice");
    return f(21);
}
struct callbacks { void *fn[16]; };
struct act { struct callbacks *cb; void **vm; void ***env; void *clazz; };
static void on_resume(struct act *a) {
    // la actividad que llega es el proxy que recibio onCreate; las funciones ANativeActivity_* la desenvuelven
    if (a == created_act) resumed++;
    ANativeActivity_finish(a);
    ANativeActivity_setWindowFlags(a, 3, 5);
}
static void on_focus(struct act *a, int has) { if (a == created_act) focus = has; }
static void on_destroy(struct act *a) { if (a == created_act) destroyed++; }
static void on_start(struct act *a) {
    started++;
    void **tbl = *(void ***)a->env;
    version = ((int (*)(void *))tbl[4])(a->env);
    // callbacks puestos despues de onCreate: tambien llegan
    a->cb->fn[1] = on_resume;
    a->cb->fn[5] = on_destroy;
    a->cb->fn[6] = on_focus;
}
static void *on_save(struct act *a, size_t *out) { saved = 1; *out = 4; return (void *)0x5000; }
void ANativeActivity_onCreate(struct act *a, void *state, size_t sz) {
    created_act = a;
    a->cb->fn[0] = on_start;
    a->cb->fn[2] = on_save;
}
// punto de entrada propio (android.app.func_name del manifiesto simulado en mock_android.c)
static int custom;
int get_custom(void) { return custom; }
void MiEntrada(struct act *a, void *state, size_t sz) {
    custom++;
    ANativeActivity_onCreate(a, state, sz);
}
// declarado en el manifiesto para otra biblioteca: aqui es una funcion normal
static int otra = -1;
int get_otra(void) { return otra; }
void OtraEntrada(void *a, void *state, size_t sz) { otra = state == (void *)sz; }
