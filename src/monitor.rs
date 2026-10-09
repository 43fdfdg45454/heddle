//! Monitor exclusivo ARM64 con equivalencia funcional estricta ("alternativa K" de las pruebas):
//! granulos de 64 B frios/calientes, stores normales sin bloqueo sobre granulos frios, armado de un granulo
//! (frio -> caliente) con una barrera remota, enfriamiento por epocas, saturacion ante presion sostenida.
//!
//! Garantia: STXR solo tiene exito si NINGUNA escritura (normal, atomica o exclusiva, aunque deje
//! el mismo valor) toco el granulo desde el LDXR, siempre que el escritor sea codigo traducido.
//! Limite conocido: escrituras de codigo x86 nativo o del kernel no pasan por aqui.
//!
//! MODOS (se elige uno al inicio, `init`, y queda en el registro: "monitor: modo=..."):
//!  * `rseq`: la libc (glibc) registra rseq; el store rapido es una seccion critica rseq (`heddle_rst*`,
//!    ensamblador, llamado directamente por el JIT) y el armado usa membarrier(PRIVATE_EXPEDITED_RSEQ), que aborta las
//!    secciones en curso.
//!  * `fence` (Android: kernel sin CONFIG_RSEQ y seccomp que lo bloquea): "fence asimetrico" con
//!    membarrier(PRIVATE_EXPEDITED). Ver abajo.
//!  * `bloqueo` (FALLBACK): sin membarrier; todo store toma el bloqueo del granulo y sube su version.
//!  Se puede forzar con HEDDLE_MONITOR=rseq|fence|bloqueo (o la propiedad debug.heddle.monitor);
//!  HEDDLE_FORCE_FALLBACK=1 equivale a `fence` (la ruta de Android, para probarla en el host).
//!
//! FENCE ASIMETRICO (modo `fence`). Cada hilo guest tiene una marca `MonFlag::in_store` (en su `GuestThread`; la
//! `Cpu` guarda su direccion en `monflag` y el JIT la pasa en RCX). Store rapido (`heddle_fst*`, ensamblador):
//!     S1: in_store = 1        (mov, sin lock)
//!     S2: lee HOT[granulo]    (mov)
//!     S3: si es FRIO: mov del dato; si no: in_store = 0 y ruta con bloqueo (lock + version, ya exacta)
//!     S4: in_store = 0
//! Armado de un granulo (ldx/ldxp/rmw_pair, con ARM_LOCK tomado):
//!     A1: HOT = ARMANDO       A2: membarrier(PRIVATE_EXPEDITED)
//!     A3: esperar a que in_store == 0 en todo hilo guest vivo (`FLAGS`)
//!     A4: HOT = CALIENTE (sello de epoca)   ... y despues la reserva lee version y dato.
//! Razonamiento x86-TSO. En TSO los stores salen en orden del bufer de stores y una carga solo puede adelantarse
//! a stores ANTERIORES del mismo hilo (S2 puede leer antes de que S1 sea visible: es el hueco de Dekker). La
//! barrera A2 lo cierra: membarrier(PRIVATE_EXPEDITED) garantiza que, al volver, cada hilo del proceso ejecuto
//! una barrera completa (IPI con smp_mb, o un cambio de contexto, que tambien la implica) en un punto B posterior
//! a A1 (el propio syscall lleva barrera completa antes de enviar las IPI, asi que A1 ya es visible en B).
//! Para un store rapido S de un hilo T sobre el granulo:
//!   (i)  si S2 esta despues de B en el orden de programa de T, S2 lee ARMANDO o CALIENTE: S va por el bloqueo;
//!   (ii) si S2 esta antes de B, tambien S1 (va antes en el programa) y B vacia el bufer: S1 es visible al volver
//!        A2. En A3 el armador lee in_store: si ve 1 espera; si ve 0 es porque S4 ya es visible, y como TSO
//!        publica los stores en orden, el dato de S3 tambien lo es. En ambos casos, al terminar A3 el dato de S
//!        es visible antes de que la reserva lea la version y el dato: S precede al LDXR (no lo invalida, igual
//!        que en un ARM real, donde S habria llegado antes que el LDXR).
//! Despues de A4 todo store al granulo va por el bloqueo y sube la version (exacto, como en `bloqueo`).
//! Enfriar (CALIENTE -> FRIO, `cool_locked`) no necesita barrera: primero sube EPOCH (toda reserva anterior falla
//! en STXR, que la compara con el bloqueo del granulo tomado) y luego pone FRIO bajo ese bloqueo; un store que
//! vea FRIO ocurre despues de que ninguna reserva viva pueda tener exito, y una reserva nueva vuelve a armar.
//! Senales: un manejador guest nunca corre con in_store = 1 (`defer_signal` la aplaza y la salida del store la
//! reenvia; un fallo sincrono en el propio mov limpia la marca y reinicia el store desde S1, `fault_in_store`):
//! si no, el armador esperaria a un hilo dormido dentro de su manejador (el interbloqueo del 53 %).
//! Atomicas LSE (`rmw`) en `fence`: no arman. Granulo FRIO: bucle de CAS cuya escritura es `heddle_fcas*`, la misma
//! ventana S1-S4 con un `lock cmpxchg` en lugar del mov (sin bloqueo ni version: con el granulo frio ninguna reserva
//! puede tener exito, igual que para el store rapido). Caliente o armando: con el bloqueo del granulo tomado, lock
//! cmpxchg del host (atomico frente a los stores rapidos) y suben la version.
//!
//! ALINEACION DE LAS RUTAS CALIENTES. El JIT llama en cada store a `heddle_rst*` (rseq) o `heddle_fst*` (fence), y
//! `rmw` en fence a `heddle_fcas*`. Su coste dependia de donde las dejara el enlazador: con el mismo codigo, una
//! `monitor::store` que empezaba a 48 B de una linea de 64 B hacia los stores rseq un 15-20 % mas lentos, y
//! `heddle_fst3` fuera del inicio de una linea, los de fence un ~10 % (en Cascade Lake: la ruta caliente repartida en
//! dos lineas y ventanas de 32 B del DSB, y saltos que cruzan o terminan en una ventana de 32 B, la errata JCC, que
//! los deja fuera de la cache de uops). Por eso cada entrada empieza con `.p2align 6` y su ruta caliente (hasta el
//! `ret`) cabe en esa linea sin saltos que crucen o terminen en un limite de 32 B; el orden de las instrucciones de
//! `heddle_rst*` y `heddle_fcas*` esta elegido para ello (R9 como indice en fcas, `fs:[off + 8]` en rst). Al
//! cambiarlas, comprobarlo con objdump. `align_tests::rutas_calientes_alineadas` y `tests/arch_lint.rs` comprueban la
//! alineacion. El modo bloqueo (ruta en Rust, `slow_store`) no depende de la posicion en lo medido (+-1 % entre cuatro
//! disposiciones, tambien con todas las funciones alineadas a 64 B), asi que no se alinea (Rust estable no alinea una
//! funcion concreta y `-align-all-functions=6` cuesta un 3 % de .text sin ganancia medible).

use std::arch::asm;
use std::cell::{Cell, UnsafeCell};
use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::time::Instant;

extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn dlsym(h: *mut std::ffi::c_void, name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
}

/// Desplazamiento de la estructura rseq respecto a tp (simbolo `__rseq_offset` de glibc/bionic, resuelto en
/// tiempo de ejecucion: si la libc no lo exporta se usa el modo FALLBACK).
static RSEQ_OFFSET: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
static RSEQ_KNOWN: AtomicBool = AtomicBool::new(false);

const SYS_MEMBARRIER: i64 = 324;
const MB_REGISTER_PRIVATE_EXPEDITED_RSEQ: i64 = 256;
const MB_PRIVATE_EXPEDITED_RSEQ: i64 = 128;

pub static MEMBARRIERS: AtomicU64 = AtomicU64::new(0);
pub static MB_NS: AtomicU64 = AtomicU64::new(0);
pub static RSEQ_ABORTS: AtomicU64 = AtomicU64::new(0);
pub static COOLS: AtomicU64 = AtomicU64::new(0);
pub static SATURATIONS: AtomicU64 = AtomicU64::new(0);
/// Modo fallback: no hay rseq/membarrier; todos los stores van por lock+version.
pub static FALLBACK: AtomicBool = AtomicBool::new(false);
static SATURATED: AtomicBool = AtomicBool::new(false);

/// Direccion de la estructura rseq del hilo actual (glibc la registra desde 2.35).
pub fn rseq_area() -> *mut u8 {
    unsafe {
        let tp: usize;
        asm!("mov {}, fs:[0]", out(reg) tp, options(nostack, preserves_flags, readonly));
        (tp as isize + RSEQ_OFFSET.load(Relaxed)) as *mut u8
    }
}

fn rseq_registered() -> bool {
    unsafe { std::ptr::read_volatile(rseq_area().add(4) as *const u32) != u32::MAX }
}

/// Comprueba que el hilo actual tiene rseq registrado; si no, el camino rapido deja de ser valido.
pub fn thread_check() {
    if mode() == MODE_RSEQ && !FALLBACK.load(SeqCst) && RSEQ_KNOWN.load(SeqCst) && !rseq_registered() {
        FALLBACK.store(true, SeqCst);
    }
}

pub const MODE_RSEQ: u8 = 1;
pub const MODE_FENCE: u8 = 2;
pub const MODE_LOCK: u8 = 3;
/// Modo del monitor (0 = sin elegir). Se elige UNA vez (`init`) y no cambia (salvo FALLBACK de rseq, ver thread_check).
static MODE: AtomicU8 = AtomicU8::new(0);
static MODE_ONCE: std::sync::Once = std::sync::Once::new();
const MB_REGISTER_PRIVATE_EXPEDITED: i64 = 16;
const MB_PRIVATE_EXPEDITED: i64 = 8;

/// Modo actual; lo elige la primera vez que se consulta (asi ningun camino usa el monitor antes de elegirlo).
#[inline]
pub fn mode() -> u8 {
    let m = MODE.load(Relaxed);
    if m != 0 {
        return m;
    }
    init();
    MODE.load(Acquire)
}

pub fn mode_name() -> &'static str {
    match MODE.load(Relaxed) {
        MODE_RSEQ => "rseq",
        MODE_FENCE => "fence",
        MODE_LOCK => "bloqueo",
        _ => "sin_elegir",
    }
}

fn try_rseq() -> Result<(), &'static str> {
    let p = unsafe { dlsym(std::ptr::null_mut(), b"__rseq_offset\0".as_ptr() as *const _) } as *const isize;
    if p.is_null() {
        return Err("la libc no exporta __rseq_offset");
    }
    RSEQ_OFFSET.store(unsafe { *p }, SeqCst);
    RSEQ_KNOWN.store(true, SeqCst);
    if unsafe { syscall(SYS_MEMBARRIER, MB_REGISTER_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) } != 0 {
        return Err("membarrier REGISTER_PRIVATE_EXPEDITED_RSEQ fallo");
    }
    if !rseq_registered() {
        return Err("rseq no registrado en el hilo");
    }
    Ok(())
}

fn try_fence() -> Result<(), &'static str> {
    // registro + una llamada de prueba: tras registrarse, el nucleo no la rechaza (solo con EPERM si no lo esta)
    if unsafe { syscall(SYS_MEMBARRIER, MB_REGISTER_PRIVATE_EXPEDITED, 0i64, 0i64) } != 0 {
        return Err("membarrier REGISTER_PRIVATE_EXPEDITED fallo");
    }
    if unsafe { syscall(SYS_MEMBARRIER, MB_PRIVATE_EXPEDITED, 0i64, 0i64) } != 0 {
        return Err("membarrier PRIVATE_EXPEDITED fallo");
    }
    Ok(())
}

fn select_mode() {
    let want = std::env::var("HEDDLE_MONITOR")
        .ok()
        .or_else(|| std::env::var_os("HEDDLE_FORCE_FALLBACK").filter(|v| v == "1").map(|_| "fence".to_string()))
        .or_else(|| crate::boundary::prop("debug.heddle.monitor"))
        .unwrap_or_default();
    let (m, why): (u8, String) = match want.as_str() {
        "bloqueo" => (MODE_LOCK, "forzado".into()),
        "fence" => match try_fence() {
            Ok(()) => (MODE_FENCE, "forzado".into()),
            Err(e) => (MODE_LOCK, format!("fence pedido pero {}", e)),
        },
        "rseq" => match try_rseq() {
            Ok(()) => (MODE_RSEQ, "forzado".into()),
            Err(e) => (MODE_LOCK, format!("rseq pedido pero {}", e)),
        },
        _ => match try_rseq() {
            Ok(()) => (MODE_RSEQ, "automatico".into()),
            Err(e1) => match try_fence() {
                Ok(()) => (MODE_FENCE, format!("automatico; sin rseq: {}", e1)),
                Err(e2) => (MODE_LOCK, format!("automatico; sin rseq: {}; sin fence: {}", e1, e2)),
            },
        },
    };
    FALLBACK.store(m == MODE_LOCK, SeqCst);
    MODE.store(m, SeqCst);
    let name = match m {
        MODE_RSEQ => "rseq",
        MODE_FENCE => "fence",
        _ => "bloqueo",
    };
    crate::bridge::alog(&format!("monitor: modo={} ({})", name, why));
}

/// Inicializa el monitor (elige el modo una sola vez). Devuelve true si hay camino rapido de stores.
pub fn init() -> bool {
    MODE_ONCE.call_once(select_mode);
    MODE.load(Acquire) != MODE_LOCK
}

fn membarrier_rseq() {
    MEMBARRIERS.fetch_add(1, Relaxed);
    let t = Instant::now();
    let r = unsafe { syscall(SYS_MEMBARRIER, MB_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) };
    MB_NS.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    if r != 0 {
        // sin membarrier no se puede garantizar nada: caer a fallback total
        FALLBACK.store(true, SeqCst);
    }
}

/// Barrera del armado: en `rseq`, membarrier que aborta las secciones rseq; en `fence`, membarrier(PRIVATE_EXPEDITED)
/// y espera a que ningun hilo este a mitad de un store rapido (pasos A2-A3 de la cabecera).
fn arm_barrier() {
    if mode() != MODE_FENCE {
        membarrier_rseq();
        return;
    }
    MEMBARRIERS.fetch_add(1, Relaxed);
    let t = Instant::now();
    let r = unsafe { syscall(SYS_MEMBARRIER, MB_PRIVATE_EXPEDITED, 0i64, 0i64) };
    if r != 0 {
        // Imposible tras registrarse (el nucleo solo la rechaza si no hay registro), salvo que la app instale un
        // filtro seccomp nuevo que la prohiba. Sin la barrera remota no hay forma exacta de seguir: se termina con
        // diagnostico en lugar de continuar con un monitor que podria aceptar un STXR que un ARM real rechaza.
        crate::bridge::alog_fatal(&format!("FALLO monitor: membarrier(PRIVATE_EXPEDITED) devolvio {} tras registrarse; no se puede garantizar el monitor exclusivo", r));
        std::process::abort();
    }
    wait_no_store_in_flight();
    MB_NS.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
}

/// Contadores del modo fence: hilos encontrados a mitad de store al armar y la espera maxima (ns).
pub static FENCE_WAITS: AtomicU64 = AtomicU64::new(0);
pub static FENCE_WAIT_MAX_NS: AtomicU64 = AtomicU64::new(0);

/// Paso A3: recorre los hilos guest vivos (FLAGS) y espera a que su marca in_store sea 0. La espera es de nanosegundos (un
/// store); si el hilo fue desalojado a mitad, se cede la CPU. Un hilo que ya no existe (proceso hijo tras fork, con
/// la lista heredada) se salta tras comprobarlo con tgkill(…, 0).
fn wait_no_store_in_flight() {
    let list = FLAGS.lock().unwrap_or_else(|e| e.into_inner());
    for &(tid, p) in list.iter() {
        let f = unsafe { &*(p as *const MonFlag) };
        if f.in_store.load(Acquire) == 0 {
            continue;
        }
        FENCE_WAITS.fetch_add(1, Relaxed);
        let t0 = Instant::now();
        let mut n = 0u64;
        while f.in_store.load(Acquire) != 0 {
            n += 1;
            if n < 256 {
                spin_loop();
            } else {
                unsafe { crate::sys::syscall(24) }; // sched_yield
                if n % 1024 == 0 {
                    let pid = unsafe { crate::sys::syscall(39) };
                    if unsafe { crate::sys::syscall(234, pid, tid as i64, 0i64) } < 0 {
                        break; // el hilo no existe
                    }
                }
            }
        }
        FENCE_WAIT_MAX_NS.fetch_max(t0.elapsed().as_nanos() as u64, Relaxed);
    }
}

// ===========================================================================
// Granulos con lock + version
// ===========================================================================

pub const NGRAN: usize = 1024;

#[repr(align(64))]
pub struct Granule {
    pub lock: AtomicBool,
    pub ver: AtomicU64,
}

pub static GRAN: [Granule; NGRAN] =
    [const { Granule { lock: AtomicBool::new(false), ver: AtomicU64::new(0) } }; NGRAN];

#[inline]
pub fn gran(addr: usize) -> &'static Granule {
    &GRAN[(addr >> 6) & (NGRAN - 1)]
}

#[inline]
fn glock(g: &Granule) {
    crit_enter();
    spin_acquire(&g.lock);
}

/// Giros de espera (con `pause`) antes de ceder la CPU cuando un bloqueo del monitor esta tomado. Las secciones
/// criticas duran decenas de ns: si no se libera tras miles de giros, lo mas probable es que el dueno haya sido
/// desalojado (mas hilos que CPU, como el banco con 8 hilos en 4 vCPU) y seguir girando solo le quita la CPU.
/// Medido (xbench, 8 hilos en 4 CPU): ceder tras 8192 giros baja la pila LL/SC de ~915 a ~540 ms y no cambia nada
/// con CPU de sobra (sin ceder 1128, cediendo 1094 ms).
const SPIN_BEFORE_YIELD: u32 = 8192;

/// Toma un bloqueo por giro (TTAS: solo intenta el CAS cuando lo ve libre; girar con CAS saca la linea del dueno en
/// cada intento y alarga la seccion critica de todos). El camino sin contencion es un CAS.
#[inline]
fn spin_acquire(l: &AtomicBool) {
    if l.compare_exchange_weak(false, true, Acquire, Relaxed).is_err() {
        spin_acquire_slow(l);
    }
}

#[cold]
#[inline(never)]
fn spin_acquire_slow(l: &AtomicBool) {
    loop {
        let mut n = 0u32;
        while l.load(Relaxed) {
            spin_loop();
            n += 1;
            if n >= SPIN_BEFORE_YIELD {
                unsafe { crate::sys::syscall(24) }; // sched_yield
                n = 0;
            }
        }
        if l.compare_exchange_weak(false, true, Acquire, Relaxed).is_ok() {
            return;
        }
    }
}

#[inline]
fn gunlock(g: &Granule) {
    g.lock.store(false, Release);
    crit_exit();
}

// ===========================================================================
// Secciones criticas y senales asincronas
// ===========================================================================
//
// Los bloqueos de granulo y ARM_LOCK son bloqueos por giro no reentrantes. Un manejador de senal guest que se
// ejecutara con uno de ellos tomado por el hilo interrumpido quedaria con el bloqueo retenido todo el tiempo que
// dure el manejador, y un manejador puede dormir indefinidamente (el recolector de basura de Boehm suspende los
// hilos con una senal y los deja en sigsuspend hasta que termina de marcar): cualquier otro hilo que necesitara
// ese granulo (el propio recolector al marcar) giraria para siempre. Pasaba en Android (modo FALLBACK, donde cada
// store toma un bloqueo) y era el cuelgue de Unity al 53 %.
//
// Por eso, mientras el hilo tiene algun bloqueo, el manejador host no ejecuta el manejador guest: anota la senal
// (`defer_signal`) y, al soltar el ultimo bloqueo, se la reenvia al propio hilo. Para el guest equivale a que la
// senal llego justo despues de la instruccion atomica, como en hardware.

