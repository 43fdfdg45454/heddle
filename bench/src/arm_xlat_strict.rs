// Comparacion de traducciones ARM64 -> x86-64, SOLO entre alternativas funcionalmente equivalentes.
//
// Metodo: cada estrategia candidata pasa primero pruebas diferenciales contra el comportamiento
// que exige ARM. Las que fallan alguna prueba se DESCARTAN y no se miden. El tiempo total se
// reparte en partes iguales entre las corridas de las estrategias que sobreviven.
//
//   A. FLAGS            subs + csel hs + csel gt, y las 14 condiciones deben poder leerse despues
//   B. MONITOR EXCL.    ldxr/stxr: debe fallar ante CUALQUIER escritura intermedia (aunque
//                       devuelva el mismo valor), de otro core, normal o atomica
//   C. SIMD             uqadd .4s: igual que u32::saturating_add en todos los casos borde
//   D. COSTO            lo que cuesta instrumentar las escrituras normales para que B sea fiel
//
// Compilar: rustc --edition 2021 -C opt-level=3 arm_xlat_strict.rs -o arm_xlat_strict

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
// ===========================================================================

#[derive(Clone, Copy)]
enum Cond {
    Hs,
    Gt,
}

struct Cpu {
    x: [u64; 8],
    nzcv: u8,    // flags "eager": N=3 Z=2 C=1 V=0
    lazy_a: u64, // flags perezosas: operandos del ultimo SUBS
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

// Fusion SIN guardar el estado de las flags: las flags quedan perdidas al salir del bloque.
#[inline(never)]
fn block_fused_nostate(cpu: &mut Cpu) {
    let a = cpu.x[0];
    let b = cpu.x[1];
    cpu.x[2] = a.wrapping_sub(b);
    cpu.x[3] = if a >= b { a } else { b };
    cpu.x[4] = if (a as i64) > (b as i64) { a } else { b };
    cpu.x[5] = cpu.x[5].wrapping_add(cpu.x[3]) ^ cpu.x[4];
}

// Fusion guardando los operandos (2 stores) para poder reconstruir las flags despues.
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
        z == 1,               // EQ
        z == 0,               // NE
        c == 1,               // HS
        c == 0,               // LO
        n == 1,               // MI
        n == 0,               // PL
        v == 1,               // VS
        v == 0,               // VC
        c == 1 && z == 0,     // HI
        !(c == 1 && z == 0),  // LS
        n == v,               // GE
        n != v,               // LT
        z == 0 && n == v,     // GT
        z == 1 || n != v,     // LE
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

// Equivalencia observable al salir del bloque: registros x0..x5 y las 14 condiciones.
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
// B. MONITOR EXCLUSIVO
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
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool; // true = exito (w2 == 0)
    fn store(cell: &AtomicU64, v: u64); // store normal del guest (str)
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64; // bucle ldxr/add/stxr/cbnz reconocido
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

// M1: monitor con version, pero solo las exclusivas la incrementan; stores normales sin instrumentar.
struct SoloExcl;
impl Mem for SoloExcl {
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
    fn store(cell: &AtomicU64, v: u64) {
        cell.store(v, Release);
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        generic_add::<Self>(cell, imm)
    }
}

// M2: ldxr = load, stxr = lock cmpxchg comparando el valor.
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

// M4: monitor fiel: version por granulo; las escrituras normales tambien toman el lock y la incrementan.
struct Fiel;
impl Mem for Fiel {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        SoloExcl::ldxr(mon, cell)
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        SoloExcl::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        let g = gran(cell);
        glock(g);
        cell.store(v, Release);
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
        gunlock(g);
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        generic_add::<Self>(cell, imm)
    }
}

// M3: idioma -> lock xadd directo, conviviendo con el camino generico fiel (M4).
struct XaddIdiom;
impl Mem for XaddIdiom {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        Fiel::ldxr(mon, cell)
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        Fiel::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        Fiel::store(cell, v)
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        cell.fetch_add(imm, AcqRel).wrapping_add(imm)
    }
}

