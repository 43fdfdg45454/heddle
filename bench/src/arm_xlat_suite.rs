// Suite unificada: traduccion ARM64 -> x86-64 con equivalencia funcional estricta.
//
//   1. Pruebas de equivalencia (flags, monitor exclusivo, SIMD). Lo que falla no se mide.
//   2. Costos, cada fila con la columna "ARM ns" (version nativa ARM, ver arm_native.c) y "x/ARM".
//   3. Monitor: armado, ENFRIAMIENTO de granulos, modo FALLBACK, abortos de rseq, estres.
//
// Las columnas ARM se leen de un archivo (por defecto arm_results.txt) generado por arm_native.c
// en un equipo ARM64 real. Sin archivo muestran n/d. IMPORTANTE: x/ARM compara dos maquinas
// distintas si el archivo viene de otro equipo: mezcla velocidad de CPU y sobrecosto de traduccion.
// La columna "vs base" (misma maquina, mismo binario) es la que mide el sobrecosto puro.
//
// Compilar: rustc --edition 2021 -C opt-level=3 arm_xlat_suite.rs -o arm_xlat_suite
// Uso:      ./arm_xlat_suite [--arm arm_results.txt]
// Requiere Linux x86-64 con glibc >= 2.35 (rseq registrado) y membarrier RSEQ (kernel >= 5.10);
// si no estan, la suite cae sola al modo FALLBACK (todos los stores instrumentados).

use std::arch::asm;
use std::arch::x86_64::*;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::hint::{black_box, spin_loop};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn syscall(num: i64, ...) -> i64;
    static __rseq_offset: isize;
}

// ===========================================================================
// Referencia ARM (archivo generado por arm_native.c)
// ===========================================================================

static ARM: OnceLock<HashMap<String, f64>> = OnceLock::new();

fn load_arm(path: &str) -> usize {
    let mut m = HashMap::new();
    if let Ok(s) = std::fs::read_to_string(path) {
        for l in s.lines() {
            let mut it = l.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                if let Ok(f) = v.parse::<f64>() {
                    m.insert(k.to_string(), f);
                }
            }
        }
    }
    let n = m.len();
    let _ = ARM.set(m);
    n
}

fn arm(key: &str) -> Option<f64> {
    ARM.get().and_then(|m| m.get(key).copied())
}

fn header(title: &str) {
    println!("--- {} ---", title);
    println!(
        "{:<44} {:>13} {:>8} {:>8} {:>8} {:>8}",
        "variante", "ops/s", "ns/op", "vs base", "ARM ns", "x/ARM"
    );
}

fn row(name: &str, ns: f64, base_ns: f64, key: &str) {
    let (a, x) = match arm(key) {
        Some(a) => (format!("{:.2}", a), format!("{:.1}x", ns / a)),
        None => ("n/d".to_string(), "n/d".to_string()),
    };
    println!(
        "{:<44} {:>13.0} {:>8.2} {:>7.2}x {:>8} {:>8}",
        name,
        1e9 / ns,
        ns,
        ns / base_ns,
        a,
        x
    );
}

struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

fn verdict(b: bool) -> &'static str {
    if b {
        "PASA"
    } else {
        "FALLA"
    }
}

// ===========================================================================
// membarrier / rseq
// ===========================================================================

const SYS_MEMBARRIER: i64 = 324;
const MB_REGISTER_PRIVATE_EXPEDITED_RSEQ: i64 = 256;
const MB_PRIVATE_EXPEDITED_RSEQ: i64 = 128;

fn membarrier_register() -> bool {
    unsafe { syscall(SYS_MEMBARRIER, MB_REGISTER_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) == 0 }
}

static MEMBARRIERS: AtomicU64 = AtomicU64::new(0);

fn membarrier_rseq() {
    MEMBARRIERS.fetch_add(1, Relaxed);
    let t = Instant::now();
    let r = unsafe { syscall(SYS_MEMBARRIER, MB_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) };
    MB_NS.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    assert!(r == 0, "membarrier(PRIVATE_EXPEDITED_RSEQ) fallo");
}

fn rseq_area() -> *mut u8 {
    unsafe {
        let tp: usize;
        asm!("mov {}, fs:[0]", out(reg) tp, options(nostack, preserves_flags, readonly));
        (tp as isize + __rseq_offset) as *mut u8
    }
}

fn rseq_registered() -> bool {
    unsafe { std::ptr::read_volatile(rseq_area().add(4) as *const u32) != u32::MAX }
}

// Modo fallback: todos los stores pasan por lock+version, sin rseq ni membarrier.
static FALLBACK: AtomicBool = AtomicBool::new(false);

// ===========================================================================
// A. FLAGS
// ===========================================================================

#[derive(Clone, Copy)]
enum Cond {
    Hs,
    Gt,
}

struct Cpu {
    x: [u64; 8],
    nzcv: u8,
    lazy_a: u64,
    lazy_b: u64,
}

fn new_cpu() -> Cpu {
    Cpu { x: [0; 8], nzcv: 0, lazy_a: 0, lazy_b: 0 }
}

#[inline(never)]
fn subs_eager(cpu: &mut Cpu, rd: usize, rn: usize, rm: usize) {
    let a = cpu.x[rn];
    let b = cpu.x[rm];
    let r = a.wrapping_sub(b);
    cpu.x[rd] = r;
    let n = (r >> 63) as u8;
    let z = (r == 0) as u8;
    let c = (a >= b) as u8;
    let v = (((a ^ b) & (a ^ r)) >> 63) as u8;
    cpu.nzcv = (n << 3) | (z << 2) | (c << 1) | v;
}

#[inline(never)]
fn csel_eager(cpu: &mut Cpu, rd: usize, rn: usize, rm: usize, cond: Cond) {
    let f = cpu.nzcv;
    let (n, z, c, v) = ((f >> 3) & 1, (f >> 2) & 1, (f >> 1) & 1, f & 1);
    let take = match cond {
        Cond::Hs => c == 1,
        Cond::Gt => z == 0 && n == v,
    };
    cpu.x[rd] = if take { cpu.x[rn] } else { cpu.x[rm] };
}

#[inline(never)]
fn subs_lazy(cpu: &mut Cpu, rd: usize, rn: usize, rm: usize) {
    let a = cpu.x[rn];
    let b = cpu.x[rm];
    cpu.x[rd] = a.wrapping_sub(b);
    cpu.lazy_a = a;
    cpu.lazy_b = b;
}

#[inline(never)]
fn csel_lazy(cpu: &mut Cpu, rd: usize, rn: usize, rm: usize, cond: Cond) {
    let (a, b) = (cpu.lazy_a, cpu.lazy_b);
    let take = match cond {
        Cond::Hs => a >= b,
        Cond::Gt => (a as i64) > (b as i64),
    };
    cpu.x[rd] = if take { cpu.x[rn] } else { cpu.x[rm] };
}

#[inline(never)]
fn acc_update(cpu: &mut Cpu) {
    cpu.x[5] = cpu.x[5].wrapping_add(cpu.x[3]) ^ cpu.x[4];
}

fn block_eager(cpu: &mut Cpu) {
    subs_eager(cpu, 2, 0, 1);
    csel_eager(cpu, 3, 0, 1, Cond::Hs);
    csel_eager(cpu, 4, 0, 1, Cond::Gt);
    acc_update(cpu);
}

fn block_lazy(cpu: &mut Cpu) {
    subs_lazy(cpu, 2, 0, 1);
    csel_lazy(cpu, 3, 0, 1, Cond::Hs);
    csel_lazy(cpu, 4, 0, 1, Cond::Gt);
    acc_update(cpu);
}

