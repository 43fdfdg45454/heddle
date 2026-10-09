//! libc/libm/libdl/liblog del guest servidas por HLE.
//!
//! Estrategia: los bionic arm64 y x86-64 comparten el layout LP64 de casi todo (pthread_*, FILE*, dirent,
//! sockaddr, timespec...), y AAPCS64/SysV asignan enteros y flotantes a bancos de registros independientes en
//! el mismo orden. Por eso la mayoria de funciones se reenvian con un thunk universal (x0..x7, v0..v7, pila).
//! Se implementan aparte las que necesitan conversion: stat, flags de open, epoll, sigaction, setjmp, printf,
//! callbacks (qsort, atexit, pthread_create...), dl*, TLS, datos, long double, etc.

use crate::cpu::Cpu;
use crate::fmt::{self, ArgSrc, RegArgs, VaList};
use crate::hle::{self, Handler, Ret};
pub use crate::boundary::to_host_callable;
use crate::boundary::HostFn;
use crate::rt;
use crate::sig::{self, GuestAct};
use crate::sys::*;
use crate::syscall;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_void};
use std::sync::{Mutex, Once};

extern "C" {
    fn fwrite(p: *const c_void, s: usize, n: usize, f: *mut c_void) -> usize;
    fn fflush(f: *mut c_void) -> i32;
    fn malloc(n: usize) -> *mut c_void;
    fn dup(fd: i32) -> i32;
}

// ---------------------------------------------------------------------------------------------
// utilidades
// ---------------------------------------------------------------------------------------------

pub fn set_errno(e: i32) {
    unsafe { *__errno_location() = e };
}

pub(crate) fn cstr(p: u64) -> String {
    if p == 0 {
        return "(null)".into();
    }
    unsafe { CStr::from_ptr(p as *const c_char).to_string_lossy().into_owned() }
}

fn cbytes<'a>(p: u64) -> &'a [u8] {
    unsafe { CStr::from_ptr(p as *const c_char).to_bytes() }
}

fn sysret(c: &mut Cpu, r: i64) {
    if r < 0 && r > -4096 {
        set_errno((-r) as i32);
        c.x[0] = u64::MAX;
    } else {
        c.x[0] = r as u64;
    }
}

fn reg<F: Fn(&mut Cpu) -> Ret + Send + Sync + 'static>(name: &str, f: F) {
    hle::register(name, Box::new(f));
}

pub(crate) fn reg_ret<F: Fn(&mut Cpu) + Send + Sync + 'static>(name: &str, f: F) {
    hle::register(
        name,
        Box::new(move |c| {
            f(c);
            Ret::Return
        }),
    );
}

fn alias(new: &str, old: &str) {
    if let Some(a) = hle::addr_of(old) {
        // mismo manejador: se registra una entrada que delega por nombre
        let old = old.to_string();
        let _ = a;
        reg(new, move |c| {
            let a = hle::addr_of(&old).unwrap();
            c.pc = a;
            hle::dispatch(c);
            Ret::Stay
        });
    }
}

/// Funcion exportada por una biblioteca permitida del host (None si no existe o no se permite).
fn host_sym(name: &str) -> Option<HostFn> {
    HostFn::symbol(name)
}

/// Llamada al host con argumentos explicitos; (rax, xmm0). Sin simbolo no llama y devuelve ceros.
fn host_call(f: Option<HostFn>, ints: &[u64], fps: &[u64]) -> (u64, u64) {
    crate::boundary::call_raw(f, ints, fps)
}

/// Reenvia la llamada guest en curso a la funcion del host `f` (ver boundary::call_host).
pub fn forward(c: &mut Cpu, f: HostFn, arg0: Option<u64>) {
    crate::boundary::call_host(c, f, arg0)
}

// ---------------------------------------------------------------------------------------------
// resolucion de simbolos
// ---------------------------------------------------------------------------------------------

static INIT: Once = Once::new();
static DATA: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Funciones del host que reciben punteros a funcion (el guest pasaria direcciones guest) o estructuras
/// incompatibles y que no se han implementado: se tratan como no disponibles en lugar de fallar al azar.
const DENY: &[&str] = &[
    "scandir", "scandir64", "ftw", "nftw", "ftw64", "nftw64", "tsearch", "tfind", "tdelete", "twalk", "tdestroy", "lfind", "lsearch",
    "on_exit", "getcontext", "setcontext", "makecontext", "swapcontext", "aio_read", "aio_write", "lio_listio", "fts_open",
    "fts_read", "backtrace", "_Unwind_Backtrace", "pthread_atfork",
    // crean un hilo/proceso que ejecuta un puntero a funcion guest sobre una pila dada, o reciben una estructura de
    // punteros a funcion por valor: no hay forma segura de reenviarlas
    "clone", "__clone", "__bionic_clone", "fopencookie",
    // reciben callbacks guest y no estan implementadas: mejor "no disponible" (con mensaje) que SIGSEGV en el host
    "thrd_create", "call_once", "eglSetBlobCacheFuncsANDROID", "wordexp", "tss_create",
];


pub fn resolve(name: &str) -> Option<u64> {
    INIT.call_once(register_all);
    // Funciones que no se pueden reenviar (long double, complejos) y que la biblioteca guest incrustada implementa:
    // el guest las llama como codigo ARM, sin frontera. Tiene prioridad sobre las aproximaciones en double.
    if matches!(crate::sigs::lookup(name).map(|s| s.k), Some(crate::sigs::K::Unsafe(_))) {
        if let Some(a) = crate::guestlib::lookup(name) {
            return Some(a);
        }
    }
    if let Some(a) = hle::addr_of(name) {
        return Some(a);
    }
    if let Some(d) = crate::monitor::lock(&DATA).as_ref().and_then(|m| m.get(name).copied()) {
        return Some(d);
    }
    if DENY.contains(&name) {
        return None;
    }
    // Dato exportado por el NDK (stdin, environ, __sF, SL_IID_ENGINE...): el guest lo lee directamente (mismo layout
    // LP64). Los identificadores de interfaz de OpenSL/OpenMAX son los del host: el proxy los compara por contenido.
    if crate::sigs::is_data(name) {
        let p = crate::boundary::symbol_addr(name);
        if p != 0 && HostFn::from_host(p as u64).is_none() {
            return Some(p as u64);
        }
        // Sin OpenSL/OpenMAX en el host: una celda propia por nombre para que el enlace del modulo no falle
        // (slCreateEngine responde entonces "no soportado").
        if name.starts_with("SL_IID_") || name.starts_with("XA_IID_") {
            let blob = intern_cstr(&format!("iid:{:<15}", name));
            data_cell(name, &blob.to_le_bytes());
            return crate::monitor::lock(&DATA).as_ref().and_then(|m| m.get(name).copied());
        }
        return None;
    }
    // Funcion: solo se reenvia si su firma esta en la tabla generada y es directa. Las conversiones salen del tipo.
    use crate::sigs::K;
    let no = |why: &str| {
        dbg_log(&format!("simbolo '{}' no disponible: {}", name, why));
        None
    };
    match crate::sigs::lookup(name).map(|s| s.k) {
        None => no("no es una funcion de la API en C del NDK"),
        Some(K::NoSig) => no("el NDK la exporta pero no hay firma (anadirla a tools/sigtool/extra.h)"),
        Some(K::Unsafe(r)) => no(&format!("ABI incompatible con el reenvio ({}) y sin implementacion propia", r)),
        Some(K::Manual(r)) => no(&format!("necesita implementacion a mano ({})", r)),
        Some(K::Direct { .. }) => {
            let Some(f) = HostFn::symbol(name) else { dbg_log(&format!("simbolo '{}' no disponible: el host no lo exporta ({:#x})", name, crate::boundary::symbol_addr(name))); return None };
            // semantica que el tipo no dice (registrador de liblog...): proxy escrito a mano, solo si el host la tiene
            if let Some(a) = crate::ndkcb::special_slot(name, f) {
                return Some(a);
            }
            // conversiones por tipo; las estructuras de callbacks solo donde las listas escritas a mano lo dicen
            // (`sigs::STRUCT_COPIED`, `sigs::STRUCT_INPLACE`, ver boundary::Conv::of)
            let conv = crate::boundary::Conv::of(name).unwrap_or(crate::boundary::Conv::NONE);
            Some(crate::boundary::typed_slot(name, f, conv))
        }
    }
}

pub fn missing(name: &str) -> u64 {
    let n = name.to_string();
    hle::register(
        &format!("missing:{}", name),
        Box::new(move |c: &mut Cpu| {
            crate::bridge::alog_fatal(&format!("el guest llamo a una funcion no disponible: {}\n{}", n, rt::dump_backtrace(c)));
            unsafe { abort() }
        }),
    )
}

/// Ruta lenta del resolutor TLSDESC dinamico (`tls::resolver`): x0 = TlsDynamicResolverArg.
pub fn tlsdesc_slow_path() -> u64 {
    INIT.call_once(register_all);
    hle::addr_of("__heddle_tlsdesc_slow").unwrap()
}

/// Resolutor TLSDESC de un simbolo TLS debil sin definir (como `tlsdesc_resolver_unresolved_weak` de bionic): la
/// direccion resultante es el sumando (NULL sin sumando).
pub fn tlsdesc_resolver_weak() -> u64 {
    INIT.call_once(register_all);
    hle::addr_of("__heddle_tlsdesc_weak").unwrap()
}

/// Cadena C interna (una sola copia por contenido): dladdr/dl_iterate_phdr/dlerror se llaman muchas veces
/// (el GC de Unity en cada coleccion) y devolvian una copia nueva filtrada en cada llamada.
pub(crate) fn intern_cstr(s: &str) -> u64 {
    static M: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
    let mut g = crate::monitor::lock(&M);
    let m = g.get_or_insert_with(HashMap::new);
    if let Some(&a) = m.get(s) {
        return a;
    }
    if m.len() >= crate::mem::TABLE_CAP {
        // tabla llena: no se crece; se devuelve una cadena fija
        return b"(heddle: tabla de cadenas llena)\0".as_ptr() as u64;
    }
    let b = Box::leak(CString::new(s.replace('\0', "")).unwrap().into_boxed_c_str());
    let a = b.as_ptr() as u64;
    m.insert(s.to_string(), a);
    a
}

fn data_cell(name: &str, bytes: &[u8]) {
    let b: &'static mut [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
    crate::monitor::lock(&DATA).get_or_insert_with(HashMap::new).insert(name.to_string(), b.as_ptr() as u64);
}

// ---------------------------------------------------------------------------------------------
// atexit / destructores / claves TLS
// ---------------------------------------------------------------------------------------------

struct AtExit {
    f: u64,
    arg: u64,
    dso: u64,
    noarg: bool,
}

static ATEXIT: Mutex<Vec<AtExit>> = Mutex::new(Vec::new());
static KEYS: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

thread_local! {
    /// destructores de thread_local: (funcion, argumento, dso_handle)
    static THREAD_ATEXIT: RefCell<Vec<(u64, u64, u64)>> = RefCell::new(Vec::new());
    /// pila alternativa de senales que el guest cree tener (ss_sp, ss_flags, ss_size); por defecto SS_DISABLE
    static ALTSTACK: std::cell::Cell<(u64, u64, u64)> = const { std::cell::Cell::new((0, 2, 0)) };
}

/// Pila alternativa de senales declarada por el guest en este hilo (ss_sp, ss_flags, ss_size).
pub fn altstack() -> (u64, u64, u64) {
    ALTSTACK.try_with(|a| a.get()).unwrap_or((0, 2, 0))
}

fn run_key_dtors_only() {
    for _ in 0..4 {
        let keys: Vec<(u32, u64)> = crate::monitor::lock(&KEYS).clone();
        let mut any = false;
        for (k, d) in keys {
            if d == 0 {
                continue;
            }
            let v = unsafe { pthread_getspecific(k) };
            if !v.is_null() {
                unsafe { pthread_setspecific(k, std::ptr::null()) };
                rt::call_guest(d, &[v as u64], &[]);
                any = true;
            }
        }
        if !any {
            break;
        }
    }
}

fn run_thread_atexit() {
    loop {
        let e = THREAD_ATEXIT.with(|t| t.borrow_mut().pop());
        match e {
            Some((f, a, dso)) => {
                rt::call_guest(f, &[a], &[]);
                crate::elf::dso_unref(dso);
            }
            None => break,
        }
    }
}

/// Lo que hace `pthread_exit` de bionic antes de terminar el hilo (tambien cuando la funcion del hilo vuelve: bionic
/// llama a `pthread_exit` con su resultado), en su orden: destructores de `thread_local` (`__cxa_thread_finalize`), los
/// manejadores de `pthread_cleanup_push` del hilo del mas reciente al mas antiguo (cada uno se desenlaza antes de
/// ejecutarse) y los destructores de las claves (`pthread_key_clean_all`, hasta 4 pasadas).
pub fn pthread_exit_dtors() {
    run_thread_atexit();
    loop {
        let cl = CLEANUP.with(|h| h.get());
        if cl == 0 {
            break;
        }
        let (prev, f, a) = unsafe { (*(cl as *const u64), *((cl + 8) as *const u64), *((cl + 16) as *const u64)) };
        CLEANUP.with(|h| h.set(prev));
        rt::call_guest(f, &[a], &[]);
    }
    run_key_dtors_only();
}

pub unsafe fn thread_exit_now(code: u64) -> ! {
    pthread_exit_dtors();
    pthread_exit(code as *mut c_void)
}

