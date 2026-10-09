//! Frontera host <-> guest. UNICO modulo que puede ejecutar codigo del host o convertir punteros a funcion entre
//! los dos mundos. Leer CLAUDE.md antes de tocarlo.
//!
//! Invariantes (los comprueba `tests/arch_lint.rs` y los tests de este modulo):
//!  1. El guest NUNCA ejecuta una direccion del host. Lo unico "ejecutable" que ve son direcciones guest: codigo
//!     ARM de sus modulos y slots HLE. Un salto a codigo del host es una filtracion: fallo con diagnostico.
//!  2. El host NUNCA ejecuta una direccion guest. El codigo guest no se mapea ejecutable; si el host la llama, el
//!     fallo de ejecucion se redirige a un trampolin (solo si la direccion es realmente guest).
//!  3. Solo se llama al host a traves de `HostFn`, y un `HostFn` solo nace de: un simbolo exportado por una
//!     biblioteca PERMITIDA (`symbol`), o un puntero que el propio host entrego y que se valida como codigo del
//!     host (`from_host`). No hay conversion desde un entero arbitrario.
//!  4. Un puntero a funcion que el host DEVUELVE se convierte a slot guest solo donde el tipo se conoce (`RET_FN`,
//!     JNI). El reenvio universal no toca el retorno: un valor devuelto no tiene tipo y convertirlo por su aspecto
//!     corrompe datos. Lo que llegue crudo no se ejecuta nunca (invariante 1).
//!  5. Un `JNIEnv*`/`JavaVM*` guest solo se sustituye por el real donde el tipo es conocido: primer argumento de
//!     las funciones JNI y los argumentos que la tabla de firmas declara como JNIEnv* (sigs.rs). Nunca por heuristica: un
//!     envoltorio que el guest pasa como DATO (pthread_setspecific, memcpy...) debe llegar intacto.

use crate::cpu::Cpu;
use crate::hle::{self, Handler, Ret};
use crate::hostcall::{self, Call};
use crate::sys::*;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

const TBI: u64 = 0x00FF_FFFF_FFFF_FFFF;

// ---------------------------------------------------------------------------------------------
// Tipos
// ---------------------------------------------------------------------------------------------

/// Direccion de codigo x86-64 del host que el puente puede llamar. Campo privado: solo se construye aqui.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HostFn(usize);

impl HostFn {
    pub fn addr(self) -> usize {
        self.0
    }

    /// Puntero que el HOST entrego (tabla JNI, retorno de GetProcAddress, callbacks del runtime). Se valida que sea
    /// codigo del host: nunca un slot HLE, codigo guest, datos ni un entero cualquiera.
    pub fn from_host(p: u64) -> Option<HostFn> {
        if is_host_code(p) {
            Some(HostFn(p as usize))
        } else {
            None
        }
    }

    /// Como `from_host`, validada sin bloqueos ni reservas (manejadores de senal, registrador de liblog): la ultima
    /// instantanea del mapa del host o un trampolin del puente.
    pub fn from_host_sig(p: u64) -> Option<HostFn> {
        if is_host_code_sig(p) {
            Some(HostFn(p as usize))
        } else {
            None
        }
    }

    /// Funcion exportada por una biblioteca del host PERMITIDA (ver `lib_allowed`). Un simbolo de datos no es
    /// una funcion: None.
    pub fn symbol(name: &str) -> Option<HostFn> {
        HostFn::from_host(symbol_addr(name) as u64)
    }
}

// ---------------------------------------------------------------------------------------------
// Mapa de codigo ejecutable del host
// ---------------------------------------------------------------------------------------------

mod hostmap {
    use super::*;
    use std::sync::atomic::{fence, AtomicUsize, Ordering::{Acquire, Release}};

    /// Instantaneas del mapa: NBUF bufferes fijos de CAP rangos (inicio, fin), ordenados y sin solaparse. El escritor
    /// (serializado con WRITER) rellena el bufer siguiente al publicado y lo publica en CUR; un lector (tambien un
    /// manejador de senal: sin bloqueos ni reservas) lee el publicado y comprueba con la secuencia de ese bufer que no
    /// lo reutilizo un escritor mientras leia (haria falta que el mapa se releyera NBUF-1 veces durante una busqueda
    /// binaria). Memoria fija: NBUF * CAP * 16 bytes (estatica: solo ocupa lo que se toca).
    const NBUF: usize = 4;
    pub(super) const CAP: usize = 8192;
    struct Snap {
        /// impar = escribiendose
        seq: AtomicU64,
        len: AtomicUsize,
        lo: [AtomicU64; CAP],
        hi: [AtomicU64; CAP],
    }
    static SNAPS: [Snap; NBUF] =
        [const { Snap { seq: AtomicU64::new(0), len: AtomicUsize::new(0), lo: [const { AtomicU64::new(0) }; CAP], hi: [const { AtomicU64::new(0) }; CAP] } }; NBUF];
    /// indice del bufer publicado
    static CUR: AtomicUsize = AtomicUsize::new(0);
    static WRITER: Mutex<()> = Mutex::new(());
    /// mapas con mas de CAP rangos (se descartan los de mas; se registra una vez)
    static OVER: AtomicU64 = AtomicU64::new(0);
    /// contador de ciclos (rdtsc) de la ultima lectura; 0 = nunca
    static STAMP: AtomicU64 = AtomicU64::new(0);
    /// numero de relecturas: invalida las caches por hilo
    static GEN: AtomicU64 = AtomicU64::new(1);
    static LIBGEN: AtomicU64 = AtomicU64::new(u64::MAX);
    /// Antiguedad maxima del mapa en ciclos de reloj (2^27: unos 30-60 ms). Pasada, se pregunta al enlazador
    /// (barato) si cargo o descargo algo; el mapa solo se relee si hubo cambios.
    const STALE: u64 = 1 << 27;

    thread_local! {
        /// Ultimo hueco (sin codigo del host) consultado por este hilo: (inicio, fin, generacion). La gran mayoria
        /// de los punteros devueltos (heap, pila, memoria guest) caen una y otra vez en el mismo hueco.
        static GAP: std::cell::Cell<(u64, u64, u64)> = const { std::cell::Cell::new((0, 0, 0)) };
    }