#[inline(never)]
fn block_fused_nostate(cpu: &mut Cpu) {
    let a = cpu.x[0];
    let b = cpu.x[1];
    cpu.x[2] = a.wrapping_sub(b);
    cpu.x[3] = if a >= b { a } else { b };
    cpu.x[4] = if (a as i64) > (b as i64) { a } else { b };
    cpu.x[5] = cpu.x[5].wrapping_add(cpu.x[3]) ^ cpu.x[4];
}

#[inline(never)]
fn block_fused_state(cpu: &mut Cpu) {
    let a = cpu.x[0];
    let b = cpu.x[1];
    cpu.x[2] = a.wrapping_sub(b);
    cpu.lazy_a = a;
    cpu.lazy_b = b;
    cpu.x[3] = if a >= b { a } else { b };
    cpu.x[4] = if (a as i64) > (b as i64) { a } else { b };
    cpu.x[5] = cpu.x[5].wrapping_add(cpu.x[3]) ^ cpu.x[4];
}

fn cond_mask(n: u8, z: u8, c: u8, v: u8) -> u16 {
    let conds = [
        z == 1,
        z == 0,
        c == 1,
        c == 0,
        n == 1,
        n == 0,
        v == 1,
        v == 0,
        c == 1 && z == 0,
        !(c == 1 && z == 0),
        n == v,
        n != v,
        z == 0 && n == v,
        z == 1 || n != v,
    ];
    conds.iter().enumerate().fold(0u16, |m, (i, &b)| m | ((b as u16) << i))
}

fn mask_from_eager(f: u8) -> u16 {
    cond_mask((f >> 3) & 1, (f >> 2) & 1, (f >> 1) & 1, f & 1)
}

fn mask_from_lazy(a: u64, b: u64) -> u16 {
    let r = a.wrapping_sub(b);
    cond_mask(
        (r >> 63) as u8,
        (r == 0) as u8,
        (a >= b) as u8,
        ((((a ^ b) & (a ^ r)) >> 63) & 1) as u8,
    )
}

fn flags_equivalent(block: fn(&mut Cpu), from_lazy: bool) -> bool {
    let edges: [u64; 8] = [0, 1, 2, u64::MAX, u64::MAX - 1, 1 << 63, (1 << 63) - 1, (1 << 63) + 1];
    let mut rng = XorShift(0x1234_5678_9ABC_DEF1);
    let mut reference = new_cpu();
    let mut test = new_cpu();
    for _ in 0..300_000 {
        let mut a = rng.next();
        let mut b = rng.next();
        match rng.next() & 7 {
            0 => a = edges[(rng.next() & 7) as usize],
            1 => b = edges[(rng.next() & 7) as usize],
            2 => b = a,
            3 => {
                a = edges[(rng.next() & 7) as usize];
                b = edges[(rng.next() & 7) as usize];
            }
            _ => {}
        }
        for cpu in [&mut reference, &mut test] {
            cpu.x[0] = a;
            cpu.x[1] = b;
        }
        block_eager(&mut reference);
        block(&mut test);
        if reference.x[..6] != test.x[..6] {
            return false;
        }
        let mr = mask_from_eager(reference.nzcv);
        let mt = if from_lazy {
            mask_from_lazy(test.lazy_a, test.lazy_b)
        } else {
            mask_from_eager(test.nzcv)
        };
        if mr != mt {
            return false;
        }
    }
    true
}

// ns por iteracion (2 xorshift + bloque de 4 instrucciones guest)
fn run_flags(block: fn(&mut Cpu), dur: Duration) -> f64 {
    let mut cpu = new_cpu();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let t0 = Instant::now();
    let deadline = t0 + dur;
    let mut iters = 0u64;
    loop {
        for _ in 0..1024 {
            cpu.x[0] = rng.next();
            cpu.x[1] = rng.next();
            block(&mut cpu);
        }
        iters += 1024;
        if Instant::now() >= deadline {
            break;
        }
    }
    black_box(cpu.x[5]);
    t0.elapsed().as_secs_f64() * 1e9 / iters as f64
}

// ===========================================================================
// Monitor exclusivo: granulos lock+version, calientes/frios, enfriamiento
// ===========================================================================

const NGRAN: usize = 1024;

#[repr(align(64))]
struct Granule {
    lock: AtomicBool,
    ver: AtomicU64,
}

static GRAN: [Granule; NGRAN] =
    [const { Granule { lock: AtomicBool::new(false), ver: AtomicU64::new(0) } }; NGRAN];

fn gran(cell: &AtomicU64) -> &'static Granule {
    &GRAN[((cell as *const AtomicU64 as usize) >> 6) & (NGRAN - 1)]
}

fn glock(g: &Granule) {
    while g
        .lock
        .compare_exchange_weak(false, true, Acquire, Relaxed)
        .is_err()
    {
        spin_loop();
    }
}

fn gunlock(g: &Granule) {
    g.lock.store(false, Release);
}

struct Mon {
    valid: bool,
    val: u64,
    ver: u64,
    epoch: u64,
}

impl Mon {
    fn new() -> Mon {
        Mon { valid: false, val: 0, ver: 0, epoch: 0 }
    }
}

// Epoca global: cada enfriamiento la incrementa e INVALIDA todas las reservas pendientes
// (un fallo espurio de STXR esta permitido por ARM; lo que no se permite es un exito indebido).
static EPOCH: AtomicU64 = AtomicU64::new(1);

trait Mem {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64;
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool;
    fn store(cell: &AtomicU64, v: u64);
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64;
}

fn generic_add<M: Mem>(cell: &AtomicU64, imm: u64) -> u64 {
    let mut mon = Mon::new();
    loop {
        let v = M::ldxr(&mut mon, cell).wrapping_add(imm);
        if M::stxr(&mut mon, cell, v) {
            return v;
        }
    }
}

// Referencia NO estricta: ldxr = load, stxr = lock cmpxchg por valor (problema ABA).
struct Cmpxchg;
impl Mem for Cmpxchg {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        mon.val = cell.load(Acquire);
        mon.valid = true;
        mon.val
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        let ok = mon.valid && cell.compare_exchange(mon.val, val, AcqRel, Relaxed).is_ok();
        mon.valid = false;
        ok
    }
    fn store(cell: &AtomicU64, v: u64) {
        cell.store(v, Release);
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        generic_add::<Self>(cell, imm)
    }
}

struct Versioned;
impl Versioned {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64, epoch: u64) -> u64 {
        let g = gran(cell);
        mon.epoch = epoch;
        mon.ver = g.ver.load(Acquire);
        mon.val = cell.load(Acquire);
        mon.valid = true;
        mon.val
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        let g = gran(cell);
        glock(g);
        let ok = mon.valid && g.ver.load(Relaxed) == mon.ver && EPOCH.load(Acquire) == mon.epoch;
        if ok {
            cell.store(val, Release);
            g.ver.store(mon.ver.wrapping_add(1), Release);
        }
        gunlock(g);
        mon.valid = false;
        ok
    }
    fn slow_store(cell: &AtomicU64, v: u64) {
        let g = gran(cell);
        glock(g);
        cell.store(v, Release);
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
        gunlock(g);
    }
    fn locked_add(cell: &AtomicU64, imm: u64) -> u64 {
        let g = gran(cell);
        glock(g);
        let v = cell.load(Relaxed).wrapping_add(imm);
        cell.store(v, Release);
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
        gunlock(g);
        v
    }
}

// Todos los stores instrumentados (~50x en stores; es tambien el modo FALLBACK).
struct Fiel;
impl Mem for Fiel {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        Versioned::ldxr(mon, cell, EPOCH.load(Acquire))
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        Versioned::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        Versioned::slow_store(cell, v)
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        Versioned::locked_add(cell, imm)
    }
}

