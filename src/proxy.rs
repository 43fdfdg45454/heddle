//! Proxys de los objetos del NDK con tabla de funciones del host: OpenSL ES y OpenMAX AL.
//!
//! Un objeto de estas API es un puntero a un puntero a una tabla de punteros a funcion (`SLPlayItf` =
//! `const struct SLPlayItf_ * const *`). Entregar al guest la interfaz real violaria la frontera (el guest saltaria a
//! codigo del host). El guest recibe en su lugar un PROXY: una entrada de `ENTS` cuya primera palabra apunta a una
//! tabla de slots HLE, uno por metodo, con la firma que declaran las cabeceras (`sigs::ITFS`, generada). Cada slot:
//!  1. desenvuelve el proxy a la interfaz real (y cualquier otra interfaz que llegue como argumento),
//!  2. convierte por TIPO lo que lo necesita: callbacks (trampolin con su firma; la interfaz que el host pasa como
//!     `caller` llega al guest como su proxy, 'O'), interfaces de salida (el guest recibe un proxy nuevo),
//!     SLDataSource/SLDataSink (su localizador OUTPUTMIX/IODEVICE lleva un objeto: se pasa una copia con el real),
//!  3. reparte los argumentos con los dos ABI reales (AAPCS64 -> SysV, tambien coma flotante y pila) y llama al
//!     metodo real con un `HostFn` validado.
//! Lo que el tipo no dice esta escrito a mano en `special` (GetInterface devuelve una interfaz segun el IID, Destroy
//! libera los proxys del objeto, `pAuxEffect` de SLEffectSendItf es una interfaz).
//!
//! Memoria: tabla fija de `CAP` entradas (128 KiB estaticos), tablas de metodos en un arreglo fijo y un slot HLE por
//! metodo (creados la primera vez que se usa la interfaz). `Destroy` libera las entradas del objeto; agotada la tabla,
//! la creacion responde SL_RESULT_MEMORY_FAILURE y se destruye el objeto real.
//! Senales: la creacion y la liberacion se serializan con `monitor::lock`; las busquedas (tambien desde el hilo de
//! audio del host, en cada callback) no toman bloqueos ni reservan memoria. Nunca se llama al guest ni al host con el
//! bloqueo tomado.

use crate::boundary::{self, HostFn};
use crate::cpu::Cpu;
use crate::hle::{self, Ret};
use crate::sigs::{A, ITFS};
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, AtomicUsize};
use std::sync::{Mutex, OnceLock};

// Codigos de resultado (iguales en OpenSL ES y OpenMAX AL).
const RESULT_PARAMETER_INVALID: u64 = 2;
const RESULT_MEMORY_FAILURE: u64 = 3;
const RESULT_FEATURE_UNSUPPORTED: u64 = 12;
const RESULT_INTERNAL_ERROR: u64 = 13;

/// Entradas de proxy como mucho (interfaces vivas que el guest ha recibido).
pub const CAP: usize = 4096;
/// Interfaces y metodos como mucho en la tabla generada (lo comprueba un test).
const MAX_ITFS: usize = 128;
const VT_CAP: usize = 1024;
/// Valor que el host nunca escribe en un parametro de salida (para saber si lo escribio).
const UNSET: u64 = 0x5EAD_5EAD_5EAD_5EAD;

/// Entrada de proxy. La direccion de `vtbl` es la interfaz que ve el guest; `vtbl` apunta a la tabla de slots.
#[repr(C, align(32))]
struct Ent {
    /// tabla guest de la interfaz (0 = entrada libre)
    vtbl: AtomicU64,
    /// interfaz real del host
    host: AtomicU64,
    /// indice en ITFS | entrada del objeto duenio << 16
    meta: AtomicU64,
}

static ENTS: [Ent; CAP] = [const { Ent { vtbl: AtomicU64::new(0), host: AtomicU64::new(0), meta: AtomicU64::new(0) } }; CAP];
/// entradas usadas alguna vez (las busquedas recorren solo hasta aqui)
static HIGH: AtomicUsize = AtomicUsize::new(0);
/// creacion y liberacion de entradas, y relleno de las tablas
static ALLOC: Mutex<()> = Mutex::new(());
/// tablas de metodos (slots HLE), una tras otra; `VT_READY[i]` = la de la interfaz i esta rellena
static VT: [AtomicU64; VT_CAP] = [const { AtomicU64::new(0) }; VT_CAP];
static VT_READY: [AtomicU8; MAX_ITFS] = [const { AtomicU8::new(0) }; MAX_ITFS];
static FULL_LOGS: AtomicU32 = AtomicU32::new(0);
static BAD_LOGS: AtomicU32 = AtomicU32::new(0);

