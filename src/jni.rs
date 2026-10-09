//! Capa JNI: JNIEnv/JavaVM del guest son tablas de funciones HLE que reenvian a las del host. Los
//! argumentos escalares (incluidas las funciones variadicas Call*Method) se reenvian con el thunk universal;
//! se convierten solo RegisterNatives (punteros guest -> trampolines), GetJavaVM/GetEnv, y las variantes
//! "V" (va_list AAPCS64 -> jvalue[] segun el shorty del metodo).

use crate::cbthunk;
use crate::cpu::Cpu;
use crate::fmt::{ArgSrc, VaList};
use crate::hle::{self, Ret};
use crate::boundary::{self, HostFn};
use crate::libc_hle::forward;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, Once};

#[repr(C)]
pub struct GuestEnv {
    table: u64,
    host: u64,
}

static TABLE: AtomicU64 = AtomicU64::new(0);
static VM_TABLE: AtomicU64 = AtomicU64::new(0);
static ENVS: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);
static VMS: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);
static INIT: Once = Once::new();
/// NativeBridgeRuntimeCallbacks* proporcionado por el runtime (getMethodShorty en el offset 0)
pub static RUNTIME_CB: AtomicU64 = AtomicU64::new(0);

type ShortyFn = Box<dyn Fn(u64, u64) -> String + Send + Sync>;
static SHORTY_PROVIDER: Mutex<Option<ShortyFn>> = Mutex::new(None);

/// Para pruebas con un JNIEnv simulado.
pub fn set_shorty_provider(f: ShortyFn) {
    *crate::monitor::lock(&SHORTY_PROVIDER) = Some(f);
}

fn method_shorty(henv: u64, mid: u64) -> String {
    if let Some(f) = crate::monitor::lock(&SHORTY_PROVIDER).as_ref() {
        return f(henv, mid);
    }
    let cb = RUNTIME_CB.load(Relaxed);
    if cb == 0 {
        eprintln!("[heddle] sin getMethodShorty: no se pueden convertir llamadas JNI con va_list");
        return String::new();
    }
    let Some(f) = HostFn::from_host(unsafe { *(cb as *const u64) }) else {
        eprintln!("[heddle] getMethodShorty del runtime no es codigo del host");
        return String::new();
    };
    let (r, _, _) = boundary::call_ints(f, [henv, mid, 0, 0, 0, 0]);
    if r == 0 {
        return String::new();
    }
    unsafe { std::ffi::CStr::from_ptr(r as *const std::os::raw::c_char).to_string_lossy().into_owned() }
}

/// Funcion `idx` de la tabla del JNIEnv/JavaVM del host `henv`. Se valida que sea codigo del host: si `henv` no es
/// un entorno real (puntero filtrado, envoltorio sin desenvolver) se aborta con diagnostico en vez de saltar.
fn host_fn(c: &Cpu, henv: u64, idx: usize) -> HostFn {
    // un envoltorio guest sin desenvolver tiene como tabla slots HLE: from_host lo rechaza (no se salta a ellos)
    let f = if henv < 0x10000 {
        0
    } else {
        unsafe {
            let tbl = *(henv as *const u64);
            if tbl < 0x10000 {
                0
            } else {
                *((tbl + 8 * idx as u64) as *const u64)
            }
        }
    };
    match HostFn::from_host(f) {
        Some(h) => h,
        None => {
            crate::bridge::alog_fatal(&format!("JNI#{}: entorno invalido x0={:#x} host={:#x} fn={:#x}\n{}", idx, c.x[0], henv, f, crate::rt::dump_backtrace(c)));
            unsafe { crate::sys::abort() }
        }
    }
}

pub fn ensure_tables() {
    INIT.call_once(build_tables);
}

/// Los envoltorios guest (JNIEnv/JavaVM) viven en una arena propia: reconocerlos es una comparacion de rango,
/// sin leer memoria (los argumentos de cualquier llamada pueden ser enteros arbitrarios).
const WRAP_ARENA: usize = 1 << 20;
static WRAP_BASE: AtomicU64 = AtomicU64::new(0);
static WRAP_USED: AtomicU64 = AtomicU64::new(0);

fn alloc_wrapper(table: u64, host: u64) -> u64 {
    use crate::sys::*;
    // una sola arena aunque dos hilos lleguen a la vez (JNIEnv y JavaVM usan bloqueos distintos)
    static ONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let base = *ONCE.get_or_init(|| {
        let b = crate::mem::Region::permanent(WRAP_ARENA, PROT_READ | PROT_WRITE).expect("no se pudo reservar la arena de envoltorios JNI");
        WRAP_BASE.store(b, Relaxed);
        b
    });
    let off = WRAP_USED.fetch_add(16, Relaxed);
    assert!((off as usize) + 16 <= WRAP_ARENA, "arena de envoltorios JNI agotada");
    let a = base + off;
    unsafe { *(a as *mut GuestEnv) = GuestEnv { table, host } };
    a
}