    #[inline]
    fn tsc() -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() | 1 }
    }

    /// Relee /proc/self/maps: mapeos ejecutables respaldados por archivo (bibliotecas, vDSO, cache JIT de ART).
    /// Los anonimos ejecutables (JIT y trampolines del puente) no cuentan: no son funciones del host.
    pub fn refresh() {
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
        let mut v: Vec<(u64, u64)> = Vec::new();
        for l in maps.lines() {
            let mut it = l.splitn(6, ' ');
            let (range, perms) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
            let path = it.nth(3).unwrap_or("").trim_start();
            if perms.as_bytes().get(2) != Some(&b'x') || !(path.starts_with('/') || path == "[vdso]") {
                continue;
            }
            let Some((a, b)) = range.split_once('-') else { continue };
            let (Ok(a), Ok(b)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) else { continue };
            match v.last_mut() {
                Some(last) if last.1 == a => last.1 = b,
                _ => v.push((a, b)),
            }
        }
        v.sort_unstable();
        if v.len() > CAP {
            // Tope: se quedan los primeros CAP (un rango que falte se trata como "no es del host": su codigo no se
            // ejecuta nunca, solo deja de diagnosticarse como FILTRACION)
            if OVER.fetch_add(1, Relaxed) == 0 {
                log(&format!("mapa del host con {} rangos ejecutables: se consideran los primeros {}", v.len(), CAP));
            }
            v.truncate(CAP);
        }
        publish(&v);
        GEN.fetch_add(1, Relaxed);
        STAMP.store(tsc(), Relaxed);
    }

    /// Publica `v` en el bufer siguiente al actual.
    pub(super) fn publish(v: &[(u64, u64)]) {
        let _w = crate::monitor::lock(&WRITER);
        let i = (CUR.load(Relaxed) + 1) % NBUF;
        let s = &SNAPS[i];
        s.seq.fetch_add(1, Relaxed); // impar: escribiendose
        fence(Release);
        let n = v.len().min(CAP);
        for (k, &(a, b)) in v.iter().take(n).enumerate() {
            s.lo[k].store(a, Relaxed);
            s.hi[k].store(b, Relaxed);
        }
        s.len.store(n, Relaxed);
        s.seq.fetch_add(1, Release); // par: lista
        CUR.store(i, Release);
    }

    /// Busca `a` en la instantanea publicada: Ok si esta en codigo del host; Err((inicio, fin)) con el hueco que lo
    /// contiene si no. Sin bloqueos ni reservas (vale en un manejador de senal).
    pub(super) fn lookup(a: u64) -> Result<(), (u64, u64)> {
        loop {
            let s = &SNAPS[CUR.load(Acquire)];
            let q = s.seq.load(Acquire);
            if q & 1 != 0 {
                std::hint::spin_loop();
                continue; // se reutiliza justo ahora: CUR ya cambio o cambiara enseguida
            }
            let n = s.len.load(Relaxed).min(CAP);
            // primer rango cuyo fin es > a
            let (mut l, mut r) = (0usize, n);
            while l < r {
                let m = (l + r) / 2;
                if s.hi[m].load(Relaxed) <= a {
                    l = m + 1;
                } else {
                    r = m;
                }
            }
            let res = if l < n && s.lo[l].load(Relaxed) <= a {
                Ok(())
            } else {
                Err((if l == 0 { 0 } else { s.hi[l - 1].load(Relaxed) }, if l < n { s.lo[l].load(Relaxed) } else { u64::MAX }))
            };
            fence(Acquire);
            if s.seq.load(Relaxed) == q {
                return res;
            }
        }
    }

    extern "C" fn gen_cb(info: *mut c_void, size: usize, data: *mut c_void) -> i32 {
        // dl_phdr_info: addr(0) name(8) phdr(16) phnum(24) adds(32) subs(40); el enlazador la entrega alineada a 8
        if size >= 48 {
            unsafe { *(data as *mut u64) = (*(info as *const u64).add(4)).wrapping_add((*(info as *const u64).add(5)) << 32) };
        }
        1
    }

    /// Contador de cargas/descargas de bibliotecas del enlazador del host (u64::MAX si no esta disponible).
    fn lib_generation() -> u64 {
        let mut g = u64::MAX;
        unsafe { dl_iterate_phdr(gen_cb, &mut g as *mut u64 as *mut c_void) };
        g
    }

    /// `a` esta en un mapeo ejecutable del host?
    /// Camino rapido (sin bloqueos): el hueco recordado por el hilo, mientras el mapa tenga menos de ~1 s.
    /// Pasado ese tiempo se pregunta al enlazador si cargo o descargo algo y solo entonces se relee el mapa.
    pub fn is_exec(a: u64) -> bool {
        let st = STAMP.load(Relaxed);
        let fresh = st != 0 && tsc().wrapping_sub(st) < STALE;
        if fresh {
            let (lo, hi, gen) = GAP.with(|g| g.get());
            if a >= lo && a < hi && gen == GEN.load(Relaxed) {
                return false;
            }
        } else {
            let g = lib_generation();
            if st != 0 && g != u64::MAX && g == LIBGEN.load(Relaxed) {
                STAMP.store(tsc(), Relaxed);
            } else {
                LIBGEN.store(g, Relaxed);
                refresh();
            }
        }
        match lookup(a) {
            Ok(()) => true,
            Err((lo, hi)) => {
                GAP.with(|g| g.set((lo, hi, GEN.load(Relaxed))));
                false
            }
        }
    }

    /// Como `is_exec`, sin releer el mapa (manejadores de senal): la ultima instantanea publicada, sin bloqueos.
    pub fn is_exec_sig(a: u64) -> bool {
        lookup(a).is_ok()
    }

    /// Como `is_exec`, pero sin ventana de 1 s: relee siempre que falle. Solo para caminos lentos.
    pub fn is_exec_precise(a: u64) -> bool {
        if lookup(a).is_ok() {
            return true;
        }
        refresh();
        lookup(a).is_ok()
    }
}

/// Invalida el mapa de codigo del host (tras cargar una biblioteca del host).
pub fn host_code_changed() {
    hostmap::refresh();
}

/// `a` es codigo del host (biblioteca del host o un trampolin del puente)? Las direcciones del guest (modulos,
/// regiones que creo el guest, slots HLE) nunca lo son. Camino lento (compilacion de bloque, validaciones).
pub fn is_host_code(a: u64) -> bool {
    let a = a & TBI;
    if a < 0x10000 || hle::is_hle(a) || crate::elf::is_guest_code(a) {
        return false;
    }
    // el mapa real del proceso manda sobre lo que el guest haya "pedido" con mprotect
    crate::cbthunk::is_thunk(a) || hostmap::is_exec_precise(a)
}

/// Como `is_host_code`, valida en un manejador de senal: sin bloqueos, sin reservas y sin releer /proc (la ultima
/// instantanea publicada del mapa). Los modulos guest nunca se mapean ejecutables, asi que no hace falta consultar su
/// lista (que tiene bloqueo): una direccion guest nunca esta en el mapa del host.
pub fn is_host_code_sig(a: u64) -> bool {
    let a = a & TBI;
    if a < 0x10000 || hle::is_hle(a) {
        return false;
    }
    crate::cbthunk::is_thunk(a) || hostmap::is_exec_sig(a)
}

/// Como `is_guest_executable`, valida en un manejador de senal: sin esperar bloqueos ni reservar memoria y sin releer
/// el mapa del host. Reintenta un poco si otro hilo tiene una lista tomada (carga de un modulo, mprotect); si sigue
/// tomada (o la tiene el propio hilo interrumpido) responde `false`, el lado seguro.
pub fn is_guest_executable_sig(a: u64) -> bool {
    let a = a & TBI;
    if a < 0x10000 || crate::cbthunk::is_thunk(a) {
        return false;
    }
    if hle::is_hle(a) {
        return true;
    }
    for _ in 0..1000 {
        let code = crate::elf::is_guest_code_try(a);
        if code == Some(true) {
            return true;
        }
        let ex = crate::mem::is_guest_exec_try(a);
        if ex == Some(false) && code == Some(false) {
            return false;
        }
        if ex == Some(true) {
            return !hostmap::is_exec_sig(a);
        }
        std::hint::spin_loop();
    }
    false
}

/// `a` es una direccion que el GUEST puede ejecutar (codigo de un modulo guest, region ejecutable que creo el
/// guest o slot HLE)? Es lo unico que el redireccionamiento de ejecucion acepta. Nunca codigo del host, aunque
/// el guest haya llamado a mprotect sobre el.
pub fn is_guest_executable(a: u64) -> bool {
    let a = a & TBI;
    if a < 0x10000 {
        return false;
    }
    hle::is_hle(a) || crate::elf::is_guest_code(a) || (crate::mem::is_guest_exec(a) && !crate::cbthunk::is_thunk(a) && !hostmap::is_exec(a))
}

/// Descripcion de una direccion del host: biblioteca!simbolo+desplazamiento, o trampolin.
pub fn host_describe(a: u64) -> String {
    if crate::cbthunk::is_thunk(a) {
        return format!("trampolin->{}", crate::cbthunk::guest_of(a).map(crate::elf::describe_addr).unwrap_or_default());
    }
    let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
    unsafe {
        if dladdr(a as *const c_void, &mut info) == 0 {
            return format!("{:#x}(sin dladdr)", a);
        }
        let cs = |p: *const c_char| if p.is_null() { String::from("?") } else { CStr::from_ptr(p).to_string_lossy().into_owned() };
        let base = cs(info.fname).rsplit('/').next().unwrap_or("?").to_string();
        if info.sname.is_null() {
            format!("{}+{:#x}", base, a.wrapping_sub(info.fbase as u64))
        } else {
            format!("{}!{}+{:#x}", base, cs(info.sname), a.wrapping_sub(info.saddr as u64))
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Politica de bibliotecas del host
// ---------------------------------------------------------------------------------------------

/// Bibliotecas NDK del host cuyas funciones se reenvian. Cerrado por defecto: un simbolo que solo exista en otra
/// biblioteca del proceso (libart, libc++, libandroid_runtime, HAL...) NO se entrega al guest. Criterio de
/// admision: API en C con escalares y punteros a datos; los callbacks y retornos de puntero a funcion quedan
/// cubiertos por los mecanismos de este modulo. OpenSL ES y OpenMAX AL (objetos con tablas de punteros a funcion del
/// host) se sirven a traves de los proxys de `proxy.rs`: de sus bibliotecas solo se reenvian los datos (`SL_IID_...`)
/// y las funciones directas de la tabla; `slCreateEngine`/`xaCreateEngine` entregan proxys. Fuera: C++ (retornos por
/// x8, clases por valor).
const ALLOWED_LIBS: &[&str] = &[
    "libc.so", "libm.so", "libdl.so", "liblog.so", "libstdc++.so", "libandroid.so", "libEGL.so", "libGLESv1_CM.so", "libGLESv2.so",
    "libGLESv3.so", "libnativewindow.so", "libjnigraphics.so", "libz.so", "libmediandk.so", "libaaudio.so", "libcamera2ndk.so",
    "libsync.so", "libnativehelper.so", "libbinder_ndk.so", "libicu.so", "libvulkan.so", "libOpenSLES.so", "libOpenMAXAL.so",
];
/// Fuera de Android (pruebas, heddle-run) las mismas API viven en estas bibliotecas del host.
#[cfg(not(target_os = "android"))]
const LINUX_LIBS: &[&str] = &["libc.so.6", "libm.so.6", "libdl.so.2", "libpthread.so.0", "librt.so.1", "libz.so.1"];
/// Fuera de Android, despues del espacio global (donde estan los simulacros del NDK de las pruebas con LD_PRELOAD):
/// el cargador de Vulkan del host, si existe (difftests tvk, con el ICD por software lavapipe). Se abre al primer uso.
#[cfg(not(target_os = "android"))]
const LINUX_LAST: &[&str] = &["libvulkan.so.1"];

/// Vulkan a traves del puente: activado por defecto; `debug.heddle.vulkan=0` (propiedad de Android) o
/// HEDDLE_VULKAN=0 lo desactivan (el guest no ve libvulkan.so y la app cae a GLES).
pub fn vulkan_enabled() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        if let Ok(v) = std::env::var("HEDDLE_VULKAN") {
            return v != "0";
        }
        prop("debug.heddle.vulkan").map_or(true, |v| v != "0")
    })
}

