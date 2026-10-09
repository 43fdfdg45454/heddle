//! Firmas de la frontera: clase de cada funcion de la API en C del NDK (tabla generada por tools/sigtool).
//!
//! El puente solo reenvia al host una funcion si su firma esta en la tabla y es `Direct`. Las conversiones
//! (callbacks, JNIEnv, retorno de puntero a funcion) salen del TIPO declarado en las cabeceras, nunca del aspecto
//! del valor. Lo que no tiene firma, o cuya ABI difiere entre arm64 y x86-64, no esta disponible salvo que tenga
//! una implementacion propia (HLE).

pub use crate::sigs_gen::{DATA, EXPORTS, ITFS, NDK_API, NDK_REVISION, SIGS, STRUCT_CBS, VK_ALLOC, VK_ALLOC_SIZE, VK_CB_STYPES, VK_FNS, VK_STYPES, VK_TYPES};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum K {
    /// Reenvio con el mapeo de registros, mas las conversiones tipadas indicadas.
    Direct {
        /// mascara de bits: argumentos (0..5) que son JNIEnv*/JavaVM* (se pasa el del host)
        env: u8,
        /// argumentos que son punteros a funcion guest: (indice, clase del retorno del callback)
        cbs: &'static [(u8, u8)],
        /// argumentos estrechos (registro entero, clase): 1 = u8/bool, 2 = i8, 3 = u16, 4 = i16. AAPCS64 no obliga a
        /// quien llama a limpiar los bits altos; el codigo x86-64 del host asume que vienen extendidos
        narrow: &'static [(u8, u8)],
        /// devuelve un puntero a funcion del host (se entrega como slot)
        ret_fn: bool,
        variadic: bool,
        /// alguna estructura alcanzable por puntero contiene punteros a funcion (callbacks en memoria: los cubre
        /// el redireccionamiento de ejecucion; no se convierten al cruzar)
        struct_cb: bool,
    },
    /// ABI incompatible con el mapeo de registros (long double, complejos, va_list, estructuras por valor...).
    Unsafe(&'static str),
    /// Necesita implementacion a mano o un proxy (estructuras con distinta disposicion, objetos con tabla de
    /// funciones del host...).
    Manual(&'static str),
    /// Exportada por el NDK pero sin declaracion en las cabeceras analizadas.
    NoSig,
}

pub struct Sig {
    pub name: &'static str,
    pub lib: &'static str,
    pub k: K,
}

/// Argumento de un metodo de interfaz con tabla de funciones (OpenSL ES / OpenMAX AL), segun las cabeceras.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum A {
    /// escalar o puntero a datos: caracter de shorty (Z B C S I J F D)
    V(u8),
    /// interfaz por valor (indice en `ITFS`): el guest pasa su proxy, el host recibe la interfaz real
    Itf(u16),
    /// puntero a una interfaz que escribe el host (parametro de salida): el guest recibe un proxy
    ItfOut(u16),
    /// puntero a funcion: firma del callback (retorno primero; 'O' = interfaz que el guest ve como su proxy)
    Cb(&'static [u8]),
    /// puntero a SLDataSource/SLDataSink (o XA): su localizador puede llevar un objeto (OUTPUTMIX, IODEVICE)
    Data,
}

/// Metodo de una interfaz. `bad` no vacio: no se puede reenviar (el proxy responde "no soportado").
pub struct Method {
    pub name: &'static str,
    pub ret: u8,
    pub args: &'static [A],
    pub bad: &'static str,
}

/// Interfaz (estructura `...Itf_` de punteros a funcion) y el identificador exportado que la pide (`SL_IID_...`).
pub struct Itf {
    pub name: &'static str,
    pub iid: &'static str,
    pub methods: &'static [Method],
}

/// Puntero a funcion dentro de una estructura: desplazamiento y firma del callback.
pub struct CbField {
    pub off: u32,
    pub sig: &'static [u8],
}

/// Estructura con punteros a funcion que recibe una funcion del NDK.
pub struct StructCb {
    pub func: &'static str,
    /// la estructura (p. ej. "struct z_stream_s")
    pub ty: &'static str,
    /// indice del argumento en C y registro entero (AAPCS64) en que llega
    pub arg: u8,
    pub reg: u8,
    /// por valor (en AAPCS64 una estructura de mas de 16 bytes llega como puntero a una copia)
    pub byval: bool,
    pub size: u32,
    pub fields: &'static [CbField],
    /// vacia: se puede convertir campo a campo; si no, por que no
    pub note: &'static str,
}

/// Funciones que COPIAN la estructura de callbacks durante la llamada (no guardan el puntero que reciben): se les
/// pasa una copia con cada puntero a funcion convertido en un trampolin con su firma. Escrita a mano: el tipo no dice
/// si el host guarda el puntero. Fuera, a proposito: zlib (`STRUCT_INPLACE`), `__pthread_cleanup_push` (HLE propia:
/// enlaza la estructura), `glob` (proxy en ndkcb.rs), Vulkan (`VK_FNS`, vk.rs) y `sigaction` (implementacion propia).
pub const STRUCT_COPIED: &[&str] = &[
    "ACameraCaptureSession_capture",
    "ACameraCaptureSession_captureV2",
    "ACameraCaptureSession_logicalCamera_capture",
    "ACameraCaptureSession_logicalCamera_captureV2",
    "ACameraCaptureSession_logicalCamera_setRepeatingRequest",
    "ACameraCaptureSession_logicalCamera_setRepeatingRequestV2",
    "ACameraCaptureSession_setRepeatingRequest",
    "ACameraCaptureSession_setRepeatingRequestV2",
    "ACameraDevice_createCaptureSession",
    "ACameraDevice_createCaptureSessionWithSessionParameters",
    "ACameraManager_openCamera",
    "ACameraManager_registerAvailabilityCallback",
    "ACameraManager_registerExtendedAvailabilityCallback",
    "ACameraManager_unregisterAvailabilityCallback",
    "ACameraManager_unregisterExtendedAvailabilityCallback",
    "AImageReader_setBufferRemovedListener",
    "AImageReader_setImageListener",
    "utrans_trans",
    "utrans_transIncremental",
];

/// Estructuras de callbacks que el host GUARDA por su direccion y vuelve a leer en cada llamada que la recibe (zlib:
/// `inflateInit` anota `strm` en su estado y `inflate` comprueba que `state->strm == strm`; lee `zalloc`/`zfree` en
/// cada reserva). No se puede pasar una copia: durante la llamada los punteros a funcion del guest se sustituyen EN SU
/// SITIO por los del host (trampolin con su firma, o la funcion del host de un slot) y al volver se reponen; lo que el
/// host escribio (su `zcalloc` por defecto, la copia de `deflateCopy`) se entrega al guest como slot o como su funcion.
/// Escrita a mano: el tipo no dice que la estructura se guarde.
pub const STRUCT_INPLACE: &[&str] = &["struct z_stream_s"];

/// Funcion de Vulkan con `pAllocator` (registro AAPCS64, 255 = ninguno) o estructuras de entrada que pueden llevar
/// callbacks (`args`; vacio en casi todas: solo las de creacion de instancia, dispositivo y mensajeros).
pub struct VkFn {
    pub name: &'static str,
    pub alloc: u8,
    pub args: &'static [VkArg],
}

/// Argumento puntero a estructura(s) de entrada que pueden llevar callbacks: registro, registro de la cuenta si es un
/// arreglo (255 = un solo elemento) y tipo (indice en `VK_TYPES`).
pub struct VkArg {
    pub reg: u8,
    pub count: u8,
    pub ty: u16,
}

/// Indice de tipo nulo (`VkSType::ty` de una estructura sin callbacks).
pub const VK_NONE: u16 = u16::MAX;

/// Estructura de Vulkan que puede llevar callbacks, directa o transitivamente (ver sigtool).
pub struct VkType {
    pub name: &'static str,
    pub size: u32,
    /// su cadena `pNext` puede llevar estructuras con callbacks (solo cuando es la base de la cadena)
    pub chain: bool,
    /// sus punteros a funcion (`note` no vacia: no se convierten y se registra)
    pub fields: &'static [CbField],
    pub note: &'static str,
    /// campos que apuntan a otras estructuras de `VK_TYPES`
    pub ptrs: &'static [VkPtr],
}

/// Campo puntero a una estructura (o arreglo) que puede llevar callbacks.
pub struct VkPtr {
    pub off: u32,
    /// desplazamiento del campo con la cuenta (`u32::MAX`: un solo elemento) y su tamano (4 u 8)
    pub count_off: u32,
    pub count_size: u8,
    /// 1: arreglo de estructuras; 2: arreglo de punteros a estructuras
    pub deref: u8,
    pub ty: u16,
}

/// Estructura de Vulkan con cabecera sType/pNext: tamano y, si lleva callbacks como nodo de una cadena, su tipo.
pub struct VkSType {
    pub stype: u32,
    pub size: u32,
    pub name: &'static str,
    pub ty: u16,
}

/// Datos de la funcion de Vulkan `name` (None: ni pAllocator ni cadenas).
pub fn vk_fn(name: &str) -> Option<&'static VkFn> {
    VK_FNS.binary_search_by(|f| f.name.cmp(name)).ok().map(|i| &VK_FNS[i])
}

/// Estructura de Vulkan con el sType `st`.
pub fn vk_stype(st: u32) -> Option<&'static VkSType> {
    VK_STYPES.binary_search_by(|s| s.stype.cmp(&st)).ok().map(|i| &VK_STYPES[i])
}

