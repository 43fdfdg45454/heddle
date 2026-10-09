//! Objetos y estructuras del NDK con punteros a funcion que necesitan conversion a mano (lo que no cubren el reenvio
//! tipado de `boundary::typed_slot` ni los proxys de interfaces de `proxy.rs`):
//!
//!  * `ANativeActivity`: el framework llama a `ANativeActivity_onCreate` con su estructura (vm, env y la tabla de
//!    callbacks son del host). El guest recibe un PROXY: una copia con su JavaVM/JNIEnv envueltos y una tabla de
//!    callbacks propia que el guest rellena cuando quiera (en onCreate o despues). En la tabla real se instalan
//!    despachadores del puente que, en cada evento, buscan el callback actual del guest y lo llaman con su proxy y
//!    los argumentos con su tipo. La estructura del host no se modifica (el framework sigue usando su vm/env). Las
//!    funciones `ANativeActivity_*` desenvuelven el proxy. onDestroy libera el proxy.
//!  * `AMediaCodec_setAsyncNotifyCallback`: recibe una estructura de 4 callbacks POR VALOR (32 bytes: en AAPCS64
//!    llega como puntero a una copia; en SysV va en la pila).
//!  * `timer_create` con SIGEV_THREAD: el puntero a funcion esta en una union (solo vale si `sigev_notify` lo dice).
//!  * `glob`: con GLOB_ALTDIRFUNC el host llama a `gl_opendir`/`gl_readdir`/`gl_closedir`/`gl_lstat`/`gl_stat` de la
//!    `glob_t` del guest; `gl_lstat`/`gl_stat` reciben un `struct stat` del host (144 bytes) y el guest escribe el suyo
//!    (128): el host recibe una copia de `glob_t` con trampolines y, para las dos de stat, un adaptador que llama a la
//!    funcion guest con un `struct stat` de arm64 y lo convierte. El resultado se copia de vuelta.
//!  * liblog: `__android_log_set_logger`/`__android_log_set_aborter` (globales del proceso, el host los llama desde
//!    cualquier hilo). El registrador del guest se instala detras de un despachador del puente: los mensajes del propio
//!    puente van directos a logd (se escriben en manejadores de senal, con bloqueos tomados...) y los demas al guest.
//!  * `android.app.func_name`: el punto de entrada de NativeActivity que declara el manifiesto (ver `is_activity_entry`).
//!
//! Memoria: tabla fija de `MAX_ACTS` actividades; las conversiones de callbacks usan trampolines de la arena fija de
//! cbthunk (uno por funcion y firma) y copias en la pila del manejador. Nunca se llama al guest con un bloqueo tomado.

use crate::boundary::{self, HostFn};
use crate::cpu::Cpu;
use crate::hle::{self, Ret};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU32, AtomicU64};
use std::sync::{Mutex, OnceLock};

fn log(m: &str) {
    #[cfg(target_os = "android")]
    crate::bridge::alog(m);
    #[cfg(not(target_os = "android"))]
    let _ = m;
}

// ---------------------------------------------------------------------------------------------
// ANativeActivity
// ---------------------------------------------------------------------------------------------

/// Actividades nativas vivas a la vez (una app tiene una; se recrea tras onDestroy).
pub const MAX_ACTS: usize = 8;
/// Palabras de `ANativeActivity` (LP64, igual en arm64 y x86-64): callbacks, vm, env, clazz, internalDataPath,
/// externalDataPath, sdkVersion, instance, assetManager, obbPath.
const ACT_WORDS: usize = 10;
/// Callbacks de `ANativeActivityCallbacks`, en orden.
const NCB: usize = 16;
const CB_ON_SAVE_INSTANCE_STATE: usize = 2;
const CB_ON_DESTROY: usize = 5;

/// Proxy de una actividad: lo que ve el guest (estructura y tabla de callbacks) y la actividad real.
#[repr(C)]
struct Act {
    act: [AtomicU64; ACT_WORDS],
    cbs: [AtomicU64; NCB],
    host: AtomicU64,
}

static ACTS: [Act; MAX_ACTS] =
    [const { Act { act: [const { AtomicU64::new(0) }; ACT_WORDS], cbs: [const { AtomicU64::new(0) }; NCB], host: AtomicU64::new(0) } }; MAX_ACTS];
