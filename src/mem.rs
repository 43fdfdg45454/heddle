//! Memoria propia del puente: regiones con dueno (RAII), cuentas, limites y vigilante. Leer CLAUDE.md.
//!
//! Reglas:
//!  - Toda reserva del puente con `mmap` pasa por `Region` (se libera en `Drop`) salvo las permanentes, que se
//!    declaran con `Region::permanent` y tienen tamano fijo (acotadas por construccion).
//!  - Nada crece sin cota: cada estructura global tiene un limite y al alcanzarlo degrada (niega, recicla), no aborta
//!    ni sigue creciendo.
//!  - El vigilante mide el proceso entero (RSS) y corta la app antes de agotar la maquina.

use crate::sys::*;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

pub const PAGE: usize = 4096;

/// Tamano de pagina que ve el guest (`AT_PAGESZ`, `sysconf`, granularidad de `mmap` y del cargador): 4 KiB por
/// defecto o 16 KiB con `HEDDLE_PAGE_SIZE=16384` (o la propiedad `debug.heddle.page_size`), como un dispositivo
/// arm64 con paginas de 16 KiB. El del host es siempre `PAGE`. Se decide una vez.
#[inline]
pub fn guest_page() -> u64 {
    let p = GUEST_PAGE.load(Relaxed);
    if p != 0 {
        return p;
    }
    init_guest_page()
}

static GUEST_PAGE: AtomicU64 = AtomicU64::new(0);

#[cold]
fn init_guest_page() -> u64 {
    let want = std::env::var("HEDDLE_PAGE_SIZE").ok().or_else(|| crate::boundary::prop("debug.heddle.page_size")).unwrap_or_default();
    let p = match want.trim() {
        "" | "4096" | "4k" | "4K" => 4096,
        "16384" | "16k" | "16K" => 16384,
        o => {
            crate::bridge::alog(&format!("debug.heddle.page_size: valor no valido '{}' (4096 o 16384): se usa 4096", o));
            4096
        }
    };
    GUEST_PAGE.store(p, Relaxed);
    p
}

/// Modo de compatibilidad de bionic para bibliotecas con `p_align` de 4 KiB en paginas de 16 KiB (Android 15+), para
/// la biblioteca `rp` (ruta real): como `ElfReader::Read`, la propiedad `bionic.linker.16kb.app_compat.enabled` o el
/// modo del proceso que fija zygote, aqui `pagecompat::process_mode` (manifiesto `android:pageSizeCompat` y alineacion
/// de las bibliotecas de la app, como Android 16). `HEDDLE_PAGE_COMPAT` (o `debug.heddle.page_compat`): `1` lo fuerza
/// como la propiedad de bionic; `0` es el ajuste del usuario que lo desactiva (`SETTINGS_OVERRIDE_DISABLED`).
pub fn page_compat(rp: &str) -> bool {
    let val = |v: &str| match v.trim() {
        "1" | "y" | "yes" | "on" | "true" => Some(true),
        "0" | "n" | "no" | "off" | "false" => Some(false),
        _ => None,
    };
    if crate::boundary::prop("bionic.linker.16kb.app_compat.enabled").and_then(|v| val(&v)) == Some(true) {
        return true;
    }
    let ours = std::env::var("HEDDLE_PAGE_COMPAT").ok().and_then(|v| val(&v)).or_else(|| crate::boundary::prop("debug.heddle.page_compat").and_then(|v| val(&v)));
    match ours {
        Some(true) => true,
        settings => crate::pagecompat::process_mode(settings.map(|_| false), rp),
    }
}

/// Bytes reservados actualmente por el puente, por clase.
pub static JIT_BYTES: AtomicU64 = AtomicU64::new(0);
/// Bytes de codigo traducido en uso (suma de todos los hilos) y su presupuesto global.
pub static JIT_USED: AtomicU64 = AtomicU64::new(0);
pub const JIT_BUDGET: u64 = 512 << 20;
/// Tope de entradas de las tablas globales indexadas por cadenas que aporta el guest (simbolos, cadenas internas).
pub const TABLE_CAP: usize = 1 << 16;
pub static STACK_BYTES: AtomicU64 = AtomicU64::new(0);
pub static OTHER_BYTES: AtomicU64 = AtomicU64::new(0);
static TEST_BYTES: AtomicU64 = AtomicU64::new(0);
pub static THREADS_LIVE: AtomicU64 = AtomicU64::new(0);
pub static THREADS_TOTAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Jit,
    Stack,
    Other,
    /// solo para pruebas (contador propio, sin interferencia de otros hilos)
    Test,
}

