//! Vulkan: punteros a funcion que el guest entrega dentro de estructuras.
//!
//!  * `VkAllocationCallbacks` (`pAllocator` de vkCreate*/vkDestroy*/vkAllocate*/vkFree*...): el host recibe una copia
//!    con las cinco funciones convertidas en trampolines con su firma (`sigs::VK_ALLOC`, generada). El cargador y los
//!    controladores copian la estructura (la usan despues para las reservas del objeto): los trampolines son
//!    permanentes y deterministas, asi que el `pAllocator` de vkDestroy* es "compatible" con el de la creacion.
//!  * Estructuras de ENTRADA que pueden llevar callbacks (`sigs::VK_FNS[..].args`, con su tipo en `sigs::VK_TYPES`).
//!    sigtool lo decide con `structextends` de vk.xml: solo `vkCreateInstance` (VkDebugUtilsMessengerCreateInfoEXT,
//!    VkDebugReportCallbackCreateInfoEXT, VkDirectDriverLoadingListLUNARG en su cadena), `vkCreateDevice`
//!    (VkDeviceDeviceMemoryReportCreateInfoEXT) y la creacion de los propios mensajeros. Las demas funciones (dibujo,
//!    comandos...) no tienen ninguna conversion: reenvio directo, sin coste. En las que si, se recorren por tipo:
//!    los campos que son punteros a funcion, los punteros a otras estructuras de la tabla (anidadas, arreglos con su
//!    cuenta, arreglos de punteros, anidadas por valor) y, si el tipo tiene `chain`, la cadena `pNext` por `sType`
//!    (`sigs::VK_STYPES`). Si algo lleva un callback, el host recibe copias convertidas hasta el ultimo nodo que lo
//!    necesita; el resto es lo del guest. Nada se convierte por el valor (regla 7): solo los campos que el tipo
//!    declara punteros a funcion, en la estructura que dice el `sType`. Las estructuras de entrada no se retienen
//!    tras la llamada (lo exige la especificacion), asi que las copias viven en este manejador (`Keep`).
//!  * Un `sType` desconocido (sin tamano: no se puede copiar) antes de un nodo con callbacks no corta la conversion:
//!    se deja la estructura del guest y se cambia EN SU SITIO su `pNext` (toda estructura de Vulkan empieza por
//!    sType, pNext) para que apunte a la copia convertida; al volver se repone (`Keep`, como `boundary::InPlace`). Si
//!    su memoria no es escribible (una constante), esa cadena queda sin convertir y se registra.
//!
//! Coste: cero en las funciones sin `args`; en las demas, leer los campos y la cadena de cada estructura de entrada.

use crate::boundary::host_callable_cached;
use crate::cpu::Cpu;
use crate::sigs::{VkFn, VkPtr, VkType, VK_ALLOC, VK_ALLOC_SIZE, VK_CB_STYPES, VK_NONE, VK_TYPES};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// Estructuras como mucho que se recorren en una cadena (una cadena circular es un uso invalido: se corta).
const MAX_NODES: usize = 256;
/// Elementos como mucho de un arreglo de estructuras de entrada que se recorren.
const MAX_ELEMS: usize = 1 << 16;
/// Niveles como mucho de estructuras anidadas (punteros que forman un ciclo: uso invalido, se corta).
const MAX_DEPTH: u32 = 16;

static LOGS: AtomicU32 = AtomicU32::new(0);

fn log(m: &str) {
    if LOGS.fetch_add(1, Relaxed) < 50 {
        #[cfg(target_os = "android")]
        crate::bridge::alog(m);
        #[cfg(not(target_os = "android"))]
        let _ = m;
    }
}

/// Almacenamiento de las copias de una llamada (vive hasta que el host vuelve) y los `pNext` del guest cambiados en
/// su sitio, que se reponen al soltarlo (`Drop`, despues de la llamada al host).
pub struct Keep {
    alloc: [u64; 6],
    bufs: Vec<Vec<u64>>,
    /// (direccion del campo `pNext` de una estructura del guest, valor original), en el orden en que se cambiaron
    patched: Vec<(u64, u64)>,
}

