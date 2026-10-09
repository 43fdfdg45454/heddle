// Monitor exclusivo de ARM64 (LDXR/STXR) sobre x86-64 con equivalencia funcional estricta
// y costo bajo para los stores normales.
//
// Idea ("granulos calientes" + rseq + membarrier):
//   * Un granulo (64 B) es FRIO hasta que alguien hace LDXR/idioma sobre el.
//   * Store normal sobre granulo frio: una secuencia rseq de 3 instrucciones
//       cmp byte [flag],0 ; jne lento ; mov [addr],val
//     Sin lock, sin atomica.
//   * Primer LDXR sobre un granulo frio: lo marca (estado 1) y llama a
//     membarrier(PRIVATE_EXPEDITED_RSEQ). El kernel (a) reinicia cualquier secuencia rseq
//     que otro hilo tenga a medias, y (b) fuerza una barrera de memoria en los demas hilos.
//     Al volver, ningun store "frio" puede aterrizar despues del LDXR. Recien entonces pasa
//     a estado 2 (caliente).
//   * Granulo caliente: stores normales y atomicas pasan por lock + version (como antes),
//     asi que cualquier escritura, aunque devuelva el mismo valor, invalida la reserva.
//
// Limitaciones (no implementadas aqui): un granulo nunca vuelve a frio.
//
// Compilar: rustc --edition 2021 -C opt-level=3 monitor_strict.rs -o monitor_strict

use std::arch::asm;
use std::hint::{black_box, spin_loop};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering::*};
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn syscall(num: i64, ...) -> i64;
    static __rseq_offset: isize;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
}

const SYS_MEMBARRIER: i64 = 324;
const MB_REGISTER_PRIVATE_EXPEDITED_RSEQ: i64 = 256;
const MB_PRIVATE_EXPEDITED_RSEQ: i64 = 128;

fn membarrier_register() {
    let r = unsafe { syscall(SYS_MEMBARRIER, MB_REGISTER_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) };
    assert!(r == 0, "membarrier(REGISTER_PRIVATE_EXPEDITED_RSEQ) fallo");
}

fn membarrier_rseq() {
    let r = unsafe { syscall(SYS_MEMBARRIER, MB_PRIVATE_EXPEDITED_RSEQ, 0i64, 0i64) };
    assert!(r == 0, "membarrier(PRIVATE_EXPEDITED_RSEQ) fallo");
}

fn rseq_area() -> *mut u8 {
    unsafe {
        let tp: usize;
        asm!("mov {}, fs:[0]", out(reg) tp, options(nostack, preserves_flags, readonly));
        (tp as isize + __rseq_offset) as *mut u8
    }
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

// ===========================================================================
// Granulos con lock + version (camino lento, estricto)
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
}

impl Mon {
    fn new() -> Mon {
        Mon { valid: false, val: 0, ver: 0 }
    }
}

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

// Base: version por granulo (ldxr/stxr).
struct Versioned;
impl Versioned {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        let g = gran(cell);
        mon.ver = g.ver.load(Acquire);
        mon.val = cell.load(Acquire);
        mon.valid = true;
        mon.val
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        let g = gran(cell);
        glock(g);
        let ok = mon.valid && g.ver.load(Relaxed) == mon.ver;
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

// Estricta "todos los stores instrumentados" (la de antes, ~50x en stores).
struct Fiel;
impl Mem for Fiel {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        Versioned::ldxr(mon, cell)
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

// ===========================================================================
// Granulos calientes
// ===========================================================================

const HOT_BITS: usize = 16;
static HOT: [AtomicU8; 1 << HOT_BITS] = [const { AtomicU8::new(0) }; 1 << HOT_BITS];
static ARM_LOCK: AtomicBool = AtomicBool::new(false);

fn hot_idx(addr: usize) -> usize {
    (addr >> 6) & ((1 << HOT_BITS) - 1)
}

// 0 = frio, 1 = armando, 2 = caliente. BARRIER=false omite membarrier (variante insegura,
// solo para el estres: muestra que las pruebas no distinguen un esquema roto).
fn ensure_hot<const BARRIER: bool>(cell: &AtomicU64) {
    let h = &HOT[hot_idx(cell as *const AtomicU64 as usize)];
    if h.load(Acquire) == 2 {
        return;
    }
    while ARM_LOCK
        .compare_exchange_weak(false, true, Acquire, Relaxed)
        .is_err()
    {
        spin_loop();
    }
    if h.load(Relaxed) == 0 {
        h.store(1, SeqCst);
        if BARRIER {
            membarrier_rseq();
        }
        h.store(2, Release);
    }
    ARM_LOCK.store(false, Release);
}

// Store rapido sobre granulo frio: secuencia rseq. Devuelve true si el store se hizo.
// Si el kernel interrumpe la secuencia (preemption, senal, migracion o membarrier RSEQ),
// salta a la etiqueta de aborto y la repite desde el principio, volviendo a mirar el flag.
#[inline(always)]
fn hot_store_fast(cell: &AtomicU64, v: u64, rs: *mut u8) -> bool {
    let addr = cell as *const AtomicU64 as *mut u64;
    let flag = &HOT[hot_idx(addr as usize)] as *const AtomicU8 as *const u8;
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
            out("rax") _,
            options(att_syntax, nostack)
        );
    }
    r == 0
}