// M5: idioma reconocido, pero ejecutado bajo el mismo lock/version que el camino generico.
struct LockIdiom;
impl Mem for LockIdiom {
    fn ldxr(mon: &mut Mon, cell: &AtomicU64) -> u64 {
        Fiel::ldxr(mon, cell)
    }
    fn stxr(mon: &mut Mon, cell: &AtomicU64, val: u64) -> bool {
        Fiel::stxr(mon, cell, val)
    }
    fn store(cell: &AtomicU64, v: u64) {
        Fiel::store(cell, v)
    }
    fn idiom_add(cell: &AtomicU64, imm: u64) -> u64 {
        let g = gran(cell);
        glock(g);
        let v = cell.load(Relaxed).wrapping_add(imm);
        cell.store(v, Release);
        g.ver.store(g.ver.load(Relaxed).wrapping_add(1), Release);
        gunlock(g);
        v
    }
}

const MON_TESTS: [&str; 7] = [
    "E1 ABA con stores",
    "E2 store otro valor",
    "E3 store mismo valor",
    "E4 otro core excl.",
    "E5 idioma vs LL/SC",
    "E6 idioma ABA",
    "E7 sin fallo espurio",
];

// Pruebas deterministas: se intercalan a mano las acciones de dos "cores".
fn monitor_tests<M: Mem>() -> Vec<bool> {
    let mut res = Vec::new();
    // E1: A -> B -> A con stores normales; el stxr debe fallar
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    // E2: otro core escribe un valor distinto; el stxr debe fallar
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 7);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    // E3: otro core escribe el MISMO valor; ARM exige que el stxr falle
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::store(&c, 100);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    // E4: otro core completa su par ldxr/stxr; el primero debe fallar
    {
        let c = AtomicU64::new(100);
        let (mut m1, mut m2) = (Mon::new(), Mon::new());
        M::ldxr(&mut m1, &c);
        M::ldxr(&mut m2, &c);
        let ok2 = M::stxr(&mut m2, &c, 101);
        let ok1 = M::stxr(&mut m1, &c, 101);
        res.push(ok2 && !ok1);
    }
    // E5: otro core ejecuta el idioma (+1); el stxr pendiente debe fallar
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        res.push(!M::stxr(&mut m, &c, 101));
    }
    // E6: el idioma sube y baja (A -> A+1 -> A); el stxr pendiente debe fallar
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        M::idiom_add(&c, 1);
        M::idiom_add(&c, u64::MAX);
        res.push(c.load(SeqCst) == 100 && !M::stxr(&mut m, &c, 101));
    }
    // E7: sin interferencia el stxr debe tener exito
    {
        let c = AtomicU64::new(100);
        let mut m = Mon::new();
        M::ldxr(&mut m, &c);
        let ok = M::stxr(&mut m, &c, 101);
        res.push(ok && c.load(SeqCst) == 101);
    }
    res
}

type Runner = fn(&AtomicU64, &AtomicBool) -> (u64, u64);

#[derive(Clone, Copy)]
enum Insn {
    Ldxr,
    AddImm(u64),
    Stxr,
    CbnzRetry,
}

// Interprete: decodifica las 4 instrucciones guest y usa las primitivas de M.
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

// Traduccion por instruccion (sin decodificar en tiempo de ejecucion).
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

// Traduccion por idioma: el bucle completo se reemplaza por M::idiom_add.
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

struct Outcome {
    ok: u64,
    fail: u64,
    secs: f64,
    final_value: u64,
}