/// Salida del hilo por la llamada al sistema `exit` del guest (`svc`, no `pthread_exit` ni el retorno de la funcion del
/// hilo): en ARM el nucleo termina el hilo sin ejecutar nada de bionic ni del guest. Aqui tampoco: ni destructores de
/// `thread_local` (la referencia al DSO de cada uno queda tomada, como en bionic), ni la cadena de
/// `pthread_cleanup_push`, ni destructores de claves; lo pendiente se descarta. Solo se libera el estado del puente
/// (pila, TLS, JIT, pila de senales: `GuestThread`), con los destructores de TLS del host, que no llaman al guest (las
/// claves del guest no tienen destructor en el host, ver `pthread_key_create`). `pthread_join` devuelve NULL, como en
/// bionic (el valor de retorno del hilo no se escribe). El hilo principal sale con la llamada al sistema directa:
/// el nucleo hace lo mismo que en ARM (el proceso sigue si quedan hilos; si era el ultimo, termina con `code`).
pub unsafe fn thread_exit_syscall(code: u64) -> ! {
    let _ = THREAD_ATEXIT.try_with(|t| t.borrow_mut().clear());
    let _ = CLEANUP.try_with(|h| h.set(0));
    if syscall(186) == getpid() as std::os::raw::c_long {
        // sin destructores de TLS del host que lo suelten: el estado guest se libera aqui (salvo dentro de un manejador
        // de senal guest, que corre sobre esa memoria)
        rt::release_current();
        syscall(60, code as std::os::raw::c_long);
        abort()
    }
    pthread_exit(std::ptr::null_mut())
}

