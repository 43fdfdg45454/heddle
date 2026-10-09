//! Microbenchmark de atomicas con hilos guest: codigo ARM64 real (copias de las cargas `atomicos_lse`, `atomicos` y
//! `pila_lockfree` del banco de pruebas de weft) ejecutado por el JIT en 8 hilos, mas LDADDAL con 1 hilo y con 8 hilos
//! sobre la misma linea y sobre lineas distintas. Comprueba los resultados exactos y mide la mediana de XB_REPS (5).
//!   cargo test --release --lib xbench -- --ignored --nocapture --test-threads=1
//! HEDDLE_MONITOR=fence|bloqueo|rseq elige el monitor (Android: fence); XB_CPUS=4 limita el proceso a 4 CPU (el
//! emulador de pruebas da 4 vCPU a los 8 hilos del banco); XB_ONLY=lse,llsc,ldadd,pila elige las cargas.
//! Nucleos ensamblados con clang --target=aarch64-linux-android (fuente en el comentario de CODE).
//! Modulo solo de pruebas (lib.rs lo declara con #[cfg(test)]; arch_lint descarta el archivo desde esa marca).
use std::sync::atomic::Ordering::*;
use std::sync::Barrier;
use std::time::Instant;

/// Palabras de este ensamblador (clang --target=aarch64-linux-android -march=armv8.1-a; llvm-objcopy -O binary):
///     // Nucleos ARM64 del microbenchmark de regresion (copias de las cargas del banco). Fin: svc #0.
///     .text
///     .globl k_lse_mix
///     // x10 = base de `at` (alineada a 128), x11 = t, x15 = iteraciones (at_thread_lse del banco)
///     k_lse_mix:
///       mov x8, xzr
///       mov x9, xzr
///       add x12, x10, x11, lsl #7
///       add w13, w11, #0x1
///       add x14, x10, x11, lsl #3
///       sxtw x11, w13
///       mov w13, #0x1
///       add x12, x12, #0x80
///       add x14, x14, #0x480
///     1:
///       ldadd x13, x16, [x10]
///       ldaddal x9, x16, [x12]
///       add x9, x9, #0x1
///       ldaddal x13, x16, [x14]
///       cmp x9, x15
///       add x16, x10, #0x500
///       ldeor x8, x16, [x16]
///       add x8, x8, x11
///       b.ne 1b
///       svc #0
///     .globl k_llsc_mix
///     // x10 = base de `at`, x11 = t (sxtw), x9 = iteraciones (at_thread del banco)
///     k_llsc_mix:
///       add x13, x10, x11, lsl #3
///       mov x8, xzr
///       add x12, x10, x11, lsl #7
///       add w11, w11, #0x1
///       sxtw x11, w11
///       add x13, x13, #0x480
///       add x14, x10, #0x500
///       add x12, x12, #0x80
///     1:
///       ldxr x15, [x10]
///       add x15, x15, #0x1
///       stxr w16, x15, [x10]
///       cbnz w16, 1b
///     2:
///       ldaxr x15, [x12]
///       add x15, x15, x8
///       stlxr w16, x15, [x12]
///       cbnz w16, 2b
///     3:
///       ldaxr x15, [x13]
///       add x15, x15, #0x1
///       stlxr w16, x15, [x13]
///       cbnz w16, 3b
///       mul x15, x8, x11
///     4:
///       ldxr x16, [x14]
///       eor x16, x16, x15
///       stxr w17, x16, [x14]
///       cbnz w17, 4b
///       add x8, x8, #0x1
///       cmp x8, x9
///       b.ne 1b
///       svc #0
///     .globl k_ldadd
///     // x0 = direccion, x15 = iteraciones: LDADDAL en bucle
///     k_ldadd:
///       mov x1, #1
///     1:
///       ldaddal x1, x2, [x0]
///       subs x15, x15, #1
///       b.ne 1b
///       svc #0
///     .globl k_pila
///     // x0 = t, x1 = &cabeza, x2 = pool (nodos de 64 B), x3 = iteraciones (st_thread del banco; sched_yield -> yield)
///     k_pila:
///       mov w20, wzr
///       mov x21, x1
///       mov x22, x2
///       add w23, w0, #1
///       mov x24, #0x100000000
///       mov x26, x3
///       b 3f
///     1: tbz w10, #31, 5f
///     2: yield
///     3: ldar x11, [x21]
///       b 4f
///     10: mov w12, wzr
///       clrex
///       tbnz w12, #0, 1b
///     4: mov x9, x11
///       cbz w9, 2b
///       sub w10, w9, #1
///       add x8, x22, x10, lsl #6
///       ldr w12, [x8]
///       ldaxr x11, [x21]
///       cmp x11, x9
///       b.ne 10b
///       and x13, x9, #0xffffffff00000000
///       orr x12, x13, x12
///       add x12, x12, x24
///       stxr w13, x12, [x21]
///       cmp w13, #0
///       csetm w12, eq
///       tbz w12, #0, 4b
///       b 1b
///     5: and w10, w20, #0xf
///       ldr x11, [x8, #8]
///       add w10, w10, #1
///       smaddl x10, w23, w10, x11
///       str x10, [x8, #8]
///       mov x10, #0x100000000
///       ldr x11, [x21]
///       bfxil x10, x9, #0, #32
///       b 7f
///     6: mov w12, wzr
///       clrex
///       mov x11, x9
///       tbnz w12, #0, 8f
///     7: str w11, [x8]
///       ldxr x9, [x21]
///       cmp x9, x11
///       b.ne 6b
///       and x11, x11, #0xffffffff00000000
///       add x11, x10, x11
///       stlxr w12, x11, [x21]
///       cmp w12, #0
///       csetm w12, eq
///       mov x11, x9
///       tbz w12, #0, 7b
///     8: add w20, w20, #1
///       cmp w20, w26
///       b.lo 3b
///       svc #0
const CODE: [u32; 111] = [
    0xaa1f03e8, 0xaa1f03e9, 0x8b0b1d4c, 0x1100056d, 0x8b0b0d4e, 0x93407dab, 0x5280002d, 0x9102018c,
    0x911201ce, 0xf82d0150, 0xf8e90190, 0x91000529, 0xf8ed01d0, 0xeb0f013f, 0x91140150, 0xf8282210,
    0x8b0b0108, 0x54ffff01, 0xd4000001, 0x8b0b0d4d, 0xaa1f03e8, 0x8b0b1d4c, 0x1100056b, 0x93407d6b,
    0x911201ad, 0x9114014e, 0x9102018c, 0xc85f7d4f, 0x910005ef, 0xc8107d4f, 0x35ffffb0, 0xc85ffd8f,
    0x8b0801ef, 0xc810fd8f, 0x35ffffb0, 0xc85ffdaf, 0x910005ef, 0xc810fdaf, 0x35ffffb0, 0x9b0b7d0f,
    0xc85f7dd0, 0xca0f0210, 0xc8117dd0, 0x35ffffb1, 0x91000508, 0xeb09011f, 0x54fffda1, 0xd4000001,
    0xd2800021, 0xf8e10002, 0xf10005ef, 0x54ffffc1, 0xd4000001, 0x2a1f03f4, 0xaa0103f5, 0xaa0203f6,
    0x11000417, 0xd2c00038, 0xaa0303fa, 0x14000003, 0x36f802ea, 0xd503203f, 0xc8dffeab, 0x14000004,
    0x2a1f03ec, 0xd5033f5f, 0x3707ff4c, 0xaa0b03e9, 0x34ffff29, 0x5100052a, 0x8b0a1ac8, 0xb940010c,
    0xc85ffeab, 0xeb09017f, 0x54fffec1, 0x92607d2d, 0xaa0c01ac, 0x8b18018c, 0xc80d7eac, 0x710001bf,
    0x5a9f13ec, 0x3607fe4c, 0x17ffffea, 0x12000e8a, 0xf940050b, 0x1100054a, 0x9b2a2eea, 0xf900050a,
    0xd2c0002a, 0xf94002ab, 0xb3407d2a, 0x14000005, 0x2a1f03ec, 0xd5033f5f, 0xaa0903eb, 0x3700018c,
    0xb900010b, 0xc85f7ea9, 0xeb0b013f, 0x54ffff21, 0x92607d6b, 0x8b0b014b, 0xc80cfeab, 0x7100019f,
    0x5a9f13ec, 0xaa0903eb, 0x3607fecc, 0x11000694, 0x6b1a029f, 0x54fffa23, 0xd4000001,
];
const K_LSE_MIX: usize = 0;
const K_LLSC_MIX: usize = 0x4c / 4;
const K_LDADD: usize = 0xc0 / 4;
const K_PILA: usize = 0xd4 / 4;

