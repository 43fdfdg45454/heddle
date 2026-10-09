// Vulkan por el puente (vk.rs): instancia y dispositivo con un asignador propio (pAllocator: las cinco funciones son
// guest y el host las llama por trampolines) y un mensajero de VK_EXT_debug_utils en la cadena pNext de
// vkCreateInstance (callback guest en una estructura que el puente copia y convierte). Comprueba que todo lo que el
// cargador y el ICD reservaron con el asignador se libera al destruir. Sin cargador o sin dispositivo fisico
// (ningun ICD) imprime "sin Vulkan" y no cuenta como fallo, salvo con HEDDLE_REQUIRE_VULKAN=1 (el CI, con lavapipe).
// Las estructuras se declaran aqui (disposicion LP64 de vulkan_core.h) para no depender de las cabeceras.
extern int printf(const char *, ...);
extern char *getenv(const char *);
extern int posix_memalign(void **, unsigned long, unsigned long);
extern void free(void *);
extern void *memcpy(void *, const void *, unsigned long);

typedef unsigned int u32;
typedef unsigned long u64;
typedef int VkResult;
typedef void *VkInstance, *VkPhysicalDevice, *VkDevice;

typedef struct { u32 sType; const void *pNext; const char *app; u32 appv; const char *eng; u32 engv; u32 api; } AppInfo;
typedef struct {
    u32 sType; const void *pNext; u32 flags; const AppInfo *app;
    u32 nlayers; const char *const *layers; u32 nexts; const char *const *exts;
} InstInfo;
typedef struct {
    void *ud;
    void *(*alloc)(void *, u64, u64, int);
    void *(*realloc)(void *, void *, u64, u64, int);
    void (*free)(void *, void *);
    void (*ialloc)(void *, u64, int, int);
    void (*ifree)(void *, u64, int, int);
} Alloc;
typedef struct {
    u32 sType; const void *pNext; u32 flags; u32 severity; u32 types;
    u32 (*cb)(u32, u32, const void *, void *); void *ud;
} MsgInfo;
typedef struct { u32 sType; const void *pNext; u32 flags; u32 family; u32 count; const float *prio; } QueueInfo;
typedef struct {
    u32 sType; const void *pNext; u32 flags; u32 nqueues; const QueueInfo *queues;
    u32 nlayers; const char *const *layers; u32 nexts; const char *const *exts; const void *features;
} DevInfo;
typedef struct { u32 flags; u32 count; u32 tsbits; u32 gran[3]; } QueueFamily;

extern VkResult vkCreateInstance(const InstInfo *, const Alloc *, VkInstance *);
extern void vkDestroyInstance(VkInstance, const Alloc *);
extern VkResult vkEnumeratePhysicalDevices(VkInstance, u32 *, VkPhysicalDevice *);
extern void vkGetPhysicalDeviceQueueFamilyProperties(VkPhysicalDevice, u32 *, QueueFamily *);
extern VkResult vkCreateDevice(VkPhysicalDevice, const DevInfo *, const Alloc *, VkDevice *);
extern void vkDestroyDevice(VkDevice, const Alloc *);

static long vivos, total;
static int ud_mal;
static char marca;

// cabecera antes del bloque: [tamano][alineacion] en los 16 bytes previos (alineacion >= 16)
static void *a_alloc(void *ud, u64 size, u64 align, int scope) {
    if (ud != &marca) ud_mal = 1;
    if (align < 16) align = 16;
    void *p;
    if (posix_memalign(&p, align, size + align)) return 0;
    char *r = (char *)p + align;
    ((u64 *)r)[-1] = size;
    ((u64 *)r)[-2] = align;
    vivos++;
    total++;
    return r;
}
static void a_free(void *ud, void *m) {
    if (ud != &marca) ud_mal = 1;
    if (!m) return;
    vivos--;
    free((char *)m - ((u64 *)m)[-2]);
}
static void *a_realloc(void *ud, void *orig, u64 size, u64 align, int scope) {
    if (!orig) return a_alloc(ud, size, align, scope);
    if (!size) { a_free(ud, orig); return 0; }
    void *n = a_alloc(ud, size, align, scope);
    if (!n) return 0;
    u64 old = ((u64 *)orig)[-1];
    memcpy(n, orig, old < size ? old : size);
    a_free(ud, orig);
    return n;
}

static int mensajes;
static u32 msg_cb(u32 sev, u32 types, const void *data, void *ud) {
    if (ud == &marca) mensajes++;
    return 0;
}

static int fallos;
static void check(int ok, const char *que) {
    if (!ok) { printf("FALLO %s\n", que); fallos++; }
}

int run_all(void) {
    const char *req = getenv("HEDDLE_REQUIRE_VULKAN");
    int requerido = req && req[0] == '1';
    Alloc al = { &marca, a_alloc, a_realloc, a_free, 0, 0 };
    MsgInfo msg = { 1000128004, 0, 0, 0x1111 /* verbose..error */, 0x7, msg_cb, &marca };
    AppInfo app = { 0, 0, "tvk", 1, "heddle", 1, (1u << 22) /* 1.0 */ };
    const char *ext[] = { "VK_EXT_debug_utils" };
    InstInfo ci = { 1, &msg, 0, &app, 0, 0, 1, ext };
    VkInstance inst = 0;
    VkResult r = vkCreateInstance(&ci, &al, &inst);
    if (r == -7 /* VK_ERROR_EXTENSION_NOT_PRESENT */) {
        ci.pNext = 0;
        ci.nexts = 0;
        r = vkCreateInstance(&ci, &al, &inst);
    }
    if (r != 0) {
        printf("tvk: sin Vulkan (vkCreateInstance = %d)\n", r);
        check(!requerido, "Vulkan requerido");
        printf("tvk run_all: %d fallos\n", fallos);
        return fallos;
    }
    check(total > 0, "el cargador no uso el asignador en vkCreateInstance");
    u32 n = 0;
    vkEnumeratePhysicalDevices(inst, &n, 0);
    VkPhysicalDevice pds[8];
    if (n > 8) n = 8;
    if (n == 0 || vkEnumeratePhysicalDevices(inst, &n, pds) < 0 || n == 0) {
        printf("tvk: sin Vulkan (ningun dispositivo fisico)\n");
        check(!requerido, "Vulkan requerido");
    } else {
        u32 nf = 0;
        vkGetPhysicalDeviceQueueFamilyProperties(pds[0], &nf, 0);
        QueueFamily fam[16];
        if (nf > 16) nf = 16;
        vkGetPhysicalDeviceQueueFamilyProperties(pds[0], &nf, fam);
        check(nf > 0, "sin familias de colas");
        float prio = 1.0f;
        QueueInfo q = { 2, 0, 0, 0, 1, &prio };
        DevInfo di = { 3, 0, 0, 1, &q, 0, 0, 0, 0, 0 };
        long antes = total;
        VkDevice dev = 0;
        check(vkCreateDevice(pds[0], &di, &al, &dev) == 0 && dev, "vkCreateDevice");
        check(total > antes, "el ICD no uso el asignador en vkCreateDevice");
        if (dev) vkDestroyDevice(dev, &al);
    }
    vkDestroyInstance(inst, &al);
    check(vivos == 0, "bloques del asignador sin liberar");
    check(!ud_mal, "pUserData del asignador cambiado");
    printf("tvk: %ld reservas con el asignador guest, %d mensajes de debug_utils\n", total, mensajes);
    printf("tvk run_all: %d fallos\n", fallos);
    return fallos;
}