// ---- tabla de granulos calientes ----
// Entrada: byte bajo = estado (0 frio, 1 armando, 2 caliente); 24 bits altos = sello de epoca.
const HOT_BITS: usize = 16;
static HOT: [AtomicU32; 1 << HOT_BITS] = [const { AtomicU32::new(0) }; 1 << HOT_BITS];
static ARM_LOCK: AtomicBool = AtomicBool::new(false);
static HOT_COUNT: AtomicUsize = AtomicUsize::new(0);
const DEFAULT_THRESH: usize = 49152; // 3/4 de la tabla
static COOL_THRESH: AtomicUsize = AtomicUsize::new(DEFAULT_THRESH);
static COOLS: AtomicU64 = AtomicU64::new(0);

struct HotList(UnsafeCell<Vec<u32>>);
unsafe impl Sync for HotList {}
static HOT_LIST: HotList = HotList(UnsafeCell::new(Vec::new())); // protegida por ARM_LOCK

fn hot_idx(addr: usize) -> usize {
    (addr >> 6) & ((1 << HOT_BITS) - 1)
}

fn stamp_of(e: u64) -> u32 {
    ((e as u32) & 0x00FF_FFFF) << 8
}

fn arm_lock() {
    while ARM_LOCK
        .compare_exchange_weak(false, true, Acquire, Relaxed)
        .is_err()
    {
        spin_loop();
    }
}

// Enfria granulos calientes. Requiere ARM_LOCK.
//  1) sube la epoca: toda reserva pendiente falla (no puede haber exito indebido despues);
//  2) para cada entrada vieja toma el lock del granulo (espera a un STXR/idioma en vuelo, que ya
//     habia pasado el chequeo de epoca) y la pone en 0 con CAS (si otro hilo la re-sello, queda).
// aggr=true: enfria todo lo no usado desde el incremento de epoca.
// aggr=false: conserva ademas lo usado en la epoca anterior (dos generaciones).
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

fn cool() -> usize {
    arm_lock();
    let n = cool_locked(true);
    ARM_LOCK.store(false, Release);
    n
}

// Solo para pruebas (sin otros hilos con reservas vivas): deja la tabla completamente fria.
fn reset_hot() {
    arm_lock();
    if SATURATED.load(Acquire) {
        EPOCH.fetch_add(1, SeqCst);
        for h in HOT.iter() {
            h.store(0, Release);
        }
        unsafe { (*HOT_LIST.0.get()).clear() };
        HOT_COUNT.store(0, Relaxed);
        SATURATED.store(false, Release);
        unsafe { *COOL_HIST.0.get() = [None; 3] };
    } else {
        cool_locked(true);
    }
    ARM_LOCK.store(false, Release);
}

// Saturacion: si armar granulos nuevos cuesta demasiado tiempo (conjunto de trabajo mayor que la
// tabla, o rotacion constante), TODA la tabla pasa a caliente. Usa el mismo protocolo que el armado
// (0 -> 1, un membarrier, 1 -> 2), asi que es igual de seguro; desde entonces todos los stores
// van por lock+version (modo "todos instrumentados"), sin mas armado ni enfriamiento.
// Requiere ARM_LOCK.
fn saturate_locked<const BARRIER: bool>() {
    for h in HOT.iter() {
        if h.load(Relaxed) & 0xFF == 0 {
            h.store(1, Relaxed);
        }
    }
    std::sync::atomic::fence(SeqCst);
    if BARRIER {
        membarrier_rseq();
    }
    let st = stamp_of(EPOCH.load(Acquire)) | 2;
    for h in HOT.iter() {
        if h.load(Relaxed) & 0xFF == 1 {
            h.store(st, Release);
        }
    }
    SATURATED.store(true, SeqCst);
    SATURATIONS.fetch_add(1, Relaxed);
}

static SATURATED: AtomicBool = AtomicBool::new(false);
static SATURATIONS: AtomicU64 = AtomicU64::new(0);
static MB_NS: AtomicU64 = AtomicU64::new(0); // tiempo acumulado dentro de membarrier
// Historial de los ultimos 3 enfriamientos provocados por falta de espacio (bajo ARM_LOCK).
// Saturar solo si hay presion SOSTENIDA: 3 enfriamientos por umbral seguidos y, en ese lapso,
// mas del 25 % del tiempo dentro de membarrier. Una rafaga de armados al arrancar (sin llegar
// al umbral) no satura.
struct CoolHist(UnsafeCell<[Option<(Instant, u64)>; 3]>);
unsafe impl Sync for CoolHist {}
static COOL_HIST: CoolHist = CoolHist(UnsafeCell::new([None; 3]));
const SAT_FRACTION: f64 = 0.25;

fn note_pressure_cool<const B: bool>() {
    let h = unsafe { &mut *COOL_HIST.0.get() };
    let now = Instant::now();
    let mb = MB_NS.load(Relaxed);
    if let Some((t0, mb0)) = h[0] {
        let dt = now.duration_since(t0).as_nanos() as f64;
        if B && dt > 0.0 && (mb - mb0) as f64 / dt > SAT_FRACTION {
            saturate_locked::<B>();
            *h = [None; 3];
            return;
        }
    }
    *h = [h[1], h[2], Some((now, mb))];
}

// Garantiza que el granulo este caliente y sellado con la epoca `e`. BARRIER=false omite
// membarrier (variante insegura, solo para mostrar que las pruebas no la distinguen).
fn ensure_hot<const BARRIER: bool>(cell: &AtomicU64, e: u64) {
    let h = &HOT[hot_idx(cell as *const AtomicU64 as usize)];
    let want = stamp_of(e) | 2;
    loop {
        if SATURATED.load(Acquire) {
            return;
        }
        let cur = h.load(Acquire);
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
                note_pressure_cool::<BARRIER>();
                if SATURATED.load(Acquire) {
                    ARM_LOCK.store(false, Release);
                    return;
                }
            }
            h.store(1, SeqCst);
            if BARRIER {
                membarrier_rseq();
            }
            h.store(stamp_of(EPOCH.load(Acquire)) | 2, Release);
            unsafe { (*HOT_LIST.0.get()).push(hot_idx(cell as *const AtomicU64 as usize) as u32) };
            HOT_COUNT.fetch_add(1, Relaxed);
        }
        ARM_LOCK.store(false, Release);
    }
}

static RSEQ_ABORTS: AtomicU64 = AtomicU64::new(0);

// Store rapido sobre granulo frio dentro de una seccion critica rseq.
// El kernel la reinicia (etiqueta 5) si hay preemption, senal, migracion o membarrier RSEQ.
// En cada aborto cuenta en RSEQ_ABORTS y repite desde el principio (vuelve a mirar el flag).
#[inline(always)]
fn hot_store_fast(cell: &AtomicU64, v: u64, rs: *mut u8) -> bool {
    let addr = cell as *const AtomicU64 as *mut u64;
    let flag = &HOT[hot_idx(addr as usize)] as *const AtomicU32 as *const u8; // byte bajo = estado
    let r: u64;
    unsafe {
        asm!(
            "2:",
            "leaq 8f(%rip), %rax",
            "movq %rax, 8({rs})",
            "3:",
            "cmpb $0, ({flag})",
            "jne 6f",
            "movq {val}, ({addr})",
            "4:",
            "xorl {r:e}, {r:e}",
            "jmp 7f",
            ".long 0x53053053",
            "5:",
            "lock incq {ab}(%rip)",
            "jmp 2b",
            "6:",
            "movl $1, {r:e}",
            "7:",
            ".pushsection .data.rel.ro, \"aw\"",
            ".balign 32",
            "8:",
            ".long 0, 0",
            ".quad 3b",
            ".quad 4b - 3b",
            ".quad 5b",
            ".popsection",
            rs = in(reg) rs,
            flag = in(reg) flag,
            val = in(reg) v,
            addr = in(reg) addr,
            r = out(reg) r,
            ab = sym RSEQ_ABORTS,
            out("rax") _,
            options(att_syntax, nostack)
        );
    }
    r == 0
}