impl Keep {
    pub fn new() -> Keep {
        crate::boundary::no_abort_enter();
        Keep { alloc: [0; 6], bufs: Vec::new(), patched: Vec::new() }
    }

    /// Repone los `pNext` cambiados (en orden inverso: un mismo campo cambiado dos veces vuelve a su valor original).
    pub fn restore(&mut self) {
        while let Some((at, v)) = self.patched.pop() {
            unsafe { (at as *mut u64).write_unaligned(v) };
        }
    }

    /// Memoria para una copia de `bytes` (alineada a 8, a cero).
    fn buf(&mut self, bytes: usize) -> u64 {
        let mut b = vec![0u64; bytes.div_ceil(8).max(1)];
        let p = b.as_mut_ptr() as u64;
        self.bufs.push(b);
        p
    }
}

impl Drop for Keep {
    fn drop(&mut self) {
        self.restore();
        crate::boundary::no_abort_leave();
    }
}

#[inline]
unsafe fn rd32(a: u64) -> u32 {
    (a as *const u32).read_unaligned()
}

#[inline]
unsafe fn rd64(a: u64) -> u64 {
    (a as *const u64).read_unaligned()
}

/// Se puede escribir el campo de 8 bytes en `a`? Se comprueba escribiendo su propio valor con `process_vm_writev`
/// (el nucleo respeta la proteccion de la pagina: una constante del guest en solo lectura da EFAULT, sin senal). Sin
/// esa llamada (ENOSYS/EPERM) se responde que no: el `pNext` no se cambia.
fn writable(a: u64) -> bool {
    use crate::sys::{getpid, syscall};
    #[repr(C)]
    struct Iov {
        base: u64,
        len: usize,
    }
    let v = unsafe { rd64(a) };
    let local = Iov { base: &v as *const u64 as u64, len: 8 };
    let remote = Iov { base: a, len: 8 };
    const SYS_PROCESS_VM_WRITEV: std::os::raw::c_long = 311; // x86-64
    let r = unsafe { syscall(SYS_PROCESS_VM_WRITEV, getpid() as std::os::raw::c_long, &local as *const Iov, 1usize, &remote as *const Iov, 1usize, 0usize) };
    r == 8
}

/// Convierte los argumentos de la llamada guest en curso a `vf`: `pAllocator` y estructuras que llevan callbacks.
pub fn convert_args(c: &mut Cpu, vf: &VkFn, keep: &mut Keep) {
    if vf.alloc != 255 {
        let r = vf.alloc as usize;
        let p = c.x[r];
        if p != 0 {
            let words = (VK_ALLOC_SIZE as usize + 7) / 8;
            unsafe { std::ptr::copy_nonoverlapping(p as *const u64, keep.alloc.as_mut_ptr(), words.min(6)) };
            let base = keep.alloc.as_mut_ptr() as u64;
            convert_fields(base, VK_ALLOC);
            c.x[r] = base;
        }
    }
    for a in vf.args {
        let p = c.x[a.reg as usize];
        if p == 0 {
            continue;
        }
        let n = if a.count == 255 { 1 } else { (c.x[a.count as usize] as u32 as usize).min(MAX_ELEMS) };
        if let Some(np) = conv_array(p, n, &VK_TYPES[a.ty as usize], 1, keep, 0) {
            c.x[a.reg as usize] = np;
        }
    }
}

/// Cada puntero a funcion de `fields` en la estructura `base` (una copia) por el que puede llamar el host.
fn convert_fields(base: u64, fields: &'static [crate::sigs::CbField]) {
    for f in fields {
        let at = (base + f.off as u64) as *mut u64;
        unsafe {
            let v = at.read_unaligned();
            if v != 0 {
                at.write_unaligned(host_callable_cached(v, f.sig));
            }
        }
    }
}

/// `st` es el sType de una estructura que lleva callbacks como nodo? (casi siempre se descarta con una comparacion:
/// todas son de extensiones, sType >= 1000000000)
#[inline]
fn is_cb(st: u32) -> bool {
    !VK_CB_STYPES.is_empty() && st >= VK_CB_STYPES[0] && VK_CB_STYPES.contains(&st)
}

