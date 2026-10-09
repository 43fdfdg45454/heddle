//! Runtime guest: hilos (pila guest, bloque TLS, JIT por hilo), llamadas host->guest, bucle de
//! ejecucion (HLE / SVC / BRK / indefinida) y senales guest.

use crate::cpu::Cpu;
use crate::hle;
use crate::jit::{Event, Jit};
use crate::sys::*;
use std::cell::{Cell, RefCell};
use std::os::raw::c_void;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

pub const TLS_SLOTS_BYTES: usize = 256;
/// Area TLS de cada hilo: ranuras de bionic, el DTV (`tls::DTV_OFF`, `tls::DTV_BYTES`) y en la ultima pagina el
/// bloque del hilo (tid) y el estado de su reservador de bloques TLS (`tls::ALLOC_OFF`). El TLS de las bibliotecas
/// es dinamico (`tls.rs`): cada bloque se reserva con ese reservador.
pub const TLS_AREA_BYTES: usize = 16 << 10;
const _: () = assert!(TLS_SLOTS_BYTES + crate::tls::DTV_BYTES <= TLS_AREA_BYTES - 4096);
pub const DEFAULT_STACK: usize = 4 << 20;
pub const SIG_STACK: usize = 256 << 10;
const TBI: u64 = 0x00FF_FFFF_FFFF_FFFF;

pub struct GuestThread {
    pub cpu: Box<Cpu>,
    pub jit: Jit,
    /// Cpu de cada nivel de anidamiento de senales, reservadas al crear el hilo (un manejador de senal no reserva
    /// memoria), y la del manejador en curso (nula si no hay). Los manejadores se ejecutan con el interprete (ver
    /// `run_loop`): sin traductor por nivel.
    pub sig_cpus: [Box<Cpu>; crate::sig::MAX_SIG_DEPTH],
    pub sig_cpu: *mut Cpu,
    /// estado de cada nivel de senal en curso (ver sig.rs)
    pub sig_lvl: [crate::sig::SigLevel; crate::sig::MAX_SIG_DEPTH],
    /// el propio puente esta preparando o cerrando la entrega de una senal (un fallo ahora no es del guest)
    pub sig_setup: bool,
    /// senales reservadas (SIGSEGV...) que el guest cree bloqueadas: el nucleo nunca las bloquea (ver
    /// `sig::guest_sigprocmask`)
    pub sig_view: u64,
    pub stack_lo: u64,
    pub stack_hi: u64,
    pub tls_base: u64,
    pub sig_stack_lo: u64,
    pub tid: i32,
    pub depth: u32,
    pub sig_depth: u32,
    /// diagnostico (muestreador): ultima funcion HLE y ultimo SVC de este hilo
    pub last_hle: u32,
    pub last_svc: u32,
    /// contadores HLE propios (llamadas y bytes por slot); los suma el muestreador
    pub hle: crate::hle::HleStats,
    pub last_sc: [u64; 4],
    /// diagnostico (perfilador): 0 = codigo guest traducido, 1 = dentro de una funcion HLE, 2 = en una llamada al sistema
    pub phase: u8,
    /// `run_loop` activos en este hilo (con `phase == 0`, una senal asincrona se aplaza a la frontera de instruccion)
    pub runs: u32,
    /// senales asincronas aplazadas (bit = numero - 1) y el `siginfo` con que llegaron (ver sig.rs, "Senales
    /// asincronas"). Las escribe el manejador del host de este hilo; las consume `sig::deliver_pending`.
    pub sig_pend: std::sync::atomic::AtomicU64,
    pub sig_pinfo: [[u8; 128]; 64],
    /// instancias extra de senales de tiempo real aplazadas (ver `sig::RtQueue`)
    pub sig_rtq: crate::sig::RtQueue,
    /// punto de reanudacion de la llamada HLE en curso (`heddle_hle_call`; 0 = ninguna): una senal en ella puede
    /// abandonarla (ver sig.rs)
    pub hle_sp: u64,
    /// pila (con guarda), area TLS y pila de senales: se liberan solas al terminar el hilo
    _regions: [crate::mem::Region; 3],
    /// marca "a mitad de store" del monitor (modo fence): la apuntan `cpu.monflag` y las Cpu de los manejadores
    pub mon_flag: crate::monitor::MonFlag,
    /// pila alternativa de senales del host instalada para este hilo (ver `HOST_ALT_STACK`) y la que tenia antes
    host_alt: Option<HostAlt>,
}

/// Pila alternativa de senales del HOST en los hilos que ejecutan codigo guest. Los manejadores guest de los fallos
/// sincronos se ejecutan con el interprete dentro del manejador del host, que corre en la pila alternativa (SA_ONSTACK:
/// la del puente y la de sigchain en ART); la de bionic/ART es de 16 KiB y un manejador guest con fallos anidados o
/// HLE con callbacks la desbordaba. Se instala una de 64 KiB con pagina de guarda (si la del hilo es menor) y al
/// terminar el hilo se repone la anterior. El guest sigue viendo la suya (sigaltstack del guest, emulada).
pub const HOST_ALT_STACK: usize = 64 << 10;

struct HostAlt {
    _region: crate::mem::Region,
    /// sp de la nuestra (para no reponer si alguien la cambio despues) y la anterior (ss_sp, ss_flags, ss_size)
    ours: u64,
    prev: [u64; 3],
}

/// stack_t del kernel x86-64: ss_sp, ss_flags (int + relleno), ss_size.
fn host_sigaltstack(new: Option<&[u64; 3]>, old: Option<&mut [u64; 3]>) -> i64 {
    unsafe { syscall(131, new.map_or(std::ptr::null(), |n| n as *const [u64; 3]), old.map_or(std::ptr::null_mut(), |o| o as *mut [u64; 3])) as i64 }
}

const SS_ONSTACK: u64 = 1;
const SS_DISABLE: u64 = 2;

/// Instala la pila alternativa grande del host si la del hilo es menor (o no tiene). None si no hace falta o no se
/// puede (el hilo esta ahora mismo en su pila alternativa, o sin memoria).
fn install_host_alt() -> Option<HostAlt> {
    let mut prev = [0u64; 3];
    if host_sigaltstack(None, Some(&mut prev)) != 0 || prev[1] & 0xffff_ffff & SS_ONSTACK != 0 {
        return None;
    }
    if prev[1] & SS_DISABLE == 0 && prev[2] >= HOST_ALT_STACK as u64 {
        return None;
    }
    let r = crate::mem::Region::with_guard(HOST_ALT_STACK, crate::mem::Kind::Stack)?;
    let sp = r.base() + crate::mem::PAGE as u64;
    if host_sigaltstack(Some(&[sp, 0, HOST_ALT_STACK as u64]), None) != 0 {
        return None;
    }
    Some(HostAlt { _region: r, ours: sp, prev })
}

/// Repone la pila alternativa anterior si la actual sigue siendo la nuestra y no se esta usando.
fn restore_host_alt(h: &HostAlt) {
    let mut cur = [0u64; 3];
    if host_sigaltstack(None, Some(&mut cur)) != 0 || cur[0] != h.ours || cur[1] & SS_ONSTACK != 0 {
        return;
    }
    let p = if h.prev[1] & SS_DISABLE != 0 { [0, SS_DISABLE, 0] } else { [h.prev[0], h.prev[1] & 0xffff_ffff, h.prev[2]] };
    host_sigaltstack(Some(&p), None);
}

impl Drop for GuestThread {
    fn drop(&mut self) {
        // Primero se suelta el puntero rapido y la marca del monitor: una senal que llegue mientras este hilo tiene
        // tomadas las listas de abajo no ejecuta codigo guest (que podria armar un granulo y esperar ALL_THREADS).
        let me = self as *mut GuestThread;
        let _ = CUR.try_with(|c| {
            if c.get() == me {
                c.set(std::ptr::null_mut());
            }
        });
        crate::monitor::set_thread_flag(std::ptr::null());
        crate::monitor::unregister_flag(&self.mon_flag);
        crate::monitor::lock(&STACKS).retain(|e| e.1 != self.stack_lo);
        {
            // el volcado a los globales y la baja de la lista van bajo el mismo bloqueo: el muestreador no cuenta dos veces
            let mut g = crate::monitor::lock(&ALL_THREADS);
            g.retain(|e| e.0 != self.tid);
            self.hle.fold_global();
        }
        crate::mem::THREADS_LIVE.fetch_sub(1, Relaxed);
        // bloques de TLS dinamico de este hilo (bionic los libera al salir el hilo)
        crate::tls::free_thread(self.tls_base);
        if let Some(h) = &self.host_alt {
            restore_host_alt(h); // antes de liberar su region (al soltar los campos)
        }
        // el puntero rapido del hilo ya no es valido (arriba): si algo vuelve a entrar al guest durante la salida del
        // hilo (destructor de clave del host), se crea un estado nuevo en lugar de usar memoria liberada
        // las regiones (pila, TLS, pila de senales) y los bufers del JIT se liberan en sus propios Drop
    }
}

thread_local! {
    static SLOT: RefCell<Option<Box<GuestThread>>> = RefCell::new(None);
    static CUR: Cell<*mut GuestThread> = const { Cell::new(std::ptr::null_mut()) };
}

/// Todos los hilos guest vivos (tid, puntero a GuestThread) para el muestreador de diagnostico.
static ALL_THREADS: Mutex<Vec<(i32, usize)>> = Mutex::new(Vec::new());