fn log(m: &str) {
    #[cfg(target_os = "android")]
    crate::bridge::alog(m);
    #[cfg(not(target_os = "android"))]
    let _ = m;
}

/// Inicio de la tabla de cada interfaz en `VT`.
fn vt_off(i: usize) -> usize {
    static OFFS: OnceLock<[u32; MAX_ITFS + 1]> = OnceLock::new();
    OFFS.get_or_init(|| {
        let mut o = [0u32; MAX_ITFS + 1];
        for k in 0..ITFS.len().min(MAX_ITFS) {
            o[k + 1] = o[k] + ITFS[k].methods.len() as u32;
        }
        o
    })[i] as usize
}

fn base() -> u64 {
    ENTS.as_ptr() as u64
}

fn guest_addr(e: usize) -> u64 {
    base() + (e * std::mem::size_of::<Ent>()) as u64
}

/// Entrada de proxy viva de la interfaz guest `g` (None si `g` no es un proxy).
fn ent_of(g: u64) -> Option<usize> {
    let off = g.wrapping_sub(base()) as usize;
    let sz = std::mem::size_of::<Ent>();
    if off >= CAP * sz || off % sz != 0 {
        return None;
    }
    let e = off / sz;
    (ENTS[e].vtbl.load(Acquire) != 0).then_some(e)
}

/// Interfaz real del host detras del proxy `g`; None si `g` no es un proxy.
pub fn host_of(g: u64) -> Option<u64> {
    ent_of(g).map(|e| ENTS[e].host.load(Relaxed))
}

/// Proxy (interfaz guest) de la interfaz del host `h`; 0 si no tiene. Sin bloqueos ni reservas: la usan los
/// trampolines de los callbacks ('O' en la firma) en el hilo del host que los llama.
pub fn guest_itf(h: u64) -> u64 {
    if h == 0 {
        return 0;
    }
    let n = HIGH.load(Acquire).min(CAP);
    for e in 0..n {
        if ENTS[e].host.load(Relaxed) == h && ENTS[e].vtbl.load(Acquire) != 0 {
            return guest_addr(e);
        }
    }
    0
}

/// Semantica que el tipo no dice (escrita a mano).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Special {
    None,
    /// GetInterface(self, iid, void *pInterface): el host escribe una interfaz del tipo que dice el IID
    GetInterface,
    /// Destroy(self): tras destruir el objeto real se liberan los proxys de todas sus interfaces
    Destroy,
    /// SLEffectSendItf: `const void *pAuxEffect` (argumento 1) es la interfaz del efecto auxiliar
    AuxEffect,
}

fn special(itf: &str, m: &str) -> Special {
    match (itf, m) {
        ("SLObjectItf_" | "XAObjectItf_", "GetInterface") => Special::GetInterface,
        ("SLObjectItf_" | "XAObjectItf_", "Destroy") => Special::Destroy,
        ("SLEffectSendItf_", "EnableEffectSend" | "IsEnabled" | "SetSendLevel" | "GetSendLevel") => Special::AuxEffect,
        _ => Special::None,
    }
}

/// Tabla guest de la interfaz `it`; la rellena la primera vez (un slot HLE por metodo). Sin bloqueo propio: si dos
/// hilos la rellenan a la vez, registran los mismos nombres (el mismo slot) y escriben lo mismo.
fn vtable(it: usize) -> u64 {
    let off = vt_off(it);
    if VT_READY[it].load(Acquire) == 0 {
        let itf = &ITFS[it];
        for (mi, m) in itf.methods.iter().enumerate() {
            let sp = special(itf.name, m.name);
            let a = hle::register(
                &format!("{}::{}", itf.name, m.name),
                Box::new(move |c: &mut Cpu| {
                    call_method(c, it, mi, sp);
                    Ret::Return
                }),
            );
            VT[off + mi].store(a, Relaxed);
        }
        VT_READY[it].store(1, Release);
    }
    VT.as_ptr() as u64 + 8 * off as u64
}