/// Rango [base, base+usado) de la arena de envoltorios (para comprobar muchos valores con una sola lectura).
#[inline]
pub fn wrapper_range() -> (u64, u64) {
    (WRAP_BASE.load(Relaxed), WRAP_USED.load(Relaxed))
}

/// `p` es un envoltorio guest (JNIEnv o JavaVM del puente)?
pub fn is_guest_wrapper(p: u64) -> bool {
    let b = WRAP_BASE.load(Relaxed);
    b != 0 && p.wrapping_sub(b) < WRAP_USED.load(Relaxed) && p & 15 == 0
}

pub fn guest_env_for(host: u64) -> u64 {
    if host == 0 {
        return 0;
    }
    ensure_tables();
    if is_guest_wrapper(host) {
        return host; // ya es un JNIEnv guest: no volver a envolverlo
    }
    let mut g = crate::monitor::lock(&ENVS);
    let m = g.get_or_insert_with(HashMap::new);
    *m.entry(host).or_insert_with(|| alloc_wrapper(TABLE.load(Relaxed), host))
}

pub fn host_env_of(guest: u64) -> u64 {
    // Solo un envoltorio del puente (rango de la arena) se desenvuelve: nunca se lee memoria de un valor cualquiera.
    if !is_guest_wrapper(guest) {
        return 0;
    }
    // Desenvuelve tantas capas del puente como haya (un envoltorio guest apuntando a otro).
    let mut p = unsafe { *((guest + 8) as *const u64) };
    for _ in 0..4 {
        if !is_guest_wrapper(p) {
            break;
        }
        p = unsafe { *((p + 8) as *const u64) };
    }
    p
}

/// Si `v` es un JNIEnv/JavaVM guest (envoltorio del puente) devuelve el puntero real del host; si no, `v`.
pub fn unwrap_to_host(v: u64) -> u64 {
    if is_guest_wrapper(v) {
        unsafe { *((v + 8) as *const u64) }
    } else {
        v
    }
}

pub fn guest_vm_for(host: u64) -> u64 {
    if host == 0 {
        return 0;
    }
    ensure_tables();
    if is_guest_wrapper(host) {
        return host;
    }
    let mut g = crate::monitor::lock(&VMS);
    let m = g.get_or_insert_with(HashMap::new);
    *m.entry(host).or_insert_with(|| alloc_wrapper(VM_TABLE.load(Relaxed), host))
}

fn ret_char(sig: &str) -> u8 {
    match sig.rfind(')') {
        Some(p) => sig.as_bytes().get(p + 1).copied().unwrap_or(b'V'),
        None => b'V',
    }
}

fn shorty_args(shorty: &str, a: &mut dyn ArgSrc) -> Vec<u64> {
    let mut jv = Vec::new();
    for ch in shorty.bytes().skip(1) {
        jv.push(match ch {
            b'F' => (f64::from_bits(a.fp()) as f32).to_bits() as u64,
            b'D' => a.fp(),
            b'Z' => a.int() & 0xff,
            b'B' => a.int() as i8 as i64 as u64 & 0xff,
            b'C' => a.int() & 0xffff,
            b'S' => a.int() as i16 as u16 as u64,
            b'I' => a.int() as u32 as u64,
            _ => a.int(),
        });
    }
    jv
}