/// Propiedad del sistema de Android (None fuera de Android o si no existe).
pub fn prop(name: &str) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        extern "C" {
            fn __system_property_get(name: *const c_char, value: *mut c_char) -> i32;
        }
        let cn = CString::new(name).ok()?;
        let mut buf = [0 as c_char; 96];
        let n = unsafe { __system_property_get(cn.as_ptr(), buf.as_mut_ptr()) };
        if n <= 0 {
            return None;
        }
        Some(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = name;
        None
    }
}

/// La biblioteca del host `base` (nombre sin ruta) puede servir simbolos al guest?
pub fn lib_allowed(base: &str) -> bool {
    if base == "libvulkan.so" && !vulkan_enabled() {
        return false;
    }
    ALLOWED_LIBS.contains(&base)
}

/// Bibliotecas del sistema que el puente sirve al guest, en el orden de la lista de permitidas.
pub fn allowed_libs() -> impl Iterator<Item = &'static str> {
    ALLOWED_LIBS.iter().copied().filter(|l| lib_allowed(l))
}

/// Nombre canonico (el de la lista de permitidas) de la biblioteca del host `base`, si puede servir al guest.
pub fn allowed_lib_name(base: &str) -> Option<&'static str> {
    if lib_allowed(base) {
        ALLOWED_LIBS.iter().copied().find(|&n| n == base)
    } else {
        None
    }
}

fn lib_handles() -> &'static Vec<(String, usize)> {
    static H: std::sync::OnceLock<Vec<(String, usize)>> = std::sync::OnceLock::new();
    H.get_or_init(|| {
        #[cfg(target_os = "android")]
        let names = ALLOWED_LIBS;
        #[cfg(not(target_os = "android"))]
        let names = LINUX_LIBS;
        let v: Vec<(String, usize)> = names
            .iter()
            .filter(|l| cfg!(not(target_os = "android")) || lib_allowed(l))
            .filter_map(|l| {
                let cn = CString::new(*l).unwrap();
                let h = unsafe { dlopen(cn.as_ptr(), RTLD_NOW) };
                if h.is_null() {
                    None
                } else {
                    Some((l.to_string(), h as usize))
                }
            })
            .collect();
        hostmap::refresh();
        v
    })
}

static SYMS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);
static DENIED_LOGS: AtomicU32 = AtomicU32::new(0);

/// Ruta y base de la biblioteca del host que contiene `p` (`dladdr` del host), para el `dladdr` del guest sobre una
/// funcion del sistema: en Android es la ruta del dispositivo (/apex/.../libc.so, /system/lib64/...).
pub fn host_lib_of(p: u64) -> Option<(String, u64)> {
    let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
    unsafe {
        if p == 0 || dladdr(p as *const c_void, &mut info) == 0 || info.fname.is_null() {
            return None;
        }
        Some((CStr::from_ptr(info.fname).to_string_lossy().into_owned(), info.fbase as u64))
    }
}

/// Biblioteca (nombre sin ruta) que contiene la direccion del host `p`.
fn owner_lib(p: usize) -> Option<String> {
    let mut info = DlInfo { fname: std::ptr::null(), fbase: std::ptr::null_mut(), sname: std::ptr::null(), saddr: std::ptr::null_mut() };
    unsafe {
        if dladdr(p as *const c_void, &mut info) == 0 || info.fname.is_null() {
            return None;
        }
        Some(CStr::from_ptr(info.fname).to_string_lossy().rsplit('/').next().unwrap_or("").to_string())
    }
}

/// Direccion de un simbolo (funcion o dato) exportado por una biblioteca permitida; 0 si no existe o no se permite.
pub fn symbol_addr(name: &str) -> usize {
    if let Some(&p) = crate::monitor::lock(&SYMS).get_or_insert_with(HashMap::new).get(name) {
        return p;
    }
    let Ok(cn) = CString::new(name) else { return 0 };
    let mut found = 0usize;
    let mut denied: Option<String> = None;
    // En Android solo se consultan las bibliotecas permitidas, y se confirma (dladdr) que el simbolo vive en una de
    // ellas: dlsym(handle) tambien recorre dependencias (libc++...). Fuera de Android (pruebas, heddle-run) se usa
    // ademas el espacio global.
    for (_, h) in lib_handles().iter() {
        let p = unsafe { dlsym(*h as *mut c_void, cn.as_ptr()) } as usize;
        if p == 0 {
            continue;
        }
        match owner_lib(p) {
            Some(o) if lib_allowed(&o) || cfg!(not(target_os = "android")) => {
                found = p;
                break;
            }
            o => denied = Some(o.unwrap_or_else(|| String::from("(biblioteca desconocida)"))),
        }
    }
    if found == 0 && cfg!(not(target_os = "android")) {
        found = unsafe { dlsym(RTLD_DEFAULT, cn.as_ptr()) } as usize;
    }
    #[cfg(not(target_os = "android"))]
    if found == 0 {
        static LAST: std::sync::OnceLock<Vec<usize>> = std::sync::OnceLock::new();
        let hs = LAST.get_or_init(|| {
            LINUX_LAST
                .iter()
                .filter_map(|l| {
                    let cn = CString::new(*l).unwrap();
                    let h = unsafe { dlopen(cn.as_ptr(), RTLD_NOW) } as usize;
                    (h != 0).then_some(h)
                })
                .collect()
        });
        found = hs.iter().map(|&h| unsafe { dlsym(h as *mut c_void, cn.as_ptr()) } as usize).find(|&p| p != 0).unwrap_or(0);
    }
    if found == 0 {
        if let Some(o) = denied {
            if DENIED_LOGS.fetch_add(1, Relaxed) < 100 {
                log(&format!("simbolo '{}' solo existe en '{}' (biblioteca no permitida): no se entrega al guest", name, o));
            }
        }
    }
    let mut g = crate::monitor::lock(&SYMS);
    let m = g.as_mut().unwrap();
    if m.len() < crate::mem::TABLE_CAP {
        m.insert(name.to_string(), found);
    }
    drop(g);
    found
}

fn log(m: &str) {
    #[cfg(target_os = "android")]
    crate::bridge::alog(m);
    #[cfg(not(target_os = "android"))]
    let _ = m;
}

/// Como `log`, con prioridad ERROR y escritura sincrona (ver `bridge::alog_fatal`).
fn log_fatal(m: &str) {
    #[cfg(target_os = "android")]
    crate::bridge::alog_fatal(m);
    #[cfg(not(target_os = "android"))]
    let _ = m;
}

// ---------------------------------------------------------------------------------------------
// Llamadas guest -> host
// ---------------------------------------------------------------------------------------------