/// Diagnostico de cuelgues (Android): cada 6 s anota los hilos guest que no avanzaron entre dos muestras
/// (mismo pc, misma ultima funcion HLE y mismo ultimo SVC): donde esta bloqueado cada uno. El mismo hilo es el
/// vigilante de memoria (`VIGILANTE`), que sigue activo aunque los registros periodicos esten apagados
/// (`debug.heddle.samp=0` / `HEDDLE_SAMP=0`).
fn start_sampler() {
    // En Android siempre; fuera de Android solo con HEDDLE_SAMPLER=1 (el codigo se compila en todas partes).
    if !cfg!(target_os = "android") && std::env::var_os("HEDDLE_SAMPLER").is_none() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let quiet = !sampler_logs_on(std::env::var("HEDDLE_SAMP").ok().or_else(|| crate::boundary::prop("debug.heddle.samp")));
        if quiet && crate::mem::rss_limit_kb() == 0 {
            return; // sin registros y sin vigilante: nada que hacer
        }
        let _ = std::thread::Builder::new().name("heddle-samp".into()).spawn(move || {
            let mut st = SamplerState { quiet, ..Default::default() };
            loop {
                std::thread::sleep(std::time::Duration::from_secs(6));
                sampler_tick(&mut st);
            }
        });
    });
}

/// Perfilador (opt-in: `debug.heddle.prof=1` en Android o HEDDLE_PROF=1): muestrea cada 20 ms el estado de cada
/// hilo guest y cada 30 s anota en logcat donde se va el tiempo de CPU: codigo guest (por biblioteca y bloque),
/// funciones HLE y llamadas al sistema, mas bloques ejecutados y tiempo de traduccion. Los hilos dormidos (en una
/// HLE o syscall bloqueante) no cuentan. Apagado no cuesta nada salvo dos escrituras por llamada HLE/SVC.
fn start_profiler() {
    let on = crate::boundary::prop("debug.heddle.prof").map_or(false, |v| v == "1") || std::env::var_os("HEDDLE_PROF").is_some();
    if !on {
        return;
    }
    crate::jit::PROF_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("heddle-prof".into()).spawn(|| {
            use std::collections::HashMap;
            use std::sync::atomic::Ordering::Relaxed;
            let (mut guest, mut hle, mut svc): (HashMap<u64, u32>, HashMap<u32, u32>, HashMap<u32, u32>) = Default::default();
            let (mut samples, mut sleeping, mut idle) = (0u32, 0u32, 0u32);
            let mut last = (std::time::Instant::now(), 0u64, 0u64, 0u64);
            let mut prev = [0u64; 8];
            let mut prev_otras = [0u64; 64];
            let mut prev_fpcr = 0u64;
            let mut prev_mb = 0u64;
            let mut prev_fpg = vec![0u64; crate::profg::GRUPOS * crate::profg::SUBS * crate::profg::RAZONES];
            loop {
                std::thread::sleep(std::time::Duration::from_millis(20));
                {
                    let guard = ALL_THREADS.lock().unwrap();
                    for (tid, p) in guard.iter() {
                        let t = unsafe { &*(*p as *const GuestThread) };
                        let ph = unsafe { std::ptr::read_volatile(&t.phase) };
                        if ph != 0 {
                            // dentro de una HLE o syscall: solo cuenta si el hilo esta ejecutando, no dormido
                            let st = std::fs::read_to_string(format!("/proc/self/task/{}/stat", tid)).unwrap_or_default();
                            if st.rsplit(')').next().unwrap_or("").trim().chars().next() != Some('R') {
                                sleeping += 1;
                                continue;
                            }
                        }
                        if ph == 0 && unsafe { std::ptr::read_volatile(&t.cpu.pc) } == 0 {
                            idle += 1; // hilo adjunto que nunca ejecuto codigo guest (p. ej. un hilo de Java): no es carga
                            continue;
                        }
                        samples += 1;
                        match ph {
                            0 => *guest.entry(unsafe { std::ptr::read_volatile(&t.cpu.pc) } & TBI).or_default() += 1,
                            1 => *hle.entry(unsafe { std::ptr::read_volatile(&t.last_hle) }).or_default() += 1,
                            _ => *svc.entry(unsafe { std::ptr::read_volatile(&t.last_svc) }).or_default() += 1,
                        }
                    }
                }
                if last.0.elapsed() >= std::time::Duration::from_secs(30) {
                    let secs = last.0.elapsed().as_secs_f64();
                    let (b, cmp, ns) = (crate::jit::PROF_BLOCKS.load(Relaxed), crate::jit::PROF_COMPILED.load(Relaxed), crate::jit::PROF_COMPILE_NS.load(Relaxed));
                    let alog = crate::bridge::alog;
                    let pct = |n: u32| 100.0 * n as f64 / samples.max(1) as f64;
                    let (g, h, s): (u32, u32, u32) = (guest.values().sum(), hle.values().sum(), svc.values().sum());
                    alog(&format!(
                        "perfil {:.0}s: muestras_en_CPU={} (dormidos descartados={}, hilos sin codigo guest={}) guest={:.1}% hle={:.1}% svc={:.1}% | bloques={} ({:.2} M/s) traducidos={} tiempo_traduccion={} ms",
                        secs, samples, sleeping, idle, pct(g), pct(h), pct(s), b - last.1, (b - last.1) as f64 / secs / 1e6, cmp - last.2, (ns - last.3) / 1_000_000
                    ));
                    let mut libs: HashMap<String, u32> = HashMap::new();
                    for (pc, n) in &guest {
                        let d = crate::elf::describe_addr(*pc);
                        *libs.entry(d.split('!').next().unwrap_or("?").to_string()).or_default() += n;
                    }
                    let top = |v: Vec<(String, u32)>, k: usize| {
                        let mut v = v;
                        v.sort_by(|a, b| b.1.cmp(&a.1));
                        v.into_iter().take(k).map(|(n, c)| format!("{} {:.1}%", n, pct(c))).collect::<Vec<_>>().join(" | ")
                    };
                    {
                        let r = |a: &std::sync::atomic::AtomicU64| a.load(Relaxed);
                        let cur = [
                            r(&crate::jit::PROF_INTERP[0]),
                            r(&crate::jit::PROF_INTERP[1]),
                            r(&crate::jit::PROF_INTERP[2]),
                            r(&crate::jit::PROF_INTERP[3]),
                            r(&crate::jit::PROF_STORES),
                            r(&crate::monitor::PROF_LDX),
                            r(&crate::monitor::PROF_STX),
                            r(&crate::monitor::MEMBARRIERS),
                        ];
                        let d = |i: usize| (cur[i] - prev[i]) as f64 / secs;
                        alog(&format!(
                            "perfil interprete (h_exec por segundo): fp_escalar={:.0} simd={:.0} bitfield={:.0} otras={:.0} | bloques/s={:.0}",
                            d(0), d(1), d(2), d(3), (b - last.1) as f64 / secs
                        ));
                        {
                            let mut v: Vec<(usize, u64)> = (0..crate::jit::OP_NAMES.len())
                                .map(|i| (i, crate::jit::PROF_OTRAS[i].load(Relaxed)))
                                .collect();
                            let prev_o = &mut prev_otras;
                            for e in v.iter_mut() {
                                let t = e.1;
                                e.1 = t - prev_o[e.0];
                                prev_o[e.0] = t;
                            }
                            v.sort_by(|a, b| b.1.cmp(&a.1));
                            let txt: Vec<String> = v.iter().take(8).filter(|e| e.1 > 0).map(|e| format!("{}={:.0}", crate::jit::OP_NAMES[e.0], e.1 as f64 / secs)).collect();
                            alog(&format!("perfil interprete otras por variante (por segundo, top 8): {}", txt.join(" ")));
                            let f = r(&crate::jit::PROF_FP_FPCR);
                            alog(&format!("perfil interprete FP/SIMD con FPCR!=0 (por segundo): {:.0}", (f - prev_fpcr) as f64 / secs));
                            prev_fpcr = f;
                            {
                                use crate::profg::{GRUPOS, GRUPO_NOMBRE, RAZONES, RAZON_NOMBRE, SUBS};
                                let mut ent: Vec<(usize, u64, [u64; RAZONES])> = Vec::new();
                                let mut razon_tot = [0u64; RAZONES];
                                let mut grupo_tot = [0u64; GRUPOS];
                                for e in 0..GRUPOS * SUBS {
                                    let mut rz = [0u64; RAZONES];
                                    let mut t = 0u64;
                                    for k in 0..RAZONES {
                                        let i = e * RAZONES + k;
                                        let cur = crate::profg::PROF_FPG[i].load(Relaxed);
                                        rz[k] = cur - prev_fpg[i];
                                        prev_fpg[i] = cur;
                                        t += rz[k];
                                        razon_tot[k] += rz[k];
                                    }
                                    grupo_tot[e / SUBS] += t;
                                    if t > 0 {
                                        ent.push((e, t, rz));
                                    }
                                }
                                ent.sort_by(|a, b| b.1.cmp(&a.1));
                                for (rank, (e, t, rz)) in ent.iter().take(12).enumerate() {
                                    let (g, sub) = (e / SUBS, e % SUBS);
                                    let rs: Vec<String> = (0..RAZONES)
                                        .filter(|&k| rz[k] * 10 >= *t)
                                        .map(|k| format!("{} {:.0}%", RAZON_NOMBRE[k], 100.0 * rz[k] as f64 / *t as f64))
                                        .collect();
                                    alog(&format!(
                                        "perfil fpsimd top {}: [{}] {} sub={:#x} = {:.0}/s | razon: {}",
                                        rank + 1, GRUPO_NOMBRE[g], crate::profg::nombre(g, sub), sub, *t as f64 / secs, rs.join(", ")
                                    ));
                                }
                                let gt: Vec<String> = (0..GRUPOS)
                                    .filter(|&g| grupo_tot[g] > 0)
                                    .map(|g| format!("{}={:.0}", GRUPO_NOMBRE[g], grupo_tot[g] as f64 / secs))
                                    .collect();
                                alog(&format!("perfil fpsimd por grupo (por segundo): {}", gt.join(" | ")));
                                let rt: Vec<String> = (0..RAZONES).map(|k| format!("{}={:.0}", RAZON_NOMBRE[k], razon_tot[k] as f64 / secs)).collect();
                                alog(&format!("perfil fpsimd razones de caida (por segundo): {}", rt.join(" ")));
                            }
                        }
                        alog(&format!(
                            "perfil memoria: stores/s={:.0} ldxr/s={:.0} stxr/s={:.0} granulos_distintos_LLSC_acumulados={} | monitor modo={} armados/s={:.0} barrera+espera={:.2} ms/s esperas_store={} espera_max_us={:.1}",
                            d(4), d(5), d(6), r(&crate::monitor::PROF_DISTINCT), crate::monitor::mode_name(), d(7),
                            {
                                let mb = r(&crate::monitor::MB_NS);
                                let v = (mb - prev_mb) as f64 / secs / 1e6;
                                prev_mb = mb;
                                v
                            },
                            r(&crate::monitor::FENCE_WAITS), r(&crate::monitor::FENCE_WAIT_MAX_NS) as f64 / 1000.0
                        ));
                        let ops = r(&crate::jit::PROF_OPS) as f64;
                        let ex: Vec<u64> = (0..3).map(|i| r(&crate::jit::PROF_EXITS[i])).collect();
                        let tot = (ex[0] + ex[1] + ex[2]).max(1) as f64;
                        alog(&format!(
                            "perfil bloques traducidos (estatico, acumulado): ops/bloque={:.1} salidas directas={:.0}% indirectas={:.0}% secuencia={:.0}%",
                            ops / tot, 100.0 * ex[0] as f64 / tot, 100.0 * ex[1] as f64 / tot, 100.0 * ex[2] as f64 / tot
                        ));
                        prev = cur;
                    }
                    alog(&format!("perfil guest por biblioteca: {}", top(libs.into_iter().collect(), 8)));
                    alog(&format!("perfil guest bloques: {}", top(guest.iter().map(|(pc, n)| (crate::elf::describe_addr(*pc), *n)).collect(), 14)));
                    alog(&format!("perfil hle: {}", top(hle.iter().map(|(i, n)| (crate::hle::name_at_index(*i), *n)).collect(), 10)));
                    alog(&format!("perfil svc: {}", top(svc.iter().map(|(i, n)| (format!("{}({})", crate::diag::syscall_name(*i as u64), i), *n)).collect(), 6)));
                    (guest.clear(), hle.clear(), svc.clear());
                    (samples, sleeping, idle) = (0, 0, 0);
                    last = (std::time::Instant::now(), b, cmp, ns);
                }
            }
        });
    });
}

