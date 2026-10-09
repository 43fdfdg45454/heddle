//! Senales: tabla de acciones guest, manejador host que reenvia al guest, fallos sincronos.
//! Se usa la syscall rt_sigaction cruda con la estructura del kernel x86-64, asi no depende de la libc.

use crate::cpu::Cpu;
use crate::rt;
use crate::sys::*;
use std::os::raw::c_void;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{fence, AtomicU64};

#[derive(Clone, Copy, Default)]
pub struct GuestAct {
    pub handler: u64,
    pub flags: u64,
    pub mask: u64,
}

pub const SA_SIGINFO: u64 = 4;
pub const SA_ONSTACK: u64 = 0x0800_0000;
pub const SA_RESTART: u64 = 0x1000_0000;
pub const SA_NODEFER: u64 = 0x4000_0000;
pub const SA_RESETHAND: u64 = 0x8000_0000;
const SA_NOCLDSTOP: u64 = 1;
const SA_NOCLDWAIT: u64 = 2;
const SA_RESTORER: u64 = 0x0400_0000;

/// Accion guest de cada senal (1..=64), legible sin bloqueos ni reservas desde un manejador de senal: un seqlock
/// por senal. Los escritores (sigaction, SA_RESETHAND) se serializan con el propio contador (impar = escribiendo).
struct ActSlot {
    seq: AtomicU64,
    handler: AtomicU64,
    flags: AtomicU64,
    mask: AtomicU64,
}

impl ActSlot {
    const fn new() -> ActSlot {
        ActSlot { seq: AtomicU64::new(0), handler: AtomicU64::new(0), flags: AtomicU64::new(0), mask: AtomicU64::new(0) }
    }
    fn load(&self) -> GuestAct {
        GuestAct { handler: self.handler.load(Relaxed), flags: self.flags.load(Relaxed), mask: self.mask.load(Relaxed) }
    }
}

static ACTS: [ActSlot; 65] = [const { ActSlot::new() }; 65];
/// Giros como mucho esperando a un escritor: si la senal interrumpio al propio escritor en este hilo, nunca acabaria.
const SEQ_SPINS: u32 = 1 << 16;

fn act_read(sig: i32) -> GuestAct {
    if !(1..=64).contains(&sig) {
        return GuestAct::default();
    }
    let s = &ACTS[sig as usize];
    for _ in 0..SEQ_SPINS {
        let a = s.seq.load(Acquire);
        if a & 1 == 0 {
            let r = s.load();
            fence(Acquire);
            if s.seq.load(Relaxed) == a {
                return r;
            }
        }
        std::hint::spin_loop();
    }
    s.load()
}

/// Sustituye la accion y devuelve la anterior (lectura y escritura atomicas respecto a otros escritores).
fn act_swap(sig: i32, new: GuestAct) -> GuestAct {
    let s = &ACTS[sig as usize];
    let mut owned = false;
    for _ in 0..SEQ_SPINS {
        let c = s.seq.load(Relaxed);
        if c & 1 == 0 && s.seq.compare_exchange_weak(c, c + 1, Acquire, Relaxed).is_ok() {
            owned = true;
            break;
        }
        std::hint::spin_loop();
    }
    fence(Release);
    let old = s.load();
    s.handler.store(new.handler, Relaxed);
    s.flags.store(new.flags, Relaxed);
    s.mask.store(new.mask, Relaxed);
    if owned {
        s.seq.fetch_add(1, Release);
    } else {
        s.seq.fetch_add(2, Release); // escritor interrumpido en este hilo: se conserva su paridad
    }
    old
}