/// Llama a la funcion del host `f` con los argumentos de la llamada guest en curso (mapeo universal AAPCS64 ->
/// SysV: x0..x5 -> registros, x6, x7 y 8 palabras de la pila guest -> pila, v0..v7 -> xmm0..7) y deja el
/// resultado en x0/x1/v0. Sanea argumentos y retorno. `arg0` sustituye a x0 (JNIEnv real).
pub fn call_host(c: &mut Cpu, f: HostFn, arg0: Option<u64>) {
    let mut call = Call::new(f);
    // Los argumentos pasan TAL CUAL: su tipo no se conoce y cualquiera puede ser un dato (ver invariante 5).
    for i in 0..6 {
        call.ints[i] = c.x[i];
    }
    if let Some(a) = arg0 {
        call.ints[0] = a;
    }
    for i in 0..8 {
        call.fps[i] = c.v[i][0];
    }
    let mut st: [u64; 10] = [0; 10];
    st[0] = c.x[6];
    st[1] = c.x[7];
    let sp = c.x[31] as *const u64;
    if !sp.is_null() {
        for i in 0..8 {
            st[2 + i] = unsafe { *sp.add(i) };
        }
    }
    call.nstack = 10;
    call.stack = st.as_ptr() as u64;
    unsafe { hostcall::call(&mut call) };
    // El retorno pasa TAL CUAL: no tiene tipo. Un entero puede coincidir con la direccion de una funcion del host
    // (el guest que lee /proc/self/maps con strtoull) y convertirlo corrompe datos. Los punteros a funcion solo se
    // convierten donde el tipo se conoce: RET_FN y JNI.
    c.x[0] = call.out_rax;
    c.x[1] = call.out_rdx;
    c.v[0] = [call.out_xmm0, 0];
}

/// Llamada al host con argumentos explicitos (implementaciones HLE que convierten tipos). Devuelve (rax, xmm0).
/// `None` (simbolo ausente) no llama a nada y devuelve ceros.
pub fn call_raw(f: Option<HostFn>, ints: &[u64], fps: &[u64]) -> (u64, u64) {
    let Some(f) = f else { return (0, 0) };
    let mut call = Call::new(f);
    for (i, v) in ints.iter().take(6).enumerate() {
        call.ints[i] = *v;
    }
    for (i, v) in fps.iter().take(8).enumerate() {
        call.fps[i] = *v;
    }
    unsafe { hostcall::call(&mut call) };
    (call.out_rax, call.out_xmm0)
}

/// Llamada al host con los seis argumentos enteros explicitos. Devuelve (rax, rdx, xmm0).
pub fn call_ints(f: HostFn, ints: [u64; 6]) -> (u64, u64, u64) {
    let mut call = Call::new(f);
    call.ints = ints;
    unsafe { hostcall::call(&mut call) };
    (call.out_rax, call.out_rdx, call.out_xmm0)
}

/// Llamada al host con los argumentos ya repartidos segun SysV x86-64 (proxys que conocen la firma): `ints` = rdi..r9,
/// `fps` = xmm0..7 y `stack` = los argumentos en pila en orden, una palabra de 8 bytes cada uno. Devuelve (rax, rdx,
/// xmm0).
pub fn call_host_args(f: HostFn, ints: &[u64; 6], fps: &[u64; 8], stack: &[u64]) -> (u64, u64, u64) {
    let mut call = Call::new(f);
    call.ints = *ints;
    call.fps = *fps;
    call.nstack = stack.len() as u64;
    call.stack = stack.as_ptr() as u64;
    unsafe { hostcall::call(&mut call) };
    (call.out_rax, call.out_rdx, call.out_xmm0)
}

/// Manejador HLE que reenvia al host.
pub fn forward_handler(f: HostFn) -> Handler {
    Box::new(move |c: &mut Cpu| {
        call_host(c, f, None);
        Ret::Return
    })
}

// ---------------------------------------------------------------------------------------------
// Punteros a funcion: host -> guest
// ---------------------------------------------------------------------------------------------

/// Funciones que devuelven punteros a codigo del host pedidos por nombre (el retorno se etiqueta con ese nombre).
pub const RET_FN: &[&str] =
    &["eglGetProcAddress", "vkGetInstanceProcAddr", "vkGetDeviceProcAddr", "glXGetProcAddress", "glXGetProcAddressARB", "vkGetPhysicalDeviceProcAddr", "vk_icdGetInstanceProcAddr"];

static SLOTS: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);

/// Slot HLE (direccion guest) que representa a la funcion del host `f`. Un slot por funcion (cache). Con nombre
/// (pedido por GetProcAddress) y firma conocida (tabla generada: `sigs::SIGS` directa o `sigs::VK_FNS`), el slot hace
/// las mismas conversiones tipadas que si el guest la hubiera importado por ese nombre.
fn guest_slot_for(f: HostFn, name: &str) -> u64 {
    if let Some(&a) = crate::monitor::lock(&SLOTS).get_or_insert_with(HashMap::new).get(&f.0) {
        return a;
    }
    let conv = if name.is_empty() { None } else { Conv::of(name) };
    let a = if RET_FN.contains(&name) {
        hle::register(&format!("hostfn@{:x}:{}", f.0, name), ret_fn_handler(name, f))
    } else if name.is_empty() {
        hle::register(&format!("hostfn@{:x}", f.0), forward_handler(f))
    } else if let Some(cv) = conv.filter(|c| !c.is_plain()) {
        hle::register(&format!("hostfn@{:x}:{}", f.0, name), conv_handler(f, cv))
    } else {
        // con nombre (pedido por GetProcAddress): se registran las primeras llamadas y una de cada 50000
        let nm = name.to_string();
        let n = AtomicU64::new(0);
        hle::register(
            &format!("hostfn@{:x}:{}", f.0, name),
            Box::new(move |c: &mut Cpu| {
                let k = n.fetch_add(1, Relaxed);
                let noisy = k < 3 || k % 50000 == 0;
                if noisy {
                    log(&format!("llamada {} #{} x0={:#x} x1={:#x} x2={:#x} x3={:#x} lr={}", nm, k, c.x[0], c.x[1], c.x[2], c.x[3], crate::elf::describe_addr(c.x[30])));
                }
                call_host(c, f, None);
                if noisy {
                    log(&format!("  {} #{} -> x0={:#x}", nm, k, c.x[0]));
                }
                Ret::Return
            }),
        )
    };
    // con la region agotada `a` es el slot comun: no se cachea (la tabla no debe crecer sin slots nuevos)
    if hle::exhausted_slot() != Some(a) {
        hle::set_host_twin(a, f.0 as u64);
        crate::monitor::lock(&SLOTS).as_mut().unwrap().insert(f.0, a);
    }
    a
}

/// Puntero a funcion entregado por el host -> direccion que el guest puede llamar. 0 y direcciones guest se dejan
/// igual; un trampolin del puente vuelve a ser su funcion guest; lo que no sea codigo del host se devuelve tal
/// cual (es un dato).
pub fn host_fn_to_guest(p: u64) -> u64 {
    host_fn_to_guest_named(p, "")
}

pub fn host_fn_to_guest_named(p: u64, name: &str) -> u64 {
    if p == 0 || hle::is_hle(p) {
        return p;
    }
    if let Some(g) = crate::cbthunk::guest_of(p) {
        return g;
    }
    match HostFn::from_host(p) {
        Some(f) => guest_slot_for(f, name),
        None => p,
    }
}

/// Manejador de una funcion tipo GetProcAddress: el puntero devuelto se entrega como slot con el nombre pedido.
fn ret_fn_handler(name: &str, f: HostFn) -> Handler {
    let idx = if name.starts_with("vk") { 1 } else { 0 };
    Box::new(move |c: &mut Cpu| {
        let req = c.x[idx];
        let reqname = if req > 0x10000 { unsafe { CStr::from_ptr(req as *const c_char).to_string_lossy().into_owned() } } else { String::new() };
        let mut call = Call::new(f);
        for i in 0..6 {
            call.ints[i] = c.x[i];
        }
        unsafe { hostcall::call(&mut call) };
        c.x[0] = host_fn_to_guest_named(call.out_rax, &reqname);
        Ret::Return
    })
}

