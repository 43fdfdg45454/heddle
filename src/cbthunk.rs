//! Trampolines host -> guest: una funcion x86-64 que, al llamarse, ejecuta una funcion guest.
//!
//! Reparto de argumentos:
//!  * Con firma (shorty de JNI: natives de getTrampoline/RegisterNatives): se recorre la firma con los dos ABI reales.
//!    Host SysV x86-64: 6 enteros en rdi..r9, 8 flotantes en xmm0..7 y el resto en la pila del host, en orden, una
//!    palabra de 8 bytes por argumento. Guest AAPCS64 (Linux): 8 enteros en x0..x7, 8 flotantes en v0..v7 y el resto
//!    en la pila guest, en orden, una palabra de 8 bytes por argumento (un `float` en los 4 bytes bajos). Asi un
//!    metodo con mas de 8 `float`/`double` o con coma flotante en pila recibe cada argumento en su sitio.
//!  * Sin firma (callbacks del NDK, redireccion de ejecucion): reparto universal. AAPCS64 y SysV asignan enteros y
//!    flotantes a bancos independientes en el mismo orden: x0..x5 <- rdi..r9, v0..v7 <- xmm0..7 y los enteros 7 y 8
//!    (x6, x7) y siguientes salen de la pila del host. Correcto mientras no haya flotantes en pila (sigtool marca
//!    `Manual` los callbacks con mas de 8 parametros).
//!
//! La arena y su tabla de busqueda no toman bloqueos ni reservan memoria: `make` se puede llamar desde un manejador de
//! senal (redireccion de ejecucion, `sig::try_exec_redirect`).

use crate::rt;
use std::cell::UnsafeCell;
use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU64, AtomicU8};

/// Argumentos como mucho que se reparten por firma (sin contar JNIEnv* y jclass/jobject). Una firma mas larga usa el
/// reparto universal (y queda en el registro).
pub const MAX_SIG_ARGS: usize = 51;

#[repr(C)]
pub struct CbFrame {
    ints: [u64; 6],   // 0
    fps: [u64; 8],    // 48
    stack: u64,       // 112
    info: u64,        // 120
    out_rax: u64,     // 128
    out_xmm0: u64,    // 136
    out_rdx: u64,     // 144
}

/// Datos de un trampolin (64 bytes, en una tabla estatica: no se reserva memoria al crearlo).
#[repr(C)]
pub struct ThunkInfo {
    pub guest: u64,
    pub ret: u8,
    pub special: u8, // 1 = ANativeActivity_onCreate
    pub jni: u8,     // 0 = ninguno, 1 = arg0 es JNIEnv* (y arg1 jclass/jobject), 2 = arg0 es JavaVM*
    /// argumentos de la firma (shorty sin el retorno); 0 = sin firma (reparto universal)
    pub nargs: u8,
    ready: AtomicU8,
    pub args: [u8; MAX_SIG_ARGS],
}

impl ThunkInfo {
    const EMPTY: ThunkInfo = ThunkInfo { guest: 0, ret: 0, special: 0, jni: 0, nargs: 0, ready: AtomicU8::new(0), args: [0; MAX_SIG_ARGS] };
    fn sig(&self) -> &[u8] {
        &self.args[..self.nargs as usize]
    }
}

std::arch::global_asm!(
    ".globl heddle_cb_entry",
    ".type heddle_cb_entry,@function",
    "heddle_cb_entry:",
    "push rbp",
    "mov rbp, rsp",
    "sub rsp, 160",
    "mov [rsp], rdi",
    "mov [rsp+8], rsi",
    "mov [rsp+16], rdx",
    "mov [rsp+24], rcx",
    "mov [rsp+32], r8",
    "mov [rsp+40], r9",
    "movq [rsp+48], xmm0",
    "movq [rsp+56], xmm1",
    "movq [rsp+64], xmm2",
    "movq [rsp+72], xmm3",
    "movq [rsp+80], xmm4",
    "movq [rsp+88], xmm5",
    "movq [rsp+96], xmm6",
    "movq [rsp+104], xmm7",
    "lea rax, [rbp+16]",
    "mov [rsp+112], rax",
    "mov [rsp+120], r10",
    "mov rdi, rsp",
    "call {d}",
    "mov rax, [rsp+128]",
    "movq xmm0, [rsp+136]",
    "mov rdx, [rsp+144]",
    "leave",
    "ret",
    d = sym cb_dispatch,
);