/// Proxy de la interfaz del host `h` (tipo `it`) del objeto cuya entrada es `owner` (None: `h` es un objeto nuevo).
/// Si ya tiene proxy, el mismo. 0 si `h` es 0 o la tabla esta llena.
fn wrap(h: u64, it: usize, owner: Option<usize>) -> u64 {
    if h == 0 || it >= ITFS.len().min(MAX_ITFS) {
        return 0;
    }
    // la tabla de metodos se rellena antes del bloqueo (registrar slots escribe en logcat)
    let vt = vtable(it);
    let r = {
        let _g = crate::monitor::lock(&ALLOC);
        let hi = HIGH.load(Relaxed);
        if let Some(e) = (0..hi).find(|&e| ENTS[e].vtbl.load(Relaxed) != 0 && ENTS[e].host.load(Relaxed) == h) {
            Some(e)
        } else if let Some(e) = (0..CAP).find(|&e| ENTS[e].vtbl.load(Relaxed) == 0) {
            let ow = owner.unwrap_or(e);
            ENTS[e].host.store(h, Relaxed);
            ENTS[e].meta.store(it as u64 | (ow as u64) << 16, Relaxed);
            ENTS[e].vtbl.store(vt, Release);
            if e >= hi {
                HIGH.store(e + 1, Release);
            }
            Some(e)
        } else {
            None
        }
    };
    match r {
        Some(e) => guest_addr(e),
        None => {
            if FULL_LOGS.fetch_add(1, Relaxed) < 10 {
                log(&format!("proxy: tabla de interfaces llena ({} entradas): {} no se entrega", CAP, ITFS[it].name));
            }
            0
        }
    }
}

/// Libera los proxys del objeto cuya entrada es `obj` (el objeto y todas sus interfaces).
fn free_object(obj: usize) {
    let _g = crate::monitor::lock(&ALLOC);
    for e in 0..HIGH.load(Relaxed) {
        if ENTS[e].vtbl.load(Relaxed) != 0 && (ENTS[e].meta.load(Relaxed) >> 16) as usize == obj {
            ENTS[e].vtbl.store(0, Release);
            ENTS[e].host.store(0, Relaxed);
            ENTS[e].meta.store(0, Relaxed);
        }
    }
}

/// Interfaces vivas (diagnostico y pruebas).
pub fn live() -> usize {
    (0..HIGH.load(Acquire).min(CAP)).filter(|&e| ENTS[e].vtbl.load(Relaxed) != 0).count()
}

/// Tipo de interfaz que pide el IID `iid` (puntero a SLInterfaceID_/XAInterfaceID_, 16 bytes). Se compara el
/// contenido con los IID que exporta el host (`SL_IID_...`, `XA_IID_...`).
fn itf_of_iid(iid: u64) -> Option<usize> {
    static MAP: OnceLock<Vec<([u8; 16], u16)>> = OnceLock::new();
    if iid == 0 {
        return None;
    }
    let map = MAP.get_or_init(|| {
        let mut v = Vec::new();
        for (i, itf) in ITFS.iter().enumerate() {
            if itf.iid.is_empty() {
                continue;
            }
            let a = boundary::symbol_addr(itf.iid) as u64;
            if a == 0 || HostFn::from_host(a).is_some() {
                continue;
            }
            let p = unsafe { *(a as *const u64) };
            if p != 0 {
                v.push((unsafe { *(p as *const [u8; 16]) }, i as u16));
            }
        }
        v
    });
    let want = unsafe { *(iid as *const [u8; 16]) };
    map.iter().find(|(k, _)| *k == want).map(|(_, i)| *i as usize)
}

// ---------------------------------------------------------------------------------------------
// Reparto de argumentos por firma: AAPCS64 (guest) -> SysV x86-64 (host)
// ---------------------------------------------------------------------------------------------

/// Lee los argumentos de la llamada guest en curso en orden (AAPCS64 de Linux: 8 enteros en x0..x7, 8 flotantes en
/// v0..v7 y el resto en la pila, en orden, una palabra de 8 bytes por argumento).
pub struct GuestArgs<'a> {
    c: &'a Cpu,
    ni: usize,
    nf: usize,
    ns: usize,
}

