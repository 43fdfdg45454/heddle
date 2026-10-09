// Benchmark: tres casos dificiles de traducir de ARM64 a x86-64.
//
//   A. FLAGS            SUBS + B.HS / B.GT  (ARM: C = "no hubo prestamo"; x86: CF = "hubo prestamo")
//   B. MONITOR EXCLUSIVO  LDXR/ADD/STXR/CBNZ  (x86 no tiene reserva de direccion)
//   C. SIMD             UQADD v.4s  (suma saturada de 4x u32; SSE2/SSE4.1 solo trae 8 y 16 bits)
//
// Para cada caso hay varias estrategias de traduccion. Todas se verifican entre si
// antes de medir. El presupuesto TOTAL se reparte en partes iguales entre todas las corridas.
//
// Compilar: rustc --edition 2021 -C opt-level=3 arm_xlat_bench.rs -o arm_xlat_bench

use std::arch::asm;
use std::arch::x86_64::*;
use std::hint::{black_box, spin_loop};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::thread;
use std::time::{Duration, Instant};

const TOTAL_BUDGET: Duration = Duration::from_secs(20);

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
// A. FLAGS
//
// Guest (ARM64), por iteracion:
//   subs x2, x0, x1          ; fija N Z C V
//   csel x3, x0, x1, hs      ; usa C   (max sin signo)
//   csel x4, x0, x1, gt      ; usa Z N V (max con signo)
//   add/eor acumulador
// ===========================================================================

#[derive(Clone, Copy)]
enum Cond {
    Hs,
    Gt,
}

struct Cpu {
    x: [u64; 8],
    nzcv: u8, // bits: N=3 Z=2 C=1 V=0 (flags "eager")
    lazy_a: u64, // operandos del ultimo SUBS (flags "perezosas")
    lazy_b: u64,
}

