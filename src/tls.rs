//! TLS dinamico de las bibliotecas guest, como el de bionic (libc/bionic/bionic_elf_tls.cpp, linker/linker_tls.cpp,
//! linker/arch/arm64/tlsdesc_resolver.S). Toda biblioteca guest se abre con `dlopen`, asi que en bionic todas usan
//! TLS dinamico: no hay area estatica por modulo.
//!
//! * Tabla global de modulos TLS (`TABLE`): un segmento PT_TLS por modulo cargado, con identificador desde 1
//!   (`get_unused_module_index`: se reutiliza el hueco de uno descargado) y la generacion en que se registro
//!   (`first_generation`). Registrar sube la generacion global (`GENERATION`); retirar no.
//! * DTV de cada hilo dentro de su area TLS (`rt::TLS_AREA_BYTES`), con la forma de `TlsDtv` de bionic: `count`,
//!   `next`, `generation` y un puntero por modulo. La ranura 0 del hilo (`TLS_SLOT_DTV`) apunta a su `generation`.
//!   Capacidad fija (`MAX_TLS_MODULES`): bionic la hace crecer; aqui nunca cambia de sitio.
//! * El bloque de un modulo en un hilo se reserva en su primer acceso (`get_addr`, `tls_get_addr_slow_path`) con el
//!   reservador del hilo (como `BionicAllocator`: bloques pequenos en paginas compartidas por clase de tamano, los
//!   grandes en su propia region) y lleva la imagen inicial. Se libera cuando el hilo ve una generacion nueva y el modulo ya no
//!   esta (o su hueco es de otro modulo mas nuevo) (`update_tls_dtv`) y al terminar el hilo (`free_thread`).
//! * TLSDESC: el resolutor dinamico de bionic, en codigo ARM64 (lo traduce el JIT como cualquier codigo guest): ruta
//!   rapida si la generacion del DTV es al menos la del descriptor y el bloque existe; si no, la HLE
//!   `__heddle_tlsdesc_slow` (preserva todo salvo x0, como `tlsdesc_resolver_dynamic_slow_path`).
//!   En la secuencia de llamada del ABI, el JIT emite la ruta rapida en linea (jit.rs, "TLSDESC en linea"): depende de
//!   la forma del DTV (ranura 0 -> `generation`, modulo `id` en `generation + 8 * id`), de `TlsDynamicResolverArg` y de
//!   `RESOLVER_SLOW_OFF`.
//!
//! Senales: la ruta lenta toma el bloqueo de la tabla con `monitor::lock` (una senal guest se aplaza mientras dura) y
//! puede reservar memoria (mmap). Un manejador de senal guest que toca por primera vez el TLS dinamico de un modulo
//! en su hilo lo reserva ahi mismo, como bionic (que bloquea las senales y usa su asignador).

use crate::mem::{Kind, Region};
use crate::sys::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::Mutex;

/// Modulos TLS a la vez (tope de la tabla y capacidad del DTV de cada hilo). Agotado, el `dlopen` de una biblioteca
/// con PT_TLS falla.
pub const MAX_TLS_MODULES: usize = 1024;
/// Desplazamiento del DTV dentro del area TLS del hilo (tras las ranuras de bionic).
pub const DTV_OFF: usize = crate::rt::TLS_SLOTS_BYTES;
/// Bytes del DTV: count, next, generation y los punteros de los modulos.
pub const DTV_BYTES: usize = 24 + 8 * MAX_TLS_MODULES;

#[derive(Clone, Copy)]
struct Seg {
    /// handle del modulo (0 = hueco libre)
    owner: u64,
    /// generacion en que se registro (0 = ninguna)
    first_gen: u64,
    init: u64,
    init_size: usize,
    size: usize,
    align: usize,
}

const EMPTY: Seg = Seg { owner: 0, first_gen: 0, init: 0, init_size: 0, size: 0, align: 0 };