extern "C" {
    fn heddle_cb_entry();
}

/// Argumentos repartidos para el guest (AAPCS64).
pub struct GuestArgs {
    pub x: [u64; 8],
    pub v: [u64; 8],
    pub stack: [u64; MAX_SIG_ARGS + 2],
    pub nstack: usize,
}

/// Reparte los argumentos de una llamada SysV x86-64 (`ints` = rdi..r9, `fps` = xmm0..7, `host_stack` = la pila del
/// llamador en el punto de entrada, primer argumento en pila en la palabra 0) segun la firma: `lead` enteros primero
/// (JNIEnv*, jclass/jobject) y luego `sig` (caracteres de shorty: Z B C S I J L enteros, F float, D double; O = una
/// interfaz del host con tabla de funciones, OpenSL/OpenMAX, que el guest recibe como su proxy).
/// Los enteros estrechos se extienden como pide el shorty (el host no lo garantiza en los 32 bits altos).
///
/// # Safety
/// `host_stack` apunta a la pila del llamador con al menos tantas palabras como argumentos en pila tenga la firma.
pub unsafe fn marshal_sig(ints: &[u64; 6], fps: &[u64; 8], host_stack: *const u64, lead: usize, sig: &[u8]) -> GuestArgs {
    let mut g = GuestArgs { x: [0; 8], v: [0; 8], stack: [0; MAX_SIG_ARGS + 2], nstack: 0 };
    let (mut hi, mut hf, mut hs, mut gi, mut gf) = (0usize, 0usize, 0usize, 0usize, 0usize);
    for k in 0..lead + sig.len() {
        let c = if k < lead { b'J' } else { sig[k - lead] };
        let fp = c == b'F' || c == b'D';
        let raw = if fp {
            if hf < 8 {
                hf += 1;
                fps[hf - 1]
            } else {
                hs += 1;
                unsafe { *host_stack.add(hs - 1) }
            }
        } else if hi < 6 {
            hi += 1;
            ints[hi - 1]
        } else {
            hs += 1;
            unsafe { *host_stack.add(hs - 1) }
        };
        let val = match c {
            b'Z' => raw & 0xff,
            b'B' => raw as i8 as i64 as u64,
            b'C' => raw & 0xffff,
            b'S' => raw as i16 as i64 as u64,
            b'I' => raw as i32 as i64 as u64,
            b'F' => raw & 0xffff_ffff,
            b'O' => crate::proxy::guest_itf(raw),
            _ => raw,
        };
        if fp && gf < 8 {
            g.v[gf] = val;
            gf += 1;
        } else if !fp && gi < 8 {
            g.x[gi] = val;
            gi += 1;
        } else if g.nstack < g.stack.len() {
            g.stack[g.nstack] = val;
            g.nstack += 1;
        }
    }
    g
}

extern "C" fn cb_dispatch(f: *mut CbFrame) {
    let f = unsafe { &mut *f };
    let info = unsafe { &*(f.info as *const ThunkInfo) };
    rt::ensure_thread(rt::DEFAULT_STACK);
    #[cfg(target_os = "android")]
    {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 300 {
            crate::bridge::alog(&format!(
                "host->guest {} jni={} firma={} args={:#x},{:#x},{:#x},{:#x}",
                crate::elf::describe_addr(info.guest),
                info.jni,
                String::from_utf8_lossy(info.sig()),
                f.ints[0],
                f.ints[1],
                f.ints[2],
                f.ints[3]
            ));
        }
    }
    if info.special == 1 {
        // ANativeActivity_onCreate(activity, savedState, savedStateSize): el guest recibe un proxy de la actividad
        crate::ndkcb::activity_on_create(info.guest, f.ints[0], f.ints[1], f.ints[2]);
        f.out_rax = 0;
        return;
    }
    let mut ints = f.ints;
    match info.jni {
        1 => ints[0] = crate::jni::guest_env_for(ints[0]),
        2 => ints[0] = crate::jni::guest_vm_for(ints[0]),
        _ => {}
    }
    let sp = f.stack as *const u64;
    let g = if info.nargs > 0 {
        let lead = if info.jni == 1 { 2 } else { 0 };
        unsafe { marshal_sig(&ints, &f.fps, sp, lead, info.sig()) }
    } else {
        // universal: enteros 7.. en la pila del host (x6, x7, luego pila guest)
        let mut g = GuestArgs { x: [0; 8], v: f.fps, stack: [0; MAX_SIG_ARGS + 2], nstack: 6 };
        g.x[..6].copy_from_slice(&ints);
        for i in 0..8 {
            let w = unsafe { *sp.add(i) };
            if i < 2 {
                g.x[6 + i] = w;
            } else {
                g.stack[i - 2] = w;
            }
        }
        g
    };
    let (x0, v0, x1) = rt::call_guest_regs(info.guest, &g.x, &g.v, &g.stack[..g.nstack]);
    f.out_rax = match info.ret {
        b'Z' => x0 & 0xff,
        b'B' => x0 as i8 as i64 as u64,
        b'C' => x0 & 0xffff,
        b'S' => x0 as i16 as i64 as u64,
        b'I' => x0 as i32 as i64 as u64,
        _ => x0,
    };
    f.out_xmm0 = if info.ret == b'F' { v0 & 0xffff_ffff } else { v0 };
    f.out_rdx = x1;
}

