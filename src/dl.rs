//! dl* del guest (libdl de bionic y `__loader_*` del enlazador): dlopen/android_dlopen_ext, dlsym/dlvsym, dlclose,
//! dlerror, dladdr, dl_iterate_phdr y el TLS dinamico (`__tls_get_addr`, ruta lenta del resolutor TLSDESC). La carga
//! y la busqueda de simbolos estan en `elf.rs`, los espacios de nombres en `namespace.rs` y el TLS en `tls.rs`.

use crate::cpu::Cpu;
use crate::elf::{self, Dep, Ver};
use crate::libc_hle::{cstr, dbg_log, intern_cstr, reg_ret};
use crate::{hle, rt};
use std::cell::RefCell;
use std::ffi::CString;

/// handle de `dlopen(NULL)`: el ejecutable (bionic `somain`)
pub const H_MAIN: u64 = 1;
/// `RTLD_NEXT` en LP64
const H_NEXT: u64 = u64::MAX;

thread_local! {
    static DLERR: RefCell<Option<CString>> = const { RefCell::new(None) };
    static DLERR_OUT: RefCell<Option<CString>> = const { RefCell::new(None) };
}

pub fn set_error(s: &str) {
    DLERR.with(|d| *d.borrow_mut() = CString::new(s).ok());
}

/// bionic `symbol_display_name`: `nombre` o `nombre@version`.
fn display(name: &str, ver: Option<&str>) -> String {
    match ver {
        Some(v) => format!("{}@{}", name, v),
        None => name.to_string(),
    }
}

/// `do_dlsym` de bionic: Err = texto de dlerror. `ver`: la version de `dlvsym`.
pub fn lookup(h: u64, name: &str, ver: Option<&str>, lr: u64) -> Result<u64, String> {
    let vi = ver.map(Ver::new);
    let r = match h {
        0 | H_NEXT => {
            let (_, caller) = elf::caller_of(lr);
            elf::dlsym_default(name, caller.as_ref(), h == H_NEXT, vi)
        }
        H_MAIN => elf::dlsym_main(name, vi),
        _ => match elf::sys_lib_of_handle(h) {
            // biblioteca del sistema: solo lo que exporta ella
            Some(l) => elf::dlsym_sys(l, name, vi),
            None => match elf::module_by_handle(h) {
                Some(m) => elf::dlsym_handle(&m, name, vi),
                None => return Err(format!("dlsym failed: invalid handle: {:#x}", h)),
            },
        },
    };
    r.ok_or_else(|| format!("undefined symbol: {}", display(name, ver)))
}

/// `do_dlopen` del guest: handle o el texto de dlerror (sin "dlopen failed: ").
fn open(c: &Cpu, name: u64, flags: u64, extinfo: u64, caller_pc: u64) -> Result<u64, String> {
    if name == 0 {
        return Ok(H_MAIN);
    }
    let nm = cstr(name);
    // espacio del llamador (bionic `get_caller_namespace`); ANDROID_DLEXT_USE_NAMESPACE lo cambia en open_lib
    let (ns, caller) = elf::caller_of(caller_pc);
    let _ = c;
    let ext = if extinfo != 0 { Some(unsafe { elf::ExtInfo::read(extinfo) }) } else { None };
    let d = elf::open_lib(&nm, flags, ns, caller, ext.as_ref())?;
    dbg_log(&format!("guest dlopen('{}') en {} -> {}", nm, crate::namespace::name(ns), if let Dep::M(_) = d { "modulo guest" } else { "biblioteca del sistema" }));
    Ok(match d {
        Dep::M(m) => m.handle,
        Dep::Sys(l) => elf::sys_handle(l),
        Dep::Exe => H_MAIN,
    })
}

