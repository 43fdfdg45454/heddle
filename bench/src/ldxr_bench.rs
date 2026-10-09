// Benchmark: traducir el bucle atómico de ARM64 (LDXR/STXR) a x86-64.
//
// Código guest (ARM64) de un contador atómico:
//
//   retry:
//     ldxr  x0, [x1]        ; leer y reservar la dirección (monitor exclusivo)
//     add   x0, x0, #1
//     stxr  w2, x0, [x1]    ; escribir solo si nadie tocó la dirección
//     cbnz  w2, retry       ; si falló, reintentar
//
// x86 no tiene monitor exclusivo, así que hay tres estrategias de traducción:
//
//   1. Intérprete fiel:       decodifica y emula el monitor (versión + spinlock).
//   2. Traducción por instr.: LDXR -> load, STXR -> lock cmpxchg (compara valor).
//                             Rápida, pero no es fiel (problema ABA).
//   3. Traducción por idioma: reconoce el patrón completo -> lock xadd.
//                             La más rápida; es lo que hacen los traductores buenos.
//
// Métrica: incrementos exitosos del contador compartido por segundo.
//
// Compilar: rustc -C opt-level=3 ldxr_bench.rs -o ldxr_bench

use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::thread;
use std::time::{Duration, Instant};

const RUN_TIME: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// 1. Intérprete fiel
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Insn {
    Ldxr,
    AddImm(u64),
    Stxr,
    CbnzRetry,
}

// Estado global del "monitor" para la dirección compartida:
// cualquier escritura exitosa incrementa la versión, lo que invalida
// las reservas de los demás núcleos (aunque el valor vuelva a ser el mismo).
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

// ---------------------------------------------------------------------------
// 2. Traducción instrucción por instrucción (LDXR -> load, STXR -> cmpxchg)
// ---------------------------------------------------------------------------

fn run_cas_translation(cell: &AtomicU64, stop: &AtomicBool) -> (u64, u64) {
    let (mut ok, mut fail) = (0u64, 0u64);
    let mut n = 0u64;
    loop {
        let old = cell.load(Acquire); // ldxr
        let new = old.wrapping_add(1); // add
        match cell.compare_exchange(old, new, AcqRel, Relaxed) {
            // stxr + cbnz
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

// ---------------------------------------------------------------------------
// 3. Traducción por idioma (patrón ldxr/add/stxr/cbnz -> lock xadd)
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

type Runner = fn(&AtomicU64, &AtomicBool) -> (u64, u64);

struct Outcome {
    ok: u64,
    fail: u64,
    secs: f64,
    final_value: u64,
}

fn bench(runner: Runner, threads: usize) -> Outcome {
    static CELL: AtomicU64 = AtomicU64::new(0);
    CELL.store(0, SeqCst);
    MONITOR_VER.store(0, SeqCst);
    let stop = AtomicBool::new(false);

    let start = Instant::now();
    let (ok, fail) = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| s.spawn(|| runner(&CELL, &stop)))
            .collect();
        thread::sleep(RUN_TIME);
        stop.store(true, SeqCst);
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    });
    let secs = start.elapsed().as_secs_f64();

    Outcome {
        ok,
        fail,
        secs,
        final_value: CELL.load(SeqCst),
    }
}

fn main() {
    let max_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let mut thread_counts = vec![1];
    if max_threads > 1 {
        thread_counts.push(max_threads);
    }

    let cases: [(&str, Runner); 3] = [
        ("1. Interprete fiel (monitor emulado)", run_interpreter),
        ("2. Traduccion por instruccion (cmpxchg)", run_cas_translation),
        ("3. Traduccion por idioma (lock xadd)", run_idiom_translation),
    ];

    println!("Incrementos atomicos exitosos en ~{} s por caso\n", RUN_TIME.as_secs());

    let mut all_correct = true;
    for &threads in &thread_counts {
        println!("== {} hilo(s) ==", threads);
        println!(
            "{:<42} {:>14} {:>10} {:>12} {:>9}",
            "caso", "ops/s", "ns/op", "reintentos", "vs idioma"
        );

        let results: Vec<_> = cases
            .iter()
            .map(|(name, runner)| (*name, bench(*runner, threads)))
            .collect();
        let best = results
            .iter()
            .map(|(_, o)| o.ok as f64 / o.secs)
            .fold(0.0, f64::max);

        for (name, o) in &results {
            let rate = o.ok as f64 / o.secs;
            let correct = o.final_value == o.ok;
            all_correct &= correct;
            println!(
                "{:<42} {:>14.0} {:>10.1} {:>12} {:>8.1}x{}",
                name,
                rate,
                1e9 / rate,
                o.fail,
                best / rate,
                if correct { "" } else { "  (contador INCORRECTO)" }
            );
        }
        println!();
    }

    println!(
        "Verificacion (contador final == incrementos exitosos): {}",
        if all_correct { "OK" } else { "FALLO" }
    );
}