impl<'a> GuestArgs<'a> {
    pub fn new(c: &'a Cpu) -> Self {
        GuestArgs { c, ni: 0, nf: 0, ns: 0 }
    }
    fn stack(&mut self) -> u64 {
        let sp = self.c.x[31];
        self.ns += 1;
        if sp == 0 {
            return 0;
        }
        unsafe { *((sp + 8 * (self.ns as u64 - 1)) as *const u64) }
    }
    pub fn int(&mut self) -> u64 {
        if self.ni < 8 {
            self.ni += 1;
            self.c.x[self.ni - 1]
        } else {
            self.stack()
        }
    }
    pub fn fp(&mut self) -> u64 {
        if self.nf < 8 {
            self.nf += 1;
            self.c.v[self.nf - 1][0]
        } else {
            self.stack()
        }
    }
}

/// Argumentos para el host (SysV x86-64: 6 enteros en registros, 8 flotantes en xmm0..7 y el resto en la pila, en
/// orden, una palabra por argumento).
pub struct HostArgs {
    pub ints: [u64; 6],
    pub fps: [u64; 8],
    pub stack: [u64; 24],
    ni: usize,
    nf: usize,
    pub ns: usize,
}

impl HostArgs {
    pub fn new() -> Self {
        HostArgs { ints: [0; 6], fps: [0; 8], stack: [0; 24], ni: 0, nf: 0, ns: 0 }
    }
    fn push_stack(&mut self, v: u64) {
        if self.ns < self.stack.len() {
            self.stack[self.ns] = v;
            self.ns += 1;
        }
    }
    pub fn int(&mut self, v: u64) {
        if self.ni < 6 {
            self.ints[self.ni] = v;
            self.ni += 1;
        } else {
            self.push_stack(v);
        }
    }
    pub fn fp(&mut self, v: u64) {
        if self.nf < 8 {
            self.fps[self.nf] = v;
            self.nf += 1;
        } else {
            self.push_stack(v);
        }
    }
    /// Un valor de la clase `c` (caracter de shorty): los enteros estrechos se extienden como espera el host.
    pub fn val(&mut self, c: u8, v: u64) {
        match c {
            b'F' => self.fp(v & 0xffff_ffff),
            b'D' => self.fp(v),
            _ => self.int(narrow(c, v)),
        }
    }
}

/// Extiende un entero de la clase `c` (Z B C S I) a 64 bits.
pub fn narrow(c: u8, v: u64) -> u64 {
    match c {
        b'Z' => v & 0xff,
        b'B' => v as i8 as i64 as u64,
        b'C' => v & 0xffff,
        b'S' => v as i16 as i64 as u64,
        b'I' => v as i32 as i64 as u64,
        _ => v,
    }
}

/// Copia del SLDataSource/SLDataSink `p` (y de su localizador si lleva un objeto) con el objeto real del host en
/// lugar del proxy. `src` y `loc` son el almacenamiento de la copia. Err si el objeto no es un proxy.
fn data_arg(p: u64, src: &mut [u64; 2], loc: &mut [u64; 3]) -> Result<u64, ()> {
    if p == 0 {
        return Ok(0);
    }
    unsafe {
        *src = *(p as *const [u64; 2]);
        let l = src[0];
        if l == 0 {
            return Ok(src.as_ptr() as u64);
        }
        // locatorType: OUTPUTMIX (4) {tipo; objeto @8}, IODEVICE (3) {tipo; deviceType; deviceID; objeto @16}
        let (words, at) = match *(l as *const u32) {
            4 => (2, 1),
            3 => (3, 2),
            _ => return Ok(p),
        };
        std::ptr::copy_nonoverlapping(l as *const u64, loc.as_mut_ptr(), words);
        let obj = loc[at];
        loc[at] = if obj == 0 { 0 } else { host_of(obj).ok_or(())? };
        src[0] = loc.as_ptr() as u64;
    }
    Ok(src.as_ptr() as u64)
}