// ---------------------------------------------------------------------------------------------
// Arena y tabla de busqueda (sin bloqueos)
// ---------------------------------------------------------------------------------------------

const ARENA_BYTES: usize = 1 << 20;
const THUNK: usize = 32;
/// trampolines como mucho (arena fija)
const SLOTS: usize = ARENA_BYTES / THUNK;
/// entradas de la tabla de busqueda (el doble de trampolines: sondeo lineal corto)
const MAP: usize = 2 * SLOTS;
/// valor de una entrada cuyo trampolin no se pudo crear (arena agotada)
const NONE: u64 = u64::MAX;
/// Bit de VALS: trampolin retirado porque su funcion guest se descargo (`retire_range`). Sigue llamando a la misma
/// direccion guest (un puntero colgante del host falla alli, como en ARM), pero `find_for_guest` ya no lo devuelve
/// (otra biblioteca puede ocupar esas direcciones con otras firmas) y `make` solo lo reaprovecha para la misma clave
/// exacta (misma funcion, firma y modo: el trampolin seria identico), sin gastar arena en cada recarga.
const RETIRED: u64 = 1 << 62;

struct Infos(UnsafeCell<[ThunkInfo; SLOTS]>);
// SAFETY: cada ThunkInfo lo escribe solo quien reservo su indice (fetch_add) y se publica con `ready` (Release).
unsafe impl Sync for Infos {}

static INFOS: Infos = Infos(UnsafeCell::new([const { ThunkInfo::EMPTY }; SLOTS]));
/// tabla de busqueda: clave = hash de (guest, firma, modo) (0 = libre), valor = indice + 1 (0 = en creacion)
static KEYS: [AtomicU64; MAP] = [const { AtomicU64::new(0) }; MAP];
static VALS: [AtomicU64; MAP] = [const { AtomicU64::new(0) }; MAP];
static ARENA_BASE: AtomicU64 = AtomicU64::new(0);
/// indices de trampolin ya reservados
static USED: AtomicU64 = AtomicU64::new(0);
/// la arena se agoto alguna vez (diagnostico)
pub static EXHAUSTED: AtomicU64 = AtomicU64::new(0);
/// Giros como mucho esperando a que otro hilo publique un trampolin a medio crear (si quien lo crea es este mismo
/// hilo, interrumpido por una senal, se sigue sondeando y se crea otro).
const WAIT_SPINS: u32 = 1 << 14;

fn info(i: usize) -> &'static ThunkInfo {
    unsafe { &(*INFOS.0.get())[i] }
}

/// Base de la arena RWX, reservada la primera vez (mmap: vale en un manejador de senal).
fn arena() -> u64 {
    let b = ARENA_BASE.load(Acquire);
    if b != 0 {
        return b;
    }
    let Some(r) = crate::mem::Region::new(ARENA_BYTES, crate::sys::PROT_READ | crate::sys::PROT_WRITE | crate::sys::PROT_EXEC, crate::mem::Kind::Other) else {
        return 0;
    };
    match ARENA_BASE.compare_exchange(0, r.base(), AcqRel, Acquire) {
        Ok(_) => r.into_permanent(),
        Err(other) => other, // otro hilo gano: `r` se libera al salir
    }
}

fn hash(guest: u64, ret: u8, jni: u8, special: u8, sig: Option<&[u8]>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in guest.to_le_bytes() {
        mix(b);
    }
    for b in [ret, jni, special, sig.map_or(0, |s| s.len() as u8 + 1)] {
        mix(b);
    }
    for &b in sig.unwrap_or(&[]) {
        mix(b);
    }
    h.max(1)
}