/// Tabla de modulos TLS (bionic `TlsModules::module_table`). Cota: `MAX_TLS_MODULES` entradas fijas.
static TABLE: Mutex<[Seg; MAX_TLS_MODULES]> = Mutex::new([EMPTY; MAX_TLS_MODULES]);
/// Entradas usadas alguna vez (`module_count`): nunca baja.
static COUNT: AtomicUsize = AtomicUsize::new(0);
/// Generacion global (bionic `TlsModules::generation`, empieza en 1).
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// Registra el segmento PT_TLS de un modulo (bionic `register_tls_module`): su identificador (desde 1).
pub fn register(owner: u64, init: u64, init_size: usize, size: usize, align: usize) -> Option<usize> {
    let mut t = crate::monitor::lock(&TABLE);
    let n = COUNT.load(SeqCst);
    let idx = match (0..n).find(|&i| t[i].owner == 0) {
        Some(i) => i,
        None if n < MAX_TLS_MODULES => {
            COUNT.store(n + 1, SeqCst);
            n
        }
        None => return None,
    };
    let gen = GENERATION.fetch_add(1, SeqCst) + 1;
    t[idx] = Seg { owner, first_gen: gen, init, init_size, size, align: align.max(1).next_power_of_two() };
    Some(idx + 1)
}

/// Retira un modulo (bionic `unregister_tls_module`): su hueco queda libre; la generacion no cambia (los bloques de
/// cada hilo se liberan cuando ese hilo pasa por la ruta lenta o termina).
pub fn unregister(id: usize) {
    if id == 0 || id > MAX_TLS_MODULES {
        return;
    }
    crate::monitor::lock(&TABLE)[id - 1] = EMPTY;
}

/// Generacion global actual (la que guarda cada descriptor TLSDESC al relocar).
pub fn generation() -> u64 {
    GENERATION.load(SeqCst)
}

fn dtv(tp: u64) -> *mut u64 {
    (tp + DTV_OFF as u64) as *mut u64
}

/// Prepara el DTV de un hilo nuevo (area TLS a cero): `count`, generacion 0 (la primera ruta lenta lo pone al dia) y
/// la ranura `TLS_SLOT_DTV` apuntando a su `generation`.
pub fn init_thread(tp: u64) {
    unsafe {
        let d = dtv(tp);
        *d = MAX_TLS_MODULES as u64;
        *d.add(1) = 0;
        *d.add(2) = 0;
        *(tp as *mut u64) = d.add(2) as u64;
    }
}

// ---------------------------------------------------------------------------------------------
// Reservador de bloques por hilo (como `BionicAllocator`, bionic_allocator.cpp)
// ---------------------------------------------------------------------------------------------
//
// Bloques de hasta 1 KiB: clases de potencias de dos (16..1024), cada una con su lista de paginas compartidas de
// 4 KiB (`mem::Region` soltada con `detach`); la cabecera de la pagina va al principio y los bloques, alineados a su
// tamano, detras. Una pagina llena sale de la lista y vuelve al liberar uno de sus bloques; de las vacias se guarda
// como mucho una por clase (`free_pages_cnt`). Mayores de 1 KiB: una `Region` propia con su cabecera en la pagina
// del bloque. `memalign` como bionic: alineacion entre 16 y una pagina del guest, tamano al menos la alineacion.
// El estado (cabeza de lista y paginas vacias de cada clase) vive en el area TLS del hilo (`ALLOC_OFF`): cada hilo
// solo toca el suyo (sin bloqueos) y todo se devuelve al terminar el hilo (`free_thread`).

/// Firma de las cabeceras (bionic `kSignature`).
const SIGNATURE: u32 = u32::from_le_bytes([b'L', b'M', b'A', 1]);
/// Tipo de un bloque grande (bionic `kLargeObject`); los pequenos llevan el log2 de su clase.
const LARGE: u32 = 111;
const MIN_LOG2: usize = 4;
const MAX_LOG2: usize = 10;
const CLASSES: usize = MAX_LOG2 - MIN_LOG2 + 1;
/// Pagina de los bloques pequenos (la del host).
const APAGE: u64 = crate::mem::PAGE as u64;
/// Cabecera de una pagina de bloques pequenos: firma, tipo, siguiente, anterior, lista libre y libres.
#[repr(C)]
struct PageInfo {
    sig: u32,
    typ: u32,
    next: u64,
    prev: u64,
    free_list: u64,
    free_cnt: u64,
}
/// Cabecera de un bloque grande: firma, tipo y la `Region` (base y longitud).
#[repr(C)]
struct LargeInfo {
    sig: u32,
    typ: u32,
    base: u64,
    len: u64,
}
/// Registro de un tramo libre dentro de una pagina (bionic `small_object_block_record`).
#[repr(C)]
struct FreeRec {
    next: u64,
    cnt: u64,
}

