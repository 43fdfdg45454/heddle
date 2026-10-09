//! HLE ("high level emulation"): direcciones guest reservadas cuyo "codigo" es una funcion del host.
//! El guest las invoca con BL/BLR/BR normales; el despachador las detecta por rango y ejecuta el
//! manejador con los registros guest (x0..x7, v0..v7, sp). Al terminar, pc = x30 salvo que el
//! manejador pida otra cosa.

use crate::cpu::Cpu;
use crate::sys::*;
use std::collections::HashMap;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering::{Acquire, Relaxed, Release}};
use std::sync::RwLock;

pub const SLOT: u64 = 16;
pub const SLOTS: u64 = 1 << 16;
const TBI: u64 = 0x00FF_FFFF_FFFF_FFFF;

pub enum Ret {
    /// pc = x30
    Return,
    /// el manejador dejo `cpu.pc` listo (longjmp, salto, etc.)
    Stay,
}

pub type Handler = Box<dyn Fn(&mut Cpu) -> Ret + Send + Sync>;

struct Table {
    names: HashMap<String, u64>,
    list: Vec<String>,
    /// Dueno de todos los manejadores publicados en HANDLERS: nunca se liberan ni se mueven (cada Box mantiene su
    /// direccion aunque el Vec crezca). Cota: un manejador por registro, es decir, SLOTS mas los reemplazos que se
    /// hagan al registrar de nuevo un nombre existente (solo durante el arranque).
    owned: Vec<Box<Handler>>,
}

/// Manejadores por slot, sin bloqueo: `dispatch` es la ruta mas caliente del puente (cientos de miles de llamadas por
/// segundo desde decenas de hilos) y un RwLock global mas un `Arc` clonado en cada llamada se disputaban entre hilos.
/// Los manejadores son propiedad de `Table.owned` y nunca se liberan, asi que el puntero leido sigue valido aunque
/// otro hilo lo reemplace.
static HANDLERS: [AtomicPtr<Handler>; SLOTS as usize] = [const { AtomicPtr::new(std::ptr::null_mut()) }; SLOTS as usize];

fn publish(t: &mut Table, idx: usize, h: Handler) {
    // un manejador nuevo no hereda la equivalencia del anterior (quien registra la anota despues)
    TWIN[idx].store(0, Release);
    let b = Box::new(h);
    let p = &*b as *const Handler as *mut Handler;
    t.owned.push(b);
    HANDLERS[idx].store(p, Release);
}

static BASE: AtomicU64 = AtomicU64::new(0);

/// Funcion del host equivalente a cada slot para un llamador del HOST (0 = ninguna): la de un slot que solo reenvia a
/// esa funcion (`boundary::typed_slot`, `hostfn@X`). Si el guest entrega el slot como callback, el host recibe la
/// funcion real (sin trampolin ni paso por el guest). Tabla fija (512 KiB estaticos: solo ocupa lo que se toca).
static TWIN: [AtomicU64; SLOTS as usize] = [const { AtomicU64::new(0) }; SLOTS as usize];

/// Anota la funcion del host `f` como equivalente del slot `slot` (ver TWIN).
pub fn set_host_twin(slot: u64, f: u64) {
    let idx = (slot & TBI).wrapping_sub(BASE.load(Relaxed)) / SLOT;
    if idx < SLOTS - 1 {
        TWIN[idx as usize].store(f, Release);
    }
}

/// Funcion del host equivalente al slot `slot` (0 si no tiene o `slot` no es un slot). Sin bloqueos.
#[inline]
pub fn host_twin(slot: u64) -> u64 {
    let idx = (slot & TBI).wrapping_sub(BASE.load(Relaxed)) / SLOT;
    if idx < SLOTS {
        TWIN[idx as usize].load(Acquire)
    } else {
        0
    }
}

// Contadores por slot (diagnostico de bucles): llamadas y bytes pedidos por las funciones de reserva.
// La ruta caliente cuenta en la tabla del propio hilo (`HleStats`, sin atomicos compartidos); estos globales reciben lo
// que no cabe en esa tabla, lo de hilos sin estado guest y lo que vuelcan los hilos al terminar.
static COUNTS: [AtomicU32; SLOTS as usize] = [const { AtomicU32::new(0) }; SLOTS as usize];
static BYTES: [AtomicU64; SLOTS as usize] = [const { AtomicU64::new(0) }; SLOTS as usize];
const STAT_ENTS: usize = 256;
const STAT_PROBES: usize = 8;