static ACTS_LOCK: Mutex<()> = Mutex::new(());
static FULL_LOGS: AtomicU32 = AtomicU32::new(0);

fn act_guest(k: usize) -> u64 {
    ACTS[k].act.as_ptr() as u64
}

/// Proxy de la actividad real `h` (None si no tiene).
fn act_of_host(h: u64) -> Option<usize> {
    (0..MAX_ACTS).find(|&k| h != 0 && ACTS[k].host.load(Acquire) == h)
}

/// Actividad real detras del proxy `g`; None si `g` no es un proxy de actividad.
pub fn activity_host(g: u64) -> Option<u64> {
    (0..MAX_ACTS).find(|&k| act_guest(k) == g).map(|k| ACTS[k].host.load(Acquire)).filter(|&h| h != 0)
}

extern "C" fn act_cb<const I: usize>(a: u64, b: u64) -> u64 {
    act_dispatch(I, a, b)
}

/// Despachadores que se instalan en la tabla de callbacks real (uno por callback).
const DISPATCH: [extern "C" fn(u64, u64) -> u64; NCB] = [
    act_cb::<0>,
    act_cb::<1>,
    act_cb::<2>,
    act_cb::<3>,
    act_cb::<4>,
    act_cb::<5>,
    act_cb::<6>,
    act_cb::<7>,
    act_cb::<8>,
    act_cb::<9>,
    act_cb::<10>,
    act_cb::<11>,
    act_cb::<12>,
    act_cb::<13>,
    act_cb::<14>,
    act_cb::<15>,
];

/// Evento `i` de la actividad real `host` (todos los callbacks reciben la actividad y, como mucho, un argumento entero o
/// puntero: el foco, la ventana, la cola de entrada, el rectangulo o el puntero al tamano del estado guardado).
fn act_dispatch(i: usize, host: u64, arg: u64) -> u64 {
    crate::rt::ensure_thread(crate::rt::DEFAULT_STACK);
    let Some(k) = act_of_host(host) else { return 0 };
    let f = ACTS[k].cbs[i].load(Relaxed);
    let mut r = 0;
    if f != 0 && boundary::is_guest_executable(f) {
        let mut x = [0u64; 8];
        x[0] = act_guest(k);
        x[1] = if i == 6 { arg & 0xffff_ffff } else { arg };
        r = crate::rt::call_guest_regs(f, &x, &[0; 8], &[]).0;
    }
    if i == CB_ON_DESTROY {
        let _g = crate::monitor::lock(&ACTS_LOCK);
        ACTS[k].host.store(0, Release);
    }
    if i == CB_ON_SAVE_INSTANCE_STATE {
        r
    } else {
        0
    }
}

/// `ANativeActivity_onCreate` del guest (`guest`) llamado por el framework con la actividad real: el guest recibe su
/// proxy; en la tabla real quedan los despachadores.
pub fn activity_on_create(guest: u64, host: u64, saved: u64, saved_len: u64) {
    if host == 0 {
        return;
    }
    let k = {
        let _g = crate::monitor::lock(&ACTS_LOCK);
        let k = act_of_host(host).or_else(|| (0..MAX_ACTS).find(|&k| ACTS[k].host.load(Relaxed) == 0));
        if let Some(k) = k {
            let a = &ACTS[k];
            let words = unsafe { *(host as *const [u64; ACT_WORDS]) };
            for (w, v) in a.act.iter().zip(words) {
                w.store(v, Relaxed);
            }
            for c in &a.cbs {
                c.store(0, Relaxed);
            }
            a.act[0].store(a.cbs.as_ptr() as u64, Relaxed);
            a.host.store(host, Release);
        }
        k
    };
    let Some(k) = k else {
        if FULL_LOGS.fetch_add(1, Relaxed) < 10 {
            crate::bridge::alog_fatal(&format!("ANativeActivity: mas de {} actividades vivas; onCreate no se entrega al guest", MAX_ACTS));
        }
        return;
    };
    // JavaVM/JNIEnv que ve el guest: sus envoltorios (fuera del bloqueo: toman los suyos)
    ACTS[k].act[1].store(crate::jni::guest_vm_for(ACTS[k].act[1].load(Relaxed)), Relaxed);
    ACTS[k].act[2].store(crate::jni::guest_env_for(ACTS[k].act[2].load(Relaxed)), Relaxed);
    let cbs = unsafe { *(host as *const u64) } as *mut u64;
    if !cbs.is_null() {
        for (i, d) in DISPATCH.iter().enumerate() {
            unsafe { *cbs.add(i) = *d as usize as u64 };
        }
    }
    let mut x = [0u64; 8];
    x[0] = act_guest(k);
    x[1] = saved;
    x[2] = saved_len;
    crate::rt::call_guest_regs(guest, &x, &[0; 8], &[]);
}