/// Estado del reservador de un hilo: por clase, cabeza de su lista de paginas y paginas vacias. Va en la ultima
/// pagina del area TLS (la del bloque del hilo, que ya se toca al crearlo: fuera de ella seria un fallo de pagina mas
/// en el primer acceso), lejos del tid.
pub const ALLOC_OFF: usize = crate::rt::TLS_AREA_BYTES - 4096 + 2048;
const ALLOC_BYTES: usize = 16 * CLASSES;
const _: () = assert!(ALLOC_OFF + ALLOC_BYTES <= crate::rt::TLS_AREA_BYTES);

fn class_state(tp: u64, c: usize) -> *mut [u64; 2] {
    (tp + (ALLOC_OFF + 16 * c) as u64) as *mut [u64; 2]
}
fn blocks_per_page(log2: usize) -> u64 {
    (APAGE - std::mem::size_of::<PageInfo>() as u64) >> log2
}

fn list_add(st: *mut [u64; 2], page: *mut PageInfo) {
    unsafe {
        (*page).next = (*st)[0];
        (*page).prev = 0;
        if (*st)[0] != 0 {
            (*((*st)[0] as *mut PageInfo)).prev = page as u64;
        }
        (*st)[0] = page as u64;
    }
}
fn list_remove(st: *mut [u64; 2], page: *mut PageInfo) {
    unsafe {
        if (*page).prev != 0 {
            (*((*page).prev as *mut PageInfo)).next = (*page).next;
        }
        if (*page).next != 0 {
            (*((*page).next as *mut PageInfo)).prev = (*page).prev;
        }
        if (*st)[0] == page as u64 {
            (*st)[0] = (*page).next;
        }
        (*page).prev = 0;
        (*page).next = 0;
    }
}

fn small_alloc(tp: u64, log2: usize) -> Option<u64> {
    let st = class_state(tp, log2 - MIN_LOG2);
    let bs = 1u64 << log2;
    let per = blocks_per_page(log2);
    unsafe {
        if (*st)[0] == 0 {
            // alloc_page
            let r = Region::new(APAGE as usize, PROT_READ | PROT_WRITE, Kind::Other)?;
            let (b, _) = r.detach();
            let page = b as *mut PageInfo;
            let first = (b + std::mem::size_of::<PageInfo>() as u64 + bs - 1) & !(bs - 1);
            *page = PageInfo { sig: SIGNATURE, typ: log2 as u32, next: 0, prev: 0, free_list: first, free_cnt: per };
            *(first as *mut FreeRec) = FreeRec { next: 0, cnt: per };
            list_add(st, page);
            (*st)[1] += 1;
        }
        let page = (*st)[0] as *mut PageInfo;
        let rec = (*page).free_list as *mut FreeRec;
        if (*rec).cnt > 1 {
            let nx = (rec as u64 + bs) as *mut FreeRec;
            *nx = FreeRec { next: (*rec).next, cnt: (*rec).cnt - 1 };
            (*page).free_list = nx as u64;
        } else {
            (*page).free_list = (*rec).next;
        }
        if (*page).free_cnt == per {
            (*st)[1] -= 1;
        }
        (*page).free_cnt -= 1;
        std::ptr::write_bytes(rec as *mut u8, 0, bs as usize);
        if (*page).free_cnt == 0 {
            list_remove(st, page);
        }
        Some(rec as u64)
    }
}

fn free_page(st: *mut [u64; 2], page: *mut PageInfo) {
    list_remove(st, page);
    unsafe { (*st)[1] -= 1 };
    drop(unsafe { Region::reattach(page as u64, APAGE as usize, Kind::Other) });
}

fn small_free(tp: u64, page: *mut PageInfo, p: u64) {
    unsafe {
        let log2 = (*page).typ as usize;
        let (bs, per) = (1u64 << log2, blocks_per_page(log2));
        let st = class_state(tp, log2 - MIN_LOG2);
        if p % bs != 0 {
            bad_pointer(p);
        }
        std::ptr::write_bytes(p as *mut u8, 0, bs as usize);
        *(p as *mut FreeRec) = FreeRec { next: (*page).free_list, cnt: 1 };
        (*page).free_list = p;
        (*page).free_cnt += 1;
        if (*page).free_cnt == per {
            (*st)[1] += 1;
            if (*st)[1] > 1 {
                // ya hay una pagina vacia: esta se devuelve
                free_page(st, page);
            }
        } else if (*page).free_cnt == 1 {
            // estaba llena: vuelve a la lista
            list_add(st, page);
        }
    }
}