struct HotMem<const BARRIER: bool>;
impl<const B: bool> Mem for HotMem<B> {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        if FALLBACK.load(Relaxed) {
            return Versioned::ldxr(mon, cell, EPOCH.load(Acquire));
        }
        loop {
            let e = EPOCH.load(Acquire);
            ensure_hot::<B>(cell, e);
            let v = Versioned::ldxr(mon, cell, e);
            if EPOCH.load(Acquire) == e {
                return v;
            }
        }
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        Versioned::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        if FALLBACK.load(Relaxed) || !hot_store_fast(cell, v, rseq_area()) {
            Versioned::slow_store(cell, v);
        }
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        if FALLBACK.load(Relaxed) {
            return Versioned::locked_add(cell, imm);
        }
        let h = &HOT[hot_idx(cell as *const AtomicU64 as usize)];
        loop {
            let e = EPOCH.load(Acquire);
            ensure_hot::<B>(cell, e);
            let g = gran(cell);
            glock(g);
            // bajo el lock: si no hubo enfriamiento desde `e` y el granulo sigue caliente, los
            // stores normales de los demas pasan por este mismo lock => el RMW es atomico.
            if EPOCH.load(Acquire) == e && h.load(Acquire) & 0xFF == 2 {
                let v = cell.load(Relaxed).wrapping_add(imm);
                cell.store(v, Release);
                g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
                gunlock(g);
                return v;
            }
            gunlock(g);
        }
    }
}

// Variante de prueba: fuerza un enfriamiento ANTES de cada store/idioma, es decir justo entre
// el LDXR y el STXR de las pruebas deterministas.
struct HotCool;
impl Mem for HotCool {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        HotMem::<true>::ldxr(mon, cell)
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        HotMem::<true>::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        cool();
        HotMem::<true>::store(cell, v)
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        cool();
        HotMem::<true>::idiom_add(cell, imm)
    }
}

// ===========================================================================
// Pruebas deterministas de equivalencia (dos "cores" intercalados a mano)
// ===========================================================================

const MON_TESTS: [&str; 7] = ["E1", "E2", "E3", "E4", "E5", "E6", "E7"];

fn monitor_tests<M: Mem>() -> Vec<bool> {
    let mut res = Vec::new();
    {
        let c = AtomicU64::new(100); // E1: A -> B -> A con stores normales
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        let c = AtomicU64::new(100); // E2: store con otro valor
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        let c = AtomicU64::new(100); // E3: store con el MISMO valor
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        let c = AtomicU64::new(100); // E4: otro core completa su par ldxr/stxr
        let (mut m1, mut m2) = (Mon::new(), Mon::new());
        M::ldxr(&mut m1, &c);
        M::ldxr(&mut m2, &c);
        let ok2 = M::stxr(&mut m2, &c, 101);
        let ok1 = M::stxr(&mut m1, &c, 101);
        res.push(ok2 && !ok1);
    }
    {
        let c = AtomicU64::new(100); // E5: otro core ejecuta el idioma (+1)
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        let c = AtomicU64::new(100); // E6: el idioma sube y baja (A -> A+1 -> A)
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        M::idiom_add(&c, u64::MAX);
        res.push(c.load(SeqCst) == 100 && !M::stxr(&mut m, &c, 101));
    }
    {
        let c = AtomicU64::new(100); // E7: sin interferencia el stxr tiene exito
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        let ok = M::stxr(&mut m, &c, 101);
        res.push(ok && c.load(SeqCst) == 101);
    }
    res
}

// ===========================================================================
// Estres multihilo: ABA real con un escritor concurrente (y opcionalmente un enfriador)
// ===========================================================================

#[repr(align(64))]
struct SCell {
    val: AtomicU64,
    b: AtomicU64,
    e: AtomicU64,
}

struct StressOut {
    ok: u64,
    viol: u64,
    secs: f64,
}

fn stress<M: Mem>(trials: usize, cooler: bool) -> StressOut {
    const NC: usize = 50_000;
    let cells: Vec<SCell> = (0..NC)
        .map(|_| SCell { val: AtomicU64::new(0), b: AtomicU64::new(0), e: AtomicU64::new(0) })
        .collect();
    let target = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let t0 = Instant::now();
    let (ok_n, viol) = thread::scope(|s| {
        s.spawn(|| {
            while !stop.load(Relaxed) {
                let c = &cells[target.load(Acquire)];
                c.b.fetch_add(1, SeqCst);
                M::store(&c.val, 1);
                M::store(&c.val, 0);
                c.e.fetch_add(1, SeqCst);
            }
        });
        if cooler {
            s.spawn(|| {
                while !stop.load(Relaxed) {
                    cool();
                    for _ in 0..200 {
                        spin_loop();
                    }
                }
            });
        }
        let mut mon = Mon::new();
        let (mut ok_n, mut viol) = (0u64, 0u64);
        for t in 0..trials {
            let c = &cells[t % NC];
            let e_base = c.e.load(Acquire);
            target.store(t % NC, Release);
            let mut w = 0;
            while c.e.load(Acquire) == e_base && w < 20_000 {
                spin_loop();
                w += 1;
            }
            let v = M::ldxr(&mut mon, &c.val);
            let b1 = c.b.load(Acquire);
            for _ in 0..30 {
                spin_loop();
            }
            let e2 = c.e.load(Acquire);
            if M::stxr(&mut mon, &c.val, v) {
                ok_n += 1;
                if e2 > b1 {
                    viol += 1;
                }
            }
        }
        stop.store(true, SeqCst);
        (ok_n, viol)
    });
    StressOut { ok: ok_n, viol, secs: t0.elapsed().as_secs_f64() }
}