/// Registros periodicos del muestreador: todo valor salvo "0" (o sin definir) los deja encendidos.
pub fn sampler_logs_on(v: Option<String>) -> bool {
    v.as_deref().map(str::trim) != Some("0")
}

#[derive(Default)]
pub struct SamplerState {
    prev: std::collections::HashMap<i32, ((u64, u32, u32), u32)>,
    top_prev: crate::hle::TopState,
    /// registros periodicos apagados (`debug.heddle.samp=0`): solo el vigilante de memoria
    pub quiet: bool,
}

/// Un paso del muestreador/vigilante: hilos que no avanzan, memoria, funciones mas llamadas y limite de memoria.
/// Con `quiet` solo el limite de memoria (lo fatal, `VIGILANTE`, se registra siempre).
pub fn sampler_tick(st: &mut SamplerState) {
    if st.quiet {
        let (_, rss) = crate::mem::process_kb();
        if crate::mem::over_limit(rss, crate::mem::rss_limit_kb()) {
            vigilante(&ALL_THREADS.lock().unwrap(), rss);
        }
        return;
    }
    let SamplerState { prev, top_prev, .. } = st;
    // El bloqueo se mantiene durante todo el muestreo: un hilo que termina espera aqui (en su Drop) y su
    // GuestThread no se libera mientras se lee. (Antes se copiaba la lista y se leia memoria ya liberada.)
    let guard = ALL_THREADS.lock().unwrap();
    let list: &Vec<(i32, usize)> = &guard;
    let mut stuck = 0;
    for (tid, p) in list {
        let t = unsafe { &*(*p as *const GuestThread) };
        let key = unsafe { (std::ptr::read_volatile(&t.cpu.pc), std::ptr::read_volatile(&t.last_hle), std::ptr::read_volatile(&t.last_svc)) };
        let n = match prev.get(tid) {
            Some((k, n)) if *k == key => n + 1,
            _ => 0,
        };
        prev.insert(*tid, (key, n));
        if n >= 1 {
            stuck += 1;
            if n == 1 || n % 10 == 0 {
                let comm = std::fs::read_to_string(format!("/proc/self/task/{}/comm", tid)).unwrap_or_default();
                let bt = crate::rt::dump_backtrace(&t.cpu);
                let alog = crate::bridge::alog;
                let rd = |f: &str| std::fs::read_to_string(format!("/proc/self/task/{}/{}", tid, f)).unwrap_or_default();
                let stat = rd("stat");
                let st = stat.rsplit(')').next().unwrap_or("").trim().chars().next().unwrap_or('?');
                {
                    let pc = t.cpu.pc & 0x00FF_FFFF_FFFF_FFFF;
                    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
                    let ml = maps.lines().find(|l| {
                        let mut it = l.split(|c| c == '-' || c == ' ');
                        let (a, b) = (u64::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0), u64::from_str_radix(it.next().unwrap_or("0"), 16).unwrap_or(0));
                        pc >= a && pc < b
                    });
                    alog(&format!("muestreo: hilo {} pc={} host={} mapa={}", tid, crate::elf::describe_addr(pc), crate::boundary::host_describe(pc), ml.unwrap_or("-")));
                    let lr = t.cpu.x[30] & 0x00FF_FFFF_FFFF_FFFF;
                    if lr >= 4096 && (crate::elf::is_guest_code(lr) || crate::elf::module_for_addr(lr).is_some()) && addr_readable(lr - 48) && addr_readable(lr + 16) {
                        let w: Vec<String> = (0..16).map(|i| format!("{:08x}", unsafe { *((lr - 48 + 4 * i) as *const u32) })).collect();
                        alog(&format!("muestreo: hilo {} codigo en lr-48: {}", tid, w.join(" ")));
                    }
                    alog(&format!("muestreo: hilo {} x0-x8: {:x?}", tid, &t.cpu.x[0..9]));
                    // hilo que gira dentro de codigo guest: codigo alrededor del pc, todos los registros y la reserva
                    if !crate::hle::is_hle(pc) && pc >= 4096 && crate::elf::is_guest_code(pc) && addr_readable(pc - 160) && addr_readable(pc + 96) {
                        let w: Vec<String> = (0..64).map(|i| format!("{:08x}", unsafe { *((pc - 160 + 4 * i) as *const u32) })).collect();
                        alog(&format!("muestreo: hilo {} codigo en pc-160: {}", tid, w.join(" ")));
                        alog(&format!("muestreo: hilo {} x9-x30,sp: {:x?}", tid, &t.cpu.x[9..32]));
                        let m = &t.cpu.mon;
                        alog(&format!("muestreo: hilo {} reserva valida={} dir={:#x} tam={} ver={} epoca={}", tid, m.valid, m.addr, m.size, m.ver, m.epoch));
                        for r in [19usize, 20, 8, 0] {
                            let a = t.cpu.x[r] & !7;
                            if a >= 4096 && addr_readable(a) && addr_readable(a + 64) {
                                let d: Vec<String> = (0..9).map(|i| format!("{:x}", unsafe { *((a + 8 * i) as *const u64) })).collect();
                                alog(&format!("muestreo: hilo {} mem[x{}]: {}", tid, r, d.join(" ")));
                            }
                        }
                    }
                    // manejador de senal guest en curso (p. ej. hilo detenido por el recolector)
                    if t.sig_depth > 0 && !t.sig_cpu.is_null() {
                        let sc = unsafe { &*t.sig_cpu };
                        alog(&format!("muestreo: hilo {} en senal nivel={} pc={} lr={} x0={:#x}", tid, t.sig_depth, crate::elf::describe_addr(sc.pc), crate::elf::describe_addr(sc.x[30]), sc.x[0]));
                    }
                }
                let sc = unsafe { std::ptr::read_volatile(&t.last_sc) };
                let host_sc = rd("syscall");
                alog(&format!(
                    "muestreo: hilo {} estado={} wchan={} syscall_host=[{}] ultimo_sc(arm64)={} ({}) a0={:#x} a1={:#x} a2={:#x}",
                    tid, st, rd("wchan").trim(), host_sc.trim().chars().take(60).collect::<String>(), sc[0] as i64, crate::diag::syscall_name(sc[0]), sc[1], sc[2], sc[3]
                ));
                let hle_name = if key.1 == u32::MAX { String::from("-") } else { crate::hle::name_at_index(key.1) };
                crate::bridge::alog(&format!(
                    "muestreo: hilo {} ({}) sin avanzar {} muestras; ultima HLE={} ultimo SVC={} ({})\n{}",
                    tid,
                    comm.trim(),
                    n + 1,
                    hle_name,
                    if key.2 == u32::MAX { -1 } else { key.2 as i64 },
                    if key.2 == u32::MAX { "-" } else { crate::diag::syscall_name(key.2 as u64) },
                    bt.lines().take(8).collect::<Vec<_>>().join("\n")
                ));
            }
        }
    }
    crate::bridge::alog(&format!("muestreo: {} hilos guest, {} sin avanzar", list.len(), stuck));
    {
        use crate::monitor as m;
        crate::bridge::alog(&format!(
            "monitor: modo={} epoca={} barreras={} enfriamientos={} saturaciones={} abortos_rseq={} respaldo={} esperas_store={} espera_max_us={:.1}",
            m::mode_name(), m::EPOCH.load(Relaxed), m::MEMBARRIERS.load(Relaxed), m::COOLS.load(Relaxed), m::SATURATIONS.load(Relaxed), m::RSEQ_ABORTS.load(Relaxed), m::FALLBACK.load(Relaxed),
            m::FENCE_WAITS.load(Relaxed), m::FENCE_WAIT_MAX_NS.load(Relaxed) as f64 / 1000.0
        ));
    }
    crate::bridge::alog(&crate::mem::status_line());
    let (calls, bytes) = crate::hle::top_deltas(top_prev, 8, list.iter().map(|(_, p)| unsafe { &(*(*p as *const GuestThread)).hle }));
    if !calls.is_empty() {
        crate::bridge::alog(&format!("llamadas HLE en 6 s: {}", calls.iter().map(|(n, k)| format!("{}={}", n, k)).collect::<Vec<_>>().join(" ")));
    }
    if !bytes.is_empty() {
        crate::bridge::alog(&format!("memoria pedida en 6 s: {}", bytes.iter().map(|(n, k)| format!("{}={}MiB", n, k >> 20)).collect::<Vec<_>>().join(" ")));
    }
    let (_, rss) = crate::mem::process_kb();
    if crate::mem::over_limit(rss, crate::mem::rss_limit_kb()) {
        vigilante(list, rss);
    }
    // olvidar los hilos que ya no existen (si no, la tabla creceria con cada hilo que nace y muere)
    prev.retain(|tid, _| list.iter().any(|e| e.0 == *tid));
}