/// Tipo del nodo `q` de una cadena si lleva callbacks como nodo.
#[inline]
fn node_type(q: u64) -> Option<&'static VkType> {
    let st = unsafe { rd32(q) };
    if !is_cb(st) {
        return None;
    }
    let s = crate::sigs::vk_stype(st)?;
    (s.ty != VK_NONE).then(|| &VK_TYPES[s.ty as usize])
}

/// Elementos del arreglo al que apunta `p` en la estructura `e`.
fn count(e: u64, p: &VkPtr) -> usize {
    let n = match (p.count_off, p.count_size) {
        (u32::MAX, _) => 1,
        (o, 8) => (unsafe { rd64(e + o as u64) }) as usize,
        (o, _) => (unsafe { rd32(e + o as u64) }) as usize,
    };
    n.min(MAX_ELEMS)
}

/// Direccion del elemento `i` del arreglo `q` de estructuras `ty` (0 si es un puntero nulo de un arreglo de punteros).
#[inline]
fn elem(q: u64, i: usize, ty: &VkType, deref: u8) -> u64 {
    if deref == 2 {
        unsafe { rd64(q + i as u64 * 8) }
    } else {
        q + (i * ty.size as usize) as u64
    }
}

/// Hay algo que convertir en la estructura `e` de tipo `ty`? (`as_node`: es un nodo de una cadena ajena; su `pNext`
/// es el resto de esa cadena, no una cadena propia)
fn needs(e: u64, ty: &VkType, as_node: bool, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    if ty.fields.iter().any(|f| unsafe { rd64(e + f.off as u64) } != 0) {
        return true;
    }
    for p in ty.ptrs {
        let sub = &VK_TYPES[p.ty as usize];
        if p.deref == 0 {
            if needs(e + p.off as u64, sub, false, depth + 1) {
                return true;
            }
            continue;
        }
        let q = unsafe { rd64(e + p.off as u64) };
        if q != 0 && (0..count(e, p)).any(|i| matches!(elem(q, i, sub, p.deref), x if x != 0 && needs(x, sub, false, depth + 1))) {
            return true;
        }
    }
    !as_node && ty.chain && last_needed(unsafe { rd64(e + 8) }, depth).is_some()
}

/// Indice del ultimo nodo de la cadena `q` que hay que convertir.
fn last_needed(mut q: u64, depth: u32) -> Option<usize> {
    let mut last = None;
    let mut k = 0;
    while q != 0 && k < MAX_NODES {
        if let Some(t) = node_type(q) {
            if needs(q, t, true, depth + 1) {
                last = Some(k);
            }
        }
        q = unsafe { rd64(q + 8) };
        k += 1;
    }
    last
}

/// `n` estructuras `ty` en `p` (`deref` 2: arreglo de punteros): si alguna lleva algo que convertir, direccion de la
/// copia convertida.
fn conv_array(p: u64, n: usize, ty: &'static VkType, deref: u8, keep: &mut Keep, depth: u32) -> Option<u64> {
    if !(0..n).any(|i| matches!(elem(p, i, ty, deref), x if x != 0 && needs(x, ty, false, depth))) {
        return None;
    }
    if deref == 2 {
        let base = keep.buf(n * 8);
        for i in 0..n {
            let x = elem(p, i, ty, 2);
            let v = if x == 0 { 0 } else { conv_array(x, 1, ty, 1, keep, depth + 1).unwrap_or(x) };
            unsafe { ((base + i as u64 * 8) as *mut u64).write_unaligned(v) };
        }
        return Some(base);
    }
    let bytes = n * ty.size as usize;
    let base = keep.buf(bytes);
    unsafe { std::ptr::copy_nonoverlapping(p as *const u8, base as *mut u8, bytes) };
    for i in 0..n {
        conv_elem(base + (i * ty.size as usize) as u64, ty, false, keep, depth);
    }
    Some(base)
}