pub fn register() {
    // dlopen(name, flags), android_dlopen_ext(name, flags, extinfo), __loader_dlopen(name, flags, caller)
    for n in ["dlopen", "android_dlopen_ext", "__loader_dlopen"] {
        reg_ret(n, move |c| {
            let (ext, caller) = match n {
                "android_dlopen_ext" => (c.x[2], c.x[30]),
                "__loader_dlopen" => (0, c.x[2]),
                _ => (0, c.x[30]),
            };
            c.x[0] = match open(c, c.x[0], c.x[1], ext, caller) {
                Ok(h) => h,
                Err(e) => {
                    dbg_log(&format!("guest dlopen('{}') fallo: {}", cstr(c.x[0]), e));
                    set_error(&format!("dlopen failed: {}", e));
                    0
                }
            };
        });
    }
    // dlsym(handle, name) y dlvsym(handle, name, version)
    for n in ["dlsym", "dlvsym"] {
        reg_ret(n, move |c| {
            let (h, np) = (c.x[0], c.x[1]);
            if np == 0 {
                set_error("dlsym failed: symbol name is null");
                c.x[0] = 0;
                return;
            }
            let name = cstr(np);
            let ver = (n == "dlvsym" && c.x[2] != 0).then(|| cstr(c.x[2]));
            c.x[0] = match lookup(h, &name, ver.as_deref(), c.x[30]) {
                Ok(a) => a,
                Err(e) => {
                    dbg_log(&format!("guest dlsym('{}'): {}", name, e));
                    set_error(&e);
                    0
                }
            };
        });
    }
    reg_ret("dlclose", |c| {
        let h = c.x[0];
        c.x[0] = match elf::module_by_handle(h) {
            Some(m) => {
                elf::unload(&m);
                0
            }
            None if h == H_MAIN || elf::is_sys_handle(h) => 0,
            None => {
                // como bionic: un handle desconocido o de una biblioteca ya descargada
                set_error(&format!("dlclose failed: invalid handle: {:#x}", h));
                -1i64 as u64
            }
        };
    });
    reg_ret("dlerror", |c| {
        c.x[0] = DLERR.with(|d| match d.borrow_mut().take() {
            Some(s) => {
                // el mensaje vive en el hilo hasta el siguiente dlerror (como en bionic): sin copias permanentes
                DLERR_OUT.with(|o| {
                    *o.borrow_mut() = Some(s);
                    o.borrow().as_ref().map_or(0, |m| m.as_ptr() as u64)
                })
            }
            None => 0,
        });
    });
    reg_ret("dladdr", |c| {
        let (addr, info) = (c.x[0], c.x[1] as *mut u64);
        if hle::is_hle(addr) {
            c.x[0] = sys_dladdr(addr, info);
            return;
        }
        match elf::module_for_addr(addr) {
            Some(m) => {
                let fname = intern_cstr(&m.realpath);
                unsafe {
                    *info = fname;
                    *info.add(1) = m.lo;
                    let (sn, sa) = match m.symbol_at(addr) {
                        Some((n, a)) => (intern_cstr(&n), a),
                        None => (0, 0),
                    };
                    *info.add(2) = sn;
                    *info.add(3) = sa;
                }
                c.x[0] = 1;
            }
            None => c.x[0] = 0,
        }
    });
    // el mismo valor que usa el cargador (bridge::target_sdk), tambien fuera de Android
    reg_ret("android_get_application_target_sdk_version", |c| c.x[0] = crate::bridge::target_sdk() as i64 as u64);
    reg_ret("dl_iterate_phdr", |c| {
        let (cb, data) = (c.x[0], c.x[1]);
        let mut ret = 0u64;
        for m in elf::modules() {
            let name = intern_cstr(&m.realpath);
            let mut info = [0u64; 8];
            info[0] = m.base;
            info[1] = name;
            info[2] = m.phdr_addr;
            info[3] = m.phnum as u64;
            // dlpi_adds/dlpi_subs, dlpi_tls_modid y dlpi_tls_data (el bloque de este hilo si ya existe; no lo reserva)
            info[4] = elf::LOADS.load(std::sync::atomic::Ordering::SeqCst);
            info[5] = elf::UNLOADS.load(std::sync::atomic::Ordering::SeqCst);
            info[6] = elf::tls_module_id(&m);
            info[7] = crate::tls::block_if_allocated(c.tpidr, info[6]);
            let r = rt::call_guest_on(c, cb, &[info.as_ptr() as u64, 64, data], &[]);
            ret = r.0 as i32 as i64 as u64;
            if ret != 0 {
                break;
            }
        }
        c.x[0] = ret;
    });
    reg_ret("__heddle_tlsdesc_slow", |c| {
        // tlsdesc_resolver_dynamic_slow_path: x0 = {generacion, modulo, desplazamiento}; devuelve direccion - tp
        let a = c.x[0] as *const u64;
        let (id, off) = unsafe { (*a.add(1), *a.add(2)) };
        c.x[0] = crate::tls::get_addr(c.tpidr, id, off).wrapping_sub(c.tpidr);
    });
    reg_ret("__heddle_tlsdesc_weak", |c| {
        // descriptor [resolver, sumando]: tp + resultado = sumando
        c.x[0] = unsafe { *((c.x[0] + 8) as *const u64) }.wrapping_sub(c.tpidr);
    });
    reg_ret("__tls_get_addr", |c| {
        // TlsIndex {module_id, offset}
        let ti = c.x[0] as *const u64;
        let (m, o) = unsafe { (*ti, *ti.add(1)) };
        c.x[0] = crate::tls::get_addr(c.tpidr, m, o);
    });
}

/// `dladdr` sobre una funcion del sistema (un slot HLE), como en el dispositivo: la ruta de la biblioteca que la
/// exporta (en Android, la del host, que es la misma ruta que en arm64; fuera, `elf::sys_realpath`), su base (la de
/// la biblioteca del host, legible; 0 si la funcion solo la implementa el puente) y el simbolo con la direccion que ve
/// el guest (el slot, la misma que da dlsym). Un slot interno del puente (sin simbolo exportado) da 0.
fn sys_dladdr(addr: u64, info: *mut u64) -> u64 {
    let name = hle::name_at(addr);
    let Some(slot) = hle::addr_of(&name) else { return 0 };
    let exported = !crate::sigs::exporters(&name).is_empty();
    if !exported && crate::libc_hle::resolve(&name) != Some(slot) {
        return 0;
    }
    let lib = elf::sys_lib_of_symbol(&name);
    let host = crate::boundary::host_lib_of(hle::host_twin(slot));
    let fname = match &host {
        Some((p, _)) if cfg!(target_os = "android") => p.clone(),
        _ => elf::sys_realpath(lib),
    };
    unsafe {
        *info = intern_cstr(&fname);
        *info.add(1) = host.map_or(0, |h| h.1);
        *info.add(2) = intern_cstr(&name);
        *info.add(3) = slot;
    }
    1
}