/// Estado por hilo del monitor, en UNA sola variable thread_local: en una biblioteca cargada con dlopen (Android) cada
/// acceso TLS es una llamada a __tls_get_addr (~1 ns), y un store con FALLBACK hacia cuatro. Con este agrupamiento
/// `slow_store` hace uno.
struct ThreadMon {
    /// profundidad de bloqueos tomados (seccion critica)
    crit: Cell<u32>,
    /// senales diferidas (bit = numero) y el siginfo con que llegaron, para reenviarlas identicas
    pending: Cell<u64>,
    info: UnsafeCell<[[u8; 128]; 64]>,
    /// instancias extra de senales de tiempo real diferidas, con su siginfo (no se funden; ver `sig::RtQueue`)
    rtq: crate::sig::RtQueue,
    /// escrituras propias desde la ultima reserva (ver `Own`)
    own: Cell<Own>,
    /// marca de store rapido del hilo guest (modo fence; nula si el hilo no es guest)
    flag: Cell<*const MonFlag>,
    /// paginas comprobadas escribibles por `probe_w` (pagina + 1; 0 = libre) y, en [4], su generacion (`PROT_GEN`)
    wpages: Cell<[usize; 5]>,
}

thread_local! {
    static TM: ThreadMon = const { ThreadMon { crit: Cell::new(0), pending: Cell::new(0), info: UnsafeCell::new([[0; 128]; 64]), rtq: crate::sig::RtQueue::new(), own: Cell::new(Own { g: 0, start: 0, cur: 0, res_lo: 0, res_hi: 0 }), flag: Cell::new(std::ptr::null()), wpages: Cell::new([0; 5]) } };
}

/// Marca por hilo guest del modo fence (vive en el `GuestThread`; ver la cabecera). Escrita solo por su hilo (y por el
/// manejador de fallos de ese hilo); leida por los armadores. `pending`: hay senales aplazadas por in_store que la
/// salida del store debe reenviar. Alineada a 64 B: los armadores la leen sin compartir linea con otros hilos.
#[repr(C, align(64))]
pub struct MonFlag {
    pub in_store: AtomicU8,
    pub pending: AtomicU8,
}

impl MonFlag {
    pub const fn new() -> MonFlag {
        MonFlag { in_store: AtomicU8::new(0), pending: AtomicU8::new(0) }
    }
}

/// Marcas de los hilos guest vivos (tid, *const MonFlag) que recorre el armado. Lista propia (no rt::ALL_THREADS, que
/// el muestreador y el perfilador retienen mientras leen /proc): solo la toman, y por poco tiempo, el alta y la baja
/// de un hilo y el armado. Cota: hilos guest vivos (cada `GuestThread` se da de alta al crearse y de baja en su Drop,
/// antes de liberar la marca).
static FLAGS: std::sync::Mutex<Vec<(i32, usize)>> = std::sync::Mutex::new(Vec::new());

pub fn register_flag(tid: i32, f: *const MonFlag) {
    lock(&FLAGS).push((tid, f as usize));
}

pub fn unregister_flag(f: *const MonFlag) {
    lock(&FLAGS).retain(|e| e.1 != f as usize);
}

/// Asocia (o con nulo, desasocia) la marca del hilo guest actual: la usan `store` (ruta desde Rust) y `defer_signal`.
pub fn set_thread_flag(f: *const MonFlag) {
    let _ = TM.try_with(|t| t.flag.set(f));
}

#[inline]
fn crit_enter() {
    let _ = TM.try_with(|t| t.crit.set(t.crit.get() + 1));
}

#[inline]
fn crit_exit() {
    let _ = TM.try_with(crit_leave);
}

/// Sale de una seccion critica; al soltar la ultima, reenvia las senales diferidas.
#[inline]
fn crit_leave(t: &ThreadMon) {
    let n = t.crit.get().wrapping_sub(1);
    t.crit.set(n);
    if n == 0 && t.pending.get() != 0 {
        flush_pending(t);
    }
}

// ---------------------------------------------------------------------------------------------
// Secciones criticas del resto del puente
// ---------------------------------------------------------------------------------------------
//
// Lo mismo vale para cualquier bloqueo del puente que un manejador de senal guest pueda volver a pedir en el mismo
// hilo (registro de slots, cache de simbolos, tablas de envoltorios JNI, lista de modulos...): si la senal llega con
// el bloqueo tomado y el manejador guest llama a una funcion que lo pide, el hilo se bloquea a si mismo. Estas
// secciones son cortas y nunca ejecutan codigo guest: mientras duran, las senales con manejador guest se aplazan con
// el mismo mecanismo (`defer_signal`) y se reenvian al salir. Para el guest equivale a tenerlas bloqueadas durante la
// seccion (como pthread_sigmask alrededor de ella), sin dos llamadas al sistema por cada entrada.

/// Seccion critica sin bloqueo propio (p. ej. la creacion de un trampolin, sin bloqueos pero no reentrante): aplaza las
/// senales con manejador guest mientras vive.
pub struct NoSignals(());

impl NoSignals {
    #[inline]
    pub fn new() -> NoSignals {
        crit_enter();
        NoSignals(())
    }
}

impl Drop for NoSignals {
    #[inline]
    fn drop(&mut self) {
        crit_exit();
    }
}

/// Guarda de un bloqueo del puente tomado con las senales guest aplazadas (ver arriba). Se suelta el bloqueo y despues
/// se sale de la seccion critica (que reenvia las senales aplazadas).
pub struct Locked<G> {
    /// siempre Some salvo en el propio Drop (se suelta antes de salir de la seccion)
    g: Option<G>,
}

impl<G> Drop for Locked<G> {
    fn drop(&mut self) {
        drop(self.g.take());
        crit_exit();
    }
}

impl<G: std::ops::Deref> std::ops::Deref for Locked<G> {
    type Target = G::Target;
    fn deref(&self) -> &G::Target {
        self.g.as_ref().unwrap()
    }
}

impl<G: std::ops::DerefMut> std::ops::DerefMut for Locked<G> {
    fn deref_mut(&mut self) -> &mut G::Target {
        self.g.as_mut().unwrap()
    }
}

/// `m.lock()` con las senales guest aplazadas mientras se tiene (un Mutex envenenado se sigue usando).
pub fn lock<T>(m: &std::sync::Mutex<T>) -> Locked<std::sync::MutexGuard<'_, T>> {
    crit_enter();
    Locked { g: Some(m.lock().unwrap_or_else(|e| e.into_inner())) }
}

/// `l.read()` con las senales guest aplazadas (un lector anidado en un manejador se bloquearia si hay un escritor en
/// espera: los RwLock de std dan preferencia a los escritores).
pub fn read<T>(l: &std::sync::RwLock<T>) -> Locked<std::sync::RwLockReadGuard<'_, T>> {
    crit_enter();
    Locked { g: Some(l.read().unwrap_or_else(|e| e.into_inner())) }
}

/// `l.write()` con las senales guest aplazadas.
pub fn write<T>(l: &std::sync::RwLock<T>) -> Locked<std::sync::RwLockWriteGuard<'_, T>> {
    crit_enter();
    Locked { g: Some(l.write().unwrap_or_else(|e| e.into_inner())) }
}

/// true si este hilo tiene un bloqueo del monitor tomado (no se puede ejecutar un manejador guest ahora).
pub fn in_critical() -> bool {
    TM.try_with(|t| t.crit.get() > 0).unwrap_or(false)
}

/// Llamar desde el manejador host de una senal asincrona. Si el hilo esta en seccion critica, anota la senal (con su
/// `siginfo`) para reenviarla al salir y devuelve true (el manejador debe volver sin hacer nada mas).
/// Como en el nucleo, las senales estandar iguales pendientes se funden en una: se conserva el primer `siginfo`.
/// Tambien si el hilo esta a mitad de un store rapido (modo fence, `MonFlag::in_store`): entonces marca
/// `MonFlag::pending` para que la salida del store la reenvie.
///
/// # Safety
/// `info` es nulo o apunta a un `siginfo` legible de 128 bytes.
pub unsafe fn defer_signal(sig: i32, info: *const u8) -> bool {
    TM.try_with(|t| {
        if t.crit.get() == 0 {
            let f = t.flag.get();
            if f.is_null() || unsafe { (*f).in_store.load(Relaxed) } == 0 {
                return false;
            }
            unsafe { (*f).pending.store(1, Relaxed) };
            #[cfg(test)]
            DEFER_IN_STORE.fetch_add(1, Relaxed);
        }
        if !(1..=64).contains(&sig) {
            return false;
        }
        // bit e indice sig-1: la senal 64 (SIGRTMAX) cabe (con `sig & 63` caia en el 0 y se perdia)
        let (n, bit) = (sig as usize - 1, 1u64 << (sig as u32 - 1));
        if t.pending.get() & bit == 0 && !info.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(info, (*t.info.get())[n].as_mut_ptr(), 128) };
        } else if t.pending.get() & bit != 0 && sig as u32 >= crate::sig::rtmin() {
            // otra instancia de una senal de tiempo real: se encola con su siginfo y se reenvia despues de la primera
            let mut tmp = [0u8; 128];
            if info.is_null() {
                tmp[0..4].copy_from_slice(&sig.to_ne_bytes());
            } else {
                unsafe { std::ptr::copy_nonoverlapping(info, tmp.as_mut_ptr(), 128) };
            }
            t.rtq.push(sig as u32, &tmp);
        }
        t.pending.set(t.pending.get() | bit);
        true
    })
    .unwrap_or(false)
}

#[cfg(test)]
pub static DEFER_IN_STORE: AtomicU64 = AtomicU64::new(0);

/// Salida de un store rapido con senales aplazadas (`MonFlag::pending`): las reenvia (si no hay bloqueo tomado; si
/// lo hubiera, lo hara la salida de la seccion critica).
extern "C" fn fst_flush(f: *const MonFlag) {
    unsafe { (*f).pending.store(0, Relaxed) };
    let _ = TM.try_with(|t| {
        if t.crit.get() == 0 {
            flush_pending(t);
        }
    });
}

#[cold]
fn flush_pending(t: &ThreadMon) {
    let f = t.flag.get();
    if !f.is_null() {
        unsafe { (*f).pending.store(0, Relaxed) };
    }
    let mask = t.pending.replace(0);
    if mask == 0 {
        return;
    }
    unsafe {
        let (pid, tid) = (crate::sys::syscall(39), crate::sys::syscall(186));
        for sig in 1..=64usize {
            if mask & (1u64 << (sig - 1)) == 0 {
                continue;
            }
            // rt_tgsigqueueinfo con el siginfo original (si_code, si_pid, si_value...): el guest ve la senal tal
            // como la habria visto sin el aplazamiento. El nucleo respeta la mascara del hilo.
            let info = (*t.info.get())[sig - 1];
            let has_info = info[0..4] == (sig as i32).to_ne_bytes();
            if !has_info || crate::sys::syscall(297, pid, tid, sig as i64, info.as_ptr()) < 0 {
                crate::sys::syscall(234, pid, tid, sig as i64); // tgkill
            }
            // las demas instancias de una de tiempo real, en orden (el nucleo las encola)
            while let Some(more) = t.rtq.pop(sig as u32) {
                if crate::sys::syscall(297, pid, tid, sig as i64, more.as_ptr()) < 0 {
                    crate::sys::syscall(234, pid, tid, sig as i64);
                }
            }
            std::ptr::write_bytes((*t.info.get())[sig - 1].as_mut_ptr(), 0, 4);
        }
    }
}

/// Epoca global: cada enfriamiento la incrementa e invalida toda reserva pendiente
/// (un fallo espurio de STXR esta permitido por la arquitectura).
pub static EPOCH: AtomicU64 = AtomicU64::new(1);

// ===========================================================================
// Tabla de granulos calientes
// ===========================================================================

pub const HOT_BITS: usize = 16;
pub static HOT: [AtomicU32; 1 << HOT_BITS] = [const { AtomicU32::new(0) }; 1 << HOT_BITS];

/// Etiqueta de cada entrada de HOT: la linea de 64 B (`addr >> 6`) que la armo, `TAG_MULTI` si la armaron varias
/// lineas (o la tabla esta saturada) y 0 si no se sabe (entrada fria o recien enfriada). La entrada se indexa con 16
/// bits de la linea: las lineas separadas por 4 MiB comparten entrada, y sin etiqueta un store a una linea que nunca
/// hizo LDXR iba por la ruta con bloqueo si otra linea de su entrada estaba armada (con la tabla llena, la mayoria de
/// los stores: ~30 % de la CPU de un juego Unity). El store rapido del modo fence (`heddle_fst*`) con la entrada
/// caliente mira la etiqueta: si es otra linea concreta, hace el mov con su marca puesta, como con la entrada fria.
/// Es exacto por lo mismo que el armado: la etiqueta se escribe ANTES de A1 (armado nuevo) o pasa a `TAG_MULTI` con la
/// misma barrera que un armado (entrada caliente que arma otra linea), siempre antes de que la reserva lea version y
/// dato; un store que vio la etiqueta vieja tiene la marca puesta y el armador la espera (ver la cabecera).
pub static HOT_TAG: [AtomicU64; 1 << HOT_BITS] = [const { AtomicU64::new(0) }; 1 << HOT_BITS];
pub const TAG_MULTI: u64 = u64::MAX;

/// La entrada de `addr`, caliente, la armo otra linea concreta: la etiqueta pasa a TAG_MULTI con barrera (A2 + A3)
/// antes de que la reserva lea. Con arm_lock.
fn tag_collision_locked(addr: usize) {
    let i = hot_idx(addr);
    let t = HOT_TAG[i].load(Acquire);
    if t != (addr >> 6) as u64 && t != TAG_MULTI && HOT[i].load(Acquire) & 0xFF == 2 {
        HOT_TAG[i].store(TAG_MULTI, SeqCst);
        arm_barrier();
    }
}
static ARM_LOCK: AtomicBool = AtomicBool::new(false);
static HOT_COUNT: AtomicUsize = AtomicUsize::new(0);
pub const DEFAULT_THRESH: usize = 49152;
pub static COOL_THRESH: AtomicUsize = AtomicUsize::new(DEFAULT_THRESH);

struct HotList(UnsafeCell<Vec<u32>>);
unsafe impl Sync for HotList {}
static HOT_LIST: HotList = HotList(UnsafeCell::new(Vec::new()));

#[inline]
pub fn hot_idx(addr: usize) -> usize {
    (addr >> 6) & ((1 << HOT_BITS) - 1)
}

#[inline]
fn stamp_of(e: u64) -> u32 {
    ((e as u32) & 0x00FF_FFFF) << 8
}

fn arm_lock() {
    crit_enter();
    spin_acquire(&ARM_LOCK);
}

fn arm_unlock() {
    ARM_LOCK.store(false, Release);
    crit_exit();
}

fn cool_locked(aggr: bool) -> usize {
    if SATURATED.load(Acquire) {
        return 0;
    }
    let new = EPOCH.fetch_add(1, SeqCst) + 1;
    let keep_a = stamp_of(new);
    let keep_b = if aggr { keep_a } else { stamp_of(new - 1) };
    let list = unsafe { &mut *HOT_LIST.0.get() };
    let mut cleared = 0usize;
    list.retain(|&i| {
        let h = &HOT[i as usize];
        let cur = h.load(Acquire);
        let old = |c: u32| c & 0xFF == 2 && (c & 0xFFFF_FF00) != keep_a && (c & 0xFFFF_FF00) != keep_b;
        if old(cur) {
            let g = &GRAN[i as usize & (NGRAN - 1)];
            glock(g);
            let cur = h.load(Acquire);
            let ok = old(cur) && h.compare_exchange(cur, 0, AcqRel, Relaxed).is_ok();
            gunlock(g);
            if ok {
                HOT_TAG[i as usize].store(0, Release);
                cleared += 1;
                return false;
            }
        }
        true
    });
    HOT_COUNT.fetch_sub(cleared, Relaxed);
    COOLS.fetch_add(1, Relaxed);
    cleared
}

/// Enfria todo granulo no usado desde la ultima llamada.
pub fn cool() -> usize {
    arm_lock();
    let n = cool_locked(true);
    arm_unlock();
    n
}

/// Solo para pruebas (sin otros hilos con reservas vivas): tabla completamente fria.
pub fn reset_hot() {
    arm_lock();
    if SATURATED.load(Acquire) {
        EPOCH.fetch_add(1, SeqCst);
        for h in HOT.iter() {
            h.store(0, Release);
        }
        for t in HOT_TAG.iter() {
            t.store(0, Release);
        }
        unsafe {
            (*HOT_LIST.0.get()).clear();
            *COOL_HIST.0.get() = [None; 3];
        }
        HOT_COUNT.store(0, Relaxed);
        SATURATED.store(false, Release);
    } else {
        cool_locked(true);
    }
    arm_unlock();
}

fn saturate_locked() {
    // saturada, ensure_hot ya no arma ni etiqueta: todo store con la entrada caliente va por la ruta con bloqueo
    for t in HOT_TAG.iter() {
        t.store(TAG_MULTI, Relaxed);
    }
    for h in HOT.iter() {
        if h.load(Relaxed) & 0xFF == 0 {
            h.store(1, Relaxed);
        }
    }
    std::sync::atomic::fence(SeqCst);
    arm_barrier();
    let st = stamp_of(EPOCH.load(Acquire)) | 2;
    for h in HOT.iter() {
        if h.load(Relaxed) & 0xFF == 1 {
            h.store(st, Release);
        }
    }
    SATURATED.store(true, SeqCst);
    SATURATIONS.fetch_add(1, Relaxed);
}

struct CoolHist(UnsafeCell<[Option<(Instant, u64)>; 3]>);
unsafe impl Sync for CoolHist {}
static COOL_HIST: CoolHist = CoolHist(UnsafeCell::new([None; 3]));
const SAT_FRACTION: f64 = 0.25;

fn note_pressure_cool() {
    let h = unsafe { &mut *COOL_HIST.0.get() };
    let now = Instant::now();
    let mb = MB_NS.load(Relaxed);
    if let Some((t0, mb0)) = h[0] {
        let dt = now.duration_since(t0).as_nanos() as f64;
        if dt > 0.0 && (mb - mb0) as f64 / dt > SAT_FRACTION {
            saturate_locked();
            *h = [None; 3];
            return;
        }
    }
    *h = [h[1], h[2], Some((now, mb))];
}

/// Garantiza que el granulo de `addr` este caliente y sellado con la epoca `e`.
fn ensure_hot(addr: usize, e: u64) {
    let h = &HOT[hot_idx(addr)];
    let want = stamp_of(e) | 2;
    loop {
        if SATURATED.load(Acquire) {
            return;
        }
        let cur = h.load(Acquire);
        if cur & 0xFF == 2 {
            let t = HOT_TAG[hot_idx(addr)].load(Acquire);
            if t != (addr >> 6) as u64 && t != TAG_MULTI {
                arm_lock();
                tag_collision_locked(addr);
                arm_unlock();
                continue;
            }
        }
        if cur == want {
            return;
        }
        if cur & 0xFF == 2 {
            if h.compare_exchange(cur, want, AcqRel, Relaxed).is_ok() {
                return;
            }
            continue;
        }
        arm_lock();
        let cur = h.load(Relaxed);
        if cur & 0xFF == 0 && !SATURATED.load(Acquire) {
            if HOT_COUNT.load(Relaxed) >= COOL_THRESH.load(Relaxed) {
                cool_locked(false);
                if HOT_COUNT.load(Relaxed) * 4 >= COOL_THRESH.load(Relaxed) * 3 {
                    cool_locked(true);
                }
                note_pressure_cool();
                if SATURATED.load(Acquire) {
                    arm_unlock();
                    return;
                }
            }
            HOT_TAG[hot_idx(addr)].store((addr >> 6) as u64, SeqCst); // antes de A1 (ver HOT_TAG)
            h.store(1, SeqCst); // A1 (ARMANDO)
            arm_barrier(); // A2 + A3
            h.store(stamp_of(EPOCH.load(Acquire)) | 2, Release);
            unsafe { (*HOT_LIST.0.get()).push(hot_idx(addr) as u32) };
            HOT_COUNT.fetch_add(1, Relaxed);
        }
        arm_unlock();
    }
}

// ===========================================================================
// Lectura / escritura de memoria guest (direccion guest == direccion host)
// ===========================================================================

#[inline(always)]
pub unsafe fn rd(addr: usize, size: u8) -> u64 {
    match size {
        0 => std::ptr::read_unaligned(addr as *const u8) as u64,
        1 => std::ptr::read_unaligned(addr as *const u16) as u64,
        2 => std::ptr::read_unaligned(addr as *const u32) as u64,
        _ => std::ptr::read_unaligned(addr as *const u64),
    }
}