fn counter(k: Kind) -> &'static AtomicU64 {
    match k {
        Kind::Jit => &JIT_BYTES,
        Kind::Stack => &STACK_BYTES,
        Kind::Other => &OTHER_BYTES,
        Kind::Test => &TEST_BYTES,
    }
}

/// Mapeo anonimo con dueno: se devuelve al sistema al soltarlo.
pub struct Region {
    base: *mut u8,
    len: usize,
    kind: Kind,
}

unsafe impl Send for Region {}
unsafe impl Sync for Region {}

impl Region {
    /// Reserva `len` bytes (redondeado a pagina) con proteccion `prot`. NORESERVE: solo ocupa lo que se toca.
    pub fn new(len: usize, prot: i32, kind: Kind) -> Option<Region> {
        let len = (len + PAGE - 1) & !(PAGE - 1);
        let p = unsafe { mmap(std::ptr::null_mut(), len, prot, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
        if p == MAP_FAILED {
            return None;
        }
        counter(kind).fetch_add(len as u64, Relaxed);
        Some(Region { base: p as *mut u8, len, kind })
    }

    /// Region de lectura/escritura con una pagina de guarda inaccesible por debajo (pilas: un desbordamiento
    /// falla en la guarda en lugar de pisar en silencio la memoria vecina). `lo()` es el inicio utilizable.
    pub fn with_guard(len: usize, kind: Kind) -> Option<Region> {
        let len = (len + PAGE - 1) & !(PAGE - 1);
        let r = Region::new(len + PAGE, PROT_NONE, kind)?;
        if unsafe { mprotect(r.base.add(PAGE) as *mut c_void, len, PROT_READ | PROT_WRITE) } != 0 {
            return None;
        }
        Some(r)
    }

    /// Reserva permanente (vive todo el proceso; tamano fijo): no se libera nunca.
    pub fn permanent(len: usize, prot: i32) -> Option<u64> {
        Some(Region::new(len, prot, Kind::Other)?.into_permanent())
    }

    /// Convierte la region en permanente (no se libera nunca) y devuelve su base. Para reservas de tamano fijo que se
    /// publican con una comparacion atomica: quien pierde la carrera suelta la suya.
    pub fn into_permanent(self) -> u64 {
        let b = self.base as u64;
        std::mem::forget(self);
        b
    }

    /// Separa la region de su dueno: (base, longitud). Quien la recibe la devuelve con `reattach` (bloques del TLS
    /// dinamico: su cabecera guarda las dos cosas).
    pub fn detach(self) -> (u64, usize) {
        let len = self.len;
        (self.into_permanent(), len)
    }

    /// Recupera una region separada con `detach` (se libera al soltarla).
    ///
    /// # Safety
    /// `(base, len)` debe venir de un `detach` de una region de tipo `kind` y no haberse recuperado ya.
    pub unsafe fn reattach(base: u64, len: usize, kind: Kind) -> Region {
        Region { base: base as *mut u8, len, kind }
    }

    pub fn base(&self) -> u64 {
        self.base as u64
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn end(&self) -> u64 {
        self.base as u64 + self.len as u64
    }

    /// Devuelve al sistema las paginas tocadas de los primeros `used` bytes (el contenido pasa a ceros).
    pub fn discard(&self, used: usize) {
        let n = ((used + PAGE - 1) & !(PAGE - 1)).min(self.len);
        if n > 0 {
            unsafe { madvise(self.base as *mut c_void, n, 4 /*MADV_DONTNEED*/) };
        }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        unsafe { munmap(self.base as *mut c_void, self.len) };
        counter(self.kind).fetch_sub(self.len as u64, Relaxed);
    }
}

// ---------------------------------------------------------------------------------------------
// Regiones ejecutables que crea el guest (mmap/mprotect con PROT_EXEC: JIT propios del guest, trampolines)
// ---------------------------------------------------------------------------------------------
// El host nunca las mapea ejecutables (el puente traduce leyendolas); aqui se recuerda que el guest las pidio
// ejecutables, para distinguir "direccion que el guest puede ejecutar" de datos cualesquiera.

static GEXEC: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static GEXEC_MIN: AtomicU64 = AtomicU64::new(u64::MAX);
static GEXEC_MAX: AtomicU64 = AtomicU64::new(0);
/// Cota: un guest que fragmente mas alla de esto pierde precision (se fusionan), no memoria.
const GEXEC_CAP: usize = 4096;

fn gexec_bounds(v: &[(u64, u64)]) {
    GEXEC_MIN.store(v.iter().map(|r| r.0).min().unwrap_or(u64::MAX), Relaxed);
    GEXEC_MAX.store(v.iter().map(|r| r.1).max().unwrap_or(0), Relaxed);
}

fn gexec_remove(v: &mut Vec<(u64, u64)>, lo: u64, hi: u64) {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(v.len() + 1);
    for &(a, b) in v.iter() {
        if b <= lo || a >= hi {
            out.push((a, b));
            continue;
        }
        if a < lo {
            out.push((a, lo));
        }
        if b > hi {
            out.push((hi, b));
        }
    }
    *v = out;
}

/// El guest cambio la proteccion de [addr, addr+len): `exec` indica si pidio PROT_EXEC.
/// La lista se mantiene ordenada y con los intervalos contiguos fusionados. Al llegar al tope NO se amplia nada
/// (un intervalo envolvente podria abarcar bibliotecas del host): la region nueva simplemente no cuenta como
/// ejecutable por el guest, que es el lado seguro (el host no la ejecutara y el guest falla con diagnostico).
pub fn guest_prot(addr: u64, len: u64, exec: bool) {
    crate::monitor::prot_changed();
    if len == 0 || (!exec && addr >= GEXEC_MAX.load(Relaxed)) || (!exec && addr.saturating_add(len) <= GEXEC_MIN.load(Relaxed)) {
        return;
    }
    let (lo, hi) = (addr & !(PAGE as u64 - 1), (addr.saturating_add(len) + PAGE as u64 - 1) & !(PAGE as u64 - 1));
    let mut g = crate::monitor::lock(&GEXEC);
    gexec_remove(&mut g, lo, hi);
    if exec {
        g.push((lo, hi));
    }
    // ordenar y fusionar vecinos
    g.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(g.len());
    for &(a, b) in g.iter() {
        match out.last_mut() {
            Some(last) if last.1 >= a => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    if out.len() > GEXEC_CAP {
        // tope: se descartan los intervalos de mas (los mas altos); nunca se crean intervalos que el guest no pidio
        static WARN: std::sync::Once = std::sync::Once::new();
        WARN.call_once(|| eprintln!("[heddle] demasiadas regiones ejecutables del guest ({}): las nuevas no se registran", GEXEC_CAP));
        out.truncate(GEXEC_CAP);
    }
    *g = out;
    gexec_bounds(&g);
}

/// El guest desmapeo [addr, addr+len).
pub fn guest_unmap(addr: u64, len: u64) {
    guest_prot(addr, len, false);
}

/// `a` esta en una region que el guest pidio ejecutable?
pub fn is_guest_exec(a: u64) -> bool {
    if a < GEXEC_MIN.load(Relaxed) || a >= GEXEC_MAX.load(Relaxed) {
        return false;
    }
    crate::monitor::lock(&GEXEC).iter().any(|&(lo, hi)| a >= lo && a < hi)
}

/// Como `is_guest_exec`, sin esperar (manejadores de senal): None si la lista esta tomada.
pub fn is_guest_exec_try(a: u64) -> Option<bool> {
    if a < GEXEC_MIN.load(Relaxed) || a >= GEXEC_MAX.load(Relaxed) {
        return Some(false);
    }
    Some(GEXEC.try_lock().ok()?.iter().any(|&(lo, hi)| a >= lo && a < hi))
}

// ---------------------------------------------------------------------------------------------
// Medicion del proceso y vigilante
// ---------------------------------------------------------------------------------------------

/// (tamano virtual, residente) del proceso en KiB.
pub fn process_kb() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let mut it = s.split_whitespace().map(|x| x.parse::<u64>().unwrap_or(0) * 4);
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

fn mem_total_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    s.lines().find(|l| l.starts_with("MemTotal:")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// Limite de memoria residente del proceso (KiB): propiedad `debug.heddle.rss_limit_mb`, o el 60 % de la RAM
/// del sistema con tope de 3 GiB.
pub fn rss_limit_kb() -> u64 {
    static L: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *L.get_or_init(|| {
        let custom = std::env::var("HEDDLE_RSS_LIMIT_MB").ok().or_else(|| crate::boundary::prop("debug.heddle.rss_limit_mb"));
        if let Some(mb) = custom.and_then(|v| v.parse::<u64>().ok()) {
            return mb * 1024;
        }
        let total = mem_total_kb();
        let auto = if total == 0 { 3 << 20 } else { total * 6 / 10 };
        auto.min(3 << 20)
    })
}

/// Decision del vigilante (separada para poder probarla): supera el limite?
pub fn over_limit(rss_kb: u64, limit_kb: u64) -> bool {
    limit_kb != 0 && rss_kb > limit_kb
}

/// Linea de estado de memoria del puente y del proceso.
pub fn status_line() -> String {
    let (vm, rss) = process_kb();
    format!(
        "memoria: rss={} MiB (limite {} MiB) virtual={} MiB | puente: jit en uso={} MiB (reservado {} MiB) pilas={} MiB otros={} MiB | hilos guest vivos={} creados={}",
        rss >> 10,
        rss_limit_kb() >> 10,
        vm >> 10,
        JIT_USED.load(Relaxed) >> 20,
        JIT_BYTES.load(Relaxed) >> 20,
        STACK_BYTES.load(Relaxed) >> 20,
        OTHER_BYTES.load(Relaxed) >> 20,
        THREADS_LIVE.load(Relaxed),
        THREADS_TOTAL.load(Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_se_libera_y_descuenta() {
        assert_eq!(TEST_BYTES.load(Relaxed), 0);
        {
            let r = Region::new(3 * PAGE + 1, PROT_READ | PROT_WRITE, Kind::Test).unwrap();
            assert_eq!(r.len(), 4 * PAGE);
            unsafe { *(r.base() as *mut u8) = 1 };
            assert_eq!(TEST_BYTES.load(Relaxed), 4 * PAGE as u64);
            r.discard(PAGE);
            assert_eq!(unsafe { *(r.base() as *const u8) }, 0, "discard devuelve las paginas");
        }
        assert_eq!(TEST_BYTES.load(Relaxed), 0, "al soltar la region se descuenta y se desmapea");
    }

    #[test]
    fn guarda_de_pila_inaccesible() {
        let r = Region::with_guard(8 * PAGE, Kind::Stack).unwrap();
        let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
        let guard = format!("{:x}-{:x} ---p", r.base(), r.base() + PAGE as u64);
        assert!(maps.lines().any(|l| l.starts_with(&guard)), "falta la pagina de guarda");
        unsafe { *((r.base() + PAGE as u64) as *mut u8) = 1 };
    }

    /// En un proceso hijo: llena la tabla global `GEXEC` hasta su tope, y en paralelo las demas pruebas que ejecutan
    /// codigo guest no podrian registrar sus regiones (SIGSEGV/SIGBUS al azar en `cargo test` con varios hilos).
    #[test]
    fn regiones_ejecutables_del_guest() {
        if std::env::var_os("HEDDLE_GEXEC_HIJO").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["mem::tests::regiones_ejecutables_del_guest", "--exact", "--test-threads=1"])
                .env("HEDDLE_GEXEC_HIJO", "1")
                .output()
                .unwrap();
            let so = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && so.contains("1 passed"), "{}\n{}", so, String::from_utf8_lossy(&out.stderr));
            return;
        }
        let base = 0x7100_0000_0000u64;
        guest_prot(base, 0x4000, true);
        assert!(is_guest_exec(base + 0x1000));
        assert!(!is_guest_exec(base + 0x4000));
        guest_prot(base + 0x1000, 0x1000, false); // mprotect sin exec en el medio
        assert!(!is_guest_exec(base + 0x1800));
        assert!(is_guest_exec(base) && is_guest_exec(base + 0x2000));
        guest_unmap(base, 0x4000);
        assert!(!is_guest_exec(base) && !is_guest_exec(base + 0x2000));
        // paginas contiguas se fusionan (un JIT guest que marca pagina a pagina no agota la tabla)
        for i in 0..10_000u64 {
            guest_prot(base + i * 0x1000, 0x1000, true);
        }
        assert!(GEXEC.lock().unwrap().iter().filter(|r| r.0 >= base && r.1 <= base + 10_000 * 0x1000).count() == 1);
        // paginas alternas: al llegar al tope no se inventa un intervalo envolvente
        guest_unmap(base, 10_000 * 0x1000);
        let far = 0x7200_0000_0000u64;
        for i in 0..(GEXEC_CAP as u64 + 500) {
            guest_prot(far + i * 0x2000, 0x1000, true);
        }
        assert!(GEXEC.lock().unwrap().len() <= GEXEC_CAP);
        assert!(!is_guest_exec(far + 0x1000), "un hueco nunca pedido no puede pasar a ser ejecutable");
        guest_unmap(far, (GEXEC_CAP as u64 + 500) * 0x2000);
    }

    #[test]
    fn vigilante_decide_por_limite() {
        assert!(!over_limit(100, 200));
        assert!(over_limit(201, 200));
        assert!(!over_limit(1 << 40, 0), "limite 0 = desactivado");
    }
}