#[repr(C, align(4096))]
struct Page([u32; 1024]);

#[repr(C, align(128))]
struct At {
    shared: u64,
    _p0: [u64; 15],
    line: [[u64; 16]; 8],
    packed: [u64; 8],
    _p1: [u64; 8],
    xr: u64,
    _p2: [u64; 15],
}

/// Ejecuta `n` hilos guest; cada uno arranca en `entry` con los registros que devuelve `regs(t)`. Devuelve ms.
fn run(code: &Page, entry: usize, n: usize, regs: &(dyn Fn(usize) -> Vec<(usize, u64)> + Sync)) -> f64 {
    let bar = Barrier::new(n + 1);
    let pc = code.0.as_ptr() as u64 + 4 * entry as u64;
    let mut ms = 0.0;
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..n)
            .map(|t| {
                let bar = &bar;
                s.spawn(move || unsafe {
                    let g = &mut *crate::rt::ensure_thread(256 << 10);
                    for (r, v) in regs(t) {
                        g.cpu.x[r] = v;
                    }
                    g.cpu.pc = pc;
                    bar.wait();
                    let ev = g.jit.run(&mut g.cpu);
                    assert!(matches!(ev, crate::jit::Event::Svc(0)), "evento inesperado");
                })
            })
            .collect();
        bar.wait();
        let t0 = Instant::now();
        for h in hs {
            h.join().unwrap();
        }
        ms = t0.elapsed().as_nanos() as f64 / 1e6;
    });
    ms
}