fn run_atexit(dso: Option<u64>, c: &mut Cpu) {
    loop {
        let e = {
            let mut g = crate::monitor::lock(&ATEXIT);
            let pos = g.iter().rposition(|e| dso.map_or(true, |d| d == 0 || e.dso == d));
            pos.map(|p| g.remove(p))
        };
        match e {
            Some(e) => {
                if e.noarg {
                    rt::call_guest_on(c, e.f, &[], &[]);
                } else {
                    rt::call_guest_on(c, e.f, &[e.arg], &[]);
                }
            }
            None => break,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// qsort / bsearch (algoritmo BSD, igual que bionic, para que el orden de elementos iguales coincida)
// ---------------------------------------------------------------------------------------------
//
// `Sorter::sort` es una traduccion a Rust del qsort.c de FreeBSD que usa bionic
// (libc/upstream-freebsd/lib/libc/stdlib/qsort.c, "@(#)qsort.c 8.1 (Berkeley) 6/4/93"). Se conserva su aviso:
//
// SPDX-License-Identifier: BSD-3-Clause
//
// Copyright (c) 1992, 1993
//      The Regents of the University of California.  All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
// 1. Redistributions of source code must retain the above copyright
//    notice, this list of conditions and the following disclaimer.
// 2. Redistributions in binary form must reproduce the above copyright
//    notice, this list of conditions and the following disclaimer in the
//    documentation and/or other materials provided with the distribution.
// 3. Neither the name of the University nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE REGENTS AND CONTRIBUTORS ``AS IS'' AND
// ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
// ARE DISCLAIMED.  IN NO EVENT SHALL THE REGENTS OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
// OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
// HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
// LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
// OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
// SUCH DAMAGE.

struct Sorter<'a> {
    c: &'a mut Cpu,
    cmp: u64,
    arg: Option<u64>,
    es: usize,
}

impl<'a> Sorter<'a> {
    fn cmp(&mut self, a: usize, b: usize) -> i32 {
        let r = match self.arg {
            Some(x) => rt::call_guest_on(self.c, self.cmp, &[a as u64, b as u64, x], &[]),
            None => rt::call_guest_on(self.c, self.cmp, &[a as u64, b as u64], &[]),
        };
        r.0 as i32
    }
    fn swap(&self, a: usize, b: usize) {
        if a != b {
            unsafe { std::ptr::swap_nonoverlapping(a as *mut u8, b as *mut u8, self.es) };
        }
    }
    fn vecswap(&self, mut a: usize, mut b: usize, n: usize) {
        let mut i = n;
        while i > 0 {
            self.swap(a, b);
            a += self.es;
            b += self.es;
            i -= self.es;
        }
    }
    fn med3(&mut self, a: usize, b: usize, c: usize) -> usize {
        if self.cmp(a, b) < 0 {
            if self.cmp(b, c) < 0 {
                b
            } else if self.cmp(a, c) < 0 {
                c
            } else {
                a
            }
        } else if self.cmp(b, c) > 0 {
            b
        } else if self.cmp(a, c) < 0 {
            a
        } else {
            c
        }
    }
    fn insertion(&mut self, a: usize, n: usize) {
        let es = self.es;
        let mut pm = a + es;
        while pm < a + n * es {
            let mut pl = pm;
            while pl > a && self.cmp(pl - es, pl) > 0 {
                self.swap(pl, pl - es);
                pl -= es;
            }
            pm += es;
        }
    }
    fn sort(&mut self, mut a: usize, mut n: usize) {
        let es = self.es;
        loop {
            let mut swap_cnt = false;
            if n < 7 {
                self.insertion(a, n);
                return;
            }
            let mut pm = a + (n / 2) * es;
            if n > 7 {
                let mut pl = a;
                let mut pn = a + (n - 1) * es;
                if n > 40 {
                    let d = (n / 8) * es;
                    pl = self.med3(pl, pl + d, pl + 2 * d);
                    pm = self.med3(pm - d, pm, pm + d);
                    pn = self.med3(pn - 2 * d, pn - d, pn);
                }
                pm = self.med3(pl, pm, pn);
            }
            self.swap(a, pm);
            let mut pa = a + es;
            let mut pb = pa;
            let mut pc = a + (n - 1) * es;
            let mut pd = pc;
            loop {
                while pb <= pc {
                    let r = self.cmp(pb, a);
                    if r > 0 {
                        break;
                    }
                    if r == 0 {
                        swap_cnt = true;
                        self.swap(pa, pb);
                        pa += es;
                    }
                    pb += es;
                }
                while pb <= pc {
                    let r = self.cmp(pc, a);
                    if r < 0 {
                        break;
                    }
                    if r == 0 {
                        swap_cnt = true;
                        self.swap(pc, pd);
                        pd -= es;
                    }
                    pc -= es;
                }
                if pb > pc {
                    break;
                }
                self.swap(pb, pc);
                swap_cnt = true;
                pb += es;
                pc -= es;
            }
            if !swap_cnt {
                self.insertion(a, n);
                return;
            }
            let pn = a + n * es;
            let r = (pa - a).min(pb - pa);
            self.vecswap(a, pb - r, r);
            let r = (pd - pc).min(pn - pd - es);
            self.vecswap(pb, pn - r, r);
            let r = pb - pa;
            if r > es {
                self.sort(a, r / es);
            }
            let r = pd - pc;
            if r > es {
                a = pn - r;
                n = r / es;
                continue;
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// printf
// ---------------------------------------------------------------------------------------------

fn host_stdout() -> *mut c_void {
    let p = crate::boundary::symbol_addr("stdout");
    if p == 0 {
        std::ptr::null_mut()
    } else {
        unsafe { *(p as *const *mut c_void) }
    }
}

fn sink_buf(c: &mut Cpu, buf: u64, n: u64, out: &[u8]) {
    if n > 0 && buf != 0 {
        let k = out.len().min(n as usize - 1);
        unsafe {
            std::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, k);
            *((buf + k as u64) as *mut u8) = 0;
        }
    }
    c.x[0] = out.len() as u64;
}

fn android_log(prio: i32, tag: &str, msg: &str) {
    let f = host_sym("__android_log_write");
    if f.is_some() {
        let t = CString::new(tag).unwrap_or_default();
        let m = CString::new(msg.replace('\0', "")).unwrap_or_default();
        host_call(f, &[prio as u64, t.as_ptr() as u64, m.as_ptr() as u64], &[]);
    } else {
        let l = ['?', '?', 'V', 'D', 'I', 'W', 'E', 'F', 'S'];
        eprintln!("{}/{}: {}", l.get(prio as usize).copied().unwrap_or('?'), tag, msg);
    }
}

fn reg_printf() {
    // (nombre, argumentos nombrados, tipo de salida) ; va_list: el ultimo argumento es el puntero a va_list
    for (name, named, kind, va) in [
        ("printf", 1usize, 'o', false),
        ("vprintf", 1, 'o', true),
        ("sprintf", 2, 's', false),
        ("vsprintf", 2, 's', true),
        ("snprintf", 3, 'n', false),
        ("vsnprintf", 3, 'n', true),
        ("fprintf", 2, 'f', false),
        ("vfprintf", 2, 'f', true),
        ("dprintf", 2, 'd', false),
        ("vdprintf", 2, 'd', true),
        ("asprintf", 2, 'a', false),
        ("vasprintf", 2, 'a', true),
        ("__sprintf_chk", 4, 's', false),
        ("__vsprintf_chk", 4, 's', true),
        ("__snprintf_chk", 5, 'n', false),
        ("__vsnprintf_chk", 5, 'n', true),
        ("__fprintf_chk", 3, 'f', false),
        ("__vfprintf_chk", 3, 'f', true),
        ("__printf_chk", 2, 'o', false),
        ("__vprintf_chk", 2, 'o', true),
        ("__android_log_print", 3, 'l', false),
        ("__android_log_vprint", 3, 'l', true),
        ("syslog", 2, 'y', false),
        ("vsyslog", 2, 'y', true),
    ] {
        reg_ret(name, move |c| {
            // argumentos nombrados segun la variante
            let x = c.x;
            let (fmt_idx, buf, size, file, fd, prio_tag) = match (kind, name.starts_with("__") && name.contains("_chk")) {
                ('s', true) => (3, x[0], u64::MAX, 0, 0, (0, 0)),
                ('n', true) => (4, x[0], x[1], 0, 0, (0, 0)),
                ('f', true) => (2, 0, 0, x[0], 0, (0, 0)),
                ('o', true) => (1, 0, 0, 0, 0, (0, 0)),
                ('s', false) => (1, x[0], u64::MAX, 0, 0, (0, 0)),
                ('n', false) => (2, x[0], x[1], 0, 0, (0, 0)),
                ('f', false) => (1, 0, 0, x[0], 0, (0, 0)),
                ('d', _) => (1, 0, 0, 0, x[0] as i32, (0, 0)),
                ('a', _) => (1, x[0], 0, 0, 0, (0, 0)),
                ('l', _) => (2, 0, 0, 0, 0, (x[0], x[1])),
                ('y', _) => (1, 0, 0, 0, 0, (x[0], 0)),
                _ => (0, 0, 0, 0, 0, (0, 0)),
            };
            let fmtp = x[fmt_idx];
            let out = if va {
                let mut a = VaList::new(x[named]);
                fmt::format(c, cbytes(fmtp), &mut a)
            } else {
                let mut a = RegArgs::new(c, named);
                fmt::format(c, cbytes(fmtp), &mut a)
            };
            match kind {
                's' => {
                    if buf != 0 {
                        unsafe {
                            std::ptr::copy_nonoverlapping(out.as_ptr(), buf as *mut u8, out.len());
                            *((buf + out.len() as u64) as *mut u8) = 0;
                        }
                    }
                    c.x[0] = out.len() as u64;
                }
                'n' => sink_buf(c, buf, size, &out),
                'o' => {
                    let so = host_stdout();
                    unsafe { fwrite(out.as_ptr() as *const c_void, 1, out.len(), so) };
                    c.x[0] = out.len() as u64;
                }
                'f' => {
                    unsafe { fwrite(out.as_ptr() as *const c_void, 1, out.len(), file as *mut c_void) };
                    c.x[0] = out.len() as u64;
                }
                'd' => {
                    unsafe { write(fd, out.as_ptr() as *const c_void, out.len()) };
                    c.x[0] = out.len() as u64;
                }
                'a' => {
                    let p = unsafe { malloc(out.len() + 1) } as *mut u8;
                    if p.is_null() {
                        c.x[0] = u64::MAX;
                    } else {
                        unsafe {
                            std::ptr::copy_nonoverlapping(out.as_ptr(), p, out.len());
                            *p.add(out.len()) = 0;
                            *(buf as *mut *mut u8) = p;
                        }
                        c.x[0] = out.len() as u64;
                    }
                }
                'l' => {
                    android_log(prio_tag.0 as i32, &cstr(prio_tag.1), &String::from_utf8_lossy(&out));
                    c.x[0] = out.len() as u64;
                }
                _ => {
                    android_log(4, "syslog", &String::from_utf8_lossy(&out));
                    c.x[0] = 0;
                }
            }
        });
    }
}

/// sigprocmask del guest con punteros guest (`set`/`old` pueden ser nulos). Devuelve 0 o -errno.
/// `raw`: llamada directa al sistema (sin los filtros de libsigchain y bionic).
pub fn guest_mask_call(how: u64, set: u64, old: u64, raw: bool) -> i64 {
    let s = if set != 0 { Some(unsafe { *(set as *const u64) }) } else { None };
    let mut o = 0u64;
    let op = if raw { sig::guest_rt_sigprocmask } else { sig::guest_sigprocmask };
    let r = op(how, s, if old != 0 { Some(&mut o) } else { None });
    if r == 0 && old != 0 {
        unsafe { *(old as *mut u64) = o };
    }
    r
}

// ---------------------------------------------------------------------------------------------
// setjmp / longjmp  (formato propio: 256 bytes)
// ---------------------------------------------------------------------------------------------

const JB_MAGIC: u64 = 0x61726d6a_6d702100;

fn do_setjmp(c: &mut Cpu, env: u64, save_mask: bool) {
    let p = env as *mut u64;
    unsafe {
        *p = JB_MAGIC;
        for i in 0..10 {
            *p.add(1 + i) = c.x[19 + i];
        }
        *p.add(11) = c.x[29];
        *p.add(12) = c.x[30];
        *p.add(13) = c.x[31];
        for i in 0..8 {
            *p.add(14 + i) = c.v[8 + i][0];
        }
        *p.add(23) = save_mask as u64;
        if save_mask {
            // la mascara que ve el guest (con las reservadas que cree bloqueadas, ver sig::guest_sigprocmask)
            let mut m = 0u64;
            sig::guest_sigprocmask(0, None, Some(&mut m));
            *p.add(22) = m;
        }
    }
    c.x[0] = 0;
}

fn do_longjmp(c: &mut Cpu, env: u64, val: u64) {
    let p = env as *const u64;
    unsafe {
        if *p != JB_MAGIC {
            crate::bridge::alog_fatal(&format!("longjmp con jmp_buf invalido\n{}", rt::dump_backtrace(c)));
            abort();
        }
        for i in 0..10 {
            c.x[19 + i] = *p.add(1 + i);
        }
        c.x[29] = *p.add(11);
        c.x[30] = *p.add(12);
        c.x[31] = *p.add(13);
        for i in 0..8 {
            c.v[8 + i] = [*p.add(14 + i), 0];
        }
        if *p.add(23) != 0 {
            sig::guest_sigprocmask(2, Some(*p.add(22)), None);
        }
    }
    let v = val as u32;
    c.x[0] = if v == 0 { 1 } else { v as u64 };
    c.pc = c.x[30];
    // un siglongjmp que sale de un manejador de senal termina ese manejador (sig::note_longjmp)
    sig::note_longjmp(c);
}

/// Registro de diagnostico (logcat en Android). Los dl* del guest estan en `dl.rs`.
pub(crate) fn dbg_log(m: &str) {
    #[cfg(target_os = "android")]
    crate::bridge::alog(m);
    #[cfg(not(target_os = "android"))]
    let _ = m;
}

/// verr/verrx/vwarn/vwarnx: se formatea en el guest y se delega en err/errx/warn/warnx del host.
fn err_va(c: &mut Cpu, host_name: &str, has_code: bool) {
    let f = host_sym(host_name);
    let (code, fmt, ap) = if has_code { (c.x[0], c.x[1], c.x[2]) } else { (0, c.x[0], c.x[1]) };
    let text = if fmt == 0 {
        Vec::new()
    } else {
        let fb = unsafe { std::ffi::CStr::from_ptr(fmt as *const std::os::raw::c_char).to_bytes().to_vec() };
        crate::fmt::format(c, &fb, &mut crate::fmt::VaList::new(ap))
    };
    let mut z = text;
    z.push(0);
    if f.is_some() {
        let pct_s = b"%s\0";
        let ints: Vec<u64> = if has_code { vec![code, pct_s.as_ptr() as u64, z.as_ptr() as u64] } else { vec![pct_s.as_ptr() as u64, z.as_ptr() as u64] };
        host_call(f, &ints, &[]);
    }
}

fn reg_valist_misc() {
    // scanf y wprintf: el host sirve todo salvo las conversiones %L (long double binary128), que van a la
    // biblioteca guest (ldio.rs); las de va_list reciben aqui un va_list SysV
    use crate::ldio::{ScanKind, WPrintKind};
    for (name, wide, src, va) in [
        ("sscanf", false, 's', false),
        ("fscanf", false, 'f', false),
        ("scanf", false, 'i', false),
        ("vsscanf", false, 's', true),
        ("vfscanf", false, 'f', true),
        ("vscanf", false, 'i', true),
        ("swscanf", true, 's', false),
        ("fwscanf", true, 'f', false),
        ("wscanf", true, 'i', false),
        ("vswscanf", true, 's', true),
        ("vfwscanf", true, 'f', true),
        ("vwscanf", true, 'i', true),
    ] {
        let host = crate::ldio::HostCache::new();
        reg_ret(name, move |c| crate::ldio::scanf(c, ScanKind { name, wide, src, va }, &host));
    }
    for (name, dst, va) in
        [("wprintf", 'o', false), ("fwprintf", 'f', false), ("swprintf", 's', false), ("vwprintf", 'o', true), ("vfwprintf", 'f', true), ("vswprintf", 's', true)]
    {
        let host = crate::ldio::HostCache::new();
        reg_ret(name, move |c| crate::ldio::wprintf(c, WPrintKind { name, dst, va }, &host));
    }
    reg_ret("verr", |c| err_va(c, "err", true));
    reg_ret("verrx", |c| err_va(c, "errx", true));
    reg_ret("vwarn", |c| err_va(c, "warn", false));
    reg_ret("vwarnx", |c| err_va(c, "warnx", false));
}

/// JNI_GetCreatedJavaVMs devuelve JavaVM* del host; el guest debe recibir el envoltorio guest.
fn reg_jni_vm() {
    // jniRegisterNativeMethods(env, className, JNINativeMethod*, n): los punteros a funcion son guest
    reg_ret("jniRegisterNativeMethods", |c| {
        let Some(f) = host_sym("jniRegisterNativeMethods") else {
            c.x[0] = -1i64 as u64;
            return;
        };
        let (env, cls, methods, n) = (crate::jni::unwrap_to_host(c.x[0]), c.x[1], c.x[2], c.x[3] as i32);
        let mut copy: Vec<[u64; 3]> = Vec::new();
        for i in 0..n.max(0) as u64 {
            let p = (methods + 24 * i) as *const u64;
            let (name, sig, fp) = unsafe { (*p, *p.add(1), *p.add(2)) };
            let ret = cbytes(sig).iter().rposition(|&b| b == b')').and_then(|k| cbytes(sig).get(k + 1).copied()).unwrap_or(b'V');
            copy.push([name, sig, to_host_callable(fp, ret, 1)]);
        }
        c.x[0] = host_call(Some(f), &[env, cls, copy.as_ptr() as u64, n as u64], &[]).0;
    });
    reg_ret("JNI_GetCreatedJavaVMs", |c| {
        let Some(p) = host_sym("JNI_GetCreatedJavaVMs") else {
            c.x[0] = -1i64 as u64; // JNI_ERR
            return;
        };
        let (vms, cap, nout) = (c.x[0], c.x[1] as i64, c.x[2]);
        forward(c, p, None);
        if c.x[0] as i32 == 0 && vms != 0 && nout != 0 {
            let n = unsafe { *(nout as *const i32) } as i64;
            for i in 0..n.min(cap).max(0) as u64 {
                unsafe {
                    let slot = (vms + 8 * i) as *mut u64;
                    *slot = crate::jni::guest_vm_for(*slot);
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------------------------
// registro principal
// ---------------------------------------------------------------------------------------------

fn open_common(c: &mut Cpu, dirfd: u64, path: u64, flags: u64, mode: u64) {
    if let Some(r) = crate::procemu::try_open(dirfd, path, flags) {
        return sysret(c, r);
    }
    let r = syscall::host_syscall(257, [dirfd, path, syscall::open_flags_to_host(flags), mode, 0, 0]);
    // maps/smaps de este proceso con paginas del guest mayores: contenido a 16 KiB
    sysret(c, syscall::proc_maps_after_open(r, path, flags));
}

/// Flags de `open` (arm64) para un modo de `fopen`.
fn fopen_flags(mode: u64) -> u64 {
    let m = if mode == 0 { &b""[..] } else { unsafe { CStr::from_ptr(mode as *const c_char) }.to_bytes() };
    let mut f = match m.first() {
        Some(b'r') => 0,
        Some(b'w') => 1 | 0o100 | 0o1000,
        Some(b'a') => 1 | 0o100 | 0o2000,
        _ => 0,
    };
    if m.contains(&b'+') {
        f = (f & !3) | 2;
    }
    if m.contains(&b'x') {
        f |= 0o200;
    }
    if m.contains(&b'e') {
        f |= 0o2000000;
    }
    f
}

/// `fopen` de un archivo emulado: FILE del host sobre el descriptor de solo lectura (NULL y errno si no se puede).
fn fopen_emulated(k: crate::procemu::Kind, mode: u64) -> u64 {
    let fd = crate::procemu::open(k, fopen_flags(mode));
    if fd < 0 {
        set_errno((-fd) as i32);
        return 0;
    }
    let f = host_call(host_sym("fdopen"), &[fd as u64, mode], &[]).0;
    if f == 0 {
        unsafe { close(fd as i32) };
    }
    f
}

fn stat_common(c: &mut Cpu, dirfd: u64, path: u64, buf: u64, flags: u64) {
    let mut tmp = [0u8; 144];
    let r = syscall::host_syscall(262, [dirfd, path, tmp.as_mut_ptr() as u64, flags, 0, 0]);
    if r >= 0 {
        unsafe { syscall::stat_to_guest(tmp.as_ptr(), buf as *mut u8) };
    }
    sysret(c, r);
}

/// Con paginas del guest mayores que las del host (16 KiB): las funciones de bionic que hacen llamadas al sistema
/// cuya granularidad ve el guest pasan por su traduccion (`syscall::translate`), y fopen de /proc/self/maps y smaps
/// da su contenido a 16 KiB. Con paginas de 4 KiB no se registran (reenvio directo, sin coste).
fn reg_paged_memory() {
    reg_ret("mincore", |c| {
        let r = syscall::translate(c, 232, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    for (n, nr) in [("mlock", 228), ("munlock", 229)] {
        reg_ret(n, move |c| {
            let r = syscall::translate(c, nr, [c.x[0], c.x[1], 0, 0, 0, 0]);
            sysret(c, r);
        });
    }
    reg_ret("mlock2", |c| {
        let r = syscall::translate(c, 284, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    // bionic brk/sbrk: __brk con el valor pedido; si el nucleo no llega, ENOMEM
    reg_ret("brk", |c| {
        let want = c.x[0];
        let r = syscall::translate(c, 214, [want, 0, 0, 0, 0, 0]) as u64;
        sysret(c, if r < want { -12 } else { 0 });
    });
    reg_ret("sbrk", |c| {
        let inc = c.x[0] as i64;
        let cur = syscall::translate(c, 214, [0; 6]) as u64;
        if inc == 0 {
            c.x[0] = cur;
            return;
        }
        let want = cur.wrapping_add(inc as u64);
        if (inc > 0 && want < cur) || (inc < 0 && want > cur) || syscall::translate(c, 214, [want, 0, 0, 0, 0, 0]) as u64 != want {
            sysret(c, -12);
            return;
        }
        c.x[0] = cur;
    });
    for n in ["fopen", "fopen64"] {
        reg_ret(n, move |c| {
            let (path, mode) = (c.x[0], c.x[1]);
            // sustituye al fopen de `procemu` (archivos de la CPU emulados)
            if let Some(k) = crate::procemu::classify((-100i64) as u64, path) {
                c.x[0] = fopen_emulated(k, mode);
                return;
            }
            let m = if mode == 0 { &b""[..] } else { cbytes(mode) };
            // como bionic __sflags: solo lectura ('r' sin '+'); 'e' = O_CLOEXEC
            let ro = m.first() == Some(&b'r') && !m.contains(&b'+');
            if ro && path != 0 && rt::addr_readable(path) && cbytes(path).starts_with(b"/proc/") {
                let cloexec = if m.contains(&b'e') { 0o2000000 } else { 0 };
                let h = syscall::host_syscall(257, [(-100i64) as u64, path, cloexec, 0, 0, 0]);
                let fd = syscall::proc_maps_after_open(h, path, cloexec);
                if fd == h && h >= 0 {
                    // otra ruta de /proc: la abre fopen del host
                    syscall::host_syscall(3, [h as u64, 0, 0, 0, 0, 0]);
                }
                if fd != h && fd >= 0 {
                    let f = host_call(host_sym("fdopen"), &[fd as u64, mode], &[]).0;
                    if f == 0 {
                        let e = unsafe { *__errno_location() };
                        syscall::host_syscall(3, [fd as u64, 0, 0, 0, 0, 0]);
                        set_errno(e);
                    }
                    c.x[0] = f;
                    return;
                }
            }
            match host_sym(n) {
                Some(f) => forward(c, f, None),
                None => sysret(c, -38),
            }
        });
    }
}

fn ld_unary(name: &'static str, host: &'static str) {
    reg_ret(name, move |c| {
        let q = c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64);
        let d = fmt::quad_to_f64(q);
        let (_, x) = host_call(host_sym(host), &[], &[d.to_bits()]);
        let r = fmt::f64_to_quad(f64::from_bits(x));
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
}

fn ld_binary(name: &'static str, host: &'static str) {
    reg_ret(name, move |c| {
        let a = fmt::quad_to_f64(c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64));
        let b = fmt::quad_to_f64(c.v[1][0] as u128 | ((c.v[1][1] as u128) << 64));
        let (_, x) = host_call(host_sym(host), &[], &[a.to_bits(), b.to_bits()]);
        let r = fmt::f64_to_quad(f64::from_bits(x));
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
}

fn register_all() {
    hle::init();
    rt::ret_slot();
    crate::sig::debuggerd_slot();
    reg_printf();
    crate::dl::register();
    reg_jni_vm();
    reg_valist_misc();

    // OpenSL ES / OpenMAX AL: sus objetos son tablas de punteros a funciones del host; el guest recibe proxys
    // (proxy.rs). Estructuras de callbacks y objetos del NDK que necesitan conversion a mano: ndkcb.rs.
    crate::proxy::register();
    crate::ndkcb::register();
    // Retorno de estructura de mas de 16 bytes: en AAPCS64 el destino va en x8; en SysV es el primer argumento.
    for n in ["mallinfo", "mallinfo2"] {
        reg_ret(n, move |c| {
            let dst = c.x[8];
            if host_sym(n).is_some() && dst != 0 {
                host_call(host_sym(n), &[dst], &[]);
            } else if dst != 0 {
                unsafe { std::ptr::write_bytes(dst as *mut u8, 0, 80) };
            }
        });
    }

    // --- funciones BSD antiguas que la libc arm64 de Android no exporta pero que algunos compiladores emiten ----
    reg_ret("bcmp", |c| {
        let (a, b, n) = (c.x[0] as *const u8, c.x[1] as *const u8, c.x[2] as usize);
        c.x[0] = unsafe { std::slice::from_raw_parts(a, n) != std::slice::from_raw_parts(b, n) } as u64;
    });
    reg_ret("bcopy", |c| unsafe { std::ptr::copy(c.x[0] as *const u8, c.x[1] as *mut u8, c.x[2] as usize) });
    reg_ret("bzero", |c| unsafe { std::ptr::write_bytes(c.x[0] as *mut u8, 0, c.x[1] as usize) });

    // --- clasificacion de long double (binary128 en v0): no se puede reenviar, se calcula aqui ----------
    let quad = |c: &Cpu| -> (u64, bool, bool) {
        let (lo, hi) = (c.v[0][0], c.v[0][1]);
        ((hi >> 48) & 0x7fff, (hi & 0x0000_ffff_ffff_ffff) | lo != 0, hi >> 63 != 0)
    };
    for n in ["isnanl", "__isnanl"] {
        reg_ret(n, move |c| c.x[0] = (quad(c).0 == 0x7fff && quad(c).1) as u64);
    }
    for n in ["isinfl", "__isinfl"] {
        reg_ret(n, move |c| c.x[0] = (quad(c).0 == 0x7fff && !quad(c).1) as u64);
    }
    for n in ["isfinitel", "__isfinitel"] {
        reg_ret(n, move |c| c.x[0] = (quad(c).0 != 0x7fff) as u64);
    }
    for n in ["isnormall", "__isnormall"] {
        reg_ret(n, move |c| c.x[0] = (quad(c).0 != 0 && quad(c).0 != 0x7fff) as u64);
    }
    reg_ret("__signbitl", move |c| c.x[0] = quad(c).2 as u64);
    // FP_INFINITE 1, FP_NAN 2, FP_NORMAL 4, FP_SUBNORMAL 8, FP_ZERO 0x10 (bionic)
    reg_ret("__fpclassifyl", move |c| {
        let (e, frac, _) = quad(c);
        c.x[0] = match (e, frac) {
            (0x7fff, false) => 1,
            (0x7fff, true) => 2,
            (0, false) => 0x10,
            (0, true) => 8,
            _ => 4,
        };
    });

    // --- soporte que el NDK exporta sin firma publica ---------------------------------------------------
    // CFI entre bibliotecas: la comprobacion del host no conoce los modulos guest; se acepta la llamada.
    reg_ret("__cfi_slowpath", |_c| {});
    reg_ret("__cfi_slowpath_diag", |_c| {});
    reg_ret("__cfi_shadow_size", |c| c.x[0] = 0);
    reg_ret("__cxa_pure_virtual", |c| {
        crate::bridge::alog_fatal(&format!("llamada a funcion virtual pura\n{}", rt::dump_backtrace(c)));
        unsafe { abort() }
    });
    alias("epoll_pwait64", "epoll_pwait");

    // --- entorno de coma flotante (fenv) -------------------------------------------------------------
    // No se reenvia: fenv_t mide 8 bytes en arm64 y 32 en x86-64 (el host escribiria fuera del bufer guest), las
    // constantes difieren y el estado real del guest esta en FPCR/FPSR emulados, no en el MXCSR del host.
    // bionic arm64: fenv_t { u32 control(FPCR); u32 status(FPSR) }, fexcept_t = u32, redondeo 0..3, excepciones 0x9f.
    const FE_ALL: u64 = 0x9f;
    reg_ret("fegetround", |c| c.x[0] = (c.fpcr >> 22) & 3);
    reg_ret("fesetround", |c| {
        let r = c.x[0];
        if r > 3 {
            c.x[0] = 1;
        } else {
            c.fpcr = (c.fpcr & !(3 << 22)) | (r << 22);
            c.x[0] = 0;
        }
    });
    reg_ret("feclearexcept", |c| {
        crate::jit::fold_mxcsr(c);
        c.fpsr &= !(c.x[0] & FE_ALL);
        c.x[0] = 0;
    });
    reg_ret("feraiseexcept", |c| {
        c.fpsr |= c.x[0] & FE_ALL;
        c.x[0] = 0;
    });
    reg_ret("fetestexcept", |c| {
        crate::jit::fold_mxcsr(c);
        c.x[0] = c.fpsr & c.x[0] & FE_ALL;
    });
    reg_ret("fegetexceptflag", |c| {
        crate::jit::fold_mxcsr(c);
        unsafe { *(c.x[0] as *mut u32) = (c.fpsr & c.x[1] & FE_ALL) as u32 };
        c.x[0] = 0;
    });
    reg_ret("fesetexceptflag", |c| {
        crate::jit::fold_mxcsr(c);
        let (f, e) = (unsafe { *(c.x[0] as *const u32) } as u64, c.x[1] & FE_ALL);
        c.fpsr = (c.fpsr & !e) | (f & e);
        c.x[0] = 0;
    });
    reg_ret("fegetenv", |c| {
        crate::jit::fold_mxcsr(c);
        unsafe { *(c.x[0] as *mut [u32; 2]) = [c.fpcr as u32, c.fpsr as u32] };
        c.x[0] = 0;
    });
    reg_ret("feholdexcept", |c| {
        crate::jit::fold_mxcsr(c);
        unsafe { *(c.x[0] as *mut [u32; 2]) = [c.fpcr as u32, c.fpsr as u32] };
        c.fpsr &= !FE_ALL;
        c.fpcr &= !(FE_ALL << 8); // sin trampas
        c.x[0] = 0;
    });
    reg_ret("fesetenv", |c| {
        crate::jit::fold_mxcsr(c);
        let e = unsafe { *(c.x[0] as *const [u32; 2]) };
        c.fpcr = e[0] as u64 & crate::interp::FPCR_RW;
        c.fpsr = e[1] as u64 & crate::interp::FPSR_RW;
        c.x[0] = 0;
    });
    reg_ret("feupdateenv", |c| {
        crate::jit::fold_mxcsr(c);
        let e = unsafe { *(c.x[0] as *const [u32; 2]) };
        let pend = c.fpsr & FE_ALL;
        c.fpcr = e[0] as u64 & crate::interp::FPCR_RW;
        c.fpsr = (e[1] as u64 & crate::interp::FPSR_RW) | pend;
        c.x[0] = 0;
    });
    // trampas de coma flotante: no soportadas (como en la mayoria del hardware arm64)
    reg_ret("feenableexcept", |c| c.x[0] = u64::MAX);
    reg_ret("fedisableexcept", |c| c.x[0] = 0);
    reg_ret("fegetexcept", |c| c.x[0] = 0);
    data_cell("__fe_dfl_env", &[0u8; 8]);

    // --- funciones que tocan el flujo de control o la pila del hilo host: nunca se reenvian tal cual ----
    // sigaltstack: la pila alternativa del guest no puede instalarse en el hilo del host (ART ejecutaria sus
    // manejadores x86 sobre memoria del guest, normalmente demasiado pequena). Se emula: se guarda y se devuelve.
    reg_ret("sigaltstack", |c| {
        let (new, old) = (c.x[0], c.x[1]);
        let cur = ALTSTACK.with(|a| a.get());
        if old != 0 {
            unsafe { *(old as *mut [u64; 3]) = [cur.0, cur.1, cur.2] };
        }
        if new != 0 {
            let n = unsafe { *(new as *const [u64; 3]) };
            ALTSTACK.with(|a| a.set((n[0], n[1] & 0xffff_ffff, n[2])));
        }
        c.x[0] = 0;
    });
    // vfork comparte la pila con el padre hasta exec: a traves de un reenvio corromperia los marcos. Se trata como fork.
    reg_ret("vfork", |c| {
        c.x[0] = host_call(host_sym("fork"), &[], &[]).0;
    });

    // --- errno -------------------------------------------------------------------------------
    reg_ret("__errno", |c| c.x[0] = unsafe { __errno_location() } as u64);

    // --- archivos: flags de open y stat ----------------------------------------------------
    for n in ["open", "open64"] {
        reg_ret(n, |c| open_common(c, (-100i64) as u64, c.x[0], c.x[1], c.x[2]));
    }
    for n in ["openat", "openat64"] {
        reg_ret(n, |c| open_common(c, c.x[0], c.x[1], c.x[2], c.x[3]));
    }
    // fopen/freopen de los archivos de la CPU emulados (`procemu`): el FILE es del host (fdopen/freopen del host)
    for n in ["fopen", "fopen64"] {
        reg_ret(n, move |c| {
            let (path, mode) = (c.x[0], c.x[1]);
            match crate::procemu::classify((-100i64) as u64, path) {
                None => c.x[0] = host_call(host_sym(n), &[path, mode], &[]).0,
                Some(k) => c.x[0] = fopen_emulated(k, mode),
            }
        });
    }
    for n in ["freopen", "freopen64"] {
        reg_ret(n, move |c| {
            let (path, mode, stream) = (c.x[0], c.x[1], c.x[2]);
            let Some(k) = crate::procemu::classify((-100i64) as u64, path) else {
                c.x[0] = host_call(host_sym(n), &[path, mode, stream], &[]).0;
                return;
            };
            let fd = crate::procemu::open(k, fopen_flags(mode));
            if fd < 0 {
                // freopen cierra el flujo aunque falle la apertura
                host_call(host_sym("fclose"), &[stream], &[]);
                set_errno((-fd) as i32);
                c.x[0] = 0;
                return;
            }
            let p = format!("/proc/self/fd/{}\0", fd);
            c.x[0] = host_call(host_sym(n), &[p.as_ptr() as u64, mode, stream], &[]).0;
            unsafe { close(fd as i32) };
        });
    }
    reg_ret("__open_2", |c| open_common(c, (-100i64) as u64, c.x[0], c.x[1], 0));
    reg_ret("__openat_2", |c| open_common(c, c.x[0], c.x[1], c.x[2], 0));
    reg_ret("creat", |c| open_common(c, (-100i64) as u64, c.x[0], 0o1101, c.x[1]));
    reg_ret("creat64", |c| open_common(c, (-100i64) as u64, c.x[0], 0o1101, c.x[1]));
    for n in ["fcntl", "fcntl64"] {
        reg_ret(n, |c| {
            let a = [c.x[0], c.x[1], c.x[2], 0, 0, 0];
            let r = syscall::translate(c, 25, a);
            sysret(c, r);
        });
    }
    reg_ret("pipe2", |c| {
        let r = syscall::translate(c, 59, [c.x[0], c.x[1], 0, 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("dup3", |c| {
        let r = syscall::translate(c, 24, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("ioctl", |c| {
        let r = syscall::host_syscall(16, [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], c.x[5]]);
        sysret(c, r);
    });
    for n in ["stat", "stat64"] {
        reg_ret(n, |c| stat_common(c, (-100i64) as u64, c.x[0], c.x[1], 0));
    }
    for n in ["lstat", "lstat64"] {
        reg_ret(n, |c| stat_common(c, (-100i64) as u64, c.x[0], c.x[1], 0x100));
    }
    for n in ["fstat", "fstat64"] {
        reg_ret(n, |c| {
            let mut tmp = [0u8; 144];
            let r = syscall::host_syscall(5, [c.x[0], tmp.as_mut_ptr() as u64, 0, 0, 0, 0]);
            if r >= 0 {
                unsafe { syscall::stat_to_guest(tmp.as_ptr(), c.x[1] as *mut u8) };
            }
            sysret(c, r);
        });
    }
    for n in ["fstatat", "fstatat64", "newfstatat"] {
        reg_ret(n, |c| stat_common(c, c.x[0], c.x[1], c.x[2], c.x[3]));
    }
    // --- memoria --------------------------------------------------------------------------
    for n in ["mmap", "mmap64"] {
        reg_ret(n, |c| {
            let r = syscall::translate(c, 222, [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], c.x[5]]);
            sysret(c, r);
        });
    }
    reg_ret("munmap", |c| {
        let r = syscall::translate(c, 215, [c.x[0], c.x[1], 0, 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("mprotect", |c| {
        let r = syscall::translate(c, 226, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    // madvise, msync y mremap pasan por la traduccion de llamadas: con paginas de 16 KiB, sus comprobaciones
    reg_ret("madvise", |c| {
        let r = syscall::translate(c, 233, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("msync", |c| {
        let r = syscall::translate(c, 227, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("mremap", |c| {
        let r = syscall::translate(c, 216, [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], 0]);
        sysret(c, r);
    });
    if crate::mem::guest_page() > crate::mem::PAGE as u64 {
        reg_paged_memory();
    }
    reg_ret("pkey_mprotect", |c| {
        let r = syscall::translate(c, 226, [c.x[0], c.x[1], c.x[2], 0, 0, 0]);
        sysret(c, r);
    });
    reg_ret("syscall", |c| {
        let n = c.x[0];
        let a = [c.x[1], c.x[2], c.x[3], c.x[4], c.x[5], c.x[6]];
        crate::rt::note_syscall(n, a);
        let r = syscall::translate(c, n, a);
        sysret(c, r);
    });
    reg_ret("prctl", |c| {
        let r = syscall::translate(c, 167, [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], 0]);
        sysret(c, r);
    });
    reg_ret("uname", |c| {
        let r = syscall::translate(c, 160, [c.x[0], 0, 0, 0, 0, 0]);
        sysret(c, r);
    });
    // epoll: struct epoll_event es de 16 bytes en arm64 y empaquetada de 12 en x86-64
    reg_ret("epoll_ctl", |c| {
        let r = syscall::translate(c, 21, [c.x[0], c.x[1], c.x[2], c.x[3], 0, 0]);
        sysret(c, r);
    });
    reg_ret("epoll_wait", |c| {
        let r = syscall::translate(c, 22, [c.x[0], c.x[1], c.x[2], c.x[3], 0, 8]);
        sysret(c, r);
    });
    reg_ret("epoll_pwait", |c| {
        let r = syscall::translate(c, 22, [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], 8]);
        sysret(c, r);
    });

    // --- senales ----------------------------------------------------------------------------
    reg_ret("sigaction", |c| {
        let (s, act, old) = (c.x[0] as i32, c.x[1], c.x[2]);
        // struct sigaction de bionic LP64: { int sa_flags; handler; sigset_t mask; restorer }
        let new = if act != 0 {
            unsafe { Some(GuestAct { flags: *(act as *const i32) as u32 as u64, handler: *((act + 8) as *const u64), mask: *((act + 16) as *const u64) }) }
        } else {
            None
        };
        match sig::guest_sigaction(s, new) {
            Ok(o) => {
                if old != 0 {
                    unsafe {
                        std::ptr::write_bytes(old as *mut u8, 0, 32);
                        *(old as *mut i32) = o.flags as i32;
                        *((old + 8) as *mut u64) = o.handler;
                        *((old + 16) as *mut u64) = o.mask;
                    }
                }
                c.x[0] = 0;
            }
            Err(e) => {
                set_errno(e);
                c.x[0] = u64::MAX;
            }
        }
    });
    alias("sigaction64", "sigaction");
    for n in ["signal", "bsd_signal", "sysv_signal"] {
        reg_ret(n, |c| {
            let (s, h) = (c.x[0] as i32, c.x[1]);
            match sig::guest_sigaction(s, Some(GuestAct { handler: h, flags: sig::SA_RESTART, mask: 0 })) {
                Ok(o) => c.x[0] = o.handler,
                Err(e) => {
                    set_errno(e);
                    c.x[0] = u64::MAX;
                }
            }
        });
    }
    for (n, sz) in [("sigemptyset", 0), ("sigfillset", 1)] {
        reg_ret(n, move |c| {
            unsafe { *(c.x[0] as *mut u64) = if sz == 0 { 0 } else { u64::MAX } };
            c.x[0] = 0;
        });
    }
    reg_ret("sigaddset", |c| {
        let s = c.x[1] as i32;
        if s < 1 || s > 64 {
            set_errno(22);
            c.x[0] = u64::MAX;
            return;
        }
        unsafe { *(c.x[0] as *mut u64) |= 1u64 << (s - 1) };
        c.x[0] = 0;
    });
    reg_ret("sigdelset", |c| {
        let s = c.x[1] as i32;
        if s < 1 || s > 64 {
            set_errno(22);
            c.x[0] = u64::MAX;
            return;
        }
        unsafe { *(c.x[0] as *mut u64) &= !(1u64 << (s - 1)) };
        c.x[0] = 0;
    });
    reg_ret("sigismember", |c| {
        let s = c.x[1] as i32;
        if s < 1 || s > 64 {
            set_errno(22);
            c.x[0] = u64::MAX;
            return;
        }
        c.x[0] = unsafe { (*(c.x[0] as *const u64) >> (s - 1)) & 1 };
    });
    // La mascara se aplica al hilo host que ejecuta al guest (es el mismo hilo del nucleo), con la semantica de
    // libsigchain y bionic (sig::guest_sigprocmask): SIG_BLOCK/SIG_SETMASK no bloquean SIGSEGV ni SIGBUS (las reclama
    // ART), las reservadas de bionic nunca se bloquean y la del temporizador (32) siempre; la consulta devuelve la
    // mascara real. sigset_t y sigset64_t de bionic LP64
    // ocupan 8 bytes en arm64 y x86-64, con los mismos numeros de senal y los mismos SIG_BLOCK/UNBLOCK/SETMASK.
    for n in ["pthread_sigmask", "pthread_sigmask64"] {
        reg_ret(n, |c| {
            let r = guest_mask_call(c.x[0], c.x[1], c.x[2], false);
            c.x[0] = if r < 0 { (-r) as u64 } else { 0 }; // pthread_sigmask devuelve el errno, sin tocar errno
        });
    }
    for n in ["sigprocmask", "sigprocmask64"] {
        reg_ret(n, |c| {
            let r = guest_mask_call(c.x[0], c.x[1], c.x[2], false);
            sysret(c, if r < 0 { r } else { 0 }); // sigprocmask: -1 y errno
        });
    }
    reg_ret("__libc_current_sigrtmin", |c| c.x[0] = sig::rtmin() as u64);
    reg_ret("__libc_current_sigrtmax", |c| c.x[0] = 64);

    // --- setjmp/longjmp ------------------------------------------------------------------------
    for n in ["setjmp", "_setjmp"] {
        reg_ret(n, move |c| do_setjmp(c, c.x[0], n == "setjmp"));
    }
    reg_ret("sigsetjmp", |c| do_setjmp(c, c.x[0], c.x[1] != 0));
    reg_ret("__sigsetjmp", |c| do_setjmp(c, c.x[0], c.x[1] != 0));
    for n in ["longjmp", "_longjmp", "siglongjmp", "__siglongjmp"] {
        reg(n, |c| {
            do_longjmp(c, c.x[0], c.x[1]);
            Ret::Stay
        });
    }

    // --- callbacks ------------------------------------------------------------------------------
    reg_ret("qsort", |c| {
        let (base, n, es, cmp) = (c.x[0] as usize, c.x[1] as usize, c.x[2] as usize, c.x[3]);
        if n < 2 || es == 0 {
            return;
        }
        let mut s = Sorter { c, cmp, arg: None, es };
        s.sort(base, n);
    });
    reg_ret("qsort_r", |c| {
        let (base, n, es, cmp, arg) = (c.x[0] as usize, c.x[1] as usize, c.x[2] as usize, c.x[3], c.x[4]);
        if n < 2 || es == 0 {
            return;
        }
        let mut s = Sorter { c, cmp, arg: Some(arg), es };
        s.sort(base, n);
    });
    reg_ret("bsearch", |c| {
        let (key, base, nmemb, size, cmp) = (c.x[0], c.x[1], c.x[2], c.x[3], c.x[4]);
        let (mut b, mut lim) = (base, nmemb);
        while lim != 0 {
            let p = b + (lim >> 1) * size;
            let r = rt::call_guest_on(c, cmp, &[key, p], &[]).0 as i32;
            if r == 0 {
                c.x[0] = p;
                return;
            }
            if r > 0 {
                b = p + size;
                lim -= 1;
            }
            lim >>= 1;
        }
        c.x[0] = 0;
    });
    reg_ret("atexit", |c| {
        crate::monitor::lock(&ATEXIT).push(AtExit { f: c.x[0], arg: 0, dso: 0, noarg: true });
        c.x[0] = 0;
    });
    reg_ret("__cxa_atexit", |c| {
        crate::monitor::lock(&ATEXIT).push(AtExit { f: c.x[0], arg: c.x[1], dso: c.x[2], noarg: false });
        c.x[0] = 0;
    });
    reg_ret("__cxa_finalize", |c| {
        let d = c.x[0];
        run_atexit(Some(d), c);
    });
    reg_ret("__cxa_thread_atexit_impl", |c| {
        let (f, a, dso) = (c.x[0], c.x[1], c.x[2]);
        // como bionic: la biblioteca de `dso` no se descarga hasta que el hilo ejecute el destructor
        crate::elf::dso_ref(dso);
        THREAD_ATEXIT.with(|t| t.borrow_mut().push((f, a, dso)));
        c.x[0] = 0;
    });
    reg_ret("exit", |c| {
        run_atexit(None, c);
        unsafe { exit(c.x[0] as i32) }
    });
    reg_ret("quick_exit", |c| unsafe { _exit(c.x[0] as i32) });
    reg_ret("pthread_atfork", |c| c.x[0] = 0); // sin fork multihilo: se ignora
    reg_ret("__register_atfork", |c| c.x[0] = 0); // lo que importan los binarios del NDK: mismo tratamiento
    reg_ret("pthread_create", |c| {
        let (tidp, attr, start, arg) = (c.x[0], c.x[1], c.x[2], c.x[3]);
        let mut hattr = [0u8; 128];
        let mut stack: usize = 1 << 20;
        unsafe {
            pthread_attr_init(hattr.as_mut_ptr() as *mut c_void);
            pthread_attr_setstacksize(hattr.as_mut_ptr() as *mut c_void, 1 << 20);
            if attr != 0 {
                let mut st = 0usize;
                if pthread_attr_getstacksize(attr as *const c_void, &mut st) == 0 && st >= 16384 {
                    stack = st;
                }
                let mut ds = 0;
                if pthread_attr_getdetachstate(attr as *const c_void, &mut ds) == 0 {
                    pthread_attr_setdetachstate(hattr.as_mut_ptr() as *mut c_void, ds);
                }
            }
            // como bionic: el hilo nuevo nace con todo bloqueado (salvo los fallos sincronos) y pone la mascara del
            // creador cuando ya tiene estado guest; si no, una senal que llegara antes se perderia
            let si = Box::into_raw(Box::new(rt::StartInfo { start, arg, stack, mask: sig::guest_mask() }));
            let all: u64 = !(sig::sbit(4) | sig::sbit(7) | sig::sbit(8) | sig::sbit(11));
            let mut prev = 0u64;
            crate::sys::syscall(14, 2i64, &all as *const u64, &mut prev as *mut u64, 8usize);
            let mut t = 0u64;
            let r = pthread_create(&mut t, hattr.as_ptr() as *const c_void, rt::thread_entry, si as *mut c_void);
            crate::sys::syscall(14, 2i64, &prev as *const u64, 0usize, 8usize);
            pthread_attr_destroy(hattr.as_mut_ptr() as *mut c_void);
            if r == 0 {
                *(tidp as *mut u64) = t;
            } else {
                drop(Box::from_raw(si));
            }
            c.x[0] = r as u64;
        }
    });
    // El GC conservador (Boehm de IL2CPP) pide los limites de la pila del hilo: deben ser los de la pila GUEST.
    reg_ret("pthread_getattr_np", |c| {
        let (th, attr) = (c.x[0], c.x[1] as *mut c_void);
        let r = unsafe { pthread_getattr_np(th, attr) };
        if r == 0 {
            if let Some((lo, hi)) = rt::guest_stack_of(th) {
                unsafe { pthread_attr_setstack(attr, lo as *mut c_void, (hi - lo) as usize) };
            }
        }
        c.x[0] = r as u64;
    });
    reg_ret("pthread_exit", |c| unsafe { thread_exit_now(c.x[0]) });
    reg_ret("thrd_exit", |c| unsafe { thread_exit_now(c.x[0] as u32 as u64) });
    reg_ret("pthread_key_create", |c| {
        let mut k = 0u32;
        let r = unsafe { pthread_key_create(&mut k, std::ptr::null()) };
        if r == 0 {
            unsafe { *(c.x[0] as *mut u32) = k };
            crate::monitor::lock(&KEYS).push((k, c.x[1]));
        }
        c.x[0] = r as u64;
    });
    reg_ret("pthread_key_delete", |c| {
        let k = c.x[0] as u32;
        crate::monitor::lock(&KEYS).retain(|e| e.0 != k);
        c.x[0] = unsafe { pthread_key_delete(k) } as u64;
    });
    reg_ret("pthread_once", |c| {
        let (once, f) = (c.x[0] as *mut std::sync::atomic::AtomicI32, c.x[1]);
        use std::sync::atomic::Ordering::*;
        let o = unsafe { &*once };
        loop {
            match o.compare_exchange(0, 1, SeqCst, SeqCst) {
                Ok(_) => {
                    rt::call_guest_on(c, f, &[], &[]);
                    o.store(2, SeqCst);
                    break;
                }
                Err(2) => break,
                Err(_) => unsafe {
                    syscall(24);
                },
            }
        }
        c.x[0] = 0;
    });
    // pthread_cleanup_push/pop (bionic): struct __pthread_cleanup_t { prev, routine, arg } en memoria del guest,
    // enlazada en una cadena por hilo (CLEANUP); pthread_exit la ejecuta (pthread_exit_dtors)
    reg_ret("__pthread_cleanup_push", |c| {
        let (cl, f, a) = (c.x[0] as *mut u64, c.x[1], c.x[2]);
        let head = CLEANUP.with(|h| h.get());
        unsafe {
            *cl = head;
            *cl.add(1) = f;
            *cl.add(2) = a;
        }
        CLEANUP.with(|h| h.set(cl as u64));
    });
    reg_ret("__pthread_cleanup_pop", |c| {
        let (cl, exec) = (c.x[0] as *mut u64, c.x[1]);
        let (prev, f, a) = unsafe { (*cl, *cl.add(1), *cl.add(2)) };
        CLEANUP.with(|h| h.set(prev));
        if exec != 0 {
            rt::call_guest_on(c, f, &[a], &[]);
        }
    });

    // --- misc ---------------------------------------------------------------------------------------
    reg_ret("getauxval", |c| {
        let t = c.x[0];
        c.x[0] = guest_auxv_entry(t, unsafe { getauxval(t) }).unwrap_or(0);
    });
    // tamano de pagina del guest (`mem::guest_page`): _SC_PAGESIZE/_SC_PAGE_SIZE de bionic (39, 40); lo demas, al host
    reg_ret("getpagesize", |c| c.x[0] = crate::mem::guest_page());
    reg_ret("sysconf", |c| {
        c.x[0] = match c.x[0] {
            39 | 40 => crate::mem::guest_page(),
            n => (unsafe { sysconf(n as i32) }) as u64,
        };
    });
    reg_ret("__stack_chk_fail", |c| {
        crate::bridge::alog_fatal(&format!("*** stack smashing detected ***\n{}", rt::dump_backtrace(c)));
        unsafe { abort() }
    });
    reg_ret("__assert2", |c| {
        crate::bridge::alog_fatal(&format!("{}:{}: {}: assertion \"{}\" failed", cstr(c.x[0]), c.x[1] as i32, cstr(c.x[2]), cstr(c.x[3])));
        unsafe { abort() }
    });
    reg_ret("__assert", |c| {
        crate::bridge::alog_fatal(&format!("{}:{}: assertion \"{}\" failed", cstr(c.x[0]), c.x[1] as i32, cstr(c.x[2])));
        unsafe { abort() }
    });
    reg_ret("android_set_abort_message", |c| {
        crate::bridge::alog_fatal(&format!("abort message: {}", cstr(c.x[0])));
    });
    reg_ret("__system_property_get", |c| {
        let f = host_sym("__system_property_get");
        let (name, val) = (c.x[0], c.x[1]);
        // el guest es arm64: las propiedades de ABI del host (x86_64) no deben verse
        let abi = match cstr(name).as_str() {
            "ro.product.cpu.abi" | "ro.product.cpu.abilist" | "ro.product.cpu.abilist64" => Some("arm64-v8a"),
            "ro.product.cpu.abilist32" => Some(""),
            "ro.bionic.arch" => Some("arm64"),
            _ => None,
        };
        if let Some(v) = abi {
            unsafe {
                std::ptr::copy_nonoverlapping(v.as_ptr(), val as *mut u8, v.len());
                *((val + v.len() as u64) as *mut u8) = 0;
            }
            c.x[0] = v.len() as u64;
            return;
        }
        if f.is_some() {
            c.x[0] = host_call(f, &[name, val], &[]).0;
        } else {
            unsafe { *(val as *mut u8) = 0 };
            c.x[0] = 0;
        }
    });
    reg_ret("div", |c| {
        let (n, d) = (c.x[0] as i32, c.x[1] as i32);
        c.x[0] = (n.wrapping_div(d) as u32 as u64) | ((n.wrapping_rem(d) as u32 as u64) << 32);
    });
    reg_ret("ldiv", |c| {
        let (n, d) = (c.x[0] as i64, c.x[1] as i64);
        c.x[0] = n.wrapping_div(d) as u64;
        c.x[1] = n.wrapping_rem(d) as u64;
    });
    alias("lldiv", "ldiv");
    reg_ret("imaxdiv", |c| {
        let (n, d) = (c.x[0] as i64, c.x[1] as i64);
        c.x[0] = n.wrapping_div(d) as u64;
        c.x[1] = n.wrapping_rem(d) as u64;
    });

    // --- fortify (__*_chk): comprueba el tamano y delega -------------------------------------------
    for (n, host, dstlen_idx, len_idx) in [
        ("__memcpy_chk", "memcpy", 3usize, 2usize),
        ("__memmove_chk", "memmove", 3, 2),
        ("__memset_chk", "memset", 3, 2),
    ] {
        reg_ret(n, move |c| {
            if c.x[len_idx] > c.x[dstlen_idx] {
                crate::bridge::alog_fatal(&format!("FORTIFY: {}: desbordamiento de buffer detectado\n{}", n, rt::dump_backtrace(c)));
                unsafe { abort() }
            }
            c.x[0] = host_call(host_sym(host), &[c.x[0], c.x[1], c.x[2]], &[]).0;
        });
    }
    for (n, host) in [("__strcpy_chk", "strcpy"), ("__strcat_chk", "strcat"), ("__stpcpy_chk", "stpcpy")] {
        reg_ret(n, move |c| {
            let (d, s, dl) = (c.x[0], c.x[1], c.x[2]);
            let need = cbytes(s).len() as u64 + 1 + if n == "__strcat_chk" { cbytes(d).len() as u64 } else { 0 };
            if need > dl {
                crate::bridge::alog_fatal(&format!("FORTIFY: {}: desbordamiento de buffer detectado\n{}", n, rt::dump_backtrace(c)));
                unsafe { abort() }
            }
            c.x[0] = host_call(host_sym(host), &[d, s], &[]).0;
        });
    }
    for (n, host) in [("__strncpy_chk", "strncpy"), ("__strncat_chk", "strncat")] {
        reg_ret(n, move |c| {
            if c.x[2] > c.x[3] {
                crate::bridge::alog_fatal(&format!("FORTIFY: {}: desbordamiento de buffer detectado\n{}", n, rt::dump_backtrace(c)));
                unsafe { abort() }
            }
            c.x[0] = host_call(host_sym(host), &[c.x[0], c.x[1], c.x[2]], &[]).0;
        });
    }
    reg_ret("__strlen_chk", |c| {
        let l = cbytes(c.x[0]).len() as u64;
        if l >= c.x[1] {
            crate::bridge::alog_fatal(&format!("FORTIFY: strlen: lectura fuera de limites\n{}", rt::dump_backtrace(c)));
            unsafe { abort() }
        }
        c.x[0] = l;
    });
    reg_ret("__strchr_chk", |c| {
        c.x[0] = host_call(host_sym("strchr"), &[c.x[0], c.x[1]], &[]).0;
    });
    reg_ret("__strrchr_chk", |c| {
        c.x[0] = host_call(host_sym("strrchr"), &[c.x[0], c.x[1]], &[]).0;
    });
    for (n, host) in [("__read_chk", "read"), ("__pread_chk", "pread"), ("__readlink_chk", "readlink"), ("__recvfrom_chk", "recvfrom")] {
        reg_ret(n, move |c| {
            // (fd, buf, count, buflen) -> read(fd, buf, count)
            let r = host_call(host_sym(host), &[c.x[0], c.x[1], c.x[2], c.x[4], c.x[5]], &[]).0;
            c.x[0] = r;
        });
    }
    reg_ret("__fgets_chk", |c| {
        c.x[0] = host_call(host_sym("fgets"), &[c.x[0], c.x[1], c.x[3]], &[]).0;
    });
    reg_ret("__fread_chk", |c| {
        c.x[0] = host_call(host_sym("fread"), &[c.x[0], c.x[2], c.x[3], c.x[4]], &[]).0;
    });
    reg_ret("__fwrite_chk", |c| {
        c.x[0] = host_call(host_sym("fwrite"), &[c.x[0], c.x[2], c.x[3], c.x[4]], &[]).0;
    });
    reg_ret("__getcwd_chk", |c| {
        c.x[0] = host_call(host_sym("getcwd"), &[c.x[0], c.x[1]], &[]).0;
    });

    // --- long double (binary128 en arm64, x87 en x86-64): se calcula en double (con perdida) ----------
    for (g, h) in [
        ("sqrtl", "sqrt"), ("fabsl", "fabs"), ("floorl", "floor"), ("ceill", "ceil"), ("truncl", "trunc"), ("roundl", "round"),
        ("rintl", "rint"), ("nearbyintl", "nearbyint"), ("expl", "exp"), ("exp2l", "exp2"), ("logl", "log"), ("log2l", "log2"),
        ("log10l", "log10"), ("sinl", "sin"), ("cosl", "cos"), ("tanl", "tan"), ("asinl", "asin"), ("acosl", "acos"), ("atanl", "atan"),
        ("sinhl", "sinh"), ("coshl", "cosh"), ("tanhl", "tanh"), ("cbrtl", "cbrt"), ("expm1l", "expm1"), ("log1pl", "log1p"),
    ] {
        ld_unary(g, h);
    }
    for (g, h) in [("powl", "pow"), ("atan2l", "atan2"), ("fmodl", "fmod"), ("hypotl", "hypot"), ("fmaxl", "fmax"), ("fminl", "fmin"), ("copysignl", "copysign"), ("remainderl", "remainder")] {
        ld_binary(g, h);
    }
    reg_ret("ldexpl", |c| {
        let a = fmt::quad_to_f64(c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64));
        let (_, x) = host_call(host_sym("ldexp"), &[c.x[0]], &[a.to_bits()]);
        let r = fmt::f64_to_quad(f64::from_bits(x));
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
    alias("scalbnl", "ldexpl");
    // texto -> long double: se convierte en double (con perdida) y se devuelve como binary128. La variante _l
    // ignora la configuracion regional (como la de bionic, que solo tiene "C").
    for (g, h) in [("strtold", "strtod"), ("strtold_l", "strtod"), ("wcstold", "wcstod"), ("wcstold_l", "wcstod")] {
        reg_ret(g, move |c| {
            let (_, x) = host_call(host_sym(h), &[c.x[0], c.x[1]], &[]);
            let r = fmt::f64_to_quad(f64::from_bits(x));
            c.v[0] = [r as u64, (r >> 64) as u64];
        });
    }

    // --- mas libm long double y numeros complejos (arm64 los pasa en registros vectoriales distintos a SysV) ----
    for (g, h) in [("asinhl", "asinh"), ("acoshl", "acosh"), ("atanhl", "atanh"), ("erfl", "erf"), ("erfcl", "erfc"), ("lgammal", "lgamma"), ("tgammal", "tgamma")] {
        ld_unary(g, h);
    }
    for (g, h) in [("fdiml", "fdim"), ("nextafterl", "nextafter"), ("nexttowardl", "nextafter")] {
        ld_binary(g, h);
    }
    reg_ret("fmal", |c| {
        let q = |i: usize| fmt::quad_to_f64(c.v[i][0] as u128 | ((c.v[i][1] as u128) << 64));
        let r = fmt::f64_to_quad(q(0).mul_add(q(1), q(2)));
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
    // long double -> entero
    for (g, h) in [("lroundl", "lround"), ("llroundl", "llround"), ("lrintl", "lrint"), ("llrintl", "llrint"), ("ilogbl", "ilogb")] {
        reg_ret(g, move |c| {
            let d = fmt::quad_to_f64(c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64));
            let (r, _) = host_call(host_sym(h), &[], &[d.to_bits()]);
            c.x[0] = if g == "ilogbl" { r as i32 as i64 as u64 } else { r };
        });
    }
    reg_ret("frexpl", |c| {
        let d = fmt::quad_to_f64(c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64));
        let (_, x) = host_call(host_sym("frexp"), &[c.x[0]], &[d.to_bits()]);
        let r = fmt::f64_to_quad(f64::from_bits(x));
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
    reg_ret("modfl", |c| {
        let d = fmt::quad_to_f64(c.v[0][0] as u128 | ((c.v[0][1] as u128) << 64));
        let mut ip = 0f64;
        let (_, x) = host_call(host_sym("modf"), &[&mut ip as *mut f64 as u64], &[d.to_bits()]);
        let r = fmt::f64_to_quad(f64::from_bits(x));
        let i = fmt::f64_to_quad(ip);
        unsafe { *(c.x[0] as *mut u128) = i };
        c.v[0] = [r as u64, (r >> 64) as u64];
    });
    reg_complex();

    // --- datos ---------------------------------------------------------------------------------------
    data_cell("__stack_chk_guard", &0x5a5a_5a5a_5a5a_5a00u64.to_le_bytes());
    let mut ct = [0u8; 257];
    for i in 0..256usize {
        let ch = i as u8;
        let mut f = 0u8;
        if ch.is_ascii_uppercase() {
            f |= 0x01;
        }
        if ch.is_ascii_lowercase() {
            f |= 0x02;
        }
        if ch.is_ascii_digit() {
            f |= 0x04;
        }
        if matches!(ch, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            f |= 0x08;
        }
        if ch.is_ascii_punctuation() {
            f |= 0x10;
        }
        if ch.is_ascii_control() {
            f |= 0x20;
        }
        if ch.is_ascii_hexdigit() {
            f |= 0x40;
        }
        if ch == b' ' {
            f |= 0x80;
        }
        ct[i + 1] = f;
    }
    data_cell("_ctype_", &ct);
    let mut lo = Vec::new();
    let mut up = Vec::new();
    for i in -1i32..256 {
        let l = if (65..=90).contains(&i) { i + 32 } else { i };
        let u = if (97..=122).contains(&i) { i - 32 } else { i };
        lo.extend_from_slice(&(l as i16).to_le_bytes());
        up.extend_from_slice(&(u as i16).to_le_bytes());
    }
    data_cell("_tolower_tab_", &lo);
    data_cell("_toupper_tab_", &up);
}

thread_local! {
    static CLEANUP: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

/// Valor que ve el guest de la entrada `t` del vector auxiliar (`host`: la del proceso), o None si no la ve. Lo
/// comparten `getauxval` y `/proc/self/auxv` (`procemu`).
pub fn guest_auxv_entry(t: u64, host: u64) -> Option<u64> {
    Some(match t {
        // AT_HWCAP/AT_HWCAP2 derivados de los registros de identificacion (`feat`); AT_HWCAP3/4 vacios
        16 => crate::feat::hwcap(),
        26 => crate::feat::hwcap2(),
        29 | 30 => return None,
        6 => crate::mem::guest_page(), // AT_PAGESZ
        15 => b"aarch64\0".as_ptr() as u64,
        25 => random16(),
        17 => 100,
        23 => 0,
        33 => return None, // AT_SYSINFO_EHDR: el vDSO del host es x86, no debe verlo el guest
        _ => host,
    })
}

fn random16() -> u64 {
    // AT_RANDOM: 16 bytes aleatorios en una direccion estable (una sola copia para todo el proceso)
    static BUF: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    BUF.get_or_init(|| {
        let mut b = [0u8; 16];
        unsafe { getrandom(b.as_mut_ptr() as *mut c_void, 16, 0) };
        b
    })
    .as_ptr() as u64
}

#[allow(dead_code)]
fn _unused() {
    let _ = dup;
    let _ = fflush;
}


#[cfg(test)]
mod tests_wrap {
    use super::*;

    #[test]
    fn sin_doble_envoltorio() {
        INIT.call_once(register_all);
        let guest = 0x7000_1230u64;
        let t = crate::cbthunk::make(guest, b'I', 1);
        assert!(crate::cbthunk::is_thunk(t));
        assert_eq!(crate::cbthunk::guest_of(t), Some(guest));
        // el host nunca recibe un trampolin envuelto otra vez, ni el guest un trampolin x86
        assert_eq!(to_host_callable(t, b'I', 1), t);
        assert_eq!(crate::boundary::host_fn_to_guest(t), guest);
        // un slot HLE nunca se entrega crudo al host
        // el de una funcion que solo se reenvia equivale a la funcion del host: el host recibe la real
        let slot = resolve("strlen").unwrap();
        assert!(hle::is_hle(slot));
        let h = to_host_callable(slot, b'J', 0);
        assert_eq!(h, HostFn::symbol("strlen").unwrap().addr() as u64);
        // el de una implementacion propia (HLE) va por un trampolin que ejecuta el slot en el guest
        let slot = resolve("pthread_create").unwrap();
        let h = to_host_callable(slot, b'I', 0);
        assert_ne!(h, slot);
        assert!(crate::cbthunk::is_thunk(h));
        // un entero cualquiera NO se convierte en "funcion del host" (no se crea un slot que salte ahi)
        assert_eq!(crate::boundary::host_fn_to_guest(0x1234_5000), 0x1234_5000);
        assert!(HostFn::from_host(0x1234_5000).is_none());
        // enteros arbitrarios no son envoltorios ni trampolines
        assert!(!crate::jni::is_guest_wrapper(0x30));
        assert!(!crate::cbthunk::is_thunk(0x30));
        assert_eq!(to_host_callable(0, b'V', 0), 0);
    }

    fn guest_page(words: &[u32]) -> u64 {
        use crate::sys::*;
        let p = unsafe { mmap(std::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        assert!(p != MAP_FAILED);
        for (i, w) in words.iter().enumerate() {
            unsafe { *((p as *mut u32).add(i)) = *w };
        }
        unsafe { mprotect(p, 4096, PROT_READ) };
        // como si el guest la hubiera pedido ejecutable (mmap/mprotect con PROT_EXEC)
        crate::mem::guest_prot(p as u64, 4096, true);
        p as u64
    }

    #[test]
    fn host_llama_a_funcion_guest_por_fallo_de_ejecucion() {
        INIT.call_once(register_all);
        crate::sig::install_fault_handlers();
        // add x0, x0, #1 ; ret  (pagina solo-lectura: el host no puede ejecutarla)
        let code = guest_page(&[0x9100_0400, 0xd65f_03c0]);
        let f: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(code) };
        assert_eq!(f(41), 42);
        assert_eq!(f(9), 10);
    }

    extern "C" fn host_suma(a: u64, b: u64) -> u64 {
        a * 100 + b
    }

    #[test]
    fn guest_no_ejecuta_codigo_del_host() {
        INIT.call_once(register_all);
        // una direccion cruda del host se reconoce como codigo del host: el JIT no la traduce ni la ejecuta
        let h = host_suma as *const () as usize as u64;
        assert!(crate::boundary::is_host_code(h));
        assert!(!crate::boundary::is_guest_executable(h));
        // codigo guest: no es del host y si es ejecutable por el guest solo si es de un modulo o region declarada
        let code = guest_page(&[0xd65f_03c0]);
        assert!(!crate::boundary::is_host_code(code));
    }

    #[test]
    fn complejos_double() {
        let z = cexp_((0.0, std::f64::consts::PI));
        assert!((z.0 + 1.0).abs() < 1e-12 && z.1.abs() < 1e-12);
        let r = csqrt_((-4.0, 0.0));
        assert!(r.0.abs() < 1e-12 && (r.1 - 2.0).abs() < 1e-12);
        let p = cpow_((0.0, 1.0), (2.0, 0.0)); // i^2 = -1
        assert!((p.0 + 1.0).abs() < 1e-12 && p.1.abs() < 1e-12);
    }

    #[test]
    fn scanf_cuenta_destinos() {
        use crate::ldio::scanf_nargs;
        assert_eq!(scanf_nargs(b"%d %s"), 2);
        assert_eq!(scanf_nargs(b"%*d %f %%"), 1);
        assert_eq!(scanf_nargs(b"%[^,]%ld %5c"), 3);
    }

    #[test]
    fn vsscanf_con_va_list_aapcs64() {
        INIT.call_once(register_all);
        // va_list AAPCS64 con todo en pila (gr_offs = 0): { stack, gr_top, vr_top, gr_offs, vr_offs }
        let mut a: i32 = 0;
        let mut b = [0u8; 16];
        let args: [u64; 2] = [&mut a as *mut i32 as u64, b.as_mut_ptr() as u64];
        let va: [u64; 4] = [args.as_ptr() as u64, 0, 0, 0];
        let (s, f) = (b"12 hola\0", b"%d %s\0");
        let mut cpu = crate::cpu::Cpu::new();
        cpu.x[0] = s.as_ptr() as u64;
        cpu.x[1] = f.as_ptr() as u64;
        cpu.x[2] = va.as_ptr() as u64;
        cpu.pc = resolve("vsscanf").unwrap();
        hle::dispatch(&mut cpu);
        assert_eq!(cpu.x[0] as i32, 2);
        assert_eq!(a, 12);
        assert_eq!(&b[..4], b"hola");
    }

    #[test]
    fn fenv_opera_sobre_el_estado_guest_y_no_desborda() {
        INIT.call_once(register_all);
        let call = |c: &mut crate::cpu::Cpu, n: &str| {
            c.pc = resolve(n).unwrap();
            hle::dispatch(c);
        };
        let mut c = crate::cpu::Cpu::new();
        // fesetround(FE_TOWARDZERO = 3) cambia FPCR[23:22] del guest
        c.x[0] = 3;
        call(&mut c, "fesetround");
        assert_eq!((c.fpcr >> 22) & 3, 3);
        call(&mut c, "fegetround");
        assert_eq!(c.x[0], 3);
        // fegetenv escribe exactamente 8 bytes (fenv_t de arm64), no los 32 del host
        let mut buf = [0xAAu8; 40];
        c.x[0] = buf.as_mut_ptr() as u64;
        call(&mut c, "fegetenv");
        assert!(buf[8..].iter().all(|&b| b == 0xAA), "fegetenv escribio fuera del fenv_t guest");
        // excepciones
        c.x[0] = 0x10;
        call(&mut c, "feraiseexcept");
        c.x[0] = 0x9f;
        call(&mut c, "fetestexcept");
        assert_eq!(c.x[0] & 0x10, 0x10);
        // no se reenvian al host
        assert!(hle::addr_of("fegetenv").is_some() && hle::addr_of("feholdexcept").is_some());
    }

    #[test]
    fn sigaltstack_se_emula_sin_tocar_el_hilo_host() {
        INIT.call_once(register_all);
        std::thread::spawn(|| {
            let mut c = crate::cpu::Cpu::new();
            let new: [u64; 3] = [0x1234_0000, 0, 0x4000];
            let mut old: [u64; 3] = [9, 9, 9];
            c.x[0] = new.as_ptr() as u64;
            c.x[1] = old.as_mut_ptr() as u64;
            c.pc = resolve("sigaltstack").unwrap();
            hle::dispatch(&mut c);
            assert_eq!((c.x[0], old[1]), (0, 2), "al principio: SS_DISABLE");
            c.x[0] = 0;
            c.x[1] = old.as_mut_ptr() as u64;
            c.pc = resolve("sigaltstack").unwrap();
            hle::dispatch(&mut c);
            assert_eq!(old, new, "el guest ve la pila que instalo");
            // la pila alternativa real del hilo host no cambio
            extern "C" {
                fn sigaltstack(new: *const [u64; 3], old: *mut [u64; 3]) -> i32;
            }
            let mut host: [u64; 3] = [0; 3];
            unsafe { sigaltstack(std::ptr::null(), &mut host) };
            assert_ne!(host[0], 0x1234_0000);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn funciones_con_abi_incompatible_no_se_reenvian() {
        INIT.call_once(register_all);
        // con implementacion propia
        for n in ["sqrtl", "strtold", "strtold_l", "wcstold", "mallinfo", "cabs", "conjf", "fesetround", "vfork", "sigaltstack"] {
            assert!(hle::addr_of(n).is_some(), "{} deberia tener implementacion HLE", n);
        }
        // sin implementacion: no disponibles (nunca un reenvio universal)
        for n in ["qecvt", "clone", "fopencookie", "getcontext"] {
            assert!(resolve(n).is_none(), "{} no debe reenviarse", n);
        }
        // long double sin aproximacion propia: o lo implementa la biblioteca guest (codigo ARM) o no existe
        match resolve("nexttoward") {
            None => assert!(!crate::guestlib::available()),
            Some(a) => assert!(crate::guestlib::available() && !hle::is_hle(a), "nexttoward no debe reenviarse al host"),
        }
        // retorno de estructura por x8: el host la escribe donde dice x8, no donde apunte x0
        let mut out = [0xAAu8; 96];
        let mut c = crate::cpu::Cpu::new();
        c.x[0] = 0; // si se reenviara, el host escribiria en la direccion 0
        c.x[8] = out.as_mut_ptr() as u64;
        c.pc = resolve("mallinfo").unwrap();
        hle::dispatch(&mut c);
        assert!(out[80..].iter().all(|&b| b == 0xAA), "mallinfo escribio mas de 80 bytes");
    }

    /// Huecos de cobertura: funciones del NDK que no se pueden reenviar (ABI, a mano, sin firma) y que tampoco
    /// tienen implementacion propia. cargo test --release --lib huecos -- --ignored --nocapture
    #[test]
    #[ignore]
    fn huecos_de_cobertura() {
        INIT.call_once(register_all);
        use crate::sigs::K;
        let mut by: std::collections::BTreeMap<String, Vec<&str>> = std::collections::BTreeMap::new();
        let (mut direct, mut hle_n) = (0, 0);
        for s in crate::sigs::SIGS {
            let has = hle::addr_of(s.name).is_some() || DENY.contains(&s.name);
            match s.k {
                K::Direct { .. } => direct += 1,
                _ if has => hle_n += 1,
                K::Unsafe(_) => by.entry(format!("{} ABI", s.lib)).or_default().push(s.name),
                K::Manual(_) => by.entry(format!("{} a mano", s.lib)).or_default().push(s.name),
                K::NoSig => by.entry(format!("{} sin firma", s.lib)).or_default().push(s.name),
            }
        }
        println!("directas={} no directas con implementacion propia={} huecos={}", direct, hle_n, by.values().map(|v| v.len()).sum::<usize>());
        for (k, v) in by {
            println!("{} ({}): {}", k, v.len(), v.join(" "));
        }
    }

    #[test]
    fn cadenas_internas_no_se_duplican() {
        let a = intern_cstr("/data/app/x/libfoo.so");
        let b = intern_cstr("/data/app/x/libfoo.so");
        assert_eq!(a, b);
        assert_ne!(a, intern_cstr("otra"));
    }

    #[test]
    fn hilos_guest_liberan_su_memoria() {
        // Crear y terminar muchos hilos guest no debe dejar nada reservado: ni bufer de JIT, ni pila, ni TLS.
        // (sin Drop, 200 hilos dejaban 6,8 GB de espacio virtual). Otras pruebas corren en paralelo: se tolera el
        // equivalente a unos pocos hilos vivos.
        use std::sync::atomic::Ordering::Relaxed;
        let vm_kb = || crate::mem::process_kb().0;
        let run = || {
            std::thread::spawn(|| {
                let t = rt::ensure_thread(256 << 10);
                let _ = t;
            })
            .join()
            .unwrap();
        };
        for _ in 0..4 {
            run();
        }
        let (v0, j0, s0) = (vm_kb(), crate::mem::JIT_BYTES.load(Relaxed), crate::mem::STACK_BYTES.load(Relaxed));
        for _ in 0..200 {
            run();
        }
        let (v1, j1, s1) = (vm_kb(), crate::mem::JIT_BYTES.load(Relaxed), crate::mem::STACK_BYTES.load(Relaxed));
        assert!(v1 < v0 + 512 * 1024, "tamano virtual crece: {} -> {} KB", v0, v1);
        assert!(j1 <= j0 + (16 * 32 << 20), "bufers de JIT sin liberar: {} -> {}", j0, j1);
        assert!(s1 <= s0 + (16 * 8 << 20), "pilas sin liberar: {} -> {}", s0, s1);
    }

    /// La llamada al sistema `exit` del guest termina el hilo sin ejecutar codigo guest (destructores de clave, de
    /// thread_local, cadena de cleanup) y suelta su estado (pila guest dada de baja); pthread_join recibe NULL.
    #[test]
    fn exit_directo_no_ejecuta_codigo_guest_y_libera_el_hilo() {
        use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::SeqCst};
        static RAN: AtomicU32 = AtomicU32::new(0);
        static KEY: AtomicU64 = AtomicU64::new(0);
        static mut CL: [u64; 3] = [0; 3];
        extern "C" {
            fn pthread_join(t: u64, r: *mut *mut c_void) -> i32;
        }
        hle::init();
        let slot = hle::register("prueba_exit_dtor", Box::new(|_c| {
            RAN.fetch_add(1, SeqCst);
            Ret::Return
        }));
        // "C-unwind": en glibc pthread_exit desenrolla (en el puente real lo corta el primer marco del JIT, sin
        // informacion de desenrollado)
        extern "C-unwind" fn body(p: *mut c_void) -> *mut c_void {
            let slot = p as u64;
            rt::ensure_thread(256 << 10);
            assert!(rt::guest_stack_of(unsafe { pthread_self() }).is_some());
            let mut k = 0u32;
            unsafe { pthread_key_create(&mut k, std::ptr::null()) };
            KEY.store(k as u64, SeqCst);
            crate::monitor::lock(&KEYS).push((k, slot));
            unsafe { pthread_setspecific(k, 0x1234 as *const c_void) };
            THREAD_ATEXIT.with(|t| t.borrow_mut().push((slot, 1, 0)));
            unsafe {
                let cl = std::ptr::addr_of_mut!(CL) as *mut u64;
                *cl.add(1) = slot;
                *cl.add(2) = 2;
                CLEANUP.with(|h| h.set(cl as u64));
            }
            let mut c = Cpu::new();
            syscall::translate(&mut c, 93, [3, 0, 0, 0, 0, 0]);
            unreachable!("exit volvio");
        }
        let mut th = 0u64;
        let mut ret = 1 as *mut c_void;
        unsafe {
            let f: extern "C" fn(*mut c_void) -> *mut c_void = std::mem::transmute(body as extern "C-unwind" fn(*mut c_void) -> *mut c_void);
            assert_eq!(pthread_create(&mut th, std::ptr::null(), f, slot as *mut c_void), 0);
            assert_eq!(pthread_join(th, &mut ret), 0);
        }
        let k = KEY.load(SeqCst) as u32;
        crate::monitor::lock(&KEYS).retain(|e| e.0 != k);
        unsafe { pthread_key_delete(k) };
        assert_eq!(RAN.load(SeqCst), 0, "se ejecuto codigo guest al salir con exit");
        assert!(ret.is_null(), "pthread_join debe recibir NULL");
        assert_eq!(rt::guest_stack_of(th), None, "el estado guest del hilo no se libero");
    }

    #[test]
    fn pila_guest_para_pthread_getattr_np() {
        let t = rt::ensure_thread(256 << 10);
        let (lo, hi) = unsafe { ((*t).stack_lo, (*t).stack_hi) };
        let me = unsafe { pthread_self() };
        assert_eq!(rt::guest_stack_of(me), Some((lo, hi)));
        assert_eq!(rt::guest_stack_of(1), None);
    }
}


// -------------------------------------------------------------------------------------------------
// numeros complejos: double complex = (d0, d1), float complex = (s0, s1) en arm64; en SysV el float complex va
// empaquetado en un solo xmm y el retorno complejo usa xmm0:xmm1. Se calculan aqui, sin pasar por el host.
// -------------------------------------------------------------------------------------------------
type C64 = (f64, f64);

fn cmul(a: C64, b: C64) -> C64 {
    (a.0 * b.0 - a.1 * b.1, a.0 * b.1 + a.1 * b.0)
}

fn cdiv(a: C64, b: C64) -> C64 {
    let d = b.0 * b.0 + b.1 * b.1;
    ((a.0 * b.0 + a.1 * b.1) / d, (a.1 * b.0 - a.0 * b.1) / d)
}

fn cexp_(z: C64) -> C64 {
    let e = z.0.exp();
    (e * z.1.cos(), e * z.1.sin())
}

fn clog_(z: C64) -> C64 {
    (z.0.hypot(z.1).ln(), z.1.atan2(z.0))
}

fn csqrt_(z: C64) -> C64 {
    let m = z.0.hypot(z.1);
    let re = ((m + z.0) / 2.0).sqrt();
    let im = ((m - z.0) / 2.0).sqrt();
    (re, if z.1.is_sign_negative() { -im } else { im })
}

fn csin_(z: C64) -> C64 {
    (z.0.sin() * z.1.cosh(), z.0.cos() * z.1.sinh())
}

fn ccos_(z: C64) -> C64 {
    (z.0.cos() * z.1.cosh(), -(z.0.sin() * z.1.sinh()))
}

fn csinh_(z: C64) -> C64 {
    (z.0.sinh() * z.1.cos(), z.0.cosh() * z.1.sin())
}

fn ccosh_(z: C64) -> C64 {
    (z.0.cosh() * z.1.cos(), z.0.sinh() * z.1.sin())
}

fn cpow_(a: C64, b: C64) -> C64 {
    if a.0 == 0.0 && a.1 == 0.0 {
        return if b.0 == 0.0 && b.1 == 0.0 { (1.0, 0.0) } else { (0.0, 0.0) };
    }
    cexp_(cmul(b, clog_(a)))
}

fn reg_complex() {
    // (nombre base, f: complejo -> complejo)
    let unary: [(&str, fn(C64) -> C64); 10] = [
        ("cexp", cexp_), ("clog", clog_), ("csqrt", csqrt_), ("csin", csin_), ("ccos", ccos_), ("csinh", csinh_), ("ccosh", ccosh_),
        ("conj", |z| (z.0, -z.1)), ("cproj", |z| if z.0.is_infinite() || z.1.is_infinite() { (f64::INFINITY, 0f64.copysign(z.1)) } else { z }),
        ("ctan", |z| cdiv(csin_(z), ccos_(z))),
    ];
    for (n, f) in unary {
        reg_ret(n, move |c| {
            let r = f((f64::from_bits(c.v[0][0]), f64::from_bits(c.v[1][0])));
            c.v[0] = [r.0.to_bits(), 0];
            c.v[1] = [r.1.to_bits(), 0];
        });
        let nf = format!("{}f", n);
        reg_ret(&nf, move |c| {
            let z = (f32::from_bits(c.v[0][0] as u32) as f64, f32::from_bits(c.v[1][0] as u32) as f64);
            let r = f(z);
            c.v[0] = [(r.0 as f32).to_bits() as u64, 0];
            c.v[1] = [(r.1 as f32).to_bits() as u64, 0];
        });
    }
    reg_ret("cabs", |c| c.v[0] = [f64::from_bits(c.v[0][0]).hypot(f64::from_bits(c.v[1][0])).to_bits(), 0]);
    reg_ret("cabsf", |c| {
        let r = (f32::from_bits(c.v[0][0] as u32) as f64).hypot(f32::from_bits(c.v[1][0] as u32) as f64) as f32;
        c.v[0] = [r.to_bits() as u64, 0];
    });
    reg_ret("carg", |c| c.v[0] = [f64::from_bits(c.v[1][0]).atan2(f64::from_bits(c.v[0][0])).to_bits(), 0]);
    reg_ret("cargf", |c| {
        let r = (f32::from_bits(c.v[1][0] as u32) as f64).atan2(f32::from_bits(c.v[0][0] as u32) as f64) as f32;
        c.v[0] = [r.to_bits() as u64, 0];
    });
    reg_ret("creal", |c| c.v[0] = [c.v[0][0], 0]);
    reg_ret("cimag", |c| c.v[0] = [c.v[1][0], 0]);
    reg_ret("crealf", |c| c.v[0] = [c.v[0][0] & 0xffff_ffff, 0]);
    reg_ret("cimagf", |c| c.v[0] = [c.v[1][0] & 0xffff_ffff, 0]);
    reg_ret("cpow", |c| {
        let r = cpow_((f64::from_bits(c.v[0][0]), f64::from_bits(c.v[1][0])), (f64::from_bits(c.v[2][0]), f64::from_bits(c.v[3][0])));
        c.v[0] = [r.0.to_bits(), 0];
        c.v[1] = [r.1.to_bits(), 0];
    });
    reg_ret("cpowf", |c| {
        let g = |i: usize| f32::from_bits(c.v[i][0] as u32) as f64;
        let r = cpow_((g(0), g(1)), (g(2), g(3)));
        c.v[0] = [(r.0 as f32).to_bits() as u64, 0];
        c.v[1] = [(r.1 as f32).to_bits() as u64, 0];
    });
}

/// Mascaras de senales desde el guest. En Android, ART bloquea SIGUSR1 en el zygote y todos los hilos de la app la
/// heredan bloqueada: una senal enviada con pthread_kill queda pendiente hasta que el propio hilo la desbloquea con
/// pthread_sigmask, y entonces se entrega. Esta prueba reproduce ese escenario con la HLE (sin ART) y comprueba
/// ademas la convencion de retorno de cada variante (pthread_sigmask: errno; sigprocmask/sigprocmask64: -1 y errno).
#[cfg(test)]
mod tests_mascara {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    const SIG: i32 = 12; // SIGUSR2: ninguna otra prueba la usa
    static RECIBIDAS: AtomicU32 = AtomicU32::new(0);

    #[repr(C)]
    struct SigAct {
        handler: usize,
        mask: [u64; 16],
        flags: i32,
        restorer: usize,
    }
    extern "C" {
        fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
    }
    extern "C" fn contar(_sig: i32) {
        RECIBIDAS.fetch_add(1, SeqCst);
    }

    fn llamar(nombre: &str, x: [u64; 3]) -> u64 {
        let mut c = Cpu::new();
        c.x[0] = x[0];
        c.x[1] = x[1];
        c.x[2] = x[2];
        c.x[30] = 0x1234;
        c.pc = crate::hle::addr_of(nombre).unwrap();
        crate::hle::dispatch(&mut c);
        assert_eq!(c.pc, 0x1234);
        c.x[0]
    }

    /// Campo "Sig...:" de /proc/thread-self/status como mascara de bits.
    fn estado(campo: &str) -> u64 {
        let s = std::fs::read_to_string("/proc/thread-self/status").unwrap();
        let l = s.lines().find(|l| l.starts_with(campo)).unwrap();
        u64::from_str_radix(l[campo.len()..].trim(), 16).unwrap()
    }

    #[test]
    fn senal_bloqueada_heredada_queda_pendiente_y_llega_al_desbloquear() {
        INIT.call_once(register_all);
        let mut old = SigAct { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
        let act = SigAct { handler: contar as usize, mask: [0; 16], flags: 0, restorer: 0 };
        assert_eq!(unsafe { sigaction(SIG, &act, &mut old) }, 0);
        let bit = 1u64 << (SIG - 1);
        std::thread::spawn(move || {
            // el hilo "hereda" la senal bloqueada (como un hilo de app creado por ART)
            let set = bit;
            assert_eq!(llamar("pthread_sigmask", [0 /* SIG_BLOCK */, &set as *const u64 as u64, 0]), 0);
            assert_ne!(estado("SigBlk:") & bit, 0);
            RECIBIDAS.store(0, SeqCst);
            let tid = unsafe { crate::sys::syscall(186) };
            let pid = unsafe { crate::sys::syscall(39) };
            assert_eq!(unsafe { crate::sys::syscall(234, pid, tid, SIG as i64) }, 0); // tgkill
            assert_eq!(RECIBIDAS.load(SeqCst), 0, "con la senal bloqueada no debe ejecutarse el manejador");
            assert_ne!(estado("SigPnd:") & bit, 0, "la senal debe quedar pendiente en el hilo");
            // el guest la desbloquea: se entrega exactamente una vez, al volver la llamada
            let mut prev = 0u64;
            for n in ["pthread_sigmask64", "sigprocmask64"] {
                RECIBIDAS.store(0, SeqCst);
                assert_eq!(llamar(n, [0, &set as *const u64 as u64, 0]), 0);
                assert_eq!(unsafe { crate::sys::syscall(234, pid, tid, SIG as i64) }, 0);
                assert_eq!(llamar(n, [1 /* SIG_UNBLOCK */, &set as *const u64 as u64, &mut prev as *mut u64 as u64]), 0);
                assert_ne!(prev & bit, 0, "{}: mascara anterior", n);
                assert_eq!(RECIBIDAS.load(SeqCst), 1, "{}: entrega al desbloquear", n);
                assert_eq!(estado("SigPnd:") & bit, 0);
            }
            // convencion de retorno con how invalido
            assert_eq!(llamar("pthread_sigmask", [7, &set as *const u64 as u64, 0]), 22);
            assert_eq!(llamar("pthread_sigmask64", [7, &set as *const u64 as u64, 0]), 22);
            for n in ["sigprocmask", "sigprocmask64"] {
                set_errno(0);
                assert_eq!(llamar(n, [7, &set as *const u64 as u64, 0]), u64::MAX, "{}", n);
                assert_eq!(unsafe { *__errno_location() }, 22, "{}", n);
            }
        })
        .join()
        .unwrap();
        assert_eq!(unsafe { sigaction(SIG, &old, std::ptr::null_mut()) }, 0);
    }
}

/// Ejecuta `f` con la lista de claves tomada (pruebas de las secciones criticas).
#[cfg(test)]
pub fn with_keys_locked<R>(f: impl FnOnce() -> R) -> R {
    let _g = crate::monitor::lock(&KEYS);
    f()
}