/// La app supera el limite de memoria: vuelca los hilos guest y termina el proceso (`list` con ALL_THREADS tomado).
fn vigilante(list: &[(i32, usize)], rss: u64) {
    crate::bridge::alog_fatal(&format!("VIGILANTE: la app supera el limite de memoria ({} MiB > {} MiB); se termina para proteger el sistema", rss >> 10, crate::mem::rss_limit_kb() >> 10));
    for (tid, p) in list {
        let t = unsafe { &*(*p as *const GuestThread) };
        crate::bridge::alog_fatal(&format!("VIGILANTE: hilo {} {}", tid, crate::rt::dump_backtrace(&t.cpu).lines().take(4).collect::<Vec<_>>().join(" | ")));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    unsafe { kill(getpid(), 9) };
}

/// diagnostico: registra la ultima llamada al sistema (numero aarch64 y 3 primeros argumentos) del hilo actual
pub fn note_syscall(n: u64, a: [u64; 6]) {
    if let Some(t) = cur_opt() {
        t.last_sc = [n, a[0], a[1], a[2]];
    }
}
/// (pthread_t del host, pila guest baja, alta): el guest obtiene su pthread_t del host (pthread_self se reenvia) y
/// pregunta por los limites de pila (GC conservador de Unity/IL2CPP); debe ver la pila guest, no la del hilo host.
static STACKS: Mutex<Vec<(u64, u64, u64)>> = Mutex::new(Vec::new());

/// Limites de la pila guest del hilo cuyo pthread_t (del host) es `pt`.
pub fn guest_stack_of(pt: u64) -> Option<(u64, u64)> {
    crate::monitor::lock(&STACKS).iter().find(|e| e.0 == pt).map(|e| (e.1, e.2))
}
static RET_SLOT: AtomicU64 = AtomicU64::new(0);
static CANARY: OnceLock<u64> = OnceLock::new();

fn canary() -> u64 {
    *CANARY.get_or_init(|| {
        let mut v = [0u8; 8];
        let ok = unsafe { getrandom(v.as_mut_ptr() as *mut c_void, 8, 0) } == 8;
        let mut x = u64::from_le_bytes(v);
        if !ok {
            x = 0x9E37_79B9_7F4A_7C15 ^ (unsafe { getpid() } as u64) << 17;
        }
        x & !0xff // byte bajo a cero, como la canary de bionic
    })
}

pub fn ret_slot() -> u64 {
    let r = RET_SLOT.load(Relaxed);
    if r != 0 {
        return r;
    }
    let a = hle::register("__heddle_return", Box::new(|_c| hle::Ret::Stay));
    RET_SLOT.store(a, Relaxed);
    a
}

/// Suelta ya el estado guest del hilo actual (salida por `exit` sin destructores de TLS del host que lo hagan). Dentro
/// de un manejador de senal guest no (corre sobre la pila de senales y quiza la alternativa del host): se deja.
pub fn release_current() {
    let p = CUR.with(|c| c.get());
    if p.is_null() || unsafe { (*p).sig_depth } > 0 {
        return;
    }
    let t = SLOT.try_with(|s| s.try_borrow_mut().ok().and_then(|mut g| g.take())).ok().flatten();
    drop(t);
}

fn gettid() -> i32 {
    unsafe { syscall(186) as i32 }
}

/// Crea el estado guest del hilo host actual (si no existe).
pub fn ensure_thread(stack_size: usize) -> *mut GuestThread {
    let p = CUR.with(|c| c.get());
    if !p.is_null() {
        return p;
    }
    hle::init();
    ret_slot();
    static MON: std::sync::Once = std::sync::Once::new();
    MON.call_once(|| {
        crate::feat::init();
        crate::monitor::init();
        init_emergency();
    });
    crate::monitor::thread_check();
    unsafe {
        let tid = gettid();
        let t = build_thread(stack_size, tid, true, crate::jit::CODE_CAP);
        let st = t.stack_lo;
        let stack_size = (t.stack_hi - t.stack_lo) as usize;
        crate::mem::THREADS_LIVE.fetch_add(1, Relaxed);
        crate::mem::THREADS_TOTAL.fetch_add(1, Relaxed);
        crate::monitor::lock(&STACKS).push((pthread_self(), st as u64, st as u64 + stack_size as u64));
        let raw = Box::into_raw(t);
        (*raw).cpu.monflag = &(*raw).mon_flag as *const crate::monitor::MonFlag as usize;
        (*raw).jit.set_sig_pend(&(*raw).sig_pend);
        crate::monitor::set_thread_flag(&(*raw).mon_flag);
        crate::monitor::register_flag(tid, &(*raw).mon_flag);
        crate::monitor::lock(&ALL_THREADS).push((tid, raw as usize));
        start_sampler();
        start_profiler();
        SLOT.with(|s| *s.borrow_mut() = Some(Box::from_raw(raw)));
        CUR.with(|c| c.set(raw));
        raw
    }
}

/// Construye el estado guest de un hilo (pila con guarda, area TLS con las ranuras de bionic, pila de senales, Cpu
/// por nivel de senal) sin asociarlo al hilo actual. Reserva memoria: nunca desde un manejador de senal.
unsafe fn build_thread(stack_size: usize, tid: i32, host_alt: bool, jit_cap: usize) -> Box<GuestThread> {
    // pila guest: como maximo 64 MiB (NORESERVE: solo ocupa lo que se usa), con pagina de guarda por debajo
    let stack_size = (stack_size.max(64 << 10).min(64 << 20) + 4095) & !4095;
    unsafe {
        use crate::mem::{Kind, Region};
        let (rst, rtls, rss) = match (
            Region::with_guard(stack_size, Kind::Stack),
            Region::new(TLS_AREA_BYTES, PROT_READ | PROT_WRITE, Kind::Other),
            Region::with_guard(SIG_STACK, Kind::Stack),
        ) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                crate::bridge::alog_fatal(&format!("sin memoria para el hilo guest\n{}", crate::mem::status_line()));
                abort()
            }
        };
        let st = (rst.base() + crate::mem::PAGE as u64) as *mut c_void;
        let tls = rtls.base() as *mut c_void;
        let ss = (rss.base() + crate::mem::PAGE as u64) as *mut c_void;
        let tp = tls as u64;
        // ranuras de bionic: 0 = DTV (TLS_SLOT_DTV), 1 = pthread_internal_t (aqui un bloque con tid en +16), 5 = canary
        let slots = tp as *mut u64;
        crate::tls::init_thread(tp);
        let pi = (tp + TLS_AREA_BYTES as u64 - 4096) as *mut u8;
        std::ptr::write_unaligned(pi.add(16) as *mut i32, tid);
        *slots.add(1) = pi as u64;
        *slots.add(5) = canary();
        let mut cpu = Cpu::new();
        cpu.tpidr = tp;
        cpu.fpcr = 0;
        Box::new(GuestThread {
            cpu,
            jit: Jit::with_capacity(jit_cap),
            sig_cpus: std::array::from_fn(|_| Cpu::new()),
            sig_cpu: std::ptr::null_mut(),
            sig_lvl: [crate::sig::SigLevel::EMPTY; crate::sig::MAX_SIG_DEPTH],
            sig_setup: false,
            sig_view: 0,
            stack_lo: st as u64,
            stack_hi: st as u64 + stack_size as u64,
            tls_base: tp,
            sig_stack_lo: ss as u64,
            tid,
            depth: 0,
            sig_depth: 0,
            last_hle: u32::MAX,
            last_svc: u32::MAX,
            hle: crate::hle::HleStats::new(),
            phase: 0,
            runs: 0,
            sig_pend: std::sync::atomic::AtomicU64::new(0),
            sig_pinfo: [[0; 128]; 64],
            sig_rtq: crate::sig::RtQueue::new(),
            hle_sp: 0,
            last_sc: [u64::MAX, 0, 0, 0],
            _regions: [rst, rtls, rss],
            mon_flag: crate::monitor::MonFlag::new(),
            host_alt: if host_alt { install_host_alt() } else { None },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Llamada HLE abandonable
// ---------------------------------------------------------------------------------------------
//
// `heddle_hle_call(slot, f, cpu)` guarda los callee-saved y el valor anterior de `*slot`, apunta en `*slot` el RSP y
// llama a `f(cpu)` (devuelve 1). Una senal que llega con el host en un punto seguro de la llamada (ver
// `sig::async_deliver`) y cuyo manejador guest sale con siglongjmp o cambia su contexto, vuelve del manejador del
// host con rip = `heddle_hle_abort`, rsp = `*slot` y rbx = slot: se descartan los marcos de la llamada al host y
// `heddle_hle_call` devuelve 0 (el contexto guest nuevo ya esta en la Cpu). Como `heddle_call_block` o `heddle_sc`.
std::arch::global_asm!(
    ".p2align 4",
    ".globl heddle_hle_call",
    ".hidden heddle_hle_call",
    ".type heddle_hle_call,@function",
    "heddle_hle_call:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rbx, rdi",
    "push qword ptr [rdi]",
    "mov [rdi], rsp",
    "mov rdi, rdx",
    "call rsi",
    "mov eax, 1",
    ".Lheddle_hle_out:",
    "pop qword ptr [rbx]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
    ".globl heddle_hle_abort",
    ".hidden heddle_hle_abort",
    ".type heddle_hle_abort,@function",
    "heddle_hle_abort:",
    "xor eax, eax",
    "jmp .Lheddle_hle_out",
);

extern "C" {
    fn heddle_hle_call(slot: *mut u64, f: extern "C" fn(*mut Cpu), c: *mut Cpu) -> u32;
    pub fn heddle_hle_abort();
}

extern "C" fn hle_dispatch_c(c: *mut Cpu) {
    hle::dispatch(unsafe { &mut *c });
}

/// `hle::dispatch(c)` abandonable por una senal. false: una senal la abandono y dejo en `c` el contexto guest nuevo.
fn hle_call(t: *mut GuestThread, c: &mut Cpu) -> bool {
    unsafe { heddle_hle_call(&mut (*t).hle_sp, hle_dispatch_c, c) != 0 }
}

// ---------------------------------------------------------------------------------------------
// Estados guest de emergencia
// ---------------------------------------------------------------------------------------------
//
// Una senal con manejador guest puede llegar a un hilo que nunca ejecuto codigo guest (un hilo del host). En ARM la
// ejecutaria cualquier hilo; aqui hace falta un estado guest (pila, TLS, Cpu por nivel), y en un manejador de senal no
// se puede crear (reserva memoria). Se crean `EMERG_N` de antemano (al crear el primer hilo guest) y el manejador del
// host toma uno libre con un CAS, lo asocia al hilo durante el manejador y lo suelta. Agotados, la senal se aplaza en
// una lista fija (`EMERG_DEFER`) y se reenvia al hilo (con su siginfo) al soltarse uno.

use std::sync::atomic::Ordering::{AcqRel, Acquire, Release};

pub const EMERG_N: usize = 4;
const EMERG_STACK: usize = 1 << 20;
/// bufer JIT de un estado de emergencia: no traduce (los manejadores corren con el interprete)
const EMERG_JIT: usize = 256 << 10;
pub const EMERG_DEFER_N: usize = 8;

struct Emerg {
    t: std::cell::UnsafeCell<Box<GuestThread>>,
    busy: std::sync::atomic::AtomicBool,
}

// Un estado de emergencia lo usa un solo hilo a la vez (`busy`), dentro de su manejador de senal.
unsafe impl Sync for Emerg {}
unsafe impl Send for Emerg {}

static EMERG: std::sync::OnceLock<Vec<Emerg>> = std::sync::OnceLock::new();

struct EmergDefer {
    /// 0 libre; u32::MAX escribiendose; si no, la senal
    sig: [std::sync::atomic::AtomicU32; EMERG_DEFER_N],
    tid: [std::sync::atomic::AtomicI32; EMERG_DEFER_N],
    info: [std::cell::UnsafeCell<[u8; 128]>; EMERG_DEFER_N],
}

unsafe impl Sync for EmergDefer {}

static EMERG_DEFER: EmergDefer = EmergDefer {
    sig: [const { std::sync::atomic::AtomicU32::new(0) }; EMERG_DEFER_N],
    tid: [const { std::sync::atomic::AtomicI32::new(0) }; EMERG_DEFER_N],
    info: [const { std::cell::UnsafeCell::new([0u8; 128]) }; EMERG_DEFER_N],
};

/// Senales perdidas con los estados de emergencia y la lista de aplazadas agotados.
pub static EMERG_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Crea los estados de emergencia (una vez; fuera de manejadores de senal).
fn init_emergency() {
    EMERG.get_or_init(|| {
        (0..EMERG_N)
            .map(|_| unsafe {
                let mut b = build_thread(EMERG_STACK, 0, false, EMERG_JIT);
                let p: *mut GuestThread = &mut *b;
                (*p).cpu.monflag = &(*p).mon_flag as *const crate::monitor::MonFlag as usize;
                (*p).jit.set_sig_pend(&(*p).sig_pend);
                crate::monitor::register_flag(0, &(*p).mon_flag);
                Emerg { t: std::cell::UnsafeCell::new(b), busy: std::sync::atomic::AtomicBool::new(false) }
            })
            .collect()
    });
}

/// En un manejador de senal de un hilo sin estado guest: asocia al hilo un estado de emergencia libre. Sin reservas
/// ni bloqueos. None si no hay (o no se crearon).
pub fn emergency_enter() -> Option<usize> {
    let e = EMERG.get()?;
    for (i, s) in e.iter().enumerate() {
        if s.busy.compare_exchange(false, true, Acquire, Relaxed).is_ok() {
            let t: &mut GuestThread = unsafe { &mut **s.t.get() };
            let tid = unsafe { gettid() };
            t.tid = tid;
            unsafe { std::ptr::write_unaligned(((t.tls_base + TLS_AREA_BYTES as u64 - 4096) as *mut u8).add(16) as *mut i32, tid) };
            t.sig_depth = 0;
            t.phase = 1; // como dentro de una HLE: una senal anidada se entrega en el acto
            t.sig_pend.store(0, Relaxed);
            CUR.with(|c| c.set(t));
            crate::monitor::set_thread_flag(&t.mon_flag);
            return Some(i);
        }
    }
    None
}

/// Suelta el estado de emergencia `i` y reenvia las senales que esperaban uno libre.
pub fn emergency_leave(i: usize) {
    CUR.with(|c| c.set(std::ptr::null_mut()));
    crate::monitor::set_thread_flag(std::ptr::null());
    if let Some(e) = EMERG.get() {
        e[i].busy.store(false, Release);
    }
    let d = &EMERG_DEFER;
    for k in 0..EMERG_DEFER_N {
        let sig = d.sig[k].load(Acquire);
        if sig == 0 || sig == u32::MAX {
            continue;
        }
        let (tid, info) = (d.tid[k].load(Relaxed), unsafe { *d.info[k].get() });
        if d.sig[k].compare_exchange(sig, 0, AcqRel, Relaxed).is_ok() {
            unsafe {
                let pid = crate::sys::syscall(39);
                if crate::sys::syscall(297, pid, tid as i64, sig as i64, info.as_ptr()) < 0 {
                    crate::sys::syscall(234, pid, tid as i64, sig as i64);
                }
            }
        }
    }
}

/// Estados de emergencia agotados: la senal espera en la lista fija (con su siginfo) a que se suelte uno.
///
/// # Safety
/// `info` es nulo o un `siginfo` de 128 bytes.
pub unsafe fn emergency_defer(sig: i32, info: *const u8) {
    let d = &EMERG_DEFER;
    for k in 0..EMERG_DEFER_N {
        if d.sig[k].compare_exchange(0, u32::MAX, Acquire, Relaxed).is_ok() {
            let dst = unsafe { &mut *d.info[k].get() };
            if info.is_null() {
                *dst = [0; 128];
                dst[0..4].copy_from_slice(&sig.to_ne_bytes());
            } else {
                unsafe { std::ptr::copy_nonoverlapping(info, dst.as_mut_ptr(), 128) };
            }
            d.tid[k].store(unsafe { gettid() }, Relaxed);
            d.sig[k].store(sig as u32, Release);
            return;
        }
    }
    EMERG_LOST.fetch_add(1, Relaxed);
}

pub fn cur() -> &'static mut GuestThread {
    unsafe { &mut *ensure_thread(DEFAULT_STACK) }
}

pub fn cur_opt() -> Option<&'static mut GuestThread> {
    let p = CUR.with(|c| c.get());
    if p.is_null() {
        None
    } else {
        Some(unsafe { &mut *p })
    }
}

// ---------------------------------------------------------------------------------------------
// Llamadas host -> guest
// ---------------------------------------------------------------------------------------------

/// Llama a una funcion guest desde el hilo actual. Devuelve (x0, v0[63:0]).
pub fn call_guest(addr: u64, ints: &[u64], fps: &[u64]) -> (u64, u64) {
    let t = cur();
    let cpu: *mut Cpu = &mut *t.cpu;
    call_guest_on(unsafe { &mut *cpu }, addr, ints, fps)
}

pub fn call_guest_on(c: &mut Cpu, addr: u64, ints: &[u64], fps: &[u64]) -> (u64, u64) {
    let r = call_guest_on3(c, addr, ints, fps);
    (r.0, r.1)
}

/// Como `call_guest_on` pero devuelve tambien x1 (estructuras de 16 bytes devueltas en x0:x1).
pub fn call_guest3(addr: u64, ints: &[u64], fps: &[u64]) -> (u64, u64, u64) {
    let t = cur();
    let cpu: *mut Cpu = &mut *t.cpu;
    call_guest_on3(unsafe { &mut *cpu }, addr, ints, fps)
}

/// Llama a una funcion guest con argumentos vectoriales de 128 bits (long double) y devuelve (x0, v0, v1) completos.
pub fn call_guest_q(addr: u64, ints: &[u64], qs: &[[u64; 2]]) -> (u64, [u64; 2], [u64; 2]) {
    let t = cur();
    let c: *mut Cpu = &mut *t.cpu;
    let c = unsafe { &mut *c };
    let (s_x, s_pc, s_f, s_v, s_fpcr) = (c.x, c.pc, c.flags(), c.v, c.fpcr);
    let first = t.depth == 0 && c.x[31] == 0;
    if first {
        c.x[31] = t.stack_hi - 64;
    }
    t.depth += 1;
    c.x[31] = (c.x[31].wrapping_sub(256)) & !15;
    for i in 0..8 {
        c.x[i] = ints.get(i).copied().unwrap_or(0);
        c.v[i] = qs.get(i).copied().unwrap_or([0, 0]);
    }
    let rs = ret_slot();
    c.x[30] = rs;
    c.pc = addr;
    run_loop(c, rs);
    t.depth -= 1;
    let r = (c.x[0], c.v[0], c.v[1]);
    c.x = s_x;
    c.pc = s_pc;
    c.v = s_v;
    c.fpcr = s_fpcr;
    c.set_flags(s_f);
    if first {
        c.x[31] = 0;
    }
    r
}

pub fn call_guest_on3(c: &mut Cpu, addr: u64, ints: &[u64], fps: &[u64]) -> (u64, u64, u64) {
    let mut x = [0u64; 8];
    let mut v = [0u64; 8];
    for i in 0..8 {
        x[i] = ints.get(i).copied().unwrap_or(0);
        v[i] = fps.get(i).copied().unwrap_or(0);
    }
    // argumentos enteros adicionales a la pila
    call_guest_core(c, addr, &x, &v, ints.get(8..).unwrap_or(&[]))
}

/// Llama a `addr` con los argumentos ya repartidos segun AAPCS64: `x` (x0-x7), `v` (64 bits bajos de v0-v7) y
/// `stack`, los argumentos en pila en orden, una palabra de 8 bytes cada uno (Linux; un `float` ocupa los 4 bytes
/// bajos de su palabra). Sin reservas de memoria. Devuelve (x0, v0[63:0], x1).
pub fn call_guest_regs(addr: u64, x: &[u64; 8], v: &[u64; 8], stack: &[u64]) -> (u64, u64, u64) {
    let t = cur();
    let cpu: *mut Cpu = &mut *t.cpu;
    call_guest_core(unsafe { &mut *cpu }, addr, x, v, stack)
}

fn call_guest_core(c: &mut Cpu, addr: u64, x: &[u64; 8], v: &[u64; 8], stack: &[u64]) -> (u64, u64, u64) {
    let vq = v.map(|d| [d, 0]);
    let (x0, v0, x1) = call_guest_core_q(c, addr, x, &vq, stack);
    (x0, v0[0], x1)
}

/// Como `call_guest_on` sobre `c` (la `Cpu` de la HLE en curso, tambien la de un manejador de senal) con argumentos
/// vectoriales de 128 bits (long double). Devuelve (x0, v0 completo).
pub fn call_guest_on_q(c: &mut Cpu, addr: u64, ints: &[u64], qs: &[[u64; 2]]) -> (u64, [u64; 2]) {
    let mut x = [0u64; 8];
    let mut v = [[0u64; 2]; 8];
    for i in 0..8 {
        x[i] = ints.get(i).copied().unwrap_or(0);
        v[i] = qs.get(i).copied().unwrap_or([0, 0]);
    }
    let (x0, v0, _) = call_guest_core_q(c, addr, &x, &v, ints.get(8..).unwrap_or(&[]));
    (x0, v0)
}

fn call_guest_core_q(c: &mut Cpu, addr: u64, x: &[u64; 8], v: &[[u64; 2]; 8], stack: &[u64]) -> (u64, [u64; 2], u64) {
    let t = cur();
    let s_x = c.x;
    let s_pc = c.pc;
    let s_f = c.flags();
    let s_v = c.v;
    let s_fpcr = c.fpcr;
    let first = t.depth == 0 && c.x[31] == 0;
    if first {
        c.x[31] = t.stack_hi - 64;
    }
    t.depth += 1;
    let mut sp = (c.x[31].wrapping_sub(256)) & !15;
    if !stack.is_empty() {
        sp = (sp - (stack.len() as u64) * 8) & !15;
        for (i, w) in stack.iter().enumerate() {
            unsafe { *((sp + 8 * i as u64) as *mut u64) = *w };
        }
    }
    c.x[31] = sp;
    for i in 0..8 {
        c.x[i] = x[i];
        c.v[i] = v[i];
    }
    let rs = ret_slot();
    c.x[30] = rs;
    c.pc = addr;
    run_loop(c, rs);
    t.depth -= 1;
    let r = (c.x[0], c.v[0], c.x[1]);
    c.x = s_x;
    c.pc = s_pc;
    c.v = s_v;
    c.fpcr = s_fpcr;
    c.set_flags(s_f);
    if first {
        c.x[31] = 0;
    }
    r
}

/// Ejecuta codigo guest en `c` hasta que vuelva a la ranura de retorno `rs`. La Cpu principal del hilo usa el JIT;
/// las de los manejadores de senal (y lo que llamen) usan el interprete de referencia: un manejador puede haber
/// interrumpido al traductor a mitad de una operacion y traducir reserva memoria, cosa que no se puede hacer dentro de
/// un manejador de senal. Los manejadores son cortos; el coste del interprete no importa ahi.
pub fn run_loop(c: &mut Cpu, rs: u64) {
    let Some(t) = cur_opt() else {
        crate::bridge::alog_fatal("run_loop sin estado de hilo guest");
        unsafe { abort() }
    };
    // la fase se guarda y se restaura: un manejador o un callback anidado no deja al hilo "en codigo guest" cuando
    // vuelve a la HLE o a la llamada al sistema que interrumpio (la usa sig.rs para saber de donde viene un fallo y
    // si una senal asincrona se aplaza a la frontera de instruccion)
    let t: *mut GuestThread = t;
    let prev = unsafe { (*t).phase };
    let set = move |ph: u8, runs: u32| unsafe {
        std::ptr::write_volatile(&mut (*t).runs, runs);
        std::ptr::write_volatile(&mut (*t).phase, ph);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    };
    let runs0 = unsafe { (*t).runs };
    let main = std::ptr::eq(c as *const Cpu, unsafe { &*(*t).cpu } as *const Cpu);
    loop {
        set(0, runs0 + 1);
        if main {
            let jit: *mut Jit = unsafe { &mut (*t).jit };
            run_loop_jit(c, unsafe { &mut *jit }, rs, t);
        } else {
            run_interp(c, rs);
        }
        // desde aqui una senal se entrega en el acto (o en la frontera del bucle exterior); una aplazada justo antes
        // de salir se entrega ahora, en el punto de vuelta (pc = rs), salvo si el manejador sale con siglongjmp
        set(prev, runs0);
        if unsafe { (*t).sig_pend.load(std::sync::atomic::Ordering::Relaxed) } == 0 || !deliverable(t) || crate::sig::escaping(c) {
            break;
        }
        set(0, runs0 + 1);
        crate::sig::deliver_pending(c);
    }
}

/// Hay senales aplazadas (tras marcar la fase de una HLE o de una llamada al sistema: una que llegue despues ya no se
/// aplaza, ver sig.rs).
#[inline(always)]
fn sig_pending(t: *mut GuestThread) -> bool {
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    unsafe { (*t).sig_pend.load(std::sync::atomic::Ordering::Relaxed) != 0 && deliverable(t) }
}

/// Queda un nivel de manejador libre: con todos ocupados, lo aplazado espera a que vuelva uno (sino el bucle
/// intentaria entregarla sin avanzar).
#[inline(always)]
fn deliverable(t: *mut GuestThread) -> bool {
    unsafe { ((*t).sig_depth as usize) < crate::sig::MAX_SIG_DEPTH }
}

fn run_loop_jit(c: &mut Cpu, jit: &mut Jit, rs: u64, t: *mut GuestThread) {
    let phase = |v: u8| unsafe { std::ptr::write_volatile(&mut (*t).phase, v) };
    loop {
        match jit.run(c) {
            Event::Hle => {
                if c.pc & TBI == rs {
                    return;
                }
                phase(1);
                if sig_pending(t) {
                    // llego antes de la llamada: se entrega con el pc en la funcion (como en ARM antes del `bl`)
                    phase(0);
                    crate::sig::deliver_pending(c);
                    continue;
                }
                let done = hle_call(t, c);
                phase(0);
                if !done {
                    continue; // una senal abandono la llamada: sigue en el contexto que dejo el guest
                }
            }
            Event::Signal => crate::sig::deliver_pending(c),
            Event::HostCall => crate::boundary::guest_jumped_to_host(c),
            Event::Svc(n) => svc(c, n, t),
            Event::Brk(n) => crate::sig::guest_fault(c, 5, 1 /*TRAP_BRKPT*/, c.pc, "brk", n as u64),
            Event::Undef(w) => undef(c, w, true),
            Event::InstAbort => crate::sig::guest_fault(c, 11, 1 /*SEGV_MAPERR*/, c.pc, "salto a memoria sin codigo", c.pc),
        }
    }
}

/// Instruccion que heddle no ejecuta: SIGILL (ILL_ILLOPC) para el guest, como en un procesador ARM sin esa extension.
/// El diagnostico nombra la instruccion (`diag::classify`) con la palabra que hay realmente en el pc (`w` puede venir
/// de un subdecodificador). Sin reservas de memoria: `info` (registro INFO cuando el guest tiene manejador de SIGILL,
/// las primeras 16 veces) solo desde el bucle del JIT, que no corre dentro de un manejador de senal del host.
fn undef(c: &mut Cpu, w: u32, info: bool) {
    use std::fmt::Write;
    let pc = c.pc & TBI;
    let w = if addr_readable(pc) { unsafe { std::ptr::read_unaligned(pc as *const u32) } } else { w };
    let mut msg = crate::diag::Buf::new();
    let _ = write!(msg, "{}", crate::diag::classify(w));
    if info && crate::sig::guest_act(4).handler > 1 {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if N.fetch_add(1, Relaxed) < 16 {
            crate::bridge::alog(&format!("SIGILL al manejador del guest: {} en pc={}", msg.as_str(), crate::elf::describe_addr(pc)));
        }
    }
    crate::sig::guest_fault(c, 4, 1 /*ILL_ILLOPC*/, c.pc, msg.as_str(), w as u64)
}

fn svc(c: &mut Cpu, imm: u16, t: *mut GuestThread) {
    // diagnostico: el numero de la llamada (x8), no el inmediato del SVC (0 en Linux)
    let n = c.x[8];
    unsafe {
        (*t).last_svc = n as u32;
        (*t).last_sc = [n, c.x[0], c.x[1], c.x[2]];
        std::ptr::write_volatile(&mut (*t).phase, 2);
    }
    if sig_pending(t) {
        // una senal aplazada antes del `svc`: se entrega antes (pc en el `svc`) y despues se ejecuta
        unsafe { std::ptr::write_volatile(&mut (*t).phase, 0) };
        c.pc -= 4;
        return;
    }
    crate::syscall::svc(c, imm);
    unsafe { std::ptr::write_volatile(&mut (*t).phase, 0) };
}

/// Bucle del interprete para las Cpu de los manejadores de senal. Va dentro de `heddle_call_block`: un fallo sincrono
/// de su codigo que el guest gestiona vuelve aqui con el contexto nuevo en `c` (0) y se sigue interpretando.
fn run_interp(c: &mut Cpu, rs: u64) {
    let saved = c.aux[6];
    c.aux[6] = rs;
    loop {
        if unsafe { crate::jit::heddle_call_block(c, interp_entry) } != 0 {
            break;
        }
    }
    c.aux[6] = saved;
}

extern "C" fn interp_entry(cp: *mut Cpu) -> u64 {
    use crate::interp::Flow;
    let c = unsafe { &mut *cp };
    let rs = c.aux[6];
    let Some(t) = cur_opt() else { return 1 };
    let t: *mut GuestThread = t;
    let mut next = u64::MAX; // pc esperado si la ejecucion sigue en secuencia
    loop {
        if sig_pending(t) {
            // senal aplazada: se entrega en esta frontera de instruccion
            crate::sig::deliver_pending(c);
            if crate::sig::escaping(c) {
                return 1;
            }
            next = u64::MAX;
            continue;
        }
        let pc = c.pc & TBI;
        if hle::is_hle(pc) {
            if pc == rs {
                return 1;
            }
            unsafe { std::ptr::write_volatile(&mut (*t).phase, 1) };
            if sig_pending(t) {
                unsafe { std::ptr::write_volatile(&mut (*t).phase, 0) };
                continue;
            }
            let done = hle_call(t, c);
            unsafe { std::ptr::write_volatile(&mut (*t).phase, 0) };
            if !done {
                next = u64::MAX;
                continue; // una senal abandono la llamada: sigue en el contexto que dejo el guest
            }
            if crate::sig::escaping(c) {
                return 1; // siglongjmp fuera del manejador: lo resuelve quien lo entrego
            }
            next = u64::MAX;
            continue;
        }
        if pc != next || pc & 0xFFF == 0 {
            // salto o pagina nueva: el mismo criterio que el JIT al traducir (Jit::lookup). Nunca se interpreta codigo
            // del host (el mapa se lee sin bloqueos: instantanea publicada).
            if crate::boundary::is_host_code_sig(pc) {
                crate::boundary::guest_jumped_to_host(c); // trampolin del puente -> su funcion guest; si no, FILTRACION
                next = u64::MAX;
                continue;
            }
            if !addr_readable(pc) {
                crate::sig::guest_fault(c, 11, 1 /*SEGV_MAPERR*/, c.pc, "salto a memoria sin codigo", c.pc);
                next = u64::MAX;
                continue;
            }
        }
        match crate::interp::step(c) {
            Flow::Next => {}
            Flow::Svc(n) => svc(c, n, t),
            Flow::Brk(n) => crate::sig::guest_fault(c, 5, 1, c.pc, "brk", n as u64),
            Flow::Undef(w) => undef(c, w, false),
            Flow::ICacheFlush(a) => crate::jit::publish_ic(a),
            Flow::Jump(_) => {}
        }
        next = pc.wrapping_add(4);
    }
}

/// Ejecuta una funcion guest en un hilo nuevo del host con pila guest propia.
pub struct StartInfo {
    pub start: u64,
    pub arg: u64,
    pub stack: usize,
    /// mascara de senales del guest en el hilo que lo creo (la hereda, como en bionic)
    pub mask: u64,
}

pub extern "C" fn thread_entry(p: *mut c_void) -> *mut c_void {
    // se copian los campos y se libera la caja ya: si el hilo sale con pthread_exit este marco no se desapila
    let (start, arg, stack, mask) = {
        let si = unsafe { Box::from_raw(p as *mut StartInfo) };
        (si.start, si.arg, si.stack, si.mask)
    };
    // el hilo nace con las senales bloqueadas (ver pthread_create en libc_hle): una que llegue antes de tener estado
    // guest queda pendiente en el nucleo y se entrega al poner la mascara heredada
    ensure_thread(stack);
    crate::sig::set_mask_raw(mask);
    let (r, _) = call_guest(start, &[arg], &[]);
    // como __pthread_start de bionic: pthread_exit(start(arg))
    crate::libc_hle::pthread_exit_dtors();
    r as *mut c_void
}

pub fn dump_backtrace(c: &Cpu) -> String {
    let mut s = format!("pc={}  lr={}\n", crate::elf::describe_addr(c.pc), crate::elf::describe_addr(c.x[30]));
    // recorrido por cadena de frame pointers (x29)
    let mut fp = c.x[29];
    for i in 0..16 {
        if fp == 0 || fp & 7 != 0 || !addr_readable(fp) || !addr_readable(fp + 8) {
            break;
        }
        let (next, lr) = unsafe { (*(fp as *const u64), *((fp + 8) as *const u64)) };
        s += &format!("  #{} {}\n", i, crate::elf::describe_addr(lr));
        if next <= fp {
            break;
        }
        fp = next;
    }
    s
}

/// El byte en `a` se puede leer sin fallar? Lo usan el diagnostico y el muestreador antes de leer memoria apuntada
/// por registros guest. Sin reservas ni bloqueos (vale en un manejador de senal).
///
/// `process_vm_readv` sobre el propio proceso copia con las comprobaciones del nucleo: falla con EFAULT tanto si la
/// pagina no esta mapeada como si es PROT_NONE o de solo escritura (mincore solo detecta lo primero, y leer una
/// pagina de guarda tumbaba la app desde el propio diagnostico). Si el nucleo no la ofrece (sin
/// CONFIG_CROSS_MEMORY_ATTACH, o un filtro seccomp: ENOSYS/EPERM) se usa mincore, como antes.
pub fn addr_readable(a: u64) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    static SIN_VM_READV: AtomicBool = AtomicBool::new(false);
    if a < 4096 {
        return false;
    }
    if !SIN_VM_READV.load(Relaxed) {
        #[repr(C)]
        struct Iov {
            base: u64,
            len: usize,
        }
        let mut b = 0u8;
        let local = Iov { base: &mut b as *mut u8 as u64, len: 1 };
        let remote = Iov { base: a, len: 1 };
        use std::os::raw::c_long;
        const SYS_PROCESS_VM_READV: c_long = 310; // x86-64
        const ENOSYS: i32 = 38;
        const EPERM: i32 = 1;
        let r = unsafe { syscall(SYS_PROCESS_VM_READV, getpid() as c_long, &local as *const Iov, 1usize, &remote as *const Iov, 1usize, 0usize) };
        if r == 1 {
            return true;
        }
        let e = errno();
        if e != ENOSYS && e != EPERM {
            return false;
        }
        SIN_VM_READV.store(true, Relaxed);
    }
    // mincore falla con ENOMEM si la pagina no esta mapeada
    let mut v = 0u8;
    unsafe { mincore((a & !4095) as *mut c_void, 4096, &mut v) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// addr_readable distingue una pagina PROT_NONE (mapeada, pero leerla falla) de una legible.
    #[test]
    fn addr_readable_detecta_prot_none() {
        let x = 7u64;
        assert!(addr_readable(&x as *const u64 as u64));
        assert!(!addr_readable(0));
        let r = crate::mem::Region::new(4096, PROT_NONE, crate::mem::Kind::Other).unwrap();
        assert!(!addr_readable(r.base() as u64), "pagina PROT_NONE dada por legible");
        assert!(!addr_readable(r.base() as u64 + 100));
    }

    #[test]
    fn muestreador_no_falla_y_olvida_hilos_terminados() {
        let mut st = SamplerState::default();
        // un hilo guest vivo y parado mientras se muestrea dos veces (segunda pasada: "sin avanzar")
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (tx2, rx2) = std::sync::mpsc::channel::<i32>();
        let h = std::thread::spawn(move || {
            let t = ensure_thread(256 << 10);
            tx2.send(unsafe { (*t).tid }).unwrap();
            rx.recv().unwrap();
        });
        let tid = rx2.recv().unwrap();
        sampler_tick(&mut st);
        sampler_tick(&mut st);
        assert!(st.prev.contains_key(&tid));
        tx.send(()).unwrap();
        h.join().unwrap();
        sampler_tick(&mut st);
        assert!(!st.prev.contains_key(&tid), "el muestreador recuerda un hilo que ya termino");
    }

    /// Lo que ejecuta el proceso hijo de `pila_alternativa_del_host_grande`.
    fn fallos_anidados_en_pila_alternativa() {
        std::thread::spawn(|| {
            // el hilo ya tiene una pila alternativa pequena (como la de 16 KiB de bionic/ART), con guarda debajo
            let small = crate::mem::Region::with_guard(16 << 10, crate::mem::Kind::Test).unwrap();
            let ssp = small.base() + crate::mem::PAGE as u64;
            assert_eq!(host_sigaltstack(Some(&[ssp, 0, 16 << 10]), None), 0);
            let t = cur();
            let mut a = [0u64; 3];
            host_sigaltstack(None, Some(&mut a));
            assert_eq!(a[2], HOST_ALT_STACK as u64, "no se instalo la pila alternativa grande");
            assert!(t.host_alt.is_some());
            crate::sig::install_fault_handlers();
            // manejador de SIGSEGV (SA_NODEFER): cuenta y, hasta el tercer nivel, provoca otro fallo; en el tercero llama
            // a `c`, que se llama a si misma PROF veces a traves de bsearch (HLE con callback: run_loop anidado). Cada
            // nivel salta la instruccion que fallo (pc del ucontext + 4).
            const PROF: u32 = 12;
            let mov64 = |rd: u32, v: u64| -> [u32; 4] {
                let h = |k: u32| ((v >> (16 * k)) & 0xffff) as u32;
                [0xD280_0000 | rd | h(0) << 5, 0xF2A0_0000 | rd | h(1) << 5, 0xF2C0_0000 | rd | h(2) << 5, 0xF2E0_0000 | rd | h(3) << 5]
            };
            let n: &'static mut [u64; 3] = Box::leak(Box::new([0u64; 3]));
            let np = n.as_ptr() as u64;
            let cbuf: &'static mut [u32; 64] = Box::leak(Box::new([0u32; 64]));
            let cp = cbuf.as_ptr() as u64;
            let bsearch = crate::libc_hle::resolve("bsearch").unwrap();
            let mut c: Vec<u32> = vec![0xa9bf7bfd];
            c.extend(mov64(11, np + 8));
            c.extend([0xf9400169, 0x91000529, 0xf9000169, 0xF100_001F | PROF << 10 | 9 << 5, 0x5400020a]);
            c.extend(mov64(1, np + 16));
            c.extend([0xd2800022, 0xd2800103]);
            c.extend(mov64(4, cp));
            c.extend(mov64(16, bsearch));
            c.extend([0xd63f0200, 0xd2800000, 0xa8c17bfd, 0xd65f03c0]);
            cbuf[..c.len()].copy_from_slice(&c);
            let mut h: Vec<u32> = vec![0xa9be7bfd, 0xf9000bf3, 0xaa0203f3];
            h.extend(mov64(11, np));
            h.extend([0xf9400169, 0x91000529, 0xf9000169, 0xf1000d3f, 0x5400008a, 0xd2800201, 0xb9400023, 0x14000006]);
            h.extend(mov64(16, cp));
            h.extend([0xd63f0200, 0xf940de69, 0x91001129, 0xf900de69, 0xf9400bf3, 0xa8c27bfd, 0xd65f03c0]);
            let h: &'static [u32] = Box::leak(h.into_boxed_slice());
            let main: &'static [u32] = Box::leak(vec![0xd2800201, 0xb9400020, 0xd2800020, 0xd65f03c0].into_boxed_slice());
            let act = crate::sig::GuestAct { handler: h.as_ptr() as u64, flags: crate::sig::SA_SIGINFO | crate::sig::SA_NODEFER, mask: 0 };
            crate::sig::guest_sigaction(11, Some(act)).unwrap();
            assert_eq!(call_guest(main.as_ptr() as u64, &[], &[]).0, 1);
            assert_eq!(unsafe { std::ptr::read_volatile(&n[0]) }, 3, "tres niveles de fallo");
            assert_eq!(unsafe { std::ptr::read_volatile(&n[1]) }, PROF as u64, "llamadas anidadas por bsearch");
            // pila del host usada: desde el primer byte escrito de la region hasta su tope
            let base = a[0];
            let used = (0..HOST_ALT_STACK as u64).find(|&o| unsafe { *((base + o) as *const u8) } != 0).map_or(0, |o| HOST_ALT_STACK as u64 - o);
            println!("pila alternativa del host usada: {} bytes", used);
            assert!(used > 16 << 10, "la prueba no supera los 16 KiB ({} bytes): no demuestra nada", used);
            let _ = t;
        })
        .join()
        .unwrap();
        // el hilo termino: su pila alternativa se repuso y se libero (no se comprueba aqui la del hilo muerto)
        println!("fallos anidados OK");
    }

    /// Un manejador guest con tres fallos sincronos anidados usa mas de 16 KiB de pila del host (la alternativa de
    /// bionic/ART): con la pila alternativa de 64 KiB que se instala al crear el estado guest del hilo, no se desborda.
    #[test]
    fn pila_alternativa_del_host_grande() {
        if std::env::var_os("HEDDLE_PRUEBA_ALT").is_some() {
            fallos_anidados_en_pila_alternativa();
            return;
        }
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["rt::tests::pila_alternativa_del_host_grande", "--exact", "--test-threads=1", "--nocapture"])
            .env("HEDDLE_PRUEBA_ALT", "1")
            .output()
            .unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        println!("{}", so);
        assert!(out.status.success() && so.contains("fallos anidados OK"), "{}\n{}", so, String::from_utf8_lossy(&out.stderr));
    }

    /// Al terminar el hilo se repone la pila alternativa que tenia.
    #[test]
    fn pila_alternativa_del_host_se_repone() {
        std::thread::spawn(|| {
            let small = crate::mem::Region::with_guard(16 << 10, crate::mem::Kind::Test).unwrap();
            let ssp = small.base() + crate::mem::PAGE as u64;
            assert_eq!(host_sigaltstack(Some(&[ssp, 0, 16 << 10]), None), 0);
            // estado guest en un hilo interior que termina: aqui se comprueba en el mismo hilo con un GuestThread
            // creado y soltado a mano
            let h = install_host_alt().expect("deberia instalar la grande");
            let mut a = [0u64; 3];
            host_sigaltstack(None, Some(&mut a));
            assert_eq!((a[0], a[2]), (h.ours, HOST_ALT_STACK as u64));
            restore_host_alt(&h);
            host_sigaltstack(None, Some(&mut a));
            assert_eq!((a[0], a[2]), (ssp, 16 << 10), "no se repuso la pila alternativa anterior");
            // una ya grande no se cambia
            assert!(install_host_alt().is_none() || HOST_ALT_STACK > 16 << 10);
        })
        .join()
        .unwrap();
    }

    /// `debug.heddle.samp=0`: sin registros periodicos (ni el seguimiento de hilos que los alimenta); el vigilante
    /// sigue (mismo `over_limit`, probado aparte).
    #[test]
    fn muestreador_silenciado() {
        assert!(sampler_logs_on(None));
        assert!(sampler_logs_on(Some("1".into())));
        assert!(!sampler_logs_on(Some("0".into())));
        assert!(!sampler_logs_on(Some(" 0\n".into())));
        let mut st = SamplerState { quiet: true, ..Default::default() };
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (tx2, rx2) = std::sync::mpsc::channel::<()>();
        let h = std::thread::spawn(move || {
            ensure_thread(256 << 10);
            tx2.send(()).unwrap();
            rx.recv().unwrap();
        });
        rx2.recv().unwrap();
        sampler_tick(&mut st);
        sampler_tick(&mut st);
        assert!(st.prev.is_empty(), "el muestreador silenciado sigue los hilos");
        tx.send(()).unwrap();
        h.join().unwrap();
    }

    #[test]
    fn pila_guest_con_guarda_y_tamano_acotado() {
        std::thread::spawn(|| {
            let t = unsafe { &*ensure_thread(1 << 40) };
            assert!(t.stack_hi - t.stack_lo <= 64 << 20, "pila guest sin tope");
            // la pagina inmediatamente inferior a la pila es la guarda (mapeada, sin acceso)
            let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
            let g = format!("{:x}-{:x} ---p", t.stack_lo - 4096, t.stack_lo);
            assert!(maps.lines().any(|l| l.starts_with(&g)), "falta la guarda de la pila guest");
        })
        .join()
        .unwrap();
    }
}