/// Estructuras con callbacks que recibe `name` (vacio si ninguna).
pub fn struct_cbs(name: &str) -> &'static [StructCb] {
    let lo = STRUCT_CBS.partition_point(|s| s.func < name);
    let hi = STRUCT_CBS.partition_point(|s| s.func <= name);
    &STRUCT_CBS[lo..hi]
}

/// Indice en `ITFS` de la interfaz `name` (p. ej. "SLObjectItf_").
pub fn itf(name: &str) -> Option<usize> {
    ITFS.binary_search_by(|i| i.name.cmp(name)).ok()
}

/// Firma de la funcion `name` de la API del NDK (None: no es API publica en C del NDK).
pub fn lookup(name: &str) -> Option<&'static Sig> {
    SIGS.binary_search_by(|s| s.name.cmp(name)).ok().map(|i| &SIGS[i])
}

/// `name` es un simbolo de datos exportado por el NDK?
pub fn is_data(name: &str) -> bool {
    DATA.binary_search_by(|d| d.0.cmp(name)).is_ok()
}

/// DT_NEEDED de cada biblioteca del sistema que sirve el puente, en su orden, como en un dispositivo (Android 16):
/// escrita a mano desde los Android.bp de AOSP, porque los stubs del NDK no llevan dependencias. Solo las bibliotecas
/// que el puente "carga" (NDK, libdl_android y ld-android): las privadas de la plataforma (libbinder, libutils,
/// libhwui...) no se siguen. Las de bionic son exactas (libc -> ld-android, libdl; libm -> libc; libdl -> ld-android);
/// el resto, sus dependencias publicas directas y las `system_shared_libs` por defecto de Soong (libc, libm, libdl).
/// La recorren `dlsym` con el handle de una biblioteca del sistema y el grupo local de una biblioteca guest (bionic
/// `walk_dependencies_tree`).
pub const SYS_NEEDED: &[(&str, &[&str])] = &[
    ("ld-android.so", &[]),
    ("libdl.so", &["ld-android.so"]),
    ("libc.so", &["ld-android.so", "libdl.so"]),
    ("libm.so", &["libc.so"]),
    ("libdl_android.so", &["ld-android.so"]),
    ("libstdc++.so", &["libc.so"]),
    ("liblog.so", &["libc.so", "libm.so", "libdl.so"]),
    ("libz.so", &["libc.so", "libm.so", "libdl.so"]),
    ("libsync.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libnativehelper.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libicu.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libbinder_ndk.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libnativewindow.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libEGL.so", &["liblog.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
    ("libGLESv1_CM.so", &["liblog.so", "libEGL.so", "libc.so", "libm.so", "libdl.so"]),
    ("libGLESv2.so", &["liblog.so", "libEGL.so", "libc.so", "libm.so", "libdl.so"]),
    ("libGLESv3.so", &["liblog.so", "libEGL.so", "libc.so", "libm.so", "libdl.so"]),
    ("libvulkan.so", &["liblog.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
    ("libandroid.so", &["liblog.so", "libbinder_ndk.so", "libEGL.so", "libGLESv2.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
    ("libjnigraphics.so", &["liblog.so", "libandroid.so", "libc.so", "libm.so", "libdl.so"]),
    ("libmediandk.so", &["liblog.so", "libbinder_ndk.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
    ("libcamera2ndk.so", &["liblog.so", "libmediandk.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
    ("libaaudio.so", &["liblog.so", "libbinder_ndk.so", "libc.so", "libm.so", "libdl.so"]),
    ("libOpenSLES.so", &["liblog.so", "libc.so", "libm.so", "libdl.so"]),
    ("libOpenMAXAL.so", &["liblog.so", "libnativewindow.so", "libc.so", "libm.so", "libdl.so"]),
];

/// DT_NEEDED de la biblioteca del sistema `lib` (vacio si no esta en `SYS_NEEDED`).
pub fn sys_needed(lib: &str) -> &'static [&'static str] {
    SYS_NEEDED.iter().find(|e| e.0 == lib).map_or(&[], |e| e.1)
}

/// Hash FNV-1a de un nombre (indice de `exporters`).
fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

/// Bibliotecas del NDK que exportan `name`, cada una con la version del simbolo (`EXPORTS`; vacio si ninguna). Se
/// consulta en cada simbolo de cada relocacion y en cada dlsym: tabla de dispersion fija (creada una vez, una ranura
/// por nombre distinto con factor de carga <= 1/2) en lugar de la busqueda binaria por cadenas (~170 ns).
pub fn exporters(name: &str) -> &'static [(&'static str, &'static str, &'static str)] {
    static IDX: std::sync::OnceLock<(Vec<(u32, u32)>, usize)> = std::sync::OnceLock::new();
    let (tab, mask) = IDX.get_or_init(|| {
        let distinct = EXPORTS.windows(2).filter(|w| w[0].0 != w[1].0).count() + 1;
        let size = (distinct * 2).next_power_of_two();
        let mut tab = vec![(0u32, 0u32); size];
        let mut i = 0;
        while i < EXPORTS.len() {
            let n = EXPORTS[i..].iter().take_while(|e| e.0 == EXPORTS[i].0).count();
            let mut h = fnv(EXPORTS[i].0) as usize & (size - 1);
            while tab[h].1 != 0 {
                h = (h + 1) & (size - 1);
            }
            tab[h] = (i as u32, n as u32);
            i += n;
        }
        (tab, size - 1)
    });
    let mut h = fnv(name) as usize & mask;
    loop {
        let (i, n) = tab[h];
        if n == 0 {
            return &[];
        }
        if EXPORTS[i as usize].0 == name {
            return &EXPORTS[i as usize..(i + n) as usize];
        }
        h = (h + 1) & mask;
    }
}

/// Versiones que define cada biblioteca del NDK (vacio: la biblioteca no tiene versiones de simbolos).
pub fn lib_versions(lib: &str) -> &'static [&'static str] {
    static V: std::sync::OnceLock<Vec<(&'static str, Vec<&'static str>)>> = std::sync::OnceLock::new();
    let v = V.get_or_init(|| {
        let mut v: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
        for &(_, l, ver) in EXPORTS {
            let i = match v.iter().position(|e| e.0 == l) {
                Some(i) => i,
                None => {
                    v.push((l, Vec::new()));
                    v.len() - 1
                }
            };
            if !ver.is_empty() && !v[i].1.contains(&ver) {
                v[i].1.push(ver);
            }
        }
        v
    });
    v.iter().find(|e| e.0 == lib).map_or(&[], |e| e.1.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_tabla_esta_ordenada_y_sin_duplicados() {
        assert!(SIGS.len() > 3000);
        assert!(SIGS.windows(2).all(|w| w[0].name < w[1].name));
        assert!(DATA.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(EXPORTS.windows(2).all(|w| (w[0].0, w[0].1) < (w[1].0, w[1].1)));
    }

    #[test]
    fn cada_simbolo_con_todas_sus_bibliotecas_y_versiones() {
        let libs = |n: &str| exporters(n).iter().map(|e| (e.1, e.2)).collect::<Vec<_>>();
        assert_eq!(libs("glClear"), [("libGLESv1_CM.so", ""), ("libGLESv2.so", ""), ("libGLESv3.so", "")]);
        assert_eq!(libs("malloc"), [("libc.so", "LIBC")]);
        assert_eq!(libs("dlvsym"), [("libdl.so", "LIBC_N")]);
        assert_eq!(libs("_Znwm"), [("libstdc++.so", "LIBC_O")]);
        assert!(libs("no_existe_en_el_ndk").is_empty());
        // la tabla de dispersion da lo mismo que la busqueda binaria para todos los nombres
        for e in EXPORTS {
            let lo = EXPORTS.partition_point(|x| x.0 < e.0);
            let hi = EXPORTS.partition_point(|x| x.0 <= e.0);
            assert_eq!(exporters(e.0).as_ptr(), EXPORTS[lo..hi].as_ptr());
            assert_eq!(exporters(e.0).len(), hi - lo);
        }
        assert!(lib_versions("libc.so").contains(&"LIBC_N") && lib_versions("libGLESv2.so").is_empty());
    }

    #[test]
    fn el_generador_detecta_los_fallos_conocidos() {
        let k = |n: &str| lookup(n).map(|s| s.k);
        // retorno de estructura grande, long double, va_list, complejos: nunca reenvio directo
        for n in ["mallinfo", "strtold", "sqrtl", "nexttoward", "vsnprintf", "vsscanf", "cabs", "cexpf", "__isinfl"] {
            assert!(matches!(k(n), Some(K::Unsafe(_))), "{} deberia ser ABI incompatible: {:?}", n, k(n));
        }
        // estructuras con distinta disposicion entre arm64 y x86-64, u objetos con tabla de funciones del host
        for n in ["fegetenv", "fesetenv", "feholdexcept", "epoll_wait", "epoll_ctl", "stat64", "fstat64", "sigaction", "slCreateEngine"] {
            assert!(matches!(k(n), Some(K::Manual(_))), "{} deberia requerir implementacion a mano: {:?}", n, k(n));
        }
        // la estructura stat difiere aunque alguna cabecera solo la declare por adelantado; tambien via callbacks
        for n in ["stat", "lstat", "fstat", "ftw", "nftw", "fts_children"] {
            assert!(matches!(k(n), Some(K::Manual(_))), "{} deberia requerir implementacion a mano: {:?}", n, k(n));
        }
        // argumentos estrechos anotados para extenderlos
        assert!(matches!(k("glColorMask"), Some(K::Direct { narrow: &[(0, 1), (1, 1), (2, 1), (3, 1)], .. })));
        assert!(matches!(k("glDepthMask"), Some(K::Direct { narrow: &[(0, 1)], .. })));
        // funciones corrientes: directas y sin conversiones
        for n in ["strlen", "memcpy", "malloc", "strtoull", "pthread_setspecific", "glDrawArrays", "eglSwapBuffers", "vkCreateInstance", "sin"] {
            assert!(matches!(k(n), Some(K::Direct { env: 0, cbs: &[], narrow: &[], ret_fn: false, .. })), "{}: {:?}", n, k(n));
        }
        // callbacks detectados por tipo
        assert!(matches!(k("qsort"), Some(K::Direct { cbs: &[(3, b'I')], .. })));
        assert!(matches!(k("pthread_create"), Some(K::Direct { cbs: &[(2, b'J')], .. })));
        assert!(matches!(k("ALooper_addFd"), Some(K::Direct { cbs: &[(4, b'I')], .. })));
        assert!(matches!(k("glDebugMessageCallback"), Some(K::Direct { cbs: &[(0, b'V')], .. })));
        // JNIEnv* detectado por tipo (y solo donde lo hay)
        assert!(matches!(k("AAssetManager_fromJava"), Some(K::Direct { env: 1, .. })));
        assert!(matches!(k("ANativeWindow_fromSurface"), Some(K::Direct { env: 1, .. })));
        // retorno de puntero a funcion detectado por tipo
        for n in ["eglGetProcAddress", "vkGetInstanceProcAddr", "vkGetDeviceProcAddr", "signal"] {
            assert!(matches!(k(n), Some(K::Direct { ret_fn: true, .. })), "{}: {:?}", n, k(n));
        }
        // lo que no es API del NDK no existe
        assert!(lookup("_ZN7android10VectorImpl12appendVectorERKS0_").is_none());
        assert!(lookup("art_quick_invoke_stub").is_none());
        assert!(is_data("environ") && is_data("stdout") && !is_data("strlen"));
    }

    #[test]
    fn interfaces_y_estructuras_con_callbacks() {
        assert!(ITFS.windows(2).all(|w| w[0].name < w[1].name));
        assert!(STRUCT_CBS.windows(2).all(|w| w[0].func <= w[1].func));
        let o = itf("SLObjectItf_").unwrap();
        let m = |i: usize, n: &str| ITFS[i].methods.iter().find(|m| m.name == n).unwrap();
        // RegisterCallback del objeto: slObjectCallback(caller, ctx, event, result, param, pInterface)
        assert_eq!(m(o, "RegisterCallback").args[1], A::Cb(b"VOJIIIJ"));
        assert_eq!(m(o, "Destroy").ret, b'V');
        let e = itf("SLEngineItf_").unwrap();
        assert_eq!(ITFS[e].iid, "SL_IID_ENGINE");
        assert_eq!(m(e, "CreateAudioPlayer").args[..4], [A::Itf(e as u16), A::ItfOut(o as u16), A::Data, A::Data]);
        // SLmillibel (int16): se extiende con signo
        let v = itf("SLVolumeItf_").unwrap();
        assert_eq!(m(v, "SetVolumeLevel").args[1], A::V(b'S'));
        let bq = itf("SLAndroidSimpleBufferQueueItf_").unwrap();
        assert_eq!(m(bq, "RegisterCallback").args[1], A::Cb(b"VOJ"));
        // OpenMAX AL: 11 argumentos (los ultimos en la pila)
        let xe = itf("XAEngineItf_").unwrap();
        assert_eq!(m(xe, "CreateMediaPlayer").args.len(), 11);
        assert!(ITFS.iter().flat_map(|i| i.methods).filter(|m| m.bad.is_empty()).count() > 500);
        // estructuras de callbacks: camara (copia), AMediaCodec por valor, timer_create en una union
        let c = struct_cbs("ACameraManager_openCamera");
        assert_eq!((c.len(), c[0].reg, c[0].size, c[0].fields.len(), c[0].note), (1, 2, 24, 2, ""));
        assert_eq!(c[0].fields[1].sig, b"VJJI");
        let mc = struct_cbs("AMediaCodec_setAsyncNotifyCallback");
        assert!(mc[0].byval && mc[0].size == 32 && mc[0].fields.len() == 4);
        assert!(!struct_cbs("timer_create")[0].note.is_empty());
        for f in STRUCT_COPIED {
            let s = struct_cbs(f);
            assert!(!s.is_empty() && s.iter().all(|s| s.note.is_empty() && !s.byval), "{}", f);
        }
    }
}