/// Actividades vivas (pruebas).
pub fn live_activities() -> usize {
    ACTS.iter().filter(|a| a.host.load(Relaxed) != 0).count()
}

// ---------------------------------------------------------------------------------------------
// Registro
// ---------------------------------------------------------------------------------------------

/// Reenvio de una funcion `ANativeActivity_*`: el primer argumento es el proxy de la actividad.
fn reg_activity_fn(name: &'static str) {
    hle::register(
        name,
        Box::new(move |c: &mut Cpu| {
            let Some(f) = HostFn::symbol(name) else { return Ret::Return };
            if let Some(h) = activity_host(c.x[0]) {
                c.x[0] = h;
            }
            boundary::call_host(c, f, None);
            Ret::Return
        }),
    );
}

/// AMediaCodec_setAsyncNotifyCallback(codec, AMediaCodecOnAsyncNotifyCallback callback, userdata): la estructura
/// de callbacks va por valor; en AAPCS64 llega en x1 como puntero a una copia, en SysV va entera en la pila.
fn media_codec_async(c: &mut Cpu) {
    const NAME: &str = "AMediaCodec_setAsyncNotifyCallback";
    let (Some(f), Some(sc)) = (HostFn::symbol(NAME), crate::sigs::struct_cbs(NAME).first()) else {
        c.x[0] = (-10000i32) as u32 as u64; // AMEDIA_ERROR_UNKNOWN
        return;
    };
    let mut buf = [0u64; boundary::STRUCT_COPY_WORDS];
    let words = (sc.size as usize + 7) / 8;
    let p = boundary::copy_struct_callbacks(c.x[1], sc, &mut buf);
    if p != buf.as_ptr() as u64 {
        if c.x[1] != 0 {
            unsafe { std::ptr::copy_nonoverlapping(c.x[1] as *const u64, buf.as_mut_ptr(), words) };
        }
        log(&format!("{}: la estructura no se pudo convertir ({})", NAME, sc.note));
    }
    let (r, _, _) = boundary::call_host_args(f, &[c.x[0], c.x[2], 0, 0, 0, 0], &[0; 8], &buf[..words]);
    c.x[0] = r & 0xffff_ffff;
}

/// timer_create(clockid, struct sigevent *evp, timer_t *id): con SIGEV_THREAD el host recibe una copia con la funcion
/// convertida en un trampolin (la libc la copia al crear el temporizador). Con otro `sigev_notify` el campo de la union
/// no es una funcion y la estructura pasa tal cual.
fn timer_create(c: &mut Cpu) {
    const SIGEV_THREAD: i32 = 2;
    let Some(f) = HostFn::symbol("timer_create") else {
        c.x[0] = u64::MAX;
        return;
    };
    let evp = c.x[1];
    let mut ev = [0u64; 8];
    if evp != 0 && unsafe { *((evp + 12) as *const i32) } == SIGEV_THREAD {
        ev.copy_from_slice(unsafe { &*(evp as *const [u64; 8]) });
        // sigev_notify_function(union sigval): la union de 8 bytes llega en un registro entero
        ev[2] = boundary::to_host_callable_sig(ev[2], b"VJ", 0);
        c.x[1] = ev.as_ptr() as u64;
    }
    boundary::call_host(c, f, None);
}

// ---------------------------------------------------------------------------------------------
// glob con GLOB_ALTDIRFUNC
// ---------------------------------------------------------------------------------------------

/// glob_t de bionic (igual en arm64 y en x86-64): gl_pathc, gl_matchc, gl_offs, gl_flags, gl_pathv, gl_errfunc,
/// gl_closedir, gl_readdir, gl_opendir, gl_lstat, gl_stat.
const GLOB_WORDS: usize = 11;
const GLOB_ALTDIRFUNC: u32 = 0x40;
/// Firmas de gl_closedir, gl_readdir y gl_opendir (palabras 6, 7 y 8) y de gl_errfunc.
const GLOB_DIR_SIGS: [&[u8]; 3] = [b"VJ", b"JJ", b"JJ"];
const GLOB_ERR_SIG: &[u8] = b"IJI";