#[inline(always)]
unsafe fn wr_raw(addr: usize, size: u8, v: u64) {
    match size {
        0 => std::ptr::write_unaligned(addr as *mut u8, v as u8),
        1 => std::ptr::write_unaligned(addr as *mut u16, v as u16),
        2 => std::ptr::write_unaligned(addr as *mut u32, v as u32),
        _ => std::ptr::write_unaligned(addr as *mut u64, v),
    }
}

/// Comprueba, ANTES de tomar un bloqueo, que se puede escribir en [addr, addr+n) (n <= 16): un `lock or` de 0 sobre
/// el primer byte (y el ultimo si cruza de pagina) no cambia nada, es atomico frente a cualquier otra escritura y
/// falla igual que fallaria el store. Asi el fallo de un store guest a memoria no escribible llega al manejador con el
/// hilo fuera de seccion critica y sin bloqueos tomados (se puede reanudar; ver sig::sync_fault), y no con un bloqueo
/// de granulo retenido para siempre.
#[inline(always)]
unsafe fn probe_w(addr: usize, n: usize) {
    let r = TM.try_with(|t| unsafe { probe_w_t(t, addr, n) });
    if r.is_err() {
        unsafe { probe_w_raw(addr, n) };
    }
}

unsafe fn probe_w_raw(addr: usize, n: usize) {
    asm!("lock or byte ptr [{a}], 0", a = in(reg) addr, options(nostack));
    if (addr & 0xFFF) + n > 0x1000 {
        asm!("lock or byte ptr [{a}], 0", a = in(reg) addr + n - 1, options(nostack));
    }
}

/// Generacion de los mapeos del guest: sube con cada mmap/munmap/mprotect/mremap del guest (`prot_changed`) e invalida
/// las paginas que cada hilo ya comprobo escribibles (`ThreadMon::wpages`).
static PROT_GEN: AtomicUsize = AtomicUsize::new(1);

/// El guest cambio mapeos o protecciones: las paginas comprobadas por `probe_w` dejan de valer.
pub fn prot_changed() {
    PROT_GEN.fetch_add(1, Release);
}

/// `probe_w` con una cache de 4 paginas por hilo ya comprobadas escribibles en la generacion actual: el `lock or` cuesta
/// tanto como el propio bloqueo y en modo `bloqueo` va en cada store (fib(35): +44 % con la sonda en cada uno). Una
/// pagina que otro hilo protege entre la comprobacion y la escritura falla con el bloqueo tomado, igual que si el
/// mprotect llegara entre la sonda y la escritura sin cache (la ventana es la misma: la generacion sube al volver
/// del mprotect). Los cambios de proteccion que haga el host por su cuenta no se ven (no los hace sobre memoria guest
/// en uso: el recolector de ART usa userfaultfd).
#[inline]
unsafe fn probe_w_t(t: &ThreadMon, addr: usize, n: usize) {
    let pg = addr >> 12;
    let single = (addr + n - 1) >> 12 == pg;
    let g = PROT_GEN.load(Acquire);
    let mut c = t.wpages.get();
    if c[4] != g {
        c = [0, 0, 0, 0, g];
    }
    let i = pg & 3;
    if single && c[i] == pg + 1 {
        return;
    }
    unsafe { probe_w_raw(addr, n) };
    if single {
        c[i] = pg + 1;
    }
    t.wpages.set(c);
}

/// Camino lento: toma el lock del/los granulos, escribe e incrementa la version.
unsafe fn slow_store(addr: usize, size: u8, v: u64) {
    let nbytes = 1usize << size;
    let g1 = gran(addr);
    let g2 = gran(addr + nbytes - 1);
    // el store va por `lk_store`: si falla, suelta los bloqueos y repite como sonda sin bloqueo (ver "Accesos a
    // memoria guest con un bloqueo de granulo tomado"); false = la pagina ya era escribible al repetir: otra vez
    if std::ptr::eq(g1, g2) {
        // camino caliente en FALLBACK: un solo acceso TLS para contador de seccion critica, cadena propia y salida
        let r = TM.try_with(|t| {
            t.crit.set(t.crit.get() + 1);
            spin_acquire(&g1.lock);
            if !lk_store(addr, size, v, &g1.lock, std::ptr::null()) {
                return false;
            }
            bump_own_t(t, g1, addr, nbytes);
            g1.lock.store(false, Release);
            crit_leave(t);
            true
        });
        match r {
            Ok(true) => {}
            Ok(false) => slow_store_again(addr, size, v),
            Err(_) => {
                // hilo en destruccion (TLS ya liberada): sin seccion critica
                glock(g1);
                if !lk_store(addr, size, v, &g1.lock, std::ptr::null()) {
                    return slow_store_again(addr, size, v);
                }
                bump_own(g1, addr, nbytes);
                gunlock(g1);
            }
        }
    } else {
        // acceso que cruza dos granulos: tomar ambos en orden de direccion
        let (a, b) = if (g1 as *const Granule) < (g2 as *const Granule) { (g1, g2) } else { (g2, g1) };
        glock(a);
        glock(b);
        if !lk_store(addr, size, v, &a.lock, &b.lock) {
            return slow_store_again(addr, size, v);
        }
        bump_own(g1, addr, nbytes);
        bump_own(g2, addr, nbytes);
        gunlock(b);
        gunlock(a);
    }
}

/// `slow_store` otra vez, tras un fallo del store con bloqueo ya resuelto (fuera de la ruta caliente).
#[cold]
#[inline(never)]
unsafe fn slow_store_again(addr: usize, size: u8, v: u64) {
    unsafe { slow_store(addr, size, v) }
}

// ===========================================================================
// Store rapido del modo rseq
// ===========================================================================
//
// `heddle_rst{0..3}`: RDI = direccion, RDX = valor (`size` = log2 de bytes en el nombre). El JIT los llama
// directamente (`fast_store_entry`); desde Rust, `rseq_store`. Seccion critica rseq [start, end): leer HOT del
// granulo y, si es FRIO, el mov del dato. Un armado (membarrier PRIVATE_EXPEDITED_RSEQ), una expropiacion o una
// senal dentro de la seccion la abortan: el nucleo salta a `abort` (precedido de la firma RSEQ_SIG de glibc), que
// cuenta el aborto y la repite entera. Un fallo del mov (SIGSEGV del guest) tambien aborta: el manejador ve el rip
// de `abort` y, si vuelve, el store se repite. Granulo caliente/armando, acceso que cruza el granulo o FALLBACK
// (hilo sin rseq): `fst_slow` (ruta con bloqueo). El descriptor `rseq_cs` queda puesto al salir (el nucleo solo mira
// si el rip esta dentro del intervalo). Clobber: RAX, RCX, RSI, R9 y lo que use `fst_slow` (ABI de C).
//
// Alineacion: cada entrada empieza en una linea de 64 B (`.p2align 6`; ver "Alineacion de las rutas calientes" en la
// cabecera): la ruta caliente ocupa siempre las mismas lineas y ventanas de 32 B, sin saltos que crucen una ventana
// (errata JCC de Skylake). La prueba `rutas_calientes_alineadas` lo comprueba.
std::arch::global_asm!(
    ".pushsection .text.heddle_rst,\"ax\",@progbits",
    ".p2align 6",
    ".globl heddle_rst0",
    ".hidden heddle_rst0",
    "heddle_rst0:",
    "cmp byte ptr [rip + {fallback}], 0",
    "jne .Lrst0_slow",
    "mov rax, qword ptr [rip + {off}]",
    "mov ecx, edi",
    "shr ecx, 6",
    "movzx ecx, cx",
    "lea rsi, [rip + {hot}]",
    ".Lrst0_arm:",
    "lea r9, [rip + .Lrst0_cs]",
    "mov qword ptr fs:[rax + 8], r9",
    ".Lrst0_start:",
    "cmp byte ptr [rsi + rcx*4], 0",
    "jne .Lrst0_slow",
    "mov [rdi], dl",
    ".Lrst0_end:",
    "ret",
    ".long 0x53053053",
    ".Lrst0_abort:",
    "lock inc qword ptr [rip + {aborts}]",
    "jmp .Lrst0_arm",
    ".Lrst0_slow:",
    "mov esi, 0",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_rst1",
    ".hidden heddle_rst1",
    "heddle_rst1:",
    "cmp byte ptr [rip + {fallback}], 0",
    "jne .Lrst1_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 62",
    "ja .Lrst1_slow",
    "mov rax, qword ptr [rip + {off}]",
    "mov ecx, edi",
    "shr ecx, 6",
    "movzx ecx, cx",
    "lea rsi, [rip + {hot}]",
    ".Lrst1_arm:",
    "lea r9, [rip + .Lrst1_cs]",
    "mov qword ptr fs:[rax + 8], r9",
    ".Lrst1_start:",
    "cmp byte ptr [rsi + rcx*4], 0",
    "jne .Lrst1_slow",
    "mov [rdi], dx",
    ".Lrst1_end:",
    "ret",
    ".long 0x53053053",
    ".Lrst1_abort:",
    "lock inc qword ptr [rip + {aborts}]",
    "jmp .Lrst1_arm",
    ".Lrst1_slow:",
    "mov esi, 1",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_rst2",
    ".hidden heddle_rst2",
    "heddle_rst2:",
    "cmp byte ptr [rip + {fallback}], 0",
    "jne .Lrst2_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 60",
    "ja .Lrst2_slow",
    "mov rax, qword ptr [rip + {off}]",
    "mov ecx, edi",
    "shr ecx, 6",
    "movzx ecx, cx",
    "lea rsi, [rip + {hot}]",
    ".Lrst2_arm:",
    "lea r9, [rip + .Lrst2_cs]",
    "mov qword ptr fs:[rax + 8], r9",
    ".Lrst2_start:",
    "cmp byte ptr [rsi + rcx*4], 0",
    "jne .Lrst2_slow",
    "mov [rdi], edx",
    ".Lrst2_end:",
    "ret",
    ".long 0x53053053",
    ".Lrst2_abort:",
    "lock inc qword ptr [rip + {aborts}]",
    "jmp .Lrst2_arm",
    ".Lrst2_slow:",
    "mov esi, 2",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_rst3",
    ".hidden heddle_rst3",
    "heddle_rst3:",
    "cmp byte ptr [rip + {fallback}], 0",
    "jne .Lrst3_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 56",
    "ja .Lrst3_slow",
    "mov rax, qword ptr [rip + {off}]",
    "mov ecx, edi",
    "shr ecx, 6",
    "movzx ecx, cx",
    "lea rsi, [rip + {hot}]",
    ".Lrst3_arm:",
    "lea r9, [rip + .Lrst3_cs]",
    "mov qword ptr fs:[rax + 8], r9",
    ".Lrst3_start:",
    "cmp byte ptr [rsi + rcx*4], 0",
    "jne .Lrst3_slow",
    "mov [rdi], rdx",
    ".Lrst3_end:",
    "ret",
    ".long 0x53053053",
    ".Lrst3_abort:",
    "lock inc qword ptr [rip + {aborts}]",
    "jmp .Lrst3_arm",
    ".Lrst3_slow:",
    "mov esi, 3",
    "jmp {slow}",
    ".popsection",
    ".pushsection .data.rel.ro,\"aw\"",
    ".balign 32",
    ".Lrst0_cs:",
    ".long 0, 0",
    ".quad .Lrst0_start",
    ".quad .Lrst0_end - .Lrst0_start",
    ".quad .Lrst0_abort",
    ".balign 32",
    ".Lrst1_cs:",
    ".long 0, 0",
    ".quad .Lrst1_start",
    ".quad .Lrst1_end - .Lrst1_start",
    ".quad .Lrst1_abort",
    ".balign 32",
    ".Lrst2_cs:",
    ".long 0, 0",
    ".quad .Lrst2_start",
    ".quad .Lrst2_end - .Lrst2_start",
    ".quad .Lrst2_abort",
    ".balign 32",
    ".Lrst3_cs:",
    ".long 0, 0",
    ".quad .Lrst3_start",
    ".quad .Lrst3_end - .Lrst3_start",
    ".quad .Lrst3_abort",
    ".popsection",
    fallback = sym FALLBACK,
    off = sym RSEQ_OFFSET,
    hot = sym HOT,
    aborts = sym RSEQ_ABORTS,
    slow = sym fst_slow,
);

extern "C" {
    fn heddle_rst0();
    fn heddle_rst1();
    fn heddle_rst2();
    fn heddle_rst3();
}

// ===========================================================================
// Store rapido del modo fence (ver la cabecera: pasos S1-S4)
// ===========================================================================
//
// `heddle_fst{0..3}`: RDI = direccion, RDX = valor, RCX = *const MonFlag (0 = sin marca: ruta con bloqueo). El JIT
// los llama con RCX = cpu.monflag; desde Rust, `fence_store`. Sin instrucciones atomicas ni barreras. Un acceso que
// cruza el granulo de 64 B va por la ruta con bloqueo. `_r` es el punto de reinicio (S1) y `_m` el mov del dato:
// si el mov falla (SIGSEGV del guest), `fault_in_store` pone in_store = 0 y mueve el rip a `_r`, de modo que el
// manejador guest corre sin la marca y, si vuelve, el store se repite entero (igual que en hardware, donde un store
// que falla no se ejecuta).
std::arch::global_asm!(
    ".pushsection .text.heddle_fst,\"ax\",@progbits",
    ".p2align 6",
    ".globl heddle_fst0",
    ".hidden heddle_fst0",
    "heddle_fst0:",
    "test rcx, rcx",
    "jz .Lfst0_slow",
    ".globl heddle_fst0_r",
    ".hidden heddle_fst0_r",
    "heddle_fst0_r:",
    "mov byte ptr [rcx], 1",
    "mov eax, edi",
    "shr eax, 6",
    "movzx eax, ax",
    "lea r8, [rip + {hot}]",
    "cmp byte ptr [r8 + rax*4], 0",
    "jne .Lfst0_hot",
    ".globl heddle_fst0_m",
    ".hidden heddle_fst0_m",
    "heddle_fst0_m:",
    "mov [rdi], dl",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfst0_fl",
    "ret",
    ".Lfst0_fl:",
    "mov rdi, rcx",
    "jmp {flush}",
    ".Lfst0_hot:",
    // entrada caliente: si su etiqueta es otra linea concreta (ni esta, ni 0, ni TAG_MULTI), mov con la marca puesta
    "lea r9, [rip + {tag}]",
    "mov r9, [r9 + rax*8]",
    "mov r10, rdi",
    "shr r10, 6",
    "cmp r9, r10",
    "je .Lfst0_hot2",
    "lea r10, [r9 + 1]",
    "cmp r10, 1",
    "jbe .Lfst0_hot2",
    "jmp heddle_fst0_m",
    ".Lfst0_hot2:",
    "mov byte ptr [rcx], 0",
    ".Lfst0_slow:",
    "mov esi, 0",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_fst1",
    ".hidden heddle_fst1",
    "heddle_fst1:",
    "test rcx, rcx",
    "jz .Lfst1_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 62",
    "ja .Lfst1_slow",
    ".globl heddle_fst1_r",
    ".hidden heddle_fst1_r",
    "heddle_fst1_r:",
    "mov byte ptr [rcx], 1",
    "mov eax, edi",
    "shr eax, 6",
    "movzx eax, ax",
    "lea r8, [rip + {hot}]",
    "cmp byte ptr [r8 + rax*4], 0",
    "jne .Lfst1_hot",
    ".globl heddle_fst1_m",
    ".hidden heddle_fst1_m",
    "heddle_fst1_m:",
    "mov [rdi], dx",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfst1_fl",
    "ret",
    ".Lfst1_fl:",
    "mov rdi, rcx",
    "jmp {flush}",
    ".Lfst1_hot:",
    // entrada caliente: si su etiqueta es otra linea concreta (ni esta, ni 0, ni TAG_MULTI), mov con la marca puesta
    "lea r9, [rip + {tag}]",
    "mov r9, [r9 + rax*8]",
    "mov r10, rdi",
    "shr r10, 6",
    "cmp r9, r10",
    "je .Lfst1_hot2",
    "lea r10, [r9 + 1]",
    "cmp r10, 1",
    "jbe .Lfst1_hot2",
    "jmp heddle_fst1_m",
    ".Lfst1_hot2:",
    "mov byte ptr [rcx], 0",
    ".Lfst1_slow:",
    "mov esi, 1",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_fst2",
    ".hidden heddle_fst2",
    "heddle_fst2:",
    "test rcx, rcx",
    "jz .Lfst2_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 60",
    "ja .Lfst2_slow",
    ".globl heddle_fst2_r",
    ".hidden heddle_fst2_r",
    "heddle_fst2_r:",
    "mov byte ptr [rcx], 1",
    "mov eax, edi",
    "shr eax, 6",
    "movzx eax, ax",
    "lea r8, [rip + {hot}]",
    "cmp byte ptr [r8 + rax*4], 0",
    "jne .Lfst2_hot",
    ".globl heddle_fst2_m",
    ".hidden heddle_fst2_m",
    "heddle_fst2_m:",
    "mov [rdi], edx",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfst2_fl",
    "ret",
    ".Lfst2_fl:",
    "mov rdi, rcx",
    "jmp {flush}",
    ".Lfst2_hot:",
    // entrada caliente: si su etiqueta es otra linea concreta (ni esta, ni 0, ni TAG_MULTI), mov con la marca puesta
    "lea r9, [rip + {tag}]",
    "mov r9, [r9 + rax*8]",
    "mov r10, rdi",
    "shr r10, 6",
    "cmp r9, r10",
    "je .Lfst2_hot2",
    "lea r10, [r9 + 1]",
    "cmp r10, 1",
    "jbe .Lfst2_hot2",
    "jmp heddle_fst2_m",
    ".Lfst2_hot2:",
    "mov byte ptr [rcx], 0",
    ".Lfst2_slow:",
    "mov esi, 2",
    "jmp {slow}",
    ".p2align 6",
    ".globl heddle_fst3",
    ".hidden heddle_fst3",
    "heddle_fst3:",
    "test rcx, rcx",
    "jz .Lfst3_slow",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 56",
    "ja .Lfst3_slow",
    ".globl heddle_fst3_r",
    ".hidden heddle_fst3_r",
    "heddle_fst3_r:",
    "mov byte ptr [rcx], 1",
    "mov eax, edi",
    "shr eax, 6",
    "movzx eax, ax",
    "lea r8, [rip + {hot}]",
    "cmp byte ptr [r8 + rax*4], 0",
    "jne .Lfst3_hot",
    ".globl heddle_fst3_m",
    ".hidden heddle_fst3_m",
    "heddle_fst3_m:",
    "mov [rdi], rdx",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfst3_fl",
    "ret",
    ".Lfst3_fl:",
    "mov rdi, rcx",
    "jmp {flush}",
    ".Lfst3_hot:",
    // entrada caliente: si su etiqueta es otra linea concreta (ni esta, ni 0, ni TAG_MULTI), mov con la marca puesta
    "lea r9, [rip + {tag}]",
    "mov r9, [r9 + rax*8]",
    "mov r10, rdi",
    "shr r10, 6",
    "cmp r9, r10",
    "je .Lfst3_hot2",
    "lea r10, [r9 + 1]",
    "cmp r10, 1",
    "jbe .Lfst3_hot2",
    "jmp heddle_fst3_m",
    ".Lfst3_hot2:",
    "mov byte ptr [rcx], 0",
    ".Lfst3_slow:",
    "mov esi, 3",
    "jmp {slow}",
    ".popsection",
    hot = sym HOT,
    tag = sym HOT_TAG,
    slow = sym fst_slow,
    flush = sym fst_flush,
);

extern "C" {
    fn heddle_fst0();
    fn heddle_fst1();
    fn heddle_fst2();
    fn heddle_fst3();
    fn heddle_fst0_r();
    fn heddle_fst1_r();
    fn heddle_fst2_r();
    fn heddle_fst3_r();
    fn heddle_fst0_m();
    fn heddle_fst1_m();
    fn heddle_fst2_m();
    fn heddle_fst3_m();
}

