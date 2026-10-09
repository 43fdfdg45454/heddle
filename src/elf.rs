//! Cargador ELF AArch64 (bibliotecas compartidas estilo Android/bionic) dentro del proceso host.
//! Mapea segmentos, resuelve DT_NEEDED (HLE o ELF guest), aplica relocaciones (RELA, ANDROID_RELA APS2,
//! RELR, TLS/TLSDESC) y ejecuta constructores en el guest.
//!
//! Sigue el algoritmo del enlazador de bionic (linker.cpp: `find_libraries`, `soinfo_unload`):
//!  * Carga (`find_libraries`): las DT_NEEDED se recorren en anchura; una biblioteca ya cargada (o ya vista en esta
//!    misma carga) se reutiliza, asi que los ciclos de DT_NEEDED son validos. Las nuevas se leen en ese recorrido y
//!    despues se mapean en orden aleatorio, cada una donde decida el kernel (`reserve`; 2 MiB si es apta para paginas
//!    enormes). Despues se relocan las no enlazadas del grupo local de la raiz (anchura desde la raiz: la
//!    raiz primero) y se ejecutan los constructores en profundidad (`call_constructors`: primero los hijos, una vez
//!    por modulo; en un ciclo A -> B -> A, B antes que A).
//!  * Referencias como bionic: la cuenta vive en la raiz del grupo local (el modulo que se abrio con dlopen y cargo a
//!    los demas). `dlopen` de un modulo ya cargado sube la de su raiz; una arista DT_NEEDED entre grupos distintos
//!    sube la del grupo destino.
//!  * Descarga (`soinfo_unload`): cuando la cuenta de la raiz llega a 0 (y el grupo no es NODELETE ni GLOBAL), se
//!    recorren los hijos desde la raiz; los del mismo grupo sin mas padres se descargan con ella y los de otro grupo
//!    sueltan una referencia despues. Destructores (DT_FINI_ARRAY en orden inverso, DT_FINI) de todos, en orden de
//!    recorrido, y luego se liberan: fuera de la lista de modulos, codigo traducido de todos los hilos invalidado
//!    (`jit::publish_ic(0)`), trampolines retirados, regiones ejecutables del guest olvidadas, modulo TLS retirado
//!    y la reserva de direcciones desmapeada (al soltar la ultima referencia `Arc`, normalmente en el mismo dlclose):
//!    un puntero viejo a la biblioteca falla con SIGSEGV como en un dispositivo.
//!  * TLS: todas las bibliotecas guest se abren con dlopen (no hay ejecutable guest), asi que, como en bionic, todas
//!    usan TLS dinamico (`tls.rs`: modulo TLS, DTV por hilo, bloque reservado en el primer acceso, resolutor TLSDESC
//!    dinamico) y una relocacion initial-exec (R_AARCH64_TLS_TPREL64) que apunte a una variable de una de ellas hace
//!    fallar la carga.

use crate::cpu::Cpu;
use crate::hle;
use crate::rt;
use crate::sys::*;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, Weak};