/// Primera entrada de sondeo: solo depende de la funcion guest (los trampolines de una misma funcion quedan juntos y
/// `find_for_guest` los encuentra).
fn probe_start(guest: u64) -> usize {
    (guest.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) as usize % MAP
}

fn matches(i: &ThunkInfo, guest: u64, ret: u8, jni: u8, special: u8, sig: Option<&[u8]>) -> bool {
    i.guest == guest && i.ret == ret && i.jni == jni && i.special == special && sig.unwrap_or(&[]) == i.sig()
}

/// Devuelve un puntero a funcion x86-64 que ejecuta `guest` (ret = char de shorty del retorno), reparto universal.
pub fn make(guest: u64, ret: u8, jni: u8) -> u64 {
    make_full(guest, ret, jni, 0, None)
}

pub fn make_ex(guest: u64, ret: u8, jni: u8, special: u8) -> u64 {
    make_full(guest, ret, jni, special, None)
}

/// Trampolin con firma: `shorty` incluye el retorno en la primera posicion (como lo da ART). Una firma no valida o
/// demasiado larga usa el reparto universal.
pub fn make_sig(guest: u64, shorty: &[u8], jni: u8) -> u64 {
    let ret = shorty.first().copied().unwrap_or(b'J');
    let args = shorty.get(1..).unwrap_or(&[]);
    let valid = !args.is_empty() && args.len() <= MAX_SIG_ARGS && args.iter().all(|c| b"ZBCSIJFDLO".contains(c));
    make_full(guest, ret, jni, 0, if valid { Some(args) } else { None })
}

fn make_full(guest: u64, ret: u8, jni: u8, special: u8, sig: Option<&[u8]>) -> u64 {
    // una senal guest que llegue entre reservar la entrada y publicarla esperaria a este mismo hilo: se aplaza
    let _ns = crate::monitor::NoSignals::new();
    let base = arena();
    if base == 0 {
        return 0;
    }
    let h = hash(guest, ret, jni, special, sig);
    let start = probe_start(guest);
    for k in 0..MAP {
        let e = (start + k) % MAP;
        let mut key = KEYS[e].load(Acquire);
        if key == 0 {
            match KEYS[e].compare_exchange(0, h, AcqRel, Acquire) {
                Ok(_) => return create(base, e, guest, ret, jni, special, sig),
                Err(k2) => key = k2,
            }
        }
        if key != h {
            continue;
        }
        let mut v = VALS[e].load(Acquire);
        let mut n = 0;
        while v == 0 && n < WAIT_SPINS {
            std::hint::spin_loop();
            v = VALS[e].load(Acquire);
            n += 1;
        }
        if v == NONE {
            return 0;
        }
        if v != 0 && v & RETIRED != 0 {
            let idx = v & !RETIRED;
            if matches(info(idx as usize - 1), guest, ret, jni, special, sig) {
                // reactivar (si otro hilo ya lo hizo, la comparacion falla y vale igual)
                let _ = VALS[e].compare_exchange(v, idx, AcqRel, Acquire);
                return base + THUNK as u64 * (idx - 1);
            }
            continue;
        }
        if v != 0 && matches(info(v as usize - 1), guest, ret, jni, special, sig) {
            return base + THUNK as u64 * (v - 1);
        }
    }
    0
}

fn create(base: u64, e: usize, guest: u64, ret: u8, jni: u8, special: u8, sig: Option<&[u8]>) -> u64 {
    let s = USED.fetch_add(1, AcqRel) as usize;
    if s >= SLOTS {
        // Arena agotada: no se aborta (puede ejecutarse dentro de un manejador de senal) ni se crece. Se devuelve 0:
        // el host recibe un callback nulo o, en el redireccionamiento, el fallo sigue su curso.
        EXHAUSTED.store(1, Relaxed);
        VALS[e].store(NONE, Release);
        return 0;
    }
    unsafe {
        let p = &mut (*INFOS.0.get())[s];
        p.guest = guest;
        p.ret = ret;
        p.jni = jni;
        p.special = special;
        let a = sig.unwrap_or(&[]);
        p.nargs = a.len() as u8;
        p.args[..a.len()].copy_from_slice(a);
        p.ready.store(1, Release);
    }
    let at = base + (THUNK * s) as u64;
    let mut code = [0u8; THUNK];
    code[0..2].copy_from_slice(&[0x49, 0xBA]); // mov r10, imm64 (ThunkInfo)
    code[2..10].copy_from_slice(&(info(s) as *const ThunkInfo as u64).to_le_bytes());
    code[10..12].copy_from_slice(&[0x49, 0xBB]); // mov r11, imm64
    code[12..20].copy_from_slice(&(heddle_cb_entry as usize as u64).to_le_bytes());
    code[20..23].copy_from_slice(&[0x41, 0xFF, 0xE3]); // jmp r11
    unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, 23) };
    VALS[e].store(s as u64 + 1, Release);
    at
}