// ===========================================================================
// Accesos a memoria guest con un bloqueo de granulo tomado
// ===========================================================================
//
// Un acceso que falla con un bloqueo del monitor tomado no puede llegar al manejador guest (`in_critical`: el
// bloqueo quedaria retenido mientras dure el manejador, que puede dormir). Antes, cada STXR, store con bloqueo y RMW
// comprobaba la direccion fuera del bloqueo (`probe_w`, un `lock or` de 0, con cache por hilo): ~20 instrucciones
// por STXR y ~23 por store en modo bloqueo aun con la cache. Ahora el acceso bajo el bloqueo es una sola instruccion
// en linea con una entrada en la tabla de reparaciones `heddle_lkfix` (como las tablas de excepciones del nucleo:
// direccion de la instruccion y de su reparacion, relativas). Si falla (SIGSEGV/SIGBUS), `fault_in_locked` (al
// principio de `sig::sync_fault`, antes de mirar `in_critical`) solo cambia el rip a la reparacion, que devuelve
// "fallo": x86 no completa una instruccion que falla, asi que no se escribio nada. Ya fuera del manejador, quien hizo
// el acceso suelta los bloqueos y la seccion critica y repite el acceso como sonda sin bloqueo (`lk_fault_exit`):
// ese fallo si llega al guest, con el pc de la instruccion, el hilo fuera de seccion critica y la direccion que
// fallo. Despues (pagina ya escribible: el manejador la arreglo u otro hilo cambio la proteccion) repite la operacion
// entera desde el principio (no escribio nada y la version no subio).

/// Inicio y fin de la tabla de reparaciones (los define el enlazador): pares (instruccion - &par.0, reparacion -
/// &par.1).
extern "C" {
    static __start_heddle_lkfix: [i32; 0];
    static __stop_heddle_lkfix: [i32; 0];
}

/// Acceso `$insn` a memoria guest con entrada en `heddle_lkfix`: `$st` = 0 si se hizo, 1 si fallo (reparacion).
macro_rules! lk_access {
    ($insn:literal, $($args:tt)*) => {
        asm!(
            "2:",
            $insn,
            "xor {st:e}, {st:e}",
            "3:",
            ".pushsection heddle_lkfix,\"aR\",@progbits",
            ".balign 4",
            ".long 2b - .",
            ".long 4f - .",
            ".popsection",
            ".pushsection .text.heddle_lkfix,\"ax\",@progbits",
            "4:",
            "mov {st:e}, 1",
            "jmp 3b",
            ".popsection",
            $($args)*
            options(nostack),
        )
    };
}

/// Lectura de una instruccion guest que tolera un fallo (el traductor y el interprete leen el codigo asi). None si
/// la pagina no se puede leer: otro hilo la desmapeo despues de la comprobacion, o nunca lo estuvo. El fallo lo
/// resuelve `fault_in_locked` (tabla `heddle_lkfix`), lo primero de `sig::sync_fault`.
#[inline]
pub fn read_code(addr: u64) -> Option<u32> {
    let (v, st): (u32, u32);
    unsafe { lk_access!("mov {v:e}, dword ptr [{a}]", a = in(reg) addr, v = out(reg) v, st = out(reg) st,) };
    (st == 0).then_some(v)
}

/// Store de `size` en `addr` con el bloqueo `l1` (y `l2`, si no es nulo) tomado. false: fallo; los bloqueos y la
/// seccion critica ya se soltaron, no se escribio nada y la direccion ya es escribible (repetir la operacion entera).
#[inline(always)]
unsafe fn lk_store(addr: usize, size: u8, v: u64, l1: &AtomicBool, l2: *const AtomicBool) -> bool {
    let st: u32;
    match size {
        0 => lk_access!("mov byte ptr [{a}], {v}", a = in(reg) addr, v = in(reg_byte) v as u8, st = out(reg) st,),
        1 => lk_access!("mov word ptr [{a}], {v:x}", a = in(reg) addr, v = in(reg) v, st = out(reg) st,),
        2 => lk_access!("mov dword ptr [{a}], {v:e}", a = in(reg) addr, v = in(reg) v, st = out(reg) st,),
        _ => lk_access!("mov qword ptr [{a}], {v}", a = in(reg) addr, v = in(reg) v, st = out(reg) st,),
    }
    if st != 0 {
        lk_fault_exit(addr, 1usize << size, l1, l2);
        return false;
    }
    true
}

/// Carga de `size` en `addr` con el bloqueo `l1` tomado, para un RMW (la sonda tras un fallo es de escritura). None:
/// fallo, como en `lk_store`.
#[inline(always)]
unsafe fn lk_load(addr: usize, size: u8, l1: &AtomicBool) -> Option<u64> {
    let st: u32;
    let v: u64;
    match size {
        0 => lk_access!("movzx {v:e}, byte ptr [{a}]", a = in(reg) addr, v = out(reg) v, st = out(reg) st,),
        1 => lk_access!("movzx {v:e}, word ptr [{a}]", a = in(reg) addr, v = out(reg) v, st = out(reg) st,),
        2 => lk_access!("mov {v:e}, dword ptr [{a}]", a = in(reg) addr, v = out(reg) v, st = out(reg) st,),
        _ => lk_access!("mov {v}, qword ptr [{a}]", a = in(reg) addr, v = out(reg) v, st = out(reg) st,),
    }
    if st != 0 {
        lk_fault_exit(addr, 1usize << size, l1, std::ptr::null());
        return None;
    }
    Some(v)
}

/// Tras un acceso con bloqueo que fallo: suelta los bloqueos y la seccion critica (una entrada por bloqueo, como
/// `glock`) y repite el acceso como sonda sin bloqueo (este fallo llega al guest).
#[cold]
#[inline(never)]
fn lk_fault_exit(addr: usize, n: usize, l1: &AtomicBool, l2: *const AtomicBool) {
    unsafe {
        if !l2.is_null() {
            (*l2).store(false, Release);
            crit_exit();
        }
        l1.store(false, Release);
        crit_exit();
        probe_w_raw(addr, n);
    }
}

/// Llamar desde el manejador host de un fallo sincrono antes que nada (ver arriba): si el fallo es un acceso de la
/// tabla `heddle_lkfix`, el rip pasa a su reparacion y devuelve true (el manejador del host vuelve sin mas). Sin
/// reservas ni bloqueos.
///
/// # Safety
/// `uc` es nulo o el ucontext que el nucleo entrego al manejador.
pub unsafe fn fault_in_locked(uc: *mut std::ffi::c_void) -> bool {
    if uc.is_null() {
        return false;
    }
    let rip = (uc as *mut u8).add(40 + 8 * 16) as *mut u64;
    let r = *rip as usize;
    let mut e = std::ptr::addr_of!(__start_heddle_lkfix) as *const i32;
    let end = std::ptr::addr_of!(__stop_heddle_lkfix) as *const i32;
    while e < end {
        if (e as usize).wrapping_add(*e as isize as usize) == r {
            let f = e.add(1);
            *rip = (f as usize).wrapping_add(*f as isize as usize) as u64;
            return true;
        }
        e = e.add(2);
    }
    false
}

// ===========================================================================
// CAS rapido del modo fence para las atomicas LSE (ver `rmw`)
// ===========================================================================
//
// `heddle_fcas{0..3}`: RDI = direccion, RSI = valor esperado, RDX = valor nuevo, RCX = *const MonFlag (no nula).
// Devuelve en R8: 0 = escrito (RAX = esperado), 1 = el valor no era el esperado (RAX = valor actual, sin escribir),
// 2 = granulo caliente/armando o acceso que cruza el granulo (sin tocar memoria: ruta con bloqueo). Misma ventana que
// el store rapido (pasos S1-S4 de la cabecera) con un `lock cmpxchg` en lugar del mov: dentro de la marca solo hay
// esas instrucciones (sin llamadas, bloqueos ni esperas). `_r` es el reinicio y `_m` la unica instruccion que toca
// memoria guest: si falla (SIGSEGV del guest) `fault_in_store` limpia la marca y vuelve a `_r` (x86 no escribe nada
// en un cmpxchg que falla); RDI, RSI, RDX y RCX no cambian dentro de la rutina.
std::arch::global_asm!(
    ".pushsection .text.heddle_fcas,\"ax\",@progbits",
    ".p2align 6",
    ".globl heddle_fcas0",
    ".hidden heddle_fcas0",
    "heddle_fcas0:",
    ".globl heddle_fcas0_r",
    ".hidden heddle_fcas0_r",
    "heddle_fcas0_r:",
    "mov byte ptr [rcx], 1",
    "mov r9d, edi",
    "shr r9d, 6",
    "movzx r9d, r9w",
    "lea r8, [rip + {hot}]",
    "mov rax, rsi",
    "cmp byte ptr [r8 + r9*4], 0",
    "jne .Lfcas0_hot",
    ".globl heddle_fcas0_m",
    ".hidden heddle_fcas0_m",
    "heddle_fcas0_m:",
    "lock cmpxchg [rdi], dl",
    "jne .Lfcas0_ne",
    "xor r8d, r8d",
    ".Lfcas0_out:",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfcas0_fl",
    "ret",
    ".Lfcas0_ne:",
    "mov r8d, 1",
    "jmp .Lfcas0_out",
    ".Lfcas0_fl:",
    "push rax",
    "push r8",
    "sub rsp, 8",
    "mov rdi, rcx",
    "call {flush}",
    "add rsp, 8",
    "pop r8",
    "pop rax",
    "ret",
    ".Lfcas0_hot:",
    "mov byte ptr [rcx], 0",
    ".Lfcas0_slow:",
    "mov r8d, 2",
    "ret",
    ".p2align 6",
    ".globl heddle_fcas1",
    ".hidden heddle_fcas1",
    "heddle_fcas1:",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 62",
    "ja .Lfcas1_slow",
    ".globl heddle_fcas1_r",
    ".hidden heddle_fcas1_r",
    "heddle_fcas1_r:",
    "mov byte ptr [rcx], 1",
    "mov r9d, edi",
    "shr r9d, 6",
    "movzx r9d, r9w",
    "lea r8, [rip + {hot}]",
    "mov rax, rsi",
    "cmp byte ptr [r8 + r9*4], 0",
    "jne .Lfcas1_hot",
    ".globl heddle_fcas1_m",
    ".hidden heddle_fcas1_m",
    "heddle_fcas1_m:",
    "lock cmpxchg [rdi], dx",
    "jne .Lfcas1_ne",
    "xor r8d, r8d",
    ".Lfcas1_out:",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfcas1_fl",
    "ret",
    ".Lfcas1_ne:",
    "mov r8d, 1",
    "jmp .Lfcas1_out",
    ".Lfcas1_fl:",
    "push rax",
    "push r8",
    "sub rsp, 8",
    "mov rdi, rcx",
    "call {flush}",
    "add rsp, 8",
    "pop r8",
    "pop rax",
    "ret",
    ".Lfcas1_hot:",
    "mov byte ptr [rcx], 0",
    ".Lfcas1_slow:",
    "mov r8d, 2",
    "ret",
    ".p2align 6",
    ".globl heddle_fcas2",
    ".hidden heddle_fcas2",
    "heddle_fcas2:",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 60",
    "ja .Lfcas2_slow",
    ".globl heddle_fcas2_r",
    ".hidden heddle_fcas2_r",
    "heddle_fcas2_r:",
    "mov byte ptr [rcx], 1",
    "mov r9d, edi",
    "shr r9d, 6",
    "movzx r9d, r9w",
    "lea r8, [rip + {hot}]",
    "mov rax, rsi",
    "cmp byte ptr [r8 + r9*4], 0",
    "jne .Lfcas2_hot",
    ".globl heddle_fcas2_m",
    ".hidden heddle_fcas2_m",
    "heddle_fcas2_m:",
    "lock cmpxchg [rdi], edx",
    "jne .Lfcas2_ne",
    "xor r8d, r8d",
    ".Lfcas2_out:",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfcas2_fl",
    "ret",
    ".Lfcas2_ne:",
    "mov r8d, 1",
    "jmp .Lfcas2_out",
    ".Lfcas2_fl:",
    "push rax",
    "push r8",
    "sub rsp, 8",
    "mov rdi, rcx",
    "call {flush}",
    "add rsp, 8",
    "pop r8",
    "pop rax",
    "ret",
    ".Lfcas2_hot:",
    "mov byte ptr [rcx], 0",
    ".Lfcas2_slow:",
    "mov r8d, 2",
    "ret",
    ".p2align 6",
    ".globl heddle_fcas3",
    ".hidden heddle_fcas3",
    "heddle_fcas3:",
    "mov eax, edi",
    "and eax, 63",
    "cmp eax, 56",
    "ja .Lfcas3_slow",
    ".globl heddle_fcas3_r",
    ".hidden heddle_fcas3_r",
    "heddle_fcas3_r:",
    "mov byte ptr [rcx], 1",
    "mov r9d, edi",
    "shr r9d, 6",
    "movzx r9d, r9w",
    "lea r8, [rip + {hot}]",
    "mov rax, rsi",
    "cmp byte ptr [r8 + r9*4], 0",
    "jne .Lfcas3_hot",
    ".globl heddle_fcas3_m",
    ".hidden heddle_fcas3_m",
    "heddle_fcas3_m:",
    "lock cmpxchg [rdi], rdx",
    "jne .Lfcas3_ne",
    "xor r8d, r8d",
    ".Lfcas3_out:",
    "mov byte ptr [rcx], 0",
    "cmp byte ptr [rcx + 1], 0",
    "jne .Lfcas3_fl",
    "ret",
    ".Lfcas3_ne:",
    "mov r8d, 1",
    "jmp .Lfcas3_out",
    ".Lfcas3_fl:",
    "push rax",
    "push r8",
    "sub rsp, 8",
    "mov rdi, rcx",
    "call {flush}",
    "add rsp, 8",
    "pop r8",
    "pop rax",
    "ret",
    ".Lfcas3_hot:",
    "mov byte ptr [rcx], 0",
    ".Lfcas3_slow:",
    "mov r8d, 2",
    "ret",
    ".popsection",
    hot = sym HOT,
    flush = sym fst_flush,
);

extern "C" {
    fn heddle_fcas0();
    fn heddle_fcas1();
    fn heddle_fcas2();
    fn heddle_fcas3();
    fn heddle_fcas0_r();
    fn heddle_fcas1_r();
    fn heddle_fcas2_r();
    fn heddle_fcas3_r();
    fn heddle_fcas0_m();
    fn heddle_fcas1_m();
    fn heddle_fcas2_m();
    fn heddle_fcas3_m();
}

/// CAS rapido (modo fence) desde Rust: (estado, valor) con el estado de `heddle_fcas*`.
#[inline]
unsafe fn fence_cas(f: *const MonFlag, addr: usize, size: u8, old: u64, new: u64) -> (u64, u64) {
    let e: usize = match size {
        0 => (heddle_fcas0 as unsafe extern "C" fn()) as usize,
        1 => (heddle_fcas1 as unsafe extern "C" fn()) as usize,
        2 => (heddle_fcas2 as unsafe extern "C" fn()) as usize,
        _ => (heddle_fcas3 as unsafe extern "C" fn()) as usize,
    };
    let (st, v): (u64, u64);
    asm!("call {e}", e = in(reg) e, in("rdi") addr, in("rsi") old, in("rdx") new, in("rcx") f, lateout("r8") st, lateout("rax") v, clobber_abi("C"));
    (st, v)
}

/// Ruta con bloqueo desde el store rapido (granulo caliente/armando, acceso que cruza granulos o sin marca).
extern "C" fn fst_slow(addr: usize, size: u64, v: u64) {
    unsafe { slow_store(addr, size as u8, v) };
}

/// Entrada del store rapido para el JIT (`size` = log2 de bytes): (rutina, si quiere RCX = cpu.monflag). Modo fence:
/// `heddle_fst*` (con la marca); rseq: `heddle_rst*`; bloqueo: 0 (el JIT llama a `h_store`).
pub fn fast_store_entry(size: u8) -> (usize, bool) {
    match mode() {
        MODE_FENCE => {
            let e = match size {
                0 => (heddle_fst0 as unsafe extern "C" fn()) as usize,
                1 => (heddle_fst1 as unsafe extern "C" fn()) as usize,
                2 => (heddle_fst2 as unsafe extern "C" fn()) as usize,
                _ => (heddle_fst3 as unsafe extern "C" fn()) as usize,
            };
            (e, true)
        }
        MODE_RSEQ => (rst_entry(size), false),
        _ => (0, false),
    }
}

#[inline]
fn rst_entry(size: u8) -> usize {
    match size {
        0 => (heddle_rst0 as unsafe extern "C" fn()) as usize,
        1 => (heddle_rst1 as unsafe extern "C" fn()) as usize,
        2 => (heddle_rst2 as unsafe extern "C" fn()) as usize,
        _ => (heddle_rst3 as unsafe extern "C" fn()) as usize,
    }
}

/// Store rapido del modo rseq desde Rust (interprete, NEON por elemento): el mismo codigo que usa el JIT.
#[inline]
unsafe fn rseq_store(addr: usize, size: u8, v: u64) {
    asm!("call {e}", e = in(reg) rst_entry(size), in("rdi") addr, in("rdx") v, clobber_abi("C"));
}

/// Store rapido desde Rust (interprete, NEON por elemento): el mismo codigo que usa el JIT.
#[inline]
unsafe fn fence_store(f: *const MonFlag, addr: usize, size: u8, v: u64) {
    let e: usize = match size {
        0 => (heddle_fst0 as unsafe extern "C" fn()) as usize,
        1 => (heddle_fst1 as unsafe extern "C" fn()) as usize,
        2 => (heddle_fst2 as unsafe extern "C" fn()) as usize,
        _ => (heddle_fst3 as unsafe extern "C" fn()) as usize,
    };
    asm!("call {e}", e = in(reg) e, in("rdi") addr, in("rdx") v, in("rcx") f, clobber_abi("C"));
}

/// Llamar desde el manejador host de un fallo sincrono (SIGSEGV/SIGBUS) ANTES de ejecutar el manejador guest. Si el
/// fallo es el mov de un store rapido o el `lock cmpxchg` de un CAS rapido: no se ejecuto (x86 no completa una
/// instruccion que falla), asi que se limpia in_store y el rip pasa al reinicio (S1). Devuelve true si lo hizo.
pub unsafe fn fault_in_store(uc: *mut std::ffi::c_void) -> bool {
    if uc.is_null() {
        return false;
    }
    // ucontext_t de x86-64: mcontext.gregs en +40; REG_RCX = 14, REG_RIP = 16
    let g = (uc as *mut u8).add(40) as *mut u64;
    let rip = *g.add(16) as usize;
    let pts = [
        ((heddle_fst0_m as unsafe extern "C" fn()) as usize, (heddle_fst0_r as unsafe extern "C" fn()) as usize),
        ((heddle_fst1_m as unsafe extern "C" fn()) as usize, (heddle_fst1_r as unsafe extern "C" fn()) as usize),
        ((heddle_fst2_m as unsafe extern "C" fn()) as usize, (heddle_fst2_r as unsafe extern "C" fn()) as usize),
        ((heddle_fst3_m as unsafe extern "C" fn()) as usize, (heddle_fst3_r as unsafe extern "C" fn()) as usize),
        ((heddle_fcas0_m as unsafe extern "C" fn()) as usize, (heddle_fcas0_r as unsafe extern "C" fn()) as usize),
        ((heddle_fcas1_m as unsafe extern "C" fn()) as usize, (heddle_fcas1_r as unsafe extern "C" fn()) as usize),
        ((heddle_fcas2_m as unsafe extern "C" fn()) as usize, (heddle_fcas2_r as unsafe extern "C" fn()) as usize),
        ((heddle_fcas3_m as unsafe extern "C" fn()) as usize, (heddle_fcas3_r as unsafe extern "C" fn()) as usize),
    ];
    for (m, r) in pts {
        if rip == m {
            let f = *g.add(14) as *const MonFlag;
            if !f.is_null() {
                (*f).in_store.store(0, Release);
            }
            *g.add(16) = r as u64;
            return true;
        }
    }
    false
}