struct HotMem<const BARRIER: bool>;
impl<const B: bool> Mem for HotMem<B> {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        ensure_hot::<B>(cell);
        Versioned::ldxr(mon, cell)
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        Versioned::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        if !hot_store_fast(cell, v, rseq_area()) {
            Versioned::slow_store(cell, v);
        }
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        ensure_hot::<B>(cell);
        Versioned::locked_add(cell, imm)
    }
}

// ===========================================================================
// Pruebas deterministas de equivalencia (dos "cores" intercalados a mano)
// ===========================================================================

const MON_TESTS: [&str; 7] = ["E1", "E2", "E3", "E4", "E5", "E6", "E7"];

fn monitor_tests<M: Mem>() -> Vec<bool> {
    let mut res = Vec::new();
    {
        // E1: A -> B -> A con stores normales
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        // E2: store con otro valor
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        // E3: store con el MISMO valor
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        // E4: otro core completa su par ldxr/stxr
        let c = AtomicU64::new(100);
        let (mut m1, mut m2) = (Mon::new(), Mon::new());
        M::ldxr(&mut m1, &c);
        M::ldxr(&mut m2, &c);
        let ok2 = M::stxr(&mut m2, &c, 101);
        let ok1 = M::stxr(&mut m1, &c, 101);
        res.push(ok2 && !ok1);
    }
    {
        // E5: otro core ejecuta el idioma (+1)
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    {
        // E6: el idioma sube y baja (A -> A+1 -> A)
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        M::idiom_add(&c, u64::MAX);
        res.push(c.load(SeqCst) == 100 && !M::stxr(&mut m, &c, 101));
    }
    {
        // E7: sin interferencia el stxr tiene exito
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        let ok = M::stxr(&mut m, &c, 101);
        res.push(ok && c.load(SeqCst) == 101);
    }
    res
}

// ===========================================================================
// Estres multihilo: ABA real con un escritor concurrente
//
// Un hilo escritor hace pares de stores normales 1 -> 0 sobre la celda "objetivo".
// El hilo lector hace LDXR, espera un poco y hace STXR. Contadores b (inicio) y e (fin) por celda:
// si un par entero (b despues del LDXR, e antes del STXR) ocurrio entre LDXR y STXR y el STXR
// tuvo exito, ARM habria fallado: es una violacion.
// Cada celda esta en su propio granulo; las primeras 50.000 pruebas usan granulos frios.
// ===========================================================================

#[repr(align(64))]
struct SCell {
    val: AtomicU64,
    b: AtomicU64,
    e: AtomicU64,
}

fn stress<M: Mem>(trials: usize) -> (u64, u64, f64) {
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
    (ok_n, viol, t0.elapsed().as_secs_f64())
}

// ===========================================================================
// Costo de un store normal del guest
// ===========================================================================

// Los granulos calientes no se enfrian: toda la memoria de prueba tiene que ser verificablemente fria,
// o el benchmark mediria el camino lento sin querer.
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
            std::mem::forget(graveyard); // se pierde a proposito: evita reutilizar esa memoria
            return v;
        }
        graveyard.push(v);
    }
    panic!("no se encontro memoria fria");
}

fn store_bench<const WORK: usize, F: Fn(&AtomicU64, u64)>(dur: Duration, f: F) -> u64 {
    let mem = cold_vec(4096);
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let deadline = Instant::now() + dur;
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
    rounds * 4096
}

// ===========================================================================
// Operaciones exclusivas en estado estable
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
    (ok as f64 / secs, CELL.load(SeqCst) == ok)
}

// ===========================================================================
// Costos de armado (primer LDXR sobre un granulo frio) y de mprotect
// ===========================================================================

fn arm_cost(n: usize, with_spinner: bool) -> f64 {
    let all: Vec<SCell> = (0..4 * n)
        .map(|_| SCell { val: AtomicU64::new(0), b: AtomicU64::new(0), e: AtomicU64::new(0) })
        .collect();
    // solo granulos realmente frios: el costo medido es el del PRIMER armado
    let cells: Vec<&SCell> = all
        .iter()
        .filter(|c| HOT[hot_idx(&c.val as *const AtomicU64 as usize)].load(Relaxed) == 0)
        .take(n)
        .collect();
    assert!(cells.len() == n, "no hay suficientes granulos frios");
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
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
            ensure_hot::<true>(&c.val);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
        stop.store(true, SeqCst);
        us
    })
}