/// Llama al metodo `mi` de la interfaz real con los argumentos del guest convertidos segun la firma generada.
fn call_method(c: &mut Cpu, it: usize, mi: usize, sp: Special) {
    let itf = &ITFS[it];
    let m = &itf.methods[mi];
    let set_ret = |c: &mut Cpu, r: u64| {
        if m.ret != b'V' {
            c.x[0] = r;
        }
    };
    let Some(e) = ent_of(c.x[0]) else {
        log(&format!("proxy: {}::{} sobre algo que no es un proxy ({:#x})", itf.name, m.name, c.x[0]));
        return set_ret(c, RESULT_PARAMETER_INVALID);
    };
    if !m.bad.is_empty() {
        if BAD_LOGS.fetch_add(1, Relaxed) < 50 {
            log(&format!("proxy: {}::{} no soportado ({})", itf.name, m.name, m.bad));
        }
        return set_ret(c, RESULT_FEATURE_UNSUPPORTED);
    }
    let host = ENTS[e].host.load(Relaxed);
    let obj = (ENTS[e].meta.load(Relaxed) >> 16) as usize;
    let fp = unsafe { *((*(host as *const u64)) as *const u64).add(mi) };
    let Some(f) = HostFn::from_host(fp) else {
        log(&format!("proxy: {}::{}: la tabla del host no apunta a codigo del host ({:#x})", itf.name, m.name, fp));
        return set_ret(c, RESULT_INTERNAL_ERROR);
    };
    let mut g = GuestArgs::new(c);
    let mut h = HostArgs::new();
    // almacenamiento de las conversiones (en la pila de este manejador)
    let mut outs = [(0u64, 0usize); 4];
    let mut out_tmp = [UNSET; 4];
    let out_ptr = out_tmp.as_mut_ptr();
    let mut nout = 0usize;
    let mut srcs = [[0u64; 2]; 8];
    let mut locs = [[0u64; 3]; 8];
    let mut ndata = 0usize;
    let mut iid = 0u64;
    let mut gi_out: Option<u64> = None;
    for (k, a) in m.args.iter().enumerate() {
        match *a {
            A::V(ch) => {
                let v = if ch == b'F' || ch == b'D' { g.fp() } else { g.int() };
                if sp == Special::GetInterface && k == 1 {
                    iid = v;
                }
                if sp == Special::GetInterface && k == 2 {
                    gi_out = Some(v);
                    h.int(if v == 0 { 0 } else { out_ptr as u64 });
                    continue;
                }
                if sp == Special::AuxEffect && k == 1 && v != 0 {
                    match host_of(v) {
                        Some(x) => h.int(x),
                        None => return set_ret(c, RESULT_PARAMETER_INVALID),
                    }
                    continue;
                }
                h.val(ch, v);
            }
            A::Itf(_) => {
                let v = g.int();
                if k == 0 {
                    h.int(host);
                } else if v == 0 {
                    h.int(0);
                } else {
                    match host_of(v) {
                        Some(x) => h.int(x),
                        None => return set_ret(c, RESULT_PARAMETER_INVALID),
                    }
                }
            }
            A::ItfOut(t) => {
                let p = g.int();
                if p == 0 || nout == outs.len() {
                    h.int(p);
                } else {
                    outs[nout] = (p, t as usize);
                    h.int(unsafe { out_ptr.add(nout) } as u64);
                    nout += 1;
                }
            }
            A::Cb(sig) => {
                let v = g.int();
                h.int(boundary::to_host_callable_sig(v, sig, 0));
            }
            A::Data => {
                let p = g.int();
                if ndata == srcs.len() {
                    return set_ret(c, RESULT_PARAMETER_INVALID);
                }
                match data_arg(p, &mut srcs[ndata], &mut locs[ndata]) {
                    Ok(x) => h.int(x),
                    Err(()) => return set_ret(c, RESULT_PARAMETER_INVALID),
                }
                ndata += 1;
            }
        }
    }
    if sp == Special::GetInterface {
        // GetInterface escribe en el temporal 0: que no lo use tambien un parametro de salida tipado
        debug_assert_eq!(nout, 0);
    }
    let (rax, _rdx, xmm0) = boundary::call_host_args(f, &h.ints, &h.fps, &h.stack[..h.ns]);
    let mut r = rax;
    // interfaces de salida: objetos nuevos (o uno que el guest ya tiene: Get3DGroup)
    for k in 0..nout {
        let (p, t) = outs[k];
        let v = unsafe { out_ptr.add(k).read_volatile() };
        if v == UNSET {
            continue;
        }
        let w = wrap(v, t, None);
        if v != 0 && w == 0 {
            destroy_host_object(v, t);
            r = RESULT_MEMORY_FAILURE;
        }
        unsafe { *(p as *mut u64) = w };
    }
    if let Some(p) = gi_out.filter(|&p| p != 0) {
        let v = unsafe { out_ptr.read_volatile() };
        if v != UNSET {
            let w = if v == 0 || r as u32 != 0 {
                0
            } else {
                match itf_of_iid(iid) {
                    Some(t) => {
                        let w = wrap(v, t, Some(obj));
                        if w == 0 {
                            r = RESULT_MEMORY_FAILURE;
                        }
                        w
                    }
                    None => {
                        log(&format!("proxy: GetInterface con un IID sin firma conocida en {}", itf.name));
                        r = RESULT_FEATURE_UNSUPPORTED;
                        0
                    }
                }
            };
            unsafe { *(p as *mut u64) = w };
        }
    }
    if sp == Special::Destroy {
        free_object(obj);
    }
    match m.ret {
        b'V' => {}
        b'F' => c.v[0] = [xmm0 & 0xffff_ffff, 0],
        b'D' => c.v[0] = [xmm0, 0],
        ch => c.x[0] = narrow(ch, r),
    }
}