/// Compare-and-swap real del host sobre memoria guest (sin requisito de alineacion en x86).
#[inline]
unsafe fn cas_raw(addr: usize, size: u8, old: u64, new: u64) -> bool {
    let ok: u8;
    match size {
        0 => asm!("lock cmpxchg byte ptr [{a}], {n}", "sete {ok}", a = in(reg) addr, n = in(reg_byte) new as u8, inout("al") old as u8 => _, ok = out(reg_byte) ok, options(nostack)),
        1 => asm!("lock cmpxchg word ptr [{a}], {n:x}", "sete {ok}", a = in(reg) addr, n = in(reg) new as u16, inout("ax") old as u16 => _, ok = out(reg_byte) ok, options(nostack)),
        2 => asm!("lock cmpxchg dword ptr [{a}], {n:e}", "sete {ok}", a = in(reg) addr, n = in(reg) new as u32, inout("eax") old as u32 => _, ok = out(reg_byte) ok, options(nostack)),
        _ => asm!("lock cmpxchg qword ptr [{a}], {n}", "sete {ok}", a = in(reg) addr, n = in(reg) new, inout("rax") old => _, ok = out(reg_byte) ok, options(nostack)),
    }
    ok != 0
}

/// Store normal del guest (STR/STRB/STRH/...). `size` = log2(bytes).
pub fn store(addr: usize, size: u8, v: u64) {
    unsafe {
        match mode() {
            MODE_FENCE => {
                let f = TM.try_with(|t| t.flag.get()).unwrap_or(std::ptr::null());
                if !f.is_null() {
                    fence_store(f, addr, size, v);
                    return;
                }
            }
            MODE_RSEQ => {
                rseq_store(addr, size, v);
                return;
            }
            _ => {}
        }
        slow_store(addr, size, v);
    }
}

// ===========================================================================
// Monitor exclusivo (LDXR/STXR/LDXP/STXP) y atomicas LSE
// ===========================================================================

#[derive(Clone, Copy, Default)]
pub struct Mon {
    pub valid: bool,
    pub addr: usize,
    pub size: u8, // log2 del ancho total reservado (para pares: ancho de UN elemento + 1)
    pub ver: u64,
    pub epoch: u64,
}

impl Mon {
    pub const fn new() -> Mon {
        Mon { valid: false, addr: 0, size: 0, ver: 0, epoch: 0 }
    }
}

/// Escrituras normales del propio hilo desde su ultima reserva. Los granulos se comparten entre direcciones
/// (tabla de 1024): sin esto, un store del hilo a OTRA direccion que cae en el mismo granulo (p. ej. `nodo->next =
/// cabeza` entre LDXR y STXR de una pila sin bloqueos) invalidaba su propia reserva para siempre (bucle infinito).
/// (granulo, version al empezar la cadena, version tras el ultimo store propio). La cadena solo vale si nadie mas
/// escribio en medio (se comprueba con el bloqueo del granulo tomado) y si no toco la direccion reservada.
#[derive(Clone, Copy)]
struct Own {
    g: usize,
    start: u64,
    cur: u64,
    res_lo: usize,
    res_hi: usize,
}

#[inline]
fn own_reserve(addr: usize, len: usize) {
    let _ = TM.try_with(|t| t.own.set(Own { g: 0, start: 0, cur: 0, res_lo: addr, res_hi: addr + len }));
}

/// Llamar con el bloqueo de `g` tomado, tras escribir [addr, addr+n): incrementa la version y anota la cadena.
#[inline]
fn bump_own(g: &Granule, addr: usize, n: usize) {
    let _ = TM.try_with(|t| bump_own_t(t, g, addr, n));
}

#[inline]
fn bump_own_t(t: &ThreadMon, g: &Granule, addr: usize, n: usize) {
    let old = g.ver.load(Relaxed);
    let new = old.wrapping_add(1);
    g.ver.store(new, Release);
    let mut o = t.own.get();
    if o.res_hi == 0 {
        return; // sin reserva viva en este hilo
    }
    let gp = g as *const Granule as usize;
    if addr < o.res_hi && addr + n > o.res_lo {
        o.res_hi = 0; // escribio sobre lo reservado: la reserva se pierde
    } else if gp != gran(o.res_lo) as *const Granule as usize {
        return;
    } else if o.g == gp && o.cur == old {
        o.cur = new;
    } else if o.g == 0 {
        (o.g, o.start, o.cur) = (gp, old, new);
    } else {
        o.res_hi = 0; // otro hilo escribio entre dos stores propios
    }
    t.own.set(o);
}

/// La version del granulo cambio desde la reserva, pero solo por stores propios a otras direcciones.
#[inline]
fn own_only(g: &Granule, mon: &Mon) -> bool {
    TM.try_with(|t| {
        let o = t.own.get();
        o.res_hi != 0 && o.res_lo == mon.addr && o.g == g as *const Granule as usize && o.start == mon.ver && o.cur == g.ver.load(Relaxed)
    })
    .unwrap_or(false)
}

/// Toma una reserva sobre [addr, addr+2^size) y devuelve el valor (size 0..3).
/// Contadores del perfilador (solo con jit::PROF_ON): LDXR, STXR y granulos distintos tocados por LL/SC.
pub static PROF_LDX: AtomicU64 = AtomicU64::new(0);
pub static PROF_STX: AtomicU64 = AtomicU64::new(0);
pub static PROF_DISTINCT: AtomicU64 = AtomicU64::new(0);
static PROF_SEEN: [AtomicU64; 1 << (HOT_BITS - 6)] = [const { AtomicU64::new(0) }; 1 << (HOT_BITS - 6)];

fn prof_ll(addr: usize) {
    if crate::jit::PROF_ON.load(Relaxed) {
        PROF_LDX.fetch_add(1, Relaxed);
        let i = hot_idx(addr);
        let bit = 1u64 << (i & 63);
        if PROF_SEEN[i >> 6].fetch_or(bit, Relaxed) & bit == 0 {
            PROF_DISTINCT.fetch_add(1, Relaxed);
        }
    }
}

pub fn ldx(mon: &mut Mon, addr: usize, size: u8) -> u64 {
    mode(); // elige el modo antes de decidir si se arma
    prof_ll(addr);
    own_reserve(addr, 1usize << size);
    loop {
        let e = EPOCH.load(Acquire);
        if !FALLBACK.load(Relaxed) {
            ensure_hot(addr, e);
        }
        let g = gran(addr);
        mon.epoch = e;
        mon.ver = g.ver.load(Acquire);
        let v = unsafe { rd(addr, size) };
        mon.addr = addr;
        mon.size = size;
        mon.valid = true;
        if FALLBACK.load(Relaxed) || EPOCH.load(Acquire) == e {
            return v;
        }
    }
}

/// STXR: true = exito (Ws = 0).
pub fn stx(mon: &mut Mon, addr: usize, size: u8, v: u64) -> bool {
    if crate::jit::PROF_ON.load(Relaxed) {
        PROF_STX.fetch_add(1, Relaxed);
    }
    let g = gran(addr);
    glock(g);
    let ok = mon.valid
        && mon.addr == addr
        && mon.size == size
        && (g.ver.load(Relaxed) == mon.ver || own_only(g, mon))
        && EPOCH.load(Acquire) == mon.epoch;
    if ok {
        // un fallo suelta el bloqueo y llega al guest fuera de el (`lk_store`); false: repetir el STXR entero
        if !unsafe { lk_store(addr, size, v, &g.lock, std::ptr::null()) } {
            return stx_again(mon, addr, size, v);
        }
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
    }
    gunlock(g);
    mon.valid = false;
    own_reserve(0, 0);
    if !ok {
        // ARM: un STXR a memoria no escribible falla con SIGSEGV aunque la reserva no valga (sin bloqueo tomado)
        unsafe { probe_w(addr, 1usize << size) };
    }
    ok
}

/// `stx` otra vez, tras un fallo de la escritura con bloqueo ya resuelto (fuera de la ruta caliente).
#[cold]
#[inline(never)]
fn stx_again(mon: &mut Mon, addr: usize, size: u8, v: u64) -> bool {
    stx(mon, addr, size, v)
}

/// LDXP: reserva 2 elementos contiguos de `size` (2 = 32 bit, 3 = 64 bit). Devuelve (lo, hi).
pub fn ldxp(mon: &mut Mon, addr: usize, size: u8) -> (u64, u64) {
    mode();
    prof_ll(addr);
    own_reserve(addr, 2usize << size);
    loop {
        let e = EPOCH.load(Acquire);
        if !FALLBACK.load(Relaxed) {
            ensure_hot(addr, e);
        }
        let g = gran(addr);
        mon.epoch = e;
        mon.ver = g.ver.load(Acquire);
        let lo = unsafe { rd(addr, size) };
        let hi = unsafe { rd(addr + (1usize << size), size) };
        mon.addr = addr;
        mon.size = size + 1;
        mon.valid = true;
        if FALLBACK.load(Relaxed) || EPOCH.load(Acquire) == e {
            return (lo, hi);
        }
    }
}

pub fn stxp(mon: &mut Mon, addr: usize, size: u8, lo: u64, hi: u64) -> bool {
    unsafe { probe_w(addr, 2usize << size) };
    let g = gran(addr);
    glock(g);
    let ok = mon.valid
        && mon.addr == addr
        && mon.size == size + 1
        && (g.ver.load(Relaxed) == mon.ver || own_only(g, mon))
        && EPOCH.load(Acquire) == mon.epoch;
    if ok {
        unsafe {
            wr_raw(addr, size, lo);
            wr_raw(addr + (1usize << size), size, hi);
        }
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
    }
    gunlock(g);
    mon.valid = false;
    own_reserve(0, 0);
    ok
}

pub fn clrex(mon: &mut Mon) {
    mon.valid = false;
}

/// Lectura-modificacion-escritura atomica estricta (LDADD, SWP, CAS...). `f` recibe el valor
/// anterior y devuelve Some(nuevo) para escribir o None para no escribir (CAS fallido).
/// Devuelve el valor anterior. Una atomica que escribe invalida las reservas de los demas.
pub fn rmw(addr: usize, size: u8, f: impl Fn(u64) -> Option<u64>) -> u64 {
    if mode() == MODE_FENCE {
        // Via rapida (granulo frio, sin bloqueo): bucle de CAS cuya escritura es un `lock cmpxchg` dentro de la marca
        // in_store, como el store rapido. Exacto por el mismo razonamiento de la cabecera: con el granulo frio no hay
        // reserva que pueda tener exito (armar espera a la marca; enfriar sube EPOCH), asi que no hace falta subir la
        // version; frente a los stores rapidos, a la ruta con bloqueo y a otras atomicas es atomico por ser un
        // lock cmpxchg. Se linealiza en el CAS que escribe (lee el valor de ese instante y escribe f(valor)); si `f`
        // no escribe (CAS fallido), en la lectura, que no interactua con el monitor. Caliente o armando: ruta con
        // bloqueo de abajo.
        let fl = TM.try_with(|t| t.flag.get()).unwrap_or(std::ptr::null());
        if !fl.is_null() {
            let m = if size >= 3 { u64::MAX } else { (1u64 << (8u32 << size)) - 1 };
            let mut o = unsafe { rd(addr, size) };
            loop {
                let Some(n) = f(o) else { return o };
                match unsafe { fence_cas(fl, addr, size, o, n) } {
                    (0, _) => return o,
                    (1, cur) => o = cur & m,
                    _ => break,
                }
            }
        }
        // ARM: una atomica sobre memoria no escribible falla aunque no llegue a escribir (CAS fallido); antes del
        // bloqueo (probe_w). La via rapida falla en su lectura o en el cmpxchg, que se reinicia (fault_in_store).
        unsafe { probe_w(addr, 1usize << size) };
        // Sin armar: con el bloqueo tomado (serializa con STXR y con la ruta lenta) la escritura es un CAS real del
        // host, atomico tambien frente a los stores rapidos (movs simples) de otros hilos al mismo granulo frio. Se
        // linealiza en el CAS: lee el valor que hay en ese instante y escribe f(valor). La version sube como en
        // cualquier escritura (invalida las reservas ajenas; un granulo con reserva viva esta caliente).
        let g = gran(addr);
        glock(g);
        let old = loop {
            let o = unsafe { rd(addr, size) };
            match f(o) {
                None => break o,
                Some(n) => {
                    if unsafe { cas_raw(addr, size, o, n) } {
                        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
                        break o;
                    }
                }
            }
        };
        gunlock(g);
        return old;
    }
    // Lectura y escritura bajo el bloqueo con `lk_load`/`lk_store` (`rmw_locked`): un fallo suelta el bloqueo, llega
    // al guest fuera de el y, si la pagina ya es escribible, se repite el RMW entero.
    if FALLBACK.load(Relaxed) {
        let g = gran(addr);
        loop {
            glock(g);
            if let Some(old) = unsafe { rmw_locked(g, addr, size, &f) } {
                return old;
            }
        }
    }
    let h = &HOT[hot_idx(addr)];
    loop {
        let e = EPOCH.load(Acquire);
        ensure_hot(addr, e);
        let g = gran(addr);
        glock(g);
        // bajo el lock: sin enfriamiento desde `e` y granulo caliente => los stores normales de
        // los demas pasan por este mismo lock y el RMW es atomico frente a ellos
        if SATURATED.load(Acquire) || (EPOCH.load(Acquire) == e && h.load(Acquire) & 0xFF == 2) {
            if let Some(old) = unsafe { rmw_locked(g, addr, size, &f) } {
                return old;
            }
            continue;
        }
        gunlock(g);
    }
}

/// Cuerpo de un RMW con el bloqueo de `g` tomado: lee, aplica `f`, escribe (sube la version) y suelta el bloqueo.
/// None: un acceso fallo y el bloqueo ya se solto (repetir). ARM: una atomica sobre memoria no escribible falla aunque
/// no escriba (CAS fallido): en ese caso, sonda despues de soltar el bloqueo.
#[inline(always)]
unsafe fn rmw_locked<F: Fn(u64) -> Option<u64>>(g: &Granule, addr: usize, size: u8, f: &F) -> Option<u64> {
    let old = unsafe { lk_load(addr, size, &g.lock)? };
    match f(old) {
        Some(n) => {
            if !unsafe { lk_store(addr, size, n, &g.lock, std::ptr::null()) } {
                return None;
            }
            g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
            gunlock(g);
        }
        None => {
            gunlock(g);
            unsafe { probe_w(addr, 1usize << size) };
        }
    }
    Some(old)
}

/// RMW de 128 bits (CASP). `f` recibe (lo, hi) anteriores y devuelve Some((lo, hi)) nuevos.
pub fn rmw_pair(addr: usize, size: u8, f: impl Fn(u64, u64) -> Option<(u64, u64)>) -> (u64, u64) {
    mode();
    unsafe { probe_w(addr, 2usize << size) };
    let step = 1usize << size;
    let do_it = |g: &Granule| {
        let lo = unsafe { rd(addr, size) };
        let hi = unsafe { rd(addr + step, size) };
        if let Some((nl, nh)) = f(lo, hi) {
            unsafe {
                wr_raw(addr, size, nl);
                wr_raw(addr + step, size, nh);
            }
            g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
        }
        (lo, hi)
    };
    // CASP exige alineacion natural del par => cabe en un solo granulo de 64 B
    if FALLBACK.load(Relaxed) {
        let g = gran(addr);
        glock(g);
        let r = do_it(g);
        gunlock(g);
        return r;
    }
    let h = &HOT[hot_idx(addr)];
    loop {
        let e = EPOCH.load(Acquire);
        ensure_hot(addr, e);
        let g = gran(addr);
        glock(g);
        if SATURATED.load(Acquire) || (EPOCH.load(Acquire) == e && h.load(Acquire) & 0xFF == 2) {
            let r = do_it(g);
            gunlock(g);
            return r;
        }
        gunlock(g);
    }
}

#[cfg(test)]
mod own_tests {
    use super::*;

    /// Dos direcciones distintas que comparten granulo (y entrada de la tabla de calientes): 4 MiB de distancia.
    #[test]
    fn store_propio_a_otra_direccion_no_invalida_la_reserva() {
        const D: usize = 4 << 20;
        let buf = vec![0u64; (D + 4096) / 8];
        // alineada a 64: con `ptr + 64` (malloc solo alinea a 16) b + 16 podia caer en el bloque de 64 B SIGUIENTE y
        // el "store de otro hilo al mismo granulo" de abajo no lo era (fallo intermitente segun la direccion del bufer)
        let a = ((buf.as_ptr() as usize + 63) & !63) + 64;
        let b = a + D;
        assert!(std::ptr::eq(gran(a), gran(b)));
        let mut m = Mon::new();
        // patron de pila sin bloqueos: leer cabeza, escribir nodo->next (otra direccion, mismo granulo), publicar
        for i in 0..3u64 {
            let head = ldx(&mut m, a, 3);
            store(b, 3, head);
            store(b + 8, 3, i);
            assert!(stx(&mut m, a, 3, 100 + i), "la reserva propia debe sobrevivir a stores propios a otra direccion");
        }
        assert_eq!(unsafe { rd(a, 3) }, 102);
        // escribir sobre la direccion reservada si la invalida
        ldx(&mut m, a, 3);
        store(a, 3, 7);
        assert!(!stx(&mut m, a, 3, 8));
        // un store de OTRO hilo al mismo granulo la invalida, haya o no stores propios despues
        ldx(&mut m, a, 3);
        std::thread::spawn(move || store(b + 16, 3, 1)).join().unwrap();
        store(b, 3, 2);
        assert!(!stx(&mut m, a, 3, 9));
        assert_eq!(unsafe { rd(a, 3) }, 7);
        drop(buf);
    }
}

#[cfg(test)]
mod align_tests {
    use super::*;

    /// Las rutas calientes de los stores y CAS rapidos (los tres modos llaman a una de ellas o a `h_store`) empiezan en
    /// una linea de 64 B y su instruccion de memoria cae en la misma linea: su rendimiento no depende de donde las deje
    /// el enlazador (ver "Alineacion de las rutas calientes" en la cabecera; -15..20 % en stores rseq y ~9 % en fence
    /// segun la posicion). `tests/arch_lint.rs` comprueba lo mismo en la fuente (`.p2align 6` delante de cada entrada).
    #[test]
    fn rutas_calientes_alineadas() {
        let f = |x: unsafe extern "C" fn()| x as usize;
        let ent = [
            (f(heddle_rst0), 0),
            (f(heddle_rst1), 0),
            (f(heddle_rst2), 0),
            (f(heddle_rst3), 0),
            (f(heddle_fst0), f(heddle_fst0_m)),
            (f(heddle_fst1), f(heddle_fst1_m)),
            (f(heddle_fst2), f(heddle_fst2_m)),
            (f(heddle_fst3), f(heddle_fst3_m)),
            (f(heddle_fcas0), f(heddle_fcas0_m)),
            (f(heddle_fcas1), f(heddle_fcas1_m)),
            (f(heddle_fcas2), f(heddle_fcas2_m)),
            (f(heddle_fcas3), f(heddle_fcas3_m)),
        ];
        for (i, (e, m)) in ent.into_iter().enumerate() {
            assert_eq!(e % 64, 0, "entrada {i} en {e:#x}: no empieza en una linea de 64 B");
            assert!(m == 0 || (m > e && m - e < 56), "entrada {i}: la instruccion de memoria ({m:#x}) sale de su linea");
        }
        for s in 0..4u8 {
            let (e, _) = fast_store_entry(s);
            assert!(e == 0 || e % 64 == 0);
        }
    }

    /// El store rapido de rseq escribe el dato con cada tamano y respeta los bytes vecinos.
    #[test]
    fn store_rseq_por_tamano() {
        if mode() != MODE_RSEQ {
            return;
        }
        let mut buf = [0u64; 16];
        let b = ((buf.as_mut_ptr() as usize + 63) & !63) as *mut u8;
        for size in 0..4u8 {
            unsafe {
                std::ptr::write_bytes(b, 0xAA, 64);
                rseq_store(b as usize + 8, size, 0x1122_3344_5566_7788);
                let n = 1usize << size;
                for k in 0..64 {
                    let want = if (8..8 + n).contains(&k) { (0x1122_3344_5566_7788u64 >> (8 * (k - 8))) as u8 } else { 0xAA };
                    assert_eq!(*b.add(k), want, "tamano {size}, byte {k}");
                }
                // acceso que cruza el granulo: ruta con bloqueo, mismo resultado
                if size > 0 {
                    rseq_store(b as usize + 63, size, 0);
                    assert_eq!(*b.add(63), 0);
                }
            }
        }
        let _ = buf;
    }
}