// --- A1: flags eager: se calculan N,Z,C,V completos despues de cada SUBS ---
#[inline(never)]
fn subs_eager(cpu: &mut Cpu, rd: usize, rn: usize, rm: usize) {
    let a = cpu.x[rn];
    let b = cpu.x[rm];
    let r = a.wrapping_sub(b);
    cpu.x[rd] = r;
    let n = (r >> 63) as u8;
    let z = (r == 0) as u8;
    let c = (a >= b) as u8; // ARM: C=1 si NO hubo prestamo (inverso de CF de x86)
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

// --- A2: flags perezosas: SUBS solo guarda operandos; la condicion se calcula al consumirla ---
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

// --- A3: fusion en bloque: el traductor ve SUBS+CSEL+CSEL y emite cmp + cmov directos ---
#[inline(never)]
fn block_fused(cpu: &mut Cpu) {
    let a = cpu.x[0];
    let b = cpu.x[1];
    cpu.x[2] = a.wrapping_sub(b);
    cpu.x[3] = if a >= b { a } else { b };
    cpu.x[4] = if (a as i64) > (b as i64) { a } else { b };
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

fn new_cpu() -> Cpu {
    Cpu { x: [0; 8], nzcv: 0, lazy_a: 0, lazy_b: 0 }
}

fn checksum_flags(block: fn(&mut Cpu), iters: u64) -> u64 {
    let mut cpu = new_cpu();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    for _ in 0..iters {
        cpu.x[0] = rng.next();
        cpu.x[1] = rng.next();
        // fuerza casos borde: iguales, y cerca del limite de signo
        if rng.next() & 15 == 0 {
            cpu.x[1] = cpu.x[0];
        }
        block(&mut cpu);
    }
    cpu.x[5]
}

fn run_flags(block: fn(&mut Cpu), dur: Duration) -> u64 {
    let mut cpu = new_cpu();
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
    let deadline = Instant::now() + dur;
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
    iters
}

// ===========================================================================
// B. MONITOR EXCLUSIVO (igual que antes)
//   retry: ldxr x0,[x1] ; add x0,x0,#1 ; stxr w2,x0,[x1] ; cbnz w2,retry
// ===========================================================================

#[derive(Clone, Copy)]
enum Insn {
    Ldxr,
    AddImm(u64),
    Stxr,
    CbnzRetry,
}

static MONITOR_VER: AtomicU64 = AtomicU64::new(0);
static MONITOR_LOCK: AtomicBool = AtomicBool::new(false);

fn lock() {
    while MONITOR_LOCK
        .compare_exchange_weak(false, true, Acquire, Relaxed)
        .is_err()
    {
        spin_loop();
    }
}

fn unlock() {
    MONITOR_LOCK.store(false, Release);
}

fn run_interpreter(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let prog = [Insn::Ldxr, Insn::AddImm(1), Insn::Stxr, Insn::CbnzRetry];
    let mut pc = 0usize;
    let mut x0 = 0u64;
    let mut w2 = 0u64;
    let mut reserved = false;
    let mut reserved_ver = 0u64;
    let (mut ok, mut fail) = (0u64, 0u64);
    loop {
        match prog[pc] {
            Insn::Ldxr => {
                reserved_ver = MONITOR_VER.load(Acquire);
                x0 = cell.load(Acquire);
                reserved = true;
                pc += 1;
            }
            Insn::AddImm(i) => {
                x0 = x0.wrapping_add(i);
                pc += 1;
            }
            Insn::Stxr => {
                lock();
                if reserved && MONITOR_VER.load(Relaxed) == reserved_ver {
                    cell.store(x0, Relaxed);
                    MONITOR_VER.store(reserved_ver.wrapping_add(1), Release);
                    w2 = 0;
                } else {
                    w2 = 1;
                }
                unlock();
                reserved = false;
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

fn run_cas_translation(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let (mut ok, mut fail) = (0u64, 0u64);
    let mut n = 0u64;
    loop {
        let old = cell.load(Acquire);
        let new = old.wrapping_add(1);
        match cell.compare_exchange(old, new, AcqRel, Relaxed) {
            Ok(_) => ok += 1,
            Err(_) => fail += 1,
        }
        n += 1;
        if n & 0xFFF == 0 && stop.load(Relaxed) {
            break;
        }
    }
    (ok, fail)
}

fn run_idiom_translation(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let mut ok = 0u64;
    loop {
        cell.fetch_add(1, AcqRel);
        ok += 1;
        if ok & 0xFFF == 0 && stop.load(Relaxed) {
            break;
        }
    }
    (ok, 0)
}

type Runner = fn(&AtomicU64, &AtomicBool) -> (u64, u64);

struct Outcome {
    ok: u64,
    fail: u64,
    secs: f64,
    final_value: u64,
}

fn bench_excl(runner: Runner, threads: usize, dur: Duration) -> Outcome {
    static CELL: AtomicU64 = AtomicU64::new(0);
    CELL.store(0, SeqCst);
    MONITOR_VER.store(0, SeqCst);
    let stop = AtomicBool::new(false);
    let start = Instant::now();
    let (ok, fail) = thread::scope(|s| {
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
    Outcome {
        ok,
        fail,
        secs: start.elapsed().as_secs_f64(),
        final_value: CELL.load(SeqCst),
    }
}

// ===========================================================================
// C. SIMD:  uqadd v0.4s, v0.4s, v1.4s   (suma saturada sin signo, 4 lanes de 32 bits)
//   NEON tiene UQADD para 8/16/32/64 bits; SSE solo paddusb/paddusw (8 y 16 bits).
// ===========================================================================

const NVEC: usize = 1024;

// --- C1: traduccion ingenua: lane por lane con registros enteros (add + sbb + or) ---
fn simd_pass_scalar(acc: &mut [[u32; 4]], data: &[[u32; 4]]) {
    for (a, d) in acc.iter_mut().zip(data) {
        for i in 0..4 {
            let mut x = a[i];
            let y = d[i];
            let t: u32;
            // add x,y ; sbb t,t (t=-1 si hubo acarreo) ; or x,t  => saturacion a 0xFFFFFFFF
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

// --- C2: traduccion vectorial SSE4.1: a + min(b, ~a)  (3 instrucciones + xor) ---
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
    let mut gen = |rng: &mut XorShift| -> [u32; 4] {
        let mut v = [0u32; 4];
        for l in v.iter_mut() {
            // magnitudes variadas para que haya lanes saturadas y no saturadas
            *l = (rng.next() as u32) >> (rng.next() % 32);
        }
        v
    };
    let acc: Vec<_> = (0..NVEC).map(|_| gen(&mut rng)).collect();
    let data: Vec<_> = (0..NVEC).map(|_| gen(&mut rng)).collect();
    (acc, data)
}

fn verify_simd(pass: fn(&mut [[u32; 4]], &[[u32; 4]])) -> bool {
    let (mut acc, data) = make_simd_data();
    let expected: Vec<[u32; 4]> = acc
        .iter()
        .zip(&data)
        .map(|(a, d)| {
            let mut r = [0u32; 4];
            for i in 0..4 {
                r[i] = a[i].saturating_add(d[i]);
            }
            r
        })
        .collect();
    pass(&mut acc, &data);
    acc == expected
}

fn run_simd(pass: fn(&mut [[u32; 4]], &[[u32; 4]]), dur: Duration) -> u64 {
    let (mut acc, data) = make_simd_data();
    let deadline = Instant::now() + dur;
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
    passes * NVEC as u64
}

// ===========================================================================

fn print_row(name: &str, rate: f64, best: f64, extra: &str) {
    println!(
        "{:<44} {:>14.0} {:>9.2} {:>8.2}x  {}",
        name,
        rate,
        1e9 / rate,
        best / rate,
        extra
    );
}

fn header(title: &str, unit: &str) {
    println!("--- {} ---", title);
    println!(
        "{:<44} {:>14} {:>9} {:>9}  {}",
        "estrategia", unit, "ns/op", "vs mejor", ""
    );
}

fn main() {
    let max_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut thread_counts = vec![1];
    if max_threads > 1 {
        thread_counts.push(max_threads);
    }
    let has_sse41 = is_x86_feature_detected!("sse4.1");

    let runs = 3 + if has_sse41 { 2 } else { 1 } + 3 * thread_counts.len();
    let per_run = TOTAL_BUDGET / runs as u32;
    println!(
        "Presupuesto total {} s, {} corridas de {:.2} s cada una\n",
        TOTAL_BUDGET.as_secs(),
        runs,
        per_run.as_secs_f64()
    );

    let t0 = Instant::now();
    let mut all_ok = true;

    // ---------------- A. FLAGS ----------------
    let blocks: [(&str, fn(&mut Cpu)); 3] = [
        ("A1. Flags eager (NZCV tras cada SUBS)", block_eager),
        ("A2. Flags perezosas (guarda operandos)", block_lazy),
        ("A3. Fusion en bloque (cmp + cmov)", block_fused),
    ];
    let reference = checksum_flags(block_eager, 200_000);
    let flags_equal = blocks.iter().all(|(_, b)| checksum_flags(*b, 200_000) == reference);
    all_ok &= flags_equal;

    let results: Vec<(&str, f64)> = blocks
        .iter()
        .map(|(n, b)| (*n, run_flags(*b, per_run) as f64 / per_run.as_secs_f64()))
        .collect();
    let best = results.iter().map(|r| r.1).fold(0.0, f64::max);
    header("A. FLAGS (SUBS + 2x CSEL, 5 instr. guest por iteracion)", "iter/s");
    for (n, r) in &results {
        print_row(n, *r, best, "");
    }
    println!("equivalencia entre estrategias: {}\n", if flags_equal { "OK" } else { "FALLO" });

    // ---------------- B. MONITOR EXCLUSIVO ----------------
    let cases: [(&str, Runner); 3] = [
        ("B1. Interprete fiel (monitor emulado)", run_interpreter),
        ("B2. Por instruccion (lock cmpxchg)", run_cas_translation),
        ("B3. Por idioma (lock xadd)", run_idiom_translation),
    ];
    for &threads in &thread_counts {
        let outs: Vec<_> = cases
            .iter()
            .map(|(n, r)| (*n, bench_excl(*r, threads, per_run)))
            .collect();
        let best = outs.iter().map(|(_, o)| o.ok as f64 / o.secs).fold(0.0, f64::max);
        header(
            &format!("B. MONITOR EXCLUSIVO, {} hilo(s)", threads),
            "incr/s",
        );
        for (n, o) in &outs {
            let correct = o.final_value == o.ok;
            all_ok &= correct;
            print_row(
                n,
                o.ok as f64 / o.secs,
                best,
                &format!(
                    "reintentos={}{}",
                    o.fail,
                    if correct { "" } else { " CONTADOR INCORRECTO" }
                ),
            );
        }
        println!();
    }

    // ---------------- C. SIMD ----------------
    let mut simd_cases: Vec<(&str, fn(&mut [[u32; 4]], &[[u32; 4]]))> =
        vec![("C1. Escalar por lane (add+sbb+or)", simd_pass_scalar)];
    if has_sse41 {
        simd_cases.push(("C2. Vectorial SSE4.1 (a + min(b,~a))", simd_pass_sse41_safe));
    }
    let simd_ok = simd_cases.iter().all(|(_, p)| verify_simd(*p));
    all_ok &= simd_ok;
    let sres: Vec<(&str, f64)> = simd_cases
        .iter()
        .map(|(n, p)| (*n, run_simd(*p, per_run) as f64 / per_run.as_secs_f64()))
        .collect();
    let best = sres.iter().map(|r| r.1).fold(0.0, f64::max);
    header("C. SIMD: uqadd .4s (4 lanes u32 saturadas por operacion)", "vec/s");
    for (n, r) in &sres {
        print_row(n, *r, best, "");
    }
    println!(
        "equivalencia con u32::saturating_add: {}\n",
        if simd_ok { "OK" } else { "FALLO" }
    );

    println!("Tiempo total de pared: {:.1} s", t0.elapsed().as_secs_f64());
    println!("Verificacion global: {}", if all_ok { "OK" } else { "FALLO" });
}