fn bad_pointer(p: u64) -> ! {
    crate::bridge::alog_fatal(&format!("FALLO TLS: puntero de bloque no valido {:#x}", p));
    unsafe { abort() }
}

/// `BionicAllocator::memalign` en el reservador del hilo de `tp` (memoria a cero).
fn memalign(tp: u64, align: usize, size: usize) -> Option<u64> {
    let align = align.clamp(16, crate::mem::guest_page() as usize).next_power_of_two();
    let size = size.max(align).max(1);
    if size <= 1 << MAX_LOG2 {
        let log2 = (size.next_power_of_two().trailing_zeros() as usize).max(MIN_LOG2);
        return small_alloc(tp, log2);
    }
    // alloc_mmap: cabecera en la pagina del bloque
    let hdr = (std::mem::size_of::<LargeInfo>() + align - 1) & !(align - 1);
    let slack = if align as u64 > APAGE { align } else { 0 };
    let r = Region::new(hdr + size + slack, PROT_READ | PROT_WRITE, Kind::Other)?;
    let data = (r.base() + hdr as u64 + align as u64 - 1) & !(align as u64 - 1);
    let (base, len) = r.detach();
    unsafe { *(((data - 16) & !(APAGE - 1)) as *mut LargeInfo) = LargeInfo { sig: SIGNATURE, typ: LARGE, base, len: len as u64 } };
    Some(data)
}

/// `BionicAllocator::free` en el reservador del hilo de `tp`.
fn free(tp: u64, p: u64) {
    if p == 0 {
        return;
    }
    let info = ((p - 16) & !(APAGE - 1)) as *mut PageInfo;
    unsafe {
        if (*info).sig != SIGNATURE {
            bad_pointer(p);
        }
        match (*info).typ {
            LARGE => {
                let l = info as *const LargeInfo;
                drop(Region::reattach((*l).base, (*l).len as usize, Kind::Other));
            }
            t if (MIN_LOG2..=MAX_LOG2).contains(&(t as usize)) => small_free(tp, info, p),
            _ => bad_pointer(p),
        }
    }
}

fn alloc_block(tp: u64, seg: &Seg) -> Option<u64> {
    let data = memalign(tp, seg.align, seg.size)?;
    if seg.init_size > 0 {
        unsafe { std::ptr::copy_nonoverlapping(seg.init as *const u8, data as *mut u8, seg.init_size) };
    }
    Some(data)
}

/// bionic `update_tls_dtv`: si la generacion del hilo es vieja, libera los bloques de modulos que ya no estan (o
/// cuyo hueco es de un modulo registrado despues) y se pone al dia. Con la tabla tomada.
fn update_dtv(t: &[Seg; MAX_TLS_MODULES], tp: u64) {
    let d = dtv(tp);
    let gen = GENERATION.load(SeqCst);
    unsafe {
        let mine = *d.add(2);
        if mine == gen {
            return;
        }
        for (i, seg) in t.iter().enumerate().take(COUNT.load(SeqCst)) {
            if seg.first_gen != 0 && seg.first_gen <= mine {
                continue;
            }
            let slot = d.add(3 + i);
            free(tp, *slot);
            *slot = 0;
        }
        *d.add(2) = gen;
    }
}

/// Direccion de `offset` dentro del bloque del modulo `id` en el hilo de `tp`, reservandolo si hace falta (bionic
/// `__tls_get_addr`). 0 si el modulo no existe o no hay memoria.
pub fn get_addr(tp: u64, id: u64, offset: u64) -> u64 {
    let idx = id as usize;
    if idx == 0 || idx > MAX_TLS_MODULES {
        return 0;
    }
    let d = dtv(tp);
    unsafe {
        // ruta rapida
        if *d.add(2) == GENERATION.load(SeqCst) {
            let p = *d.add(2 + idx);
            if p != 0 {
                return p.wrapping_add(offset);
            }
        }
        // ruta lenta (tls_get_addr_slow_path)
        let t = crate::monitor::lock(&TABLE);
        update_dtv(&t, tp);
        let slot = d.add(2 + idx);
        if *slot == 0 {
            let seg = t[idx - 1];
            if seg.owner == 0 {
                return 0;
            }
            match alloc_block(tp, &seg) {
                Some(p) => *slot = p,
                None => {
                    drop(t);
                    crate::bridge::alog_fatal(&format!("FALLO TLS: sin memoria para el bloque del modulo {}\n{}", id, crate::mem::status_line()));
                    abort()
                }
            }
        }
        (*slot).wrapping_add(offset)
    }
}