#[cfg(test)]
mod crit_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

    static HANDLED: AtomicU64 = AtomicU64::new(0);
    static DEFERRED: AtomicU64 = AtomicU64::new(0);
    static TARGET: AtomicUsize = AtomicUsize::new(0);
    static NO_DEFER: AtomicBool = AtomicBool::new(false);
    /// las dos pruebas comparten manejador y variables globales: se ejecutan una tras otra (y lk_tests, que toma
    /// bloqueos de granulo, no corre a la vez: la primera comprueba al final que no queda ninguno tomado)
    pub(super) static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    extern "C" {
        fn signal(sig: i32, h: extern "C" fn(i32)) -> usize;
    }

    /// Lo que hace el manejador host de una senal asincrona (sig::async_host_handler) y luego un manejador guest
    /// tipico: escribir en memoria. Aqui escribe en una direccion que comparte granulo con la del hilo interrumpido.
    extern "C" fn handler(sig: i32) {
        if !NO_DEFER.load(Relaxed) && unsafe { defer_signal(sig, std::ptr::null()) } {
            DEFERRED.fetch_add(1, Relaxed);
            return;
        }
        HANDLED.fetch_add(1, Relaxed);
        unsafe { slow_store(TARGET.load(Relaxed) + (64 << 10), 3, 1) };
    }

    /// Reproduce el cuelgue de Unity: una senal que cae con el bloqueo de un granulo tomado y cuyo manejador escribe
    /// en el mismo granulo se bloqueaba a si misma para siempre. Con el arreglo, la senal espera a que el hilo suelte
    /// el bloqueo y se entrega despues; ninguna se pierde sin entregar mientras el hilo siga activo.
    #[test]
    fn senal_con_bloqueo_tomado_no_se_interbloquea() {
        let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        NO_DEFER.store(std::env::var_os("HEDDLE_TEST_SIN_DIFERIR").is_some(), Relaxed);
        let buf = vec![0u64; (64 << 10) * 2 / 8 + 1024];
        let a = (buf.as_ptr() as usize + 63) & !63;
        assert!(std::ptr::eq(gran(a), gran(a + (64 << 10))));
        TARGET.store(a, Relaxed);
        unsafe { signal(12, handler) }; // SIGUSR2
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        let worker = std::thread::spawn(move || {
            let tid = unsafe { crate::sys::syscall(186) };
            let t = std::thread::spawn(move || ());
            t.join().unwrap();
            (tid, d2)
        });
        let (tid, d2) = worker.join().unwrap();
        // hilo que martilla stores mientras el principal le envia senales
        let (tx, rx) = std::sync::mpsc::channel();
        let h = std::thread::spawn(move || {
            tx.send(unsafe { crate::sys::syscall(186) }).unwrap();
            let mut i = 0u64;
            while !d2.load(Relaxed) {
                unsafe { slow_store(a, 3, i) };
                i += 1;
            }
            i
        });
        let _ = tid;
        let htid = rx.recv().unwrap();
        let pid = unsafe { crate::sys::syscall(39) };
        for _ in 0..3000 {
            unsafe { crate::sys::syscall(234, pid, htid, 12i64) };
            std::thread::sleep(std::time::Duration::from_micros(150));
        }
        // el hilo debe seguir vivo (no bloqueado): se le pide terminar y debe hacerlo pronto
        done.store(true, Relaxed);
        let t0 = Instant::now();
        while !h.is_finished() {
            assert!(t0.elapsed() < std::time::Duration::from_secs(10), "el hilo quedo bloqueado en un bloqueo de granulo (interbloqueo con su propio manejador)");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(h.join().unwrap() > 0);
        assert!(HANDLED.load(Relaxed) > 0, "ninguna senal llego a ejecutar el manejador");
        assert!(DEFERRED.load(Relaxed) > 0, "ninguna senal cayo con un bloqueo tomado: la prueba no ejercita el caso");
        // no queda ningun bloqueo tomado
        assert!(GRAN.iter().all(|g| !g.lock.load(Relaxed)));
    }

    #[test]
    fn seccion_critica_anidada_y_pendientes() {
        let _s = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // el manejador escribe 64 KiB mas alla de TARGET (mismo granulo): el bufer debe cubrirlo
        let b = vec![0u64; (64 << 10) * 2 / 8 + 1024];
        let addr = (b.as_ptr() as usize + 63) & !63;
        let g = gran(addr);
        assert!(!in_critical());
        glock(g);
        assert!(in_critical());
        // un acceso que cruza granulos toma dos bloqueos (anidados): sigue en seccion critica hasta soltar ambos
        let g2 = gran(addr + 64);
        glock(g2);
        gunlock(g2);
        assert!(in_critical());
        assert!(unsafe { defer_signal(12, std::ptr::null()) });
        // el reenvio al soltar el ultimo: SIGUSR2 pendiente se entrega al propio hilo
        let before = HANDLED.load(Relaxed) + DEFERRED.load(Relaxed);
        unsafe { signal(12, handler) };
        NO_DEFER.store(false, Relaxed);
        TARGET.store(addr, Relaxed);
        gunlock(g);
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!in_critical());
        assert!(HANDLED.load(Relaxed) + DEFERRED.load(Relaxed) > before, "la senal diferida no se reenvio al soltar el bloqueo");
    }
}

#[cfg(test)]
mod crit_info_tests {
    use super::*;
    use std::os::raw::c_void;
    use std::sync::atomic::AtomicI64;

    #[repr(C)]
    struct SigAct {
        handler: usize,
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    extern "C" {
        fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
    }

    static CODE: AtomicI64 = AtomicI64::new(i64::MIN);
    static VALUE: AtomicI64 = AtomicI64::new(i64::MIN);
    static WAS_DEFERRED: AtomicBool = AtomicBool::new(false);

    extern "C" fn h(sig: i32, info: *mut u8, _uc: *mut c_void) {
        if unsafe { defer_signal(sig, info) } {
            WAS_DEFERRED.store(true, Relaxed);
            return;
        }
        unsafe {
            CODE.store(*(info.add(8) as *const i32) as i64, Relaxed);
            VALUE.store(*(info.add(24) as *const i64), Relaxed);
        }
    }

    /// Una senal con informacion (como sigqueue/temporizadores) que se aplaza llega al manejador con el mismo
    /// si_code y si_value que traia, igual que la entregaria un nucleo ARM sin aplazamiento.
    #[test]
    fn la_senal_diferida_conserva_su_siginfo() {
        let act = SigAct { handler: h as usize, mask: [0; 16], flags: 4 /* SA_SIGINFO */, restorer: 0 };
        assert_eq!(unsafe { sigaction(10, &act, std::ptr::null_mut()) }, 0); // SIGUSR1
        let b = vec![0u64; 16];
        let g = gran(b.as_ptr() as usize);
        let (pid, tid) = unsafe { (crate::sys::syscall(39), crate::sys::syscall(186)) };
        let mut info = [0u8; 128];
        info[0..4].copy_from_slice(&10i32.to_ne_bytes());
        info[8..12].copy_from_slice(&(-1i32).to_ne_bytes()); // SI_QUEUE
        info[16..20].copy_from_slice(&(pid as i32).to_ne_bytes());
        info[24..32].copy_from_slice(&0xABCDi64.to_ne_bytes());
        glock(g);
        assert_eq!(unsafe { crate::sys::syscall(297, pid, tid, 10i64, info.as_ptr()) }, 0);
        assert!(WAS_DEFERRED.load(Relaxed), "la senal no se aplazo con el bloqueo tomado");
        assert_eq!(VALUE.load(Relaxed), i64::MIN, "el manejador no debe ejecutarse con el bloqueo tomado");
        gunlock(g);
        assert_eq!((CODE.load(Relaxed), VALUE.load(Relaxed)), (-1, 0xABCD));
    }

    static SEEN64: AtomicI64 = AtomicI64::new(0);
    extern "C" fn h64(sig: i32, info: *mut u8, _uc: *mut c_void) {
        if unsafe { defer_signal(sig, info) } {
            return;
        }
        SEEN64.fetch_add(1, Relaxed);
    }

    static RT_VALS: [AtomicI64; 4] = [const { AtomicI64::new(0) }; 4];
    static RT_N: AtomicUsize = AtomicUsize::new(0);
    extern "C" fn h_rt(sig: i32, info: *mut u8, _uc: *mut c_void) {
        if unsafe { defer_signal(sig, info) } {
            return;
        }
        let n = RT_N.fetch_add(1, Relaxed);
        if n < 4 {
            RT_VALS[n].store(unsafe { *(info.add(24) as *const i64) }, Relaxed);
        }
    }

    /// Tres envios de una senal de tiempo real con el bloqueo tomado se reenvian los tres, con su valor y en orden
    /// (no se funden en el primero).
    #[test]
    fn las_de_tiempo_real_diferidas_no_se_funden() {
        let sig = crate::sig::rtmin() as i32 + 4;
        let act = SigAct { handler: h_rt as usize, mask: [0; 16], flags: 4, restorer: 0 };
        assert_eq!(unsafe { sigaction(sig, &act, std::ptr::null_mut()) }, 0);
        let b = vec![0u64; 16];
        let g = gran(b.as_ptr() as usize);
        let (pid, tid) = unsafe { (crate::sys::syscall(39), crate::sys::syscall(186)) };
        glock(g);
        for v in 1..=3i64 {
            let mut info = [0u8; 128];
            info[0..4].copy_from_slice(&sig.to_ne_bytes());
            info[8..12].copy_from_slice(&(-1i32).to_ne_bytes()); // SI_QUEUE
            info[16..20].copy_from_slice(&(pid as i32).to_ne_bytes());
            info[24..32].copy_from_slice(&v.to_ne_bytes());
            assert_eq!(unsafe { crate::sys::syscall(297, pid, tid, sig as i64, info.as_ptr()) }, 0);
        }
        assert_eq!(RT_N.load(Relaxed), 0, "con el bloqueo tomado no se entrega");
        gunlock(g);
        assert_eq!(RT_N.load(Relaxed), 3);
        let vals: Vec<i64> = RT_VALS.iter().take(3).map(|v| v.load(Relaxed)).collect();
        assert_eq!(vals, [1, 2, 3]);
    }

    /// La senal 64 (SIGRTMAX) aplazada se reenvia al soltar el bloqueo (no cae en el bit 0).
    #[test]
    fn la_senal_64_diferida_se_reenvia() {
        let act = SigAct { handler: h64 as usize, mask: [0; 16], flags: 4, restorer: 0 };
        assert_eq!(unsafe { sigaction(64, &act, std::ptr::null_mut()) }, 0);
        let b = vec![0u64; 16];
        let g = gran(b.as_ptr() as usize);
        let (pid, tid) = unsafe { (crate::sys::syscall(39), crate::sys::syscall(186)) };
        glock(g);
        assert_eq!(unsafe { crate::sys::syscall(234, pid, tid, 64i64) }, 0); // tgkill
        assert_eq!(SEEN64.load(Relaxed), 0, "el manejador no debe ejecutarse con el bloqueo tomado");
        gunlock(g);
        assert_eq!(SEEN64.load(Relaxed), 1, "la senal 64 aplazada se perdio");
    }
}

/// Pruebas del modo fence (via rapida de stores por fence asimetrico). Cada prueba se ejecuta en modo fence: si el
/// proceso de pruebas eligio otro modo (rseq en un host con glibc), se relanza a si misma con HEDDLE_MONITOR=fence.
/// Los entrelazados criticos se fuerzan con barreras, de modo que el veredicto no depende de la suerte.
#[cfg(test)]
mod lk_tests {
    use super::*;

    /// `struct sigaction` de glibc
    #[repr(C)]
    struct SigAct {
        handler: usize,
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    extern "C" {
        fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
    }

    static REDIRECTS: AtomicU64 = AtomicU64::new(0);
    static GUEST_FAULTS: AtomicU64 = AtomicU64::new(0);
    static BAD: AtomicU64 = AtomicU64::new(0);
    static PAGE: AtomicUsize = AtomicUsize::new(0);

    /// Lo que hace `sig::sync_fault`: primero `fault_in_locked`; el fallo que llega "al guest" debe llegar sin bloqueos
    /// ni seccion critica. El manejador guest de la prueba hace escribible la pagina y vuelve.
    extern "C" fn segv(_sig: i32, _info: *mut u8, uc: *mut std::ffi::c_void) {
        if unsafe { fault_in_locked(uc) } {
            REDIRECTS.fetch_add(1, SeqCst);
            return;
        }
        GUEST_FAULTS.fetch_add(1, SeqCst);
        let p = PAGE.load(SeqCst);
        if in_critical() || (0..4096).step_by(64).any(|o| gran(p + o).lock.load(SeqCst)) {
            BAD.fetch_add(1, SeqCst);
        }
        unsafe { crate::sys::syscall(10, p as i64, 4096i64, 3i64) }; // PROT_READ|PROT_WRITE
    }

    fn ro(p: usize) {
        assert_eq!(unsafe { crate::sys::syscall(10, p as i64, 4096i64, 1i64) }, 0); // PROT_READ
        prot_changed();
    }

    /// Accesos con bloqueo (STXR, store con bloqueo, RMW) a una pagina de solo lectura: el fallo llega al manejador
    /// fuera del bloqueo y de la seccion critica, y al volver (pagina ya escribible) la operacion se repite entera y
    /// escribe exactamente una vez. Un STXR sin reserva y un CAS fallido tambien fallan (como en ARM), sin escribir.
    #[test]
    fn fallo_con_bloqueo_tomado_llega_fuera_del_bloqueo() {
        // sin las pruebas que comprueban que no queda ningun bloqueo tomado ni las que instalan su manejador de SIGSEGV
        let _c = super::crit_tests::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _f = super::fence_tests::SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let v = vec![0u64; 3 * 512];
        let page = (v.as_ptr() as usize + 4095) & !4095;
        PAGE.store(page, SeqCst);
        let act = SigAct { handler: segv as usize, mask: [0; 16], flags: 4 | 0x4000_0000 /* SA_SIGINFO | SA_NODEFER */, restorer: 0 };
        let mut old = SigAct { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
        assert_eq!(unsafe { sigaction(11, &act, &mut old) }, 0);
        let check = |what: &str, redirects: u64, faults: u64| {
            assert_eq!(BAD.load(SeqCst), 0, "{}: fallo con bloqueo o seccion critica", what);
            assert_eq!(GUEST_FAULTS.swap(0, SeqCst), faults, "{}: fallos entregados", what);
            assert_eq!(REDIRECTS.swap(0, SeqCst), redirects, "{}: fallos bajo el bloqueo", what);
            assert!(!in_critical(), "{}: seccion critica sin cerrar", what);
        };
        for size in 0..4u8 {
            let a = page + 128 + 8 * size as usize;
            // STXR con reserva valida
            let mut m = Mon::new();
            let x = ldx(&mut m, a, size);
            ro(page);
            assert!(stx(&mut m, a, size, x + 1), "STXR tras el fallo");
            assert_eq!(unsafe { rd(a, size) }, x + 1);
            check("stx", 1, 1);
            // STXR sin reserva: falla (Ws = 1) y aun asi da SIGSEGV, sin escribir
            ro(page);
            assert!(!stx(&mut m, a, size, 0x77));
            assert_eq!(unsafe { rd(a, size) }, x + 1);
            check("stx sin reserva", 0, 1);
            // store con bloqueo
            ro(page);
            unsafe { slow_store(a, size, 0x42) };
            assert_eq!(unsafe { rd(a, size) }, 0x42);
            check("slow_store", 1, 1);
            // RMW que escribe y CAS fallido (fuera de la via rapida del modo fence: este hilo no es guest)
            ro(page);
            assert_eq!(rmw(a, size, |o| Some(o + 1)), 0x42);
            assert_eq!(unsafe { rd(a, size) }, 0x43);
            check("rmw", if mode() == MODE_FENCE { 0 } else { 1 }, 1);
            ro(page);
            assert_eq!(rmw(a, size, |_| None), 0x43);
            check("cas fallido", 0, 1);
        }
        // store que cruza dos granulos (dos bloqueos) y que cruza a la pagina de solo lectura: sin escritura parcial
        let a = page + 64 - 4;
        ro(page);
        unsafe { slow_store(a, 3, 0x1122_3344_5566_7788) };
        assert_eq!(unsafe { rd(a, 3) }, 0x1122_3344_5566_7788);
        check("slow_store entre granulos", 1, 1);
        let a = page - 4;
        unsafe { std::ptr::write_unaligned(a as *mut u64, 0) };
        ro(page);
        unsafe { slow_store(a, 3, u64::MAX) };
        assert_eq!(unsafe { rd(a, 3) }, u64::MAX);
        check("slow_store entre paginas", 1, 1);
        unsafe { sigaction(11, &old, std::ptr::null_mut()) };
    }
}

#[cfg(test)]
mod fence_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
    use std::sync::{Barrier, Mutex};
    use std::time::Duration;

    pub(super) static SERIAL: Mutex<()> = Mutex::new(());

    /// true: este proceso ya esta en modo fence y la prueba debe ejecutarse aqui. false: se ejecuto (y paso) en un
    /// proceso hijo en modo fence.
    pub(super) fn en_modo_fence(nombre: &str, ignorada: bool) -> bool {
        if mode() == MODE_FENCE {
            return true;
        }
        assert_ne!(std::env::var("HEDDLE_MONITOR").ok().as_deref(), Some("fence"), "modo fence pedido y no disponible ({})", mode_name());
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([nombre, "--exact", "--test-threads=1", "--nocapture"]);
        if ignorada {
            cmd.arg("--ignored");
        }
        let out = cmd.env("HEDDLE_MONITOR", "fence").output().unwrap();
        let (so, se) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        print!("{}", so);
        assert!(out.status.success() && so.contains("1 passed"), "la prueba en modo fence fallo:\n{}\n{}", so, se);
        false
    }

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hilo guest (con su marca del monitor registrada) y su marca.
    fn guest() -> &'static MonFlag {
        let t = crate::rt::ensure_thread(256 << 10);
        unsafe { &(*t).mon_flag }
    }

    /// Bufer alineado a 64 B.
    fn buf(bytes: usize) -> (Vec<u64>, usize) {
        let v = vec![0u64; bytes / 8 + 16];
        let base = (v.as_ptr() as usize + 63) & !63;
        (v, base)
    }