/// Destruye un objeto real que no se pudo entregar al guest (tabla de proxys llena).
fn destroy_host_object(h: u64, it: usize) {
    let Some(mi) = ITFS[it].methods.iter().position(|m| m.name == "Destroy") else { return };
    let fp = unsafe { *((*(h as *const u64)) as *const u64).add(mi) };
    if let Some(f) = HostFn::from_host(fp) {
        boundary::call_host_args(f, &[h, 0, 0, 0, 0, 0], &[0; 8], &[]);
    }
}

/// slCreateEngine / xaCreateEngine (pEngine, numOptions, pOptions, numInterfaces, pIds, pRequired): el guest recibe
/// un proxy del objeto motor.
fn create_engine(c: &mut Cpu, name: &str, objitf: &str) {
    let (Some(f), Some(it)) = (HostFn::symbol(name), crate::sigs::itf(objitf)) else {
        c.x[0] = RESULT_FEATURE_UNSUPPORTED;
        return;
    };
    let out = c.x[0];
    let mut tmp = UNSET;
    let ints = [if out == 0 { 0 } else { &mut tmp as *mut u64 as u64 }, c.x[1] & 0xffff_ffff, c.x[2], c.x[3] & 0xffff_ffff, c.x[4], c.x[5]];
    let (mut r, _, _) = boundary::call_host_args(f, &ints, &[0; 8], &[]);
    if out != 0 && tmp != UNSET {
        let w = wrap(tmp, it, None);
        if tmp != 0 && w == 0 {
            destroy_host_object(tmp, it);
            r = RESULT_MEMORY_FAILURE;
        }
        unsafe { *(out as *mut u64) = w };
    }
    c.x[0] = r & 0xffff_ffff;
}