/// Bloque del modulo `id` en este hilo si ya existe y esta al dia (bionic `TLS_GET_ADDR_FAST`, para
/// `dl_iterate_phdr`): no reserva nada.
pub fn block_if_allocated(tp: u64, id: u64) -> u64 {
    let idx = id as usize;
    if idx == 0 || idx > MAX_TLS_MODULES {
        return 0;
    }
    let d = dtv(tp);
    unsafe {
        if *d.add(2) != GENERATION.load(SeqCst) {
            return 0;
        }
        *d.add(2 + idx)
    }
}

/// Libera todos los bloques del hilo y las paginas de su reservador (al terminar).
pub fn free_thread(tp: u64) {
    let d = dtv(tp);
    for i in 0..MAX_TLS_MODULES {
        unsafe {
            let slot = d.add(3 + i);
            free(tp, *slot);
            *slot = 0;
        }
    }
    // sin bloques vivos, a cada clase le queda como mucho su pagina vacia
    for c in 0..CLASSES {
        let st = class_state(tp, c);
        unsafe {
            // free_page escribe (*st)[0] por el puntero: se relee en cada vuelta
            loop {
                let page = (*st)[0] as *mut PageInfo;
                if page.is_null() {
                    break;
                }
                if (*page).free_cnt != blocks_per_page(c + MIN_LOG2) {
                    bad_pointer(page as u64); // un bloque sin DTV que lo apunte: no deberia pasar
                }
                free_page(st, page);
            }
        }
    }
}

/// Codigo ARM64 de `tlsdesc_resolver_dynamic` de bionic (x0 = descriptor; [x0+8] = TlsDynamicResolverArg
/// {generation, module_id, offset}; devuelve en x0 la direccion menos tpidr_el0 y preserva todo lo demas salvo NZCV).
/// La ruta lenta llama a la HLE `__heddle_tlsdesc_slow` con x0 = argumento y x16/x30 guardados en la pila.
const RESOLVER: [u32; 26] = [
    0xa9be53f3, // stp x19, x20, [sp, #-32]!
    0xa9015bf5, // stp x21, x22, [sp, #16]
    0xd53bd053, // mrs x19, tpidr_el0
    0xf9400274, // ldr x20, [x19]             TLS_SLOT_DTV: &dtv->generation
    0xf9400295, // ldr x21, [x20]             dtv->generation
    0xf9400400, // ldr x0, [x0, #8]           TlsDynamicResolverArg*
    0xf9400016, // ldr x22, [x0]              arg->generation
    0xeb1602bf, // cmp x21, x22
    0x54000143, // b.lo lento
    0xf9400415, // ldr x21, [x0, #8]          module_id
    0xf9400816, // ldr x22, [x0, #16]         offset
    0xf8757a95, // ldr x21, [x20, x21, lsl #3] dtv->modules[module_id - 1]
    0xb40000d5, // cbz x21, lento
    0x8b1602a0, // add x0, x21, x22
    0xcb130000, // sub x0, x0, x19
    0xa9415bf5, // ldp x21, x22, [sp, #16]
    0xa8c253f3, // ldp x19, x20, [sp], #32
    0xd65f03c0, // ret
    // lento:
    0xa9415bf5, // ldp x21, x22, [sp, #16]
    0xa8c253f3, // ldp x19, x20, [sp], #32
    0xa9bf7bf0, // stp x16, x30, [sp, #-16]!
    0x580000b0, // ldr x16, literal (+0x14)
    0xd63f0200, // blr x16
    0xa8c17bf0, // ldp x16, x30, [sp], #16
    0xd65f03c0, // ret
    0xd503201f, // nop (alinea el literal)
];