/// Conversiones tipadas de una funcion del host (salen de la tabla generada, ver sigs.rs):
///  - `env`: mascara de argumentos que son JNIEnv*/JavaVM* -> se pasa el del host;
///  - `cbs`: argumentos que son punteros a funcion guest -> trampolin con el retorno declarado;
///  - `narrow`: argumentos bool/char/short -> se extienden (el host x86-64 asume que vienen extendidos);
///  - `ret_fn`: devuelve un puntero a funcion del host -> slot guest;
///  - `copied`: estructuras de callbacks que el host COPIA durante la llamada (`sigs::STRUCT_COPIED`) -> se le pasa
///    una copia con cada puntero a funcion convertido en trampolin con su firma (el original del guest no se toca);
///  - `inplace`: estructuras que el host GUARDA por su direccion (`sigs::STRUCT_INPLACE`, zlib) -> sus punteros a
///    funcion se sustituyen en su sitio durante la llamada y se reponen al volver (ver `swap_in_place`);
///  - `vk`: Vulkan (`pAllocator` y cadenas `pNext` de las estructuras de entrada, ver vk.rs).
/// Ningun otro argumento ni el retorno se tocan.
#[derive(Clone, Copy)]
pub struct Conv {
    pub env: u8,
    pub cbs: &'static [(u8, u8)],
    pub narrow: &'static [(u8, u8)],
    pub ret_fn: bool,
    pub copied: &'static [crate::sigs::StructCb],
    pub inplace: &'static [crate::sigs::StructCb],
    pub vk: Option<&'static crate::sigs::VkFn>,
}

impl Conv {
    pub const NONE: Conv = Conv { env: 0, cbs: &[], narrow: &[], ret_fn: false, copied: &[], inplace: &[], vk: None };

    /// Sin ninguna conversion: reenvio directo.
    pub fn is_plain(&self) -> bool {
        self.env == 0 && self.cbs.is_empty() && self.narrow.is_empty() && !self.ret_fn && self.copied.is_empty() && self.inplace.is_empty() && self.vk.is_none()
    }

    /// Conversiones de la funcion `name` segun la tabla generada (None si no es una funcion directa del NDK ni de
    /// Vulkan). Las listas escritas a mano (`STRUCT_COPIED`, `STRUCT_INPLACE`) dicen que estructuras se convierten.
    pub fn of(name: &str) -> Option<Conv> {
        use crate::sigs::K;
        let vk = crate::sigs::vk_fn(name);
        let mut cv = match crate::sigs::lookup(name).map(|s| s.k) {
            Some(K::Direct { env, cbs, narrow, ret_fn, .. }) => Conv { env, cbs, narrow, ret_fn, ..Conv::NONE },
            // extensiones de Vulkan: no las exporta libvulkan (solo vkGet*ProcAddr), pero su firma esta en las cabeceras
            None if vk.is_some() || name.starts_with("vk") => Conv::NONE,
            _ => return None,
        };
        cv.vk = vk;
        let scbs = crate::sigs::struct_cbs(name);
        if crate::sigs::STRUCT_COPIED.contains(&name) {
            cv.copied = scbs;
        } else if !scbs.is_empty() && scbs.iter().all(|s| crate::sigs::STRUCT_INPLACE.contains(&s.ty) && s.note.is_empty() && !s.byval) {
            cv.inplace = scbs;
        }
        Some(cv)
    }
}

/// Slot HLE de la funcion `name` del host con sus conversiones tipadas (ver `Conv`). Para un llamador del host el slot
/// equivale a `f` (`hle::set_host_twin`).
pub fn typed_slot(name: &str, f: HostFn, conv: Conv) -> u64 {
    let a = if conv.ret_fn && RET_FN.contains(&name) {
        // familia GetProcAddress: ademas se etiqueta el slot con el nombre pedido
        hle::register(name, ret_fn_handler(name, f))
    } else if conv.is_plain() {
        hle::register(name, forward_handler(f))
    } else {
        hle::register(name, conv_handler(f, conv))
    };
    if hle::exhausted_slot() != Some(a) {
        hle::set_host_twin(a, f.0 as u64);
    }
    a
}

/// Manejador que aplica las conversiones `conv` y llama a `f`.
fn conv_handler(f: HostFn, conv: Conv) -> Handler {
    Box::new(move |c: &mut Cpu| {
        for &(i, r) in conv.cbs {
            c.x[i as usize] = to_host_callable(c.x[i as usize], r, 0);
        }
        for &(i, k) in conv.narrow {
            let v = c.x[i as usize];
            c.x[i as usize] = match k {
                1 => v & 0xff,
                2 => v as i8 as i64 as u64,
                3 => v & 0xffff,
                _ => v as i16 as i64 as u64,
            };
        }
        for i in 0..6 {
            if conv.env >> i & 1 != 0 && crate::jni::is_guest_wrapper(c.x[i]) {
                c.x[i] = crate::jni::unwrap_to_host(c.x[i]);
            }
        }
        if conv.copied.is_empty() && conv.inplace.is_empty() && conv.vk.is_none() {
            call_host(c, f, None);
        } else {
            call_with_structs(c, f, &conv);
        }
        if conv.ret_fn {
            c.x[0] = host_fn_to_guest(c.x[0]);
        }
        Ret::Return
    })
}

/// La llamada con estructuras convertidas: copias en la pila de este manejador (el host las copia), sustitucion en su
/// sitio (zlib) y Vulkan.
fn call_with_structs(c: &mut Cpu, f: HostFn, conv: &Conv) {
    // caminos separados: cada uno solo prepara (y pone a cero) lo que usa
    if conv.copied.is_empty() && conv.vk.is_none() {
        let mut saved = InPlace::new();
        for sc in conv.inplace {
            let r = sc.reg as usize;
            if r < 8 {
                saved.swap(c.x[r], sc);
            }
        }
        call_host(c, f, None);
        saved.restore();
        return;
    }
    if conv.copied.is_empty() && conv.inplace.is_empty() {
        let mut keep = crate::vk::Keep::new();
        if let Some(vf) = conv.vk {
            crate::vk::convert_args(c, vf, &mut keep);
        }
        call_host(c, f, None);
        return;
    }
    let mut bufs = [[0u64; STRUCT_COPY_WORDS]; 2];
    for (k, sc) in conv.copied.iter().take(2).enumerate() {
        let r = sc.reg as usize;
        if r < 6 {
            c.x[r] = copy_struct_callbacks(c.x[r], sc, &mut bufs[k]);
        }
    }
    let mut saved = InPlace::new();
    for sc in conv.inplace {
        let r = sc.reg as usize;
        if r < 8 {
            saved.swap(c.x[r], sc);
        }
    }
    let mut keep = crate::vk::Keep::new();
    if let Some(vf) = conv.vk {
        crate::vk::convert_args(c, vf, &mut keep);
    }
    call_host(c, f, None);
    saved.restore();
}