fn bench_excl(runner: Runner, threads: usize, dur: Duration) -> Outcome {
    static CELL: AtomicU64 = AtomicU64::new(0);
    CELL.store(0, SeqCst);
    let g = gran(&CELL);
    g.ver.store(0, SeqCst);
    g.lock.store(false, SeqCst);
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
    // aleatorio con magnitudes variadas
    let (mut acc, data) = make_simd_data();
    let expected = expected_sat(&acc, &data);
    pass(&mut acc, &data);
    if acc != expected {
        return false;
    }
    // todos los pares de valores borde, con lanes mezcladas
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
// D. Costo de las escrituras normales del guest
// ===========================================================================

fn run_stores<M: Mem>(dur: Duration) -> u64 {
    let mem: Vec<AtomicU64> = (0..4096).map(|_| AtomicU64::new(0)).collect();
    let deadline = Instant::now() + dur;
    let mut rounds = 0u64;
    loop {
        for _ in 0..16 {
            for (i, c) in mem.iter().enumerate() {
                M::store(c, (i as u64) ^ rounds);
            }
        }
        rounds += 16;
        if Instant::now() >= deadline {
            break;
        }
    }
    black_box(&mem);
    rounds * 4096
}

// ===========================================================================

fn verdict(b: bool) -> &'static str {
    if b {
        "PASA"
    } else {
        "FALLA"
    }
}

fn print_row(name: &str, rate: f64, best: f64, extra: &str) {
    println!(
        "{:<46} {:>14.0} {:>9.2} {:>8.2}x  {}",
        name,
        rate,
        1e9 / rate,
        best / rate,
        extra
    );
}

fn header(title: &str, unit: &str) {
    println!("--- {} ---", title);
    println!("{:<46} {:>14} {:>9} {:>9}", "estrategia", unit, "ns/op", "vs mejor");
}

struct MonCand {
    name: &'static str,
    tests: Vec<bool>,
    runner: Runner,
}