#[derive(Default)]
struct StatEnt {
    /// slot + 1 (0 = libre)
    key: AtomicU32,
    n: AtomicU32,
    bytes: AtomicU64,
}

/// Contadores HLE de un hilo guest: tabla fija de 256 entradas (4 KiB) con sondeo lineal acotado a 8. Solo escribe el
/// hilo dueno (load + store Relaxed, sin RMW ni lineas compartidas); el muestreador la lee bajo `rt::ALL_THREADS`.
/// Si el slot no cabe en su cadena, cuenta en los globales COUNTS/BYTES (cota: nada crece).
pub struct HleStats {
    ents: [StatEnt; STAT_ENTS],
}

impl HleStats {
    pub fn new() -> HleStats {
        HleStats { ents: std::array::from_fn(|_| StatEnt::default()) }
    }

    #[inline]
    fn add(&self, idx: usize, bytes: u64) -> bool {
        let k = idx as u32 + 1;
        let h = idx.wrapping_mul(0x9E37) >> 3;
        for p in 0..STAT_PROBES {
            let e = &self.ents[(h + p) & (STAT_ENTS - 1)];
            let ek = e.key.load(Relaxed);
            if ek == k {
                e.n.store(e.n.load(Relaxed).wrapping_add(1), Relaxed);
                if bytes != 0 {
                    e.bytes.store(e.bytes.load(Relaxed).wrapping_add(bytes), Relaxed);
                }
                return true;
            }
            if ek == 0 {
                e.n.store(1, Relaxed);
                e.bytes.store(bytes, Relaxed);
                e.key.store(k, Release);
                return true;
            }
        }
        false
    }

    fn each(&self, mut f: impl FnMut(usize, u32, u64)) {
        for e in &self.ents {
            let k = e.key.load(Acquire);
            if k != 0 {
                f(k as usize - 1, e.n.load(Relaxed), e.bytes.load(Relaxed));
            }
        }
    }

    /// Vuelca los contadores a los globales (al terminar el hilo).
    pub fn fold_global(&self) {
        self.each(|i, n, b| {
            COUNTS[i].fetch_add(n, Relaxed);
            BYTES[i].fetch_add(b, Relaxed);
        });
    }
}

/// 0 = no reserva; 1 = tamano en x0; 2 = x0*x1; 3 = x1; 4 = x2
static KIND: [AtomicU8; SLOTS as usize] = [const { AtomicU8::new(0) }; SLOTS as usize];

fn alloc_kind(name: &str) -> u8 {
    match name {
        "malloc" | "valloc" | "pvalloc" | "_Znwm" | "_Znam" => 1,
        "calloc" => 2,
        "realloc" | "memalign" | "aligned_alloc" | "mmap" | "mmap64" => 3,
        "posix_memalign" => 4,
        _ => 0,
    }
}

/// Estado del muestreo anterior para `top_deltas`.
#[derive(Default)]
pub struct TopState {
    calls: Vec<u32>,
    bytes: Vec<u64>,
}

/// Funciones HLE mas llamadas y con mas memoria pedida desde la llamada anterior (nombre, incremento).
pub fn top_deltas<'a>(st: &mut TopState, n: usize, threads: impl Iterator<Item = &'a HleStats>) -> (Vec<(String, u32)>, Vec<(String, u64)>) {
    let len = crate::monitor::read(&TABLE).as_ref().map_or(0, |t| t.list.len());
    st.calls.resize(len, 0);
    st.bytes.resize(len, 0);
    let (mut c, mut b): (Vec<(usize, u32)>, Vec<(usize, u64)>) = (Vec::new(), Vec::new());
    let mut cur_c: Vec<u32> = (0..len).map(|i| COUNTS[i].load(Relaxed)).collect();
    let mut cur_b: Vec<u64> = (0..len).map(|i| BYTES[i].load(Relaxed)).collect();
    for t in threads {
        t.each(|i, n, by| {
            if i < len {
                cur_c[i] = cur_c[i].wrapping_add(n);
                cur_b[i] = cur_b[i].wrapping_add(by);
            }
        });
    }
    for i in 0..len {
        let (cv, bv) = (cur_c[i], cur_b[i]);
        let (dc, db) = (cv.wrapping_sub(st.calls[i]), bv.wrapping_sub(st.bytes[i]));
        st.calls[i] = cv;
        st.bytes[i] = bv;
        if dc > 0 {
            c.push((i, dc));
        }
        if db >= 1 << 20 {
            b.push((i, db));
        }
    }
    c.sort_unstable_by(|x, y| y.1.cmp(&x.1));
    b.sort_unstable_by(|x, y| y.1.cmp(&x.1));
    (
        c.into_iter().take(n).map(|(i, d)| (name_at_index(i as u32), d)).collect(),
        b.into_iter().take(n).map(|(i, d)| (name_at_index(i as u32), d)).collect(),
    )
}
static TABLE: RwLock<Option<Table>> = RwLock::new(None);