thread_local! {
    /// Guardas vivas que reponen memoria del guest al soltarse (`InPlace`, `vk::Keep`): mientras haya alguna, una
    /// senal no abandona la llamada al host en curso (se saltarian sus `Drop`).
    pub static NO_ABORT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Hay guardas vivas en este hilo (ver `NO_ABORT`). Sin bloqueos: vale en un manejador de senal.
pub fn abort_blocked() -> bool {
    NO_ABORT.try_with(|n| n.get() != 0).unwrap_or(true)
}

pub(crate) fn no_abort_enter() {
    let _ = NO_ABORT.try_with(|n| n.set(n.get() + 1));
}

pub(crate) fn no_abort_leave() {
    let _ = NO_ABORT.try_with(|n| n.set(n.get().saturating_sub(1)));
}

/// Punteros a funcion sustituidos en su sitio durante una llamada: (direccion del campo, valor del guest, valor del
/// host). Como mucho dos estructuras de cuatro campos (deflateCopy: destino y origen).
struct InPlace {
    n: usize,
    /// solo `e[..n]` esta escrito
    e: [std::mem::MaybeUninit<(u64, u64, u64)>; 8],
}

impl Drop for InPlace {
    fn drop(&mut self) {
        no_abort_leave();
    }
}

impl InPlace {
    fn new() -> Self {
        no_abort_enter();
        InPlace { n: 0, e: [const { std::mem::MaybeUninit::uninit() }; 8] }
    }

    /// Sustituye en la estructura `p` del guest cada puntero a funcion de `sc` por el que puede llamar el host.
    fn swap(&mut self, p: u64, sc: &crate::sigs::StructCb) {
        if p == 0 {
            return;
        }
        for fl in sc.fields {
            if self.n == self.e.len() || fl.off as usize + 8 > sc.size as usize {
                break;
            }
            let at = (p + fl.off as u64) as *mut u64;
            let v = unsafe { at.read_unaligned() };
            let h = if v == 0 { 0 } else { host_callable_cached(v, fl.sig) };
            if h != v {
                unsafe { at.write_unaligned(h) };
            }
            self.e[self.n].write((at as u64, v, h));
            self.n += 1;
        }
    }

    /// Repone lo que vera el guest: su valor si el host no lo cambio; si lo cambio (zlib escribe su `zcalloc` por
    /// defecto, `deflateCopy` copia el origen en el destino), el puntero del host convertido para el guest (slot, o la
    /// funcion guest de un trampolin).
    fn restore(&self) {
        for e in &self.e[..self.n] {
            // escrito en `swap`
            let (a, v, h) = unsafe { e.assume_init() };
            let at = a as *mut u64;
            let w = unsafe { at.read_unaligned() };
            if w == h {
                if h != v {
                    unsafe { at.write_unaligned(v) };
                }
            } else {
                unsafe { at.write_unaligned(host_fn_to_guest(w)) };
            }
        }
    }
}

/// Palabras como mucho de una estructura de callbacks copiada (la mayor del NDK tiene 80 bytes).
pub const STRUCT_COPY_WORDS: usize = 32;

/// Copia en `buf` la estructura de callbacks `p` del guest (disposicion identica en las dos arquitecturas: lo
/// comprueba el generador) con cada puntero a funcion de `sc.fields` convertido en un trampolin con su firma, y
/// devuelve la direccion de la copia. NULL, o una estructura que no cabe o no se puede convertir, se devuelve igual.
pub fn copy_struct_callbacks(p: u64, sc: &crate::sigs::StructCb, buf: &mut [u64; STRUCT_COPY_WORDS]) -> u64 {
    let words = (sc.size as usize + 7) / 8;
    if p == 0 || !sc.note.is_empty() || words > STRUCT_COPY_WORDS {
        return p;
    }
    unsafe { std::ptr::copy_nonoverlapping(p as *const u8, buf.as_mut_ptr() as *mut u8, sc.size as usize) };
    let base = buf.as_mut_ptr() as *mut u8;
    for fl in sc.fields {
        if fl.off as usize + 8 > sc.size as usize {
            continue;
        }
        unsafe {
            let at = base.add(fl.off as usize) as *mut u64;
            at.write_unaligned(to_host_callable_sig(at.read_unaligned(), fl.sig, 0));
        }
    }
    buf.as_ptr() as u64
}

// ---------------------------------------------------------------------------------------------
// Punteros a funcion: guest -> host
// ---------------------------------------------------------------------------------------------

/// Direccion guest (o slot HLE) que el guest entrega al host como callback -> puntero que el host puede llamar
/// de forma nativa. Nunca devuelve un slot HLE ni una direccion guest sin envolver:
///  - 0 y trampolines existentes se dejan igual (ya son llamables por el host);
///  - un slot que solo reenvia a una funcion del host X (`hostfn@X`, o el de una funcion importada por su nombre
///    sin implementacion propia: `hle::host_twin`) se desenvuelve a la X real;
///  - cualquier otro slot HLE o funcion guest se envuelve en un trampolin.
pub fn to_host_callable(v: u64, ret: u8, jni: u8) -> u64 {
    if v == 0 || crate::cbthunk::is_thunk(v) {
        return v;
    }
    if hle::is_hle(v) {
        // slot que equivale a una funcion del host (reenvio tipado, `hostfn@X`): el host recibe la funcion real
        let t = hle::host_twin(v);
        if t != 0 {
            return t;
        }
    }
    // Solo se envuelve lo que el guest puede ejecutar. Un valor que no es codigo guest (una constante como
    // SIG_IGN, un puntero a datos) se deja igual: no es un callback valido y no debe gastar un trampolin.
    if !is_guest_executable(v) {
        return v;
    }
    crate::cbthunk::make(v, ret, jni)
}

/// Como `to_host_callable`, para una funcion cuya firma se conoce (`shorty` de JNI, retorno primero): el trampolin
/// reparte los argumentos por la firma (SysV x86-64 -> AAPCS64).
pub fn to_host_callable_sig(v: u64, shorty: &[u8], jni: u8) -> u64 {
    if v == 0 || crate::cbthunk::is_thunk(v) || hle::is_hle(v) || !is_guest_executable(v) {
        return to_host_callable(v, shorty.first().copied().unwrap_or(b'J'), jni);
    }
    crate::cbthunk::make_sig(v, shorty, jni)
}

thread_local! {
    /// Ultimas conversiones de `host_callable_cached` en este hilo: (valor del guest, firma, valor del host).
    static CB_CACHE: [std::cell::Cell<(u64, usize, u64)>; 8] = const { [const { std::cell::Cell::new((0, 0, 0)) }; 8] };
}

/// `to_host_callable_sig(v, sig, 0)` con una cache por hilo de 8 entradas (zlib en cada `inflate`, VkAllocationCallbacks
/// en cada creacion). Solo se recuerdan las conversiones que cambian el valor: un slot equivalente o un trampolin es la
/// misma funcion mientras exista el proceso (un trampolin no se libera; el de una biblioteca descargada sigue llamando
/// a su direccion, como el puntero colgante en ARM). Un valor que no es codigo guest se reevalua cada vez.
pub fn host_callable_cached(v: u64, sig: &'static [u8]) -> u64 {
    let key = sig.as_ptr() as usize;
    let slot = ((v >> 2) as usize ^ key) & 7;
    if let Ok(e) = CB_CACHE.try_with(|c| c[slot].get()) {
        if e.0 == v && e.1 == key && v != 0 {
            return e.2;
        }
    }
    let h = to_host_callable_sig(v, sig, 0);
    if h != v && h != 0 {
        let _ = CB_CACHE.try_with(|c| c[slot].set((v, key, h)));
    }
    h
}

// ---------------------------------------------------------------------------------------------
// Violaciones de la frontera
// ---------------------------------------------------------------------------------------------

/// El guest salto (BR/BLR/RET) a una direccion del host. Solo es legitimo si es un trampolin del puente (vuelve a
/// su funcion guest). Cualquier otra es una filtracion de puntero: no se ejecuta; fallo con diagnostico.
pub fn guest_jumped_to_host(c: &mut Cpu) {
    let target = c.pc & TBI;
    if let Some(g) = crate::cbthunk::guest_of(target) {
        c.pc = g;
        return;
    }
    // El mapa del host se revalida con retraso (instantanea): la direccion pudo dejar de ser del host (biblioteca
    // descargada y rango reutilizado por el guest). Antes de declarar una FILTRACION se relee, salvo dentro de un
    // manejador de senal (releer /proc reserva memoria).
    if !crate::rt::cur_opt().is_some_and(|t| t.sig_depth > 0) {
        hostmap::refresh();
        if !crate::cbthunk::is_thunk(target) && !hostmap::is_exec_sig(target) {
            if is_guest_executable(target) {
                return; // codigo guest: el bucle lo ejecuta
            }
            // ni del host ni ejecutable para el guest: el fallo de un salto a memoria no ejecutable, sin FILTRACION
            let code = if crate::rt::addr_readable(target) { 2 } else { 1 }; // SEGV_ACCERR / SEGV_MAPERR
            crate::sig::guest_fault(c, 11, code, target, "salto a memoria no ejecutable", target);
            return;
        }
    }
    let msg = format!(
        "FILTRACION: el guest intento ejecutar codigo del host {} (lr={}, sp={:#x}, x0={:#x} x1={:#x} x2={:#x} x3={:#x}); no se ejecuta",
        host_describe(target),
        crate::elf::describe_addr(c.x[30]),
        c.x[31],
        c.x[0],
        c.x[1],
        c.x[2],
        c.x[3]
    );
    log_fatal(&msg);
    // como en ARM: SIGSEGV con el pc en la direccion no ejecutable (un manejador guest puede reanudar en otro sitio)
    crate::sig::guest_fault(c, 11, 2, target, "FILTRACION: salto a codigo del host", target);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Una direccion que la instantanea del mapa daba como codigo del host y que dejo de serlo (descargada y
    /// reutilizada por el guest como codigo) no es una FILTRACION: se relee el mapa y la ejecucion sigue ahi.
    #[test]
    fn filtracion_relee_el_mapa_antes_de_declararla() {
        use crate::sys::*;
        // un mapeo ejecutable respaldado por archivo (como una biblioteca del host): el propio ejecutable de la prueba
        let len = 1usize << 16;
        let f = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&f);
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_EXEC, MAP_PRIVATE, fd, 0) } as u64;
        assert_ne!(p as isize, -1);
        hostmap::refresh();
        assert!(hostmap::is_exec_sig(p), "la instantanea debe verlo como codigo del host");
        // se descarga y el guest mapea codigo en el mismo rango (sin PROT_EXEC en el host: el guest nunca se ejecuta)
        unsafe { munmap(p as *mut c_void, len) };
        let q = unsafe { mmap(p as *mut c_void, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) } as u64;
        assert_eq!(q, p);
        crate::mem::guest_prot(p, len as u64, true);
        assert!(hostmap::is_exec_sig(p), "sin releer, la instantanea vieja todavia lo da como del host");
        let mut c = crate::cpu::Cpu::new();
        c.pc = p + 0x40;
        guest_jumped_to_host(&mut c);
        assert_eq!(c.pc, p + 0x40, "debe seguir en el codigo guest, sin FILTRACION");
        assert!(!hostmap::is_exec_sig(p));
        crate::mem::guest_unmap(p, len as u64);
        unsafe { munmap(p as *mut c_void, len) };
    }

    extern "C" fn host_suma(a: u64, b: u64) -> u64 {
        a * 100 + b
    }

    extern "C" {
        fn atoi(s: *const c_char) -> i32;
    }

    /// devuelve un puntero a una funcion EXPORTADA del host (atoi de la libc)
    extern "C" fn devuelve_funcion() -> u64 {
        atoi as *const () as usize as u64
    }

    /// devuelve un entero de 64 bits que cae DENTRO de codigo del host sin ser el inicio de ninguna funcion
    /// (como una marca de tiempo o un resto en el registro)
    extern "C" fn devuelve_entero_que_parece_codigo() -> u64 {
        atoi as *const () as usize as u64 + 3
    }

    /// devuelve un puntero a una funcion NO exportada (de este binario de pruebas)
    extern "C" fn devuelve_funcion_interna() -> u64 {
        host_suma as *const () as usize as u64
    }

    extern "C" fn devuelve_dato() -> u64 {
        static D: u64 = 7;
        &D as *const u64 as u64
    }

    extern "C" fn fake_gpa(_a: u64, n: *const c_char) -> u64 {
        let nm = unsafe { CStr::from_ptr(n) }.to_bytes();
        if nm == b"vkGetDeviceProcAddr" {
            fake_gpa as *const () as usize as u64
        } else {
            host_suma as *const () as usize as u64
        }
    }

    fn hf(p: usize) -> HostFn {
        HostFn::from_host(p as u64).expect("codigo del host")
    }

    fn cpu() -> (Box<Cpu>, Vec<u64>) {
        let pila = vec![0u64; 64];
        let mut c = Cpu::new();
        c.x[31] = pila.as_ptr() as u64 + 256;
        (c, pila)
    }

    /// El mapa de codigo del host se lee sin bloqueos (instantaneas publicadas): mientras otro hilo lo relee sin
    /// parar, la deteccion no falla nunca, tampoco desde un manejador de senal (donde antes un bloqueo ocupado daba
    /// "no se sabe" y un salto a codigo del host no se detectaba en ese instante).
    #[test]
    fn mapa_del_host_sin_bloqueos_mientras_se_relee() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
        static STOP: AtomicBool = AtomicBool::new(false);
        static MALOS: AtomicU64 = AtomicU64::new(0);
        static EN_SENAL: AtomicU64 = AtomicU64::new(0);
        static DATO: u64 = 5;
        hostmap::refresh();
        let code = host_suma as usize as u64;
        let dato = &DATO as *const u64 as u64;
        assert!(is_host_code_sig(code) && !is_host_code_sig(dato));
        extern "C" fn h(_s: i32) {
            let ok = is_host_code_sig(host_suma as usize as u64) && !is_host_code_sig(&DATO as *const u64 as u64);
            if !ok {
                MALOS.fetch_add(1, SeqCst);
            }
            EN_SENAL.fetch_add(1, SeqCst);
        }
        // struct sigaction de la libc del host (x86-64): handler, sa_mask[128 bytes], sa_flags, sa_restorer
        #[repr(C)]
        struct Sa {
            h: usize,
            mask: [u64; 16],
            flags: u64,
            restorer: usize,
        }
        extern "C" {
            #[link_name = "sigaction"]
            fn libc_sigaction(s: i32, a: *const Sa, o: *mut Sa) -> i32;
        }
        const SIG: i32 = 46;
        let sa = Sa { h: h as *const () as usize, mask: [0; 16], flags: 0, restorer: 0 };
        let mut old = Sa { h: 0, mask: [0; 16], flags: 0, restorer: 0 };
        assert_eq!(unsafe { libc_sigaction(SIG, &sa, &mut old) }, 0);
        static RELECTURAS: AtomicU64 = AtomicU64::new(0);
        let w = std::thread::spawn(|| {
            while !STOP.load(SeqCst) {
                hostmap::refresh();
                RELECTURAS.fetch_add(1, SeqCst);
            }
        });
        let (pid, tid) = unsafe { (syscall(39), syscall(186)) };
        let mut i = 0u32;
        while i < 20_000 || RELECTURAS.load(SeqCst) < 50 {
            i += 1;
            assert!(is_host_code_sig(code), "codigo del host no detectado (iteracion {})", i);
            assert!(!is_host_code_sig(dato));
            if i % 64 == 0 {
                unsafe { syscall(234, pid, tid, SIG as i64) };
            }
        }
        STOP.store(true, SeqCst);
        w.join().unwrap();
        unsafe { libc_sigaction(SIG, &old, std::ptr::null_mut()) };
        assert!(EN_SENAL.load(SeqCst) > 0);
        assert_eq!(MALOS.load(SeqCst), 0, "fallo de deteccion dentro del manejador de senal");
    }

    #[test]
    fn hostfn_solo_acepta_codigo_del_host() {
        hle::init();
        assert!(HostFn::from_host(host_suma as *const () as usize as u64).is_some());
        assert!(HostFn::from_host(0).is_none());
        assert!(HostFn::from_host(42).is_none());
        let dato = Box::new(0u64);
        assert!(HostFn::from_host(&*dato as *const u64 as u64).is_none(), "un dato no es codigo del host");
        let slot = hle::register("prueba_frontera_slot", Box::new(|_c| Ret::Return));
        assert!(HostFn::from_host(slot).is_none(), "un slot HLE no es codigo del host");
    }

    #[test]
    fn el_retorno_del_reenvio_universal_no_se_altera() {
        hle::init();
        // Un entero devuelto por el host puede valer exactamente la direccion de una funcion exportada (strtoull
        // sobre /proc/self/maps), caer dentro de codigo, o ser una funcion interna: en ningun caso se cambia.
        for (nombre, f, esperado) in [
            ("prueba_ret_exportada", devuelve_funcion as *const () as usize, atoi as *const () as usize as u64),
            ("prueba_ret_dentro", devuelve_entero_que_parece_codigo as *const () as usize, atoi as *const () as usize as u64 + 3),
            ("prueba_ret_interna", devuelve_funcion_interna as *const () as usize, host_suma as *const () as usize as u64),
        ] {
            let slot = hle::register(nombre, forward_handler(hf(f)));
            let (mut c, _p) = cpu();
            c.pc = slot;
            hle::dispatch(&mut c);
            assert_eq!(c.x[0], esperado, "{}: el reenvio universal altero un valor devuelto por el host", nombre);
            // y aunque sea codigo del host, el guest no puede ejecutarlo
            assert!(is_host_code(c.x[0]) && !is_guest_executable(c.x[0]));
        }
        // la conversion existe solo por la via tipada
        let s = host_fn_to_guest(atoi as *const () as usize as u64);
        assert!(hle::is_hle(s));
        let (mut c2, _p2) = cpu();
        c2.x[0] = b"42\0".as_ptr() as u64;
        c2.pc = s;
        hle::dispatch(&mut c2);
        assert_eq!(c2.x[0] as u32, 42);
    }

    #[test]
    fn retorno_de_dato_no_se_toca() {
        hle::init();
        let slot = hle::register("prueba_devuelve_dato", forward_handler(hf(devuelve_dato as *const () as usize)));
        let (mut c, _p) = cpu();
        c.pc = slot;
        hle::dispatch(&mut c);
        assert!(!hle::is_hle(c.x[0]));
        assert_eq!(unsafe { *(c.x[0] as *const u64) }, 7);
    }

    #[test]
    fn getprocaddr_indirecto_no_filtra_punteros_del_host() {
        hle::init();
        let slot = hle::register("vkGetInstanceProcAddr@prueba", ret_fn_handler("vkGetInstanceProcAddr", hf(fake_gpa as *const () as usize)));
        let (mut c, _p) = cpu();
        c.x[1] = b"vkGetDeviceProcAddr\0".as_ptr() as u64;
        c.pc = slot;
        hle::dispatch(&mut c);
        let gdpa = c.x[0];
        assert!(hle::is_hle(gdpa));
        let (mut c2, _p2) = cpu();
        c2.x[1] = b"vkQueueSubmit\0".as_ptr() as u64;
        c2.pc = gdpa;
        hle::dispatch(&mut c2);
        assert!(hle::is_hle(c2.x[0]), "segundo nivel filtrado");
    }

    #[test]
    fn trampolin_devuelto_por_el_host_vuelve_a_ser_guest() {
        hle::init();
        let g = hle::register("prueba_fn_guest", Box::new(|_c| Ret::Return));
        let t = crate::cbthunk::make(g, b'J', 0);
        assert_eq!(host_fn_to_guest(t), g);
        assert_eq!(to_host_callable(t, b'J', 0), t, "sin doble envoltorio");
        assert!(crate::cbthunk::is_thunk(to_host_callable(g, b'J', 0)));
        assert_eq!(to_host_callable(0, b'V', 0), 0);
        // un slot hostfn@X se desenvuelve a la funcion real
        let s = host_fn_to_guest(host_suma as *const () as usize as u64);
        assert_eq!(to_host_callable(s, b'J', 0), host_suma as *const () as usize as u64);
        // un valor que no es codigo guest (constante, puntero a datos) no se envuelve ni gasta un trampolin
        assert_eq!(to_host_callable(1, b'V', 0), 1);
        let dato = Box::new(0u64);
        let d = &*dato as *const u64 as u64;
        assert_eq!(to_host_callable(d, b'V', 0), d);
        // la parte libre de la arena de trampolines no es un trampolin
        assert_eq!(crate::cbthunk::guest_of(t + (512 << 10)), None);
    }

    extern "C" fn devuelve_entero() -> u64 {
        5
    }

    extern "C" fn devuelve_heap() -> u64 {
        static mut B: [u8; 64] = [0; 64];
        std::ptr::addr_of!(B) as u64
    }

    /// Medicion del costo de la frontera (cargo test --release costo_de_la_frontera -- --ignored --nocapture).
    #[test]
    #[ignore]
    fn costo_de_la_frontera() {
        hle::init();
        let n = 5_000_000u64;
        let medir = |f: HostFn, sane: bool| {
            let (mut c, _p) = cpu();
            let t = std::time::Instant::now();
            for _ in 0..n {
                if sane {
                    call_host(&mut c, f, None);
                } else {
                    let mut call = Call::new(f);
                    call.ints = [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], c.x[5]];
                    unsafe { hostcall::call(&mut call) };
                    c.x[0] = call.out_rax;
                }
            }
            t.elapsed().as_nanos() as f64 / n as f64
        };
        let (fi, fh) = (hf(devuelve_entero as *const () as usize), hf(devuelve_heap as *const () as usize));
        let base = medir(fi, false);
        println!("llamada al host sin frontera:            {:.1} ns", base);
        println!("con frontera, retorno entero pequeno:    {:.1} ns", medir(fi, true));
        println!("con frontera, retorno puntero a datos:   {:.1} ns", medir(fh, true));
        let t = std::time::Instant::now();
        for _ in 0..200 {
            hostmap::refresh();
        }
        println!("releer el mapa de memoria (este proceso): {:.0} us", t.elapsed().as_micros() as f64 / 200.0);
    }

    extern "C" fn vk_nada(_a: u64, _b: u64, _c: u64) -> u64 {
        0
    }

    /// Coste de la frontera en funciones de Vulkan con y sin estructuras que pueden llevar callbacks (cargo test
    /// --release --lib bench_vk_frontera -- --ignored --nocapture --test-threads=1).
    #[test]
    #[ignore]
    fn bench_vk_frontera() {
        hle::init();
        let f = hf(vk_nada as *const () as usize);
        let n = 5_000_000u64;
        // VkRenderPassBeginInfo (64 bytes) sin cadena; VkInstanceCreateInfo con una cadena sin callbacks
        let rp = [43u64, 0, 0, 0, 0, 0, 0, 0];
        let feat = [1000059000u64, 0, 0, 0];
        let ici = [1u64, feat.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let medir = |slot: u64, x0: u64, x1: u64| {
            let (mut c, _p) = cpu();
            let t = std::time::Instant::now();
            for _ in 0..n {
                c.x[0] = x0;
                c.x[1] = std::hint::black_box(x1);
                c.x[2] = 0;
                c.x[30] = 0x1000;
                c.pc = slot;
                hle::dispatch(&mut c);
            }
            t.elapsed().as_nanos() as f64 / n as f64
        };
        let plano = typed_slot("bench_vk_plano", f, Conv::NONE);
        let rps = typed_slot("vkCmdBeginRenderPass", f, Conv::of("vkCmdBeginRenderPass").unwrap());
        let drw = typed_slot("vkCmdDraw", f, Conv::of("vkCmdDraw").unwrap());
        let ins = typed_slot("vkCreateInstance", f, Conv::of("vkCreateInstance").unwrap());
        // minimo de 9 repeticiones intercaladas (la maquina comparte CPU)
        let mut m = [f64::MAX; 4];
        for _ in 0..9 {
            m[0] = m[0].min(medir(plano, 0, rp.as_ptr() as u64));
            m[1] = m[1].min(medir(rps, 0, rp.as_ptr() as u64));
            m[2] = m[2].min(medir(drw, 0, 3));
            m[3] = m[3].min(medir(ins, ici.as_ptr() as u64, 0));
        }
        println!("BENCH vk: reenvio sin conversiones          {:.2} ns", m[0]);
        println!("BENCH vk: vkCmdBeginRenderPass               {:.2} ns (+{:.2})", m[1], m[1] - m[0]);
        println!("BENCH vk: vkCmdDraw                          {:.2} ns (+{:.2})", m[2], m[2] - m[0]);
        println!("BENCH vk: vkCreateInstance (cadena sin cb)   {:.2} ns (+{:.2})", m[3], m[3] - m[0]);
    }

    extern "C" fn devuelve_arg1(_a: u64, b: u64) -> u64 {
        b
    }

    #[test]
    fn un_envoltorio_jni_pasado_como_dato_llega_intacto() {
        hle::init();
        // el guest guarda su JavaVM* (envoltorio del puente) como dato: pthread_setspecific(key, vm), memcpy...
        let vm = crate::jni::guest_vm_for(0x7f00_dead_0000);
        assert!(crate::jni::is_guest_wrapper(vm));
        let slot = hle::register("prueba_guarda_dato", forward_handler(hf(devuelve_arg1 as *const () as usize)));
        let (mut c, _p) = cpu();
        c.x[1] = vm;
        c.pc = slot;
        hle::dispatch(&mut c);
        assert_eq!(c.x[0], vm, "el reenvio universal cambio un dato del guest por un puntero del host");
        // con el primer argumento tampoco: solo donde la firma declara un JNIEnv*
        let (mut c, _p) = cpu();
        c.x[0] = vm;
        c.x[1] = vm;
        c.pc = slot;
        hle::dispatch(&mut c);
        assert_eq!(c.x[0], vm);
    }

    extern "C" fn devuelve_arg0_32(a: u32) -> u64 {
        a as u64
    }

    #[test]
    fn argumentos_estrechos_se_extienden() {
        hle::init();
        static NW: [(u8, u8); 1] = [(0, 1)];
        static NS: [(u8, u8); 1] = [(0, 2)];
        let f = hf(devuelve_arg0_32 as *const () as usize);
        // bool/u8 con basura en los bits altos (legal en AAPCS64): el host debe ver solo el byte bajo
        let s = typed_slot("prueba_estrecho_u8", f, Conv { narrow: &NW, ..Conv::NONE });
        let (mut c, _p) = cpu();
        c.x[0] = 0xdead_be01;
        c.pc = s;
        hle::dispatch(&mut c);
        assert_eq!(c.x[0], 1);
        // i8 negativo: extension de signo a 32 bits
        let s = typed_slot("prueba_estrecho_i8", f, Conv { narrow: &NS, ..Conv::NONE });
        let (mut c, _p) = cpu();
        c.x[0] = 0x1234_56ff;
        c.pc = s;
        hle::dispatch(&mut c);
        assert_eq!(c.x[0], 0xffff_ffff);
    }

    #[test]
    fn biblioteca_no_permitida_no_resuelve() {
        assert!(!lib_allowed("libart.so"));
        assert!(!lib_allowed("libc++.so"));
        assert!(!lib_allowed("libandroid_runtime.so"));
    }
}
