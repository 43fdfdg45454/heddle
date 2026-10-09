#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
static int fails;
#define CHECK(c) do { if (!(c)) { printf("FALLO linea %d: %s\n", __LINE__, #c); fails++; } } while (0)
typedef struct { uint32_t version; int (*initialize)(const void *, const char *, const char *); void *(*loadLibrary)(const char *, int);
  void *(*getTrampoline)(void *, const char *, const char *, uint32_t); } NB;
static void *rcb[3];
static void *jenvtbl[240]; static const void *jenv = jenvtbl; static void *jvmt[8]; static const void *jvm = jvmt;
static int ver(void *e) { return 0x10006; }
int main(int argc, char **argv) {
    void *ma = dlopen("libmockandroid.so", RTLD_NOW | RTLD_GLOBAL);
    CHECK(ma);
    int (*fire)(void) = dlsym(ma, "mock_fire");
    jenvtbl[4] = (void *)ver;
    void *h = dlopen(argv[1], RTLD_NOW);
    NB *nb = dlsym(h, "NativeBridgeItf");
    CHECK(nb->initialize(rcb, "/tmp", "arm64"));
    void *lib = nb->loadLibrary(argv[2], 2);
    CHECK(lib);
    int (*run)(void) = nb->getTrampoline(lib, "t4_run", "I", 1);
    CHECK(run() == 42);                 // eglGetProcAddress devuelve una funcion del host invocada desde el guest
    CHECK(fire() == 1);                 // el host invoca el callback guest registrado con ALooper_addFd
    int (*gf)(void) = nb->getTrampoline(lib, "get_fired", "I", 1);
    CHECK(gf() == 7 * 100 + 3 + 5);
    // ANativeActivity: el guest recibe un proxy; la estructura del host no se toca (vm/env siguen siendo los del host)
    void *(*finished)(void) = dlsym(ma, "mock_finished");
    unsigned (*flags)(void) = dlsym(ma, "mock_flags");
    void *cbs[16] = {0};
    struct { void *callbacks; const void **vm; const void **env; void *clazz; } act = { cbs, &jvm, &jenv, 0 };
    void (*create)(void *, void *, unsigned long) = nb->getTrampoline(lib, "ANativeActivity_onCreate", "VLLL", 4);
    create(&act, 0, 0);
    CHECK(act.vm == &jvm && act.env == &jenv && act.callbacks == cbs);
    int all = 1;
    for (int i = 0; i < 16; i++) all &= cbs[i] != 0;
    CHECK(all);
    int (*get)(const char *) = 0;
#define GET(n) (((int (*)(void))nb->getTrampoline(lib, n, "I", 1))())
    ((void (*)(void *))cbs[0])(&act);   // onStart (host -> guest)
    CHECK(GET("get_started") == 1);
    CHECK(GET("get_version") == 0x10006);
    unsigned long sz = 0;
    void *r = ((void *(*)(void *, unsigned long *))cbs[2])(&act, &sz);
    CHECK(r == (void *)0x5000 && sz == 4);
    ((void (*)(void *))cbs[1])(&act);   // onResume: puesto por el guest en onStart; llama a ANativeActivity_finish
    CHECK(GET("get_resumed") == 1);
    CHECK(finished() == (void *)&act); // el host recibe SU actividad, no el proxy
    CHECK(flags() == 3 * 16 + 5);
    ((void (*)(void *, int))cbs[6])(&act, 1);   // onWindowFocusChanged(activity, int)
    CHECK(GET("get_focus") == 1);
    ((void (*)(void *))cbs[3])(&act);   // onPause: el guest no lo puso, no pasa nada
    ((void (*)(void *))cbs[5])(&act);   // onDestroy: libera el proxy
    CHECK(GET("get_destroyed") == 1);
    ((void (*)(void *))cbs[0])(&act);   // tras onDestroy ya no llega al guest
    CHECK(GET("get_started") == 1);
    // punto de entrada propio del manifiesto (android.app.func_name): pedido con firma nula, como loadNativeCode_native
    void *cbs2[16] = {0};
    struct { void *callbacks; const void **vm; const void **env; void *clazz; } act2 = { cbs2, &jvm, &jenv, 0 };
    void (*entry)(void *, void *, unsigned long) = nb->getTrampoline(lib, "MiEntrada", NULL, 0);
    CHECK(entry != 0);
    entry(&act2, 0, 0);
    CHECK(GET("get_custom") == 1 && act2.vm == &jvm && act2.callbacks == cbs2);
    all = 1;
    for (int i = 0; i < 16; i++) all &= cbs2[i] != 0;
    CHECK(all);
    ((void (*)(void *))cbs2[0])(&act2);   // onStart con el proxy nuevo
    CHECK(GET("get_started") == 2);
    ((void (*)(void *))cbs2[5])(&act2);
    CHECK(GET("get_destroyed") == 2);
    // un nombre del manifiesto para otra biblioteca, o con firma: una funcion normal (sin proxy de actividad: la tabla
    // de callbacks del host no se rellena)
    void *cbs3[16] = {0};
    struct { void *callbacks; const void **vm; const void **env; void *clazz; } act3 = { cbs3, &jvm, &jenv, 0 };
    void (*otra)(void *, void *, unsigned long) = nb->getTrampoline(lib, "OtraEntrada", NULL, 0);
    otra(&act3, &act3, (unsigned long)&act3);
    CHECK(GET("get_otra") == 1);
    all = 1;
    for (int i = 0; i < 16; i++) all &= cbs3[i] == 0;
    CHECK(all);
    otra = nb->getTrampoline(lib, "MiEntrada", "VLLL", 4);
    CHECK(otra != 0);
    int (*manifest)(void) = dlsym(ma, "mock_manifest");
    CHECK(manifest() == 16);            // leido una vez, con GET_ACTIVITIES | GET_META_DATA
    (void)get;
    printf("nbtest2: %d fallos\n", fails);
    return fails;
}