thread_local! {
    /// gl_lstat y gl_stat del guest durante su glob (los adaptadores las llaman en el mismo hilo).
    static GLOB_STAT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

/// Adaptador de gl_lstat/gl_stat (`k` = 0/1): el host pasa su `struct stat`; la funcion guest escribe el de arm64.
fn glob_stat_call(k: usize, path: u64, st: u64) -> i32 {
    let (l, s) = GLOB_STAT.with(|c| c.get());
    let g = if k == 0 { l } else { s };
    let mut buf = [0u64; 16];
    let (r, _) = crate::rt::call_guest(g, &[path, buf.as_mut_ptr() as u64], &[]);
    if r as i32 == 0 && st != 0 {
        unsafe { crate::syscall::stat_to_host(buf.as_ptr() as *const u8, st as *mut u8) };
    }
    r as i32
}

extern "C" fn glob_lstat_adapter(path: u64, st: u64) -> i32 {
    glob_stat_call(0, path, st)
}

extern "C" fn glob_stat_adapter(path: u64, st: u64) -> i32 {
    glob_stat_call(1, path, st)
}

/// Funciones de la glob_t del guest `g` para el host: trampolines y, para stat, los adaptadores. Devuelve las cinco
/// (closedir, readdir, opendir, lstat, stat) y las dos de stat del guest.
fn glob_dir_fns(g: &[u64; GLOB_WORDS]) -> ([u64; 5], (u64, u64)) {
    let mut h = [0u64; 5];
    for k in 0..3 {
        h[k] = if g[6 + k] == 0 { 0 } else { boundary::host_callable_cached(g[6 + k], GLOB_DIR_SIGS[k]) };
    }
    h[3] = if g[9] == 0 { 0 } else { glob_lstat_adapter as usize as u64 };
    h[4] = if g[10] == 0 { 0 } else { glob_stat_adapter as usize as u64 };
    (h, (g[9], g[10]))
}

/// glob(pattern, flags, errfunc, pglob) del guest.
fn glob(c: &mut Cpu) {
    let Some(f) = HostFn::symbol("glob") else {
        c.x[0] = (-1i64) as u64; // GLOB_NOSPACE
        return;
    };
    let (pat, flags, errf, pg) = (c.x[0], c.x[1] as u32, c.x[2], c.x[3]);
    let herr = if errf == 0 { 0 } else { boundary::host_callable_cached(errf, GLOB_ERR_SIG) };
    if pg == 0 {
        let (r, _, _) = boundary::call_ints(f, [pat, flags as u64, herr, 0, 0, 0]);
        c.x[0] = r as i32 as i64 as u64;
        return;
    }
    let g = unsafe { &mut *(pg as *mut [u64; GLOB_WORDS]) };
    let (dirs, stats) = if flags & GLOB_ALTDIRFUNC != 0 { glob_dir_fns(g) } else { ([g[6], g[7], g[8], g[9], g[10]], (0, 0)) };
    let prev = GLOB_STAT.with(|s| s.replace(stats));
    let r = host_glob(f, pat, flags, herr, g, dirs);
    GLOB_STAT.with(|s| s.set(prev));
    // bionic anota en gl_errfunc el argumento: el guest ve su funcion
    g[5] = errf;
    c.x[0] = r as i64 as u64;
}

/// Host con bionic: la misma glob_t; se pasa una copia con las funciones del host y se devuelve lo que glob escribe.
#[cfg(target_os = "android")]
fn host_glob(f: HostFn, pat: u64, flags: u32, herr: u64, g: &mut [u64; GLOB_WORDS], dirs: [u64; 5]) -> i32 {
    let mut h = *g;
    h[6..11].copy_from_slice(&dirs);
    let (r, _, _) = boundary::call_ints(f, [pat, flags as u64, herr, h.as_mut_ptr() as u64, 0, 0]);
    g[0] = h[0];
    g[1] = h[1];
    g[3] = (g[3] & !0xffff_ffff) | (h[3] & 0xffff_ffff);
    g[4] = h[4];
    r as i32
}

/// Host con glibc (heddle-run y pruebas fuera de Android): otra disposicion de glob_t (sin gl_matchc), otras banderas y
/// otros codigos de error. Se traduce en los dos sentidos.
#[cfg(not(target_os = "android"))]
fn host_glob(f: HostFn, pat: u64, flags: u32, herr: u64, g: &mut [u64; GLOB_WORDS], dirs: [u64; 5]) -> i32 {
    // (bionic, glibc): APPEND DOOFFS ERR MARK NOCHECK NOSORT ALTDIRFUNC BRACE MAGCHAR NOMAGIC TILDE NOESCAPE
    const MAP: [(u32, u32); 12] = [(0x1, 32), (0x2, 8), (0x4, 1), (0x8, 2), (0x10, 16), (0x20, 4), (0x40, 512), (0x80, 1024), (0x100, 256), (0x200, 2048), (0x800, 4096), (0x2000, 64)];
    let to = |b: u32| MAP.iter().filter(|m| b & m.0 != 0).fold(0, |a, m| a | m.1);
    let from = |x: u32| MAP.iter().filter(|m| x & m.1 != 0).fold(0, |a, m| a | m.0);
    // glibc: gl_pathc, gl_pathv, gl_offs, gl_flags, gl_closedir, gl_readdir, gl_opendir, gl_lstat, gl_stat
    let mut h = [g[0], g[4], g[2], to(g[3] as u32) as u64, dirs[0], dirs[1], dirs[2], dirs[3], dirs[4]];
    let before = if flags & 0x1 != 0 { g[0] } else { 0 };
    let (r, _, _) = boundary::call_ints(f, [pat, to(flags) as u64, herr, h.as_mut_ptr() as u64, 0, 0]);
    g[0] = h[0];
    g[1] = h[0].saturating_sub(before);
    g[3] = (g[3] & !0xffff_ffff) | from(h[3] as u32) as u64;
    g[4] = h[1];
    match r as i32 {
        0 => 0,
        1 => -1,
        2 => -2,
        3 => -3,
        _ => -1,
    }
}

/// globfree(pglob): libera gl_pathv (la reservo el malloc del host, el mismo que usa el guest).
fn globfree(c: &mut Cpu) {
    let Some(f) = HostFn::symbol("globfree") else { return };
    let pg = c.x[0];
    if pg == 0 {
        return;
    }
    #[cfg(target_os = "android")]
    boundary::call_ints(f, [pg, 0, 0, 0, 0, 0]);
    #[cfg(not(target_os = "android"))]
    {
        let g = unsafe { &mut *(pg as *mut [u64; GLOB_WORDS]) };
        let mut h = [g[0], g[4], g[2], 0, 0, 0, 0, 0, 0];
        boundary::call_ints(f, [h.as_mut_ptr() as u64, 0, 0, 0, 0, 0]);
        g[4] = h[1];
    }
}

// ---------------------------------------------------------------------------------------------
// liblog: registrador y abortador globales
// ---------------------------------------------------------------------------------------------

/// Registrador que instalo el guest, ya llamable por el host (trampolin con firma o la funcion del host de un slot).
static LOGGER: AtomicU64 = AtomicU64::new(0);
/// `__android_log_logd_logger` del host: destino de los mensajes del propio puente.
static LOGD: OnceLock<Option<HostFn>> = OnceLock::new();

thread_local! {
    /// >0: el hilo esta escribiendo un mensaje del propio puente (`bridge::alog`)
    static BRIDGE_LOG: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Ejecuta `f` (que escribe en liblog) marcando el mensaje como del propio puente: no llega al registrador del guest.
pub fn bridge_logging<R>(f: impl FnOnce() -> R) -> R {
    let _ = BRIDGE_LOG.try_with(|b| b.set(b.get() + 1));
    let r = f();
    let _ = BRIDGE_LOG.try_with(|b| b.set(b.get() - 1));
    r
}

/// Registrador que ve liblog mientras el guest tiene uno instalado. Sin bloqueos ni reservas (el puente escribe desde
/// manejadores de senal): los mensajes del puente van a logd; los demas, al registrador del guest.
extern "C" fn heddle_logger(msg: u64) {
    let own = BRIDGE_LOG.try_with(|b| b.get() != 0).unwrap_or(true);
    let f = if own {
        LOGD.get().copied().flatten()
    } else {
        // lo valida la variante sin bloqueos (instantanea del mapa del host, arena de trampolines)
        HostFn::from_host_sig(LOGGER.load(Acquire))
    };
    if let Some(f) = f {
        boundary::call_ints(f, [msg, 0, 0, 0, 0, 0]);
    }
}

/// `__android_log_set_logger(logger)`: el host recibe el despachador; NULL repone el registrador por defecto.
fn set_logger(c: &mut Cpu, f: HostFn) {
    let v = c.x[0];
    let _ = LOGD.get_or_init(|| HostFn::symbol("__android_log_logd_logger"));
    if v == 0 {
        LOGGER.store(0, Release);
        boundary::call_ints(f, [0; 6]);
        return;
    }
    LOGGER.store(boundary::to_host_callable_sig(v, b"VJ", 0), Release);
    boundary::call_ints(f, [heddle_logger as usize as u64, 0, 0, 0, 0, 0]);
}

/// Proxys de funciones directas cuya semantica el tipo no dice; solo si el host las tiene (un dispositivo anterior a
/// la API 30 no tiene el registrador). None: se reenvia por la firma.
pub fn special_slot(name: &str, f: HostFn) -> Option<u64> {
    match name {
        "__android_log_set_logger" => Some(hle::register(
            name,
            Box::new(move |c: &mut Cpu| {
                set_logger(c, f);
                Ret::Return
            }),
        )),
        // el abortador solo lo llama liblog (__android_log_assert, __android_log_call_aborter), nunca el puente
        "__android_log_set_aborter" => Some(hle::register(
            name,
            Box::new(move |c: &mut Cpu| {
                let h = boundary::to_host_callable_sig(c.x[0], b"VJ", 0);
                boundary::call_ints(f, [h, 0, 0, 0, 0, 0]);
                Ret::Return
            }),
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// NativeActivity: punto de entrada propio (android.app.func_name)
// ---------------------------------------------------------------------------------------------

/// JNIEnv del host del hilo actual con llamadas por indice de su tabla (validadas como codigo del host).
struct Jni {
    env: u64,
}

impl Jni {
    fn call(&self, idx: usize, a: [u64; 5]) -> u64 {
        let tbl = unsafe { *(self.env as *const u64) };
        let Some(f) = HostFn::from_host(unsafe { *((tbl + 8 * idx as u64) as *const u64) }) else { return 0 };
        boundary::call_ints(f, [self.env, a[0], a[1], a[2], a[3], a[4]]).0
    }
    /// Hubo una excepcion? (se limpia)
    fn exc(&self) -> bool {
        if self.call(228, [0; 5]) & 0xff != 0 {
            self.call(17, [0; 5]);
            return true;
        }
        false
    }
    fn class(&self, n: &str) -> u64 {
        let cn = std::ffi::CString::new(n).unwrap();
        let r = self.call(6, [cn.as_ptr() as u64, 0, 0, 0, 0]);
        if self.exc() {
            0
        } else {
            r
        }
    }
    fn method(&self, cls: u64, stat: bool, n: &str, sig: &str) -> u64 {
        let (cn, cs) = (std::ffi::CString::new(n).unwrap(), std::ffi::CString::new(sig).unwrap());
        let r = if cls == 0 { 0 } else { self.call(if stat { 113 } else { 33 }, [cls, cn.as_ptr() as u64, cs.as_ptr() as u64, 0, 0]) };
        if self.exc() {
            0
        } else {
            r
        }
    }
    /// Call<Static>ObjectMethodA
    fn obj(&self, target: u64, stat: bool, mid: u64, args: &[u64]) -> u64 {
        if target == 0 || mid == 0 {
            return 0;
        }
        let r = self.call(if stat { 116 } else { 36 }, [target, mid, args.as_ptr() as u64, 0, 0]);
        if self.exc() {
            0
        } else {
            r
        }
    }
    fn string(&self, s: u64) -> Option<String> {
        if s == 0 {
            return None;
        }
        let p = self.call(169, [s, 0, 0, 0, 0]);
        if p == 0 {
            return None;
        }
        let r = unsafe { std::ffi::CStr::from_ptr(p as *const std::os::raw::c_char) }.to_string_lossy().into_owned();
        self.call(170, [s, p, 0, 0, 0]);
        Some(r)
    }
}

/// (android.app.lib_name, android.app.func_name) de las actividades del paquete que declaran un punto de entrada
/// propio, leidos del manifiesto con PackageManager (lo mismo que lee NativeActivity.onCreate). Se guarda la primera
/// lectura completa (sin aplicacion todavia no se guarda: se vuelve a leer en la peticion siguiente).
fn manifest_entries() -> &'static [(String, String)] {
    static M: OnceLock<Vec<(String, String)>> = OnceLock::new();
    if let Some(v) = M.get() {
        return v;
    }
    let Some(v) = read_manifest() else { return &[] };
    log(&format!("NativeActivity: puntos de entrada del manifiesto: {:?}", v));
    M.get_or_init(|| v)
}

/// JNIEnv del host del hilo actual (None si no hay maquina virtual o el hilo no esta unido a ella; Some(None) si no
/// hay JNI en el proceso).
fn host_jni() -> Option<Option<Jni>> {
    let Some(gv) = HostFn::symbol("JNI_GetCreatedJavaVMs") else { return Some(None) };
    let (mut vm, mut n) = (0u64, 0i32);
    boundary::call_ints(gv, [&mut vm as *mut u64 as u64, 1, &mut n as *mut i32 as u64, 0, 0, 0]);
    if n < 1 || vm == 0 {
        return None;
    }
    // JavaVM->GetEnv(vm, &env, JNI_VERSION_1_6)
    let Some(ge) = HostFn::from_host(unsafe { *((*(vm as *const u64) + 8 * 6) as *const u64) }) else { return None };
    let mut env = 0u64;
    if boundary::call_ints(ge, [vm, &mut env as *mut u64 as u64, 0x10006, 0, 0, 0]).0 as i32 != 0 || env == 0 {
        return None;
    }
    Some(Some(Jni { env }))
}

/// `ApplicationInfo` de la app (`sourceDir`, `splitSourceDirs`, `nativeLibraryDir`), por JNI con
/// `ActivityThread.currentApplication().getApplicationInfo()`, para el modo de compatibilidad de 16 KiB
/// (`pagecompat`). None sin aplicacion todavia o fuera de un hilo Java.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn app_info() -> Option<(Vec<String>, Vec<String>, Option<String>)> {
    let j = host_jni()??;
    if j.call(19, [16, 0, 0, 0, 0]) as i32 != 0 {
        j.exc();
        return None;
    }
    let r = (|| {
        let at = j.class("android/app/ActivityThread");
        let app = j.obj(at, true, j.method(at, true, "currentApplication", "()Landroid/app/Application;"), &[]);
        let ctx = j.class("android/content/Context");
        let ai = j.obj(app, false, j.method(ctx, false, "getApplicationInfo", "()Landroid/content/pm/ApplicationInfo;"), &[]);
        let aic = j.class("android/content/pm/ApplicationInfo");
        if ai == 0 || aic == 0 {
            return None;
        }
        let field = |n: &std::ffi::CStr, sig: &std::ffi::CStr| {
            let fid = j.call(94, [aic, n.as_ptr() as u64, sig.as_ptr() as u64, 0, 0]);
            if j.exc() || fid == 0 {
                0
            } else {
                j.call(95, [ai, fid, 0, 0, 0])
            }
        };
        let base = j.string(field(c"sourceDir", c"Ljava/lang/String;"))?;
        let native = j.string(field(c"nativeLibraryDir", c"Ljava/lang/String;"));
        let arr = field(c"splitSourceDirs", c"[Ljava/lang/String;");
        let n = if arr == 0 { 0 } else { j.call(171, [arr, 0, 0, 0, 0]) as i32 };
        let splits = (0..n.clamp(0, 1024)).filter_map(|i| j.string(j.call(173, [arr, i as u64, 0, 0, 0]))).collect();
        Some((vec![base], splits, native))
    })();
    j.exc();
    j.call(20, [0, 0, 0, 0, 0]);
    r
}

fn read_manifest() -> Option<Vec<(String, String)>> {
    let mut out = Vec::new();
    let j = match host_jni()? {
        Some(j) => j,
        None => return Some(out),
    };
    if j.call(19, [64, 0, 0, 0, 0]) as i32 != 0 {
        j.exc();
        return None;
    }
    let at = j.class("android/app/ActivityThread");
    let app = j.obj(at, true, j.method(at, true, "currentApplication", "()Landroid/app/Application;"), &[]);
    let ctx = j.class("android/content/Context");
    let pm = j.obj(app, false, j.method(ctx, false, "getPackageManager", "()Landroid/content/pm/PackageManager;"), &[]);
    let pkg = j.obj(app, false, j.method(ctx, false, "getPackageName", "()Ljava/lang/String;"), &[]);
    let pmc = j.class("android/content/pm/PackageManager");
    // GET_ACTIVITIES | GET_META_DATA
    let gpi = j.method(pmc, false, "getPackageInfo", "(Ljava/lang/String;I)Landroid/content/pm/PackageInfo;");
    let info = if pkg == 0 { 0 } else { j.obj(pm, false, gpi, &[pkg, 1 | 128]) };
    if info == 0 {
        // sin aplicacion todavia (o PackageManager no respondio): no se da por leido
        j.call(20, [0, 0, 0, 0, 0]);
        return None;
    }
    let pic = j.class("android/content/pm/PackageInfo");
    let acts = if info == 0 || pic == 0 {
        0
    } else {
        let fid = j.call(94, [pic, c"activities".as_ptr() as u64, c"[Landroid/content/pm/ActivityInfo;".as_ptr() as u64, 0, 0]);
        if j.exc() || fid == 0 {
            0
        } else {
            j.call(95, [info, fid, 0, 0, 0])
        }
    };
    let aic = j.class("android/content/pm/ActivityInfo");
    let md = if aic == 0 { 0 } else { j.call(94, [aic, c"metaData".as_ptr() as u64, c"Landroid/os/Bundle;".as_ptr() as u64, 0, 0]) };
    j.exc();
    let bc = j.class("android/os/Bundle");
    let gs = j.method(bc, false, "getString", "(Ljava/lang/String;)Ljava/lang/String;");
    let kf = j.call(167, [c"android.app.func_name".as_ptr() as u64, 0, 0, 0, 0]);
    let kl = j.call(167, [c"android.app.lib_name".as_ptr() as u64, 0, 0, 0, 0]);
    let len = if acts == 0 { 0 } else { j.call(171, [acts, 0, 0, 0, 0]) as i32 };
    for i in 0..len.clamp(0, 1024) {
        let a = j.call(173, [acts, i as u64, 0, 0, 0]);
        if j.exc() || a == 0 || md == 0 {
            continue;
        }
        let b = j.call(95, [a, md, 0, 0, 0]);
        if b == 0 {
            continue;
        }
        if let Some(func) = j.string(j.obj(b, false, gs, &[kf])) {
            let lib = j.string(j.obj(b, false, gs, &[kl])).unwrap_or_else(|| String::from("main"));
            out.push((lib, func));
        }
    }
    j.call(20, [0, 0, 0, 0, 0]);
    Some(out)
}

/// `name`, pedido con firma nula (`NativeBridgeGetTrampoline(handle, funcName, NULL, 0)`, como hace
/// `loadNativeCode_native` de NativeActivity) en la biblioteca `lib_path`, es el punto de entrada que el manifiesto
/// declara para una actividad (`android.app.func_name`, con su `android.app.lib_name`)? Se compara el nombre exacto y la
/// biblioteca exacta (NativeActivity carga `classLoader.findLibrary(lib_name)`: lib<lib_name>.so).
pub fn is_activity_entry(lib_path: &str, name: &str) -> bool {
    let base = lib_path.rsplit('/').next().unwrap_or(lib_path);
    manifest_entries().iter().any(|(l, f)| f == name && base == format!("lib{}.so", l))
}

pub fn register() {
    for n in ["ANativeActivity_finish", "ANativeActivity_setWindowFormat", "ANativeActivity_setWindowFlags", "ANativeActivity_showSoftInput", "ANativeActivity_hideSoftInput"] {
        reg_activity_fn(n);
    }
    hle::register(
        "AMediaCodec_setAsyncNotifyCallback",
        Box::new(|c: &mut Cpu| {
            media_codec_async(c);
            Ret::Return
        }),
    );
    hle::register(
        "timer_create",
        Box::new(|c: &mut Cpu| {
            timer_create(c);
            Ret::Return
        }),
    );
    hle::register(
        "glob",
        Box::new(|c: &mut Cpu| {
            glob(c);
            Ret::Return
        }),
    );
    hle::register(
        "globfree",
        Box::new(|c: &mut Cpu| {
            globfree(c);
            Ret::Return
        }),
    );
}