#[repr(C)]
struct KSigaction {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

std::arch::global_asm!(
    ".globl heddle_restorer",
    "heddle_restorer:",
    "mov eax, 15",
    "syscall",
);

extern "C" {
    fn heddle_restorer();
}

fn raw_sigaction(sig: i32, act: Option<&KSigaction>, old: Option<&mut KSigaction>) -> i64 {
    unsafe {
        syscall(
            13,
            sig as i64,
            act.map_or(0usize, |a| a as *const _ as usize),
            old.map_or(0usize, |a| a as *mut _ as usize),
            8usize,
        ) as i64
    }
}

/// Senales en las que bionic instala `debuggerd_signal_handler` al iniciar cada proceso (`debuggerd_register_handlers`):
/// SIGABRT, SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGSTKFLT, SIGSYS, SIGTRAP y BIONIC_SIGNAL_DEBUGGER (35).
const DEBUGGERD_SIGS: [i32; 9] = [6, 7, 8, 4, 11, 16, 31, 5, 35];

/// Slot HLE que hace de `debuggerd_signal_handler` para el guest (`debuggerd_slot`).
static DEBUGGERD: AtomicU64 = AtomicU64::new(0);

/// Registra el `debuggerd_signal_handler` del guest. En un dispositivo, la accion inicial de las senales de
/// `DEBUGGERD_SIGS` es ese manejador (SA_RESTART | SA_SIGINFO | SA_ONSTACK), y es lo que `sigaction` devuelve como
/// accion anterior al primero que instala la suya (sigchain guarda la que habia). Los manejadores de fallos de los
/// motores (Unity, Crashlytics, Breakpad) la encadenan al terminar: si recibieran SIG_DFL volverian sin mas y el
/// fallo se repetiria para siempre. Como bionic (`resend_signal`), pone SIG_DFL y reenvia la senal al propio hilo:
/// al volver, el fallo se repite (o la senal reenviada llega) con la accion por defecto y el proceso termina por la
/// cadena del host (ART, debuggerd). Corre dentro del manejador del host: solo seqlock y llamadas al sistema.
pub fn debuggerd_slot() -> u64 {
    let a = DEBUGGERD.load(Relaxed);
    if a != 0 {
        return a;
    }
    let a = crate::hle::register(
        "__heddle_debuggerd_signal_handler",
        Box::new(|c| {
            let sig = c.x[0] as i32;
            if DEBUGGERD_SIGS.contains(&sig) {
                let _ = guest_sigaction(sig, Some(GuestAct::default()));
                // los fallos sincronos se repiten solos al volver; el resto (abort, raise) se reenvia
                if sig != 35 && !matches!(sig, 4 | 7 | 8 | 11) {
                    unsafe {
                        let pid = syscall(39) as i64;
                        let tid = syscall(186) as i64;
                        syscall(234, pid, tid, sig as i64);
                    }
                }
            }
            crate::hle::Ret::Return
        }),
    );
    DEBUGGERD.store(a, Relaxed);
    a
}

/// Accion guest de `sig` tal como la ve el guest: la que puso, o la inicial de bionic si nunca la cambio.
fn act_visible(sig: i32, a: GuestAct, pristine: bool) -> GuestAct {
    let d = DEBUGGERD.load(Relaxed);
    if pristine && d != 0 && a.handler == 0 && DEBUGGERD_SIGS.contains(&sig) {
        GuestAct { handler: d, flags: SA_RESTART | SA_SIGINFO | SA_ONSTACK, mask: 0 }
    } else {
        a
    }
}

/// Instala (o consulta) la accion guest de una senal. Devuelve la accion anterior o -errno.
pub fn guest_sigaction(sig: i32, new: Option<GuestAct>) -> Result<GuestAct, i32> {
    if sig <= 0 || sig > 64 || sig == 9 || sig == 19 {
        return if new.is_some() { Err(22) } else { Err(22) };
    }
    let pristine = ACTS[sig as usize].seq.load(Acquire) == 0;
    let Some(n) = new else { return Ok(act_visible(sig, act_read(sig), pristine)) };
    let old = act_visible(sig, act_swap(sig, n), pristine);
    {
        // En modo bridge, las senales de fallo las gestiona ART (sigchain): se registra la accion pero no se
        // instala un manejador propio (el fallo llega por getSignalHandler).
        if crate::bridge::BRIDGE_MODE.load(std::sync::atomic::Ordering::Relaxed) && matches!(sig, 4 | 7 | 8 | 11) {
            return Ok(old);
        }
        let ka = if n.handler == 0 || n.handler == 1 {
            // SIG_DFL / SIG_IGN se aplican directamente
            KSigaction { handler: n.handler, flags: SA_RESTORER, restorer: heddle_restorer as u64, mask: 0 }
        } else {
            KSigaction {
                handler: async_host_handler as u64,
                // los fallos sincronos siempre en la pila alternativa del host (un desbordamiento de pila tambien
                // tiene que llegar; ademas su manejador guest se ejecuta ahi y la del puente es de 64 KiB)
                flags: (n.flags & (SA_RESTART | SA_NODEFER | SA_RESETHAND | SA_ONSTACK | SA_NOCLDSTOP | SA_NOCLDWAIT))
                    | SA_SIGINFO
                    | SA_RESTORER
                    | if matches!(sig, 4 | 7 | 8 | 11) { SA_ONSTACK } else { 0 },
                restorer: heddle_restorer as u64,
                mask: kernel_part(n.mask), // SIGSEGV nunca en el nucleo (ver `KEEP_UNBLOCKED`); el resto, como lo pide
            }
        };
        let r = raw_sigaction(sig, Some(&ka), None);
        if r < 0 {
            return Err((-r) as i32);
        }
    }
    Ok(old)
}

/// Accion guest de `sig` (sin bloqueos ni reservas: vale en un manejador de senal). Indice = numero de senal: la 64
/// (SIGRTMAX) tiene la suya (`sig & 63` caeria en la 0).
pub fn guest_act(sig: i32) -> GuestAct {
    act_read(sig)
}

/// Tamano del `ucontext_t` de bionic para aarch64: cabecera (176) + mcontext (regs, sp, pc, pstate) + `__reserved`.
pub const UC_SIZE: u64 = 4560;
const UC_MCONTEXT: u64 = 176;
/// uc_sigmask del ucontext aarch64 de bionic (tras uc_flags, uc_link y uc_stack)
const UC_SIGMASK: u64 = 40;
const UC_RESERVED: u64 = 464;
/// Margen bajo el `sp` interrumpido: una instruccion a medias puede haber escrito por debajo antes de moverlo.
const SIG_MARGIN: u64 = 512;
/// Niveles de manejadores guest anidados en un hilo. Una senal asincrona que llega con todos ocupados se aplaza
/// (`pend`) y se entrega al volver uno; un fallo sincrono no se puede aplazar (se repetiria) y termina el proceso.
pub const MAX_SIG_DEPTH: usize = 8;

/// Marco de senal construido en la pila del guest.
pub struct SigFrame {
    pub info: u64,
    pub uc: u64,
    pub sp: u64,
}

/// Construye el marco de senal como lo haria el kernel: `siginfo` y un `ucontext` aarch64 real con los registros
/// interrumpidos, sobre la pila que el hilo estaba usando (o la alternativa del guest si pidio SA_ONSTACK).
/// Un recolector de basura que recorre la pila desde el `sp` del manejador hasta el final de la pila del hilo
/// encuentra asi memoria contigua y los registros del hilo. `fallback_top` se usa si no hay pila utilizable.
///
/// # Safety
/// `info` es nulo o apunta a un `siginfo` de 128 bytes; la pila elegida (`stack`, `alt` o `fallback_top`) debe ser
/// memoria escribible del hilo con espacio para el marco.
pub unsafe fn build_frame(cur: &Cpu, stack: (u64, u64), alt: (u64, u64, u64), onstack: bool, fallback_top: u64, info: *const u8) -> SigFrame {
    let need = SIG_MARGIN + UC_SIZE + 128 + 4096;
    let isp = cur.x[31];
    let (alo, ahi) = (alt.0, alt.0.wrapping_add(alt.2));
    let in_alt = alt.0 != 0 && isp > alo && isp <= ahi;
    let mut top = if isp < 0x10000 { stack.1.wrapping_sub(64) } else { isp.wrapping_sub(SIG_MARGIN) };
    if onstack && alt.0 != 0 && alt.1 & 2 == 0 && alt.2 >= need && !in_alt {
        top = ahi;
    } else if top > stack.0 && top <= stack.1 && top - stack.0 < need {
        top = fallback_top; // pila del hilo agotada: no se escribe sobre la pagina de guarda
    }
    let uc = (top - UC_SIZE) & !15;
    let infoa = (uc - 128) & !15;
    unsafe {
        if info.is_null() {
            std::ptr::write_bytes(infoa as *mut u8, 0, 128);
        } else {
            std::ptr::copy_nonoverlapping(info, infoa as *mut u8, 128);
        }
        std::ptr::write_bytes(uc as *mut u8, 0, UC_SIZE as usize);
        let w = |off: u64, v: u64| *((uc + off) as *mut u64) = v;
        // uc_stack: la pila alternativa del guest
        w(16, alt.0);
        w(24, alt.1);
        w(32, alt.2);
        let m = UC_MCONTEXT;
        w(m, if info.is_null() { 0 } else { *(info.add(16) as *const u64) }); // fault_address
        for i in 0..31 {
            w(m + 8 + 8 * i as u64, cur.x[i]);
        }
        w(m + 256, isp);
        w(m + 264, cur.pc);
        w(m + 272, cur.flags() | cur.ssbs()); // NZCV y PSTATE.SSBS
        // fpsimd_context + terminador
        let r = uc + UC_RESERVED;
        *(r as *mut u32) = 0x4650_8001;
        *((r + 4) as *mut u32) = 528;
        *((r + 8) as *mut u32) = cur.fpsr as u32;
        *((r + 12) as *mut u32) = cur.fpcr as u32;
        for i in 0..32 {
            *((r + 16 + 16 * i as u64) as *mut [u64; 2]) = cur.v[i];
        }
    }
    SigFrame { info: infoa, uc, sp: (infoa - 64) & !15 }
}

// ---------------------------------------------------------------------------------------------
// Mascara de senales del guest (lo que ve una app ARM con libsigchain y bionic)
// ---------------------------------------------------------------------------------------------
//
// En un dispositivo ARM, `sigprocmask`/`sigprocmask64`/`pthread_sigmask` de la app pasan por libsigchain (ART la
// interpone) y por bionic:
//  * libsigchain (`__sigprocmask`): con SIG_BLOCK o SIG_SETMASK quita del conjunto nuevo las senales que reclama ART
//    (`CLAIMED`); con SIG_UNBLOCK lo pasa tal cual. Solo lo hace fuera de sus propios manejadores especiales (los de
//    ART), nunca en los de la app. El conjunto viejo que devuelve es el del nucleo, sin tocar.
//  * bionic (`filter_reserved_signals`): con SIG_BLOCK/SIG_SETMASK anade __SIGRTMIN (temporizadores POSIX) y quita
//    las de tiempo real reservadas __SIGRTMIN+1..SIGRTMIN-1; con SIG_UNBLOCK al reves (nunca desbloquea __SIGRTMIN y
//    siempre desbloquea las reservadas).
// La mascara del nucleo, `uc_sigmask` de los marcos, la que guarda `sigsetjmp` y el conjunto viejo son la mascara real
// del nucleo. La llamada al sistema directa (`svc` rt_sigprocmask), la mascara de `sigaction` y `uc_sigmask` al volver
// de un manejador no se filtran (en el dispositivo tampoco: van al nucleo sin pasar por libsigchain ni bionic).
//
// Unica diferencia interna: el puente nunca bloquea SIGSEGV en el nucleo (`KEEP_UNBLOCKED`: los trampolines del host
// a codigo guest y la reanudacion de stores dependen de el). Cuando en el dispositivo el nucleo la tendria bloqueada
// (llamada directa al sistema, mascara de un manejador, `uc_sigmask` devuelto), se anota en `GuestThread::sig_view` y se
// suma a todo lo que el guest consulta; un fallo de SIGSEGV con ella "bloqueada" termina el proceso como lo haria el
// nucleo (SIG_DFL), sin llegar al manejador guest.

/// Instancias extra de senales de tiempo real aplazadas en un hilo: como el nucleo, cada envio de una senal RT
/// se entrega con su `siginfo` (sigqueue con valores distintos) en lugar de fundirse con la que ya esta pendiente. El
/// primer ejemplar va en `sig_pinfo` (como las estandar); los siguientes, aqui, en orden de llegada. Cola fija: llena,
/// la instancia se funde (como antes) y se cuenta en `lost`. La escriben los manejadores del host de este hilo
/// (pueden anidarse: cada entrada se reserva con un CAS) y la vacia `deliver_pending`; sin reservas ni bloqueos.
pub const RTQ_CAP: usize = 32;
const RTQ_FILLING: u32 = u32::MAX;

pub struct RtQueue {
    /// 0 libre; RTQ_FILLING escribiendose; si no, el numero de senal (lista)
    sig: [std::sync::atomic::AtomicU32; RTQ_CAP],
    seq: [std::sync::atomic::AtomicU64; RTQ_CAP],
    info: [std::cell::UnsafeCell<[u8; 128]>; RTQ_CAP],
    next: std::sync::atomic::AtomicU64,
    pub lost: std::sync::atomic::AtomicU32,
}

unsafe impl Sync for RtQueue {}

impl RtQueue {
    pub const fn new() -> RtQueue {
        RtQueue {
            sig: [const { std::sync::atomic::AtomicU32::new(0) }; RTQ_CAP],
            seq: [const { std::sync::atomic::AtomicU64::new(0) }; RTQ_CAP],
            info: [const { std::cell::UnsafeCell::new([0u8; 128]) }; RTQ_CAP],
            next: std::sync::atomic::AtomicU64::new(0),
            lost: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Agrega una instancia; false si la cola esta llena.
    pub fn push(&self, sig: u32, info: &[u8; 128]) -> bool {
        use std::sync::atomic::Ordering::*;
        for i in 0..RTQ_CAP {
            if self.sig[i].compare_exchange(0, RTQ_FILLING, Acquire, Relaxed).is_ok() {
                unsafe { *self.info[i].get() = *info };
                self.seq[i].store(self.next.fetch_add(1, Relaxed), Relaxed);
                self.sig[i].store(sig, Release);
                return true;
            }
        }
        self.lost.fetch_add(1, Relaxed);
        false
    }

    /// Saca la instancia mas antigua de `sig`, si hay.
    pub fn pop(&self, sig: u32) -> Option<[u8; 128]> {
        use std::sync::atomic::Ordering::*;
        let mut best: Option<(u64, usize)> = None;
        for i in 0..RTQ_CAP {
            if self.sig[i].load(Acquire) == sig {
                let s = self.seq[i].load(Relaxed);
                if best.is_none_or(|(b, _)| s < b) {
                    best = Some((s, i));
                }
            }
        }
        let (_, i) = best?;
        let info = unsafe { *self.info[i].get() };
        self.sig[i].store(0, Release);
        Some(info)
    }
}

pub const fn sbit(s: u32) -> u64 {
    1u64 << (s - 1)
}

/// Senales que reclama ART con libsigchain: SIGSEGV (comprobaciones implicitas) y SIGBUS (recolector con
/// userfaultfd, Android 14+; tambien la reclama el native bridge en el host). libsigchain no deja bloquearlas desde
/// sigprocmask/pthread_sigmask.
pub const CLAIMED: u64 = sbit(11) | sbit(7);
/// Lo que el puente nunca bloquea en el nucleo (ver arriba): si el guest la bloquea, solo en `sig_view`.
pub const KEEP_UNBLOCKED: u64 = sbit(11);
/// SIGKILL y SIGSTOP: el nucleo nunca las bloquea.
const UNBLOCKABLE: u64 = sbit(9) | sbit(19);
const EINVAL: i64 = 22;

/// SIGRTMIN que ve la app (`__libc_current_sigrtmin`): en Android, el de la bionic del host (misma version que la del
/// dispositivo: las reservadas son __SIGRTMIN+1..SIGRTMIN-1); fuera de Android, 35.
pub fn rtmin() -> u32 {
    #[cfg(target_os = "android")]
    {
        extern "C" {
            fn __libc_current_sigrtmin() -> i32;
        }
        let v = unsafe { __libc_current_sigrtmin() };
        if (33..=64).contains(&v) {
            return v as u32;
        }
    }
    35
}

/// `filter_reserved_signals` de bionic (solo Android; fuera, la libc del host no es bionic y no se filtra).
fn bionic_filter(set: u64, how: u64) -> u64 {
    if !cfg!(target_os = "android") {
        return set;
    }
    let reserved: u64 = (33..rtmin()).fold(0, |m, s| m | sbit(s));
    let timer = sbit(32);
    if how == 1 {
        (set & !timer) | reserved
    } else {
        (set | timer) & !reserved
    }
}

/// Mascara del nucleo del hilo actual.
pub fn kernel_mask() -> u64 {
    let mut m = 0u64;
    unsafe { syscall(14, 0i64, 0usize, &mut m as *mut u64, 8usize) };
    m
}

/// Bits de `KEEP_UNBLOCKED` que el guest tiene "bloqueados" (ver arriba).
fn view() -> u64 {
    rt::cur_opt().map_or(0, |t| t.sig_view & KEEP_UNBLOCKED)
}

/// Mascara que ve el guest: la del nucleo mas las que el puente no bloquea pero el guest si.
pub fn guest_mask() -> u64 {
    kernel_mask() | view()
}

/// Mascara del nucleo que corresponde a la mascara `m` que veria el guest.
fn kernel_part(m: u64) -> u64 {
    m & !KEEP_UNBLOCKED
}

/// Pone la mascara `m` sin filtrar (como el nucleo: sigreturn, mascara de un manejador).
pub fn set_mask_raw(m: u64) {
    let k = kernel_part(m);
    unsafe { syscall(14, 2i64, &k as *const u64, 0usize, 8usize) };
    if let Some(t) = rt::cur_opt() {
        t.sig_view = m & KEEP_UNBLOCKED;
    }
}

/// `rt_sigprocmask` sobre la mascara del guest con el conjunto ya filtrado. Devuelve 0 o -errno.
fn mask_op(how: u64, set: Option<u64>, old: Option<&mut u64>) -> i64 {
    let t = rt::cur_opt();
    let v0 = t.as_ref().map_or(0, |t| t.sig_view & KEEP_UNBLOCKED);
    let mut k0 = 0u64;
    let r = match set {
        Some(s) => {
            if how > 2 {
                return -EINVAL;
            }
            let s = s & !UNBLOCKABLE;
            let ks = kernel_part(s);
            let r = unsafe { syscall(14, how as i64, &ks as *const u64, &mut k0 as *mut u64, 8usize) } as i64;
            if r == 0 {
                if let Some(t) = t {
                    t.sig_view = match how {
                        0 => v0 | (s & KEEP_UNBLOCKED),
                        1 => v0 & !s,
                        _ => s & KEEP_UNBLOCKED,
                    };
                }
            }
            r
        }
        None => {
            // sin conjunto nuevo el nucleo no comprueba `how`
            let _ = how;
            unsafe { syscall(14, 0i64, 0usize, &mut k0 as *mut u64, 8usize) as i64 }
        }
    };
    if r < 0 {
        return -(errno() as i64);
    }
    if let Some(o) = old {
        *o = k0 | v0;
    }
    0
}

/// `pthread_sigmask`/`sigprocmask` del guest (libc: libsigchain + bionic, ver arriba). `how`: SIG_BLOCK (0),
/// SIG_UNBLOCK (1), SIG_SETMASK (2). Devuelve 0 o -errno. Con `how` invalido y conjunto nuevo, EINVAL; sin conjunto
/// nuevo `how` no se comprueba (como el nucleo y bionic).
pub fn guest_sigprocmask(how: u64, set: Option<u64>, old: Option<&mut u64>) -> i64 {
    let set = set.map(|s| {
        // libsigchain: no deja bloquear lo que reclama ART (solo con SIG_BLOCK/SIG_SETMASK)
        let s = if how == 1 { s } else { s & !CLAIMED };
        bionic_filter(s, how)
    });
    if set.is_none() {
        // bionic solo valida `how` si hay conjunto nuevo
        return mask_op(0, None, old);
    }
    mask_op(how, set, old)
}

/// `rt_sigprocmask` directo del guest (`svc`): sin filtrar, como el nucleo (SIGKILL/SIGSTOP se ignoran).
pub fn guest_rt_sigprocmask(how: u64, set: Option<u64>, old: Option<&mut u64>) -> i64 {
    mask_op(how, set, old)
}

/// La senal `sig` esta bloqueada para el guest: un fallo sincrono de esa senal terminaria el proceso en el nucleo.
pub fn guest_blocks(sig: i32) -> bool {
    (1..=64).contains(&sig) && guest_mask() & sbit(sig as u32) != 0
}

// ---------------------------------------------------------------------------------------------
// Entrega de senales al guest
// ---------------------------------------------------------------------------------------------
//
// Un manejador guest se ejecuta con el interprete sobre una Cpu reservada para su nivel (`GuestThread::sig_cpus`),
// dentro del propio manejador del host: ni se traduce ni se reserva memoria ni se toman bloqueos del puente.
//
// Reanudar tras un fallo sincrono (SIGSEGV/SIGBUS/SIGILL/SIGFPE de codigo guest; ver `sync_fault`):
//  * El guest ve el pc EXACTO de la instruccion que fallo (tabla codigo x86 -> pc de cada bloque, `Jit::fault_pc`; en
//    el interprete, `cpu.pc`) y un ucontext aarch64 escribible.
//  * Si el manejador vuelve, su ucontext (registros, sp, pc, NZCV, FPSR/FPCR, v0-v31 y uc_sigmask) pasa a la Cpu
//    interrumpida; si sale con siglongjmp (su sp queda fuera de la pila del manejador), pasa el contexto del salto.
//  * La Cpu interrumpida esta en un bloque traducido (cuyo prologo apunto `cpu.host_sp`) o dentro de
//    `jit::heddle_call_block` (bucle del interprete): el manejador del host vuelve con rsp = `cpu.host_sp` y rip =
//    `heddle_jit_abort` o `heddle_block_abort`; el nucleo restaura ese estado (y la mascara) y la ejecucion sigue en
//    el pc nuevo. Es lo que hace el nucleo con un manejador ARM real.
//  * Solo se abandona codigo guest o el interprete: nunca codigo del puente con un bloqueo del monitor tomado
//    (`monitor::in_critical`), a medias de preparar otra senal (`sig_setup`) o dentro de una HLE/llamada al sistema.
//    Un fallo dentro de una HLE es codigo del host (memcpy con un puntero malo, o ART: una comprobacion implicita en
//    Java llamado por JNI): no llega al manejador guest y sigue la cadena de sigchain (ART, debuggerd).
// Las senales asincronas con el hilo en codigo guest se entregan en una frontera de instruccion, fuera del manejador
// del host (ver "Senales asincronas" mas abajo); solo las que llegan en una HLE se entregan en el acto.

/// Estado de un nivel de senal en curso.
#[derive(Clone, Copy)]
pub struct SigLevel {
    /// pila del manejador [lo, hi): un siglongjmp a un sp fuera de ella sale del manejador
    lo: u64,
    hi: u64,
    /// se vigila la salida con siglongjmp (fallo sincrono, o asincrona que interrumpio codigo guest)
    catch: bool,
    /// el manejador salio con siglongjmp: su Cpu tiene el contexto de destino
    escape: bool,
}

impl SigLevel {
    pub const EMPTY: SigLevel = SigLevel { lo: 0, hi: 0, catch: false, escape: false };
}

/// Lo llama longjmp/siglongjmp (libc_hle) tras restaurar el contexto en `c`: si `c` es la Cpu del manejador en curso
/// y el salto sale de su pila, el manejador termina y quien entrego la senal aplica el contexto.
pub fn note_longjmp(c: &Cpu) {
    let Some(t) = rt::cur_opt() else { return };
    let d = t.sig_depth as usize;
    if d == 0 || !std::ptr::eq(c as *const Cpu, t.sig_cpu) {
        return;
    }
    let l = &mut t.sig_lvl[d - 1];
    let sp = c.x[31];
    if l.catch && !(sp >= l.lo && sp < l.hi) {
        l.escape = true;
    }
}

/// `c` es la Cpu de un manejador que acaba de salir con siglongjmp (el bucle de ejecucion debe terminar).
pub fn escaping(c: &Cpu) -> bool {
    let Some(t) = rt::cur_opt() else { return false };
    let d = t.sig_depth as usize;
    d > 0 && std::ptr::eq(c as *const Cpu, t.sig_cpu) && t.sig_lvl[d - 1].escape
}

struct Delivered {
    uc: u64,
    escaped: bool,
    level: usize,
}

/// Construye el marco y ejecuta el manejador guest de `sig` sobre la Cpu `cur` (el contexto interrumpido, que no se
/// modifica). `imask`: mascara del nucleo en el punto interrumpido. `sync`: fallo sincrono (la mascara del manejador
/// la pone el puente; en una asincrona ya la puso el nucleo). `catch`: vigilar la salida con siglongjmp.
/// Sin reservas ni bloqueos. None si se supera MAX_SIG_DEPTH.
///
/// # Safety
/// `cur` es una Cpu valida de este hilo; `info` es nulo o un `siginfo` de 128 bytes.
unsafe fn deliver(tp: *mut rt::GuestThread, sig: i32, info: *const u8, act: GuestAct, cur: *const Cpu, imask: u64, sync: bool, catch: bool) -> Option<Delivered> {
    // `tp` es un puntero y no `&mut`: el manejador modifica el estado del hilo (sig_lvl, sig_view) mientras corre, y
    // con un `&mut` como parametro el compilador podria dar por hecho que nada lo cambia durante `run_loop`.
    let t = unsafe { &mut *tp };
    let d = t.sig_depth as usize;
    if d >= MAX_SIG_DEPTH {
        return None; // demasiadas anidadas: quien entrega la aplaza (asincrona) o termina (fallo sincrono)
    }
    if act.flags & SA_RESETHAND != 0 {
        act_swap(sig, GuestAct::default()); // el nucleo ya lo hizo con su accion; la tabla guest tambien
    }
    t.sig_depth += 1;
    let cur = unsafe { &*cur };
    let alt = crate::libc_hle::altstack();
    let fb = t.sig_stack_lo + rt::SIG_STACK as u64 - 64 - (d as u64) * (rt::SIG_STACK as u64 / MAX_SIG_DEPTH as u64);
    let f = unsafe { build_frame(cur, (t.stack_lo, t.stack_hi), alt, act.flags & SA_ONSTACK != 0, fb, info) };
    // uc_sigmask: lo que el guest tenia bloqueado al interrumpirse
    let view0 = t.sig_view;
    let gmask = imask | (view0 & KEEP_UNBLOCKED);
    unsafe { *((f.uc + UC_SIGMASK) as *mut u64) = gmask };
    let hmask = gmask | act.mask | if act.flags & SA_NODEFER != 0 { 0 } else { sbit(sig as u32) };
    if sync {
        set_mask_raw(hmask);
    } else {
        t.sig_view = hmask & KEEP_UNBLOCKED;
    }
    let c: *mut Cpu = &mut *t.sig_cpus[d];
    let c = unsafe { &mut *c };
    c.reset();
    c.tpidr = t.tls_base;
    c.fpcr = cur.fpcr;
    c.ssbs_x = cur.ssbs_x; // PSTATE.SSBS no cambia al entrar en el manejador
    c.monflag = t.cpu.monflag; // la marca de store del monitor es del hilo, no de la Cpu
    c.x[31] = f.sp;
    c.x[0] = sig as u64;
    c.x[1] = f.info;
    c.x[2] = f.uc;
    let rs = rt::ret_slot();
    c.x[30] = rs;
    c.pc = act.handler;
    // pila del manejador: la del marco (alternativa del guest, la del hilo o la de reserva)
    let lo = if alt.0 != 0 && f.sp > alt.0 && f.sp <= alt.0.wrapping_add(alt.2) {
        alt.0
    } else if f.sp > t.stack_lo && f.sp <= t.stack_hi {
        t.stack_lo
    } else {
        t.sig_stack_lo
    };
    t.sig_lvl[d] = SigLevel { lo, hi: f.sp, catch, escape: false };
    let prev = t.sig_cpu;
    t.sig_cpu = c;
    let setup = t.sig_setup;
    t.sig_setup = false;
    rt::run_loop(c, rs);
    let t = unsafe { &mut *tp };
    t.sig_setup = setup;
    let escaped = t.sig_lvl[d].escape;
    t.sig_lvl[d] = SigLevel::EMPTY;
    t.sig_cpu = prev;
    t.sig_depth -= 1;
    if !escaped {
        t.sig_view = view0; // al volver se restaura la mascara del punto interrumpido (o la del ucontext, abajo)
    }
    Some(Delivered { uc: f.uc, escaped, level: d })
}

/// Copia en `dst` el contexto que el guest dejo en su ucontext aarch64 al volver del manejador.
///
/// # Safety
/// `uc` es el marco construido por `build_frame`.
unsafe fn apply_uc(dst: &mut Cpu, uc: u64) {
    let r = |o: u64| unsafe { *((uc + o) as *const u64) };
    let m = UC_MCONTEXT;
    for i in 0..31 {
        dst.x[i] = r(m + 8 + 8 * i as u64);
    }
    dst.x[31] = r(m + 256);
    dst.pc = r(m + 264);
    dst.set_flags(r(m + 272));
    // como el kernel al volver (valid_user_regs): SSBS se conserva si el procesador lo tiene
    if crate::feat::need(crate::feat::F_SSBS) {
        dst.set_ssbs(r(m + 272));
    }
    let fp = uc + UC_RESERVED;
    if unsafe { *(fp as *const u32) } == 0x4650_8001 {
        unsafe {
            // como el kernel al escribir FPSR/FPCR: los bits RAZ/WI no se guardan
            dst.fpsr = *((fp + 8) as *const u32) as u64 & crate::interp::FPSR_RW;
            dst.fpcr = *((fp + 12) as *const u32) as u64 & crate::interp::FPCR_RW;
            for i in 0..32 {
                dst.v[i] = *((fp + 16 + 16 * i as u64) as *const [u64; 2]);
            }
        }
    }
    dst.mon.valid = false; // la excepcion borra el monitor exclusivo local, como en ARM
}

/// Contexto de un siglongjmp que salio del manejador (Cpu del manejador) -> Cpu interrumpida.
fn copy_ctx(dst: &mut Cpu, src: &Cpu) {
    dst.x = src.x;
    dst.pc = src.pc;
    dst.set_flags(src.flags());
    dst.v = src.v;
    dst.fpsr = src.fpsr;
    dst.fpcr = src.fpcr;
    dst.mon.valid = false;
}

/// De donde viene un fallo (o donde estaba el hilo al llegar una senal).
enum Origin {
    /// codigo guest de la Cpu interrumpida (bloque traducido o interprete), con el pc exacto: se puede reanudar
    Guest(u64),
    /// dentro de una HLE o una llamada al sistema del guest
    Hle,
    /// codigo del propio puente
    Bridge,
}

// ucontext_t de x86-64: mcontext.gregs en +40 (REG_RBX = 11, REG_RSP = 15, REG_RIP = 16), uc_sigmask en +296
const X_RBX: usize = 11;
const X_RSP: usize = 15;
const X_RIP: usize = 16;
const X_SIGMASK: usize = 296;

unsafe fn greg(uc: *mut c_void, r: usize) -> *mut u64 {
    unsafe { (uc as *mut u8).add(40 + 8 * r) as *mut u64 }
}

/// # Safety
/// `uc` es el ucontext que el nucleo entrego al manejador; `cur` la Cpu del nivel `d` de este hilo.
unsafe fn fault_origin(t: &rt::GuestThread, cur: &Cpu, d: usize, uc: *mut c_void) -> Origin {
    if t.sig_setup || crate::monitor::in_critical() {
        return Origin::Bridge;
    }
    if t.phase != 0 {
        return Origin::Hle;
    }
    let (rip, rsp) = unsafe { (*greg(uc, X_RIP), *greg(uc, X_RSP)) };
    let hsp = cur.host_sp;
    // dentro de un bloque traducido (d == 0: host_sp es el RSP de su cuerpo, que el bloque no mueve; un helper esta
    // por debajo) o de heddle_call_block (interprete: host_sp es el RSP antes de la llamada) de esta Cpu
    let above = if d == 0 { rsp > hsp } else { rsp >= hsp };
    if hsp == 0 || above || hsp - rsp > (16 << 20) {
        return Origin::Bridge;
    }
    if d == 0 {
        // Cpu principal: JIT
        match unsafe { t.jit.fault_pc(hsp, rip) } {
            Some(pc) => Origin::Guest(pc),
            None => Origin::Bridge,
        }
    } else {
        // manejadores: interprete, cuyo cpu.pc es el de la instruccion en curso
        Origin::Guest(cur.pc)
    }
}

/// Prepara `uc` para que, al volver del manejador del host, la Cpu `cur` siga en su contexto (ya escrito en ella).
/// `jit`: la Cpu esta en un bloque traducido (`heddle_jit_abort` ejecuta su epilogo desde `host_sp`, que repone los
/// callee-saved); si no, en el bucle del interprete (`heddle_call_block`, que los guarda el mismo).
unsafe fn resume_at(uc: *mut c_void, cur: *mut Cpu, mask: u64, jit: bool) {
    unsafe {
        let hsp = (*cur).host_sp;
        if jit {
            *greg(uc, X_RIP) = (crate::jit::heddle_jit_abort as unsafe extern "C" fn()) as usize as u64;
        } else {
            *greg(uc, X_RIP) = (crate::jit::heddle_block_abort as unsafe extern "C" fn()) as usize as u64;
            *greg(uc, X_RBX) = cur as u64;
        }
        *greg(uc, X_RSP) = hsp;
        *((uc as *mut u8).add(X_SIGMASK) as *mut u64) = mask;
    }
}

/// Fallo sincrono (SIGSEGV/SIGBUS/SIGILL/SIGFPE) que el nucleo entrego al puente (`fault_host`, `async_host_handler`)
/// o que sigchain paso al native bridge (`native_bridge_signal`). Ejecuta el manejador guest si el fallo es del guest y
/// devuelve true si la ejecucion debe seguir con el contexto que dejo el guest (ya preparado en `uc`). false: el fallo
/// sigue su curso (lo gestiona ART o termina el proceso).
///
/// # Safety
/// `info` y `uc` son nulos o los `siginfo_t`/`ucontext_t` que el nucleo entrega al manejador.
pub unsafe fn sync_fault(sig: i32, info: *mut u8, uc: *mut c_void) -> bool {
    if uc.is_null() || info.is_null() {
        return false;
    }
    // acceso del monitor con un bloqueo de granulo tomado: sigue en su continuacion, que suelta el bloqueo y repite
    // el acceso sin el (ese fallo si llega aqui como fallo del guest; ver monitor::fault_in_locked)
    if unsafe { crate::monitor::fault_in_locked(uc) } {
        return true;
    }
    // acceso de una ruta rapida TLSDESC en linea: sigue en su ruta lenta, el resolutor traducido, que repite el acceso
    // (si vuelve a fallar, llega aqui como fallo del guest con el pc exacto; ver jit.rs, "TLSDESC en linea")
    if unsafe { crate::jit::fault_in_fused(uc) } {
        return true;
    }
    let act = guest_act(sig);
    if act.handler <= 1 {
        return false;
    }
    let Some(t) = rt::cur_opt() else { return false };
    let d = t.sig_depth as usize;
    if d >= MAX_SIG_DEPTH {
        return false;
    }
    // fallo en el mov de un store rapido: sin marca y reiniciando el store (ver monitor::fault_in_store)
    unsafe { crate::monitor::fault_in_store(uc) };
    let cur: *mut Cpu = if t.sig_cpu.is_null() { &mut *t.cpu } else { t.sig_cpu };
    // Solo los fallos del codigo guest llegan a su manejador. Uno dentro de una HLE o una llamada al sistema es
    // codigo del host (memcpy con un puntero malo, o ART: una comprobacion implicita en Java llamado por JNI): sigue
    // la cadena de sigchain (ART, debuggerd). Uno del propio puente tambien.
    let Origin::Guest(pc) = (unsafe { fault_origin(t, &*cur, d, uc) }) else { return false };
    let imask = unsafe { *((uc as *const u8).add(X_SIGMASK) as *const u64) };
    if (imask | (t.sig_view & KEEP_UNBLOCKED)) & sbit(sig as u32) != 0 {
        // el guest la tiene bloqueada (solo puede ser por `sig_view`: el nucleo ya habria terminado el proceso con
        // las demas): el nucleo la forzaria con SIG_DFL. Sigue la cadena (sin manejador guest) y termina.
        return false;
    }
    unsafe { (*cur).pc = pc };
    let setup = t.sig_setup;
    t.sig_setup = true;
    let r = unsafe { deliver(t, sig, info, act, cur, imask, true, true) };
    let ok = match r {
        None => false,
        Some(dl) if dl.escaped => {
            copy_ctx(unsafe { &mut *cur }, &t.sig_cpus[dl.level]);
            unsafe { resume_at(uc, cur, kernel_mask(), d == 0) };
            true
        }
        Some(dl) => {
            let gm = unsafe { *((dl.uc + UC_SIGMASK) as *const u64) };
            unsafe { apply_uc(&mut *cur, dl.uc) };
            t.sig_view = gm & KEEP_UNBLOCKED;
            unsafe { resume_at(uc, cur, kernel_part(gm), d == 0) };
            true
        }
    };
    t.sig_setup = setup;
    ok
}

/// Punto de entrada de las senales con manejador guest. Si el hilo tiene un bloqueo del monitor tomado, el
/// manejador guest no se ejecuta ahora (podria dormir con el bloqueo retenido y colgar a los demas hilos): la senal
/// se reenvia al hilo cuando suelte el bloqueo (ver monitor::defer_signal). Los fallos sincronos del nucleo
/// (SEGV, SIGILL...) no se pueden diferir: la instruccion volveria a fallar.
extern "C" fn async_host_handler(sig: i32, info: *mut u8, uc: *mut c_void) {
    let sync_fault_ = matches!(sig, 4 | 7 | 8 | 11) && !info.is_null() && unsafe { *(info.add(8) as *const i32) } > 0;
    if sync_fault_ {
        if unsafe { try_exec_redirect(sig, info, uc) || sync_fault(sig, info, uc) } {
            return;
        }
        fatal_fault(sig, info);
        return;
    }
    if unsafe { crate::monitor::defer_signal(sig, info) } {
        return;
    }
    if guest_act(sig).handler > 1 {
        if rt::cur_opt().is_none() {
            // hilo sin estado guest (un hilo del host): con un estado de emergencia, o espera a que se suelte uno
            match rt::emergency_enter() {
                Some(i) => {
                    unsafe { async_deliver(sig, info, uc) };
                    rt::emergency_leave(i);
                }
                None => unsafe { rt::emergency_defer(sig, info) },
            }
            return;
        }
        if let Some(t) = rt::cur_opt() {
            let (phase, runs) = unsafe { (std::ptr::read_volatile(&t.phase), std::ptr::read_volatile(&t.runs)) };
            if phase == 0 && runs > 0 {
                // codigo guest (o el bucle que lo ejecuta): a la siguiente frontera de instruccion
                unsafe { pend(t, sig, info) };
                return;
            }
            if phase == 2 && !uc.is_null() {
                let rip = unsafe { greg(uc, X_RIP) };
                let r = unsafe { *rip };
                if r >= crate::syscall::sc_start() && r <= crate::syscall::heddle_sc_insn as usize as u64 {
                    // llamada al sistema del guest sin ejecutar (o rebobinada para reiniciarla): se abandona y el
                    // `svc` se repite tras el manejador
                    unsafe { *rip = crate::syscall::heddle_sc_abort as usize as u64 };
                    unsafe { pend(t, sig, info) };
                    return;
                }
                if r == crate::syscall::heddle_sc_done as usize as u64 {
                    unsafe { pend(t, sig, info) }; // ya termino (resultado o -EINTR): despues del `svc`
                    return;
                }
            }
        }
    }
    unsafe { async_deliver(sig, info, uc) };
}

// ---------------------------------------------------------------------------------------------
// Senales asincronas: entrega en frontera de instruccion
// ---------------------------------------------------------------------------------------------
//
// Una senal asincrona que llega con el hilo en codigo guest (fase 0 dentro de `rt::run_loop`: bloque traducido,
// interprete o el propio bucle) no ejecuta el manejador guest dentro del manejador del host: este solo la anota en
// `GuestThread::sig_pend`/`sig_pinfo` (sin reservas ni bloqueos) y envenena `cpu.aux[5]` del Cpu principal
// (`jit::SIG_POISON`). El codigo traducido sale en el siguiente salto hacia atras (la comprobacion de EPOCH que ya
// tenia) o al terminar el bloque; `run_inner` devuelve `Event::Signal`; el interprete mira `sig_pend` antes de cada
// instruccion. `deliver_pending` entrega entonces la senal en contexto normal, como el nucleo ARM al volver a modo
// usuario: pc exacto (una frontera de instruccion), todos los registros, ucontext ESCRIBIBLE (lo que el manejador
// cambie se aplica, uc_sigmask incluido) y siglongjmp fuera del manejador.
//
// Antes de una HLE o un `svc` el bucle marca la fase y despues mira `sig_pend`: lo aplazado antes se entrega antes de
// la llamada (pc en la funcion o en el `svc`, que se repite despues). Dentro de una HLE (fase 1) la senal se sigue
// entregando en el acto, dentro del manejador del host (codigo del host que puede bloquearse esperandola, p. ej.
// pthread_cond_wait o el GC de ART), con el pc del slot; si el host esta en un punto seguro (justo tras un `syscall`),
// el ucontext es escribible y un siglongjmp se aplica abandonando la llamada al host (`abort_hle`); si no, solo
// lectura. Dentro de un `svc` (fase 2) la
// llamada va por `syscall::heddle_sc`: si no se ejecuto aun o el nucleo la rebobino para reiniciarla (SA_RESTART del
// guest, que es el del host), se abandona y el `svc` se repite tras el manejador; si termino (p. ej. -EINTR sin
// SA_RESTART), la senal se entrega despues con ese resultado en x0. Es lo que ve una app en ARM. Las llamadas que
// cambian la mascara mientras esperan (rt_sigsuspend, ppoll, pselect6, epoll_pwait) entregan en el acto.
//
// Las senales iguales aplazadas se funden en una, con el primer siginfo (como las estandar del nucleo; las de tiempo
// real el nucleo las encolaria). Si al entregarla el guest la tiene bloqueada o ya no tiene manejador, se devuelve al
// nucleo con rt_tgsigqueueinfo (queda pendiente ahi, con el mismo siginfo).

/// Anota la senal `sig` para la siguiente frontera de instruccion (manejador del host; sin reservas ni bloqueos).
///
/// # Safety
/// `info` es nulo o un `siginfo` de 128 bytes; `t` es el estado del hilo actual.
unsafe fn pend(t: &mut rt::GuestThread, sig: i32, info: *const u8) {
    use std::sync::atomic::Ordering::Relaxed;
    let bit = sbit(sig as u32);
    if t.sig_pend.load(Relaxed) & bit != 0 && sig as u32 >= rtmin() {
        // otra instancia de una senal de tiempo real ya aplazada: a la cola, con su siginfo
        let mut tmp = [0u8; 128];
        if info.is_null() {
            tmp[0..4].copy_from_slice(&sig.to_ne_bytes());
        } else {
            unsafe { std::ptr::copy_nonoverlapping(info, tmp.as_mut_ptr(), 128) };
        }
        t.sig_rtq.push(sig as u32, &tmp);
    } else if t.sig_pend.load(Relaxed) & bit == 0 {
        let dst = &mut t.sig_pinfo[sig as usize - 1];
        if info.is_null() {
            *dst = [0; 128];
            dst[0..4].copy_from_slice(&sig.to_ne_bytes());
        } else {
            unsafe { std::ptr::copy_nonoverlapping(info, dst.as_mut_ptr(), 128) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
    t.sig_pend.fetch_or(bit, Relaxed);
    // el Cpu principal sale en el siguiente salto hacia atras (aunque ahora corra un manejador: al volver a el)
    unsafe { std::ptr::write_volatile(&mut t.cpu.aux[5], crate::jit::SIG_POISON) };
}

/// Devuelve `sig` al nucleo (pendiente en el hilo, con su siginfo).
fn requeue(sig: u32, info: &[u8; 128]) {
    unsafe {
        let (pid, tid) = (syscall(39), syscall(186));
        if syscall(297, pid, tid, sig as i64, info.as_ptr()) < 0 {
            syscall(234, pid, tid, sig as i64);
        }
    }
}

/// Entrega las senales aplazadas de este hilo a la Cpu `c`, que esta en una frontera de instruccion (ver arriba).
pub fn deliver_pending(c: &mut Cpu) {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(t) = rt::cur_opt() else { return };
    let t: *mut rt::GuestThread = t;
    loop {
        let p = unsafe { (*t).sig_pend.load(Relaxed) };
        if p == 0 {
            return;
        }
        let sig = p.trailing_zeros() + 1;
        let bit = sbit(sig);
        // primero el siginfo y despues el bit: el manejador solo escribe el siginfo con el bit a 0
        let info = unsafe { (*t).sig_pinfo[sig as usize - 1] };
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        unsafe { (*t).sig_pend.fetch_and(!bit, Relaxed) };
        // la siguiente instancia de la misma senal de tiempo real (si la hay) pasa a ser la aplazada
        if sig >= rtmin() {
            if let Some(next) = unsafe { (*t).sig_rtq.pop(sig) } {
                unsafe { (*t).sig_pinfo[sig as usize - 1] = next };
                std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
                unsafe { (*t).sig_pend.fetch_or(bit, Relaxed) };
            }
        }
        let act = guest_act(sig as i32);
        if act.handler <= 1 || guest_mask() & bit != 0 {
            requeue(sig, &info);
            continue;
        }
        if !unsafe { deliver_at(t, c, sig as i32, &info, act) } {
            // todos los niveles ocupados: vuelve a quedar aplazada y se entrega cuando vuelva un manejador
            unsafe { pend(&mut *t, sig as i32, info.as_ptr()) };
            return;
        }
        if escaping(c) {
            return; // un siglongjmp sale tambien del manejador que interrumpio: lo resuelve quien lo entrego
        }
    }
}

/// Ejecuta el manejador guest de `sig` sobre la Cpu `c` (en una frontera de instruccion, fuera de un manejador del
/// host o dentro de uno pero sin abandonarlo) y aplica a `c` el contexto con que vuelve (ucontext) o el del
/// siglongjmp con que sale. false si se supera MAX_SIG_DEPTH.
///
/// # Safety
/// `t` es el estado del hilo actual.
unsafe fn deliver_at(t: *mut rt::GuestThread, c: &mut Cpu, sig: i32, info: &[u8; 128], act: GuestAct) -> bool {
    let tt = unsafe { &mut *t };
    let setup = tt.sig_setup;
    tt.sig_setup = true;
    let cp: *mut Cpu = c;
    let r = unsafe { deliver(t, sig, info.as_ptr(), act, cp, kernel_mask(), true, true) };
    let tt = unsafe { &mut *t };
    let ok = r.is_some();
    if let Some(dl) = r {
        if dl.escaped {
            copy_ctx(c, &tt.sig_cpus[dl.level]); // la mascara ya la dejo el siglongjmp
            note_longjmp(c); // ...que puede salir tambien del manejador en curso
        } else {
            unsafe { apply_uc(c, dl.uc) };
            let gm = unsafe { *((dl.uc + UC_SIGMASK) as *const u64) };
            set_mask_raw(gm);
        }
    }
    tt.sig_setup = setup;
    ok
}

/// El host esta justo tras una instruccion `syscall` (0f 05): bloqueado en el nucleo o recien vuelto de el.
///
/// # Safety
/// `uc` es el `ucontext_t` del nucleo.
unsafe fn at_syscall(uc: *mut c_void) -> bool {
    let rip = unsafe { *greg(uc, X_RIP) };
    rip >= 2 && crate::boundary::is_host_code_sig(rip) && unsafe { *((rip - 2) as *const [u8; 2]) } == [0x0f, 0x05]
}

/// El manejador cambio el contexto guest del marco `uc` respecto de lo interrumpido (x0-x30, sp, pc, NZCV).
///
/// # Safety
/// `uc` es el ucontext guest del marco.
unsafe fn uc_changed(uc: u64, x: &[u64; 32], pc: u64, fl: u64) -> bool {
    let r = |o: u64| unsafe { *((uc + o) as *const u64) };
    let m = UC_MCONTEXT;
    (0..31).any(|i| r(m + 8 + 8 * i as u64) != x[i]) || r(m + 256) != x[31] || r(m + 264) != pc || r(m + 272) & 0xF000_0000 != fl & 0xF000_0000
}

/// Vuelve del manejador del host a `heddle_hle_abort` con la pila de `heddle_hle_call` y la mascara `mask`.
///
/// # Safety
/// `uc` es el `ucontext_t` del nucleo; `t.hle_sp` es el de la llamada en curso.
unsafe fn abort_hle(uc: *mut c_void, t: &mut rt::GuestThread, mask: u64) {
    unsafe {
        *greg(uc, X_RIP) = rt::heddle_hle_abort as usize as u64;
        *greg(uc, X_RSP) = t.hle_sp;
        *greg(uc, X_RBX) = &mut t.hle_sp as *mut u64 as u64;
        *((uc as *mut u8).add(X_SIGMASK) as *mut u64) = mask;
    }
}

/// Senal asincrona que no se puede aplazar (hilo en una HLE, en una llamada al sistema fuera de `heddle_sc` o fuera
/// de `run_loop`): ejecuta el manejador guest en el acto sobre la pila del hilo interrumpido. El contexto es de solo
/// lectura, salvo un siglongjmp que salga del manejador cuando lo interrumpido era codigo traducido, o una HLE
/// abandonable en un punto seguro: ahi se aplican el siglongjmp o el contexto cambiado.
unsafe fn async_deliver(sig: i32, info: *mut u8, uc: *mut c_void) {
    let act = guest_act(sig);
    if act.handler <= 1 {
        return;
    }
    // En contexto de senal no se crea estado de hilo (reservaria memoria y tomaria bloqueos): si este hilo nunca
    // ejecuto codigo guest, la senal no es para el guest.
    let Some(t) = rt::cur_opt() else { return };
    let d = t.sig_depth as usize;
    if d >= MAX_SIG_DEPTH {
        // todos los niveles ocupados: se aplaza a la siguiente frontera de instruccion (al volver un manejador)
        unsafe { pend(t, sig, info) };
        return;
    }
    let cur: *mut Cpu = if t.sig_cpu.is_null() { &mut *t.cpu } else { t.sig_cpu };
    // Solo se puede abandonar (siglongjmp) un punto de codigo traducido: en un helper, en el bucle del interprete o en
    // el puente la senal puede haber llegado con un bloqueo de lectura o a mitad de una estructura (un fallo sincrono,
    // en cambio, solo ocurre en el acceso a memoria guest, fuera de esos casos).
    let in_code = !uc.is_null() && d == 0 && t.jit.code_contains(unsafe { *greg(uc, X_RIP) });
    let origin = if in_code { unsafe { fault_origin(t, &*cur, d, uc) } } else { Origin::Bridge };
    // dentro de una llamada HLE abandonable, con el host en un punto seguro (justo tras una instruccion `syscall`:
    // bloqueado en el nucleo o recien salido, como un `svc` en ARM) y sin guardas que repongan memoria del guest, el
    // manejador puede salir con siglongjmp o cambiar su contexto: se abandona la llamada al host (`heddle_hle_abort`)
    let hle_safe = !in_code && !uc.is_null() && t.phase == 1 && t.hle_sp != 0 && !crate::boundary::abort_blocked() && unsafe { at_syscall(uc) };
    let resumable = matches!(origin, Origin::Guest(_)) || hle_safe;
    // el marco lleva el pc exacto de la instruccion interrumpida; el bloque traducido sigue con el suyo al volver
    let pc0 = unsafe { (*cur).pc };
    if let Origin::Guest(pc) = origin {
        unsafe { (*cur).pc = pc };
    }
    let setup = t.sig_setup;
    t.sig_setup = true;
    let imask = if uc.is_null() { kernel_mask() } else { unsafe { *((uc as *const u8).add(X_SIGMASK) as *const u64) } };
    let before = unsafe { (*cur).x };
    let (pcb, flb) = unsafe { ((*cur).pc, (*cur).flags()) };
    match unsafe { deliver(t, sig, info, act, cur, imask, false, resumable) } {
        Some(dl) if dl.escaped && in_code => {
            copy_ctx(unsafe { &mut *cur }, &t.sig_cpus[dl.level]);
            unsafe { resume_at(uc, cur, kernel_mask(), true) };
        }
        Some(dl) if dl.escaped && hle_safe => {
            // siglongjmp fuera del manejador: el guest sigue en el destino; la llamada al host se abandona
            copy_ctx(unsafe { &mut *cur }, &t.sig_cpus[dl.level]);
            unsafe { abort_hle(uc, t, kernel_mask()) };
        }
        Some(dl) if hle_safe && unsafe { uc_changed(dl.uc, &before, pcb, flb) } => {
            // el manejador cambio el contexto: se aplica y la llamada se abandona (con el pc en la funcion, se repite
            // entera con los registros nuevos, como un `svc` reiniciado)
            unsafe { apply_uc(&mut *cur, dl.uc) };
            let gm = unsafe { *((dl.uc + UC_SIGMASK) as *const u64) };
            unsafe { abort_hle(uc, t, gm) };
        }
        _ => unsafe { (*cur).pc = pc0 },
    }
    t.sig_setup = setup;
}

/// Fallo sincrono detectado por el bucle de ejecucion (BRK, instruccion indefinida, salto a memoria sin mapear): se
/// entrega al manejador guest como lo haria el nucleo y, si vuelve o sale con siglongjmp, la ejecucion sigue con el
/// contexto que dejo. Sin manejador, termina el proceso con la senal.
pub fn guest_fault(c: &mut Cpu, sig: i32, code: i32, addr: u64, what: &str, val: u64) {
    let act = guest_act(sig);
    // como `force_sig` del nucleo: una senal de fallo bloqueada no llega al manejador, termina el proceso
    if act.handler > 1 && !guest_blocks(sig) {
        if let Some(t) = rt::cur_opt() {
            let mut info = [0u8; 128];
            unsafe {
                *(info.as_mut_ptr() as *mut i32) = sig;
                *(info.as_mut_ptr().add(8) as *mut i32) = code;
                *(info.as_mut_ptr().add(16) as *mut u64) = addr;
            }
            if unsafe { deliver_at(t, c, sig, &info, act) } {
                return;
            }
        }
    }
    crate::bridge::alog_fatal(&format!("fallo guest ({}): {} {:#x}\n{}", sig, what, val, rt::dump_backtrace(c)));
    // accion por defecto: terminar con la senal correspondiente
    let ka = KSigaction { handler: 0, flags: SA_RESTORER, restorer: heddle_restorer as u64, mask: 0 };
    raw_sigaction(sig, Some(&ka), None);
    unsafe { raise(sig) };
    unsafe { _exit(128 + sig) }
}

/// Falta de alineacion de un acceso exclusivo, atomico u ordenado: sin FEAT_LSE2 (no se anuncia, `feat`), el Arm ARM
/// exige la direccion alineada al tamano del acceso (al de los dos elementos en LDXP/STXP/CASP) y si no, un Alignment
/// fault (AArch64.CheckAlignment con un acceso atomico u ordenado; en los exclusivos, antes de mirar el monitor,
/// AArch64.ExclusiveMonitorsPass). Linux arm64 lo entrega como SIGBUS BUS_ADRALN con si_addr = la direccion.
///
/// Se envia al propio hilo con rt_tgsigqueueinfo (si_code > 0): llega al manejador del host en este mismo punto como un
/// fallo sincrono (`async_host_handler`/`fault_host` -> `sync_fault`), con el pc exacto de la instruccion (bloque
/// traducido o helper: `Jit::fault_pc`; interprete: `cpu.pc`). Si el guest tiene manejador, se ejecuta y la ejecucion
/// sigue con su contexto: no se vuelve aqui. Si vuelve (sin manejador, o bloqueada o ignorada), termina el proceso con
/// SIGBUS, como `force_sig` del nucleo.
pub fn align_fault(addr: u64) -> ! {
    let mut info = [0u8; 128];
    unsafe {
        *(info.as_mut_ptr() as *mut i32) = 7;
        *(info.as_mut_ptr().add(8) as *mut i32) = 1; // BUS_ADRALN
        *(info.as_mut_ptr().add(16) as *mut u64) = addr;
        let (pid, tid) = (syscall(39), syscall(186));
        syscall(297, pid, tid, 7i64, info.as_ptr());
    }
    if let Some(t) = rt::cur_opt() {
        crate::bridge::alog_fatal(&format!("FALLO: SIGBUS (alineacion) en codigo guest, addr={:#x}\n{}", addr, rt::dump_backtrace(&t.cpu)));
    }
    let ka = KSigaction { handler: 0, flags: SA_RESTORER, restorer: heddle_restorer as u64, mask: 0 };
    raw_sigaction(7, Some(&ka), None);
    let set: u64 = sbit(7);
    unsafe {
        syscall(14, 1i64 /* SIG_UNBLOCK */, &set as *const u64, 0usize, 8usize);
        raise(7);
        _exit(128 + 7)
    }
}

/// El host llamo (call/jmp) a una direccion que no es ejecutable: codigo guest (mapeado solo-lectura), un slot HLE
/// (PROT_NONE) o una region que creo el guest. Es un callback guest invocado de forma nativa (puntero a funcion
/// dentro de una estructura, vtable, argumento no previsto...). Se redirige el `rip` a un trampolin host->guest:
/// los registros de argumentos y la direccion de retorno siguen intactos, asi que el trampolin actua como la propia
/// funcion y retorna al llamador del host. Layout x86_64 de ucontext_t: mcontext en +40, gregs[REG_RIP=16].
///
/// # Safety
/// `info` y `uc` son nulos o los `siginfo_t`/`ucontext_t` que el nucleo entrega al manejador.
pub unsafe fn try_exec_redirect(sig: i32, info: *mut u8, uc: *mut c_void) -> bool {
    if sig != 11 || info.is_null() || uc.is_null() {
        return false;
    }
    let (code, addr) = unsafe { (*(info.add(8) as *const i32), *(info.add(16) as *const u64)) };
    let rip_p = unsafe { (uc as *mut u8).add(40 + 8 * 16) as *mut u64 };
    let rip = unsafe { *rip_p };
    const SEGV_ACCERR: i32 = 2;
    if code != SEGV_ACCERR || addr != rip || rip < 0x10000 {
        return false;
    }
    // Solo si la direccion es realmente ejecutable por el guest (modulo guest, region que el guest pidio
    // ejecutable o slot HLE). Un salto del host a datos o a basura es un fallo del host: no se interpreta como ARM.
    if !crate::boundary::is_guest_executable_sig(rip) {
        return false;
    }
    // sin bloqueos ni reservas: un trampolin ya creado para esa funcion (con su firma, si se registro como native
    // JNI) o uno nuevo con reparto universal
    let t = crate::cbthunk::find_for_guest(rip).unwrap_or_else(|| crate::cbthunk::make(rip, b'J', 0));
    if t == 0 {
        return false; // sin trampolines disponibles: el fallo sigue su curso (diagnostico y terminacion)
    }
    unsafe { *rip_p = t };
    true
}

/// Manejador del native bridge para SIGSEGV/SIGBUS/SIGFPE/SIGILL (sigchain lo llama antes que a nadie, segun
/// getSignalHandler). Devuelve true si la senal se consumio: el host llamo a una direccion guest (redireccion) o el
/// manejador guest gestiono un fallo de codigo guest y la ejecucion sigue con el contexto que dejo.
///
/// # Safety
/// `info` y `uc` son nulos o los `siginfo_t`/`ucontext_t` que el nucleo entrega al manejador.
pub unsafe fn native_bridge_signal(sig: i32, info: *mut u8, uc: *mut c_void) -> bool {
    unsafe { try_exec_redirect(sig, info, uc) || sync_fault(sig, info, uc) }
}

/// Codigo ejecutable de la propia biblioteca del puente [lo, hi): para no registrar como propio (ni tratar como del
/// guest) un fallo de otro codigo del host, p. ej. las comprobaciones implicitas de ART en un hilo que tambien
/// ejecuta codigo guest.
static OWN_LO: AtomicU64 = AtomicU64::new(0);
static OWN_HI: AtomicU64 = AtomicU64::new(0);

/// Calcula el rango de `OWN_LO/HI` (fuera de un manejador de senal: lee /proc/self/maps).
pub fn init_own_range() {
    let me = init_own_range as *const () as u64;
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let parse = |l: &str| -> Option<(u64, u64, String, bool)> {
        let mut it = l.split_whitespace();
        let (r, perms) = (it.next()?, it.next()?);
        let path = it.nth(3).unwrap_or("").to_string();
        let (a, b) = r.split_once('-')?;
        Some((u64::from_str_radix(a, 16).ok()?, u64::from_str_radix(b, 16).ok()?, path, perms.as_bytes().get(2) == Some(&b'x')))
    };
    let Some(path) = maps.lines().filter_map(parse).find(|e| me >= e.0 && me < e.1).map(|e| e.2) else { return };
    let (mut lo, mut hi) = (u64::MAX, 0);
    for (a, b, p, x) in maps.lines().filter_map(parse) {
        if x && p == path {
            lo = lo.min(a);
            hi = hi.max(b);
        }
    }
    if lo < hi {
        OWN_LO.store(lo, Relaxed);
        OWN_HI.store(hi, Relaxed);
    }
}

/// El fallo de `uc` ocurrio en codigo traducido de este hilo o en el propio puente? (sin bloqueos ni reservas)
///
/// # Safety
/// `uc` es nulo o el `ucontext_t` que el nucleo entrego al manejador.
pub unsafe fn fault_is_ours(uc: *mut c_void) -> bool {
    if uc.is_null() {
        return false;
    }
    let rip = unsafe { *greg(uc, X_RIP) };
    let (lo, hi) = (OWN_LO.load(Relaxed), OWN_HI.load(Relaxed));
    (lo == 0 || (rip >= lo && rip < hi)) || rt::cur_opt().is_some_and(|t| t.jit.code_contains(rip))
}

/// Instala los manejadores de fallo del host para que SIGSEGV etc. de codigo traducido lleguen al guest.
pub fn install_fault_handlers() {
    init_own_range();
    for sig in [11, 7, 8, 4] {
        let ka = KSigaction { handler: fault_host as u64, flags: SA_SIGINFO | SA_RESTORER | SA_NODEFER | SA_ONSTACK, restorer: heddle_restorer as u64, mask: 0 };
        raw_sigaction(sig, Some(&ka), None);
    }
}

extern "C" fn fault_host(sig: i32, info: *mut u8, uc: *mut c_void) {
    if unsafe { native_bridge_signal(sig, info, uc) } {
        return;
    }
    fatal_fault(sig, info);
}

/// Fallo que nadie gestiona: diagnostico y accion por defecto (al volver, la instruccion falla otra vez y el nucleo
/// termina el proceso con la senal).
fn fatal_fault(sig: i32, info: *mut u8) {
    if let Some(t) = rt::cur_opt() {
        let addr = if info.is_null() { 0 } else { unsafe { *(info.add(16) as *const u64) } };
        crate::bridge::alog_fatal(&format!("FALLO: senal {} en codigo guest, addr={:#x}\n{}", sig, addr, rt::dump_backtrace(&t.cpu)));
    }
    let ka = KSigaction { handler: 0, flags: SA_RESTORER, restorer: heddle_restorer as u64, mask: 0 };
    raw_sigaction(sig, Some(&ka), None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_with(sp: u64) -> Box<Cpu> {
        let mut c = Cpu::new();
        for i in 0..31 {
            c.x[i] = 0x1000 + i as u64;
        }
        c.x[31] = sp;
        c.pc = 0xABCD0;
        c.v[3] = [7, 9];
        c
    }

    /// Al volver de un manejador, FPCR/FPSR del ucontext se escriben como lo haria el kernel en el registro: los bits
    /// RAZ/WI (trampas, Len/Stride, reservados) no se guardan.
    #[test]
    fn ucontext_respeta_los_bits_raz_de_fpcr_y_fpsr() {
        let buf = vec![0u64; 512];
        let uc = buf.as_ptr() as u64;
        unsafe {
            *((uc + UC_RESERVED) as *mut u32) = 0x4650_8001;
            *((uc + UC_RESERVED + 8) as *mut u32) = u32::MAX;
            *((uc + UC_RESERVED + 12) as *mut u32) = u32::MAX;
        }
        let mut c = Cpu::new();
        unsafe { apply_uc(&mut c, uc) };
        assert_eq!(c.fpcr, crate::interp::FPCR_RW);
        assert_eq!(c.fpsr, crate::interp::FPSR_RW);
        assert_eq!(c.fpcr & 0x9F00, 0);
    }

    #[test]
    fn marco_en_la_pila_del_hilo_con_registros() {
        let stack = vec![0u8; 256 << 10];
        let (lo, hi) = (stack.as_ptr() as u64, stack.as_ptr() as u64 + stack.len() as u64);
        let isp = (hi - 4096) & !15;
        let c = cpu_with(isp);
        let f = unsafe { build_frame(&c, (lo, hi), (0, 2, 0), true, 0, std::ptr::null()) };
        // todo el marco queda bajo el sp interrumpido y dentro de la pila: [sp del manejador, fin) es contiguo
        assert!(f.sp > lo && f.sp < f.info && f.info + 128 <= f.uc && f.uc + UC_SIZE <= isp - SIG_MARGIN + 16);
        assert_eq!(f.sp & 15, 0);
        let r = |o: u64| unsafe { *((f.uc + o) as *const u64) };
        assert_eq!(r(184), 0x1000);
        assert_eq!(r(184 + 8 * 30), 0x1000 + 30);
        assert_eq!(r(432), isp);
        assert_eq!(r(440), 0xABCD0);
        assert_eq!(unsafe { *((f.uc + 464) as *const u32) }, 0x4650_8001);
        assert_eq!(unsafe { *((f.uc + 464 + 16 + 48) as *const [u64; 2]) }, [7, 9]);
    }

    /// La senal 64 tiene su propia accion (no ACTS[sig & 63] = ACTS[0]) y la lectura no toma bloqueos.
    #[test]
    fn accion_de_la_senal_64() {
        let a = GuestAct { handler: 0x1234_5670, flags: SA_SIGINFO, mask: 1 << 9 };
        // solo la tabla (sin instalar nada en el nucleo: otra prueba usa la senal 64 en paralelo)
        let old = act_swap(64, a);
        let r = guest_act(64);
        assert_eq!((r.handler, r.flags, r.mask), (a.handler, a.flags, a.mask));
        assert_eq!(guest_act(0).handler, 0);
        assert_eq!(guest_act(65).handler, 0);
        let prev = act_swap(64, old);
        assert_eq!(prev.handler, a.handler);
        assert_eq!(guest_act(64).handler, old.handler);
    }

    /// `movz/movk` de 64 bits en `rd`.
    fn mov64(rd: u32, v: u64) -> [u32; 4] {
        let h = |k: u32| ((v >> (16 * k)) & 0xffff) as u32;
        [0xD280_0000 | rd | h(0) << 5, 0xF2A0_0000 | rd | h(1) << 5, 0xF2C0_0000 | rd | h(2) << 5, 0xF2E0_0000 | rd | h(3) << 5]
    }

    /// Funcion guest en memoria propia (se queda viva hasta el final del proceso hijo).
    fn guest(words: &[u32]) -> u64 {
        let v: &'static mut Vec<u32> = Box::leak(Box::new(words.to_vec()));
        v.as_ptr() as u64
    }

    fn cat(parts: &[&[u32]]) -> Vec<u32> {
        parts.iter().flat_map(|p| p.iter().copied()).collect()
    }

    /// Lo que ejecuta el proceso hijo de `reanudar_tras_fallos_sincronos` (manejadores de SIGSEGV de todo el proceso:
    /// no puede convivir con las demas pruebas).
    fn escenarios_de_reanudacion() {
        install_fault_handlers();
        let seen: &'static mut [u64; 4] = Box::leak(Box::new([0u64; 4]));
        let valid: &'static mut u32 = Box::leak(Box::new(0x1234u32));
        let seen_p = seen.as_ptr() as u64;
        // manejador que corrige el registro base del acceso (x1 en el ucontext) y vuelve: se repite la instruccion
        let fix_x1 = guest(&cat(&[&mov64(9, valid as *const u32 as u64), &[0xf9006049 /* str x9, [x2, #192] */, 0xd65f03c0]]));
        let sa = |h: u64, flags: u64| guest_sigaction(11, Some(GuestAct { handler: h, flags: SA_SIGINFO | flags, mask: 0 })).unwrap();

        // 1) el manejador cambia el pc del contexto (lo que hace Mono) y vuelve; ademas anota si_addr
        let recov = guest(&[0xd2800540 /* mov x0, #42 */, 0xd65f03c0]);
        let h1 = guest(&cat(&[&mov64(9, recov), &[0xf900dc49 /* str x9, [x2, #440] */, 0xf940082a /* ldr x10, [x1, #16] */], &mov64(11, seen_p), &[0xf900016a /* str x10, [x11] */, 0xd65f03c0]]));
        let main1 = guest(&[0xd2800201 /* mov x1, #0x10 */, 0xb9400020 /* ldr w0, [x1] */, 0xd2800020 /* mov x0, #1 */, 0xd65f03c0]);
        sa(h1, SA_NODEFER);
        assert_eq!(rt::call_guest(main1, &[], &[]).0, 42, "1: el pc del ucontext no se aplico");
        assert_eq!(seen[0], 0x10, "1: si_addr");

        // 2) el manejador sale con siglongjmp (sin SA_NODEFER: SIGSEGV queda bloqueada en el manejador para el guest)
        let jb: &'static mut [u64; 32] = Box::leak(Box::new([0u64; 32]));
        let jbp = jb.as_ptr() as u64;
        let setjmp = crate::libc_hle::resolve("sigsetjmp").unwrap();
        let longjmp = crate::libc_hle::resolve("siglongjmp").unwrap();
        let main2 = guest(&cat(&[
            &[0xa9bf7bfd], // stp x29, x30, [sp, #-16]!
            &mov64(0, jbp),
            &[0xd2800021], // mov x1, #1
            &mov64(16, setjmp),
            &[0xd63f0200, 0xb5000080 /* cbnz x0, +16 */, 0xd2800201, 0xb9400022 /* ldr w2, [x1] */, 0xd28000e0 /* mov x0, #7 */],
            &[0xa8c17bfd, 0xd65f03c0], // ldp x29, x30, [sp], #16; ret
        ]));
        let h2 = guest(&cat(&[&mov64(0, jbp), &[0xd28000a1 /* mov x1, #5 */], &mov64(16, longjmp), &[0xd63f0200]]));
        sa(h2, 0);
        assert_eq!(rt::call_guest(main2, &[], &[]).0, 5, "2: siglongjmp desde el manejador");
        // la mascara que restauro siglongjmp no deja SIGSEGV bloqueada en el nucleo
        assert_eq!(kernel_mask() & sbit(11), 0);
        let mut m = 0;
        guest_sigprocmask(0, None, Some(&mut m));
        assert_eq!(m & sbit(11), 0, "2: el guest no deberia ver SIGSEGV bloqueada tras siglongjmp");
        // y otra vez (el estado del nivel de senal quedo limpio)
        assert_eq!(rt::call_guest(main2, &[], &[]).0, 5, "2: segunda vez");

        // 3) el manejador corrige x1 y vuelve: se repite exactamente la instruccion que fallo
        sa(fix_x1, 0);
        let main3 = guest(&[0xd2800060 /* mov x0, #3 */, 0xd2800201, 0xb9400020 /* ldr w0, [x1] */, 0x91001800 /* add x0, x0, #6 */, 0xd65f03c0]);
        assert_eq!(rt::call_guest(main3, &[], &[]).0, 0x1234 + 6, "3: reintento con el registro corregido");

        // 4) BRK -> SIGTRAP; el manejador provoca un SIGSEGV anidado (interprete) que fix_x1 corrige, y avanza el pc
        let h4 = guest(&cat(&[
            &[0xd2800201, 0xb9400023 /* ldr w3, [x1] */],
            &mov64(11, seen_p + 8),
            &[0xb9000163 /* str w3, [x11] */, 0xf940dc49 /* ldr x9, [x2, #440] */, 0x91001129 /* add x9, x9, #4 */, 0xf900dc49, 0xd65f03c0],
        ]));
        guest_sigaction(5, Some(GuestAct { handler: h4, flags: SA_SIGINFO, mask: 0 })).unwrap();
        let main4 = guest(&[0xd2800060, 0xd4200000 /* brk #0 */, 0x91001800, 0xd65f03c0]);
        assert_eq!(rt::call_guest(main4, &[], &[]).0, 9, "4: el manejador de SIGTRAP avanzo el pc");
        assert_eq!(seen[1], 0x1234, "4: fallo anidado dentro del manejador reanudado");

        // 5) salto a memoria sin mapear: SIGSEGV con pc = esa direccion; el manejador 1 lo manda a `recov`
        sa(h1, SA_NODEFER);
        let main5 = guest(&cat(&[&mov64(16, 0x10_0000), &[0xd61f0200 /* br x16 */]]));
        assert_eq!(rt::call_guest(main5, &[], &[]).0, 42, "5: fallo de instruccion");
        assert_eq!(seen[0], 0x10_0000);

        // 6) fallo en un store (en modo fence, dentro de la marca del monitor): fix_x1 corrige la base y se repite
        sa(fix_x1, 0);
        let main6 = guest(&[0xd28009a0 /* mov x0, #77 */, 0xd2800201, 0xb9000020 /* str w0, [x1] */, 0xb9400020 /* ldr w0, [x1] */, 0xd65f03c0]);
        assert_eq!(rt::call_guest(main6, &[], &[]).0, 77, "6: reintento del store");
        assert_eq!(*valid, 77);
        // y en una atomica (SWP): tambien con el bloqueo de granulo de la ruta lenta sin tomar
        let main6b = guest(&[0xd28000a0 /* mov x0, #5 */, 0xd2800201, 0xb8208022 /* swp w0, w2, [x1] */, 0xaa0203e0 /* mov x0, x2 */, 0xd65f03c0]);
        assert_eq!(rt::call_guest(main6b, &[], &[]).0, 77, "6: reintento de la atomica");
        assert_eq!(*valid, 5);

        // 7) mascaras como libsigchain + bionic. pthread_sigmask: SIG_BLOCK de SEGV, BUS, ILL y USR2 bloquea solo ILL
        // y USR2 (SEGV y BUS las reclama ART); la consulta devuelve la mascara real del nucleo
        let blk = || {
            let st = std::fs::read_to_string("/proc/thread-self/status").unwrap();
            let l = st.lines().find(|l| l.starts_with("SigBlk:")).unwrap();
            u64::from_str_radix(l[7..].trim(), 16).unwrap()
        };
        let want = sbit(11) | sbit(7) | sbit(4) | sbit(12);
        let mut old = 0u64;
        assert_eq!(crate::libc_hle::guest_mask_call(0, &want as *const u64 as u64, &mut old as *mut u64 as u64, false), 0);
        assert_eq!(old & want, 0);
        assert_eq!(blk() & want, sbit(4) | sbit(12), "7: libsigchain filtra SEGV/BUS; ILL y USR2 al nucleo");
        let mut m = 0u64;
        assert_eq!(crate::libc_hle::guest_mask_call(0, 0, &mut m as *mut u64 as u64, false), 0);
        assert_eq!(m & want, sbit(4) | sbit(12), "7: la consulta es la mascara real");
        // rt_sigprocmask directo (svc): misma consulta; sigsetsize distinto de 8 es EINVAL; `how` solo se comprueba
        // con conjunto nuevo
        let mut c = Cpu::new();
        let mut m2 = 0u64;
        assert_eq!(crate::syscall::translate(&mut c, 135, [0, 0, &mut m2 as *mut u64 as u64, 8, 0, 0]), 0);
        assert_eq!(m2, m);
        assert_eq!(crate::syscall::translate(&mut c, 135, [0, 0, &mut m2 as *mut u64 as u64, 16, 0, 0]), -EINVAL);
        assert_eq!(crate::syscall::translate(&mut c, 135, [7, 0, &mut m2 as *mut u64 as u64, 8, 0, 0]), 0);
        let segv = sbit(11);
        let segv_bus = sbit(11) | sbit(7);
        assert_eq!(crate::syscall::translate(&mut c, 135, [7, &segv as *const u64 as u64, 0, 8, 0, 0]), -EINVAL);
        // sin filtro: SEGV y BUS quedan bloqueadas para el guest (BUS en el nucleo; SEGV solo en la vista)
        assert!(!guest_blocks(11));
        assert_eq!(crate::syscall::translate(&mut c, 135, [0, &segv_bus as *const u64 as u64, 0, 8, 0, 0]), 0);
        assert_eq!(blk() & segv_bus, sbit(7), "7: BUS bloqueada en el nucleo; SEGV nunca");
        guest_sigprocmask(0, None, Some(&mut m));
        assert_eq!(m & want, want, "7: la consulta incluye SEGV");
        assert!(guest_blocks(11) && guest_blocks(7));
        // pthread_sigmask(SIG_UNBLOCK) si deja desbloquearlas
        assert_eq!(guest_sigprocmask(1, Some(segv), None), 0);
        assert!(!guest_blocks(11));
        // SIG_SETMASK filtra tambien: pedir SEGV no la bloquea
        assert_eq!(guest_sigprocmask(2, Some(old | segv), None), 0);
        assert!(!guest_blocks(11) && !guest_blocks(7) && !guest_blocks(12));
        assert_eq!(blk() & want, 0);
        // y un fallo real sigue llegando al manejador
        sa(fix_x1, 0);
        *valid = 0x1234;
        assert_eq!(rt::call_guest(main3, &[], &[]).0, 0x1234 + 6, "7: fallo tras restaurar la mascara");

        // 8) caso `mascara` del banco: USR2 bloqueada, enviada, pendiente; al desbloquear llega una sola vez
        let h8 = guest(&cat(&[&mov64(11, seen_p + 16), &[0xf940016a /* ldr x10, [x11] */, 0x9100054a /* add x10, x10, #1 */, 0xf900016a, 0xd65f03c0]]));
        guest_sigaction(12, Some(GuestAct { handler: h8, flags: SA_SIGINFO, mask: 0 })).unwrap();
        let usr2 = sbit(12);
        assert_eq!(crate::libc_hle::guest_mask_call(0, &usr2 as *const u64 as u64, 0, false), 0);
        let (pid, tid) = unsafe { (syscall(39), syscall(186)) };
        assert_eq!(unsafe { syscall(234, pid, tid, 12i64) }, 0);
        let mut pend = 0u64;
        assert_eq!(unsafe { syscall(127, &mut pend as *mut u64, 8usize) }, 0);
        assert_eq!(pend & usr2, usr2, "8: USR2 pendiente");
        assert_eq!(seen[2], 0, "8: no debe llegar bloqueada");
        assert_eq!(crate::libc_hle::guest_mask_call(1, &usr2 as *const u64 as u64, 0, false), 0);
        assert_eq!(seen[2], 1, "8: llega una vez al desbloquear");
        println!("escenarios OK");
    }

    /// Reanudar tras un fallo sincrono (punto 5 del informe): manejador que cambia el pc, que sale con siglongjmp,
    /// que corrige un registro y vuelve, BRK con un fallo anidado en el manejador y salto a memoria sin mapear.
    #[test]
    fn reanudar_tras_fallos_sincronos() {
        if std::env::var_os("HEDDLE_PRUEBA_REANUDAR").is_some() {
            escenarios_de_reanudacion();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["sig::tests::reanudar_tras_fallos_sincronos", "--exact", "--test-threads=1", "--nocapture"])
            .env("HEDDLE_PRUEBA_REANUDAR", "1")
            .output()
            .unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && so.contains("escenarios OK"), "{}\n{}", so, String::from_utf8_lossy(&out.stderr));
    }

    /// Una senal con manejador guest que llega mientras el hilo tiene un bloqueo del puente (aqui la lista de claves)
    /// se aplaza hasta soltarlo: el manejador guest podria pedir el mismo bloqueo y el hilo se bloquearia a si mismo.
    #[test]
    fn seccion_critica_del_puente_aplaza_senales() {
        std::thread::spawn(|| {
            let t = rt::cur();
            let seen: &'static mut [u64; 1] = Box::leak(Box::new([0u64; 1]));
            let h = guest(&cat(&[&mov64(11, seen.as_ptr() as u64), &[0xf940016a, 0x9100054a, 0xf900016a, 0xd65f03c0]]));
            const S: i32 = 44;
            let old = guest_sigaction(S, Some(GuestAct { handler: h, flags: SA_SIGINFO, mask: 0 })).unwrap();
            let (pid, tid) = unsafe { (syscall(39), syscall(186)) };
            {
                let _g = crate::monitor::NoSignals::new();
                assert_eq!(unsafe { syscall(234, pid, tid, S as i64) }, 0);
                assert_eq!(unsafe { std::ptr::read_volatile(&seen[0]) }, 0, "entregada dentro de la seccion critica");
            }
            assert_eq!(unsafe { std::ptr::read_volatile(&seen[0]) }, 1, "no se entrego al salir de la seccion");
            crate::libc_hle::with_keys_locked(|| {
                assert_eq!(unsafe { syscall(234, pid, tid, S as i64) }, 0);
                assert_eq!(unsafe { std::ptr::read_volatile(&seen[0]) }, 1);
            });
            assert_eq!(unsafe { std::ptr::read_volatile(&seen[0]) }, 2);
            guest_sigaction(S, Some(old)).unwrap();
            let _ = t;
        })
        .join()
        .unwrap();
    }

    #[test]
    fn marco_fuera_del_guest_y_pila_alternativa() {
        let stack = vec![0u8; 256 << 10];
        let alt = vec![0u8; 64 << 10];
        let (lo, hi) = (stack.as_ptr() as u64, stack.as_ptr() as u64 + stack.len() as u64);
        let (alo, ahi) = (alt.as_ptr() as u64, alt.as_ptr() as u64 + alt.len() as u64);
        // hilo sin codigo guest en curso (sp 0): se usa el tope de su pila
        let f = unsafe { build_frame(&cpu_with(0), (lo, hi), (0, 2, 0), false, 0, std::ptr::null()) };
        assert!(f.sp > lo && f.uc + UC_SIZE <= hi);
        // SA_ONSTACK con pila alternativa activa
        let f = unsafe { build_frame(&cpu_with(hi - 4096), (lo, hi), (alo, 0, alt.len() as u64), true, 0, std::ptr::null()) };
        assert!(f.sp > alo && f.uc + UC_SIZE <= ahi);
        // sin SA_ONSTACK se ignora
        let f = unsafe { build_frame(&cpu_with(hi - 4096), (lo, hi), (alo, 0, alt.len() as u64), false, 0, std::ptr::null()) };
        assert!(f.sp > lo && f.sp < hi);
        // pila del hilo agotada: se usa la de reserva, no la pagina de guarda
        let f = unsafe { build_frame(&cpu_with(lo + 1024), (lo, hi), (0, 2, 0), false, ahi, std::ptr::null()) };
        assert!(f.sp > alo && f.sp < ahi);
    }
}