// Atomicidad del contador: hilos que mezclan el idioma y el bucle generico ldxr/stxr, con o sin
// un hilo que enfria sin parar. El total final debe ser exactamente threads*per.
fn counter_test<M: Mem>(threads: usize, per: u64, cooler: bool) -> (u64, u64, f64) {
    let cell = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let t0 = Instant::now();
    thread::scope(|s| {
        if cooler {
            s.spawn(|| {
                while !stop.load(Relaxed) {
                    cool();
                    for _ in 0..100 {
                        spin_loop();
                    }
                }
            });
        }
        let hs: Vec<_> = (0..threads)
            .map(|t| {
                let cell = &cell;
                s.spawn(move || {
                    for _ in 0..per {
                        if t % 2 == 0 {
                            M::idiom_add(cell, 1);
                        } else {
                            generic_add::<M>(cell, 1);
                        }
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        stop.store(true, SeqCst);
    });
    (cell.load(SeqCst), threads as u64 * per, t0.elapsed().as_secs_f64())
}

// Prueba de abortos de rseq: un hilo hace stores rapidos sobre celdas frias (verificando que
// ninguno se pierda ni cambie) mientras otro arma granulos sin parar: cada armado es un membarrier
// RSEQ que aborta las secciones en vuelo.
fn abort_test(dur: Duration) -> (u64, u64, u64, u64, u64) {
    reset_hot();
    COOL_THRESH.store(1 << 30, Relaxed);
    let work = cold_vec(1024);
    let fresh: Vec<SCell> = (0..30_000)
        .map(|_| SCell { val: AtomicU64::new(0), b: AtomicU64::new(0), e: AtomicU64::new(0) })
        .collect();
    let a0 = RSEQ_ABORTS.load(Relaxed);
    let m0 = MEMBARRIERS.load(Relaxed);
    let stop = AtomicBool::new(false);
    let (stores, bad, slow) = thread::scope(|s| {
        let h = s.spawn(|| {
            let rs = rseq_area();
            let mut last = vec![0u64; work.len()];
            let (mut n, mut slow) = (0u64, 0u64);
            while !stop.load(Relaxed) {
                for (i, c) in work.iter().enumerate() {
                    n += 1;
                    let v = n;
                    if !hot_store_fast(c, v, rs) {
                        slow += 1;
                        c.store(v, Release);
                    }
                    last[i] = v;
                }
            }
            let bad = work
                .iter()
                .zip(&last)
                .filter(|(c, l)| c.load(SeqCst) != **l)
                .count() as u64;
            (n, bad, slow)
        });
        let t0 = Instant::now();
        let mut k = 0;
        while t0.elapsed() < dur && k < fresh.len() {
            ensure_hot::<true>(&fresh[k].val, EPOCH.load(Acquire));
            k += 1;
            for _ in 0..200 {
                spin_loop();
            }
        }
        thread::sleep(Duration::from_millis(50));
        stop.store(true, SeqCst);
        h.join().unwrap()
    });
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();
    (
        stores,
        bad,
        slow,
        RSEQ_ABORTS.load(Relaxed) - a0,
        MEMBARRIERS.load(Relaxed) - m0,
    )
}

// ===========================================================================
// Memoria fria y stores
// ===========================================================================

fn granules_cold(v: &[AtomicU64]) -> bool {
    let start = v.as_ptr() as usize;
    let end = start + v.len() * 8;
    let mut a = start & !63;
    while a < end {
        if HOT[hot_idx(a)].load(Relaxed) != 0 {
            return false;
        }
        a += 64;
    }
    true
}

fn cold_vec(n: usize) -> Vec<AtomicU64> {
    let mut graveyard = Vec::new();
    for _ in 0..500 {
        let v: Vec<AtomicU64> = (0..n).map(|_| AtomicU64::new(0)).collect();
        if granules_cold(&v) {
            std::mem::forget(graveyard);
            return v;
        }
        graveyard.push(v);
    }
    panic!("no se encontro memoria fria");
}

// ns por store
fn store_bench<const WORK: usize, F: Fn(&AtomicU64, u64)>(dur: Duration, f: F) -> f64 {
    let mem = cold_vec(4096);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let t0 = Instant::now();
    let deadline = t0 + dur;
    let mut rounds = 0u64;
    loop {
        for _ in 0..16 {
            for c in mem.iter() {
                for _ in 0..WORK {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                }
                f(c, x ^ rounds);
            }
        }
        rounds += 16;
        if Instant::now() >= deadline {
            break;
        }
    }
    black_box((&mem, x));
    t0.elapsed().as_secs_f64() * 1e9 / (rounds * 4096) as f64
}

// ===========================================================================
// Exclusivas en estado estable
// ===========================================================================

type Runner = fn(&AtomicU64, &AtomicBool) -> (u64, u64);

#[derive(Clone, Copy)]
enum Insn {
    Ldxr,
    AddImm(u64),
    Stxr,
    CbnzRetry,
}

fn run_interp<M: Mem>(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let prog = [Insn::Ldxr, Insn::AddImm(1), Insn::Stxr, Insn::CbnzRetry];
    let mut mon = Mon::new();
    let (mut pc, mut x0, mut w2) = (0usize, 0u64, 0u64);
    let (mut ok, mut fail) = (0u64, 0u64);
    loop {
        match prog[pc] {
            Insn::Ldxr => {
                x0 = M::ldxr(&mut mon, cell);
                pc += 1;
            }
            Insn::AddImm(i) => {
                x0 = x0.wrapping_add(i);
                pc += 1;
            }
            Insn::Stxr => {
                w2 = if M::stxr(&mut mon, cell, x0) { 0 } else { 1 };
                pc += 1;
            }
            Insn::CbnzRetry => {
                if w2 != 0 {
                    fail += 1;
                } else {
                    ok += 1;
                    if ok & 0xFFF == 0 && stop.load(Relaxed) {
                        break;
                    }
                }
                pc = 0;
            }
        }
    }
    (ok, fail)
}

fn run_trans<M: Mem>(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let mut mon = Mon::new();
    let (mut ok, mut fail) = (0u64, 0u64);
    let mut n = 0u64;
    loop {
        let v = M::ldxr(&mut mon, cell).wrapping_add(1);
        if M::stxr(&mut mon, cell, v) {
            ok += 1;
        } else {
            fail += 1;
        }
        n += 1;
        if n & 0xFFF == 0 && stop.load(Relaxed) {
            break;
        }
    }
    (ok, fail)
}

fn run_idiom<M: Mem>(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let mut ok = 0u64;
    loop {
        M::idiom_add(cell, 1);
        ok += 1;
        if ok & 0xFFF == 0 && stop.load(Relaxed) {
            break;
        }
    }
    (ok, 0)
}

// devuelve (ns por incremento exitoso, contador correcto)
fn bench_excl(runner: Runner, threads: usize, dur: Duration) -> (f64, bool) {
    static CELL: AtomicU64 = AtomicU64::new(0);
    CELL.store(0, SeqCst);
    let g = gran(&CELL);
    g.ver.store(0, SeqCst);
    g.lock.store(false, SeqCst);
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let (ok, _fail) = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| s.spawn(|| runner(&CELL, &stop)))
            .collect();
        thread::sleep(dur);
        stop.store(true, SeqCst);
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    let secs = start.elapsed().as_secs_f64();
    (secs * 1e9 / ok as f64, CELL.load(SeqCst) == ok)
}

// Conjunto de trabajo rotativo: ldxr+stxr sobre `ws` granulos distintos, en bucle.
// Con ws mayor que el umbral de enfriamiento obliga a armar/enfriar continuamente.
#[repr(align(64))]
struct Pad(AtomicU64);

static WARM_US: AtomicU64 = AtomicU64::new(0);

fn rot_bench<M: Mem>(ws: usize, dur: Duration, neighbour: bool) -> f64 {
    reset_hot();
    let cells: Vec<Pad> = (0..ws).map(|_| Pad(AtomicU64::new(0))).collect();
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
        if neighbour {
            s.spawn(|| {
                let mut x = 0u64;
                while !stop.load(Relaxed) {
                    x = x.wrapping_add(1);
                    black_box(x);
                }
            });
            thread::sleep(Duration::from_millis(20));
        }
        let mut mon = Mon::new();
        // pasada de calentamiento (armado inicial): se cronometra aparte
        let tw = Instant::now();
        for c in cells.iter() {
            let v = M::ldxr(&mut mon, &c.0).wrapping_add(1);
            M::stxr(&mut mon, &c.0, v);
        }
        WARM_US.store(tw.elapsed().as_micros() as u64, Relaxed);
        let t0 = Instant::now();
        let mut n = 0u64;
        while t0.elapsed() < dur {
            for c in cells.iter() {
                let v = M::ldxr(&mut mon, &c.0).wrapping_add(1);
                M::stxr(&mut mon, &c.0, v);
            }
            n += ws as u64;
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / n as f64;
        stop.store(true, SeqCst);
        ns
    })
}

// ---- costos de armado ----

fn arm_cost(n: usize, with_spinner: bool) -> f64 {
    reset_hot();
    COOL_THRESH.store(1 << 30, Relaxed);
    let all: Vec<SCell> = (0..4 * n)
        .map(|_| SCell { val: AtomicU64::new(0), b: AtomicU64::new(0), e: AtomicU64::new(0) })
        .collect();
    let cells: Vec<&SCell> = all
        .iter()
        .filter(|c| HOT[hot_idx(&c.val as *const AtomicU64 as usize)].load(Relaxed) == 0)
        .take(n)
        .collect();
    assert!(cells.len() == n, "no hay suficientes granulos frios");
    let stop = AtomicBool::new(false);
    let us = thread::scope(|s| {
        if with_spinner {
            s.spawn(|| {
                let mut x = 0u64;
                while !stop.load(Relaxed) {
                    x = x.wrapping_add(1);
                    black_box(x);
                }
            });
            thread::sleep(Duration::from_millis(30));
        }
        let t = Instant::now();
        for c in &cells {
            ensure_hot::<true>(&c.val, EPOCH.load(Acquire));
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        stop.store(true, SeqCst);
        us
    });
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();
    us
}

// costo de una pasada de enfriamiento con n granulos calientes (todos viejos)
fn cool_cost(n: usize) -> (f64, usize) {
    reset_hot();
    COOL_THRESH.store(1 << 30, Relaxed);
    let cells: Vec<Pad> = (0..n).map(|_| Pad(AtomicU64::new(0))).collect();
    for c in &cells {
        ensure_hot::<true>(&c.0, EPOCH.load(Acquire));
    }
    let hot = HOT_COUNT.load(Relaxed);
    let t = Instant::now();
    let cleared = cool();
    let us = t.elapsed().as_secs_f64() * 1e6;
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    let _ = hot;
    (us, cleared)
}

// ===========================================================================
// C. SIMD: uqadd v.4s
// ===========================================================================

const NVEC: usize = 1024;

fn simd_pass_scalar(acc: &mut [[u32; 4]], data: &[[u32; 4]]) {
    for (a, d) in acc.iter_mut().zip(data) {
        for i in 0..4 {
            let mut x = a[i];
            let y = d[i];
            let t: u32;
            unsafe {
                asm!(
                    "add {x:e}, {y:e}",
                    "sbb {t:e}, {t:e}",
                    "or {x:e}, {t:e}",
                    x = inout(reg) x,
                    y = in(reg) y,
                    t = out(reg) t,
                    options(pure, nomem, nostack)
                );
            }
            let _ = t;
            a[i] = x;
        }
    }
}

#[target_feature(enable = "sse4.1")]
unsafe fn simd_pass_sse41(acc: &mut [[u32; 4]], data: &[[u32; 4]]) {
    let ones = _mm_set1_epi32(-1);
    for (a, d) in acc.iter_mut().zip(data) {
        let va = _mm_loadu_si128(a.as_ptr() as *const __m128i);
        let vb = _mm_loadu_si128(d.as_ptr() as *const __m128i);
        let not_a = _mm_xor_si128(va, ones);
        let m = _mm_min_epu32(vb, not_a);
        let r = _mm_add_epi32(va, m);
        _mm_storeu_si128(a.as_mut_ptr() as *mut __m128i, r);
    }
}

fn simd_pass_sse41_safe(acc: &mut [[u32; 4]], data: &[[u32; 4]]) {
    unsafe { simd_pass_sse41(acc, data) }
}

fn make_simd_data() -> (Vec<[u32; 4]>, Vec<[u32; 4]>) {
    let mut rng = XorShift(0xDEAD_BEEF_CAFE_F00D);
    let gen = |rng: &mut XorShift| -> [u32; 4] {
        let mut v = [0u32; 4];
        for l in v.iter_mut() {
            *l = (rng.next() as u32) >> (rng.next() % 32);
        }
        v
    };
    let acc: Vec<_> = (0..NVEC).map(|_| gen(&mut rng)).collect();
    let data: Vec<_> = (0..NVEC).map(|_| gen(&mut rng)).collect();
    (acc, data)
}

fn expected_sat(acc: &[[u32; 4]], data: &[[u32; 4]]) -> Vec<[u32; 4]> {
    acc.iter()
        .zip(data)
        .map(|(a, d)| {
            let mut r = [0u32; 4];
            for i in 0..4 {
                r[i] = a[i].saturating_add(d[i]);
            }
            r
        })
        .collect()
}

fn verify_simd(pass: fn(&mut [[u32; 4]], &[[u32; 4]])) -> bool {
    let (mut acc, data) = make_simd_data();
    let expected = expected_sat(&acc, &data);
    pass(&mut acc, &data);
    if acc != expected {
        return false;
    }
    let e = [
        0u32, 1, 2, 0x7FFF_FFFE, 0x7FFF_FFFF, 0x8000_0000, 0x8000_0001, 0xFFFF_FFFE, 0xFFFF_FFFF,
    ];
    let (mut a2, mut d2) = (Vec::new(), Vec::new());
    for &x in &e {
        for &y in &e {
            a2.push([x, y, x, y]);
            d2.push([y, x, x, y]);
        }
    }
    let expected = expected_sat(&a2, &d2);
    pass(&mut a2, &d2);
    a2 == expected
}

// ns por operacion vectorial (uqadd .4s)
fn run_simd(pass: fn(&mut [[u32; 4]], &[[u32; 4]]), dur: Duration) -> f64 {
    let (mut acc, data) = make_simd_data();
    let t0 = Instant::now();
    let deadline = t0 + dur;
    let mut passes = 0u64;
    loop {
        for _ in 0..16 {
            pass(&mut acc, &data);
        }
        passes += 16;
        black_box(&acc);
        if Instant::now() >= deadline {
            break;
        }
    }
    t0.elapsed().as_secs_f64() * 1e9 / (passes * NVEC as u64) as f64
}

// ===========================================================================
// Secciones
// ===========================================================================

fn section_equivalence() -> (Vec<bool>, Vec<bool>, bool) {
    println!("=== 1. PRUEBAS DE EQUIVALENCIA (lo que falla no se mide como 'alternativa') ===\n");
    println!("A. FLAGS: x0..x5 y las 14 condiciones iguales a ARM al salir del bloque");
    let flag_cands: [(&str, fn(&mut Cpu), bool); 4] = [
        ("A1. Flags eager (NZCV tras cada subs)", block_eager, false),
        ("A2. Flags perezosas (guarda operandos)", block_lazy, true),
        ("A3. Fusion cmp+cmov SIN guardar flags", block_fused_nostate, true),
        ("A4. Fusion cmp+cmov + operandos guardados", block_fused_state, true),
    ];
    let flag_pass: Vec<bool> = flag_cands
        .iter()
        .map(|(n, b, lazy)| {
            let ok = flags_equivalent(*b, *lazy);
            println!("   {:<46} {}", n, verdict(ok));
            ok
        })
        .collect();

    println!("\nB. MONITOR EXCLUSIVO (E1 ABA stores, E2 otro valor, E3 mismo valor, E4 otro core,");
    println!("   E5 idioma vs LL/SC, E6 idioma ABA, E7 sin fallo espurio)");
    print!("   {:<46}", "");
    for t in MON_TESTS.iter() {
        print!(" {:<3}", t);
    }
    println!();
    let mut mon_all_ok = true;
    let mut show = |name: &str, r: Vec<bool>, strict: bool| {
        print!("   {:<46}", name);
        for x in &r {
            print!(" {:<3}", if *x { "ok" } else { "XX" });
        }
        let all = r.iter().all(|x| *x);
        if strict {
            mon_all_ok &= all;
        }
        println!("  => {}", verdict(all));
    };
    show("CAS por valor (referencia, no estricta)", monitor_tests::<Cmpxchg>(), false);
    show("Todos los stores instrumentados", monitor_tests::<Fiel>(), true);
    reset_hot();
    show("Calientes + rseq + membarrier", monitor_tests::<HotMem<true>>(), true);
    show("  ... con enfriamiento forzado entre pasos", monitor_tests::<HotCool>(), true);
    FALLBACK.store(true, SeqCst);
    show("  ... modo FALLBACK (sin rseq/membarrier)", monitor_tests::<HotMem<true>>(), true);
    FALLBACK.store(false, SeqCst);
    show("Calientes SIN membarrier (ROTO, ver 3)", monitor_tests::<HotMem<false>>(), false);

    println!("\nC. SIMD: igual que u32::saturating_add (aleatorio + todos los pares borde)");
    let mut simd_cands: Vec<(&str, fn(&mut [[u32; 4]], &[[u32; 4]]))> =
        vec![("C1. Escalar por lane (add+sbb+or)", simd_pass_scalar)];
    if is_x86_feature_detected!("sse4.1") {
        simd_cands.push(("C2. Vectorial SSE4.1 (a + min(b,~a))", simd_pass_sse41_safe));
    }
    let simd_pass: Vec<bool> = simd_cands
        .iter()
        .map(|(n, p)| {
            let ok = verify_simd(*p);
            println!("   {:<46} {}", n, verdict(ok));
            ok
        })
        .collect();
    reset_hot();
    (flag_pass, simd_pass, mon_all_ok)
}

fn section_costs(flag_pass: &[bool], simd_pass: &[bool], all_ok: &mut bool) {
    let d = Duration::from_millis(500);
    println!("\n=== 2. COSTO: traducido vs ARM original ===");
    println!("'vs base' = contra la mejor alternativa SIN garantias/sin traduccion, misma maquina.");
    println!("'ARM ns' = codigo nativo AArch64 (arm_native.c); x/ARM = ns traducido / ns ARM.\n");

    // A. flags
    let cands: [(&str, fn(&mut Cpu)); 4] = [
        ("A1. Flags eager", block_eager),
        ("A2. Flags perezosas", block_lazy),
        ("A3. Fusion sin flags [ref. no equivalente]", block_fused_nostate),
        ("A4. Fusion + operandos guardados", block_fused_state),
    ];
    let base = run_flags(block_fused_nostate, d);
    header("A. FLAGS: 2 xorshift + subs + 2 csel + acumulador (por iteracion)");
    for (i, (n, b)) in cands.iter().enumerate() {
        if i == 2 {
            row(n, base, base, "flags.block");
        } else if flag_pass[i] {
            row(n, run_flags(*b, d), base, "flags.block");
        }
    }
    println!();

    // C. SIMD
    header("C. SIMD: uqadd .4s (por operacion vectorial)");
    let sc = [
        ("C1. Escalar por lane (add+sbb+or)", simd_pass_scalar as fn(&mut [[u32; 4]], &[[u32; 4]])),
        ("C2. Vectorial SSE4.1 (a + min(b,~a))", simd_pass_sse41_safe),
    ];
    let best_simd = if simd_pass.len() > 1 { run_simd(simd_pass_sse41_safe, d) } else { run_simd(simd_pass_scalar, d) };
    for (i, (n, p)) in sc.iter().enumerate() {
        if i < simd_pass.len() && simd_pass[i] {
            row(n, run_simd(*p, d), best_simd, "simd.uqadd");
        }
    }
    println!();

    // D. stores
    reset_hot();
    COOL_THRESH.store(1 << 30, Relaxed);
    for (label, w, key) in [("sin trabajo entre stores", 0usize, "store.work0"), ("~10 instrucciones entre stores", 1, "store.work1")] {
        header(&format!("D. STORE normal del guest, {}", label));
        let rs = rseq_area();
        let run = |k: u8| -> f64 {
            match (w, k) {
                (0, 0) => store_bench::<0, _>(d, |c, v| c.store(v, Release)),
                (0, 1) => store_bench::<0, _>(d, |c, v| Versioned::slow_store(c, v)),
                (0, _) => store_bench::<0, _>(d, |c, v| {
                    if !hot_store_fast(c, v, rs) {
                        Versioned::slow_store(c, v)
                    }
                }),
                (_, 0) => store_bench::<1, _>(d, |c, v| c.store(v, Release)),
                (_, 1) => store_bench::<1, _>(d, |c, v| Versioned::slow_store(c, v)),
                (_, _) => store_bench::<1, _>(d, |c, v| {
                    if !hot_store_fast(c, v, rs) {
                        Versioned::slow_store(c, v)
                    }
                }),
            }
        };
        let raw = run(0);
        let all = run(1);
        let fast = run(2);
        row("S0. Store crudo [ref. no estricta]", raw, raw, key);
        row("S1. Todos instrumentados (lock+version)", all, raw, key);
        row("S3. Filtro + rseq (ESTRICTO)", fast, raw, key);
        println!();
    }
    {
        let mem: Vec<AtomicU64> = (0..4096).map(|_| AtomicU64::new(0)).collect();
        for c in &mem {
            ensure_hot::<true>(c, EPOCH.load(Acquire));
        }
        let rs = rseq_area();
        let t0 = Instant::now();
        let mut n = 0u64;
        while t0.elapsed() < d {
            for c in &mem {
                if !hot_store_fast(c, n, rs) {
                    Versioned::slow_store(c, n);
                }
            }
            n += 4096;
        }
        let ns = t0.elapsed().as_secs_f64() * 1e9 / n as f64;
        header("D. STORE sobre memoria que tambien usa exclusivas (granulo caliente)");
        row("S4. Filtro + rseq, camino lento", ns, ns / 10.0, "store.work0");
        println!("   (la columna 'vs base' de esta fila no es relevante: esos granulos son los pocos 'calientes')\n");
    }
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();

    // B. exclusivas
    let dur = Duration::from_millis(600);
    for t in [1usize, 2] {
        header(&format!("B. EXCLUSIVAS (incremento atomico ldxr/add/stxr), {} hilo(s)", t));
        let key = format!("excl.{}t.llsc", t);
        let runners: [(&str, Runner); 5] = [
            ("X0. CAS por instruccion [no estricta]", run_trans::<Cmpxchg>),
            ("X1. Interprete, todos instrumentados", run_interp::<Fiel>),
            ("X2. Idioma bajo lock, todos instrumentados", run_idiom::<Fiel>),
            ("X3. Interprete, calientes+rseq", run_interp::<HotMem<true>>),
            ("X4. Idioma bajo lock, calientes+rseq", run_idiom::<HotMem<true>>),
        ];
        let mut base = 0.0;
        for (i, (n, r)) in runners.iter().enumerate() {
            reset_hot();
            let (ns, ok) = bench_excl(*r, t, dur);
            *all_ok &= ok;
            if i == 0 {
                base = ns;
            }
            row(n, ns, base, &key);
            if !ok {
                println!("   ^ CONTADOR INCORRECTO");
            }
        }
        if let Some(l) = arm(&format!("excl.{}t.lse", t)) {
            println!("   (referencia ARMv8.1 LSE ldadd/stadd, {} hilo(s): {:.2} ns)", t, l);
        }
        println!();
    }

    // B'. conjunto de trabajo rotativo (armado, enfriamiento y saturacion en estado estable)
    header("B'. EXCLUSIVAS rotando sobre N granulos (tras una pasada de calentamiento)");
    let d2 = Duration::from_millis(500);
    let line = |name: &str, ws: usize, key: &str, neigh: bool, thresh: usize| {
        COOL_THRESH.store(thresh, Relaxed);
        let b = rot_bench::<Cmpxchg>(ws, d2, neigh);
        let c0 = COOLS.load(Relaxed);
        let s0 = SATURATIONS.load(Relaxed);
        let h = rot_bench::<HotMem<true>>(ws, d2, neigh);
        let cools = COOLS.load(Relaxed) - c0;
        let sats = SATURATIONS.load(Relaxed) - s0;
        row(&format!("{} CAS [no estricta]", name), b, b, key);
        row(&format!("{} estricto", name), h, b, key);
        println!(
            "   (pasada inicial de armado: {:.1} ms; enfriamientos: {}, saturaciones: {}{})",
            WARM_US.load(Relaxed) as f64 / 1000.0,
            cools,
            sats,
            if sats > 0 { " => modo todo-instrumentado" } else { "" }
        );
    };
    line("N=64    ", 64, "excl.rot64", false, DEFAULT_THRESH);
    line("N=8192  ", 8192, "excl.rot8192", false, DEFAULT_THRESH);
    line("N=8192  + hilo vecino", 8192, "excl.rot8192", true, DEFAULT_THRESH);
    line("N=8192  umbral 2048 (fuerza enfriar)", 8192, "excl.rot8192", false, 2048);
    line("N=200000 + hilo vecino (> tabla)", 200_000, "excl.rot200k", true, DEFAULT_THRESH);
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();
    println!();
}

fn section_overheads() {
    println!("=== 3. COSTOS PUNTUALES DEL MONITOR (ARM nativo: 0, es una instruccion) ===");
    println!("Primer LDXR sobre un granulo frio (armado):");
    println!("   1 hilo (membarrier barato)       : {:>8.2} us", arm_cost(2000, false));
    println!("   2 hilos (membarrier con IPI)     : {:>8.2} us", arm_cost(2000, true));
    println!("Una pasada de enfriamiento ('cool()'), todos los granulos viejos:");
    for n in [1000usize, 4000, 16000] {
        let (us, cleared) = cool_cost(n);
        println!(
            "   {:>6} granulos calientes -> {:>8.1} us total ({:>6.1} ns/granulo, {} enfriados)",
            n,
            us,
            us * 1000.0 / n as f64,
            cleared
        );
    }
    println!();
}

fn section_stress(all_ok: &mut bool) {
    println!("=== 4. ESTRES, ENFRIAMIENTO, FALLBACK Y ABORTOS RSEQ ===");
    let trials = 100_000;
    println!("\n4a. ABA real, {} pruebas por variante (violacion = STXR ok con escritura en medio)", trials);
    println!("{:<46} {:>9} {:>11} {:>9} {:>8}", "variante", "STXR ok", "violaciones", "STXR fallo", "tiempo");
    let mut run = |name: &str, o: StressOut, strict: bool| {
        println!(
            "{:<46} {:>9} {:>11} {:>9} {:>7.2}s",
            name,
            o.ok,
            o.viol,
            trials as u64 - o.ok,
            o.secs
        );
        if strict {
            *all_ok &= o.viol == 0;
        }
    };
    reset_hot();
    run("CAS por valor [no estricta]", stress::<Cmpxchg>(trials, false), false);
    run("Todos los stores instrumentados", stress::<Fiel>(trials, false), true);
    reset_hot();
    COOL_THRESH.store(1 << 30, Relaxed);
    run("Calientes + rseq + membarrier", stress::<HotMem<true>>(trials, false), true);
    COOL_THRESH.store(256, Relaxed);
    reset_hot();
    run("  ... con enfriamiento agresivo (umbral 256)", stress::<HotMem<true>>(trials, false), true);
    COOL_THRESH.store(1 << 30, Relaxed);
    reset_hot();
    run("  ... con hilo enfriador continuo", stress::<HotMem<true>>(trials, true), true);
    FALLBACK.store(true, SeqCst);
    run("  ... modo FALLBACK", stress::<HotMem<true>>(trials, false), true);
    FALLBACK.store(false, SeqCst);
    reset_hot();
    run("Calientes SIN membarrier (roto)", stress::<HotMem<false>>(trials, false), false);
    println!("   Nota: la variante SIN membarrier tambien da 0 violaciones; este estres NO detecta su fallo");
    println!("   (la ventana es de 3 instrucciones). Su incorrectitud es de diseno, no de medicion.");
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();

    println!("\n4b. Atomicidad del contador (hilos mezclando idioma y bucle ldxr/stxr), 2x200.000 incr.");
    println!("{:<46} {:>11} {:>11} {:>8}", "variante", "final", "esperado", "tiempo");
    let mut crow = |name: &str, r: (u64, u64, f64)| {
        println!(
            "{:<46} {:>11} {:>11} {:>7.2}s{}",
            name,
            r.0,
            r.1,
            r.2,
            if r.0 == r.1 { "" } else { "  INCORRECTO" }
        );
        *all_ok &= r.0 == r.1;
    };
    crow("Todos instrumentados", counter_test::<Fiel>(2, 200_000, false));
    COOL_THRESH.store(1 << 30, Relaxed);
    crow("Calientes + rseq", counter_test::<HotMem<true>>(2, 200_000, false));
    crow("  ... con hilo enfriador continuo", counter_test::<HotMem<true>>(2, 200_000, true));
    FALLBACK.store(true, SeqCst);
    crow("  ... modo FALLBACK", counter_test::<HotMem<true>>(2, 200_000, false));
    FALLBACK.store(false, SeqCst);
    COOL_THRESH.store(DEFAULT_THRESH, Relaxed);
    reset_hot();

    println!("\n4c. Abortos de rseq: un hilo guarda en celdas frias (verificando cada valor) mientras otro");
    println!("    arma granulos sin parar (cada armado = 1 membarrier RSEQ)");
    let (stores, bad, slow, aborts, mbs) = abort_test(Duration::from_millis(1500));
    println!("   stores del hilo escritor : {}", stores);
    println!("   ... por camino lento      : {} (granulo ya caliente)", slow);
    println!("   membarrier RSEQ emitidos  : {}", mbs);
    println!("   secciones rseq abortadas  : {} (reintentadas desde el inicio)", aborts);
    println!("   celdas con valor perdido  : {}   => {}", bad, if bad == 0 { "OK" } else { "FALLO" });
    *all_ok &= bad == 0;
    if aborts == 0 {
        println!("   (0 abortos: en esta corrida el camino de aborto NO se ejercito; no cuenta como prueba)");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arm_path = args
        .iter()
        .position(|a| a == "--arm")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "arm_results.txt".to_string());
    let n_arm = load_arm(&arm_path);

    let mb = membarrier_register();
    let rs = rseq_registered();
    println!("Modo: membarrier RSEQ {} | rseq registrado {}", if mb { "disponible" } else { "NO disponible" }, if rs { "si" } else { "NO" });
    if !mb || !rs {
        FALLBACK.store(true, SeqCst);
        println!("=> FALLBACK automatico: todos los stores instrumentados (lento pero correcto)");
    }
    if n_arm > 0 {
        println!("Referencia ARM: {} valores leidos de {}", n_arm, arm_path);
    } else {
        println!("Referencia ARM: no hay '{}'; columnas ARM = n/d (ver arm_native.c)", arm_path);
    }
    println!();
    if !mb || !rs {
        println!("(Sin membarrier/rseq no se pueden medir los caminos rapidos; solo se corre el modo fallback)");
        let r = monitor_tests::<Fiel>();
        println!("Equivalencia (instrumentado): {}", verdict(r.iter().all(|x| *x)));
        return;
    }

    let mut all_ok = true;
    let (fp, sp, mon_ok) = section_equivalence();
    all_ok &= mon_ok;
    section_costs(&fp, &sp, &mut all_ok);
    section_overheads();
    section_stress(&mut all_ok);
    println!(
        "\nVerificacion global (equivalencia estricta, contadores, violaciones, valores): {}",
        if all_ok { "OK" } else { "FALLO" }
    );
}