fn main() {
    let max_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut thread_counts = vec![1];
    if max_threads > 1 {
        thread_counts.push(max_threads);
    }
    let has_sse41 = is_x86_feature_detected!("sse4.1");
    let mut all_ok = true;

    // ---------------- Pruebas de equivalencia ----------------
    println!("=== PRUEBAS DE EQUIVALENCIA FUNCIONAL (lo que falla se descarta) ===\n");

    let flag_cands: [(&str, fn(&mut Cpu), bool); 4] = [
        ("A1. Flags eager (NZCV tras cada subs)", block_eager, false),
        ("A2. Flags perezosas (guarda operandos)", block_lazy, true),
        ("A3. Fusion cmp+cmov SIN guardar flags", block_fused_nostate, true),
        ("A4. Fusion cmp+cmov + operandos guardados", block_fused_state, true),
    ];
    println!("A. FLAGS: x0..x5 y las 14 condiciones iguales a las de ARM al salir del bloque");
    let flag_pass: Vec<bool> = flag_cands
        .iter()
        .map(|(n, b, lazy)| {
            let ok = flags_equivalent(*b, *lazy);
            println!("   {:<46} {}", n, verdict(ok));
            ok
        })
        .collect();

    println!("\nB. MONITOR EXCLUSIVO");
    let mon_cands: Vec<MonCand> = vec![
        MonCand {
            name: "M1. Interprete, version solo en exclusivas",
            tests: monitor_tests::<SoloExcl>(),
            runner: run_interp::<SoloExcl>,
        },
        MonCand {
            name: "M2. Por instruccion, lock cmpxchg",
            tests: monitor_tests::<Cmpxchg>(),
            runner: run_trans::<Cmpxchg>,
        },
        MonCand {
            name: "M3. Idioma lock xadd (+ generico fiel)",
            tests: monitor_tests::<XaddIdiom>(),
            runner: run_idiom::<XaddIdiom>,
        },
        MonCand {
            name: "M4. Interprete fiel (version + stores instr.)",
            tests: monitor_tests::<Fiel>(),
            runner: run_interp::<Fiel>,
        },
        MonCand {
            name: "M5. Idioma bajo el lock/version del monitor",
            tests: monitor_tests::<LockIdiom>(),
            runner: run_idiom::<LockIdiom>,
        },
    ];
    print!("   {:<46}", "");
    for t in MON_TESTS.iter() {
        print!(" {:<3}", &t[..2]);
    }
    println!();
    for c in &mon_cands {
        print!("   {:<46}", c.name);
        for r in &c.tests {
            print!(" {:<3}", if *r { "ok" } else { "XX" });
        }
        println!("  => {}", verdict(c.tests.iter().all(|r| *r)));
    }
    println!("   (E1 ABA con stores, E2 store otro valor, E3 store mismo valor, E4 otro core excl.,");
    println!("    E5 idioma vs LL/SC, E6 idioma ABA, E7 sin fallo espurio)");

    println!("\nC. SIMD: igual que u32::saturating_add (aleatorio + todos los pares borde)");
    let mut simd_cands: Vec<(&str, fn(&mut [[u32; 4]], &[[u32; 4]]))> =
        vec![("C1. Escalar por lane (add+sbb+or)", simd_pass_scalar)];
    if has_sse41 {
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

    // ---------------- Reparto del tiempo ----------------
    let flag_sel: Vec<_> = flag_cands
        .iter()
        .zip(&flag_pass)
        .filter(|(_, ok)| **ok)
        .map(|(c, _)| *c)
        .collect();
    let mon_sel: Vec<&MonCand> = mon_cands
        .iter()
        .filter(|c| c.tests.iter().all(|r| *r))
        .collect();
    let simd_sel: Vec<_> = simd_cands
        .iter()
        .zip(&simd_pass)
        .filter(|(_, ok)| **ok)
        .map(|(c, _)| *c)
        .collect();

    let runs = flag_sel.len() + mon_sel.len() * thread_counts.len() + simd_sel.len() + 2;
    let per_run = TOTAL_BUDGET / runs as u32;
    println!(
        "\n=== MEDICION (solo estrategias que pasan todo): {} corridas de {:.2} s ===\n",
        runs,
        per_run.as_secs_f64()
    );
    let t0 = Instant::now();

    // A
    let res: Vec<(&str, f64)> = flag_sel
        .iter()
        .map(|(n, b, _)| (*n, run_flags(*b, per_run) as f64 / per_run.as_secs_f64()))
        .collect();
    let best = res.iter().map(|r| r.1).fold(0.0, f64::max);
    header("A. FLAGS (subs + 2x csel, 5 instr. guest por iteracion)", "iter/s");
    for (n, r) in &res {
        print_row(n, *r, best, "");
    }
    println!();

    // B
    for &threads in &thread_counts {
        let outs: Vec<_> = mon_sel
            .iter()
            .map(|c| (c.name, bench_excl(c.runner, threads, per_run)))
            .collect();
        let best = outs.iter().map(|(_, o)| o.ok as f64 / o.secs).fold(0.0, f64::max);
        header(&format!("B. MONITOR EXCLUSIVO, {} hilo(s)", threads), "incr/s");
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

    // C
    let sres: Vec<(&str, f64)> = simd_sel
        .iter()
        .map(|(n, p)| (*n, run_simd(*p, per_run) as f64 / per_run.as_secs_f64()))
        .collect();
    let best = sres.iter().map(|r| r.1).fold(0.0, f64::max);
    header("C. SIMD: uqadd .4s (4 lanes u32 saturadas por operacion)", "vec/s");
    for (n, r) in &sres {
        print_row(n, *r, best, "");
    }
    println!();

    // D
    let d1 = run_stores::<SoloExcl>(per_run) as f64 / per_run.as_secs_f64();
    let d2 = run_stores::<Fiel>(per_run) as f64 / per_run.as_secs_f64();
    let best = d1.max(d2);
    header("D. COSTO de un store normal del guest (str)", "store/s");
    print_row("D1. Store normal sin instrumentar (no fiel)", d1, best, "");
    print_row("D2. Store normal instrumentado (exigido por M4/M5)", d2, best, "");
    println!();

    println!("Tiempo de medicion: {:.1} s", t0.elapsed().as_secs_f64());
    println!(
        "Verificacion de contadores: {}",
        if all_ok { "OK" } else { "FALLO" }
    );
}