fn build_tables() {
    hle::init();
    const N: usize = 233;
    let mut t: Vec<u64> = vec![0; N];
    for i in 4..N {
        let a = hle::register(
            &format!("JNIEnv#{}", i),
            Box::new(move |c: &mut Cpu| {
                let h = host_env_of(c.x[0]);
                let f = host_fn(c, h, i);
                forward(c, f, Some(h));
                Ret::Return
            }),
        );
        t[i] = a;
    }
    // variantes V: (env, obj|clazz, [clazz], mid, va_list)  ->  version A con jvalue[]
    for (base, nonvirtual) in [(34usize, false), (64, true), (114, false)] {
        for k in 0..10 {
            let v = base + 3 * k + 1;
            let ai = base + 3 * k + 2;
            t[v] = hle::register(
                &format!("JNIEnv#{}V", v),
                Box::new(move |c: &mut Cpu| {
                    call_v(c, ai, nonvirtual);
                    Ret::Return
                }),
            );
        }
    }
    t[29] = hle::register(
        "JNIEnv#NewObjectV",
        Box::new(|c: &mut Cpu| {
            call_v(c, 30, false);
            Ret::Return
        }),
    );
    t[215] = hle::register(
        "JNIEnv#RegisterNatives",
        Box::new(|c: &mut Cpu| {
            let h = host_env_of(c.x[0]);
            let (clazz, methods, n) = (c.x[1], c.x[2], c.x[3] as i32);
            let mut copy: Vec<[u64; 3]> = Vec::new();
            for i in 0..n.max(0) as u64 {
                let p = (methods + 24 * i) as *const u64;
                let (name, sig, f) = unsafe { (*p, *p.add(1), *p.add(2)) };
                let sg = unsafe { std::ffi::CStr::from_ptr(sig as *const std::os::raw::c_char).to_string_lossy().into_owned() };
                let nm = unsafe { std::ffi::CStr::from_ptr(name as *const std::os::raw::c_char).to_string_lossy().into_owned() };
                // la firma del descriptor da el reparto de argumentos del trampolin (mas de 8 FP, FP en pila...)
                let mut sh = [0u8; crate::cbthunk::MAX_SIG_ARGS + 1];
                let th = match crate::cbthunk::shorty_of_descriptor(sg.as_bytes(), &mut sh) {
                    Some(n) => boundary::to_host_callable_sig(f, &sh[..n], 1),
                    None => boundary::to_host_callable(f, ret_char(&sg), 1),
                };
                crate::bridge::alog(&format!("RegisterNatives {}{} fn={} hle={}", nm, sg, crate::elf::describe_addr(f), hle::is_hle(f)));
                copy.push([name, sig, th]);
            }
            c.x[0] = boundary::call_ints(host_fn(c, h, 215), [h, clazz, copy.as_ptr() as u64, n as u64, 0, 0]).0;
            Ret::Return
        }),
    );
    t[219] = hle::register(
        "JNIEnv#GetJavaVM",
        Box::new(|c: &mut Cpu| {
            let h = host_env_of(c.x[0]);
            let out = c.x[1];
            let r = boundary::call_ints(host_fn(c, h, 219), [h, out, 0, 0, 0, 0]).0;
            if r == 0 {
                unsafe { *(out as *mut u64) = guest_vm_for(*(out as *const u64)) };
            }
            c.x[0] = r;
            Ret::Return
        }),
    );
    let tab: &'static mut [u64] = Box::leak(t.into_boxed_slice());
    TABLE.store(tab.as_ptr() as u64, Relaxed);

    // JavaVM: DestroyJavaVM, AttachCurrentThread, DetachCurrentThread, GetEnv, AttachCurrentThreadAsDaemon
    let mut vt: Vec<u64> = vec![0; 8];
    for i in 3..8usize {
        vt[i] = hle::register(
            &format!("JavaVM#{}", i),
            Box::new(move |c: &mut Cpu| {
                let hv = host_env_of(c.x[0]);
                let out = c.x[1];
                let f = host_fn(c, hv, i);
                forward(c, f, Some(hv));
                if (i == 4 || i == 6 || i == 7) && c.x[0] as i32 == 0 && out != 0 {
                    unsafe {
                        let e = *(out as *const u64);
                        if e != 0 {
                            *(out as *mut u64) = guest_env_for(e);
                        }
                    }
                }
                Ret::Return
            }),
        );
    }
    let vtab: &'static mut [u64] = Box::leak(vt.into_boxed_slice());
    VM_TABLE.store(vtab.as_ptr() as u64, Relaxed);
}

fn call_v(c: &mut Cpu, a_index: usize, nonvirtual: bool) {
    let henv = host_env_of(c.x[0]);
    let (obj, clazz, mid, va) = if nonvirtual { (c.x[1], c.x[2], c.x[3], c.x[4]) } else { (c.x[1], 0, c.x[2], c.x[3]) };
    let shorty = method_shorty(henv, mid);
    let mut a = VaList::new(va);
    let jv = shorty_args(&shorty, &mut a);
    let ints = [henv, obj, if nonvirtual { clazz } else { mid }, if nonvirtual { mid } else { jv.as_ptr() as u64 }, if nonvirtual { jv.as_ptr() as u64 } else { 0 }, 0];
    let (rax, rdx, xmm0) = boundary::call_ints(host_fn(c, henv, a_index), ints);
    c.x[0] = rax;
    c.x[1] = rdx;
    c.v[0] = [xmm0, 0];
}