/// Tamano de pagina del guest (`mem::guest_page`): el que usa el cargador como `page_size()` de bionic.
#[inline]
fn page() -> u64 {
    crate::mem::guest_page()
}
#[inline]
fn page_start(x: u64) -> u64 {
    x & !(page() - 1)
}
#[inline]
fn page_end(x: u64) -> u64 {
    page_start(x + page() - 1)
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_NOTE: u32 = 4;
const PT_TLS: u32 = 7;
const PT_GNU_RELRO: u32 = 0x6474e552;
const SHT_STRTAB: u32 = 3;
const SHT_DYNAMIC: u32 = 6;
const STT_TLS: u8 = 6;

const DT_NULL: i64 = 0;
const DT_NEEDED: i64 = 1;
const DT_PLTRELSZ: i64 = 2;
const DT_HASH: i64 = 4;
const DT_STRTAB: i64 = 5;
const DT_SYMTAB: i64 = 6;
const DT_RELA: i64 = 7;
const DT_RELASZ: i64 = 8;
const DT_SONAME: i64 = 14;
const DT_INIT: i64 = 12;
const DT_FINI: i64 = 13;
const DT_JMPREL: i64 = 23;
const DT_INIT_ARRAY: i64 = 25;
const DT_FINI_ARRAY: i64 = 26;
const DT_INIT_ARRAYSZ: i64 = 27;
const DT_FINI_ARRAYSZ: i64 = 28;
const DT_RELR: i64 = 36;
const DT_SYMBOLIC: i64 = 16;
const DT_TEXTREL: i64 = 22;
const DF_TEXTREL: u64 = 0x4;
const DT_RUNPATH: i64 = 29;
const DT_FLAGS: i64 = 30;
const DF_SYMBOLIC: u64 = 0x2;
const DT_FLAGS_1: i64 = 0x6ffffffb;
const DF_1_GLOBAL: u64 = 0x2;
const DF_1_NODELETE: u64 = 0x8;
/// Banderas de dlopen de bionic (LP64).
pub const RTLD_LAZY: u64 = 1;
pub const RTLD_NOW: u64 = 2;
pub const RTLD_NOLOAD: u64 = 4;
pub const RTLD_GLOBAL: u64 = 0x100;
pub const RTLD_NODELETE: u64 = 0x1000;

/// Banderas de `android_dlextinfo` (android/dlext.h).
pub const DLEXT_RESERVED_ADDRESS: u64 = 0x1;
pub const DLEXT_RESERVED_ADDRESS_HINT: u64 = 0x2;
pub const DLEXT_WRITE_RELRO: u64 = 0x4;
pub const DLEXT_USE_RELRO: u64 = 0x8;
pub const DLEXT_USE_LIBRARY_FD: u64 = 0x10;
pub const DLEXT_USE_LIBRARY_FD_OFFSET: u64 = 0x20;
pub const DLEXT_FORCE_LOAD: u64 = 0x40;
pub const DLEXT_USE_NAMESPACE: u64 = 0x200;
pub const DLEXT_RESERVED_ADDRESS_RECURSIVE: u64 = 0x400;
/// `ANDROID_DLEXT_VALID_FLAG_BITS` (0x80 y 0x100 estan retiradas)
const DLEXT_VALID_FLAG_BITS: u64 = DLEXT_RESERVED_ADDRESS
    | DLEXT_RESERVED_ADDRESS_HINT
    | DLEXT_WRITE_RELRO
    | DLEXT_USE_RELRO
    | DLEXT_USE_LIBRARY_FD
    | DLEXT_USE_LIBRARY_FD_OFFSET
    | DLEXT_FORCE_LOAD
    | DLEXT_USE_NAMESPACE
    | DLEXT_RESERVED_ADDRESS_RECURSIVE;

/// `android_dlextinfo` de LP64 tal como lo pasa el guest a `android_dlopen_ext`.
#[derive(Clone, Copy, Default, Debug)]
pub struct ExtInfo {
    pub flags: u64,
    pub reserved_addr: u64,
    pub reserved_size: u64,
    pub relro_fd: i32,
    pub library_fd: i32,
    pub library_fd_offset: i64,
    pub library_namespace: u64,
}

impl ExtInfo {
    /// Lee la estructura de memoria guest (disposicion de LP64: 48 bytes).
    ///
    /// # Safety
    /// `p` apunta a 48 bytes legibles.
    pub unsafe fn read(p: u64) -> ExtInfo {
        let q = p as *const u8;
        let u64at = |o: usize| std::ptr::read_unaligned(q.add(o) as *const u64);
        let i32at = |o: usize| std::ptr::read_unaligned(q.add(o) as *const i32);
        ExtInfo {
            flags: u64at(0),
            reserved_addr: u64at(8),
            reserved_size: u64at(16),
            relro_fd: i32at(24),
            library_fd: i32at(28),
            library_fd_offset: u64at(32) as i64,
            library_namespace: u64at(40),
        }
    }
}

/// Parametros de reserva de una carga (bionic `address_space_params`): la region que dio el llamador con
/// ANDROID_DLEXT_RESERVED_ADDRESS(_HINT), que se va consumiendo, o ninguna.
#[derive(Clone, Copy, Default)]
struct AddrSpace {
    start: u64,
    size: u64,
    must_use: bool,
}
const DT_RELRSZ: i64 = 35;
const DT_GNU_HASH: i64 = 0x6ffffef5;
const DT_ANDROID_REL: i64 = 0x6000000f;
const DT_ANDROID_RELSZ: i64 = 0x60000010;
const DT_ANDROID_RELA: i64 = 0x60000011;
const DT_ANDROID_RELASZ: i64 = 0x60000012;
const DT_ANDROID_RELR: i64 = 0x6fffe000;
const DT_ANDROID_RELRSZ: i64 = 0x6fffe001;
const DT_VERSYM: i64 = 0x6ffffff0;
const DT_VERDEF: i64 = 0x6ffffffc;
const DT_VERDEFNUM: i64 = 0x6ffffffd;
const DT_VERNEED: i64 = 0x6ffffffe;
const DT_VERNEEDNUM: i64 = 0x6fffffff;
const VER_FLG_BASE: u16 = 1;
/// `kVersymNotNeeded`, `kVersymGlobal` y `kVersymHiddenBit` de bionic
const VERSYM_NOT_NEEDED: u16 = 0;
const VERSYM_GLOBAL: u16 = 1;
const VERSYM_HIDDEN: u16 = 0x8000;

const R_ABS64: u32 = 257;
const R_GLOB_DAT: u32 = 1025;
const R_JUMP_SLOT: u32 = 1026;
const R_RELATIVE: u32 = 1027;
const R_TLS_DTPMOD64: u32 = 1028;
const R_TLS_DTPREL64: u32 = 1029;
const R_TLS_TPREL64: u32 = 1030;
const R_TLSDESC: u32 = 1031;
const R_IRELATIVE: u32 = 1032;

#[repr(C)]
#[derive(Clone, Copy)]
struct Ehdr {
    ident: [u8; 16],
    typ: u16,
    machine: u16,
    version: u32,
    entry: u64,
    phoff: u64,
    shoff: u64,
    flags: u32,
    ehsize: u16,
    phentsize: u16,
    phnum: u16,
    shentsize: u16,
    shnum: u16,
    shstrndx: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Phdr {
    pub typ: u32,
    pub flags: u32,
    pub offset: u64,
    pub vaddr: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub align: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Sym {
    name: u32,
    info: u8,
    other: u8,
    shndx: u16,
    value: u64,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Rela {
    offset: u64,
    info: u64,
    addend: i64,
}

/// Segmento PT_TLS de un modulo (su bloque en cada hilo lo reserva `tls.rs`).
pub struct TlsMod {
    pub init_addr: u64,
    pub filesz: usize,
    pub memsz: usize,
    pub align: usize,
}

/// Reserva de direcciones de un modulo (todos sus segmentos caen dentro) y el hueco inaccesible que bionic deja
/// delante (`gap`): se desmapean al soltar el modulo.
struct Rsv {
    base: u64,
    len: usize,
    gap: (u64, usize),
    /// la region la reservo quien llamo a android_dlopen_ext (RESERVED_ADDRESS): al soltarla se deja reservada sin
    /// acceso en lugar de desmapearla (bionic `soinfo_free` con `is_mapped_by_caller`)
    by_caller: bool,
}

impl Drop for Rsv {
    fn drop(&mut self) {
        if self.by_caller {
            unsafe { mmap(self.base as *mut c_void, self.len, PROT_NONE, MAP_FIXED | MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
        } else {
            unsafe { munmap(self.base as *mut c_void, self.len) };
        }
        if self.gap.1 != 0 {
            unsafe { munmap(self.gap.0 as *mut c_void, self.gap.1) };
        }
    }
}

/// Entero aleatorio en [0, n) (arc4random_uniform de bionic; aqui basta con que no se repita).
fn rand_below(n: u64) -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(HANDLES.load(SeqCst));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    if n == 0 {
        0
    } else {
        h.finish() % n
    }
}

/// Reserva `size` bytes de direcciones donde decida el kernel, como `ReserveWithAlignmentPadding` de bionic en
/// LP64: alineacion de biblioteca de 256 KiB (kLibraryAlignment, por la sombra de CFI) o `start_align` si es mayor,
/// inicio aleatorio en pasos de `start_align` (pagina, o 2 MiB para bibliotecas aptas para paginas enormes) dentro
/// del relleno y, si la biblioteca cruza un limite de 2 MiB, un hueco aleatorio de 1 a 31 paginas enormes delante
/// que queda reservado sin acceso. Cada carga cae en una direccion nueva.
fn reserve(size: usize, start_align: usize) -> Result<Rsv, String> {
    const LIB_ALIGN: usize = 256 << 10;
    const GAP_ALIGN: usize = 2 << 20;
    const MAX_GAP_UNITS: u64 = 32;
    // holgura con la pagina del host (la granularidad real de mmap); el inicio, a pagina del guest
    let page = crate::mem::PAGE;
    let up = |x: usize, a: usize| (x + a - 1) & !(a - 1);
    let down = |x: usize, a: usize| x & !(a - 1);
    let start_align = start_align.max(self::page() as usize);
    let (mut align, mut gap) = (LIB_ALIGN.max(start_align), 0usize);
    loop {
        let len = up(size + gap, align) + align - page;
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
        if p == MAP_FAILED {
            return Err(format!("couldn't reserve {} bytes of address space", size));
        }
        let p = p as usize;
        let (first_byte, last_byte) = (up(p, align), down(p + len, align) - 1);
        if gap == 0 && first_byte / GAP_ALIGN != last_byte / GAP_ALIGN {
            // cruza un limite de 2 MiB: otra vez, con un hueco aleatorio delante
            unsafe { munmap(p as *mut c_void, len) };
            align = align.max(GAP_ALIGN);
            gap = GAP_ALIGN * (rand_below(MAX_GAP_UNITS - 1) as usize + 1);
            continue;
        }
        let gap_end = if gap != 0 { down(p + len, GAP_ALIGN) } else { p + len };
        let gap_start = gap_end - gap;
        let first = up(p, align);
        let last = down(gap_start, align) - size;
        let start = first + rand_below(((last - first) / start_align + 1) as u64) as usize * start_align;
        unsafe {
            munmap(p as *mut c_void, start - p);
            munmap((start + size) as *mut c_void, gap_start - (start + size));
            if gap_end != p + len {
                munmap(gap_end as *mut c_void, p + len - gap_end);
            }
        }
        return Ok(Rsv { base: start as u64, len: size, gap: (gap_start as u64, gap), by_caller: false });
    }
}

pub struct Module {
    pub name: String,
    pub path: String,
    /// ruta real (sin enlaces simbolicos), la que bionic muestra en dladdr/dlerror
    pub realpath: String,
    /// DT_SONAME; sin el, vacio (bionic con `targetSdkVersion` >= 23: solo se reconoce por ruta/inodo) o, por
    /// debajo de 23, el nombre del archivo de su ruta real (`soinfo::prelink_image`)
    soname: String,
    /// identidad del archivo (st_dev, st_ino, desplazamiento dentro de el): `find_loaded_library_by_inode`
    file_id: (u64, u64, u64),
    /// handle de dlopen: unico en todo el proceso (un handle de una biblioteca descargada nunca vuelve a ser valido)
    pub handle: u64,
    pub base: u64, // sesgo de carga
    pub lo: u64,
    pub hi: u64,
    pub phdr_addr: u64,
    pub phnum: u16,
    pub phdrs: Vec<Phdr>,
    pub entry: u64,
    symtab: u64,
    strtab: u64,
    gnu_hash: u64,
    sysv_hash: u64,
    /// DT_VERSYM, DT_VERDEF (+DT_VERDEFNUM) y DT_VERNEED (+DT_VERNEEDNUM): versiones de simbolos
    versym: u64,
    verdef: (u64, usize),
    verneed: (u64, usize),
    pub needed: Vec<String>,
    /// DT_RUNPATH con $ORIGIN y $LIB sustituidos (`soinfo::set_dt_runpath`)
    runpath: Vec<String>,
    /// DT_SYMBOLIC (o DF_SYMBOLIC): sus referencias se buscan primero en el propio modulo
    symbolic: bool,
    /// DF_1_GLOBAL: forma parte del grupo global de su espacio de nombres
    df1_global: bool,
    /// espacio de nombres primario (donde se cargo) y secundarios (heredado por otros espacios)
    pub ns: usize,
    secondary: Mutex<Vec<usize>>,
    /// DT_NEEDED resueltas, en orden (`soinfo::get_children` de bionic): bibliotecas guest o del sistema (HLE)
    children: Mutex<Vec<Dep>>,
    /// handles de los modulos que lo tienen como hijo (`get_parents`)
    parents: Mutex<Vec<u64>>,
    /// raiz de su grupo local (la fija el enlazado; vacia = el propio modulo)
    group_root: Mutex<Weak<Module>>,
    pub tls: Option<TlsMod>,
    /// identificador de modulo TLS (`tls::register`, desde 1; 0 = sin registrar)
    tls_id: AtomicUsize,
    /// argumentos de sus descriptores TLSDESC dinamicos (bionic `tlsdesc_args_`): {generacion, modulo, desplazamiento}.
    /// Se llena una vez al relocar, con la capacidad justa (no se mueve: los descriptores apuntan aqui).
    tlsdesc_args: Mutex<Vec<[u64; 3]>>,
    init_array: (u64, usize),
    fini_array: (u64, usize),
    init: u64,
    fini: u64,
    /// cuenta de referencias: solo vale la de la raiz del grupo (como `ref_count_` de bionic)
    refs: AtomicUsize,
    pub dyn_addr: u64,
    /// Enlazado (bionic `is_linked`): visible en dlsym/dladdr/dl_iterate_phdr y en la busqueda global.
    live: AtomicBool,
    ctors_called: AtomicBool,
    /// `rtld_flags_` de bionic: RTLD_GLOBAL y RTLD_NODELETE de la carga que lo creo (o DF_1_GLOBAL/DF_1_NODELETE);
    /// con cualquiera de los dos su grupo nunca se descarga
    rtld_flags: AtomicU64,
    /// `targetSdkVersion` con el que se enlazo (bionic `soinfo::get_target_sdk_version`)
    target_sdk: std::sync::atomic::AtomicI32,
    /// modo de compatibilidad de 16 KiB: la region "RELRO de compatibilidad" (codigo, solo lectura y RELRO) que se
    /// protege tras relocar (bionic `compat_relro_start_`/`compat_relro_size_`); (0, 0) fuera de ese modo
    compat_relro: (u64, u64),
    /// `should_pad_segments_` (nota NT_ANDROID_TYPE_PAD_SEGMENT)
    pad: bool,
    _rsv: Rsv,
}

impl Module {
    pub fn contains(&self, a: u64) -> bool {
        a >= self.lo && a < self.hi
    }

    fn cstr(&self, off: u32) -> &str {
        unsafe {
            let p = (self.strtab + off as u64) as *const u8;
            let mut n = 0;
            while *p.add(n) != 0 {
                n += 1;
            }
            std::str::from_utf8_unchecked(std::slice::from_raw_parts(p, n))
        }
    }

    fn sym(&self, i: usize) -> Sym {
        unsafe { *((self.symtab + (i * 24) as u64) as *const Sym) }
    }

    pub fn is_live(&self) -> bool {
        self.live.load(SeqCst)
    }

    /// Busca un simbolo definido en este modulo (no en sus dependencias), de cualquier version no oculta. No devuelve
    /// simbolos TLS: su valor es un desplazamiento en el bloque TLS, no una direccion (ver `lookup_tls_local`).
    pub fn lookup_local(&self, name: &str) -> Option<u64> {
        self.lookup_local_v(name, None)
    }

    /// `lookup_local` con la version pedida (bionic `find_symbol_by_name` con `version_info`).
    fn lookup_local_v(&self, name: &str, vi: Option<Ver>) -> Option<u64> {
        self.find_sym(name, false, vi).map(|s| self.base + s.value)
    }

    /// Simbolo TLS (STT_TLS) definido en este modulo: su desplazamiento dentro del segmento PT_TLS.
    fn lookup_tls_local(&self, name: &str, vi: Option<Ver>) -> Option<u64> {
        self.find_sym(name, true, vi).map(|s| s.value)
    }

    /// bionic `for_each_verdef`: (vd_ndx, vd_hash, nombre) de cada version definida, salvo la del propio archivo
    /// (VER_FLG_BASE), hasta que `f` devuelva true. Err con el texto de bionic si la seccion es invalida.
    fn for_each_verdef(&self, mut f: impl FnMut(u16, u32, &str) -> bool) -> Result<(), String> {
        let (p, n) = self.verdef;
        if p == 0 {
            return Ok(());
        }
        let mut off = 0u64;
        for i in 0..n {
            unsafe {
                let d = (p + off) as *const u8;
                let (ver, flags, ndx, cnt) = (*(d as *const u16), *(d.add(2) as *const u16), *(d.add(4) as *const u16), *(d.add(6) as *const u16));
                let (hash, aux, next) = (*(d.add(8) as *const u32), *(d.add(12) as *const u32), *(d.add(16) as *const u32));
                off += next as u64;
                if ver != 1 {
                    return Err(format!("unsupported verdef[{}] vd_version: {} (expected 1) library: {}", i, ver, self.realpath));
                }
                if flags & VER_FLG_BASE != 0 {
                    continue;
                }
                if cnt == 0 {
                    return Err(format!("invalid verdef[{}] vd_cnt == 0 (version without a name)", i));
                }
                let name = *(d.add(aux as usize) as *const u32);
                if f(ndx, hash, self.cstr(name)) {
                    break;
                }
            }
        }
        Ok(())
    }

    /// bionic `find_verdef_version_index`: indice de la version pedida entre las que define el modulo
    /// (`VERSYM_NOT_NEEDED` sin version pedida, `VERSYM_GLOBAL` si no la define).
    fn verdef_index(&self, vi: Option<Ver>) -> u16 {
        let Some(v) = vi else { return VERSYM_NOT_NEEDED };
        let mut r = VERSYM_GLOBAL;
        let _ = self.for_each_verdef(|ndx, hash, name| {
            if hash == v.hash && name == v.name {
                r = ndx;
                return true;
            }
            false
        });
        r
    }

    /// bionic `check_symbol_version`: sin tabla de versiones vale cualquiera; sin version pedida, cualquiera no oculta;
    /// si no, exactamente la pedida.
    fn version_ok(&self, idx: usize, need: u16) -> bool {
        if self.versym == 0 {
            return true;
        }
        let v = unsafe { *(self.versym as *const u16).add(idx) };
        if need == VERSYM_NOT_NEEDED {
            v & VERSYM_HIDDEN == 0
        } else {
            need == v & !VERSYM_HIDDEN
        }
    }

    fn find_sym(&self, name: &str, tls: bool, vi: Option<Ver>) -> Option<Sym> {
        // bionic `is_symbol_global_and_defined`: definido y con ligadura global o debil
        let want = |s: &Sym| s.shndx != 0 && matches!(s.info >> 4, 1 | 2) && ((s.info & 0xf) == STT_TLS) == tls && self.cstr(s.name) == name;
        let mut need: Option<u16> = None;
        let mut ver_ok = |i: usize| {
            let n = *need.get_or_insert_with(|| self.verdef_index(vi));
            self.version_ok(i, n)
        };
        if self.gnu_hash != 0 {
            unsafe {
                let h = self.gnu_hash as *const u32;
                let nbuckets = *h as usize;
                let symoffset = *h.add(1) as usize;
                let bloom_size = *h.add(2) as usize;
                let bloom_shift = *h.add(3);
                let bloom = h.add(4) as *const u64;
                let buckets = bloom.add(bloom_size) as *const u32;
                let chain = buckets.add(nbuckets);
                let mut hv: u32 = 5381;
                for b in name.bytes() {
                    hv = hv.wrapping_mul(33).wrapping_add(b as u32);
                }
                let word = *bloom.add(((hv / 64) as usize) % bloom_size);
                let mask = (1u64 << (hv % 64)) | (1u64 << ((hv >> bloom_shift) % 64));
                if word & mask != mask {
                    return None;
                }
                let mut idx = *buckets.add(hv as usize % nbuckets) as usize;
                if idx < symoffset {
                    return None;
                }
                loop {
                    let ch = *chain.add(idx - symoffset);
                    if (ch | 1) == (hv | 1) {
                        let s = self.sym(idx);
                        if want(&s) && ver_ok(idx) {
                            return Some(s);
                        }
                    }
                    if ch & 1 != 0 {
                        return None;
                    }
                    idx += 1;
                }
            }
        }
        if self.sysv_hash != 0 {
            unsafe {
                let h = self.sysv_hash as *const u32;
                let (nb, nc) = (*h as usize, *h.add(1) as usize);
                let buckets = h.add(2);
                let chain = h.add(2 + nb);
                let hv = elf_hash(name);
                let mut idx = *buckets.add(hv as usize % nb) as usize;
                while idx != 0 && idx < nc {
                    let s = self.sym(idx);
                    if want(&s) && ver_ok(idx) {
                        return Some(s);
                    }
                    idx = *chain.add(idx) as usize;
                }
            }
        }
        None
    }

    /// `dlsym(handle)` de bionic (`dlsym_handle_lookup`): el propio modulo y sus dependencias en anchura que sean
    /// accesibles desde su espacio de nombres, tambien las bibliotecas del sistema (HLE).
    pub fn lookup(self: &Arc<Self>, name: &str) -> Option<u64> {
        dlsym_handle(self, name, None)
    }

    /// Simbolo mas cercano a una direccion (para trazas).
    pub fn symbolize(&self, addr: u64) -> String {
        let off = addr.wrapping_sub(self.base);
        let n = self.sym_count();
        let mut best: Option<(u64, usize)> = None;
        for i in 1..n {
            let s = self.sym(i);
            if s.shndx != 0 && (s.info & 0xf) == 2 && s.value <= off && off < s.value + s.size.max(1) {
                if best.map_or(true, |(v, _)| s.value > v) {
                    best = Some((s.value, i));
                }
            }
        }
        match best {
            Some((v, i)) => format!("{}+{:#x}", self.cstr(self.sym(i).name), off - v),
            None => format!("{:#x}", off),
        }
    }

    /// (nombre, direccion) del simbolo de funcion/objeto que contiene `addr`.
    pub fn symbol_at(&self, addr: u64) -> Option<(String, u64)> {
        let off = addr.wrapping_sub(self.base);
        let n = self.sym_count();
        let mut best: Option<(u64, usize)> = None;
        for i in 1..n {
            let s = self.sym(i);
            let t = s.info & 0xf;
            if s.shndx != 0 && (t == 1 || t == 2) && s.value <= off && off < s.value + s.size.max(1) {
                if best.map_or(true, |(v, _)| s.value > v) {
                    best = Some((s.value, i));
                }
            }
        }
        best.map(|(v, i)| (self.cstr(self.sym(i).name).to_string(), self.base + v))
    }

    fn sym_count(&self) -> usize {
        unsafe {
            if self.sysv_hash != 0 {
                return *(self.sysv_hash as *const u32).add(1) as usize;
            }
            if self.gnu_hash != 0 {
                let h = self.gnu_hash as *const u32;
                let (nb, so, bs) = (*h as usize, *h.add(1) as usize, *h.add(2) as usize);
                let buckets = (h.add(4) as *const u64).add(bs) as *const u32;
                let chain = buckets.add(nb);
                let mut max = 0usize;
                for i in 0..nb {
                    max = max.max(*buckets.add(i) as usize);
                }
                if max < so {
                    return so;
                }
                let mut i = max;
                while *chain.add(i - so) & 1 == 0 {
                    i += 1;
                }
                return i + 1;
            }
        }
        0
    }
}

// ---------------------------------------------------------------------------------------------
// Registro global
// ---------------------------------------------------------------------------------------------

static MODULES: Mutex<Vec<Arc<Module>>> = Mutex::new(Vec::new());
/// Cota gruesa (sin bloqueo) de todos los segmentos ejecutables guest cargados.
static CODE_MIN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
static CODE_MAX: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Rangos ejecutables por el guest: los PT_LOAD con PF_X o, en el modo de compatibilidad de 16 KiB, toda la region
/// RELRO de compatibilidad (bionic la protege RX: codigo, solo lectura y RELRO).
fn exec_ranges(m: &Module) -> Vec<(u64, u64)> {
    if m.compat_relro.1 != 0 {
        return vec![(m.compat_relro.0, m.compat_relro.0 + m.compat_relro.1)];
    }
    m.phdrs.iter().filter(|p| p.typ == PT_LOAD && p.flags & 1 != 0).map(|p| (p.vaddr + m.base, p.vaddr + m.base + p.memsz)).collect()
}

/// `a` esta dentro de un segmento ejecutable de un modulo guest? (rapido: una comparacion en el caso comun)
pub fn is_guest_code(a: u64) -> bool {
    if crate::tls::is_resolver_code(a) {
        return true; // resolutor TLSDESC (codigo ARM del puente, ver tls.rs)
    }
    if a < CODE_MIN.load(SeqCst) || a >= CODE_MAX.load(SeqCst) {
        return false;
    }
    crate::monitor::lock(&MODULES).iter().any(|m| m.contains(a) && exec_ranges(m).iter().any(|&(lo, hi)| a >= lo && a < hi))
}

/// Como `is_guest_code`, sin esperar ni reservar memoria (manejadores de senal): None si otro hilo (o el propio,
/// interrumpido) tiene la lista de modulos tomada.
pub fn is_guest_code_try(a: u64) -> Option<bool> {
    if crate::tls::is_resolver_code(a) {
        return Some(true);
    }
    if a < CODE_MIN.load(SeqCst) || a >= CODE_MAX.load(SeqCst) {
        return Some(false);
    }
    let g = MODULES.try_lock().ok()?;
    Some(g.iter().any(|m| {
        m.contains(a)
            && if m.compat_relro.1 != 0 {
                a >= m.compat_relro.0 && a < m.compat_relro.0 + m.compat_relro.1
            } else {
                m.phdrs.iter().any(|p| p.typ == PT_LOAD && p.flags & 1 != 0 && a >= p.vaddr + m.base && a < p.vaddr + m.base + p.memsz)
            }
    }))
}
static LOADING: Mutex<()> = Mutex::new(());
/// cargas y descargas de modulos (bionic `g_module_load_counter`/`_unload_counter`: `dlpi_adds`/`dlpi_subs`)
pub static LOADS: AtomicU64 = AtomicU64::new(0);
pub static UNLOADS: AtomicU64 = AtomicU64::new(0);
/// ultimo handle repartido (ver `new_handle`)
static HANDLES: AtomicU64 = AtomicU64::new(0);

/// Handle nuevo, impar como los de bionic y distinto de todos los anteriores (biyeccion de un contador), y de los
/// especiales de dlopen (0, 1 = programa, 2 = biblioteca del sistema, -1).
fn new_handle() -> u64 {
    loop {
        let n = HANDLES.fetch_add(1, SeqCst) + 1;
        let h = (n * 2 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if h > 2 && h != u64::MAX && !is_sys_handle(h) {
            return h;
        }
    }
}

/// Anade un directorio de busqueda al espacio de nombres por defecto (su LD_LIBRARY_PATH).
pub fn add_search_path(p: &str) {
    crate::namespace::add_default_path(p);
}

/// Modulos cargados (los descargados con dlclose no cuentan, como en bionic).
pub fn modules() -> Vec<Arc<Module>> {
    crate::monitor::lock(&MODULES).iter().filter(|m| m.is_live()).cloned().collect()
}

pub fn module_for_addr(a: u64) -> Option<Arc<Module>> {
    crate::monitor::lock(&MODULES).iter().find(|m| m.is_live() && m.contains(a)).cloned()
}

/// Modulo cargado cuyo handle de dlopen es `h`.
pub fn module_by_handle(h: u64) -> Option<Arc<Module>> {
    crate::monitor::lock(&MODULES).iter().find(|m| m.is_live() && m.handle == h).cloned()
}

/// Hijos guest de un modulo (sin las bibliotecas del sistema).
fn guest_children(m: &Module) -> Vec<Arc<Module>> {
    m.children.lock().unwrap().iter().filter_map(|d| if let Dep::M(c) = d { Some(c.clone()) } else { None }).collect()
}

/// Raiz del grupo local de `m` (el propio modulo si no tiene o ya no existe).
fn group_root(m: &Arc<Module>) -> Arc<Module> {
    m.group_root.lock().unwrap().upgrade().unwrap_or_else(|| m.clone())
}

// ---------------------------------------------------------------------------------------------
// Busqueda de simbolos (bionic: SymbolLookupList, dlsym_handle_lookup, dlsym_linear_lookup)
// ---------------------------------------------------------------------------------------------

/// Una dependencia resuelta o un elemento de una lista de busqueda: el ejecutable (app_process64, en el grupo global
/// de todos los espacios), una biblioteca del sistema que sirve el puente (HLE) o una biblioteca guest.
#[derive(Clone)]
pub enum Dep {
    Exe,
    Sys(&'static str),
    M(Arc<Module>),
}

impl Dep {
    fn same(&self, o: &Dep) -> bool {
        match (self, o) {
            (Dep::Exe, Dep::Exe) => true,
            (Dep::Sys(a), Dep::Sys(b)) => a == b,
            (Dep::M(a), Dep::M(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Lo que exporta app_process64 (el ejecutable de las apps, en el grupo global por DF_1_GLOBAL): libsigchain, que
/// lleva enlazada y exporta (`--export-dynamic`), y que por eso tiene prioridad sobre las de libc.
const EXE_EXPORTS: &[&str] = &["sigaction", "sigaction64", "signal", "bsd_signal", "sigprocmask", "sigprocmask64"];

/// Bibliotecas que el puente "carga" sin implementar nada util (enlazador): existen para que el enlace no falle.
/// OpenSL ES y OpenMAX AL estan en `ALLOWED_LIBS` (proxys).
const SYS_EXTRA: &[&str] = &["libdl_android.so", "ld-android.so"];

/// Biblioteca del sistema servida por HLE con ese nombre (o ruta): su nombre canonico.
pub fn sys_lib(name: &str) -> Option<&'static str> {
    let base = name.rsplit('/').next().unwrap_or(name);
    crate::boundary::allowed_lib_name(base).or_else(|| SYS_EXTRA.iter().copied().find(|&n| n == base))
}

/// Ruta real en un dispositivo arm64 de una biblioteca del sistema servida por HLE (la de `dladdr`, `dlerror` y
/// `/proc/self/maps` en Android 10 o posterior): las de bionic en el APEX del runtime, el enlazador (ld-android.so es
/// el propio linker64) y el resto en /system/lib64.
/// Biblioteca real de una del sistema: en Android `libGLESv3.so` es un enlace simbolico a `libGLESv2.so`
/// (frameworks/native/opengl/libs, `symlinks: ["libGLESv3.so"]`), que exporta GLES 2.0 a 3.2; el NDK las separa en
/// dos stubs, pero en el dispositivo `dlsym(dlopen("libGLESv2.so"), "glGetStringi")` encuentra la funcion.
pub fn sys_canonical(l: &str) -> &str {
    match l {
        "libGLESv3.so" => "libGLESv2.so",
        _ => l,
    }
}

pub fn sys_realpath(l: &str) -> String {
    let l = sys_canonical(l);
    match l {
        "libc.so" | "libm.so" | "libdl.so" | "libdl_android.so" => format!("/apex/com.android.runtime/lib64/bionic/{}", l),
        "ld-android.so" => "/apex/com.android.runtime/bin/linker64".to_string(),
        _ => format!("/system/lib64/{}", l),
    }
}

/// Biblioteca del sistema a la que pertenece la funcion `name` que sirve el puente (`dladdr` de un slot): la primera
/// del NDK que la exporta (libc antes que las demas) o, si solo la implementa el puente, libc.so.
pub fn sys_lib_of_symbol(name: &str) -> &'static str {
    let ex = crate::sigs::exporters(name);
    ex.iter().find(|e| e.1 == "libc.so").or_else(|| ex.first()).and_then(|e| sys_lib(e.1)).unwrap_or("libc.so")
}

/// Bibliotecas del sistema servidas por HLE (no se cargan como ELF).
pub fn is_hle_lib(name: &str) -> bool {
    sys_lib(name).is_some()
}

/// Bibliotecas del sistema en el orden en que estan "cargadas" en los espacios de sistema (las carga el proceso al
/// arrancar, antes que cualquier biblioteca de la app, con RTLD_GLOBAL).
fn sys_libs() -> &'static [&'static str] {
    static L: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    L.get_or_init(|| crate::boundary::allowed_libs().chain(SYS_EXTRA.iter().copied()).collect())
}

/// Version pedida de un simbolo (bionic `version_info`): nombre y su hash ELF.
#[derive(Clone, Copy)]
pub struct Ver<'a> {
    pub name: &'a str,
    pub hash: u32,
}

impl<'a> Ver<'a> {
    pub fn new(name: &'a str) -> Self {
        Ver { name, hash: elf_hash(name) }
    }
}

/// Hash ELF (SysV) de bionic (`calculate_elf_hash`).
fn elf_hash(name: &str) -> u32 {
    let mut h: u32 = 0;
    for b in name.bytes() {
        h = (h << 4).wrapping_add(b as u32);
        let g = h & 0xf000_0000;
        h ^= g;
        h ^= g >> 24;
    }
    h
}

/// La biblioteca del sistema `lib` exporta `name` con la version pedida? Cada simbolo se atribuye a todas las
/// bibliotecas del NDK que lo exportan (`sigs::EXPORTS`), con su version: sin version pedida vale cualquiera; si la
/// biblioteca no tiene versiones, tambien; si la define, solo esa; si no la define, ninguna (en bionic solo valdria
/// un simbolo global sin version, y las bibliotecas con versiones no tienen). Lo que solo implementa el puente (no
/// esta en el NDK) se atribuye a libc, sin version.
fn sys_exports(lib: &str, name: &str, vi: Option<Ver>, ex: &[(&str, &str, &str)]) -> bool {
    if ex.is_empty() {
        return lib == "libc.so" && crate::libc_hle::resolve(name).is_some();
    }
    let lib = sys_canonical(lib);
    let Some(e) = ex.iter().find(|e| sys_canonical(e.1) == lib) else { return false };
    match vi {
        None => true,
        Some(v) => {
            let defined = crate::sigs::lib_versions(lib);
            defined.is_empty() || (defined.contains(&v.name) && e.2 == v.name)
        }
    }
}

/// Resultado de buscar un simbolo en un elemento de la lista.
enum Hit {
    /// definido aqui, en esta direccion
    At(u64),
    /// lo exporta esta biblioteca del sistema pero el puente no lo puede ofrecer (sin firma reenviable)
    Unavailable,
}

fn served(name: &str) -> Hit {
    crate::libc_hle::resolve(name).map_or(Hit::Unavailable, Hit::At)
}

fn find_in(d: &Dep, name: &str, vi: Option<Ver>) -> Option<Hit> {
    match d {
        Dep::Exe => EXE_EXPORTS.contains(&name).then(|| served(name)),
        Dep::Sys(l) => sys_exports(l, name, vi, crate::sigs::exporters(name)).then(|| served(name)),
        Dep::M(m) => m.lookup_local_v(name, vi).map(Hit::At),
    }
}

/// Simbolo de `m` para dlsym: tambien los TLS (bionic `do_dlsym`: la direccion de la copia de este hilo, reservando
/// su bloque si hace falta).
fn dl_local(m: &Module, name: &str, vi: Option<Ver>) -> Option<u64> {
    if let Some(a) = m.lookup_local_v(name, vi) {
        return Some(a);
    }
    let off = m.lookup_tls_local(name, vi)?;
    let id = m.tls_id.load(SeqCst) as u64;
    let tp = crate::rt::cur().tls_base;
    match crate::tls::get_addr(tp, id, off) {
        0 => None,
        a => Some(a),
    }
}

/// `find_in` para dlsym (con simbolos TLS).
fn find_in_dl(d: &Dep, name: &str, vi: Option<Ver>) -> Option<Hit> {
    match d {
        Dep::M(m) => dl_local(m, name, vi).map(Hit::At),
        _ => find_in(d, name, vi),
    }
}

fn member(m: &Module, ns: usize) -> bool {
    m.ns == ns || m.secondary.lock().unwrap().contains(&ns)
}

/// `android_namespace_t::is_accessible(soinfo*)`: del espacio (primario o secundario) o hijo directo de una
/// biblioteca cuyo espacio primario es `ns`.
fn accessible(m: &Module, ns: usize) -> bool {
    if member(m, ns) {
        return true;
    }
    let parents = m.parents.lock().unwrap().clone();
    let g = crate::monitor::lock(&MODULES);
    parents.iter().any(|&h| g.iter().any(|p| p.handle == h && p.ns == ns))
}

/// Hijos (DT_NEEDED) de una biblioteca del sistema, como `Dep` (`sigs::SYS_NEEDED`; solo las que sirve el puente).
fn sys_children(l: &str) -> impl Iterator<Item = Dep> {
    crate::sigs::sys_needed(l).iter().filter_map(|&c| sys_lib(c).filter(|&n| n == c)).map(Dep::Sys)
}

/// `is_accessible` de bionic para una biblioteca del sistema a la que se llega desde otra del sistema: lo es en los
/// espacios de sistema (donde esta cargada) y, en otro espacio, si alguno de sus padres tiene ahi su espacio primario
/// (una biblioteca guest del espacio la declara en DT_NEEDED). Sus propias dependencias no la hacen accesible: desde
/// una app se busca en libandroid.so pero no en lo que necesita libandroid.so, salvo que otra biblioteca de la app
/// lo declare. `declared`: las del sistema que declaran las guest del espacio (se calcula una vez por recorrido).
fn sys_accessible(l: &str, ns: usize, declared: &mut Option<Vec<&'static str>>) -> bool {
    if crate::namespace::is_system(ns) {
        return true;
    }
    declared
        .get_or_insert_with(|| {
            let mut v = Vec::new();
            for m in crate::monitor::lock(&MODULES).iter().filter(|m| m.ns == ns) {
                for d in m.children.lock().unwrap().iter() {
                    if let Dep::Sys(c) = d {
                        if !v.contains(c) {
                            v.push(*c);
                        }
                    }
                }
            }
            v
        })
        .contains(&l)
}

/// Recorre el grupo local de `root` visto desde `ns` (bionic `walk_dependencies_tree` con `is_accessible`): en
/// anchura, sin repetir; una biblioteca no accesible se salta con sus hijos. Las del sistema tienen sus DT_NEEDED de
/// `sigs::SYS_NEEDED`; un hijo directo de una guest accesible lo es (su padre esta en el espacio). `f` devuelve Some
/// para parar.
fn walk_local<R>(root: &Arc<Module>, ns: usize, mut f: impl FnMut(&Dep) -> Option<R>) -> Option<R> {
    let mut seen: Vec<Dep> = Vec::new();
    let mut declared = None;
    // (biblioteca, hija directa de una guest accesible)
    let mut q: std::collections::VecDeque<(Dep, bool)> = std::collections::VecDeque::new();
    q.push_back((Dep::M(root.clone()), false));
    while let Some((d, from_guest)) = q.pop_front() {
        if seen.iter().any(|o| o.same(&d)) {
            continue;
        }
        seen.push(d.clone());
        match &d {
            Dep::M(m) => {
                if !accessible(m, ns) {
                    continue;
                }
                let direct = m.ns == ns;
                for c in m.children.lock().unwrap().iter() {
                    q.push_back((c.clone(), direct));
                }
            }
            Dep::Sys(l) => {
                if !from_guest && !sys_accessible(l, ns, &mut declared) {
                    continue;
                }
                q.extend(sys_children(l).map(|c| (c, false)));
            }
            Dep::Exe => {}
        }
        if let Some(r) = f(&d) {
            return Some(r);
        }
    }
    None
}

/// Grupo local de `root` visto desde `ns` (`walk_local`), en orden.
fn local_deps(root: &Arc<Module>, ns: usize) -> Vec<Dep> {
    let mut out = Vec::new();
    walk_local::<()>(root, ns, |d| {
        out.push(d.clone());
        None
    });
    out
}

/// Grupo global de `ns` (bionic `get_global_group`): el ejecutable y las bibliotecas DF_1_GLOBAL del espacio.
fn global_deps(ns: usize) -> Vec<Dep> {
    let mut v = vec![Dep::Exe];
    v.extend(crate::monitor::lock(&MODULES).iter().filter(|m| m.df1_global && member(m, ns)).map(|m| Dep::M(m.clone())));
    v
}

/// Lista de busqueda para relocar `m`, del grupo local de `root` (bionic `SymbolLookupList`): el propio modulo si
/// es DT_SYMBOLIC, el grupo global de su espacio y el grupo local.
fn lookup_list(m: &Arc<Module>, root: &Arc<Module>) -> Vec<Dep> {
    let mut v = Vec::new();
    if m.symbolic {
        v.push(Dep::M(m.clone()));
    }
    v.extend(global_deps(root.ns));
    v.extend(local_deps(root, root.ns));
    v
}

/// `dlsym(handle)` (bionic `dlsym_handle_lookup`): `si` y su arbol de dependencias accesibles desde su espacio.
pub fn dlsym_handle(si: &Arc<Module>, name: &str, vi: Option<Ver>) -> Option<u64> {
    walk_local(si, si.ns, |d| find_in_dl(d, name, vi)).and_then(|h| match h {
        Hit::At(a) => Some(a),
        Hit::Unavailable => None,
    })
}

/// `dlsym` con el handle de una biblioteca del sistema (bionic `dlsym_handle_lookup`): ella y sus dependencias en
/// anchura (`sigs::SYS_NEEDED`; p. ej. libm -> libc -> ld-android, libdl), todas accesibles desde su espacio.
pub fn dlsym_sys(lib: &str, name: &str, vi: Option<Ver>) -> Option<u64> {
    let mut seen: Vec<&'static str> = Vec::new();
    let mut q: std::collections::VecDeque<&'static str> = std::collections::VecDeque::new();
    q.push_back(sys_lib(lib)?);
    while let Some(l) = q.pop_front() {
        if seen.contains(&l) {
            continue;
        }
        seen.push(l);
        match find_in(&Dep::Sys(l), name, vi) {
            Some(Hit::At(a)) => return Some(a),
            Some(Hit::Unavailable) => return None,
            None => {}
        }
        q.extend(sys_children(l).filter_map(|d| if let Dep::Sys(c) = d { Some(c) } else { None }));
    }
    None
}

/// Biblioteca del sistema cuyo handle de `dlopen` es `h`.
pub fn sys_lib_of_handle(h: u64) -> Option<&'static str> {
    if !is_sys_handle(h) {
        return None;
    }
    sys_libs().iter().copied().find(|&l| sys_handle(l) == h)
}

/// `dlsym` con el handle especial por defecto (`next` = false) o `RTLD_NEXT` (bionic `dlsym_linear_lookup`): las
/// bibliotecas del espacio del llamador en orden de carga, solo las RTLD_GLOBAL (todas con `targetSdkVersion` < 23;
/// el ejecutable y las del sistema lo son), y luego el grupo local del llamador. Con RTLD_NEXT se empieza despues
/// del llamador.
pub fn dlsym_default(name: &str, caller: Option<&Arc<Module>>, next: bool, vi: Option<Ver>) -> Option<u64> {
    if next && caller.is_none() {
        return None;
    }
    let ns = caller.map_or_else(crate::namespace::anonymous, |c| c.ns);
    dlsym_linear(ns, name, caller, next, vi)
}

/// La lista de bibliotecas del espacio `ns` en orden de carga (bionic `android_namespace_t::soinfo_list`) es: el
/// ejecutable, las del sistema si es un espacio de sistema (las carga el proceso al arrancar) y las guest del espacio
/// (primario o secundario) en el orden en que se cargaron. El llamador (RTLD_NEXT) siempre es guest.
fn dlsym_linear(ns: usize, name: &str, caller: Option<&Arc<Module>>, next: bool, vi: Option<Ver>) -> Option<u64> {
    let hit = |h: Option<Hit>| match h {
        Some(Hit::At(a)) => Some(Some(a)),
        Some(Hit::Unavailable) => Some(None),
        None => None,
    };
    if !next {
        if let Some(r) = hit(find_in(&Dep::Exe, name, vi)) {
            return r;
        }
        if crate::namespace::is_system(ns) {
            let ex = crate::sigs::exporters(name);
            if let Some(l) = sys_libs().iter().find(|l| sys_exports(l, name, vi, ex)) {
                return hit(find_in(&Dep::Sys(l), name, vi)).flatten();
            }
        }
    }
    let members: Vec<Arc<Module>> = crate::monitor::lock(&MODULES).iter().filter(|m| m.is_live() && member(m, ns)).cloned().collect();
    let start = match (next, caller) {
        (true, Some(c)) => members.iter().position(|m| Arc::ptr_eq(m, c)).map_or(members.len(), |i| i + 1),
        _ => 0,
    };
    for m in &members[start..] {
        if m.rtld_flags.load(SeqCst) & RTLD_GLOBAL == 0 && m.target_sdk.load(SeqCst) >= 23 {
            continue;
        }
        if let Some(a) = dl_local(m, name, vi) {
            return Some(a);
        }
    }
    let c = caller?;
    let root = group_root(c);
    let mut skip = next;
    for d in local_deps(&root, root.ns) {
        if skip {
            skip = !matches!(&d, Dep::M(m) if Arc::ptr_eq(m, c));
            continue;
        }
        if let Some(r) = hit(find_in_dl(&d, name, vi)) {
            return r;
        }
    }
    None
}

/// `dlsym` con el handle de `dlopen` con NULL (el ejecutable): bionic hace `dlsym_linear_lookup` en el espacio por
/// defecto sin llamador.
pub fn dlsym_main(name: &str, vi: Option<Ver>) -> Option<u64> {
    dlsym_linear(crate::namespace::DEFAULT, name, None, false, vi)
}

/// Espacio de nombres de quien llama desde `pc` (bionic `get_caller_namespace`): el de la biblioteca guest que lo
/// contiene, o el anonimo.
pub fn caller_of(pc: u64) -> (usize, Option<Arc<Module>>) {
    match module_for_addr(pc) {
        Some(m) => (m.ns, Some(m)),
        None => (crate::namespace::anonymous(), None),
    }
}

/// Al crear el espacio `new` con padre `parent`: con SHARED hereda todas las bibliotecas del padre; si no, su grupo
/// compartido (del espacio por defecto, las DF_1_GLOBAL; de otro, las RTLD_GLOBAL). bionic `create_namespace`.
pub fn inherit_namespace(parent: usize, new: usize, shared: bool) {
    for m in crate::monitor::lock(&MODULES).iter() {
        if !member(m, parent) {
            continue;
        }
        let take = shared || if parent == crate::namespace::DEFAULT { m.df1_global } else { m.rtld_flags.load(SeqCst) & RTLD_GLOBAL != 0 };
        if take {
            m.secondary.lock().unwrap().push(new);
        }
    }
}

/// Handle de `dlopen` de una biblioteca del sistema (impar como los de bionic, uno distinto por biblioteca).
pub fn sys_handle(lib: &'static str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in lib.bytes() {
        h = (h ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    SYS_HANDLE_TAG | (h & 0xff_ffff_fffe) | 1
}

const SYS_HANDLE_TAG: u64 = 0x5359_5300_0000_0000;

/// `h` es el handle de una biblioteca del sistema?
pub fn is_sys_handle(h: u64) -> bool {
    h & 0xffff_ff00_0000_0000 == SYS_HANDLE_TAG
}

pub fn describe_addr(a: u64) -> String {
    if hle::is_hle(a) {
        return format!("hle:{}", hle::name_at(a));
    }
    match module_for_addr(a) {
        Some(m) => format!("{}!{}", m.name, m.symbolize(a)),
        None => format!("{:#x}", a),
    }
}


// ---------------------------------------------------------------------------------------------
// Carga
// ---------------------------------------------------------------------------------------------

/// Biblioteca dentro de un APK (`ruta.apk!/lib/arm64-v8a/libx.so`): Android la carga asi cuando la app no extrae
/// sus bibliotecas. Devuelve (desplazamiento de los datos, tamano, almacenada sin comprimir).
pub fn zip_entry(apk: &str, entry: &str) -> Option<(u64, u64, bool)> {
    let mut r = None;
    zip_scan(apk, |n, lho, size, stored| {
        if n == entry.as_bytes() {
            r = Some((lho, size, stored));
            return false;
        }
        true
    })?;
    let (lho, size, stored) = r?;
    Some((zip_data_off(apk, lho)?, size, stored))
}

/// Recorre el directorio central de un ZIP: `f(nombre, desplazamiento de la cabecera local, tamano en el archivo,
/// sin comprimir)`; `f` devuelve false para parar. None si el archivo no es un ZIP legible.
pub fn zip_scan(apk: &str, mut f: impl FnMut(&[u8], u64, u64, bool) -> bool) -> Option<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut fl = std::fs::File::open(apk).ok()?;
    let len = fl.metadata().ok()?.len();
    let tail = len.min(65536 + 22);
    let mut buf = vec![0u8; tail as usize];
    fl.seek(SeekFrom::Start(len - tail)).ok()?;
    fl.read_exact(&mut buf).ok()?;
    let u16at = |b: &[u8], o: usize| u16::from_le_bytes([b[o], b[o + 1]]) as u64;
    let u32at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as u64;
    let e = (0..buf.len().checked_sub(21)?).rev().find(|&i| buf[i..i + 4] == *b"PK\x05\x06")?;
    let (cd_size, cd_off) = (u32at(&buf, e + 12), u32at(&buf, e + 16));
    if cd_off + cd_size > len || cd_size > 64 << 20 {
        return None;
    }
    let mut cd = vec![0u8; cd_size as usize];
    fl.seek(SeekFrom::Start(cd_off)).ok()?;
    fl.read_exact(&mut cd).ok()?;
    let mut i = 0usize;
    while i + 46 <= cd.len() && cd[i..i + 4] == *b"PK\x01\x02" {
        let (method, csize, usize_) = (u16at(&cd, i + 10), u32at(&cd, i + 20), u32at(&cd, i + 24));
        let (nl, xl, cl, lho) = (u16at(&cd, i + 28) as usize, u16at(&cd, i + 30) as usize, u16at(&cd, i + 32) as usize, u32at(&cd, i + 42));
        if i + 46 + nl > cd.len() {
            return None;
        }
        if !f(&cd[i + 46..i + 46 + nl], lho, if method == 0 { usize_ } else { csize }, method == 0) {
            return Some(());
        }
        i += 46 + nl + xl + cl;
    }
    Some(())
}

/// Desplazamiento de los datos de una entrada, a partir de su cabecera local.
pub fn zip_data_off(apk: &str, lho: u64) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut fl = std::fs::File::open(apk).ok()?;
    let mut lh = [0u8; 30];
    fl.seek(SeekFrom::Start(lho)).ok()?;
    fl.read_exact(&mut lh).ok()?;
    if lh[0..4] != *b"PK\x03\x04" {
        return None;
    }
    Some(lho + 30 + u16::from_le_bytes([lh[26], lh[27]]) as u64 + u16::from_le_bytes([lh[28], lh[29]]) as u64)
}

/// Contenido de una entrada de un ZIP (sin comprimir o deflate), hasta `max` bytes descomprimidos.
pub fn zip_read(apk: &str, entry: &str, max: usize) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let (off, size, stored) = zip_entry(apk, entry)?;
    if size > 64 << 20 {
        return None;
    }
    let mut fl = std::fs::File::open(apk).ok()?;
    fl.seek(SeekFrom::Start(off)).ok()?;
    let mut data = vec![0u8; size as usize];
    fl.read_exact(&mut data).ok()?;
    if stored {
        return (data.len() <= max).then_some(data);
    }
    crate::pagecompat::inflate(&data, max)
}

fn split_apk(path: &str) -> Option<(&str, &str)> {
    path.split_once("!/")
}

/// Descriptor de archivo de una biblioteca: propio (se cierra al soltarlo) o del llamador
/// (ANDROID_DLEXT_USE_LIBRARY_FD, no se cierra).
struct Fd {
    fd: i32,
    owned: bool,
}

impl Drop for Fd {
    fn drop(&mut self) {
        if self.owned {
            unsafe { close(self.fd) };
        }
    }
}

impl Fd {
    /// `fstat`: (dispositivo, inodo, tamano).
    fn stat(&self) -> Result<(u64, u64, i64), i32> {
        // struct stat de x86-64: st_dev en 0, st_ino en 8, st_size en 48
        let mut st = [0u64; 18];
        if unsafe { syscall(5 /* fstat */, self.fd, st.as_mut_ptr()) } != 0 {
            return Err(unsafe { *__errno_location() });
        }
        Ok((st[0], st[1], st[6] as i64))
    }
    /// `pread64` completo (lo que haya hasta el final del archivo).
    fn pread(&self, buf: &mut [u8], off: i64) -> Result<usize, i32> {
        let mut done = 0;
        while done < buf.len() {
            let r = unsafe { pread(self.fd, buf[done..].as_mut_ptr() as *mut c_void, buf.len() - done, off + done as i64) };
            if r < 0 {
                let e = unsafe { *__errno_location() };
                if e == 4 {
                    continue; // EINTR
                }
                return Err(e);
            }
            if r == 0 {
                break;
            }
            done += r as usize;
        }
        Ok(done)
    }
    /// El archivo esta en un tmpfs? (bionic no comprueba la accesibilidad de lo que esta en tmpfs: `memfd_create`)
    fn on_tmpfs(&self) -> Result<bool, i32> {
        const TMPFS_MAGIC: i64 = 0x0102_1994;
        let mut buf = [0i64; 16];
        let r = unsafe { syscall(138 /* fstatfs */, self.fd, buf.as_mut_ptr()) };
        if r != 0 {
            return Err(unsafe { *__errno_location() });
        }
        Ok(buf[0] == TMPFS_MAGIC)
    }
}

/// Texto de `strerror` (lo que bionic escribe con `%m`).
fn strerror(e: i32) -> String {
    let s = std::io::Error::from_raw_os_error(e).to_string();
    match s.rfind(" (os error ") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn open_ro(path: &str) -> Option<Fd> {
    const O_CLOEXEC: i32 = 0o2000000;
    let c = std::ffi::CString::new(path).ok()?;
    loop {
        let fd = unsafe { open(c.as_ptr(), O_CLOEXEC) };
        if fd >= 0 {
            return Some(Fd { fd, owned: true });
        }
        if unsafe { *__errno_location() } != 4 {
            return None;
        }
    }
}

/// `realpath_fd` de bionic: la ruta que el kernel da para el descriptor (`/proc/self/fd/N`).
fn realpath_fd(fd: i32) -> Option<String> {
    std::fs::read_link(format!("/proc/self/fd/{}", fd)).ok().map(|p| p.to_string_lossy().into_owned())
}

/// Biblioteca abierta (bionic: el fd, `file_offset` y `realpath` de la `LoadTask`).
struct Opened {
    fd: Fd,
    off: i64,
    realpath: String,
    /// la ruta con la que se encontro (o la ruta real, si se abrio por descriptor)
    path: String,
}

/// `normalize_path` de bionic (linker_utils.cpp): ruta absoluta sin "//", "/./" ni "/../" (sin mirar el sistema de
/// archivos). None (con el aviso de bionic) si no es absoluta.
fn normalize_path(path: &str) -> Option<String> {
    let b = path.as_bytes();
    if b.first() != Some(&b'/') {
        crate::bridge::alog(&format!("normalize_path - invalid input: \"{}\", the input path should be absolute", path));
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' {
            let c1 = b.get(i + 1).copied().unwrap_or(0);
            if c1 == b'.' {
                let c2 = b.get(i + 2).copied().unwrap_or(0);
                if c2 == b'/' {
                    i += 2;
                    continue;
                } else if c2 == b'.' && matches!(b.get(i + 3).copied().unwrap_or(0), b'/' | 0) {
                    i += 3;
                    while let Some(c) = out.pop() {
                        if c == b'/' {
                            break;
                        }
                    }
                    if i >= b.len() {
                        out.push(b'/');
                    }
                    continue;
                }
            } else if c1 == b'/' {
                i += 1;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// `open_library_in_zipfile`: `ruta.apk!/entrada` (con la ruta normalizada) solo si la entrada esta almacenada sin
/// comprimir y alineada a pagina; si no, como si no existiera. La ruta real es la del APK y lo que sigue a "!/".
fn open_library_in_zipfile(input: &str) -> Option<Opened> {
    let norm = normalize_path(input)?;
    let (apk, entry) = split_apk(&norm)?;
    if norm.len() >= 512 {
        crate::bridge::alog(&format!("ignoring very long library path: {}", norm));
        return None;
    }
    let fd = open_ro(apk)?;
    let (off, _, stored) = zip_entry(apk, entry)?;
    if !stored || off % page() != 0 {
        return None;
    }
    let realpath = match realpath_fd(fd.fd) {
        Some(r) => format!("{}!/{}", r, entry),
        None => {
            crate::bridge::alog(&format!("unable to get realpath for the library \"{}\". Will use given path.", norm));
            norm.clone()
        }
    };
    Some(Opened { fd, off: off as i64, realpath, path: input.to_string() })
}

/// `open_library_at_path`: dentro de un APK o, si no, el archivo tal cual.
fn open_library_at_path(path: &str) -> Option<Opened> {
    if path.contains("!/") {
        if let Some(o) = open_library_in_zipfile(path) {
            return Some(o);
        }
    }
    let fd = open_ro(path)?;
    let realpath = realpath_fd(fd.fd).unwrap_or_else(|| path.to_string());
    Some(Opened { fd, off: 0, realpath, path: path.to_string() })
}

fn open_library_on_paths(name: &str, paths: &[String]) -> Option<Opened> {
    paths.iter().find_map(|d| open_library_at_path(&format!("{}/{}", d, name)))
}

/// `open_library` de bionic: con '/' la ruta tal cual; si no, las rutas LD_LIBRARY_PATH del espacio, luego el
/// DT_RUNPATH de quien la necesita (si lo encontrado es accesible desde el espacio) y luego las rutas por defecto.
fn open_library(ns: usize, name: &str, runpath: &[String]) -> Option<Opened> {
    if name.contains('/') {
        return open_library_at_path(name);
    }
    let (ld, def) = crate::namespace::search_paths(ns);
    if let Some(o) = open_library_on_paths(name, &ld) {
        return Some(o);
    }
    if let Some(o) = open_library_on_paths(name, runpath) {
        if crate::namespace::path_accessible(ns, &o.realpath) {
            return Some(o);
        }
    }
    open_library_on_paths(name, &def)
}

/// Bloqueo del cargador, reentrante por hilo: el constructor de una biblioteca puede llamar a dlopen (y el
/// destructor a dlclose), y resolver un simbolo puede cargar la biblioteca guest incrustada; con un Mutex simple eso
/// se bloqueaba para siempre.
fn with_loader<R>(f: impl FnOnce() -> R) -> R {
    thread_local! { static DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) }; }
    let outer = DEPTH.with(|d| d.get()) == 0;
    let _g = if outer { Some(LOADING.lock().unwrap_or_else(|e| e.into_inner())) } else { None };
    DEPTH.with(|d| d.set(d.get() + 1));
    let r = f();
    DEPTH.with(|d| d.set(d.get() - 1));
    r
}

/// dlopen en el espacio anonimo sin llamador (como `load_library_flags(name, 0)`).
pub fn load_library(name: &str) -> Result<Option<Arc<Module>>, String> {
    load_library_flags(name, 0)
}

/// dlopen con banderas de bionic desde fuera del guest (espacio anonimo, sin llamador). Ok(None) = biblioteca del
/// sistema servida por HLE (sin modulo ELF).
pub fn load_library_flags(name: &str, flags: u64) -> Result<Option<Arc<Module>>, String> {
    Ok(match open_lib(name, flags, crate::namespace::anonymous(), None, None)? {
        Dep::M(m) => Some(m),
        _ => None,
    })
}

/// `do_dlopen` de bionic en el espacio `ns` (el del llamador `caller`, el anonimo o el que pida ART). Devuelve la
/// biblioteca guest o del sistema; los errores llevan el texto de bionic sin el prefijo "dlopen failed: ".
///  * RTLD_NOLOAD: solo devuelve el handle si ya esta cargada.
///  * RTLD_NODELETE y RTLD_GLOBAL: como en bionic, se aplican a las bibliotecas que esta llamada carga de nuevo (en
///    una ya cargada se ignoran) y su grupo no se descarga nunca.
///  * `ext`: el `android_dlextinfo` de `android_dlopen_ext`, con todas sus opciones (ver `ExtInfo`).
pub fn open_lib(name: &str, flags: u64, mut ns: usize, caller: Option<Arc<Module>>, ext: Option<&ExtInfo>) -> Result<Dep, String> {
    if flags & !(RTLD_NOW | RTLD_LAZY | RTLD_GLOBAL | RTLD_NODELETE | RTLD_NOLOAD) != 0 {
        return Err(format!("invalid flags to dlopen: {:x}", flags));
    }
    if let Some(e) = ext {
        if e.flags & !DLEXT_VALID_FLAG_BITS != 0 {
            return Err(format!("invalid extended flags to android_dlopen_ext: 0x{:x}", e.flags));
        }
        if e.flags & DLEXT_USE_LIBRARY_FD == 0 && e.flags & DLEXT_USE_LIBRARY_FD_OFFSET != 0 {
            return Err(format!(
                "invalid extended flag combination (ANDROID_DLEXT_USE_LIBRARY_FD_OFFSET without ANDROID_DLEXT_USE_LIBRARY_FD): 0x{:x}",
                e.flags
            ));
        }
        if e.flags & DLEXT_USE_NAMESPACE != 0 {
            if e.library_namespace == 0 {
                return Err("ANDROID_DLEXT_USE_NAMESPACE is set but extinfo->library_namespace is null".into());
            }
            // bionic usaria el puntero tal cual; aqui un valor que no es un espacio se rechaza
            ns = crate::namespace::from_handle(e.library_namespace).ok_or("invalid extended library namespace")?;
        }
    }
    with_loader(|| {
        let r = find_libraries(name, flags, ns, caller.as_ref(), ext.copied())?;
        if let Dep::M(m) = &r {
            // find_library: una referencia mas en la raiz de su grupo; despues los constructores pendientes
            group_root(m).refs.fetch_add(1, SeqCst);
            call_constructors(m);
        }
        Ok(r)
    })
}

/// Una DT_NEEDED (o la biblioteca pedida) por resolver: nombre, quien la necesita y el espacio desde el que se busca
/// (el de quien la necesita: si se cargo en un espacio enlazado, ese).
struct Task {
    name: String,
    /// quien la necesita o, en la biblioteca pedida, quien llamo a dlopen (su DT_RUNPATH tambien se usa)
    needed_by: Option<Node>,
    /// es una DT_NEEDED (no la biblioteca pedida a dlopen)
    dt_needed: bool,
    start_ns: usize,
    /// el `android_dlextinfo` de la llamada (solo en la biblioteca pedida: bionic no lo pasa a las DT_NEEDED)
    ext: Option<ExtInfo>,
}

/// Modulos guest del espacio `ns` (primario o secundario).
fn ns_modules(ns: usize) -> Vec<Arc<Module>> {
    crate::monitor::lock(&MODULES).iter().filter(|m| member(m, ns)).cloned().collect()
}

/// A que se resolvio una tarea: una biblioteca ya cargada (o del sistema) o una nueva de esta carga, aun sin mapear
/// (indice en `Load::pre`).
#[derive(Clone)]
enum Node {
    Old(Dep),
    New(usize),
}

/// Biblioteca nueva de una carga, leida pero sin mapear (bionic: el soinfo que crea `load_library` en el paso 1 con
/// lo que el ElfReader leyo del archivo: DT_NEEDED, DT_SONAME, DT_RUNPATH). Se mapea en el paso 2, en orden aleatorio.
struct Pre {
    path: String,
    name: String,
    realpath: String,
    file_id: (u64, u64, u64),
    ns: usize,
    /// el archivo abierto y donde empieza la biblioteca dentro de el (y su tamano total)
    fd: Fd,
    file_offset: i64,
    file_size: i64,
    eh: Ehdr,
    phdrs: Vec<Phdr>,
    /// menor y mayor p_align valido de los PT_LOAD (`CheckProgramHeaderAlignment`)
    min_align: u64,
    max_align: u64,
    /// cargar en el modo de compatibilidad de bionic para p_align de 4 KiB con paginas de 16 KiB
    compat: bool,
    /// nota NT_ANDROID_TYPE_PAD_SEGMENT con 1 (`should_pad_segments_`): segmentos extendidos sin huecos
    pad: bool,
    soname: String,
    needed: Vec<String>,
    runpath: Vec<String>,
    /// la leyo una DT_NEEDED (no es la biblioteca pedida a dlopen)
    dt_needed: bool,
}

/// Estado de una carga (`find_libraries`): tareas en anchura, a que se resolvio cada una, lo nuevo (leido en el paso
/// 1, mapeado en el 2) y los modulos nuevos en el orden del paso 1.
struct Load {
    tasks: Vec<Task>,
    sis: Vec<Node>,
    pre: Vec<Pre>,
    fresh: Vec<Arc<Module>>,
    flags: u64,
    ext: Option<ExtInfo>,
}

impl Load {
    fn realpath_of(&self, n: &Node) -> String {
        match n {
            Node::Old(Dep::M(m)) => m.realpath.clone(),
            Node::New(i) => self.pre[*i].realpath.clone(),
            Node::Old(Dep::Sys(l)) => sys_realpath(l),
            _ => "(unknown)".into(),
        }
    }
    fn runpath_of(&self, n: &Node) -> Vec<String> {
        match n {
            Node::Old(Dep::M(m)) => m.runpath.clone(),
            Node::New(i) => self.pre[*i].runpath.clone(),
            _ => Vec::new(),
        }
    }
    fn ns_of(&self, n: &Node) -> Option<usize> {
        match n {
            Node::Old(Dep::M(m)) => Some(m.ns),
            Node::New(i) => Some(self.pre[*i].ns),
            _ => None,
        }
    }
    fn module_of(&self, n: &Node) -> Option<Arc<Module>> {
        match n {
            Node::Old(Dep::M(m)) => Some(m.clone()),
            Node::New(i) => self.fresh.get(*i).cloned(),
            _ => None,
        }
    }
    fn dep_of(&self, n: &Node) -> Dep {
        match n {
            Node::Old(d) => d.clone(),
            Node::New(i) => Dep::M(self.fresh[*i].clone()),
        }
    }

    /// `find_loaded_library_by_soname`: biblioteca ya cargada en `ns` (o en un espacio enlazado cuyo enlace la deja
    /// pasar), tambien una nueva de esta carga, cuyo soname es `name`. Un nombre con '/' nunca se compara por soname,
    /// y una biblioteca sin DT_SONAME (soname vacio, ver `Module::soname`) no coincide con ninguno. Las del sistema
    /// estan cargadas en los espacios de sistema.
    fn loaded_by_soname(&self, ns: usize, name: &str, search_links: bool) -> Option<Node> {
        if name.contains('/') || name.is_empty() {
            return None;
        }
        let n = crate::namespace::get(ns)?;
        if n.system {
            if let Some(l) = sys_lib(name).filter(|&l| l == name) {
                return Some(Node::Old(Dep::Sys(l)));
            }
        }
        if let Some(m) = ns_modules(ns).into_iter().find(|m| m.soname == name) {
            return Some(Node::Old(Dep::M(m)));
        }
        if let Some(i) = self.pre.iter().position(|p| p.ns == ns && p.soname == name) {
            return Some(Node::New(i));
        }
        if search_links {
            for l in n.links.iter().filter(|l| l.accessible(name)) {
                if let Some(d) = self.loaded_by_soname(l.to, name, false) {
                    return Some(d);
                }
            }
        }
        None
    }

    /// `find_loaded_library_by_inode`: el mismo archivo (y desplazamiento) ya cargado en `ns` con otro nombre o ruta
    /// (tambien en esta carga), o en un espacio enlazado que deja pasar su soname.
    fn loaded_by_inode(&self, ns: usize, id: (u64, u64, u64), search_links: bool) -> Option<Node> {
        if id.0 == 0 {
            return None;
        }
        let find = |ns: usize, ok: &dyn Fn(&str) -> bool| -> Option<Node> {
            if let Some(m) = ns_modules(ns).into_iter().find(|m| m.file_id == id && ok(&m.soname)) {
                return Some(Node::Old(Dep::M(m)));
            }
            self.pre.iter().position(|p| p.ns == ns && p.file_id == id && ok(&p.soname)).map(Node::New)
        };
        if let Some(n) = find(ns, &|_| true) {
            return Some(n);
        }
        if search_links {
            for l in crate::namespace::get(ns)?.links.iter() {
                if let Some(n) = find(l.to, &|s| l.accessible(s)) {
                    return Some(n);
                }
            }
        }
        None
    }
}

/// `find_libraries` de bionic para un dlopen: lectura en anchura de la biblioteca y sus DT_NEEDED (reutilizando las
/// ya cargadas, tambien las de esta misma carga: los ciclos valen), mapeo de las nuevas en orden aleatorio, enlazado
/// de cada grupo local y referencias entre grupos. Si algo falla, se descarga lo cargado (bionic `soinfo_unload` de
/// la raiz).
fn find_libraries(name: &str, flags: u64, ns: usize, caller: Option<&Arc<Module>>, ext: Option<ExtInfo>) -> Result<Dep, String> {
    let root_task = Task { name: name.to_string(), needed_by: caller.map(|c| Node::Old(Dep::M(c.clone()))), dt_needed: false, start_ns: ns, ext };
    let mut ld = Load { tasks: vec![root_task], sis: Vec::new(), pre: Vec::new(), fresh: Vec::new(), flags, ext };
    let r = load_and_link(&mut ld, ns);
    match r {
        Ok(()) => Ok(ld.dep_of(&ld.sis[0])),
        Err(e) => {
            if let Some(root) = ld.sis.first().and_then(|n| ld.module_of(n)) {
                if !root.is_live() {
                    soinfo_unload(&root);
                }
            }
            // lo que no alcanzo la descarga (no deberia quedar nada: todo lo nuevo cuelga de la raiz)
            for m in ld.fresh.iter().filter(|m| !m.is_live()) {
                if crate::monitor::lock(&MODULES).iter().any(|o| Arc::ptr_eq(o, m)) {
                    free_module(m);
                }
            }
            Err(e)
        }
    }
}

/// `find_library_internal`: ya cargada (por soname, tambien en espacios enlazados), leida ahora en `ns`, en el espacio
/// por defecto si es de la lista de excepciones (`is_exempt_lib`, espacios con EXEMPT_LIST_ENABLED) o, si no se
/// encuentra, en un espacio enlazado que la deje pasar. Si nada vale, el error es el de `ns`.
fn find_library_internal(ld: &mut Load, i: usize) -> Result<Node, String> {
    let (mut ns, name) = (ld.tasks[i].start_ns, ld.tasks[i].name.clone());
    if let Some(d) = ld.loaded_by_soname(ns, &name, true) {
        return Ok(d);
    }
    let err = match load_one(ld, i, ns, true) {
        Ok(d) => return Ok(d),
        Err(e) => e,
    };
    let nb = ld.tasks[i].needed_by.clone();
    if crate::namespace::exempt_list_enabled(ns) && ld.is_exempt_lib(ns, &name, nb.as_ref()) {
        // bionic: se reintenta desde el espacio por defecto, y sus enlaces son los que se recorren despues
        ns = crate::namespace::DEFAULT;
        if let Ok(d) = load_one(ld, i, ns, true) {
            return Ok(d);
        }
    }
    let links = crate::namespace::get(ns).map(|n| n.links).unwrap_or_default();
    for l in links {
        // find_library_in_linked_namespace: el soname es el del ya cargado o, si no, el nombre del archivo
        let cand = ld.loaded_by_soname(l.to, &name, false);
        let soname = match &cand {
            Some(Node::Old(Dep::M(m))) => m.soname.clone(),
            Some(Node::New(j)) => ld.pre[*j].soname.clone(),
            _ => name.rsplit('/').next().unwrap_or(&name).to_string(),
        };
        if !l.accessible(&soname) {
            continue;
        }
        if let Some(d) = cand {
            return Ok(d);
        }
        if let Ok(d) = load_one(ld, i, l.to, false) {
            return Ok(d);
        }
    }
    Err(err)
}

/// Bibliotecas de la lista de excepciones de bionic (`kLibraryExemptList`, b/26394120).
const EXEMPT_LIBS: &[&str] = &[
    "libandroid_runtime.so",
    "libbinder.so",
    "libcrypto.so",
    "libcutils.so",
    "libexpat.so",
    "libgui.so",
    "libmedia.so",
    "libnativehelper.so",
    "libssl.so",
    "libstagefright.so",
    "libsqlite.so",
    "libui.so",
    "libutils.so",
];

/// `kSystemLibDir` de bionic en LP64.
const SYSTEM_LIB_DIR: &str = "/system/lib64";

/// `is_system_library`: la ruta real esta en una de las rutas por defecto del espacio por defecto.
fn is_system_library(realpath: &str) -> bool {
    let (_, def) = crate::namespace::search_paths(crate::namespace::DEFAULT);
    def.iter().any(|d| realpath.strip_prefix(d.as_str()).and_then(|r| r.strip_prefix('/')).map_or(false, |r| !r.is_empty() && !r.contains('/')))
}

/// `maybe_accessible_via_namespace_links`: algun enlace de `ns` deja pasar el nombre de archivo de `name`.
fn maybe_accessible_via_namespace_links(ns: usize, name: &str) -> bool {
    let soname = name.rsplit('/').next().unwrap_or(name);
    crate::namespace::get(ns).map_or(false, |n| n.links.iter().any(|l| l.accessible(soname)))
}

impl Load {
    /// `is_exempt_lib` de bionic: con targetSdkVersion < 24, las bibliotecas de la lista (por nombre, o por ruta en
    /// /system/lib64) y cualquiera que necesite una biblioteca del sistema si ningun enlace de `ns` la deja pasar.
    fn is_exempt_lib(&self, ns: usize, name: &str, needed_by: Option<&Node>) -> bool {
        if crate::bridge::target_sdk() >= 24 {
            return false;
        }
        if let Some(nb) = needed_by {
            if is_system_library(&self.realpath_of(nb)) {
                return !maybe_accessible_via_namespace_links(ns, name);
            }
        }
        let name = match name.rsplit_once('/') {
            Some((dir, base)) if name.starts_with('/') && dir == SYSTEM_LIB_DIR => base,
            _ => name,
        };
        EXEMPT_LIBS.contains(&name)
    }
}

/// `load_library` de bionic: abre el archivo con las rutas de `ns` (o usa el descriptor de
/// ANDROID_DLEXT_USE_LIBRARY_FD), lo reutiliza si ya esta cargado (inodo, salvo ANDROID_DLEXT_FORCE_LOAD), comprueba
/// RTLD_NOLOAD y la accesibilidad del espacio y, si no, lo lee (sin mapearlo) y anade sus DT_NEEDED.
fn load_one(ld: &mut Load, i: usize, ns: usize, search_links: bool) -> Result<Node, String> {
    let n = ld.tasks[i].name.clone();
    let needed_by = ld.tasks[i].needed_by.clone();
    let dt_needed = ld.tasks[i].dt_needed;
    let ext = ld.tasks[i].ext;
    let nsd = crate::namespace::get(ns).ok_or_else(|| format!("library \"{}\" not found", n))?;
    let opened = match ext.filter(|e| e.flags & DLEXT_USE_LIBRARY_FD != 0) {
        Some(e) => {
            let off = if e.flags & DLEXT_USE_LIBRARY_FD_OFFSET != 0 { e.library_fd_offset } else { 0 };
            let realpath = realpath_fd(e.library_fd).unwrap_or_else(|| {
                crate::bridge::alog(&format!("unable to get realpath for the library \"{}\" by extinfo->library_fd. Will use given name.", n));
                n.clone()
            });
            Opened { fd: Fd { fd: e.library_fd, owned: false }, off, path: realpath.clone(), realpath }
        }
        None => {
            // biblioteca del sistema pedida por ruta en un espacio de sistema
            if nsd.system {
                if let Some(l) = sys_lib(&n) {
                    return Ok(Node::Old(Dep::Sys(l)));
                }
            }
            let runpath = needed_by.as_ref().map(|nb| ld.runpath_of(nb)).unwrap_or_default();
            match open_library(ns, &n, &runpath) {
                Some(o) => o,
                // Cerrado por defecto: una biblioteca del sistema que no esta en la lista de permitidas NO se reenvia
                // al host aunque el host la tenga (ver boundary::lib_allowed).
                None => {
                    return Err(match needed_by.as_ref().filter(|_| dt_needed) {
                        Some(nb) => format!("library \"{}\" not found: needed by {} in namespace {}", n, ld.realpath_of(nb), nsd.name),
                        None => format!("library \"{}\" not found", n),
                    })
                }
            }
        }
    };
    let off = opened.off;
    if off % page() as i64 != 0 {
        return Err(format!("file offset for the library \"{}\" is not page-aligned: {}", n, off));
    }
    if off < 0 {
        return Err(format!("file offset for the library \"{}\" is negative: {}", n, off));
    }
    let (dev, ino, size) = opened.fd.stat().map_err(|e| format!("unable to stat file for the library \"{}\": {}", n, strerror(e)))?;
    if off >= size {
        return Err(format!("file offset for the library \"{}\" >= file size: {} >= {}", n, off, size));
    }
    let fid = (dev, ino, off as u64);
    if ext.map_or(true, |e| e.flags & DLEXT_FORCE_LOAD == 0) {
        if let Some(m) = ld.loaded_by_inode(ns, fid, search_links) {
            return Ok(m);
        }
    }
    if ld.flags & RTLD_NOLOAD != 0 {
        return Err(format!("library \"{}\" wasn't loaded and RTLD_NOLOAD prevented it", n));
    }
    let tmpfs = opened.fd.on_tmpfs().map_err(|e| format!("unable to fstatfs file for the library \"{}\": {}", n, strerror(e)))?;
    if !tmpfs && !crate::namespace::path_accessible(ns, &opened.realpath) {
        let nb_dt = needed_by.as_ref().filter(|_| dt_needed);
        let by = needed_by.as_ref().map_or_else(|| "(unknown)".to_string(), |m| ld.realpath_of(m));
        if ld.is_exempt_lib(ns, &n, nb_dt) {
            // bionic: con targetSdkVersion < 24 solo un aviso (DL_ERROR_AFTER(24, ...)), y se carga
            if nb_dt.map_or(true, |nb| !is_system_library(&ld.realpath_of(nb))) {
                crate::bridge::alog(&format!(
                    "library \"{}\" (\"{}\") needed or dlopened by \"{}\" is not accessible by namespace \"{}\"",
                    n, opened.realpath, by, nsd.name
                ));
            }
        } else {
            return Err(format!("library \"{}\" needed or dlopened by \"{}\" is not accessible for the namespace \"{}\"", n, by, nsd.name));
        }
    }
    let base_name = n.rsplit('/').next().unwrap_or(&n).to_string();
    let p = read_pre(opened, size, &base_name, fid, ns, dt_needed)?;
    let idx = ld.pre.len();
    for d in &p.needed {
        ld.tasks.push(Task { name: d.clone(), needed_by: Some(Node::New(idx)), dt_needed: true, start_ns: ns, ext: None });
    }
    ld.pre.push(p);
    Ok(Node::New(idx))
}

fn load_and_link(ld: &mut Load, ns: usize) -> Result<(), String> {
    // 1. en anchura: cada tarea se resuelve a una biblioteca ya cargada o a una nueva (leida, con sus DT_NEEDED)
    let mut i = 0;
    while i < ld.tasks.len() {
        let si = find_library_internal(ld, i)?;
        ld.sis.push(si);
        i += 1;
    }
    // 2. mapeo de las nuevas en orden aleatorio (bionic: "Load libraries in random order", b/24047022; en orden si
    //    van todas a la region del llamador, ANDROID_DLEXT_RESERVED_ADDRESS_RECURSIVE); se registran en el orden del
    //    paso 1 (el de la lista de soinfo de bionic: dlsym con RTLD_DEFAULT, dl_iterate_phdr)
    let ext = ld.ext;
    let recursive = ext.map_or(false, |e| e.flags & DLEXT_RESERVED_ADDRESS_RECURSIVE != 0);
    let mut order: Vec<usize> = (0..ld.pre.len()).collect();
    if !recursive {
        for k in 0..order.len() {
            let n = order.len() - k;
            let r = rand_below(n as u64) as usize;
            order.swap(n - 1, r);
        }
    }
    let (mut ext_as, mut def_as) = (AddrSpace::default(), AddrSpace::default());
    if let Some(e) = ext {
        if e.flags & (DLEXT_RESERVED_ADDRESS | DLEXT_RESERVED_ADDRESS_HINT) != 0 {
            ext_as = AddrSpace { start: e.reserved_addr, size: e.reserved_size, must_use: e.flags & DLEXT_RESERVED_ADDRESS != 0 };
        }
    }
    let mut mapped: Vec<Option<Module>> = (0..ld.pre.len()).map(|_| None).collect();
    for &j in &order {
        let asp = if recursive || !ld.pre[j].dt_needed { &mut ext_as } else { &mut def_as };
        mapped[j] = Some(map_module(&ld.pre[j], ld.flags, asp)?);
    }
    for m in mapped.into_iter().map(|m| Arc::new(m.unwrap())) {
        for (lo, hi) in exec_ranges(&m) {
            CODE_MIN.fetch_min(lo, SeqCst);
            CODE_MAX.fetch_max(hi, SeqCst);
        }
        // registrado (aun sin enlazar: no visible para dlsym) para que is_guest_code valga durante la relocacion
        // (resolutores IRELATIVE)
        crate::monitor::lock(&MODULES).push(m.clone());
        ld.fresh.push(m);
    }
    // 3. hijos y padres (bionic `add_child`)
    for (t, si) in ld.tasks.iter().zip(ld.sis.iter()) {
        if !t.dt_needed {
            continue;
        }
        let (Some(nb), child) = (t.needed_by.as_ref().and_then(|n| ld.module_of(n)), ld.dep_of(si)) else { continue };
        if let Dep::M(m) = &child {
            m.parents.lock().unwrap().push(nb.handle);
        }
        nb.children.lock().unwrap().push(child);
    }
    let Dep::M(root) = ld.dep_of(&ld.sis[0]) else {
        return Ok(());
    };
    let is_fresh = |fresh: &[Arc<Module>], m: &Arc<Module>| !m.is_live() && fresh.iter().any(|f| Arc::ptr_eq(f, m));
    // 4. raices de los grupos locales: la de la carga y cada biblioteca nueva que cruza a otro espacio de nombres
    let mut roots = vec![root.clone()];
    for (t, si) in ld.tasks.iter().zip(ld.sis.iter()) {
        if let Node::New(j) = si {
            let m = &ld.fresh[*j];
            let needed_by_ns = t.needed_by.as_ref().filter(|_| t.dt_needed).and_then(|n| ld.ns_of(n)).unwrap_or(ns);
            if m.ns != needed_by_ns && !roots.iter().any(|r| Arc::ptr_eq(r, m)) {
                roots.push(m.clone());
            }
        }
    }
    // 5. TLS de lo nuevo en anchura (bionic `register_soinfo_tls` en el prelink de cada tarea)
    for m in ld.fresh.iter() {
        register_tls(m)?;
    }
    // 6. enlazado de cada grupo local, en anchura desde su raiz: solo lo nuevo de su propio espacio. El extinfo
    //    (RELRO compartido) solo vale para la biblioteca pedida, o para todas con RESERVED_ADDRESS_RECURSIVE.
    let sdk = crate::bridge::target_sdk();
    let mut relro_fd_offset = 0u64;
    for r in roots.iter() {
        for d in local_deps(r, r.ns) {
            let Dep::M(si) = d else { continue };
            if !is_fresh(&ld.fresh, &si) || si.ns != r.ns {
                continue;
            }
            if !Arc::ptr_eq(&si, r) {
                *si.group_root.lock().unwrap() = Arc::downgrade(r);
            }
            si.target_sdk.store(sdk, SeqCst);
            let list = lookup_list(&si, r);
            relocate(&si, &list)?;
            protect_relro(&si);
            if let Some(e) = ext.filter(|_| Arc::ptr_eq(&si, &root) || recursive) {
                share_relro(&si, &e, &mut relro_fd_offset)?;
            }
            LOADS.fetch_add(1, SeqCst);
        }
    }
    // 7. enlazados; referencias entre grupos distintos
    for m in ld.fresh.iter() {
        m.live.store(true, SeqCst);
    }
    for (t, si) in ld.tasks.iter().zip(ld.sis.iter()) {
        if !t.dt_needed {
            continue;
        }
        if let (Some(nb), Dep::M(si)) = (t.needed_by.as_ref().and_then(|n| ld.module_of(n)), ld.dep_of(si)) {
            if !Arc::ptr_eq(&group_root(&nb), &group_root(&si)) {
                group_root(&si).refs.fetch_add(1, SeqCst);
            }
        }
    }
    Ok(())
}

/// `DL_ERROR_AFTER(api, ...)` de bionic: error desde ese targetSdkVersion; por debajo, solo un aviso.
fn error_after(api: i32, msg: String) -> Result<(), String> {
    if crate::bridge::target_sdk() >= api {
        return Err(msg);
    }
    crate::bridge::alog(&format!("Warning: {} and will not work when the app moves to API level {} or later", msg, api));
    Ok(())
}

/// `VerifyElfHeader` de bionic en LP64 para arm64.
fn verify_header(eh: &Ehdr, name: &str) -> Result<(), String> {
    let id = &eh.ident;
    if &id[0..4] != b"\x7fELF" {
        return Err(format!("\"{}\" has bad ELF magic: {:02x}{:02x}{:02x}{:02x}", name, id[0], id[1], id[2], id[3]));
    }
    match id[4] {
        2 => {}
        1 => return Err(format!("\"{}\" is 32-bit instead of 64-bit", name)),
        c => return Err(format!("\"{}\" has unknown ELF class: {}", name, c)),
    }
    if id[5] != 1 {
        return Err(format!("\"{}\" not little-endian: {}", name, id[5]));
    }
    if eh.typ != 3 {
        return Err(format!("\"{}\" has unexpected e_type: {}", name, eh.typ));
    }
    if eh.version != 1 {
        return Err(format!("\"{}\" has unexpected e_version: {}", name, eh.version));
    }
    if eh.machine != 183 {
        let em = |m: u16| match m {
            3 => "EM_386",
            183 => "EM_AARCH64",
            40 => "EM_ARM",
            243 => "EM_RISCV",
            62 => "EM_X86_64",
            _ => "EM_???",
        };
        return Err(format!("\"{}\" is for {} ({}) instead of {} ({})", name, em(eh.machine), eh.machine, em(183), 183));
    }
    if eh.shentsize != 64 {
        error_after(26, format!("\"{}\" has unsupported e_shentsize: 0x{:x} (expected 0x{:x})", name, eh.shentsize, 64))?;
    }
    if eh.shstrndx == 0 {
        error_after(26, format!("\"{}\" has invalid e_shstrndx", name))?;
    }
    Ok(())
}

/// Lee `n` estructuras `T` del archivo (`MappedFileFragment::Map` de bionic, aqui con `pread`).
fn read_structs<T: Copy>(fd: &Fd, off: i64, n: usize) -> Result<Vec<T>, i32> {
    let sz = std::mem::size_of::<T>();
    let mut buf = vec![0u8; n * sz];
    let got = fd.pread(&mut buf, off)?;
    if got != buf.len() {
        return Err(5); // EIO: el archivo se acorto despues de comprobar su tamano
    }
    Ok((0..n).map(|i| unsafe { std::ptr::read_unaligned(buf.as_ptr().add(i * sz) as *const T) }).collect())
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Shdr {
    name: u32,
    typ: u32,
    flags: u64,
    addr: u64,
    offset: u64,
    size: u64,
    link: u32,
    info: u32,
    addralign: u64,
    entsize: u64,
}

/// Paso 1 de la carga: `ElfReader::Read` de bionic (cabecera, program headers, alineacion, section headers, seccion
/// dinamica y su tabla de cadenas, con `pread` y las comprobaciones y textos de bionic) y lo que `load_library` toma
/// de la seccion dinamica: DT_NEEDED, DT_SONAME y DT_RUNPATH.
fn read_pre(o: Opened, file_size: i64, name: &str, file_id: (u64, u64, u64), ns: usize, dt_needed: bool) -> Result<Pre, String> {
    let rp = o.realpath.clone();
    let (fd, off) = (&o.fd, o.off);
    let mut hb = [0u8; 64];
    let rc = fd.pread(&mut hb, off).map_err(|e| format!("can't read file \"{}\": {}", rp, strerror(e)))?;
    if rc != hb.len() {
        return Err(format!("\"{}\" is too small to be an ELF executable: only found {} bytes", rp, rc));
    }
    let eh: Ehdr = unsafe { std::ptr::read_unaligned(hb.as_ptr() as *const Ehdr) };
    verify_header(&eh, &rp)?;
    // CheckFileRange: dentro del archivo (desde la biblioteca), nunca en 0 y alineado
    let in_file = |o2: u64, size: u64, align: u64| -> bool {
        let Some(start) = (off as u64).checked_add(o2) else { return false };
        let Some(end) = start.checked_add(size) else { return false };
        o2 > 0 && (start as i64) < file_size && end as i64 <= file_size && o2 % align == 0
    };
    let phnum = eh.phnum as usize;
    if phnum < 1 || phnum > 65536 / 56 {
        return Err(format!("\"{}\" has invalid e_phnum: {}", rp, phnum));
    }
    if !in_file(eh.phoff, (phnum * 56) as u64, 8) {
        return Err(format!("\"{}\" has invalid phdr offset/size: {}/{}", rp, eh.phoff, phnum * 56));
    }
    let phdrs: Vec<Phdr> = read_structs(fd, off + eh.phoff as i64, phnum).map_err(|e| format!("\"{}\" phdr mmap failed: {}", rp, strerror(e)))?;
    // CheckProgramHeaderAlignment
    let (mut min_align, mut max_align) = (page(), page());
    for (i, p) in phdrs.iter().enumerate().filter(|(_, p)| p.typ == PT_LOAD) {
        if p.align & p.align.wrapping_sub(1) != 0 {
            crate::bridge::alog(&format!("\"{}\" has invalid p_align {:x} in phdr {}", rp, p.align, i));
            continue;
        }
        max_align = max_align.max(p.align);
        if p.align > 1 {
            min_align = min_align.min(p.align);
        }
    }
    // ReadSectionHeaders
    let shnum = eh.shnum as usize;
    if shnum == 0 {
        return Err(format!("\"{}\" has no section headers", rp));
    }
    if !in_file(eh.shoff, (shnum * 64) as u64, 8) {
        return Err(format!("\"{}\" has invalid shdr offset/size: {}/{}", rp, eh.shoff, shnum * 64));
    }
    let shdrs: Vec<Shdr> = read_structs(fd, off + eh.shoff as i64, shnum).map_err(|e| format!("\"{}\" shdr mmap failed: {}", rp, strerror(e)))?;
    // ReadDynamicSection
    let dsh = *shdrs.iter().find(|s| s.typ == SHT_DYNAMIC).ok_or_else(|| format!("\"{}\" .dynamic section header was not found", rp))?;
    let (pt_off, pt_sz) = phdrs.iter().filter(|p| p.typ == PT_DYNAMIC).last().map_or((0, 0), |p| (p.offset, p.filesz));
    if pt_off != dsh.offset {
        error_after(26, format!("\"{}\" .dynamic section has invalid offset: 0x{:x}, expected to match PT_DYNAMIC offset: 0x{:x}", rp, dsh.offset, pt_off))?;
    }
    if pt_sz != dsh.size {
        error_after(26, format!("\"{}\" .dynamic section has invalid size: 0x{:x} (expected to match PT_DYNAMIC filesz 0x{:x})", rp, dsh.size, pt_sz))?;
    }
    if dsh.link as usize >= shnum {
        return Err(format!("\"{}\" .dynamic section has invalid sh_link: {}", rp, dsh.link));
    }
    let ssh = shdrs[dsh.link as usize];
    if ssh.typ != SHT_STRTAB {
        return Err(format!("\"{}\" .dynamic section has invalid link({}) sh_type: {} (expected SHT_STRTAB)", rp, dsh.link, ssh.typ));
    }
    if !in_file(dsh.offset, dsh.size, 8) {
        return Err(format!("\"{}\" has invalid offset/size of .dynamic section", rp));
    }
    let dynamic: Vec<[u64; 2]> = read_structs(fd, off + dsh.offset as i64, dsh.size as usize / 16).map_err(|e| format!("\"{}\" dynamic section mmap failed: {}", rp, strerror(e)))?;
    if !in_file(ssh.offset, ssh.size, 1) {
        return Err(format!("\"{}\" has invalid offset/size of the .strtab section linked from .dynamic section", rp));
    }
    let strtab: Vec<u8> = read_structs(fd, off + ssh.offset as i64, ssh.size as usize).map_err(|e| format!("\"{}\" strtab section mmap failed: {}", rp, strerror(e)))?;
    // ElfReader::get_string (bionic comprueba el indice con CHECK: aqui un error)
    let s = |v: u64| -> Result<String, String> {
        let rest = strtab.get(v as usize..).ok_or_else(|| format!("\"{}\": string index {} out of range of .dynstr", rp, v))?;
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        Ok(String::from_utf8_lossy(&rest[..end]).into_owned())
    };
    let (mut needed, mut soname_off, mut runpath_off) = (Vec::new(), None, None);
    for &[tag, val] in dynamic.iter() {
        match tag as i64 {
            DT_NULL => break,
            DT_NEEDED => needed.push(s(val)?),
            DT_SONAME => soname_off = Some(val),
            DT_RUNPATH => runpath_off = Some(val),
            _ => {}
        }
    }
    let soname = match soname_off {
        Some(v) => s(v)?,
        // bionic (prelink_image): antes de la API 23 el nombre del archivo hacia de DT_SONAME
        None if crate::bridge::target_sdk() < 23 => rp.rsplit('/').next().unwrap_or("").to_string(),
        None => String::new(),
    };
    // soinfo::set_dt_runpath: rutas separadas por ':', $ORIGIN = directorio de la ruta real, $LIB = lib64
    let runpath = match runpath_off {
        Some(v) => {
            let origin = rp.rsplit_once('/').map_or(".".to_string(), |(d, _)| d.to_string());
            s(v)?
                .split(':')
                .filter(|p| !p.is_empty())
                .map(|p| p.replace("${ORIGIN}", &origin).replace("$ORIGIN", &origin).replace("${LIB}", "lib64").replace("$LIB", "lib64"))
                .collect()
        }
        None => Vec::new(),
    };
    let pad = read_pad_segment_note(fd, off, file_size, &phdrs, &rp)?;
    let compat = page() == 16384 && min_align == 4096 && crate::mem::page_compat(&rp);
    Ok(Pre {
        pad,
        path: o.path.clone(),
        name: name.to_string(),
        realpath: rp,
        file_id,
        ns,
        file_offset: off,
        file_size,
        fd: o.fd,
        eh,
        phdrs,
        compat,
        min_align,
        max_align,
        soname,
        needed,
        runpath,
        dt_needed,
    })
}

/// `ReadPadSegmentNote` de bionic: la nota "Android" de tipo NT_ANDROID_TYPE_PAD_SEGMENT (4) en un PT_NOTE, con
/// descriptor 1 = segmentos extendidos. Solo si el kernel admite la migracion de tamano de pagina.
fn read_pad_segment_note(fd: &Fd, file_offset: i64, file_size: i64, phdrs: &[Phdr], rp: &str) -> Result<bool, String> {
    if !page_size_migration_supported() {
        return Ok(false);
    }
    for (i, p) in phdrs.iter().enumerate() {
        if p.typ != PT_NOTE || p.memsz == 0 {
            continue;
        }
        let end = (file_offset as u64).checked_add(p.offset).and_then(|v| v.checked_add(p.filesz));
        if end.map_or(true, |e| e > file_size as u64) || p.filesz != p.memsz {
            if crate::bridge::target_sdk() < 37 {
                continue; // b/390328213: notas invalidas en apps publicadas
            }
            return Err(format!("\"{}\": ELF note (phdr {}) runs off end of file", rp, i));
        }
        let data: Vec<u8> = read_structs(fd, file_offset + p.offset as i64, p.filesz as usize)
            .map_err(|e| format!("\"{}\": PT_NOTE mmap(nullptr, {:#x}, PROT_READ, MAP_PRIVATE, {}, {:#x}) failed: {}", rp, p.filesz, fd.fd, page_start(file_offset as u64 + p.offset), strerror(e)))?;
        // __get_elf_note
        let u32at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
        let mut q = 0usize;
        while q + 12 <= data.len() {
            let (namesz, descsz, typ) = (u32at(q) as usize, u32at(q + 4) as usize, u32at(q + 8));
            let name_at = q + 12;
            let Some(desc_at) = name_at.checked_add((namesz + 3) & !3) else { break };
            let Some(next) = desc_at.checked_add((descsz + 3) & !3) else { break };
            if next > data.len() {
                break;
            }
            if typ == 4 && namesz == 8 && &data[name_at..name_at + 8] == b"Android\0" {
                if descsz != 4 {
                    return Err(format!("\"{}\": NT_ANDROID_TYPE_PAD_SEGMENT note has unexpected n_descsz: {}", rp, descsz));
                }
                return Ok(u32at(desc_at) == 1);
            }
            q = next;
        }
    }
    Ok(false)
}

/// Paginas enormes transparentes disponibles (bionic `get_transparent_hugepages_supported`: el archivo existe y no
/// dice "[never]").
fn thp_supported() -> bool {
    static THP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *THP.get_or_init(|| std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").map_or(false, |s| !s.contains("[never]")))
}

/// Tamano de una pagina enorme (PMD) en arm64 con paginas de 4 KiB.
const PMD_SIZE: u64 = 2 << 20;

/// Paso 2 de la carga: reserva y mapea una biblioteca leida en el paso 1 (bionic `ElfReader::Load`:
/// `ReserveAddressSpace` y `LoadSegments`).
fn map_module(pre: &Pre, flags: u64, asp: &mut AddrSpace) -> Result<Module, String> {
    let (eh, phdrs, path, rp) = (pre.eh, &pre.phdrs, pre.path.as_str(), pre.realpath.as_str());
    // phdr_table_get_load_size
    let (mut lo, mut hi) = (u64::MAX, 0u64);
    for p in phdrs.iter().filter(|p| p.typ == PT_LOAD) {
        lo = lo.min(p.vaddr);
        hi = hi.max(p.vaddr + p.memsz);
    }
    if lo == u64::MAX {
        lo = 0;
    }
    let (lo, hi) = (page_start(lo), page_end(hi));
    let mut load_size = hi - lo;
    if load_size == 0 {
        return Err(format!("\"{}\" has no loadable segments", rp));
    }
    if pre.compat {
        // sitio para alinear a pagina la frontera de permisos (Setup16KiBAppCompat)
        load_size += page();
    }
    // desde aqui la reserva tiene dueno: un error la desmapea (o, si es la del llamador, la deja sin acceso)
    let rsv = if load_size > asp.size {
        if asp.must_use {
            return Err(format!("reserved address space {} smaller than {} bytes needed for \"{}\"", load_size - asp.size, load_size, rp));
        }
        // inicio alineado a 2 MiB si hay paginas enormes transparentes, targetSdkVersion >= 31 y el mayor p_align
        // de los PT_LOAD es justo 2 MiB; si no, a pagina
        let start_align = if thp_supported() && crate::bridge::target_sdk() >= 31 && pre.max_align == PMD_SIZE { PMD_SIZE } else { page() };
        reserve(load_size as usize, start_align as usize).map_err(|_| format!("couldn't reserve {} bytes of address space for \"{}\"", load_size, rp))?
    } else {
        let r = Rsv { base: asp.start, len: load_size as usize, gap: (0, 0), by_caller: true };
        asp.start += load_size;
        asp.size -= load_size;
        r
    };
    let start = rsv.base;
    let mut bias = start.wrapping_sub(lo);
    // LoadSegments: con paginas de 16 KiB, p_align menor solo en el modo de compatibilidad
    if page() >= 16384 && pre.min_align < page() && !pre.compat {
        return Err(format!("\"{}\" program alignment ({}) cannot be smaller than system page size ({})", rp, pre.min_align, page()));
    }
    let mut compat_relro = (0, 0);
    if pre.compat {
        // la reserva entera RW: los segmentos se leen dentro (no se pueden mapear sin alinear)
        unsafe { mprotect(start as *mut c_void, load_size as usize, PROT_READ | PROT_WRITE) };
        let boundary = compat_boundary(pre).ok_or_else(|| format!("\"{}\" failed to setup 16KiB App Compat", rp))?;
        let off = boundary % page();
        if off != 0 {
            bias += page() - off;
        }
        let rw_start = bias + boundary;
        let rw_size = load_size - (rw_start - start);
        compat_relro = (start, load_size - rw_size);
        name_anon(start, load_size, &format!("{} (compat loaded)", rp));
    }
    load_segments(pre, bias)?;
    // dinamica
    let dynp = phdrs.iter().find(|p| p.typ == PT_DYNAMIC).ok_or_else(|| format!("missing PT_DYNAMIC in \"{}\"", rp))?;
    let dyn_addr = dynp.vaddr + bias;
    let mut m = Module {
        name: pre.name.clone(),
        path: path.to_string(),
        realpath: pre.realpath.clone(),
        soname: pre.soname.clone(),
        file_id: pre.file_id,
        handle: new_handle(),
        base: bias,
        lo: lo + bias,
        hi: hi + bias,
        phdr_addr: 0,
        phnum: eh.phnum,
        phdrs: phdrs.clone(),
        entry: eh.entry + bias,
        symtab: 0,
        strtab: 0,
        gnu_hash: 0,
        sysv_hash: 0,
        versym: 0,
        verdef: (0, 0),
        verneed: (0, 0),
        needed: pre.needed.clone(),
        runpath: pre.runpath.clone(),
        symbolic: false,
        df1_global: false,
        ns: pre.ns,
        secondary: Mutex::new(Vec::new()),
        children: Mutex::new(Vec::new()),
        parents: Mutex::new(Vec::new()),
        group_root: Mutex::new(Weak::new()),
        tls: None,
        tls_id: AtomicUsize::new(0),
        tlsdesc_args: Mutex::new(Vec::new()),
        init_array: (0, 0),
        fini_array: (0, 0),
        init: 0,
        fini: 0,
        refs: AtomicUsize::new(0),
        dyn_addr,
        live: AtomicBool::new(false),
        ctors_called: AtomicBool::new(false),
        rtld_flags: AtomicU64::new(flags & (RTLD_GLOBAL | RTLD_NODELETE)),
        target_sdk: std::sync::atomic::AtomicI32::new(0),
        compat_relro,
        pad: pre.pad,
        _rsv: rsv,
    };
    // direccion de los phdrs en memoria: dentro del primer PT_LOAD que cubre phoff
    for p in phdrs.iter().filter(|p| p.typ == PT_LOAD) {
        if eh.phoff >= p.offset && eh.phoff < p.offset + p.filesz {
            m.phdr_addr = p.vaddr + (eh.phoff - p.offset) + bias;
        }
    }
    unsafe {
        let mut d = dyn_addr as *const i64;
        loop {
            let (tag, val) = (*d, *d.add(1) as u64);
            if tag == DT_NULL {
                break;
            }
            match tag {
                DT_STRTAB => m.strtab = val + bias,
                DT_SYMTAB => m.symtab = val + bias,
                DT_GNU_HASH => m.gnu_hash = val + bias,
                DT_HASH => m.sysv_hash = val + bias,
                DT_VERSYM => m.versym = val + bias,
                DT_VERDEF => m.verdef.0 = val + bias,
                DT_VERDEFNUM => m.verdef.1 = val as usize,
                DT_VERNEED => m.verneed.0 = val + bias,
                DT_VERNEEDNUM => m.verneed.1 = val as usize,
                DT_INIT => m.init = val + bias,
                DT_FINI => m.fini = val + bias,
                DT_INIT_ARRAY => m.init_array.0 = val + bias,
                DT_INIT_ARRAYSZ => m.init_array.1 = val as usize / 8,
                DT_FINI_ARRAY => m.fini_array.0 = val + bias,
                DT_FINI_ARRAYSZ => m.fini_array.1 = val as usize / 8,
                DT_SYMBOLIC => m.symbolic = true,
                // bionic prelink_image (LP64): sin relocaciones de texto
                DT_TEXTREL => return Err(format!("\"{}\" has text relocations", rp)),
                DT_FLAGS => {
                    if val & DF_TEXTREL != 0 {
                        return Err(format!("\"{}\" has text relocations", rp));
                    }
                    m.symbolic |= val & DF_SYMBOLIC != 0
                }
                DT_FLAGS_1 => {
                    // bionic prelink_image: DF_1_GLOBAL -> RTLD_GLOBAL (grupo global), DF_1_NODELETE -> RTLD_NODELETE
                    m.df1_global = val & DF_1_GLOBAL != 0;
                    let mut f = 0;
                    if val & DF_1_GLOBAL != 0 {
                        f |= RTLD_GLOBAL;
                    }
                    if val & DF_1_NODELETE != 0 {
                        f |= RTLD_NODELETE;
                    }
                    m.rtld_flags.fetch_or(f, SeqCst);
                }
                _ => {}
            }
            d = d.add(2);
        }
    }
    // bionic prelink_image: `validate_verdef_section`
    m.for_each_verdef(|_, _, _| false)?;
    if let Some(t) = phdrs.iter().find(|p| p.typ == PT_TLS) {
        m.tls = Some(TlsMod { init_addr: t.vaddr + bias, filesz: t.filesz as usize, memsz: t.memsz as usize, align: t.align.max(1) as usize });
    }
    Ok(m)
}

/// `IsEligibleFor16KiBAppCompat`: a lo sumo un PT_GNU_RELRO, segmentos RW contiguos y el RELRO al principio del
/// primero; devuelve la frontera de permisos (direccion virtual: fin del RELRO o inicio del primer RW, a 4 KiB).
fn compat_boundary(pre: &Pre) -> Option<u64> {
    let rp = &pre.realpath;
    let relros: Vec<&Phdr> = pre.phdrs.iter().filter(|p| p.typ == PT_GNU_RELRO).collect();
    if relros.len() > 1 {
        crate::bridge::alog(&format!("\"{}\": Compat loading failed: Multiple RELRO segments found", rp));
        return None;
    }
    let (mut first_rw, mut last_rw): (Option<usize>, Option<usize>) = (None, None);
    for (i, p) in pre.phdrs.iter().enumerate() {
        if p.typ != PT_LOAD {
            continue;
        }
        if p.flags & 6 == 6 {
            first_rw.get_or_insert(i);
            if last_rw.is_some_and(|l| i == 0 || l != i - 1) {
                crate::bridge::alog(&format!("\"{}\": Compat loading failed: ELF contains multiple non-adjacent RW segments", rp));
                return None;
            }
            last_rw = Some(i);
        }
    }
    let first = &pre.phdrs[first_rw?];
    let Some(relro) = relros.first() else { return Some(first.vaddr & !4095) };
    if first.vaddr != relro.vaddr {
        crate::bridge::alog(&format!("\"{}\": Compat loading failed: RELRO is not in the first RW segment", rp));
        return None;
    }
    let end = relro.vaddr.checked_add(relro.memsz)?;
    Some((end + 4095) & !4095)
}

/// `_extend_load_segment_vma` de bionic: con la nota NT_ANDROID_TYPE_PAD_SEGMENT (`should_pad`), un PT_LOAD con
/// `p_align` mayor que la pagina (hasta 64 KiB) y sin bss se extiende hasta el inicio del siguiente PT_LOAD, para que
/// no quede hueco entre los dos mapeos. Devuelve (p_memsz, p_filesz) extendidos. Nunca en el modo de compatibilidad.
fn extend_load_segment_vma(phdrs: &[Phdr], i: usize, should_pad: bool, compat: bool) -> (u64, u64) {
    let p = &phdrs[i];
    let (memsz, filesz) = (p.memsz, p.filesz);
    if compat || p.align <= page() || p.align > 64 * 1024 || !should_pad {
        return (memsz, filesz);
    }
    let Some(next) = phdrs.get(i + 1).filter(|n| n.typ == PT_LOAD) else { return (memsz, filesz) };
    if memsz != filesz {
        return (memsz, filesz);
    }
    let (next_start, curr_end) = (page_start(next.vaddr), page_end(p.vaddr + memsz));
    if curr_end >= next_start {
        return (memsz, filesz);
    }
    let extend = next_start - curr_end;
    (memsz + extend, filesz + extend)
}

/// `PFLAGS_TO_PROT` para el host: el codigo guest solo se lee (lo traduce el JIT), asi que PF_X da lectura y nunca
/// PROT_EXEC.
fn pflags_prot(flags: u32) -> i32 {
    let mut prot = 0;
    if flags & 4 != 0 || flags & 1 != 0 {
        prot |= PROT_READ;
    }
    if flags & 2 != 0 {
        prot |= PROT_WRITE;
    }
    prot
}

/// `prctl(PR_SET_VMA, PR_SET_VMA_ANON_NAME, ...)`: nombre de un mapeo anonimo en /proc/self/maps (`[anon:nombre]`),
/// como los que pone bionic (".bss", "<ruta> (compat loaded)"). Sin soporte del kernel no hace nada.
fn name_anon(start: u64, len: u64, name: &str) {
    if let Ok(c) = std::ffi::CString::new(name) {
        crate::syscall::host_syscall(157, [0x5356_4d41, 0, start, len, c.as_ptr() as u64, 0]);
    }
}

/// Mapeo anonimo fijo de ceros con proteccion `prot` (bss, cola de la ultima pagina del guest, reserva del modo de
/// compatibilidad) y su nombre de bionic, si lo lleva.
fn map_zero(at: u64, len: u64, prot: i32, name: Option<&str>) -> Result<(), i32> {
    if len == 0 {
        return Ok(());
    }
    let r = unsafe { mmap(at as *mut c_void, len as usize, prot, MAP_PRIVATE | MAP_FIXED | MAP_ANONYMOUS, -1, 0) };
    if r == MAP_FAILED {
        return Err(unsafe { *__errno_location() });
    }
    if let Some(n) = name {
        name_anon(at, len, n);
    }
    Ok(())
}

/// El kernel admite la migracion de tamano de pagina (bionic `page_size_migration_supported`:
/// /sys/kernel/mm/pgsize_migration/enabled con "1"; el del host, que es el que lee el guest). `HEDDLE_PGSIZE_MIGRATION`
/// (0/1) lo fija.
fn page_size_migration_supported() -> bool {
    static S: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *S.get_or_init(|| match std::env::var("HEDDLE_PGSIZE_MIGRATION") {
        Ok(v) => v.trim() == "1",
        Err(_) => std::fs::read_to_string("/sys/kernel/mm/pgsize_migration/enabled").map_or(false, |s| s.contains('1')),
    })
}

/// `LoadSegments` de bionic: mapea cada PT_LOAD desde el archivo en `bias` con su proteccion final
/// (`PFLAGS_TO_PROT`; en LP64 no hay relocaciones de texto: solo se escribe en los escribibles, y el RELRO se protege
/// despues de relocar), pone a cero la cola de la ultima pagina de archivo de los escribibles, descarta las paginas
/// de relleno de un segmento extendido y mapea el bss anonimo (".bss"). En el modo de compatibilidad de 16 KiB lee
/// los segmentos (alineados a 4 KiB) dentro de la reserva anonima RW (`CompatMapSegment`).
fn load_segments(pre: &Pre, bias: u64) -> Result<(), String> {
    let rp = pre.realpath.as_str();
    let pg = page();
    // en el modo de compatibilidad, los segmentos (alineados a 4 KiB) se leen a esa granularidad
    let seg_align = if pre.compat { 4096 } else { pg };
    let hp = crate::mem::PAGE as u64;
    for (i, p) in pre.phdrs.iter().enumerate().filter(|(_, p)| p.typ == PT_LOAD) {
        let (p_memsz, p_filesz) = extend_load_segment_vma(&pre.phdrs, i, pre.pad, pre.compat);
        let seg_start = p.vaddr + bias;
        let seg_page_end = (seg_start + p_memsz + seg_align - 1) & !(seg_align - 1);
        let seg_file_end = seg_start + p_filesz;
        let file_page_start = p.offset & !(seg_align - 1);
        let file_length = p.offset + p_filesz - file_page_start;
        let prot = pflags_prot(p.flags);
        if pre.file_size <= 0 {
            return Err(format!("\"{}\" invalid file size: {}", rp, pre.file_size));
        }
        if p.offset + p.filesz > pre.file_size as u64 {
            return Err(format!(
                "invalid ELF file \"{}\" load segment[{}]: p_offset ({:#x}) + p_filesz ({:#x}) ( = {:#x}) past end of file (0x{:x})",
                rp,
                i,
                p.offset,
                p.filesz,
                p.offset + p.filesz,
                pre.file_size
            ));
        }
        // fin de lo mapeado desde el archivo
        let mut mapped_end = page_end(seg_file_end);
        if file_length != 0 {
            if p.flags & 3 == 3 {
                error_after(26, format!("\"{}\" has load segments that are both writable and executable", rp))?;
            }
            if pre.compat {
                // CompatMapSegment: lectura dentro de la reserva anonima RW
                let at = seg_start & !(seg_align - 1);
                let dst = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, file_length as usize) };
                if let Err(e) = pre.fd.pread(dst, pre.file_offset + file_page_start as i64) {
                    return Err(format!("Compat loading: \"{}\" failed to read LOAD segment {}: {}", rp, i, strerror(e)));
                }
                continue;
            }
            // MapSegment: dentro de un APK se suma el desplazamiento de la biblioteca. El kernel del guest cubre con el
            // archivo las paginas enteras (con paginas de 16 KiB, la ultima llega mas alla del segmento); mas alla del
            // final del archivo el host daria SIGBUS desde la pagina siguiente de 4 KiB: se mapea hasta ahi y el resto
            // de la pagina del guest son ceros (como en su kernel)
            let at = page_start(seg_start);
            let avail = (pre.file_size - pre.file_offset) as u64 - file_page_start;
            let map_len = page_end(file_length).min((avail + hp - 1) & !(hp - 1));
            let r = unsafe { mmap(at as *mut c_void, map_len as usize, prot, MAP_PRIVATE | MAP_FIXED, pre.fd.fd, pre.file_offset + file_page_start as i64) };
            if r == MAP_FAILED {
                return Err(format!("couldn't map \"{}\" segment {}: {}", rp, i, strerror(unsafe { *__errno_location() })));
            }
            // cola de ceros hasta el final de la pagina del guest (solo con paginas del guest mayores que las del host)
            if at + map_len < mapped_end {
                map_zero(at + map_len, mapped_end - (at + map_len), prot, None)
                    .map_err(|e| format!("couldn't map \"{}\" segment {}: {}", rp, i, strerror(e)))?;
            }
            mapped_end = mapped_end.max(at + map_len);
            // segmento ejecutable con p_align de 2 MiB, apto para paginas enormes
            if p.flags & 1 != 0 && p.align == PMD_SIZE && thp_supported() {
                const MADV_HUGEPAGE: i32 = 14;
                unsafe { madvise(at as *mut c_void, file_length as usize, MADV_HUGEPAGE) };
            }
        }
        if pre.compat {
            continue; // la reserva anonima RW ya es el bss
        }
        // ZeroFillSegment: solo los escribibles, hasta el final de la pagina (sin la extension)
        let unextended_file_end = seg_start + p.filesz;
        if p.flags & 2 != 0 && unextended_file_end % pg != 0 {
            unsafe { std::ptr::write_bytes(unextended_file_end as *mut u8, 0, (pg - unextended_file_end % pg) as usize) };
        }
        // DropPaddingPages: las paginas de la extension (relleno del archivo) se descartan
        let (pad_start, pad_end) = (page_end(unextended_file_end), page_end(seg_file_end));
        if pad_end > pad_start && page_size_migration_supported() {
            const MADV_DONTNEED: i32 = 4;
            unsafe { madvise(pad_start as *mut c_void, (pad_end - pad_start) as usize, MADV_DONTNEED) };
        }
        // MapBssSection: desde la pagina siguiente al contenido del archivo, con la proteccion del segmento
        if seg_page_end > mapped_end {
            map_zero(mapped_end, seg_page_end - mapped_end, prot, Some(".bss"))
                .map_err(|e| format!("couldn't map .bss section for \"{}\": {}", rp, strerror(e)))?;
        }
    }
    Ok(())
}

/// Paginas que cubre cada PT_GNU_RELRO (bionic las redondea hacia fuera: "over-protective"), extendidas como
/// `_extend_gnu_relro_prot_end` si el RELRO cubre todo su PT_LOAD y este se extendio (nota PAD_SEGMENT).
fn relro_ranges(m: &Module) -> impl Iterator<Item = (u64, u64)> + '_ {
    m.phdrs.iter().filter(|p| p.typ == PT_GNU_RELRO).map(move |r| {
        let mut end = page_end(r.vaddr + r.memsz);
        if let Some(i) = m.phdrs.iter().position(|p| p.typ == PT_LOAD && p.vaddr == r.vaddr) {
            if r.memsz >= m.phdrs[i].memsz {
                let (memsz, _) = extend_load_segment_vma(&m.phdrs, i, m.pad, m.compat_relro.1 != 0);
                end = page_end(m.phdrs[i].vaddr + memsz);
            }
        }
        (page_start(r.vaddr) + m.base, end + m.base)
    })
}

/// `protect_relro`: `phdr_table_protect_gnu_relro` o, en el modo de compatibilidad,
/// `phdr_table_protect_gnu_relro_16kib_compat` (region RELRO de compatibilidad: RX en bionic; aqui lectura, y toda
/// ella es codigo ejecutable del guest, ver `exec_ranges`).
fn protect_relro(m: &Module) {
    if m.compat_relro.1 != 0 {
        unsafe { mprotect(m.compat_relro.0 as *mut c_void, m.compat_relro.1 as usize, PROT_READ) };
        return;
    }
    for (s, e) in relro_ranges(m) {
        if e > s {
            unsafe { mprotect(s as *mut c_void, (e - s) as usize, PROT_READ) };
        }
    }
}

/// RELRO compartido de `link_image` (ANDROID_DLEXT_WRITE_RELRO / ANDROID_DLEXT_USE_RELRO, como lo usa WebView):
/// escribir las paginas RELRO ya relocadas en `relro_fd` y mapearlas desde ahi, o sustituir por las del archivo las
/// que coinciden. `off`: desplazamiento en el archivo, compartido por todas las bibliotecas de la carga.
fn share_relro(m: &Module, e: &ExtInfo, off: &mut u64) -> Result<(), String> {
    let errno = || unsafe { *__errno_location() };
    if e.flags & DLEXT_WRITE_RELRO != 0 {
        // phdr_table_serialize_gnu_relro
        for (s, end) in relro_ranges(m) {
            let size = (end - s) as usize;
            let w = loop {
                let w = unsafe { write(e.relro_fd, s as *const c_void, size) };
                if w < 0 && errno() == 4 {
                    continue;
                }
                break w;
            };
            let ok = w == size as isize
                && unsafe { mmap(s as *mut c_void, size, PROT_READ, MAP_PRIVATE | MAP_FIXED, e.relro_fd, *off as i64) } != MAP_FAILED;
            if !ok {
                return Err(format!("failed serializing GNU RELRO section for \"{}\": {}", m.realpath, strerror(errno())));
            }
            *off += size as u64;
        }
    } else if e.flags & DLEXT_USE_RELRO != 0 {
        // phdr_table_map_gnu_relro: solo las paginas iguales (otras bibliotecas pueden estar en otra direccion)
        let fd = Fd { fd: e.relro_fd, owned: false };
        let fail = |er: i32| format!("failed mapping GNU RELRO section for \"{}\": {}", m.realpath, strerror(er));
        let (_, _, file_size) = fd.stat().map_err(fail)?;
        let pg = page() as usize;
        for (s, end) in relro_ranges(m) {
            let size = (end - s) as usize;
            if (file_size as u64).saturating_sub(*off) < size as u64 {
                break; // archivo demasiado corto: probablemente de otra version de la biblioteca
            }
            let mut buf = vec![0u8; size];
            fd.pread(&mut buf, *off as i64).map_err(fail)?;
            let mem = unsafe { std::slice::from_raw_parts(s as *const u8, size) };
            let same = |o: usize| mem[o..o + pg] == buf[o..o + pg];
            let mut at = 0usize;
            while at < size {
                while at < size && !same(at) {
                    at += pg;
                }
                let mut to = at;
                while to < size && same(to) {
                    to += pg;
                }
                if to > at && unsafe { mmap((s as usize + at) as *mut c_void, to - at, PROT_READ, MAP_PRIVATE | MAP_FIXED, e.relro_fd, (*off as usize + at) as i64) } == MAP_FAILED {
                    return Err(fail(errno()));
                }
                at = to;
            }
            *off += size as u64;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Relocaciones
// ---------------------------------------------------------------------------------------------

fn dyn_val(m: &Module, tag: i64) -> Option<u64> {
    unsafe {
        let mut d = m.dyn_addr as *const i64;
        loop {
            let (t, v) = (*d, *d.add(1) as u64);
            if t == DT_NULL {
                return None;
            }
            if t == tag {
                return Some(v);
            }
            d = d.add(2);
        }
    }
}

fn sleb(p: &mut *const u8) -> i64 {
    let mut result: i64 = 0;
    let mut shift = 0;
    loop {
        let b = unsafe { **p };
        *p = unsafe { p.add(1) };
        result |= ((b & 0x7f) as i64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            if shift < 64 && (b & 0x40) != 0 {
                result |= -1i64 << shift;
            }
            return result;
        }
    }
}

/// Simbolo importado con la lista de busqueda de bionic (`soinfo_do_lookup`): el primero que lo define (con la
/// version pedida) gana. `Some(Hit::Unavailable)`: la biblioteca del sistema que lo exporta va primero pero el puente
/// no lo puede ofrecer.
fn resolve_symbol(list: &[Dep], name: &str, vi: Option<Ver>) -> Option<Hit> {
    list.iter().find_map(|d| find_in(d, name, vi))
}

/// Simbolo TLS importado: (modulo que lo define, desplazamiento en su segmento PT_TLS), con la misma lista (las
/// bibliotecas del sistema no exportan TLS al guest).
fn resolve_tls_symbol(list: &[Dep], name: &str, vi: Option<Ver>) -> Option<(Arc<Module>, u64)> {
    list.iter().find_map(|d| match d {
        Dep::M(m) => m.lookup_tls_local(name, vi).map(|v| (m.clone(), v)),
        _ => None,
    })
}

/// bionic `VersionTracker`: para cada indice de version de DT_VERSYM, la version (nombre y hash) que pide el modulo
/// (DT_VERNEED, cuyo archivo debe estar entre sus DT_NEEDED) o que define (DT_VERDEF).
fn version_tracker(m: &Module) -> Result<Vec<Option<(String, u32)>>, String> {
    let mut v: Vec<Option<(String, u32)>> = Vec::new();
    fn add(v: &mut Vec<Option<(String, u32)>>, idx: usize, name: &str, hash: u32) {
        if idx >= v.len() {
            v.resize(idx + 1, None);
        }
        v[idx] = Some((name.to_string(), hash));
    }
    let (p, n) = m.verneed;
    if p != 0 {
        let children = m.children.lock().unwrap().clone();
        let mut off = 0u64;
        for i in 0..n {
            unsafe {
                let d = (p + off) as *const u8;
                let (ver, cnt, file, aux, next) = (*(d as *const u16), *(d.add(2) as *const u16), *(d.add(4) as *const u32), *(d.add(8) as *const u32), *(d.add(12) as *const u32));
                if ver != 1 {
                    return Err(format!("unsupported verneed[{}] vn_version: {} (expected 1)", i, ver));
                }
                let target = m.cstr(file);
                let found = children.iter().any(|c| match c {
                    Dep::M(c) => c.soname == target,
                    Dep::Sys(l) => *l == target,
                    Dep::Exe => false,
                });
                if !found {
                    return Err(format!("cannot find \"{}\" from verneed[{}] in DT_NEEDED list for \"{}\"", target, i, m.realpath));
                }
                let mut ao = off + aux as u64;
                for _ in 0..cnt {
                    let a = (p + ao) as *const u8;
                    let (hash, other, name, anext) = (*(a as *const u32), *(a.add(6) as *const u16), *(a.add(8) as *const u32), *(a.add(12) as *const u32));
                    add(&mut v, other as usize, m.cstr(name), hash);
                    ao += anext as u64;
                }
                off += next as u64;
            }
        }
    }
    m.for_each_verdef(|ndx, hash, name| {
        add(&mut v, ndx as usize, name, hash);
        false
    })?;
    Ok(v)
}

fn relocate(m: &Arc<Module>, list: &[Dep]) -> Result<(), String> {
    let mut relas: Vec<Rela> = Vec::new();
    let read_rela = |addr: u64, sz: u64, out: &mut Vec<Rela>| {
        let n = sz as usize / 24;
        for i in 0..n {
            out.push(unsafe { *((addr + (i * 24) as u64) as *const Rela) });
        }
    };
    if let (Some(a), Some(s)) = (dyn_val(m, DT_RELA), dyn_val(m, DT_RELASZ)) {
        read_rela(a + m.base, s, &mut relas);
    }
    if let (Some(a), Some(s)) = (dyn_val(m, DT_JMPREL), dyn_val(m, DT_PLTRELSZ)) {
        read_rela(a + m.base, s, &mut relas);
    }
    if let (Some(a), Some(s)) = (dyn_val(m, DT_ANDROID_RELA), dyn_val(m, DT_ANDROID_RELASZ)) {
        // formato APS2: "APS2" + SLEB128 grupos
        let base = (a + m.base) as *const u8;
        unsafe {
            if std::slice::from_raw_parts(base, 4) == b"APS2" {
                let mut p = base.add(4);
                let end = base.add(s as usize);
                let count = sleb(&mut p) as usize;
                let mut offset = sleb(&mut p) as u64;
                let mut addend: i64 = 0;
                let mut done = 0usize;
                while done < count && p < end {
                    let group_size = sleb(&mut p) as usize;
                    let flags = sleb(&mut p);
                    let grouped_delta = flags & 1 != 0;
                    let grouped_info = flags & 2 != 0;
                    let grouped_addend = flags & 4 != 0;
                    let has_addend = flags & 8 != 0;
                    let mut group_delta = 0u64;
                    if grouped_delta {
                        group_delta = sleb(&mut p) as u64;
                    }
                    let mut group_info = 0u64;
                    if grouped_info {
                        group_info = sleb(&mut p) as u64;
                    }
                    if has_addend && grouped_addend {
                        addend = addend.wrapping_add(sleb(&mut p));
                    }
                    for _ in 0..group_size {
                        offset = offset.wrapping_add(if grouped_delta { group_delta } else { sleb(&mut p) as u64 });
                        let info = if grouped_info { group_info } else { sleb(&mut p) as u64 };
                        if has_addend && !grouped_addend {
                            addend = addend.wrapping_add(sleb(&mut p));
                        }
                        relas.push(Rela { offset, info, addend: if has_addend { addend } else { 0 } });
                        done += 1;
                    }
                }
            } else {
                return Err("DT_ANDROID_RELA con formato desconocido".into());
            }
        }
    }
    let _ = (DT_ANDROID_REL, DT_ANDROID_RELSZ);
    // argumentos TLSDESC con la capacidad justa: no se mueven (cada descriptor apunta al suyo)
    let n_desc = relas.iter().filter(|r| (r.info & 0xffff_ffff) as u32 == R_TLSDESC).count();
    let mut desc_args: Vec<[u64; 3]> = Vec::with_capacity(n_desc);
    let versions = version_tracker(m)?;
    // ultimo simbolo resuelto (bionic `cache_sym_val`): las relocaciones del mismo simbolo suelen ir seguidas
    let mut cache: (usize, u64) = (0, 0);
    for r in &relas {
        let typ = (r.info & 0xffff_ffff) as u32;
        let symi = (r.info >> 32) as usize;
        let loc = (r.offset + m.base) as *mut u64;
        let (sname, sym) = if symi != 0 {
            let s = m.sym(symi);
            (m.cstr(s.name).to_string(), Some(s))
        } else {
            (String::new(), None)
        };
        let weak = sym.map_or(false, |s| (s.info >> 4) == 2);
        // bionic `lookup_version_info`: la version que pide la referencia (indices 0 y 1: ninguna)
        let ver = if symi != 0 && m.versym != 0 {
            let idx = unsafe { *(m.versym as *const u16).add(symi) };
            if idx > 1 {
                match versions.get(idx as usize).and_then(|v| v.as_ref()) {
                    Some((n, h)) => Some(Ver { name: n.as_str(), hash: *h }),
                    None => return Err(format!("cannot find verneed/verdef for version index={} referenced by symbol \"{}\" at \"{}\"", idx, sname, m.realpath)),
                }
            } else {
                None
            }
        } else {
            None
        };
        let resolve = |name: &str, sym: Option<Sym>| -> Result<u64, String> {
            if let Some(s) = sym {
                if s.shndx != 0 && (s.info >> 4) == 0 {
                    // simbolo local definido en este modulo
                    return Ok(m.base + s.value);
                }
            }
            // falla perezosa: un trampolin que aborta con el nombre al ser llamado
            let lazy = |why: &str| {
                eprintln!("[heddle] simbolo sin resolver en {}: {} ({})", m.name, name, why);
                crate::bridge::alog(&format!("simbolo '{}' de {}: {}", name, m.realpath, why));
                crate::libc_hle::missing(name)
            };
            match resolve_symbol(list, name, ver) {
                Some(Hit::At(a)) => Ok(a),
                Some(Hit::Unavailable) if weak => Ok(0),
                Some(Hit::Unavailable) => Ok(lazy("la biblioteca del sistema lo exporta pero el puente no lo sirve")),
                // esta en la lista pero no con la version pedida: como bionic
                None if !weak && ver.is_some() && resolve_symbol(list, name, None).is_some() => {
                    Err(format!("cannot locate symbol \"{}\" referenced by \"{}\"...", name, m.realpath))
                }
                None if weak => Ok(0),
                // bionic (linker_relocate.cpp): no esta en la lista de busqueda (tampoco lo que exporta una biblioteca
                // del sistema que el modulo no declara en DT_NEEDED)
                None => Err(format!("cannot locate symbol \"{}\" referenced by \"{}\"...", name, m.realpath)),
            }
        };
        unsafe {
            match typ {
                0 => {}
                R_RELATIVE => *loc = (m.base as i64).wrapping_add(r.addend) as u64,
                R_ABS64 | R_GLOB_DAT | R_JUMP_SLOT => {
                    let a = if symi != 0 && symi == cache.0 {
                        cache.1
                    } else {
                        let a = resolve(&sname, sym)?;
                        cache = (symi, a);
                        a
                    };
                    *loc = (a as i64).wrapping_add(r.addend) as u64;
                }
                R_IRELATIVE => {
                    let resolver = (m.base as i64).wrapping_add(r.addend) as u64;
                    let (v, _) = rt::call_guest(resolver, &[], &[]);
                    *loc = v;
                }
                R_TLS_DTPMOD64 | R_TLS_DTPREL64 | R_TLS_TPREL64 | R_TLSDESC => {
                    // TLS estatico: el modulo que define el simbolo reserva su bloque en cada hilo
                    let (owner, soff): (Arc<Module>, u64) = match sym {
                        Some(s) if s.shndx != 0 => (m.clone(), s.value),
                        Some(_) => match resolve_tls_symbol(list, &sname, ver) {
                            Some(f) => f,
                            None if weak => {
                                // debil sin definir (bionic): &simbolo == NULL
                                match typ {
                                    // tp + sumando: &simbolo == __get_tls() (bionic deja tpoff a 0)
                                    R_TLS_TPREL64 => *loc = r.addend as u64,
                                    R_TLSDESC => {
                                        *loc = crate::libc_hle::tlsdesc_resolver_weak();
                                        *loc.add(1) = r.addend as u64;
                                    }
                                    R_TLS_DTPMOD64 => *loc = 0,
                                    _ => *loc = r.addend as u64,
                                }
                                continue;
                            }
                            None => return Err(format!("cannot locate symbol \"{}\" referenced by \"{}\"...", sname, m.realpath)),
                        },
                        None => (m.clone(), 0),
                    };
                    let _tm = owner.tls.as_ref().ok_or("simbolo TLS en modulo sin PT_TLS")?;
                    let id = owner.tls_id.load(SeqCst) as u64;
                    if typ == R_TLS_TPREL64 {
                        // Modelo initial-exec: en bionic solo vale para el TLS estatico (ejecutable y bibliotecas
                        // cargadas al arrancar). Toda biblioteca guest se abre con dlopen: la carga falla (desde
                        // Android 10, el primero con TLS ELF; linker_relocate.cpp, mismo texto).
                        let sn = if symi == 0 { "(null)" } else { sname.as_str() };
                        return Err(format!("TLS symbol \"{}\" in dlopened \"{}\" referenced from \"{}\" using IE access model", sn, owner.realpath, m.realpath));
                    }
                    // soff es relativo al inicio del segmento TLS (st_value de simbolos TLS); TLS dinamico (tls.rs)
                    match typ {
                        R_TLS_DTPMOD64 => *loc = id,
                        R_TLS_DTPREL64 => *loc = (soff as i64).wrapping_add(r.addend) as u64,
                        _ => {
                            // bionic: tlsdesc_resolver_dynamic con {generacion actual, modulo, desplazamiento}
                            if desc_args.len() == desc_args.capacity() {
                                return Err(format!("{}: descriptores TLSDESC de mas", m.name));
                            }
                            desc_args.push([crate::tls::generation(), id, (soff as i64).wrapping_add(r.addend) as u64]);
                            *loc = crate::tls::resolver();
                            *loc.add(1) = desc_args.last().unwrap().as_ptr() as u64;
                        }
                    }
                }
                other => return Err(format!("{}: relocacion AArch64 no soportada {} ({})", m.name, other, sname)),
            }
        }
    }
    *m.tlsdesc_args.lock().unwrap() = desc_args;
    // RELR
    let relr = dyn_val(m, DT_RELR).zip(dyn_val(m, DT_RELRSZ)).or_else(|| dyn_val(m, DT_ANDROID_RELR).zip(dyn_val(m, DT_ANDROID_RELRSZ)));
    if let Some((a, s)) = relr {
        let ents = (a + m.base) as *const u64;
        let n = s as usize / 8;
        let mut where_: *mut u64 = std::ptr::null_mut();
        unsafe {
            for i in 0..n {
                let e = *ents.add(i);
                if e & 1 == 0 {
                    where_ = (m.base + e) as *mut u64;
                    *where_ = (*where_).wrapping_add(m.base);
                    where_ = where_.add(1);
                } else {
                    let mut bitmap = e >> 1;
                    let mut k = 0;
                    while bitmap != 0 {
                        if bitmap & 1 != 0 {
                            let p = where_.add(k);
                            *p = (*p).wrapping_add(m.base);
                        }
                        bitmap >>= 1;
                        k += 1;
                    }
                    where_ = where_.add(63);
                }
            }
        }
    }
    Ok(())
}

/// bionic `register_soinfo_tls`: identificador de modulo TLS para su segmento PT_TLS.
fn register_tls(m: &Arc<Module>) -> Result<(), String> {
    let Some(t) = m.tls.as_ref() else { return Ok(()) };
    if m.tls_id.load(SeqCst) != 0 {
        return Ok(());
    }
    match crate::tls::register(m.handle, t.init_addr, t.filesz, t.memsz, t.align) {
        Some(id) => {
            m.tls_id.store(id, SeqCst);
            Ok(())
        }
        None => Err(format!("\"{}\": too many TLS modules ({})", m.realpath, crate::tls::MAX_TLS_MODULES)),
    }
}

/// Identificador de modulo TLS (`dlpi_tls_modid`; 0 sin PT_TLS).
pub fn tls_module_id(m: &Module) -> u64 {
    m.tls_id.load(SeqCst) as u64
}

/// bionic `soinfo::call_constructors`: una vez por modulo (la marca se pone antes, contra la recursion), primero los
/// hijos en orden y en profundidad, luego DT_INIT y DT_INIT_ARRAY.
fn call_constructors(m: &Arc<Module>) {
    if m.ctors_called.swap(true, SeqCst) {
        return;
    }
    for c in guest_children(m).iter() {
        call_constructors(c);
    }
    #[cfg(target_os = "android")]
    crate::bridge::alog(&format!("run_init {} (init={}, init_array={})", describe_addr(m.lo), m.init != 0, m.init_array.1));
    if m.init != 0 {
        rt::call_guest(m.init, &[], &[]);
    }
    for i in 0..m.init_array.1 {
        let f = unsafe { *((m.init_array.0 + (i * 8) as u64) as *const u64) };
        if f != 0 && f != u64::MAX {
            rt::call_guest(f, &[], &[]);
        }
    }
}

/// bionic `soinfo::call_destructors`: DT_FINI_ARRAY en orden inverso y luego DT_FINI, solo si corrieron los
/// constructores.
fn call_destructors(m: &Arc<Module>) {
    if !m.ctors_called.load(SeqCst) {
        return;
    }
    for i in (0..m.fini_array.1).rev() {
        let f = unsafe { *((m.fini_array.0 + (i * 8) as u64) as *const u64) };
        if f != 0 && f != u64::MAX {
            rt::call_guest(f, &[], &[]);
        }
    }
    if m.fini != 0 {
        rt::call_guest(m.fini, &[], &[]);
    }
}

/// dlclose (bionic `soinfo_unload`): suelta una referencia de la raiz del grupo de `m`; en la ultima, descarga el
/// grupo (`unload_impl`).
pub fn unload(m: &Arc<Module>) {
    with_loader(|| soinfo_unload(m));
}

/// Referencia de un `__cxa_thread_atexit_impl` con `dso` (bionic `increment_dso_handle_reference_counter`): la
/// biblioteca que contiene `dso` no se descarga mientras el hilo no haya ejecutado ese destructor.
pub fn dso_ref(dso: u64) {
    with_loader(|| {
        if let Some(m) = module_for_addr(dso) {
            group_root(&m).refs.fetch_add(1, SeqCst);
        }
    });
}

/// Suelta la referencia de `dso_ref` (como un dlclose de esa biblioteca).
pub fn dso_unref(dso: u64) {
    with_loader(|| {
        if let Some(m) = module_for_addr(dso) {
            soinfo_unload(&m);
        }
    });
}

fn soinfo_unload(si: &Arc<Module>) {
    // una carga fallida descarga su raiz sin enlazar: sin cuenta y sin grupo
    let linked = si.is_live();
    let root = if linked { group_root(si) } else { si.clone() };
    // como el size_t de bionic: un dlclose de mas desborda y ya no descarga
    let rc = if linked { root.refs.fetch_sub(1, SeqCst).wrapping_sub(1) } else { 0 };
    if rc > 0 {
        return;
    }
    unload_impl(&root);
}

/// bionic `soinfo_unload_impl`.
fn unload_impl(root: &Arc<Module>) {
    let is_linked = root.is_live();
    // bionic `can_unload`: ni RTLD_NODELETE ni RTLD_GLOBAL (tampoco DF_1_*)
    if is_linked && root.rtld_flags.load(SeqCst) & (RTLD_NODELETE | RTLD_GLOBAL) != 0 {
        return;
    }
    let mut todo: std::collections::VecDeque<Arc<Module>> = std::collections::VecDeque::new();
    todo.push_back(root.clone());
    let mut local: Vec<Arc<Module>> = Vec::new();
    let mut external: Vec<Arc<Module>> = Vec::new();
    let has = |v: &[Arc<Module>], m: &Arc<Module>| v.iter().any(|o| Arc::ptr_eq(o, m));
    while let Some(si) = todo.pop_front() {
        if has(&local, &si) {
            continue;
        }
        local.push(si.clone());
        let children = std::mem::take(&mut *si.children.lock().unwrap());
        for child in children.into_iter().filter_map(|d| if let Dep::M(c) = d { Some(c) } else { None }) {
            child.parents.lock().unwrap().retain(|&p| p != si.handle);
            if has(&local, &child) {
                continue;
            } else if child.is_live() && !Arc::ptr_eq(&group_root(&child), root) {
                external.push(child);
            } else if child.parents.lock().unwrap().is_empty() {
                todo.push_back(child);
            }
        }
    }
    // los destructores corren con todo el grupo aun cargado (pueden usar dlsym/dladdr sobre si mismos)
    for si in local.iter() {
        call_destructors(si);
    }
    for si in local.iter() {
        free_module(si);
    }
    if is_linked {
        for e in external.iter() {
            soinfo_unload(e);
        }
    }
}

/// Libera un modulo descargado (bionic `soinfo_free`): deja de existir para el cargador y para el puente. La
/// reserva de direcciones se desmapea al soltar la ultima referencia `Arc` (normalmente al volver de dlclose; un
/// hilo que la tenga un instante, como el muestreo, solo lo retrasa).
fn free_module(m: &Arc<Module>) {
    m.live.store(false, SeqCst);
    crate::monitor::lock(&MODULES).retain(|o| !Arc::ptr_eq(o, m));
    // sin ciclos de Arc entre modulos liberados
    m.children.lock().unwrap().clear();
    m.parents.lock().unwrap().clear();
    // ya no es memoria ejecutable del guest: regiones que pidio ejecutables dentro de ella y trampolines con firma
    // hacia sus funciones (un puntero del host viejo sigue llamando a la direccion, como en ARM)
    crate::mem::guest_unmap(m.lo, m.hi - m.lo);
    crate::cbthunk::retire_range(m.lo, m.hi);
    // su modulo TLS (el bloque de cada hilo se libera cuando ese hilo pasa por la ruta lenta o termina)
    let id = m.tls_id.swap(0, SeqCst);
    if id != 0 {
        crate::tls::unregister(id);
    }
    UNLOADS.fetch_add(1, SeqCst);
    // codigo traducido de todos los hilos (cada uno lo descarta antes de su siguiente bloque): una biblioteca
    // cargada despues en las mismas direcciones no puede ejecutar traducciones de esta
    crate::jit::publish_ic(0);
}

#[allow(dead_code)]
fn _unused(_c: &Cpu) {}

#[cfg(test)]
mod reloc_tests {
    use super::*;

    /// ET_DYN AArch64 minimo de una pagina: un PT_LOAD, PT_DYNAMIC con DT_RELA y una relocacion de tipo 999.
    fn elf_con_relocacion_mala() -> Vec<u8> {
        let mut d = vec![0u8; 4096];
        let put = |d: &mut Vec<u8>, off: usize, b: &[u8]| d[off..off + b.len()].copy_from_slice(b);
        put(&mut d, 0, b"\x7fELF\x02\x01\x01");
        put(&mut d, 16, &3u16.to_le_bytes()); // ET_DYN
        put(&mut d, 18, &183u16.to_le_bytes()); // EM_AARCH64
        put(&mut d, 20, &1u32.to_le_bytes());
        put(&mut d, 32, &64u64.to_le_bytes()); // phoff
        put(&mut d, 52, &64u16.to_le_bytes()); // ehsize
        put(&mut d, 54, &56u16.to_le_bytes()); // phentsize
        put(&mut d, 56, &2u16.to_le_bytes()); // phnum
        let ph = |d: &mut Vec<u8>, i: usize, typ: u32, off: u64, sz: u64| {
            let o = 64 + 56 * i;
            put(d, o, &typ.to_le_bytes());
            put(d, o + 4, &6u32.to_le_bytes()); // RW
            for (k, v) in [off, off, off, sz, sz, 4096].iter().enumerate() {
                put(d, o + 8 + 8 * k, &v.to_le_bytes());
            }
        };
        ph(&mut d, 0, PT_LOAD, 0, 4096);
        ph(&mut d, 1, PT_DYNAMIC, 0x200, 48);
        for (k, v) in [DT_RELA as u64, 0x300, DT_RELASZ as u64, 24, 0, 0].iter().enumerate() {
            put(&mut d, 0x200 + 8 * k, &v.to_le_bytes());
        }
        for (k, v) in [0x400u64, 999, 0].iter().enumerate() {
            put(&mut d, 0x300 + 8 * k, &v.to_le_bytes());
        }
        // section headers (bionic los exige): nula, .dynamic (sh_link = 2) y su .dynstr de un byte en 0x480
        put(&mut d, 40, &0x500u64.to_le_bytes()); // shoff
        put(&mut d, 58, &64u16.to_le_bytes()); // shentsize
        put(&mut d, 60, &3u16.to_le_bytes()); // shnum
        put(&mut d, 62, &2u16.to_le_bytes()); // shstrndx
        let sh = |d: &mut Vec<u8>, i: usize, typ: u32, off: u64, sz: u64, link: u32| {
            let o = 0x500 + 64 * i;
            put(d, o + 4, &typ.to_le_bytes());
            put(d, o + 24, &off.to_le_bytes());
            put(d, o + 32, &sz.to_le_bytes());
            put(d, o + 40, &link.to_le_bytes());
        };
        sh(&mut d, 1, SHT_DYNAMIC, 0x200, 48, 2);
        sh(&mut d, 2, SHT_STRTAB, 0x480, 1, 0);
        d
    }

    /// Un modulo cuya relocacion falla no queda registrado: cargarlo otra vez vuelve a fallar en lugar de
    /// devolver el modulo a medio relocar.
    #[test]
    fn relocacion_fallida_no_deja_el_modulo_registrado() {
        let path = std::env::temp_dir().join(format!("libheddle_reloc_mala_{}.so", std::process::id()));
        std::fs::write(&path, elf_con_relocacion_mala()).unwrap();
        let p = path.to_str().unwrap().to_string();
        for _ in 0..2 {
            let e = load_library(&p).err().expect("la carga debe fallar");
            assert!(e.contains("no soportada 999"), "{}", e);
            assert!(!modules().iter().any(|m| m.path == p), "modulo a medio relocar registrado");
        }
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;

    /// En el dispositivo libGLESv3.so es un enlace a libGLESv2.so: el handle de cualquiera de las dos da GLES 2.0 a
    /// 3.2 (Unity busca `glGetStringi`, `glDispatchCompute`... con el de libGLESv2.so y llama a lo que encuentra).
    #[test]
    fn gles_v2_y_v3_son_la_misma_biblioteca() {
        for name in ["glClear", "glGetStringi", "glDispatchCompute", "glBlendBarrier", "glCopyImageSubData"] {
            let ex = crate::sigs::exporters(name);
            assert!(!ex.is_empty(), "{} no esta en el NDK", name);
            for lib in ["libGLESv2.so", "libGLESv3.so"] {
                assert!(sys_exports(lib, name, None, ex), "{} en {}", name, lib);
            }
        }
        assert!(!sys_exports("libGLESv2.so", "eglGetDisplay", None, crate::sigs::exporters("eglGetDisplay")));
        assert_eq!(sys_realpath("libGLESv3.so"), "/system/lib64/libGLESv2.so");
    }

    /// Como `ReserveWithAlignmentPadding`: cada reserva cae en una direccion nueva aunque la anterior ya se haya
    /// devuelto (inicio aleatorio dentro del relleno), alineada a pagina y desmapeada al soltarla.
    #[test]
    fn reserva_en_direccion_nueva() {
        let mut seen = Vec::new();
        for _ in 0..8 {
            let r = reserve(3 << 20, page() as usize).unwrap();
            assert_eq!(r.base % page(), 0);
            seen.push(r.base);
        }
        seen.sort_unstable();
        seen.dedup();
        assert!(seen.len() > 1, "todas las reservas en la misma direccion");
        // con inicio de 2 MiB (paginas enormes): siempre alineada, y aun aleatoria
        let mut seen = Vec::new();
        for _ in 0..8 {
            let r = reserve(5 << 20, PMD_SIZE as usize).unwrap();
            assert_eq!(r.base % PMD_SIZE, 0);
            seen.push(r.base);
        }
        seen.sort_unstable();
        seen.dedup();
        assert!(seen.len() > 1, "todas las reservas de 2 MiB en la misma direccion");
    }
}

#[cfg(test)]
mod apk_tests {
    /// `normalize_path` como bionic (linker_utils_test.cpp)
    #[test]
    fn normaliza_rutas_como_bionic() {
        let n = |p: &str| super::normalize_path(p);
        assert_eq!(n("/../root///dir/.///dir2/somedir/../zipfile!/dir/dir9//..///afile").as_deref(), Some("/root/dir/dir2/zipfile!/dir/afile"));
        assert_eq!(n("/a/b/..").as_deref(), Some("/a/"));
        assert_eq!(n("/..").as_deref(), Some("/"));
        assert_eq!(n("root/dir"), None);
    }

    /// ZIP minimo con una entrada almacenada sin comprimir.
    #[test]
    fn biblioteca_dentro_de_un_apk() {
        let name = b"lib/arm64-v8a/libx.so";
        let body = b"\x7fELFcontenido";
        let mut z = Vec::new();
        let hdr = |z: &mut Vec<u8>, sig: &[u8], central: bool| {
            z.extend_from_slice(sig);
            if central {
                z.extend_from_slice(&[20, 0]);
            }
            z.extend_from_slice(&[20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // version, flags, metodo 0, hora, fecha, crc
            z.extend_from_slice(&(body.len() as u32).to_le_bytes());
            z.extend_from_slice(&(body.len() as u32).to_le_bytes());
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
        };
        hdr(&mut z, b"PK\x03\x04", false);
        z.extend_from_slice(&[3, 0]); // extra de 3 bytes
        z.extend_from_slice(name);
        z.extend_from_slice(&[0, 0, 0]);
        let data_off = z.len() as u64;
        z.extend_from_slice(body);
        let cd_off = z.len() as u32;
        hdr(&mut z, b"PK\x01\x02", true);
        z.extend_from_slice(&[0u8; 12]); // extra, comentario, disco, atributos
        z.extend_from_slice(&0u32.to_le_bytes()); // desplazamiento de la cabecera local
        z.extend_from_slice(name);
        let cd_size = z.len() as u32 - cd_off;
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0, 0, 0, 0, 1, 0, 1, 0]);
        z.extend_from_slice(&cd_size.to_le_bytes());
        z.extend_from_slice(&cd_off.to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        let p = std::env::temp_dir().join(format!("heddle-apk-{}.zip", std::process::id()));
        std::fs::write(&p, &z).unwrap();
        let ps = p.to_str().unwrap();
        assert_eq!(super::zip_entry(ps, "lib/arm64-v8a/libx.so"), Some((data_off, body.len() as u64, true)));
        assert_eq!(super::zip_entry(ps, "lib/arm64-v8a/liby.so"), None);
        // bionic (open_library_in_zipfile): una entrada que no empieza en limite de pagina no se abre
        assert!(super::open_library_in_zipfile(&format!("{}!/lib/arm64-v8a/libx.so", ps)).is_none());
        let _ = std::fs::remove_file(&p);
    }
}