/// Desplazamiento de la llamada a la ruta lenta en `RESOLVER` (`stp x16, x30`, ya repuestos x19-x22): la ruta rapida
/// en linea del JIT sigue ahi con x0 = argumento y NZCV del `cmp`, el estado de ARM en ese punto.
pub const RESOLVER_SLOW_OFF: u64 = 20 * 4;

static RESOLVER_AT: AtomicU64 = AtomicU64::new(0);

/// Direccion guest del resolutor TLSDESC dinamico (lo crea la primera vez: una pagina permanente de solo lectura).
pub fn resolver() -> u64 {
    let r = RESOLVER_AT.load(SeqCst);
    if r != 0 {
        return r;
    }
    static ONCE: Mutex<()> = Mutex::new(());
    let _g = crate::monitor::lock(&ONCE);
    let r = RESOLVER_AT.load(SeqCst);
    if r != 0 {
        return r;
    }
    let slow = crate::libc_hle::tlsdesc_slow_path();
    let Some(base) = Region::permanent(crate::mem::PAGE, PROT_READ | PROT_WRITE) else {
        crate::bridge::alog_fatal("FALLO TLS: sin memoria para el resolutor TLSDESC");
        unsafe { abort() }
    };
    unsafe {
        let p = base as *mut u32;
        for (i, w) in RESOLVER.iter().enumerate() {
            *p.add(i) = *w;
        }
        *(p.add(RESOLVER.len()) as *mut u64) = slow;
        mprotect(base as *mut std::os::raw::c_void, crate::mem::PAGE, PROT_READ);
    }
    RESOLVER_AT.store(base, SeqCst);
    base
}

/// Direccion del resolutor si ya se creo (0 si no), sin crearlo: el JIT emite su ruta rapida en linea en las llamadas
/// TLSDESC (jit.rs, "TLSDESC en linea"; depende de la forma del DTV y de `TlsDynamicResolverArg` de este archivo).
pub fn resolver_at() -> u64 {
    RESOLVER_AT.load(SeqCst)
}