/// Trampolin ya creado para `guest`, preferiblemente uno con firma (el host llamo a la funcion guest directamente:
/// si se registro con RegisterNatives/getTrampoline, su firma es la buena). Sin bloqueos ni reservas.
pub fn find_for_guest(guest: u64) -> Option<u64> {
    let base = ARENA_BASE.load(Acquire);
    if base == 0 {
        return None;
    }
    let start = probe_start(guest);
    let mut any = None;
    for k in 0..MAP {
        let e = (start + k) % MAP;
        if KEYS[e].load(Acquire) == 0 {
            break;
        }
        let v = VALS[e].load(Acquire);
        if v == 0 || v == NONE || v & RETIRED != 0 {
            continue;
        }
        let i = info(v as usize - 1);
        if i.guest == guest && i.special == 0 {
            if i.nargs > 0 {
                return Some(base + THUNK as u64 * (v - 1));
            }
            any = any.or(Some(base + THUNK as u64 * (v - 1)));
        }
    }
    any
}

/// Retira los trampolines hacia funciones guest en [lo, hi) (biblioteca descargada, ver `RETIRED`). Sin bloqueos.
pub fn retire_range(lo: u64, hi: u64) {
    if ARENA_BASE.load(Acquire) == 0 {
        return;
    }
    for e in 0..MAP {
        let v = VALS[e].load(Acquire);
        if v == 0 || v == NONE || v & RETIRED != 0 {
            continue;
        }
        let g = info(v as usize - 1).guest & crate::interp::TBI_MASK;
        if g >= lo && g < hi {
            let _ = VALS[e].compare_exchange(v, v | RETIRED, AcqRel, Acquire);
        }
    }
}

/// Sin bloqueo: se consulta en cada compilacion de bloque del JIT.
pub fn is_thunk(p: u64) -> bool {
    let b = ARENA_BASE.load(Relaxed);
    b != 0 && p.wrapping_sub(b) < ARENA_BYTES as u64
}

/// Funcion guest que ejecuta el trampolin `p` (None si `p` no es el inicio de un trampolin ya creado).
pub fn guest_of(p: u64) -> Option<u64> {
    let off = p.wrapping_sub(ARENA_BASE.load(Relaxed));
    if !is_thunk(p) || off % THUNK as u64 != 0 {
        return None;
    }
    let i = info((off / THUNK as u64) as usize);
    if i.ready.load(Acquire) == 0 {
        return None;
    }
    Some(i.guest)
}