/// Convierte la estructura `e` (ya es una copia) de tipo `ty`: sus callbacks, lo que apunta y su cadena.
fn conv_elem(e: u64, ty: &'static VkType, as_node: bool, keep: &mut Keep, depth: u32) {
    if depth > MAX_DEPTH {
        return;
    }
    if !ty.note.is_empty() {
        if ty.fields.iter().any(|f| unsafe { rd64(e + f.off as u64) } != 0) {
            log(&format!("Vulkan: {} no soportado ({}): se pasa tal cual", ty.name, ty.note));
        }
    } else {
        convert_fields(e, ty.fields);
    }
    for p in ty.ptrs {
        let sub = &VK_TYPES[p.ty as usize];
        if p.deref == 0 {
            conv_elem(e + p.off as u64, sub, false, keep, depth + 1);
            continue;
        }
        let at = e + p.off as u64;
        let q = unsafe { rd64(at) };
        if q != 0 {
            if let Some(np) = conv_array(q, count(e, p), sub, p.deref, keep, depth + 1) {
                unsafe { (at as *mut u64).write_unaligned(np) };
            }
        }
    }
    if !as_node && ty.chain {
        conv_chain(e, keep, depth);
    }
}

/// Cadena `pNext` de la estructura `e` (una copia): copia y convierte sus nodos hasta el ultimo que lo necesita.
/// Los de `sType` desconocido no se copian (no se sabe su tamano): se cambia en su sitio el `pNext` del que precede a
/// una copia, y `keep` lo repone al volver.
fn conv_chain(e: u64, keep: &mut Keep, depth: u32) {
    let Some(last) = last_needed(unsafe { rd64(e + 8) }, depth) else { return };
    // campo que enlaza con el nodo actual y si es de una copia nuestra
    let (mut link, mut ours) = (e + 8, true);
    let mut q = unsafe { rd64(e + 8) };
    for _ in 0..=last {
        let next = unsafe { rd64(q + 8) };
        let st = unsafe { rd32(q) };
        match crate::sigs::vk_stype(st) {
            Some(s) => {
                if !ours && !writable(link) {
                    log(&format!("Vulkan: cadena pNext con un sType desconocido ({}) en memoria de solo lectura antes de una estructura con callbacks: no se convierte", unsafe { rd32(link - 8) }));
                    return;
                }
                let copy = keep.buf(s.size as usize);
                unsafe { std::ptr::copy_nonoverlapping(q as *const u8, copy as *mut u8, s.size as usize) };
                if !ours {
                    keep.patched.push((link, unsafe { rd64(link) }));
                }
                unsafe { (link as *mut u64).write_unaligned(copy) };
                if s.ty != VK_NONE {
                    conv_elem(copy, &VK_TYPES[s.ty as usize], true, keep, depth + 1);
                }
                (link, ours) = (copy + 8, true);
            }
            None => {
                // el enlace anterior ya apunta a `q` (la copia conserva el pNext original)
                (link, ours) = (q + 8, false);
            }
        }
        q = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(name: &str) -> u16 {
        VK_TYPES.iter().position(|t| t.name == name).unwrap() as u16
    }

    #[test]
    fn tablas_generadas_coherentes() {
        assert!(crate::sigs::VK_STYPES.windows(2).all(|w| w[0].stype < w[1].stype));
        assert!(crate::sigs::VK_FNS.windows(2).all(|w| w[0].name < w[1].name));
        assert!(VK_TYPES.windows(2).all(|w| w[0].name < w[1].name));
        assert!(VK_CB_STYPES.windows(2).all(|w| w[0] < w[1]));
        for t in VK_TYPES {
            assert!(t.ptrs.iter().all(|p| (p.ty as usize) < VK_TYPES.len() && p.deref <= 2));
        }
        let ci = crate::sigs::vk_fn("vkCreateInstance").unwrap();
        assert_eq!((ci.alloc, ci.args.len(), ci.args[0].reg, ci.args[0].count, ci.args[0].ty), (1, 1, 0, 255, ty("VkInstanceCreateInfo")));
        assert!(VK_TYPES[ci.args[0].ty as usize].chain);
        let cd = crate::sigs::vk_fn("vkCreateDevice").unwrap();
        assert_eq!((cd.alloc, cd.args[0].reg, cd.args[0].ty), (2, 1, ty("VkDeviceCreateInfo")));
        // extension (no exportada): tambien con su pAllocator y la estructura de creacion
        let du = crate::sigs::vk_fn("vkCreateDebugUtilsMessengerEXT").unwrap();
        assert_eq!((du.alloc, du.args[0].reg), (2, 1));
        // con vk.xml, ninguna estructura de un pipeline, un render pass o un comando puede llevar callbacks: solo
        // pAllocator (o nada: reenvio directo)
        let gp = crate::sigs::vk_fn("vkCreateGraphicsPipelines").unwrap();
        assert_eq!((gp.alloc, gp.args.len()), (4, 0));
        assert!(crate::sigs::vk_fn("vkCmdBeginRenderPass").is_none() && crate::sigs::vk_fn("vkCmdDraw").is_none());
        assert!(crate::sigs::vk_fn("vkQueueSubmit").is_none());
        assert_eq!(crate::sigs::VK_FNS.iter().filter(|f| !f.args.is_empty()).count(), 4);
        let dbg = &VK_TYPES[crate::sigs::vk_stype(1000128004).unwrap().ty as usize];
        assert_eq!((dbg.name, dbg.size, dbg.fields[0].off, dbg.fields[0].sig), ("VkDebugUtilsMessengerCreateInfoEXT", 48, 32, &b"IIIJJ"[..]));
        assert!(VK_CB_STYPES.contains(&1000011000) && VK_CB_STYPES.contains(&1000284001));
        // anidada: VkDirectDriverLoadingListLUNARG.pDrivers (cuenta driverCount) -> VkDirectDriverLoadingInfoLUNARG
        let dl = &VK_TYPES[ty("VkDirectDriverLoadingListLUNARG") as usize];
        assert_eq!((dl.ptrs.len(), dl.ptrs[0].off, dl.ptrs[0].count_off, dl.ptrs[0].count_size, dl.ptrs[0].deref), (1, 24, 20, 4, 1));
        assert_eq!(dl.ptrs[0].ty, ty("VkDirectDriverLoadingInfoLUNARG"));
        assert!(!VK_TYPES[ty("VkDirectDriverLoadingInfoLUNARG") as usize].note.is_empty());
        assert_eq!(VK_ALLOC.len(), 5);
        assert_eq!(VK_ALLOC[0].sig, b"JJJJI");
        // sin conversiones: el slot es el reenvio directo
        assert!(crate::boundary::Conv::of("vkCmdBeginRenderPass").unwrap().is_plain());
        assert!(crate::boundary::Conv::of("vkCmdDraw").unwrap().is_plain());
        assert!(!crate::boundary::Conv::of("vkCreateInstance").unwrap().is_plain());
    }

    extern "C" fn g_cb() {}

    /// "Callback guest": un slot HLE que equivale a una funcion del host (se convierte sin trampolin).
    fn cb_slot(name: &str) -> u64 {
        crate::hle::init();
        let slot = crate::hle::register(name, Box::new(|_c| crate::hle::Ret::Return));
        crate::hle::set_host_twin(slot, g_cb as *const () as usize as u64);
        slot
    }

    fn g_cb_addr() -> u64 {
        g_cb as *const () as usize as u64
    }

    /// Cadena instancia -> estructura sin callbacks -> mensajero de depuracion -> otra: se copian las tres primeras, el
    /// callback se convierte y lo que sigue es la cadena del guest; el original no se toca.
    #[test]
    fn cadena_con_mensajero_se_copia_hasta_el_callback() {
        let slot = cb_slot("prueba_vk_cb");
        let tail = [1000059000u64 /* VkPhysicalDeviceFeatures2 */, 0, 0, 0];
        // VkDebugUtilsMessengerCreateInfoEXT: sType, pNext, flags, severity|type, pfnUserCallback, pUserData
        let dbg = [1000128004u64, tail.as_ptr() as u64, 0, 0x1100000011, slot, 0x55];
        // VkValidationFeaturesEXT (sType 1000247000, 48 bytes) delante
        let vf = [1000247000u64, dbg.as_ptr() as u64, 0, 0, 0, 0];
        // VkInstanceCreateInfo: 64 bytes
        let ici = [1u64, vf.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let mut c = Cpu::new();
        c.x[0] = ici.as_ptr() as u64;
        let ci = crate::sigs::vk_fn("vkCreateInstance").unwrap();
        let mut keep = Keep::new();
        convert_args(&mut c, ci, &mut keep);
        let h = c.x[0];
        assert_ne!(h, ici.as_ptr() as u64);
        unsafe {
            let n1 = rd64(h + 8);
            assert_eq!(rd32(n1), 1000247000);
            let n2 = rd64(n1 + 8);
            assert_eq!(rd32(n2), 1000128004);
            assert_eq!(rd64(n2 + 32), g_cb_addr(), "callback convertido");
            assert_eq!(rd64(n2 + 40), 0x55);
            assert_eq!(rd64(n2 + 8), tail.as_ptr() as u64, "el resto de la cadena es la del guest");
        }
        assert_eq!(dbg[4], slot, "el original no se toca");
        assert!(keep.patched.is_empty());
        // sin estructuras con callbacks: nada que copiar
        let ici2 = [1u64, tail.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let mut c2 = Cpu::new();
        c2.x[0] = ici2.as_ptr() as u64;
        convert_args(&mut c2, ci, &mut Keep::new());
        assert_eq!(c2.x[0], ici2.as_ptr() as u64);
    }

    /// Un sType desconocido antes del mensajero: no se copia; su pNext apunta a la copia durante la llamada y se
    /// repone al soltar `Keep`. Otro desconocido detras del mensajero no se toca.
    #[test]
    fn stype_desconocido_antes_del_callback_se_enlaza_en_su_sitio() {
        let slot = cb_slot("prueba_vk_cb2");
        let tail = [0x7fff_0001u64, 0, 0, 0];
        let dbg = [1000128004u64, tail.as_ptr() as u64, 0, 0x1100000011, slot, 0x66];
        // dos desconocidos seguidos y uno conocido en medio: ici -> U1 -> U2 -> VkValidationFeaturesEXT -> dbg
        let vf = [1000247000u64, dbg.as_ptr() as u64, 0, 0, 0, 0];
        let mut u2 = [0x7fff_1234u64, vf.as_ptr() as u64, 9, 9];
        let mut u1 = [0x7fff_1233u64, u2.as_ptr() as u64, 8, 8];
        let ici = [1u64, u1.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let (u1_0, u2_0) = (u1, u2);
        let mut c = Cpu::new();
        c.x[0] = ici.as_ptr() as u64;
        let ci = crate::sigs::vk_fn("vkCreateInstance").unwrap();
        {
            let mut keep = Keep::new();
            convert_args(&mut c, ci, &mut keep);
            let h = c.x[0];
            unsafe {
                assert_eq!(rd64(h + 8), u1.as_ptr() as u64, "el desconocido no se copia");
                assert_eq!(std::ptr::read_volatile(&u1[1]), u2.as_ptr() as u64, "entre desconocidos no hace falta cambiar nada");
                let n = std::ptr::read_volatile(&u2[1]);
                assert_ne!(n, vf.as_ptr() as u64, "el pNext del desconocido apunta a la copia");
                assert_eq!(rd32(n), 1000247000);
                let d = rd64(n + 8);
                assert_eq!((rd32(d), rd64(d + 32), rd64(d + 8)), (1000128004, g_cb_addr(), tail.as_ptr() as u64));
            }
            assert_eq!(keep.patched.len(), 1);
            u2[2] = 9; // el resto de la estructura es la del guest
        }
        assert_eq!((unsafe { std::ptr::read_volatile(&u1) }, unsafe { std::ptr::read_volatile(&u2) }), (u1_0, u2_0), "repuesto al volver");
        assert_eq!(dbg[4], slot);
        u1[2] = 0;
        let _ = u1;
    }

    /// Un desconocido en memoria de solo lectura antes del mensajero: no se puede enlazar; nada se escribe.
    #[test]
    fn stype_desconocido_en_solo_lectura_no_se_cambia() {
        let slot = cb_slot("prueba_vk_cb3");
        let dbg = [1000128004u64, 0, 0, 0x1100000011, slot, 0x66];
        let r = crate::mem::Region::new(4096, crate::sys::PROT_READ | crate::sys::PROT_WRITE, crate::mem::Kind::Test).unwrap();
        let u = r.base() as *mut u64;
        unsafe {
            u.write(0x7fff_4321);
            u.add(1).write(dbg.as_ptr() as u64);
            crate::sys::mprotect(u as *mut _, 4096, crate::sys::PROT_READ);
        }
        let ici = [1u64, u as u64, 0, 0, 0, 0, 0, 0];
        let mut c = Cpu::new();
        c.x[0] = ici.as_ptr() as u64;
        let mut keep = Keep::new();
        convert_args(&mut c, crate::sigs::vk_fn("vkCreateInstance").unwrap(), &mut keep);
        assert!(keep.patched.is_empty());
        assert_eq!(unsafe { u.add(1).read() }, dbg.as_ptr() as u64);
        drop(keep);
        unsafe { crate::sys::mprotect(u as *mut _, 4096, crate::sys::PROT_READ | crate::sys::PROT_WRITE) };
    }

    /// Anidada: VkDirectDriverLoadingListLUNARG en la cadena de la instancia -> pDrivers (arreglo con su cuenta) ->
    /// VkDirectDriverLoadingInfoLUNARG con un callback no soportado: se copian la lista y el arreglo (el callback va
    /// tal cual y se registra) y los originales no cambian.
    #[test]
    fn cadena_anidada_por_puntero_y_arreglo() {
        let slot = cb_slot("prueba_vk_cb4");
        let drivers = [[1000459000u64, 0, 0, slot], [1000459000u64, 0, 0, slot]];
        // VkDirectDriverLoadingListLUNARG: sType, pNext, mode (u32) | driverCount (u32), pDrivers
        let list = [1000459001u64, 0, 2u64 << 32, drivers.as_ptr() as u64];
        let ici = [1u64, list.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let mut c = Cpu::new();
        c.x[0] = ici.as_ptr() as u64;
        let mut keep = Keep::new();
        convert_args(&mut c, crate::sigs::vk_fn("vkCreateInstance").unwrap(), &mut keep);
        let h = c.x[0];
        unsafe {
            let l = rd64(h + 8);
            assert_ne!(l, list.as_ptr() as u64);
            let d = rd64(l + 24);
            assert_ne!(d, drivers.as_ptr() as u64, "el arreglo anidado se copia");
            assert_eq!((rd32(d), rd64(d + 24), rd64(d + 32 + 24)), (1000459000, slot, slot), "no soportado: tal cual");
        }
        assert_eq!(list[3], drivers.as_ptr() as u64);
    }

    /// Coste de convert_args en la funcion con estructura que puede llevar callbacks y una cadena sin ellos (cargo
    /// test --release --lib bench_vk -- --ignored --nocapture). El coste de extremo a extremo, con las funciones de
    /// dibujo, lo mide `boundary::tests::bench_vk_frontera`.
    #[test]
    #[ignore]
    fn bench_vk() {
        let feat = [1000059000u64, 0, 0, 0];
        let ici = [1u64, feat.as_ptr() as u64, 0, 0, 0, 0, 0, 0];
        let f = crate::sigs::vk_fn("vkCreateInstance").unwrap();
        let n = 20_000_000u64;
        let mut c = Cpu::new();
        let t = std::time::Instant::now();
        for _ in 0..n {
            c.x[0] = std::hint::black_box(ici.as_ptr() as u64);
            c.x[1] = 0;
            let mut k = Keep::new();
            convert_args(&mut c, f, &mut k);
            std::hint::black_box(&k);
        }
        let con = t.elapsed().as_nanos() as f64 / n as f64;
        println!("BENCH vk: vkCreateInstance con una cadena sin callbacks {:.2} ns por llamada", con);
    }
}