/// `a` esta en el codigo del resolutor (codigo ARM del puente que ejecuta el guest)?
pub fn is_resolver_code(a: u64) -> bool {
    let r = RESOLVER_AT.load(SeqCst);
    r != 0 && a >= r && a < r + (RESOLVER.len() * 4) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llamada_lenta_del_resolutor() {
        assert_eq!(RESOLVER[RESOLVER_SLOW_OFF as usize / 4], 0xa9bf7bf0); // stp x16, x30, [sp, #-16]!
        assert_eq!(RESOLVER[RESOLVER_SLOW_OFF as usize / 4 - 1], 0xa8c253f3); // ldp x19, x20, [sp], #32
    }

    #[test]
    fn huecos_reutilizados_y_generacion() {
        let g0 = generation();
        let a = register(0x1111, 0, 0, 16, 8).unwrap();
        assert!(generation() > g0);
        unregister(a);
        let g1 = generation();
        assert_eq!(g1, generation(), "retirar no sube la generacion");
        let b = register(0x2222, 0, 0, 16, 8).unwrap();
        assert!(b <= a.max(COUNT.load(SeqCst)));
        unregister(b);
    }

    /// Como BionicAllocator: los bloques pequenos de un hilo comparten pagina por clase de tamano (alineados a su
    /// clase, a cero tras la imagen) y las paginas se devuelven al terminar el hilo.
    #[test]
    fn bloques_pequenos_en_paginas_compartidas() {
        static INIT: [u8; 3] = [7, 8, 9];
        let a = register(0x5551, INIT.as_ptr() as u64, 3, 20, 4).unwrap() as u64;
        let b = register(0x5552, 0, 0, 30, 8).unwrap() as u64;
        let c = register(0x5553, 0, 0, 8, 64).unwrap() as u64;
        let big = register(0x5554, 0, 0, 5000, 16).unwrap() as u64;
        let run = move || {
            std::thread::spawn(move || {
                let tp = unsafe { (*crate::rt::ensure_thread(256 << 10)).tls_base };
                let (pa, pb, pc, pg) = (get_addr(tp, a, 0), get_addr(tp, b, 0), get_addr(tp, c, 0), get_addr(tp, big, 0));
                assert_eq!(pa & !4095, pb & !4095, "misma clase (32 bytes), misma pagina");
                assert_eq!((pa % 32, pb % 32, pc % 64, pg % 16), (0, 0, 0, 0));
                assert_ne!(pa & !4095, pc & !4095, "otra clase (64 bytes), otra pagina");
                unsafe {
                    assert_eq!(std::slice::from_raw_parts(pa as *const u8, 32), &[7, 8, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..]);
                    assert!(std::slice::from_raw_parts(pg as *const u8, 5000).iter().all(|&x| x == 0));
                }
            })
            .join()
            .unwrap();
        };
        for _ in 0..4 {
            run();
        }
        let v0 = crate::mem::process_kb().0;
        for _ in 0..1000 {
            run();
        }
        let v1 = crate::mem::process_kb().0;
        // sin liberar: 1000 hilos x 4 regiones (al menos 20 MiB)
        assert!(v1 < v0 + 8 * 1024, "paginas del reservador TLS sin liberar: {} -> {} KB", v0, v1);
        for id in [a, b, c, big] {
            unregister(id as usize);
        }
    }

    /// Paginas compartidas: liberar un bloque devuelve su hueco y la pagina vuelve a la lista; de las vacias se
    /// guarda una por clase.
    #[test]
    fn reservador_reutiliza_huecos() {
        std::thread::spawn(|| {
            let tp = unsafe { (*crate::rt::ensure_thread(256 << 10)).tls_base };
            let per = blocks_per_page(7) as usize; // 128 bytes
            let v: Vec<u64> = (0..per + 1).map(|_| memalign(tp, 16, 100).unwrap()).collect();
            assert_eq!(v[0] & !4095, v[per - 1] & !4095);
            assert_ne!(v[0] & !4095, v[per] & !4095, "pagina llena: la siguiente en otra");
            let hole = v[3];
            free(tp, hole);
            assert_eq!(memalign(tp, 16, 100).unwrap(), hole, "el hueco liberado se reutiliza");
            for p in v {
                free(tp, p);
            }
            let st = class_state(tp, 7 - MIN_LOG2);
            assert_eq!(unsafe { (*st)[1] }, 1, "una sola pagina vacia guardada");
        })
        .join()
        .unwrap();
    }

    #[test]
    fn bloques_por_hilo_con_imagen_y_liberados() {
        // 200 hilos con un bloque de 1 MiB cada uno: al terminar cada hilo su bloque se libera (sin eso, 200 MiB)
        static INIT: [u8; 4] = [1, 2, 3, 4];
        let id = register(0x3333, INIT.as_ptr() as u64, 4, 1 << 20, 64).unwrap() as u64;
        let vm_kb = || crate::mem::process_kb().0;
        let run = move || {
            std::thread::spawn(move || {
                let tp = unsafe { (*crate::rt::ensure_thread(256 << 10)).tls_base };
                assert_eq!(block_if_allocated(tp, id), 0);
                let a = get_addr(tp, id, 2);
                assert_eq!(unsafe { *(a as *const u8) }, 3);
                assert_eq!((a - 2) % 64, 0);
                assert_eq!(get_addr(tp, id, 2), a);
                assert_eq!(block_if_allocated(tp, id), a - 2);
            })
            .join()
            .unwrap();
        };
        for _ in 0..4 {
            run();
        }
        let v0 = vm_kb();
        for _ in 0..200 {
            run();
        }
        let v1 = vm_kb();
        assert!(v1 < v0 + 128 * 1024, "bloques TLS sin liberar: {} -> {} KB", v0, v1);
        // un modulo retirado: el bloque del hilo se libera en su siguiente ruta lenta (hueco reutilizado, generacion
        // nueva) y el modulo nuevo recibe su propia imagen
        std::thread::spawn(move || {
            let tp = unsafe { (*crate::rt::ensure_thread(256 << 10)).tls_base };
            let a = get_addr(tp, id, 0);
            assert_ne!(a, 0);
            unregister(id as usize);
            static INIT2: [u8; 1] = [9];
            let id2 = register(0x4444, INIT2.as_ptr() as u64, 1, 64, 8).unwrap() as u64;
            let b = get_addr(tp, id2, 0);
            assert_eq!(unsafe { *(b as *const u8) }, 9);
            if id2 == id {
                assert_ne!(a, b);
            }
            unregister(id2 as usize);
        })
        .join()
        .unwrap();
    }
}