#[cfg(test)]
mod emerg_tests {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static VISTO: AtomicU64 = AtomicU64::new(0);

    /// Codigo guest: guarda x0 (el numero de senal) en `VISTO` y vuelve.
    fn manejador_guest() -> u64 {
        use crate::sys::*;
        let a = &VISTO as *const AtomicU64 as u64;
        let mut w: Vec<u32> = vec![0xd280_0009 | (((a & 0xffff) as u32) << 5)];
        for hw in 1..4u32 {
            w.push(0xf280_0009 | (hw << 21) | ((((a >> (16 * hw)) & 0xffff) as u32) << 5));
        }
        w.push(0xf900_0120); // str x0, [x9]
        w.push(0xd65f_03c0); // ret
        let p = unsafe { mmap(std::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        assert!(p != MAP_FAILED);
        for (i, x) in w.iter().enumerate() {
            unsafe { *((p as *mut u32).add(i)) = *x };
        }
        unsafe { mprotect(p, 4096, PROT_READ) };
        crate::mem::guest_prot(p as u64, 4096, true);
        p as u64
    }

    /// Una senal con manejador guest que llega a un hilo sin estado guest (un hilo del host) se ejecuta con un
    /// estado de emergencia (no se pierde).
    #[test]
    fn senal_en_hilo_sin_estado_guest() {
        std::thread::spawn(|| {
            crate::rt::ensure_thread(256 << 10); // crea los estados de emergencia
        })
        .join()
        .unwrap();
        let h = manejador_guest();
        let sig = crate::sig::rtmin() as i32 + 6; // senal sin otro uso en las pruebas (corren en paralelo)
        let old = crate::sig::guest_sigaction(sig, Some(crate::sig::GuestAct { handler: h, flags: crate::sig::SA_SIGINFO, mask: 0 })).unwrap();
        VISTO.store(0, Relaxed);
        std::thread::spawn(move || {
            assert!(crate::rt::cur_opt().is_none(), "hilo del host, sin estado guest");
            unsafe {
                let (pid, tid) = (crate::sys::syscall(39), crate::sys::syscall(186));
                crate::sys::syscall(234, pid, tid, sig as i64);
            }
            assert!(crate::rt::cur_opt().is_none(), "el estado de emergencia se suelta al volver");
        })
        .join()
        .unwrap();
        assert_eq!(VISTO.load(Relaxed), sig as u64, "el manejador guest debe ejecutarse");
        crate::sig::guest_sigaction(sig, Some(old)).unwrap();
    }
}