fn report(name: &str, ops: u64, mut f: impl FnMut() -> f64) {
    let reps = std::env::var("XB_REPS").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(5);
    let mut v: Vec<f64> = (0..reps).map(|_| f()).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (q1, med, q3) = (v[reps / 4], v[reps / 2], v[(3 * reps) / 4]);
    println!("XB {:<44} med {:>8.1} ms  q1 {:>8.1}  q3 {:>8.1}  min {:>8.1}  ({:.1} ns/op)", name, med, q1, q3, v[0], med * 1e6 / ops as f64);
}

#[test]
#[ignore]
fn bench_atomicas_hilos() {
    crate::monitor::init();
    crate::jit::HELPER_COUNT_ON.store(false, SeqCst); // como en produccion (se ejecuta solo, --test-threads=1)
    // XB_CPUS=n: todos los hilos comparten las CPU 0..n-1 (Cuttlefish: 4 vCPU para los 8 hilos del banco)
    if let Some(n) = std::env::var("XB_CPUS").ok().and_then(|v| v.parse::<u32>().ok()) {
        extern "C" {
            fn sched_setaffinity(pid: i32, size: usize, mask: *const u64) -> i32;
        }
        let mask: [u64; 16] = std::array::from_fn(|i| if i == 0 { (1u64 << n) - 1 } else { 0 });
        assert_eq!(unsafe { sched_setaffinity(0, 128, mask.as_ptr()) }, 0);
    }
    println!("XB modo={} cpus={}", crate::monitor::mode_name(), std::env::var("XB_CPUS").unwrap_or_default());
    let mut page = Box::new(Page([0; 1024]));
    page.0[..CODE.len()].copy_from_slice(&CODE);
    let code = &*page;
    let only = std::env::var("XB_ONLY").unwrap_or_default();
    let want = |k: &str| only.is_empty() || only.split(',').any(|x| x == k);

    // atomicos_lse / atomicos del banco: 8 hilos, 4 atomicas por iteracion
    let it: u64 = 200_000;
    for (key, name, entry, itreg) in [("lse", "lse_mix 8 hilos (atomicos_lse)", K_LSE_MIX, 15usize), ("llsc", "llsc_mix 8 hilos (atomicos)", K_LLSC_MIX, 9)] {
        if !want(key) {
            continue;
        }
        report(name, it, || {
            let mut at: Box<At> = unsafe { Box::new(std::mem::zeroed()) };
            let base = &mut *at as *mut At as u64;
            let ms = run(code, entry, 8, &|t| vec![(10, base), (11, t as u64), (itreg, it)]);
            assert_eq!(at.shared, 8 * it);
            let mut xr = 0u64;
            for t in 0..8 {
                assert_eq!(at.line[t][0], it * (it - 1) / 2);
                assert_eq!(at.packed[t], it);
                for i in 0..it {
                    xr ^= i * (t as u64 + 1);
                }
            }
            assert_eq!(at.xr, xr);
            ms
        });
    }
    // LDADDAL: 1 hilo; 8 hilos misma linea; 8 hilos lineas distintas (128 B)
    if want("ldadd") {
        let mut cells: Box<At> = unsafe { Box::new(std::mem::zeroed()) };
        let line0 = cells.line.as_mut_ptr() as u64;
        let n1: u64 = 2_000_000;
        report("ldadd 1 hilo", n1, || run(code, K_LDADD, 1, &|_| vec![(0, line0), (15, n1)]));
        let n8: u64 = 300_000;
        report("ldadd 8 hilos misma linea", n8, || run(code, K_LDADD, 8, &|_| vec![(0, line0), (15, n8)]));
        report("ldadd 8 hilos lineas distintas", n8, || run(code, K_LDADD, 8, &|t| vec![(0, line0 + 128 * t as u64), (15, n8)]));
    }
    // pila_lockfree del banco: 8 hilos x 200 000 pop/store/push, 32 nodos
    if want("pila") {
        let iters: u64 = 200_000;
        report("pila 8 hilos (pila_lockfree)", iters, || {
            #[repr(C, align(128))]
            struct Head(u64);
            let mut head = Box::new(Head(0));
            let mut pool: Box<[[u64; 8]; 64]> = Box::new([[0; 8]; 64]);
            for i in 0..32u64 {
                pool[i as usize][0] = (head.0 & 0xffff_ffff) as u64; // next (u32) = cabeza anterior
                head.0 = (((head.0 >> 32) + 1) << 32) | (i + 1);
            }
            let (hp, pp) = (&head.0 as *const u64 as u64, pool.as_mut_ptr() as u64);
            let ms = run(code, K_PILA, 8, &|t| vec![(0, t as u64), (1, hp), (2, pp), (3, iters)]);
            // vaciar y comprobar
            let mut per = 0u64;
            for i in 0..iters {
                per += (i & 15) + 1;
            }
            let exp: u64 = (1..=8u64).map(|k| k * per).sum();
            let (mut cnt, mut tot, mut h) = (0u64, 0u64, head.0 as u32);
            while h != 0 {
                tot += pool[h as usize - 1][1];
                cnt += 1;
                h = pool[h as usize - 1][0] as u32;
                assert!(cnt <= 32);
            }
            assert_eq!((cnt, tot), (32, exp));
            ms
        });
    }
}