    fn cold(addr: usize) -> bool {
        HOT[hot_idx(addr)].load(Acquire) & 0xFF == 0
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(10), "tiempo agotado esperando: {}", what);
            std::thread::yield_now();
        }
    }

    /// Etiqueta de HOT: una linea alias de otra armada va por la ruta rapida (sin version); al armarse ella tambien, la
    /// entrada pasa a TAG_MULTI y sus stores vuelven a la ruta con bloqueo; enfriar borra la etiqueta.
    #[test]
    fn etiqueta_de_hot_distingue_lineas_alias() {
        if !en_modo_fence("monitor::fence_tests::etiqueta_de_hot_distingue_lineas_alias", false) {
            return;
        }
        let _s = serial();
        guest();
        const D: usize = 4 << 20;
        let (_v, base) = buf(D + 4096);
        let a = base + 128;
        let b = a + D;
        assert!(hot_idx(a) == hot_idx(b) && std::ptr::eq(gran(a), gran(b)));
        cool();
        let mut m = Mon::new();
        let v0 = ldx(&mut m, a, 3);
        assert_eq!(HOT_TAG[hot_idx(a)].load(Relaxed), (a >> 6) as u64);
        let ver = gran(a).ver.load(Relaxed);
        std::thread::spawn(move || {
            guest();
            store(b, 3, 1)
        })
        .join()
        .unwrap();
        assert_eq!(gran(a).ver.load(Relaxed), ver, "el store a la linea alias debe ir por la ruta rapida");
        assert!(stx(&mut m, a, 3, v0 + 1));
        // b tambien arma: la entrada queda con varias lineas y los stores a cualquiera van con bloqueo (version)
        let mut mb = Mon::new();
        ldx(&mut mb, b, 3);
        assert_eq!(HOT_TAG[hot_idx(a)].load(Relaxed), TAG_MULTI);
        let ver = gran(a).ver.load(Relaxed);
        std::thread::spawn(move || {
            guest();
            store(b + 8, 3, 2)
        })
        .join()
        .unwrap();
        assert_ne!(gran(a).ver.load(Relaxed), ver, "con TAG_MULTI el store va por la ruta con bloqueo");
        assert!(!stx(&mut mb, b, 3, 3) || unsafe { rd(b + 8, 3) } == 2);
        cool();
        cool();
        assert!(cold(a) && HOT_TAG[hot_idx(a)].load(Relaxed) == 0);
    }

    /// (a) La prueba de own_tests con hilos guest (ruta rapida activa), mas: un store rapido a otro bloque de 64 B
    /// que comparte bloqueo con la reserva no la toca (ni sube la version: prueba de que fue por la ruta rapida).
    #[test]
    fn a_store_propio_a_otra_direccion_no_invalida_la_reserva() {
        if !en_modo_fence("monitor::fence_tests::a_store_propio_a_otra_direccion_no_invalida_la_reserva", false) {
            return;
        }
        let _s = serial();
        guest();
        const D: usize = 4 << 20;
        let (_v, base) = buf(D + 4096);
        let a = base + 64;
        let b = a + D;
        assert!(std::ptr::eq(gran(a), gran(b)) && hot_idx(a) == hot_idx(b));
        cool();
        let mut m = Mon::new();
        for i in 0..3u64 {
            let head = ldx(&mut m, a, 3);
            store(b, 3, head);
            store(b + 8, 3, i);
            assert!(stx(&mut m, a, 3, 100 + i), "la reserva propia debe sobrevivir a stores propios a otra direccion");
        }
        assert_eq!(unsafe { rd(a, 3) }, 102);
        ldx(&mut m, a, 3);
        store(a, 3, 7);
        assert!(!stx(&mut m, a, 3, 8));
        // otro hilo escribe en b (otra linea, misma entrada de HOT): con la etiqueta de a va por la ruta rapida y la
        // reserva sigue (fallar por el alias estaria permitido, pero no hace falta); en a, falla
        let v0 = ldx(&mut m, a, 3);
        std::thread::spawn(move || {
            guest();
            store(b + 16, 3, 1)
        })
        .join()
        .unwrap();
        store(b, 3, 2);
        assert!(stx(&mut m, a, 3, v0 + 2), "un store de otro hilo a una linea alias no debe romper la reserva");
        ldx(&mut m, a, 3);
        std::thread::spawn(move || {
            guest();
            store(a + 8, 3, 1)
        })
        .join()
        .unwrap();
        assert!(!stx(&mut m, a, 3, 9));
        assert_eq!(unsafe { rd(a, 3) }, 9);
        // c: mismo bloqueo de granulo que a, otra entrada de HOT (otro bloque de 64 B): fria, ruta rapida
        let c = a + (64 << 10);
        assert!(std::ptr::eq(gran(a), gran(c)) && hot_idx(a) != hot_idx(c));
        let v0 = ldx(&mut m, a, 3);
        assert!(cold(c));
        let ver = gran(a).ver.load(Relaxed);
        store(c, 3, 5);
        assert_eq!(gran(a).ver.load(Relaxed), ver, "el store a un granulo frio debe ir por la ruta rapida (sin version)");
        assert!(stx(&mut m, a, 3, v0 + 1));
        // otro hilo escribe en c (otro bloque): la reserva sigue; en a+8 (mismo bloque): falla
        let v0 = ldx(&mut m, a, 3);
        std::thread::spawn(move || {
            guest();
            store(c + 8, 3, 1)
        })
        .join()
        .unwrap();
        assert!(stx(&mut m, a, 3, v0 + 1), "un store a otro bloque de 64 B no invalida la reserva");
        let v0 = ldx(&mut m, a, 3);
        std::thread::spawn(move || {
            guest();
            store(a + 8, 3, 1)
        })
        .join()
        .unwrap();
        assert!(!stx(&mut m, a, 3, v0 + 1), "un store de otro hilo al bloque reservado invalida la reserva");
    }

    #[repr(C, align(64))]
    struct Node {
        next: u64,
        owner: AtomicU64,
    }

    /// (b) Pila sin bloqueos como la de Unity: 8 hilos sacan y meten 1 M de nodos cada uno; meter escribe nodo->next
    /// (store normal) entre LDXR y STXR de la cabeza; sacar lee nodo->next entre ambos (el caso ABA). Un hilo enfria
    /// la tabla sin parar para que la cabeza se vuelva a armar mientras hay stores rapidos en vuelo. Al final: ningun
    /// nodo sacado dos veces a la vez, ni perdido, ni duplicado.
    #[test]
    fn b_pila_sin_bloqueos_8_hilos() {
        if !en_modo_fence("monitor::fence_tests::b_pila_sin_bloqueos_8_hilos", false) {
            return;
        }
        let _s = serial();
        const NN: usize = 512;
        const ITER: usize = 1_000_000;
        let nodes: Vec<Node> = (0..NN).map(|_| Node { next: 0, owner: AtomicU64::new(0) }).collect();
        let (_h, head) = buf(64);
        let np = |i: usize| &nodes[i] as *const Node as usize;
        unsafe {
            for i in 0..NN {
                *(np(i) as *mut u64) = if i + 1 < NN { np(i + 1) as u64 } else { 0 };
            }
            *(head as *mut u64) = np(0) as u64;
        }
        let dup = AtomicU64::new(0);
        let done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                while !done.load(Relaxed) {
                    cool();
                    std::thread::sleep(Duration::from_micros(200));
                }
            });
            let hs: Vec<_> = (0..8u64)
                .map(|t| {
                    let dup = &dup;
                    s.spawn(move || {
                        guest();
                        let mut m = Mon::new();
                        for _ in 0..ITER {
                            let n = loop {
                                let h = ldx(&mut m, head, 3);
                                if h == 0 {
                                    clrex(&mut m);
                                    spin_loop();
                                    continue;
                                }
                                let nx = unsafe { rd(h as usize, 3) };
                                if stx(&mut m, head, 3, nx) {
                                    break h as usize;
                                }
                            };
                            let node = unsafe { &*(n as *const Node) };
                            if node.owner.swap(t + 1, SeqCst) != 0 {
                                dup.fetch_add(1, Relaxed);
                            }
                            node.owner.store(0, SeqCst);
                            loop {
                                let h = ldx(&mut m, head, 3);
                                store(n, 3, h);
                                if stx(&mut m, head, 3, n as u64) {
                                    break;
                                }
                            }
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            done.store(true, Relaxed);
        });
        assert_eq!(dup.load(Relaxed), 0, "un nodo se saco dos veces a la vez (ABA)");
        let mut seen = std::collections::HashSet::new();
        let mut p = unsafe { rd(head, 3) } as usize;
        while p != 0 {
            assert!(seen.insert(p), "ciclo o nodo duplicado en la pila");
            assert!(seen.len() <= NN);
            p = unsafe { rd(p, 3) } as usize;
        }
        assert_eq!(seen.len(), NN, "se perdieron nodos");
    }

    /// (c) Contadores exactos: X con LDXR/STXR (4 hilos) y LDADD (4 hilos); C2 (granulo que nadie arma) con LDADD de
    /// 32 bits desde 8 hilos mientras todos escriben con stores rapidos la otra mitad de la misma palabra de 64 bits.
    #[test]
    fn c_contadores_llsc_y_ldadd_exactos() {
        if !en_modo_fence("monitor::fence_tests::c_contadores_llsc_y_ldadd_exactos", false) {
            return;
        }
        let _s = serial();
        const K: u64 = 200_000;
        let (_b1, x) = buf(64);
        let (_b2, c2) = buf(64 + (64 << 10));
        let c2 = if hot_idx(c2) == hot_idx(x) { c2 + 64 } else { c2 };
        let done = AtomicBool::new(false);
        let fast_seen = AtomicU64::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                while !done.load(Relaxed) {
                    cool();
                    std::thread::sleep(Duration::from_micros(100));
                }
            });
            let hs: Vec<_> = (0..8u64)
                .map(|t| {
                    let fast_seen = &fast_seen;
                    s.spawn(move || {
                        guest();
                        let mut m = Mon::new();
                        for i in 0..K {
                            if t < 4 {
                                loop {
                                    let v = ldx(&mut m, x, 3);
                                    if stx(&mut m, x, 3, v + 1) {
                                        break;
                                    }
                                }
                            } else {
                                rmw(x, 3, |o| Some(o + 1));
                            }
                            store(x + 8, 3, i);
                            rmw(c2, 2, |o| Some((o + 1) & 0xFFFF_FFFF));
                            if cold(c2) {
                                fast_seen.fetch_add(1, Relaxed);
                            }
                            store(c2 + 4, 2, i);
                        }
                    })
                })
                .collect();
            for h in hs {
                h.join().unwrap();
            }
            done.store(true, Relaxed);
        });
        assert_eq!(unsafe { rd(x, 3) }, 8 * K, "contador LDXR/STXR + LDADD");
        // LDADD y stores rapidos a la MISMA palabra (granulo frio): P suma 1 sin parar; Q escribe k<<32 y relee. Solo Q
        // cambia la mitad alta: si el LDADD no fuera atomico frente al store (leer, Q escribe, escribir viejo+1), Q
        // veria su escritura perdida.
        let (_b3, w) = buf(64 + (64 << 10));
        let w = if hot_idx(w) == hot_idx(x) || hot_idx(w) == hot_idx(c2) { w + 64 } else { w };
        let stop = AtomicBool::new(false);
        let mut perdidas = 0u64;
        let mut frio = 0u64;
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                while !stop.load(Relaxed) {
                    rmw(w, 3, |o| Some(o + 1));
                }
            });
            guest();
            for k in 1..200_000u64 {
                frio += cold(w) as u64;
                store(w, 3, k << 32);
                if unsafe { std::ptr::read_volatile(w as *const u64) } >> 32 != k {
                    perdidas += 1;
                }
            }
            stop.store(true, Relaxed);
        });
        assert_eq!(perdidas, 0, "un LDADD concurrente borro un store a la misma direccion (no atomico)");
        assert!(frio > 0, "la palabra nunca estuvo fria: la prueba no ejercita la ruta rapida");
        assert_eq!(unsafe { rd(c2, 2) }, 8 * K, "LDADD sobre granulo frio con stores rapidos a la misma palabra");
        assert!(fast_seen.load(Relaxed) > 0, "C2 nunca estuvo frio: la prueba no ejercita la ruta rapida");
    }

    /// (d) Intruso. d1: B escribe en el MISMO granulo (otra direccion) entre el LDXR y el STXR de A, siempre armando
    /// desde frio: el STXR debe fallar siempre (y tener exito sin intruso). d2: B esta "a mitad de store rapido" (marca
    /// puesta y HOT leido frio) cuando A arma: el LDXR de A debe esperar y leer el dato de B. d3: B escribe sin parar
    /// por la ruta rapida mientras A arma y reserva 100 000 veces: si el STXR tiene exito, el otro dato del granulo no
    /// pudo cambiar entre dos lecturas hechas dentro de la reserva (exactitud frente a ABA).
    #[test]
    fn d_intruso_en_el_mismo_granulo() {
        if !en_modo_fence("monitor::fence_tests::d_intruso_en_el_mismo_granulo", false) {
            return;
        }
        let _s = serial();
        let (_b, a) = buf(64);
        let me = guest();
        // d1
        let bar = Barrier::new(2);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                for i in 0..2000u64 {
                    bar.wait();
                    if i % 2 == 0 {
                        store(a + 8, 3, i);
                    }
                    bar.wait();
                }
            });
            let mut m = Mon::new();
            let mut malas = 0;
            for i in 0..2000u64 {
                cool();
                assert!(cold(a));
                let v = ldx(&mut m, a, 3);
                bar.wait();
                bar.wait();
                let ok = stx(&mut m, a, 3, v + 1);
                malas += (ok != (i % 2 == 1)) as u32; // (sin assert aqui: el otro hilo espera en la barrera)
            }
            assert_eq!(malas, 0, "el STXR debe fallar si y solo si hubo intruso");
        });
        // d2
        for i in 0..20u64 {
            cool();
            me.in_store.store(1, SeqCst); // S1 de un store que ...
            assert!(cold(a)); // ... ya leyo HOT frio (S2) y aun no escribio
            let fin = AtomicU64::new(0);
            std::thread::scope(|s| {
                s.spawn(|| {
                    guest();
                    let mut m = Mon::new();
                    let v = ldx(&mut m, a, 3);
                    fin.store(v | 1 << 63, SeqCst);
                    assert!(stx(&mut m, a, 3, v + 1), "sin escrituras tras el LDXR el STXR debe tener exito");
                });
                std::thread::sleep(Duration::from_millis(30));
                assert_eq!(fin.load(SeqCst), 0, "el armado no espero al store en curso");
                unsafe { std::ptr::write_volatile(a as *mut u64, 0xBEEF00 + i) }; // S3
                me.in_store.store(0, SeqCst); // S4
            });
            assert_eq!(fin.load(SeqCst), (0xBEEF00 + i) | 1 << 63, "el LDXR debe ver el store que estaba en curso al armar");
        }
        // d3
        let stop = AtomicBool::new(false);
        let (mut viol, mut interf, mut oks) = (0u64, 0u64, 0u64);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                let mut seq = 1u64;
                let mut r = 0x9E37_79B9u32;
                while !stop.load(Relaxed) {
                    store(a + 8, 3, seq);
                    seq += 1;
                    r ^= r << 13;
                    r ^= r >> 17;
                    r ^= r << 5;
                    for _ in 0..(r % 2048) {
                        spin_loop();
                    }
                }
            });
            let mut m = Mon::new();
            for _ in 0..100_000 {
                cool();
                let v = ldx(&mut m, a, 3);
                let s1 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                for _ in 0..20 {
                    spin_loop();
                }
                let s2 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                let ok = stx(&mut m, a, 3, v + 1);
                viol += (ok && s1 != s2) as u64;
                interf += (s1 != s2) as u64;
                oks += ok as u64;
            }
            stop.store(true, Relaxed);
        });
        println!("d3: interferencias={} exitos={} violaciones={}", interf, oks, viol);
        // d4 (prueba de tipo "litmus" del hueco de Dekker, el que cierra membarrier): A arma y reserva `a` a la vez
        // que B hace un store rapido a a+8 (arranque sincronizado por giro, sin dormir). Si el STXR de A tiene exito,
        // el store de B tuvo que ser visible antes de que A leyera a+8 justo despues del LDXR.
        let go = AtomicU64::new(0);
        let fin_b = AtomicU64::new(0);
        const ITER4: u64 = 300_000;
        let mut viol4 = 0u64;
        std::thread::scope(|s| {
            s.spawn(|| {
                let fb = guest() as *const MonFlag;
                for k in 1..=ITER4 {
                    while go.load(Acquire) != k {
                        spin_loop();
                    }
                    for _ in 0..(k * 7919) % 400 {
                        spin_loop(); // barre la ventana del armado de A
                    }
                    unsafe { fence_store(fb, a + 8, 3, k) };
                    fin_b.store(k, Release);
                }
            });
            let mut m = Mon::new();
            for k in 1..=ITER4 {
                cool();
                go.store(k, Release);
                let v = ldx(&mut m, a, 3);
                let s1 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                while fin_b.load(Acquire) != k {
                    spin_loop();
                }
                let ok = stx(&mut m, a, 3, v + 1);
                viol4 += (ok && s1 != k) as u64;
            }
        });
        println!("d4: violaciones={}", viol4);
        assert_eq!(viol4, 0, "STXR con exito aunque el store rapido de otro hilo llego despues del LDXR");
        assert_eq!(viol, 0, "STXR con exito aunque otro hilo escribio en el granulo dentro de la reserva");
        assert!(interf > 0 && oks > 0, "la prueba no ejercito el caso (interferencias={} exitos={})", interf, oks);
    }

    #[repr(C)]
    struct SigAct {
        handler: usize,
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    extern "C" {
        fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
    }
    const SIG_E: i32 = 40; // tiempo real: no la usa ninguna otra prueba

    static E_HANDLED: AtomicU64 = AtomicU64::new(0);
    static E_BUF: AtomicUsize = AtomicUsize::new(0);

    /// Lo que hace `sig::async_host_handler` y luego un manejador guest que arma un granulo (LDXR/STXR) y escribe:
    /// si corriera con la marca in_store del hilo puesta, su propio armado lo esperaria para siempre.
    extern "C" fn e_handler(sig: i32, info: *mut u8, _uc: *mut std::ffi::c_void) {
        if unsafe { defer_signal(sig, info) } {
            return;
        }
        let n = E_HANDLED.load(Relaxed);
        let b = E_BUF.load(Relaxed);
        if b != 0 {
            let addr = b + ((n as usize * 64) % (8 << 20));
            let mut m = Mon::new();
            let v = ldx(&mut m, addr, 3);
            let _ = stx(&mut m, addr, 3, v + 1);
            store(addr + 8, 3, n);
        }
        E_HANDLED.fetch_add(1, SeqCst);
    }

    fn install(sig: i32, h: usize) -> SigAct {
        let act = SigAct { handler: h, mask: [0; 16], flags: 4 /* SA_SIGINFO */, restorer: 0 };
        let mut old = SigAct { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
        assert_eq!(unsafe { sigaction(sig, &act, &mut old) }, 0);
        old
    }

    /// (e) Senales. e1 (determinista): una senal que llega con in_store = 1 se aplaza y la salida del store la
    /// reenvia. e2: 100 000 senales (una en vuelo cada vez: si alguna se pierde o el hilo se interbloquea, se agota el
    /// tiempo) sobre un hilo que hace stores rapidos sin parar, con otro hilo armando granulos sin parar y un
    /// manejador que tambien arma.
    #[test]
    fn e_senales_durante_stores_rapidos_y_armados() {
        if !en_modo_fence("monitor::fence_tests::e_senales_durante_stores_rapidos_y_armados", false) {
            return;
        }
        let _s = serial();
        let old = install(SIG_E, e_handler as usize);
        let (pid, me_tid) = unsafe { (crate::sys::syscall(39), crate::sys::syscall(186)) };
        // e1
        let f = guest();
        E_BUF.store(0, SeqCst);
        let (h0, d0) = (E_HANDLED.load(SeqCst), DEFER_IN_STORE.load(SeqCst));
        f.in_store.store(1, SeqCst);
        unsafe { crate::sys::syscall(234, pid, me_tid, SIG_E as i64) };
        assert_eq!(E_HANDLED.load(SeqCst), h0, "el manejador no debe correr a mitad de un store");
        assert_eq!(DEFER_IN_STORE.load(SeqCst), d0 + 1);
        assert_eq!(f.pending.load(SeqCst), 1);
        f.in_store.store(0, SeqCst);
        fst_flush(f); // lo que hace la salida del store al ver `pending`
        assert_eq!(E_HANDLED.load(SeqCst), h0 + 1, "la senal aplazada no se reenvio");
        assert_eq!(f.pending.load(SeqCst), 0);
        // e2
        let (_hb, hbuf) = buf(8 << 20);
        let (_wb, wbuf) = buf(32 << 20);
        let (_ab, abuf) = buf(32 << 20);
        E_BUF.store(hbuf, SeqCst);
        let stop = AtomicBool::new(false);
        let wtid = AtomicU64::new(0);
        let armados = AtomicU64::new(0);
        const N: u64 = 100_000;
        let mut rate = 0.0;
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                wtid.store(unsafe { crate::sys::syscall(186) } as u64, SeqCst);
                let mut i = 0usize;
                let mut m = Mon::new();
                while !stop.load(Relaxed) {
                    store(wbuf + (i * 8) % (32 << 20), 3, i as u64);
                    if i % 256 == 0 {
                        let v = ldx(&mut m, wbuf + 64 * (i % 1024), 3);
                        let _ = stx(&mut m, wbuf + 64 * (i % 1024), 3, v);
                    }
                    i += 1;
                }
            });
            s.spawn(|| {
                guest();
                let mut m = Mon::new();
                let mut i = 0usize;
                while !stop.load(Relaxed) {
                    let addr = abuf + (i * 64) % (32 << 20);
                    let v = ldx(&mut m, addr, 3);
                    let _ = stx(&mut m, addr, 3, v);
                    store(addr + 8, 3, 1);
                    if i % 64 == 0 {
                        cool();
                    }
                    armados.fetch_add(1, Relaxed);
                    i += 1;
                }
            });
            wait_until("hilo de stores", || wtid.load(SeqCst) != 0);
            let wt = wtid.load(SeqCst) as i64;
            let base = E_HANDLED.load(SeqCst);
            let t0 = Instant::now();
            for k in 0..N {
                unsafe { crate::sys::syscall(234, pid, wt, SIG_E as i64) };
                let t1 = Instant::now();
                while E_HANDLED.load(SeqCst) <= base + k {
                    if t1.elapsed() > Duration::from_secs(10) {
                        // el hilo receptor puede estar bloqueado para siempre: salir del proceso (la prueba se
                        // ejecuta en un proceso propio) en lugar de esperar su fin
                        println!("senal {} sin entregar tras 10 s: perdida o interbloqueo", k);
                        std::process::exit(101);
                    }
                    spin_loop();
                }
            }
            rate = N as f64 / t0.elapsed().as_secs_f64();
            stop.store(true, SeqCst);
        });
        let defer = DEFER_IN_STORE.load(SeqCst) - d0 - 1;
        println!("e2: {} senales a {:.0}/s, aplazadas por in_store={}, armados del otro hilo={}", N, rate, defer, armados.load(Relaxed));
        assert!(defer > 0, "ninguna senal cayo a mitad de un store rapido: la prueba no ejercita el caso");
        assert!(armados.load(Relaxed) > 0);
        unsafe { sigaction(SIG_E, &old, std::ptr::null_mut()) };
        E_BUF.store(0, SeqCst);
    }

    static S_FIXED: AtomicBool = AtomicBool::new(false);
    static S_INSTORE: AtomicU64 = AtomicU64::new(9);
    static S_ENTERED: AtomicBool = AtomicBool::new(false);
    static S_ARMED: AtomicBool = AtomicBool::new(false);
    static S_PAGE: AtomicUsize = AtomicUsize::new(0);
    static S_FLAG: AtomicUsize = AtomicUsize::new(0);

    extern "C" fn segv_handler(_sig: i32, _info: *mut u8, uc: *mut std::ffi::c_void) {
        S_FIXED.store(unsafe { fault_in_store(uc) }, SeqCst);
        S_INSTORE.store(unsafe { (*(S_FLAG.load(SeqCst) as *const MonFlag)).in_store.load(SeqCst) } as u64, SeqCst);
        // manejador guest "largo": mientras tanto otro hilo arma un granulo y no debe esperar a este hilo
        S_ENTERED.store(true, SeqCst);
        let t0 = Instant::now();
        while !S_ARMED.load(SeqCst) && t0.elapsed() < Duration::from_secs(5) {
            spin_loop();
        }
        unsafe { crate::sys::syscall(10, S_PAGE.load(SeqCst) as i64, 4096i64, 3i64) }; // PROT_READ|PROT_WRITE
    }

    /// (e3) Fallo sincrono (SIGSEGV) en el mov del store rapido: el manejador corre sin la marca (un armado
    /// concurrente no lo espera) y, al volver, el store se repite entero y escribe.
    #[test]
    fn e3_fallo_en_el_store_rapido_reinicia_sin_marca() {
        if !en_modo_fence("monitor::fence_tests::e3_fallo_en_el_store_rapido_reinicia_sin_marca", false) {
            return;
        }
        let _s = serial();
        let f = guest();
        S_FLAG.store(f as *const MonFlag as usize, SeqCst);
        let (_pb, pbase) = buf(3 * 4096);
        let page = (pbase + 4095) & !4095;
        S_PAGE.store(page, SeqCst);
        let (_ab, abuf) = buf(4096);
        for k in 0..20u64 {
            S_ENTERED.store(false, SeqCst);
            S_ARMED.store(false, SeqCst);
            S_FIXED.store(false, SeqCst);
            assert!(cold(page + 128));
            assert_eq!(unsafe { crate::sys::syscall(10, page as i64, 4096i64, 1i64) }, 0); // PROT_READ
            let old = install(11, segv_handler as usize);
            std::thread::scope(|s| {
                s.spawn(|| {
                    guest();
                    wait_until("manejador de SIGSEGV", || S_ENTERED.load(SeqCst));
                    cool();
                    let mut m = Mon::new();
                    let v = ldx(&mut m, abuf + 64 * (k as usize % 32), 3); // arma: no debe esperar al hilo del fallo
                    let _ = stx(&mut m, abuf + 64 * (k as usize % 32), 3, v);
                    S_ARMED.store(true, SeqCst);
                });
                store(page + 128, 3, 0x5EC0 + k);
            });
            unsafe { sigaction(11, &old, std::ptr::null_mut()) };
            assert!(S_FIXED.load(SeqCst), "el fallo no se reconocio como store rapido");
            assert_eq!(S_INSTORE.load(SeqCst), 0, "el manejador corrio con la marca puesta");
            assert!(S_ARMED.load(SeqCst), "el armado espero al hilo que estaba en el manejador");
            assert_eq!(unsafe { rd(page + 128, 3) }, 0x5EC0 + k, "el store no se repitio al volver");
            assert_eq!(f.in_store.load(SeqCst), 0);
        }
    }

    /// (g) Atomicas LSE por la via rapida (granulo frio, `heddle_fcas*`) frente al monitor. g1: LDADD de B sobre el
    /// MISMO granulo entre el LDXR y el STXR de A (armando desde frio): el STXR falla si y solo si hubo LDADD. g3: B hace
    /// LDADD sin parar (rapido mientras el granulo esta frio) y A arma y reserva 100 000 veces: si el STXR tiene exito,
    /// el dato de B no cambio entre dos lecturas dentro de la reserva. g4: litmus del hueco de Dekker con SWP rapido de
    /// B sincronizado con el armado de A. g5: SIGSEGV en el `lock cmpxchg` (pagina de solo lectura): el manejador corre
    /// sin la marca, un armado concurrente no lo espera y la atomica se repite entera al volver.
    #[test]
    fn g_atomicas_rapidas_frente_al_monitor() {
        if !en_modo_fence("monitor::fence_tests::g_atomicas_rapidas_frente_al_monitor", false) {
            return;
        }
        let _s = serial();
        let (_b, a) = buf(64);
        let f = guest();
        // la via rapida existe: LDADD sobre granulo frio no lo arma ni sube la version
        cool();
        assert!(cold(a));
        let v0 = gran(a).ver.load(SeqCst);
        assert_eq!(rmw(a + 8, 3, |o| Some(o + 5)), 0);
        assert!(cold(a) && gran(a).ver.load(SeqCst) == v0, "el LDADD sobre granulo frio no fue por la via rapida");
        assert_eq!(unsafe { rd(a + 8, 3) }, 5);
        // CAS fallido: no escribe y devuelve el valor actual
        assert_eq!(rmw(a + 8, 3, |o| if o == 7 { Some(9) } else { None }), 5);
        assert_eq!(unsafe { rd(a + 8, 3) }, 5);
        // tamanos menores: solo cambia el elemento
        unsafe { std::ptr::write_volatile((a + 16) as *mut u64, 0x1122_3344_5566_77FF) };
        assert_eq!(rmw(a + 16, 0, |o| Some((o + 1) & 0xFF)), 0xFF);
        assert_eq!(rmw(a + 18, 1, |o| Some((o + 1) & 0xFFFF)), 0x5566);
        assert_eq!(rmw(a + 20, 2, |o| Some((o + 1) & 0xFFFF_FFFF)), 0x1122_3344);
        assert_eq!(unsafe { rd(a + 16, 3) }, 0x1122_3345_5567_7700);
        assert_eq!(f.in_store.load(SeqCst), 0);
        // g1
        let bar = Barrier::new(2);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                for i in 0..2000u64 {
                    bar.wait();
                    if i % 2 == 0 {
                        rmw(a + 8, 3, |o| Some(o.wrapping_add(1)));
                    }
                    bar.wait();
                }
            });
            let mut m = Mon::new();
            let mut malas = 0;
            for i in 0..2000u64 {
                cool();
                assert!(cold(a));
                let v = ldx(&mut m, a, 3);
                bar.wait();
                bar.wait();
                let ok = stx(&mut m, a, 3, v + 1);
                malas += (ok != (i % 2 == 1)) as u32;
            }
            assert_eq!(malas, 0, "el STXR debe fallar si y solo si hubo un LDADD de otro hilo en el granulo");
        });
        // g3
        let stop = AtomicBool::new(false);
        let rapidas = AtomicU64::new(0);
        let (mut viol, mut interf, mut oks) = (0u64, 0u64, 0u64);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                let mut r = 0x9E37_79B9u32;
                while !stop.load(Relaxed) {
                    let fr = cold(a);
                    rmw(a + 8, 3, |o| Some(o.wrapping_add(1)));
                    rapidas.fetch_add(fr as u64, Relaxed);
                    r ^= r << 13;
                    r ^= r >> 17;
                    r ^= r << 5;
                    for _ in 0..(r % 2048) {
                        spin_loop();
                    }
                }
            });
            let mut m = Mon::new();
            // con la CPU cargada (pruebas en paralelo) el otro hilo puede no llegar a ver el granulo frio en 100 000
            // vueltas: se repite (hasta 10 veces) hasta ejercitar todos los casos
            for _ in 0..10 {
                for _ in 0..100_000 {
                    cool();
                    let v = ldx(&mut m, a, 3);
                    let s1 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                    for _ in 0..20 {
                        spin_loop();
                    }
                    let s2 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                    let ok = stx(&mut m, a, 3, v + 1);
                    viol += (ok && s1 != s2) as u64;
                    interf += (s1 != s2) as u64;
                    oks += ok as u64;
                }
                if interf > 0 && oks > 0 && rapidas.load(Relaxed) > 0 {
                    break;
                }
            }
            stop.store(true, Relaxed);
        });
        println!("g3: interferencias={} exitos={} violaciones={} ldadd_con_granulo_frio={}", interf, oks, viol, rapidas.load(Relaxed));
        assert_eq!(viol, 0, "STXR con exito aunque un LDADD de otro hilo escribio en el granulo dentro de la reserva");
        assert!(interf > 0 && oks > 0 && rapidas.load(Relaxed) > 0, "la prueba no ejercito el caso");
        // g4
        let go = AtomicU64::new(0);
        let fin_b = AtomicU64::new(0);
        const ITER4: u64 = 300_000;
        let mut viol4 = 0u64;
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                for k in 1..=ITER4 {
                    while go.load(Acquire) != k {
                        spin_loop();
                    }
                    for _ in 0..(k * 7919) % 400 {
                        spin_loop();
                    }
                    rmw(a + 8, 3, |_| Some(k)); // SWP
                    fin_b.store(k, Release);
                }
            });
            let mut m = Mon::new();
            for k in 1..=ITER4 {
                cool();
                go.store(k, Release);
                let v = ldx(&mut m, a, 3);
                let s1 = unsafe { std::ptr::read_volatile((a + 8) as *const u64) };
                while fin_b.load(Acquire) != k {
                    spin_loop();
                }
                let ok = stx(&mut m, a, 3, v + 1);
                viol4 += (ok && s1 != k) as u64;
            }
        });
        println!("g4: violaciones={}", viol4);
        assert_eq!(viol4, 0, "STXR con exito aunque el SWP rapido de otro hilo llego despues del LDXR");
        // g5
        S_FLAG.store(f as *const MonFlag as usize, SeqCst);
        let (_pb, pbase) = buf(3 * 4096);
        let page = (pbase + 4095) & !4095;
        S_PAGE.store(page, SeqCst);
        let (_ab, abuf) = buf(4096);
        for k in 0..20u64 {
            S_ENTERED.store(false, SeqCst);
            S_ARMED.store(false, SeqCst);
            S_FIXED.store(false, SeqCst);
            assert!(cold(page + 128));
            assert_eq!(unsafe { crate::sys::syscall(10, page as i64, 4096i64, 1i64) }, 0); // PROT_READ
            let old = install(11, segv_handler as usize);
            let antes = unsafe { rd(page + 128, 3) };
            let mut previo = 0;
            std::thread::scope(|s| {
                s.spawn(|| {
                    guest();
                    wait_until("manejador de SIGSEGV", || S_ENTERED.load(SeqCst));
                    cool();
                    let mut m = Mon::new();
                    let v = ldx(&mut m, abuf + 64 * (k as usize % 32), 3);
                    let _ = stx(&mut m, abuf + 64 * (k as usize % 32), 3, v);
                    S_ARMED.store(true, SeqCst);
                });
                previo = rmw(page + 128, 3, |o| Some(o + 0x5EC0 + k));
            });
            unsafe { sigaction(11, &old, std::ptr::null_mut()) };
            assert!(S_FIXED.load(SeqCst), "el fallo no se reconocio como CAS rapido");
            assert_eq!(S_INSTORE.load(SeqCst), 0, "el manejador corrio con la marca puesta");
            assert!(S_ARMED.load(SeqCst), "el armado espero al hilo que estaba en el manejador");
            assert_eq!(previo, antes);
            assert_eq!(unsafe { rd(page + 128, 3) }, antes + 0x5EC0 + k, "la atomica no se repitio al volver");
            assert_eq!(f.in_store.load(SeqCst), 0);
        }
    }

    /// (f) Hilos que nacen y mueren (2000, de 8 en 8) mientras otro hilo arma granulos sin parar.
    #[test]
    fn f_hilos_que_nacen_y_mueren_durante_armados() {
        if !en_modo_fence("monitor::fence_tests::f_hilos_que_nacen_y_mueren_durante_armados", false) {
            return;
        }
        let _s = serial();
        let live0 = crate::mem::THREADS_LIVE.load(SeqCst);
        let (_ab, abuf) = buf(16 << 20);
        let (_wb, wbuf) = buf(8 << 20);
        let stop = AtomicBool::new(false);
        let armados = AtomicU64::new(0);
        std::thread::scope(|s| {
            s.spawn(|| {
                guest();
                let mut m = Mon::new();
                let mut i = 0usize;
                while !stop.load(Relaxed) {
                    let addr = abuf + (i * 64) % (16 << 20);
                    let v = ldx(&mut m, addr, 3);
                    let _ = stx(&mut m, addr, 3, v);
                    if i % 64 == 0 {
                        cool();
                    }
                    armados.fetch_add(1, Relaxed);
                    i += 1;
                }
            });
            let t0 = Instant::now();
            for g in 0..250usize {
                let hs: Vec<_> = (0..8usize)
                    .map(|t| {
                        s.spawn(move || {
                            guest();
                            let base = wbuf + ((g * 8 + t) * 4096) % (8 << 20);
                            for j in 0..1000usize {
                                store(base + (j * 8) % 4096, 3, j as u64);
                            }
                        })
                    })
                    .collect();
                for h in hs {
                    h.join().unwrap();
                }
                assert!(t0.elapsed() < Duration::from_secs(60), "hilos bloqueados");
            }
            stop.store(true, SeqCst);
        });
        println!("f: armados={}", armados.load(Relaxed));
        assert!(armados.load(Relaxed) > 0);
        // los 2000 hilos (y el armador) terminaron: sus GuestThread se liberaron y salieron de la lista
        wait_until("liberacion de los GuestThread", || crate::mem::THREADS_LIVE.load(SeqCst) == live0);
    }

    /// Microbenchmark (modo fence): `cargo test --release --lib monitor::fence_tests::bench_store -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_store() {
        if !en_modo_fence("monitor::fence_tests::bench_store", true) {
            return;
        }
        let _s = serial();
        let f = guest();
        let (_b, base) = buf(1 << 20);
        cool();
        let n = 50_000_000u64;
        let t = |name: &str, n: u64, mut body: Box<dyn FnMut(u64)>| {
            for _ in 0..1000 {
                body(0);
            }
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                for i in 0..n {
                    body(i);
                }
                best = best.min(t0.elapsed().as_nanos() as f64 / n as f64);
            }
            println!("BENCH {:<58} {:>8.2} ns", name, best);
        };
        let fp = f as *const MonFlag;
        // 32 KiB de bloques frios (512 granulos)
        t("store rapido (asm, granulo frio; lo que llama el JIT)", n, Box::new(move |i| unsafe { fence_store(fp, base + ((i as usize * 8) & 0x7ff8), 3, i) }));
        t("store() desde Rust (TLS + asm, granulo frio)", n, Box::new(move |i| store(base + ((i as usize * 8) & 0x7ff8), 3, i)));
        t("ruta con bloqueo (slow_store; modo bloqueo de antes)", n, Box::new(move |i| unsafe { slow_store(base + ((i as usize * 8) & 0x7ff8), 3, i) }));
        // granulo caliente: reservado una vez
        let hot = base + (512 << 10);
        let mut m = Mon::new();
        ldx(&mut m, hot, 3);
        clrex(&mut m);
        assert!(!cold(hot));
        t("store rapido a granulo caliente (cae a la ruta con bloqueo)", n, Box::new(move |i| unsafe { fence_store(fp, hot + ((i as usize * 8) & 0x38), 3, i) }));
        // JIT: bloque de 32 STR x0,[x3] / ST1 {v0.16b},[x3] con la Cpu de un hilo guest
        let jit_ns = |w: u32, monflag: usize| -> f64 {
            #[repr(align(4096))]
            struct Page([u32; 1024]);
            let mut page = Box::new(Page([0; 1024]));
            for i in 0..32 {
                page.0[i] = w;
            }
            page.0[32] = 0x1400_0000 | ((-32i64) as u32 & 0x03FF_FFFF);
            let pc = page.0.as_ptr() as u64;
            let mut jit = crate::jit::Jit::new();
            jit.chain = false;
            let mut c = crate::cpu::Cpu::new();
            c.x[3] = base as u64;
            c.monflag = monflag;
            c.pc = pc;
            let bf = jit.lookup(pc);
            for _ in 0..1000 {
                bf(&mut *c);
            }
            let reps = 1_000_000u64;
            let t0 = Instant::now();
            for _ in 0..reps {
                bf(&mut *c);
            }
            t0.elapsed().as_nanos() as f64 / (reps * 32) as f64
        };
        let fu = f as *const MonFlag as usize;
        println!("BENCH {:<58} {:>8.2} ns", "JIT STR x0,[x3] (ruta rapida)", jit_ns(0xF9000060, fu));
        println!("BENCH {:<58} {:>8.2} ns", "JIT STR x0,[x3] (sin marca: ruta con bloqueo)", jit_ns(0xF9000060, 0));
        println!("BENCH {:<58} {:>8.2} ns", "JIT ST1 {v0.16b},[x3] (ruta rapida, 2 stores)", jit_ns(0x4C007060, fu));
        println!("BENCH {:<58} {:>8.2} ns", "JIT ST1 {v0.16b},[x3] (sin marca: ruta con bloqueo)", jit_ns(0x4C007060, 0));
        // armado: enfriar + LDXR sobre un granulo frio (membarrier + espera), sin y con 3 hilos haciendo stores
        let arm_us = |reps: usize| -> f64 {
            let mut m = Mon::new();
            let t0 = Instant::now();
            for i in 0..reps {
                cool();
                ldx(&mut m, base + 64 * (i % 1024), 3);
            }
            t0.elapsed().as_nanos() as f64 / reps as f64 / 1000.0
        };
        let cool_us = {
            let t0 = Instant::now();
            for _ in 0..20000 {
                cool();
            }
            t0.elapsed().as_nanos() as f64 / 20000.0 / 1000.0
        };
        println!("BENCH {:<58} {:>8.2} us", "enfriar (cool, referencia)", cool_us);
        println!("BENCH {:<58} {:>8.2} us", "armado (cool + LDXR frio), sin otros hilos", arm_us(20000));
        let stop = AtomicBool::new(false);
        let (_wb, wbuf) = buf(1 << 20);
        std::thread::scope(|s| {
            for t in 0..3usize {
                let stop = &stop;
                s.spawn(move || {
                    guest();
                    let mut i = 0usize;
                    while !stop.load(Relaxed) {
                        store(wbuf + t * (256 << 10) + (i * 8) % (256 << 10), 3, i as u64);
                        i += 1;
                    }
                });
            }
            std::thread::sleep(Duration::from_millis(50));
            println!("BENCH {:<58} {:>8.2} us", "armado (cool + LDXR frio), 3 hilos haciendo stores", arm_us(20000));
            stop.store(true, SeqCst);
        });
        println!("BENCH esperas a un store en curso={} espera maxima={:.1} us", FENCE_WAITS.load(Relaxed), FENCE_WAIT_MAX_NS.load(Relaxed) as f64 / 1000.0);
    }
}