fn mprotect_cost(n: usize, with_spinner: bool) -> f64 {
    let stop = AtomicBool::new(false);
    thread::scope(|s| {
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
        let us = unsafe {
            let p = mmap(std::ptr::null_mut(), 4096, 3, 0x22, -1, 0);
            *p = 1;
            let t = Instant::now();
            for _ in 0..n {
                mprotect(p, 4096, 1);
                mprotect(p, 4096, 3);
            }
            t.elapsed().as_secs_f64() * 1e6 / n as f64
        };
        stop.store(true, SeqCst);
        us
    })
}

// ===========================================================================

fn verdict(b: bool) -> &'static str {
    if b {
        "PASA"
    } else {
        "FALLA"
    }
}

fn section_tests() {
    // ---------------- 1. Pruebas deterministas ----------------
    println!("=== 1. PRUEBAS DE EQUIVALENCIA (E1..E7, ver monitor anterior) ===");
    let cands: Vec<(&str, Vec<bool>)> = vec![
        ("CAS por valor (referencia, no estricta)", monitor_tests::<Cmpxchg>()),
        ("Todos los stores instrumentados", monitor_tests::<Fiel>()),
        ("Granulos calientes + rseq + membarrier", monitor_tests::<HotMem<true>>()),
        ("Granulos calientes SIN membarrier (roto)", monitor_tests::<HotMem<false>>()),
    ];
    print!("{:<44}", "");
    for t in MON_TESTS.iter() {
        print!(" {:<3}", t);
    }
    println!();
    for (n, r) in &cands {
        print!("{:<44}", n);
        for x in r {
            print!(" {:<3}", if *x { "ok" } else { "XX" });
        }
        println!("  => {}", verdict(r.iter().all(|x| *x)));
    }

}

fn section_stress(all_ok: &mut bool) {
    // ---------------- 4. Estres multihilo ----------------
    println!("\n=== 4. ESTRES MULTIHILO: ABA real, 100.000 pruebas por variante ===");
    println!(
        "{:<44} {:>9} {:>11} {:>10}",
        "variante", "STXR ok", "violaciones", "tiempo"
    );
    let trials = 100_000;
    let mut stress_rows: Vec<(&str, (u64, u64, f64))> = Vec::new();
    stress_rows.push(("CAS por valor (no estricta)", stress::<Cmpxchg>(trials)));
    stress_rows.push(("Todos los stores instrumentados", stress::<Fiel>(trials)));
    stress_rows.push(("Calientes + rseq + membarrier", stress::<HotMem<true>>(trials)));
    stress_rows.push(("Calientes SIN membarrier (roto)", stress::<HotMem<false>>(trials)));
    for (n, (ok, v, t)) in &stress_rows {
        println!("{:<44} {:>9} {:>11} {:>9.2}s", n, ok, v, t);
    }
    let strict_viol = stress_rows[1].1 .1 + stress_rows[2].1 .1;
    *all_ok &= strict_viol == 0;
}