/// Registra los puntos de entrada de OpenSL ES / OpenMAX AL (el resto de sus funciones se reenvian por la tabla).
pub fn register() {
    hle::register(
        "slCreateEngine",
        Box::new(|c: &mut Cpu| {
            create_engine(c, "slCreateEngine", "SLObjectItf_");
            Ret::Return
        }),
    );
    hle::register(
        "xaCreateEngine",
        Box::new(|c: &mut Cpu| {
            create_engine(c, "xaCreateEngine", "XAObjectItf_");
            Ret::Return
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn la_tabla_generada_cabe_en_las_tablas_fijas() {
        assert!(ITFS.len() <= MAX_ITFS);
        assert!(ITFS.iter().map(|i| i.methods.len()).sum::<usize>() <= VT_CAP);
        assert_eq!(std::mem::size_of::<Ent>(), 32);
    }

    #[test]
    fn reparto_aapcs64_a_sysv_con_flotantes_y_pila() {
        // 9 enteros y 10 flotantes: en el guest sobran 1 entero y 2 flotantes (pila guest, en orden de argumento);
        // en el host sobran 3 enteros y 2 flotantes (pila host, en orden de argumento)
        let mut c = Cpu::new();
        let pila = [(8.5f64).to_bits(), (9.5f32).to_bits() as u64, 0x1111u64];
        c.x[31] = pila.as_ptr() as u64;
        for i in 0..8 {
            c.x[i] = 100 + i as u64;
            c.v[i][0] = (i as f64 + 0.25).to_bits();
        }
        c.x[2] = 0xffff_8001; // short -32767 con basura arriba (legal en AAPCS64)
        let mut g = GuestArgs::new(&c);
        let mut h = HostArgs::new();
        for &ch in b"IDJFSJJJJJDDDDDDDFJ" {
            let v = if ch == b'F' || ch == b'D' { g.fp() } else { g.int() };
            h.val(ch, v);
        }
        assert_eq!(h.ints, [100, 101, (-32767i64) as u64, 103, 104, 105]);
        assert_eq!(h.fps[0], (0.25f64).to_bits());
        assert_eq!(h.fps[1], (1.25f64).to_bits() & 0xffff_ffff);
        assert_eq!(h.fps[7], (7.25f64).to_bits());
        assert_eq!(&h.stack[..h.ns], &[106, 107, (8.5f64).to_bits(), (9.5f32).to_bits() as u64, 0x1111]);
    }

    #[test]
    fn proxys_envuelven_desenvuelven_y_se_liberan_con_el_objeto() {
        hle::init();
        let o = crate::sigs::itf("SLObjectItf_").unwrap();
        let p = crate::sigs::itf("SLPlayItf_").unwrap();
        static FALSO: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
        let (ho, hp) = (FALSO[0].as_ptr() as u64, FALSO[1].as_ptr() as u64);
        let go = wrap(ho, o, None);
        let e = ent_of(go).unwrap();
        let gp = wrap(hp, p, Some(e));
        assert!(go != 0 && gp != 0 && go != gp);
        assert_eq!(wrap(hp, p, Some(e)), gp, "la misma interfaz, el mismo proxy");
        assert_eq!((host_of(go), host_of(gp)), (Some(ho), Some(hp)));
        assert_eq!((guest_itf(ho), guest_itf(hp)), (go, gp));
        // la tabla que ve el guest son slots HLE con el nombre del metodo
        let vt = unsafe { *(gp as *const u64) };
        let s = unsafe { *(vt as *const u64) };
        assert!(hle::is_hle(s));
        assert_eq!(hle::name_at(s), "SLPlayItf_::SetPlayState");
        free_object(e);
        assert_eq!((host_of(go), host_of(gp), guest_itf(hp)), (None, None, 0));
        assert_eq!(host_of(12345), None);
    }

    #[test]
    fn data_source_lleva_el_objeto_real() {
        hle::init();
        let o = crate::sigs::itf("SLObjectItf_").unwrap();
        static FALSO: AtomicU64 = AtomicU64::new(0);
        let hm = FALSO.as_ptr() as u64;
        let gm = wrap(hm, o, None);
        let loc = [4u64, gm];
        let src = [loc.as_ptr() as u64, 0x77];
        let (mut s2, mut l2) = ([0u64; 2], [0u64; 3]);
        let r = data_arg(src.as_ptr() as u64, &mut s2, &mut l2).unwrap();
        let rs = unsafe { *(r as *const [u64; 2]) };
        assert_eq!(rs[1], 0x77);
        assert_eq!(unsafe { *(rs[0] as *const [u64; 2]) }, [4, hm]);
        assert_eq!(loc[1], gm, "el original del guest no se toca");
        // un objeto que no es un proxy: error
        let malo = [4u64, 0xdead_0000];
        let src2 = [malo.as_ptr() as u64, 0];
        assert!(data_arg(src2.as_ptr() as u64, &mut s2, &mut l2).is_err());
        // otro localizador: se pasa tal cual
        let otro = [0x800007BDu64, 2];
        let src3 = [otro.as_ptr() as u64, 0];
        assert_eq!(data_arg(src3.as_ptr() as u64, &mut s2, &mut l2), Ok(src3.as_ptr() as u64));
        free_object(ent_of(gm).unwrap());
    }
}