pub fn init() {
    let mut t = crate::monitor::write(&TABLE);
    if t.is_some() {
        return;
    }
    let p = crate::mem::Region::permanent((SLOT * SLOTS) as usize, PROT_NONE).expect("no se pudo reservar la region HLE");
    BASE.store(p, Relaxed);
    *t = Some(Table { names: HashMap::new(), list: Vec::new(), owned: Vec::new() });
}

#[inline]
pub fn is_hle(pc: u64) -> bool {
    pc.wrapping_sub(BASE.load(Relaxed)) < SLOT * SLOTS
}

/// Registra (o reemplaza) un manejador y devuelve su direccion guest.
pub fn register(name: &str, h: Handler) -> u64 {
    init();
    let mut g = crate::monitor::write(&TABLE);
    let t = g.as_mut().unwrap();
    if let Some(&a) = t.names.get(name) {
        let idx = ((a - BASE.load(Relaxed)) / SLOT) as usize;
        t.list[idx] = name.to_string();
        publish(t, idx, h);
        return a;
    }
    let mut idx = t.list.len() as u64;
    if idx >= SLOTS - 1 {
        // Region agotada: no se aborta ni se crece. El ultimo slot es comun y falla con diagnostico si se llama.
        if idx == SLOTS - 1 {
            eprintln!("[heddle] region HLE agotada ({} slots): las funciones nuevas quedan no disponibles", SLOTS);
            t.list.push("__heddle_slots_agotados".to_string());
            publish(
                t,
                SLOTS as usize - 1,
                Box::new(|c: &mut Cpu| {
                    crate::bridge::alog_fatal(&format!("llamada a una funcion sin slot HLE (region agotada)\n{}", crate::rt::dump_backtrace(c)));
                    unsafe { abort() }
                }),
            );
        }
        idx = SLOTS - 1;
        return BASE.load(Relaxed) + idx * SLOT;
    }
    let a = BASE.load(Relaxed) + idx * SLOT;
    KIND[idx as usize].store(alloc_kind(name), Relaxed);
    t.list.push(name.to_string());
    publish(t, idx as usize, h);
    t.names.insert(name.to_string(), a);
    drop(g); // el registro en logcat se hace sin el bloqueo de la tabla
    #[cfg(target_os = "android")]
    crate::bridge::alog(&format!("hle slot {} (+0x{:x}) = {}", idx, idx * SLOT, name));
    a
}

/// Direccion del slot comun que queda cuando la region esta agotada (None si aun hay sitio).
pub fn exhausted_slot() -> Option<u64> {
    let full = crate::monitor::read(&TABLE).as_ref().map_or(false, |t| t.list.len() as u64 >= SLOTS);
    if full {
        Some(BASE.load(Relaxed) + (SLOTS - 1) * SLOT)
    } else {
        None
    }
}

pub fn addr_of(name: &str) -> Option<u64> {
    crate::monitor::read(&TABLE).as_ref().and_then(|t| t.names.get(name).copied())
}

pub fn name_at_index(idx: u32) -> String {
    TABLE.read().ok().and_then(|g| g.as_ref().and_then(|t| t.list.get(idx as usize).cloned())).unwrap_or_default()
}

pub fn name_at(pc: u64) -> String {
    let idx = ((pc & TBI).wrapping_sub(BASE.load(Relaxed)) / SLOT) as usize;
    crate::monitor::read(&TABLE).as_ref().and_then(|t| t.list.get(idx).cloned()).unwrap_or_default()
}

thread_local! {
    static RING: std::cell::RefCell<([(u32, u64, u64, u64); 32], usize)> = const { std::cell::RefCell::new(([(0, 0, 0, 0); 32], 0)) };
}