fn section_stores() {
    // ---------------- 3. Costo de stores normales ----------------
    let dur = Duration::from_millis(500);
    println!("\n=== 3. COSTO DE UN STORE NORMAL DEL GUEST (4096 celdas, {} ms cada una) ===", dur.as_millis());
    println!(
        "{:<44} {:>14} {:>9} {:>9}",
        "variante", "store/s", "ns/store", "vs crudo"
    );
    for (label, w) in [("sin trabajo entre stores", 0usize), ("~10 instrucciones entre stores", 1usize)] {
        println!("-- {} --", label);
        let rs = rseq_area();
        let run = |k: u8| -> u64 {
            // pre-calentar todos los granulos de la memoria para la variante "caliente"
            match (w, k) {
                (0, 0) => store_bench::<0, _>(dur, |c, v| c.store(v, Release)),
                (0, 1) => store_bench::<0, _>(dur, |c, v| Versioned::slow_store(c, v)),
                (0, 2) => store_bench::<0, _>(dur, |c, v| {
                    if HOT[hot_idx(c as *const AtomicU64 as usize)].load(Relaxed) == 0 {
                        c.store(v, Release)
                    } else {
                        Versioned::slow_store(c, v)
                    }
                }),
                (0, 3) => store_bench::<0, _>(dur, |c, v| {
                    if !hot_store_fast(c, v, rs) {
                        Versioned::slow_store(c, v)
                    }
                }),
                (_, 0) => store_bench::<1, _>(dur, |c, v| c.store(v, Release)),
                (_, 1) => store_bench::<1, _>(dur, |c, v| Versioned::slow_store(c, v)),
                (_, 2) => store_bench::<1, _>(dur, |c, v| {
                    if HOT[hot_idx(c as *const AtomicU64 as usize)].load(Relaxed) == 0 {
                        c.store(v, Release)
                    } else {
                        Versioned::slow_store(c, v)
                    }
                }),
                (_, _) => store_bench::<1, _>(dur, |c, v| {
                    if !hot_store_fast(c, v, rs) {
                        Versioned::slow_store(c, v)
                    }
                }),
            }
        };
        let names = [
            "S0. Store crudo (no estricto)",
            "S1. Todos instrumentados (lock+version)",
            "S2. Solo filtro, sin rseq (NO estricto)",
            "S3. Filtro + rseq (ESTRICTO)",
        ];
        let rates: Vec<f64> = (0..4u8)
            .map(|k| run(k) as f64 / dur.as_secs_f64())
            .collect();
        for (i, r) in rates.iter().enumerate() {
            println!(
                "{:<44} {:>14.0} {:>9.2} {:>8.2}x",
                names[i],
                r,
                1e9 / r,
                rates[0] / r
            );
        }
    }
    // stores sobre granulos que si estan calientes
    {
        let mem_static: Vec<AtomicU64> = (0..4096).map(|_| AtomicU64::new(0)).collect();
        for c in &mem_static {
            ensure_hot::<true>(c);
        }
        let rs = rseq_area();
        let start = Instant::now();
        let mut n = 0u64;
        while start.elapsed() < dur {
            for c in &mem_static {
                if !hot_store_fast(c, n, rs) {
                    Versioned::slow_store(c, n);
                }
            }
            n += 4096;
        }
        let rate = n as f64 / start.elapsed().as_secs_f64();
        println!(
            "-- stores sobre memoria usada con exclusivas (granulo caliente) --\n{:<44} {:>14.0} {:>9.2}",
            "S4. Filtro + rseq, camino lento", rate, 1e9 / rate
        );
    }

}

fn section_excl(all_ok: &mut bool) {
    // ---------------- 5. Exclusivas en estado estable ----------------
    let dur = Duration::from_millis(600);
    println!("\n=== 5. OPERACIONES EXCLUSIVAS EN ESTADO ESTABLE ({} ms cada una) ===", dur.as_millis());
    let max_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut thread_counts = vec![1];
    if max_threads > 1 {
        thread_counts.push(max_threads);
    }
    let runners: [(&str, Runner); 5] = [
        ("X0. CAS por instruccion (NO estricta)", run_trans::<Cmpxchg>),
        ("X1. Interprete, todos instrumentados", run_interp::<Fiel>),
        ("X2. Idioma bajo lock, todos instrumentados", run_idiom::<Fiel>),
        ("X3. Interprete, calientes+rseq", run_interp::<HotMem<true>>),
        ("X4. Idioma bajo lock, calientes+rseq", run_idiom::<HotMem<true>>),
    ];
    for &t in &thread_counts {
        println!("-- {} hilo(s) --", t);
        println!("{:<44} {:>14} {:>9} {:>9}", "variante", "incr/s", "ns/op", "vs X0");
        let mut base = 0.0;
        for (i, (n, r)) in runners.iter().enumerate() {
            let (rate, ok) = bench_excl(*r, t, dur);
            *all_ok &= ok;
            if i == 0 {
                base = rate;
            }
            println!(
                "{:<44} {:>14.0} {:>9.2} {:>8.2}x{}",
                n,
                rate,
                1e9 / rate,
                base / rate,
                if ok { "" } else { "  CONTADOR INCORRECTO" }
            );
        }
    }

}

fn section_arming() {
    // ---------------- 2. Costos de armado ----------------
    println!("\n=== 2. PRIMER LDXR SOBRE UN GRANULO FRIO (armado) vs PROTECCION DE PAGINAS ===");
    println!("membarrier RSEQ al armar, 1 hilo      : {:>8.2} us", arm_cost(2000, false));
    println!("membarrier RSEQ al armar, 2 hilos     : {:>8.2} us", arm_cost(2000, true));
    println!("mprotect armar+desarmar, 1 hilo       : {:>8.2} us", mprotect_cost(20000, false));
    println!("mprotect armar+desarmar, 2 hilos      : {:>8.2} us", mprotect_cost(20000, true));
}

fn main() {
    membarrier_register();
    let mut all_ok = true;
    // El orden importa: los granulos calientes no se enfrian, asi que primero se mide todo lo
    // que necesita memoria fria (armado y stores) y despues el estres y las exclusivas.
    section_tests();
    section_arming();
    section_stores();
    section_stress(&mut all_ok);
    section_excl(&mut all_ok);
    println!(
        "\nVerificacion de contadores y violaciones de las variantes estrictas: {}",
        if all_ok { "OK" } else { "FALLO" }
    );
}