/// Convierte un descriptor JNI de metodo ("(IJ[Ljava/lang/String;)V") en su shorty ("VIJL"). None si no es valido o
/// tiene demasiados argumentos.
pub fn shorty_of_descriptor(d: &[u8], out: &mut [u8; MAX_SIG_ARGS + 1]) -> Option<usize> {
    let mut i = 1usize;
    if d.first() != Some(&b'(') {
        return None;
    }
    let mut n = 1usize; // out[0] = retorno
    let one = |i: &mut usize| -> Option<u8> {
        let c = *d.get(*i)?;
        match c {
            b'[' => {
                while d.get(*i) == Some(&b'[') {
                    *i += 1;
                }
                if d.get(*i)? == &b'L' {
                    while *d.get(*i)? != b';' {
                        *i += 1;
                    }
                }
                *i += 1;
                Some(b'L')
            }
            b'L' => {
                while *d.get(*i)? != b';' {
                    *i += 1;
                }
                *i += 1;
                Some(b'L')
            }
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' | b'V' => {
                *i += 1;
                Some(c)
            }
            _ => None,
        }
    };
    while *d.get(i)? != b')' {
        let c = one(&mut i)?;
        if c == b'V' || n > MAX_SIG_ARGS {
            return None;
        }
        out[n] = c;
        n += 1;
    }
    i += 1;
    out[0] = one(&mut i)?;
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_a_shorty() {
        let mut o = [0u8; MAX_SIG_ARGS + 1];
        let n = shorty_of_descriptor(b"(IJFDIFDJFDIFDJFDID)Ljava/lang/String;", &mut o).unwrap();
        assert_eq!(&o[..n], b"LIJFDIFDJFDIFDJFDID");
        let n = shorty_of_descriptor(b"([[ILjava/lang/Object;[Ljava/lang/String;Z)V", &mut o).unwrap();
        assert_eq!(&o[..n], b"VLLLZ");
        let n = shorty_of_descriptor(b"()[B", &mut o).unwrap();
        assert_eq!(&o[..n], b"L");
        assert!(shorty_of_descriptor(b"(I", &mut o).is_none());
        assert!(shorty_of_descriptor(b"IJ)V", &mut o).is_none());
    }

    /// El native de banco `mezcla`: static `(IJFDIFDJFDIFDJFDID)Ljava/lang/String;`, registrado con RegisterNatives.
    /// Se construye la llamada exactamente como la haria el host (SysV x86-64) y se comprueba que cada argumento
    /// llega al registro o a la palabra de pila que espera AAPCS64.
    #[test]
    fn reparto_por_firma_de_mezcla() {
        // valores (en orden de la firma) tal como los pasa el llamador
        let env = 0xE000u64;
        let cls = 0xC000u64;
        let f = |x: f32| x.to_bits() as u64 | 0xDEAD_0000_0000_0000; // basura en los 32 bits altos del xmm
        let d = |x: f64| x.to_bits();
        // host SysV: enteros env, cls, a, b, e, i en registros; l, o, r en pila. Flotantes f d g h j k m2 n en xmm;
        // p q s en pila. La pila del host lleva solo los que no caben, en orden de argumento.
        let ints = [env, cls, 1u64 | 0xFFFF_FFFF_0000_0000 /* jint con basura arriba */, 2_000_000_000_000, (-5i32) as u32 as u64, (-8i64) as u64];
        let fps = [f(3.5), d(4.25), f(6.5), d(7.125), f(9.5), d(10.75), f(12.5), d(13.25)];
        // orden de los argumentos que van a la pila del host: l(I) o(J) p(F) q(D) r(I) s(D)
        let host_stack = [11u64, 14, f(15.5), d(16.125), 17, d(18.5)];
        let sig = b"IJFDIFDJFDIFDJFDID";
        let g = unsafe { marshal_sig(&ints, &fps, host_stack.as_ptr(), 2, sig) };
        // AAPCS64: x0..x7 = env cls a b e i l o; v0..v7 = f d g h j k m2 n; pila = p q r s (palabras de 8 bytes)
        assert_eq!(g.x, [env, cls, 1, 2_000_000_000_000, (-5i64) as u64, (-8i64) as u64, 11, 14]);
        assert_eq!(
            g.v,
            [3.5f32.to_bits() as u64, d(4.25), 6.5f32.to_bits() as u64, d(7.125), 9.5f32.to_bits() as u64, d(10.75), 12.5f32.to_bits() as u64, d(13.25)]
        );
        assert_eq!(&g.stack[..g.nstack], &[15.5f32.to_bits() as u64, d(16.125), 17, d(18.5)]);
    }

    /// Mas de 8 flotantes: el noveno va a la pila guest (no a x6, como hacia el reparto universal), y los enteros
    /// que siguen a la pila del host ocupan x6/x7.
    #[test]
    fn reparto_por_firma_con_flotantes_en_pila() {
        let ints = [1u64, 2, 3, 4, 5, 6];
        let fps: [u64; 8] = std::array::from_fn(|i| (i as f64 + 0.5).to_bits());
        // pila del host: 9.o double, 7.o y 8.o entero, 10.o float
        let host_stack = [(8.5f64).to_bits(), 7, 8, (9.5f32).to_bits() as u64];
        let g = unsafe { marshal_sig(&ints, &fps, host_stack.as_ptr(), 0, b"IIIIIIDDDDDDDDDIIF") };
        assert_eq!(g.x, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(g.v, fps);
        assert_eq!(&g.stack[..g.nstack], &[(8.5f64).to_bits(), (9.5f32).to_bits() as u64]);
    }

    /// `movz/movk` de 64 bits en `rd`.
    fn mov64(rd: u32, v: u64) -> [u32; 4] {
        let h = |k: u32| ((v >> (16 * k)) & 0xffff) as u32;
        [0xD280_0000 | rd | h(0) << 5, 0xF2A0_0000 | rd | h(1) << 5, 0xF2C0_0000 | rd | h(2) << 5, 0xF2E0_0000 | rd | h(3) << 5]
    }

    /// De extremo a extremo con la firma de `mezcla`: el host llama al trampolin como una funcion C normal (el
    /// compilador de Rust reparte con SysV) y una funcion guest guarda x0-x7, d0-d7 y las 4 primeras palabras de su
    /// pila. Va como critical (sin JNIEnv*), con dos `J` delante en lugar de env/jclass: mismo reparto.
    #[test]
    fn trampolin_de_mezcla_de_extremo_a_extremo() {
        let mut out = vec![0u64; 20];
        let mut code: Vec<u32> = mov64(9, out.as_mut_ptr() as u64).to_vec();
        code.extend_from_slice(&[
            0xa9000520, 0xa9010d22, 0xa9021524, 0xa9031d26, // stp x0..x7
            0x6d040520, 0x6d050d22, 0x6d061524, 0x6d071d26, // stp d0..d7
            0xa9402fea, 0xa9082d2a, 0xa9412fea, 0xa9092d2a, // pila guest [sp, sp+32)
            0xd2800ee0, 0xd65f03c0, // mov x0, #0x77; ret
        ]);
        let t = make_sig(code.as_ptr() as u64, b"LJJIJFDIFDJFDIFDJFDID", 0);
        assert_ne!(t, 0);
        type Mezcla = extern "C" fn(u64, u64, i32, i64, f32, f64, i32, f32, f64, i64, f32, f64, i32, f32, f64, i64, f32, f64, i32, f64) -> u64;
        let f: Mezcla = unsafe { std::mem::transmute(t as usize) };
        let r = std::thread::spawn(move || f(0xE000, 0xC000, 1, 2_000_000_000_000, 3.5, 4.25, -5, 6.5, 7.125, -8, 9.5, 10.75, 11, 12.5, 13.25, 14, 15.5, 16.125, 17, 18.5))
            .join()
            .unwrap();
        assert_eq!(r, 0x77);
        let fb = |x: f32| x.to_bits() as u64;
        let d = |x: f64| x.to_bits();
        assert_eq!(&out[0..8], &[0xE000, 0xC000, 1, 2_000_000_000_000, (-5i64) as u64, (-8i64) as u64, 11, 14]);
        let lo32 = |w: u64, k: usize| if k % 2 == 0 { w & 0xffff_ffff } else { w };
        let fp: Vec<u64> = (0..8).map(|k| lo32(out[8 + k], k)).collect();
        assert_eq!(fp, vec![fb(3.5), d(4.25), fb(6.5), d(7.125), fb(9.5), d(10.75), fb(12.5), d(13.25)]);
        assert_eq!(out[16] & 0xffff_ffff, fb(15.5));
        assert_eq!(out[17], d(16.125));
        assert_eq!(out[18] & 0xffff_ffff, 17);
        assert_eq!(out[19], d(18.5));
    }

    #[test]
    fn tabla_sin_bloqueos_reutiliza_y_distingue_firmas() {
        let g = 0x7700_1234u64;
        let a = make(g, b'I', 1);
        let b = make_sig(g, b"IJF", 1);
        let c = make_sig(g, b"IJD", 1);
        assert!(a != 0 && b != 0 && c != 0);
        assert!(a != b && b != c);
        assert_eq!(make(g, b'I', 1), a);
        assert_eq!(make_sig(g, b"IJF", 1), b);
        assert_eq!(guest_of(b), Some(g));
        assert_eq!(find_for_guest(g).and_then(guest_of), Some(g));
        assert!(find_for_guest(g) == Some(b) || find_for_guest(g) == Some(c));
        // firma no valida: reparto universal (el mismo trampolin que sin firma)
        assert_eq!(make_sig(g, b"I?", 1), a);
        // concurrencia: muchos hilos piden el mismo trampolin a la vez
        let g2 = 0x7700_5678u64;
        let hs: Vec<_> = (0..8).map(|_| std::thread::spawn(move || make_sig(g2, b"VDDDDDDDDDD", 0))).collect();
        let r: Vec<u64> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(r.iter().all(|&x| x == r[0] && x != 0), "{:x?}", r);
    }
}