/// Ultimas llamadas HLE de este hilo (mas reciente al final), para diagnosticar fallos del host.
pub fn recent_calls() -> String {
    RING.with(|r| {
        let (buf, n) = &*r.borrow();
        let mut s = String::new();
        let total = (*n).min(32);
        for k in 0..total {
            let (idx, a, b, d) = buf[(*n - total + k) % 32];
            let name = TABLE.read().ok().and_then(|g| g.as_ref().and_then(|t| t.list.get(idx as usize).cloned())).unwrap_or_default();
            s += &format!("  hle[-{}] {} x0={:#x} x1={:#x} x2={:#x}\n", total - k, name, a, b, d);
        }
        s
    })
}

pub fn dispatch(c: &mut Cpu) {
    let idx = ((c.pc & TBI) - BASE.load(Relaxed)) / SLOT;
    // try_: una senal que interrumpa esta anotacion y llame a otra HLE desde su manejador no debe entrar en panico
    let _ = RING.try_with(|r| {
        if let Ok(mut g) = r.try_borrow_mut() {
            let (buf, n) = &mut *g;
            buf[*n % 32] = (idx as u32, c.x[0], c.x[1], c.x[2]);
            *n += 1;
        }
    });
    let kind = KIND[idx as usize].load(Relaxed);
    let bytes = match kind {
        0 => 0,
        1 => c.x[0],
        2 => c.x[0].saturating_mul(c.x[1]),
        3 => c.x[1],
        _ => c.x[2],
    };
    let counted = match crate::rt::cur_opt() {
        Some(t) => {
            t.last_hle = idx as u32;
            t.hle.add(idx as usize, bytes)
        }
        None => false,
    };
    if !counted {
        COUNTS[idx as usize].fetch_add(1, Relaxed);
        if kind != 0 {
            BYTES[idx as usize].fetch_add(bytes, Relaxed);
        }
    }
    let hp = HANDLERS[idx as usize].load(Acquire);
    if hp.is_null() {
        crate::bridge::alog_fatal(&format!("llamada HLE a un slot sin manejador ({})", idx));
        unsafe { abort() }
    }
    // SAFETY: los manejadores publicados nunca se liberan (ver HANDLERS)
    let h: &Handler = unsafe { &*hp };
    match h(c) {
        Ret::Return => c.pc = c.x[30],
        Ret::Stay => {}
    }
}

/// Microbenchmark de la ruta de llamadas HLE (no se ejecuta en `cargo test` normal):
/// `cargo test --release --lib hle::bench -- --ignored --nocapture --test-threads=1`
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;

    fn run(threads: usize, calls: u64, slot: u64, guest: bool) -> f64 {
        let t0 = Instant::now();
        let hs: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::spawn(move || {
                    if guest {
                        crate::rt::cur();
                    }
                    let mut c = Cpu::new();
                    for _ in 0..calls {
                        c.pc = slot;
                        c.x[30] = 0x1000;
                        dispatch(&mut c);
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
        t0.elapsed().as_nanos() as f64 / (calls * threads as u64) as f64
    }

    #[test]
    #[ignore]
    fn bench_dispatch_hle() {
        init();
        let slot = register("bench_nop", Box::new(|c| {
            c.x[0] = c.x[0].wrapping_add(1);
            Ret::Return
        }));
        for threads in [1, 4, 16] {
            // sin estado guest: contadores globales atomicos (como antes); con estado guest: contadores por hilo
            let g = run(threads, 3_000_000, slot, false);
            let h = run(threads, 3_000_000, slot, true);
            println!("BENCH hilos={:2} globales {:.1} ns | por hilo {:.1} ns por llamada HLE", threads, g, h);
        }
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;

    #[test]
    fn hle_stats_cuenta_y_desborda_a_falso() {
        let s = HleStats::new();
        for _ in 0..5 {
            assert!(s.add(7, 10));
        }
        assert!(s.add(9, 0));
        let mut got = Vec::new();
        s.each(|i, n, b| got.push((i, n, b)));
        got.sort();
        assert_eq!(got, vec![(7, 5, 50), (9, 1, 0)]);
        // con la tabla llena de otras claves, un slot nuevo que no cabe devuelve false (cuenta en los globales)
        let t = HleStats::new();
        let mut falsos = 0;
        for i in 0..2000usize {
            if !t.add(i, 1) {
                falsos += 1;
            }
        }
        assert!(falsos > 0);
        let mut total = 0u64;
        t.each(|_, n, _| total += n as u64);
        assert_eq!(total as usize + falsos, 2000);
    }
}
