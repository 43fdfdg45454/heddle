//! Traduccion de llamadas al sistema AArch64 (SVC) a x86-64 con conversion de estructuras/flags.

use crate::cpu::Cpu;
use crate::sig::{self, GuestAct};
use crate::sys::*;

const ENOSYS: i64 = 38;
const EINVAL: i64 = 22;
const EFAULT: i64 = 14;

/// numero arm64 -> numero x86-64 (None = sin equivalente)
pub fn map_nr(n: u64) -> Option<i64> {
    if n >= 424 {
        return Some(n as i64);
    }
    Some(match n {
        0 => 206, 1 => 207, 2 => 209, 3 => 210, 4 => 208,
        5 => 188, 6 => 189, 7 => 190, 8 => 191, 9 => 192, 10 => 193, 11 => 194, 12 => 195, 13 => 196, 14 => 197, 15 => 198, 16 => 199,
        17 => 79, 18 => 212, 19 => 290, 20 => 291, 21 => 233, 22 => 281, 23 => 32, 24 => 292, 25 => 72, 26 => 294, 27 => 254, 28 => 255,
        29 => 16, 30 => 251, 31 => 252, 32 => 73, 33 => 259, 34 => 258, 35 => 263, 36 => 266, 37 => 265, 38 => 316 /* renameat -> renameat2 */,
        39 => 166, 40 => 165, 41 => 155, 43 => 137, 44 => 138, 45 => 76, 46 => 77, 47 => 285, 48 => 269, 49 => 80, 50 => 81, 51 => 161,
        52 => 91, 53 => 268, 54 => 260, 55 => 93, 56 => 257, 57 => 3, 58 => 153, 59 => 293, 60 => 179, 61 => 217, 62 => 8, 63 => 0, 64 => 1,
        65 => 19, 66 => 20, 67 => 17, 68 => 18, 69 => 295, 70 => 296, 71 => 40, 72 => 270, 73 => 271, 74 => 289, 75 => 278, 76 => 275, 77 => 276,
        78 => 267, 79 => 262, 80 => 5, 81 => 162, 82 => 74, 83 => 75, 84 => 277, 85 => 283, 86 => 286, 87 => 287, 88 => 280, 89 => 163,
        90 => 125, 91 => 126, 92 => 135, 93 => 60, 94 => 231, 95 => 247, 96 => 218, 97 => 272, 98 => 202, 99 => 273, 100 => 274, 101 => 35,
        102 => 36, 103 => 38, 104 => 246, 105 => 175, 106 => 176, 107 => 222, 108 => 224, 109 => 225, 110 => 223, 111 => 226, 112 => 227,
        113 => 228, 114 => 229, 115 => 230, 116 => 103, 117 => 101, 118 => 142, 119 => 144, 120 => 145, 121 => 143, 122 => 203, 123 => 204,
        124 => 24, 125 => 146, 126 => 147, 127 => 148, 128 => 219, 129 => 62, 130 => 200, 131 => 234, 132 => 131, 133 => 130, 134 => 13,
        135 => 14, 136 => 127, 137 => 128, 138 => 129, 139 => 15, 140 => 141, 141 => 140, 142 => 169, 143 => 114, 144 => 106, 145 => 113,
        146 => 105, 147 => 117, 148 => 118, 149 => 119, 150 => 120, 151 => 122, 152 => 123, 153 => 100, 154 => 109, 155 => 121, 156 => 124,
        157 => 112, 158 => 115, 159 => 116, 160 => 63, 161 => 170, 162 => 171, 163 => 97, 164 => 160, 165 => 98, 166 => 95, 167 => 157,
        168 => 309, 169 => 96, 170 => 164, 171 => 159, 172 => 39, 173 => 110, 174 => 102, 175 => 107, 176 => 104, 177 => 108, 178 => 186,
        179 => 99, 180 => 240, 181 => 241, 182 => 242, 183 => 243, 184 => 244, 185 => 245, 186 => 68, 187 => 71, 188 => 70, 189 => 69,
        190 => 64, 191 => 66, 192 => 220, 193 => 65, 194 => 29, 195 => 31, 196 => 30, 197 => 67, 198 => 41, 199 => 53, 200 => 49, 201 => 50,
        202 => 43, 203 => 42, 204 => 51, 205 => 52, 206 => 44, 207 => 45, 208 => 54, 209 => 55, 210 => 48, 211 => 46, 212 => 47, 213 => 187,
        214 => 12, 215 => 11, 216 => 25, 217 => 248, 218 => 249, 219 => 250, 220 => 56, 221 => 59, 222 => 9, 223 => 221, 224 => 167, 225 => 168,
        226 => 10, 227 => 26, 228 => 149, 229 => 150, 230 => 151, 231 => 152, 232 => 27, 233 => 28, 234 => 216, 235 => 237, 236 => 239,
        237 => 238, 238 => 256, 239 => 279, 240 => 297, 241 => 298, 242 => 288, 243 => 299,
        260 => 61, 261 => 302, 262 => 300, 263 => 301, 264 => 303, 265 => 304, 266 => 305, 267 => 306, 268 => 308, 269 => 307, 270 => 310,
        271 => 311, 272 => 312, 273 => 313, 274 => 314, 275 => 315, 276 => 316, 277 => 317, 278 => 318, 279 => 319, 280 => 321, 281 => 322,
        282 => 323, 283 => 324, 284 => 325, 285 => 326, 286 => 327, 287 => 328, 288 => 329, 289 => 330, 290 => 331, 291 => 332, 292 => 333,
        _ => return None,
    })
}

// --- flags de open: arm64 <-> x86-64 ---------------------------------------------------------

const A_DIRECTORY: u64 = 0o40000;
const A_NOFOLLOW: u64 = 0o100000;
const A_DIRECT: u64 = 0o200000;
const A_LARGEFILE: u64 = 0o400000;
const X_DIRECT: u64 = 0o40000;
const X_DIRECTORY: u64 = 0o200000;
const X_NOFOLLOW: u64 = 0o400000;

pub fn open_flags_to_host(f: u64) -> u64 {
    let mut r = f & !(A_DIRECTORY | A_NOFOLLOW | A_DIRECT | A_LARGEFILE);
    if f & A_DIRECTORY != 0 {
        r |= X_DIRECTORY;
    }
    if f & A_NOFOLLOW != 0 {
        r |= X_NOFOLLOW;
    }
    if f & A_DIRECT != 0 {
        r |= X_DIRECT;
    }
    r
}

pub fn open_flags_from_host(f: u64) -> u64 {
    let mut r = f & !(X_DIRECTORY | X_NOFOLLOW | X_DIRECT | 0o100000 /*x86 O_LARGEFILE slot if reported*/);
    if f & X_DIRECTORY != 0 {
        r |= A_DIRECTORY;
    }
    if f & X_NOFOLLOW != 0 {
        r |= A_NOFOLLOW;
    }
    if f & X_DIRECT != 0 {
        r |= A_DIRECT;
    }
    r | A_LARGEFILE
}

// --- struct stat -----------------------------------------------------------------------------

/// x86-64 struct stat (144 bytes) -> arm64 struct stat (128 bytes)
pub unsafe fn stat_to_guest(src: *const u8, dst: *mut u8) {
    let r64 = |o: usize| std::ptr::read_unaligned(src.add(o) as *const u64);
    let r32 = |o: usize| std::ptr::read_unaligned(src.add(o) as *const u32);
    let w64 = |o: usize, v: u64| std::ptr::write_unaligned(dst.add(o) as *mut u64, v);
    let w32 = |o: usize, v: u32| std::ptr::write_unaligned(dst.add(o) as *mut u32, v);
    std::ptr::write_bytes(dst, 0, 128);
    w64(0, r64(0)); // st_dev
    w64(8, r64(8)); // st_ino
    w32(16, r32(24)); // st_mode
    w32(20, r64(16) as u32); // st_nlink
    w32(24, r32(28)); // st_uid
    w32(28, r32(32)); // st_gid
    w64(32, r64(40)); // st_rdev
    w64(48, r64(48)); // st_size
    w32(56, r64(56) as u32); // st_blksize
    w64(64, r64(64)); // st_blocks
    for i in 0..6 {
        w64(72 + 8 * i, r64(72 + 8 * i)); // atim, mtim, ctim (sec,nsec)
    }
}

/// Al reves que `stat_to_guest`: `struct stat` de arm64 (128 bytes, `src`) -> el del host x86-64 (144 bytes, `dst`).
///
/// # Safety
/// `src` legible (128 bytes) y `dst` escribible (144 bytes).
pub unsafe fn stat_to_host(src: *const u8, dst: *mut u8) {
    let r64 = |o: usize| std::ptr::read_unaligned(src.add(o) as *const u64);
    let r32 = |o: usize| std::ptr::read_unaligned(src.add(o) as *const u32);
    let w64 = |o: usize, v: u64| std::ptr::write_unaligned(dst.add(o) as *mut u64, v);
    let w32 = |o: usize, v: u32| std::ptr::write_unaligned(dst.add(o) as *mut u32, v);
    std::ptr::write_bytes(dst, 0, 144);
    w64(0, r64(0)); // st_dev
    w64(8, r64(8)); // st_ino
    w64(16, r32(20) as u64); // st_nlink
    w32(24, r32(16)); // st_mode
    w32(28, r32(24)); // st_uid
    w32(32, r32(28)); // st_gid
    w64(40, r64(32)); // st_rdev
    w64(48, r64(48)); // st_size
    w64(56, r32(56) as i32 as i64 as u64); // st_blksize
    w64(64, r64(64)); // st_blocks
    for i in 0..6 {
        w64(72 + 8 * i, r64(72 + 8 * i)); // atim, mtim, ctim (sec,nsec)
    }
}

// --- epoll -----------------------------------------------------------------------------------

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct XEpoll {
    events: u32,
    data: u64,
}

fn neg(r: i64) -> i64 {
    if r == -1 {
        -(errno() as i64)
    } else {
        r
    }
}

pub fn host_syscall(nr: i64, a: [u64; 6]) -> i64 {
    neg(unsafe { syscall(nr, a[0], a[1], a[2], a[3], a[4], a[5]) } as i64)
}

// Llamada al sistema del guest con punto de reinicio (ver sig.rs, "Senales asincronas"). Si una senal llega con el
// hilo en una llamada al sistema del guest (`svc`, fase 2) y el `rip` esta en [heddle_sc, heddle_sc_insn] (aun no se
// ejecuto, o el nucleo la rebobino para reiniciarla tras la senal: SA_RESTART), el manejador del host la anota y
// lleva el `rip` a `heddle_sc_abort`, que devuelve `SC_RESTART`: el `svc` se repite despues de entregar la senal en
// la frontera de instruccion, como reinicia el nucleo ARM. Si llega en `heddle_sc_done` la llamada ya termino (con
// su resultado o -EINTR) y la senal se entrega despues, con ese resultado en x0. Sin pila propia: el `ret` de
// `heddle_sc_abort` vuelve al llamador igual que el de la llamada.
std::arch::global_asm!(
    ".globl heddle_sc",
    "heddle_sc:",
    "mov rax, rdi",
    "mov rdi, [rsi]",
    "mov rdx, [rsi + 16]",
    "mov r10, [rsi + 24]",
    "mov r8, [rsi + 32]",
    "mov r9, [rsi + 40]",
    "mov rsi, [rsi + 8]",
    ".globl heddle_sc_insn",
    "heddle_sc_insn:",
    "syscall",
    ".globl heddle_sc_done",
    "heddle_sc_done:",
    "ret",
    ".globl heddle_sc_abort",
    "heddle_sc_abort:",
    "mov rax, {restart}",
    "ret",
    restart = const SC_RESTART,
);

extern "C" {
    fn heddle_sc(nr: i64, a: *const [u64; 6]) -> i64;
    pub fn heddle_sc_insn();
    pub fn heddle_sc_done();
    pub fn heddle_sc_abort();
}

/// Resultado de `heddle_sc` cuando una senal la interrumpio antes de ejecutarse (o para reiniciarla): no es un errno.
pub const SC_RESTART: i64 = -(1 << 40);

/// Direccion de `heddle_sc` (inicio del tramo reiniciable).
pub fn sc_start() -> u64 {
    heddle_sc as unsafe extern "C" fn(i64, *const [u64; 6]) -> i64 as usize as u64
}

/// Llamada al sistema del host para un `svc` del guest: reiniciable (ver `heddle_sc`) salvo las que cambian la
/// mascara mientras esperan (rt_sigsuspend, pselect6, ppoll, epoll_pwait/2): su manejador debe ejecutarse con la
/// mascara temporal, asi que la senal se entrega en el acto, dentro de la llamada.
fn guest_syscall(nr: i64, a: [u64; 6]) -> i64 {
    if matches!(nr, 130 | 270 | 271 | 281 | 441) {
        return host_syscall(nr, a);
    }
    unsafe { heddle_sc(nr, &a) }
}

pub fn svc(c: &mut Cpu, _imm: u16) {
    let n = c.x[8];
    let a = [c.x[0], c.x[1], c.x[2], c.x[3], c.x[4], c.x[5]];
    let r = translate(c, n, a);
    if r == SC_RESTART {
        // interrumpida antes de ejecutarse: se repite el `svc` tras entregar la senal (x0 intacto)
        c.pc -= 4;
        return;
    }
    c.x[0] = r as u64;
}


// ---------------------------------------------------------------------------------------------
// Paginas del guest mayores que las del host (`mem::guest_page`, 16 KiB)
// ---------------------------------------------------------------------------------------------
//
// Como un kernel arm64 de 16 KiB: mmap/munmap/mprotect/mremap/madvise/msync exigen direcciones (y desplazamiento de
// archivo) alineados a 16 KiB y redondean las longitudes; un mapeo nuevo sin MAP_FIXED (o que mremap mueve) cae en
// una direccion alineada a 16 KiB (se reserva con holgura y se recorta). Con paginas de 4 KiB no se toca nada.

const ENOMEM: i64 = 12;
const MAP_FIXED: u64 = 0x10;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_FIXED_NOREPLACE: u64 = 0x10_0000;
const MREMAP_MAYMOVE: u64 = 1;
const MREMAP_FIXED: u64 = 2;

fn host_page() -> u64 {
    crate::mem::PAGE as u64
}

/// Longitud redondeada a `pg` (None si desborda).
fn round_len(len: u64, pg: u64) -> Option<u64> {
    len.checked_add(pg - 1).map(|x| x & !(pg - 1))
}

fn ok(r: i64) -> bool {
    !(-4095..0).contains(&r)
}

/// Reserva `len` bytes sin acceso en una direccion alineada a `pg` (con holgura que luego se recorta): su inicio.
fn reserve_aligned(len: u64, pg: u64) -> Result<u64, i64> {
    let total = len + pg - host_page();
    // PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE
    let r = host_syscall(9, [0, total, 0, 0x22 | 0x4000, u64::MAX, 0]);
    if !ok(r) {
        return Err(r);
    }
    let base = r as u64;
    let at = (base + pg - 1) & !(pg - 1);
    if at > base {
        host_syscall(11, [base, at - base, 0, 0, 0, 0]);
    }
    if base + total > at + len {
        host_syscall(11, [at + len, base + total - (at + len), 0, 0, 0, 0]);
    }
    Ok(at)
}

/// mmap del guest con paginas de `pg` (a[2] ya sin PROT_EXEC/BTI/MTE).
fn mmap_pages(mut a: [u64; 6], pg: u64) -> i64 {
    if pg <= host_page() {
        return host_syscall(9, a);
    }
    if a[5] % pg != 0 {
        return -EINVAL;
    }
    if a[1] == 0 {
        return -EINVAL;
    }
    let Some(len) = round_len(a[1], pg) else { return -ENOMEM };
    a[1] = len;
    if a[3] & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0 {
        if a[0] % pg != 0 {
            return -EINVAL;
        }
        return host_syscall(9, a);
    }
    // pista: el kernel la redondea hacia arriba
    a[0] = round_len(a[0], pg).unwrap_or(0);
    let r = host_syscall(9, a);
    if !ok(r) || r as u64 % pg == 0 {
        if ok(r) {
            file_tail_zeros(r as u64, &a, pg);
        }
        return r;
    }
    host_syscall(11, [r as u64, len, 0, 0, 0, 0]);
    let at = match reserve_aligned(len, pg) {
        Ok(at) => at,
        Err(e) => return e,
    };
    let mut b = a;
    b[0] = at;
    b[3] |= MAP_FIXED;
    let r = host_syscall(9, b);
    if !ok(r) {
        host_syscall(11, [at, len, 0, 0, 0, 0]);
    } else {
        file_tail_zeros(at, &b, pg);
    }
    r
}

/// Mapeo de archivo que acaba a media pagina del guest: en el dispositivo, la ultima pagina de 16 KiB tiene ceros
/// tras el final del archivo; en el host, desde la siguiente pagina de 4 KiB, SIGBUS. Se cubre ese tramo con paginas
/// anonimas con la misma proteccion (privadas: lo que se escriba ahi no llega a otros mapeos del archivo; en el
/// dispositivo tampoco llega al archivo). `a` es la peticion ya hecha (longitud redondeada).
fn file_tail_zeros(at: u64, a: &[u64; 6], pg: u64) {
    if a[3] & MAP_ANONYMOUS != 0 {
        return;
    }
    let mut st = [0u8; 144];
    if host_syscall(5, [a[4], st.as_mut_ptr() as u64, 0, 0, 0, 0]) != 0 {
        return;
    }
    // struct stat de x86-64: st_mode en 24, st_size en 48
    let mode = u32::from_le_bytes(st[24..28].try_into().unwrap());
    let size = u64::from_le_bytes(st[48..56].try_into().unwrap());
    if mode & 0o170000 != 0o100000 || size <= a[5] {
        return;
    }
    let end = size - a[5];
    let z0 = (end + host_page() - 1) & !(host_page() - 1);
    let z1 = ((end + pg - 1) & !(pg - 1)).min(a[1]);
    if z0 < z1 {
        host_syscall(9, [at + z0, z1 - z0, a[2], MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, u64::MAX, 0]);
    }
}

// ---- /proc/self/maps y smaps, mincore, mlock y brk con paginas del guest mayores ----

/// `/proc/self/maps` (o `/proc/<pid>`, `/proc/thread-self`, `.../task/<tid>/`): Some(false) maps, Some(true) smaps.
fn proc_maps_kind(path: &[u8]) -> Option<bool> {
    let p = path.strip_prefix(b"/proc/")?;
    let (who, rest) = p.split_at(p.iter().position(|&c| c == b'/')?);
    let rest = &rest[1..];
    let me = who == b"self" || who == b"thread-self" || std::str::from_utf8(who).ok().and_then(|w| w.parse::<i32>().ok()) == Some(unsafe { getpid() });
    if !me {
        return None;
    }
    let rest = match rest.strip_prefix(b"task/") {
        Some(t) => {
            let i = t.iter().position(|&c| c == b'/')?;
            if i == 0 || !t[..i].iter().all(u8::is_ascii_digit) {
                return None;
            }
            &t[i + 1..]
        }
        None => rest,
    };
    match rest {
        b"maps" => Some(false),
        b"smaps" => Some(true),
        _ => None,
    }
}

/// Contenido de maps/smaps visto con paginas de `pg`: cada VMA con inicio y fin redondeados a la pagina del guest
/// (como los ve un kernel de 16 KiB), sin solaparse con la anterior (una que queda vacia desaparece: la cola de
/// ceros de un mapeo de archivo, las paginas de guarda de 4 KiB del host), el desplazamiento de archivo corregido y,
/// en smaps, `Size`, `KernelPageSize` y `MMUPageSize` en esa pagina.
pub fn rewrite_maps(data: &[u8], pg: u64, smaps: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let (mut prev_end, mut skip, mut size) = (0u64, false, 0u64);
    for line in data.split_inclusive(|&c| c == b'\n') {
        let header = (|| {
            let l = std::str::from_utf8(line).ok()?;
            let (range, rest) = l.split_once(' ')?;
            let (s, e) = range.split_once('-')?;
            let (start, end) = (u64::from_str_radix(s, 16).ok()?, u64::from_str_radix(e, 16).ok()?);
            let (perms, rest) = rest.split_once(' ')?;
            let (off, rest) = rest.split_once(' ')?;
            Some((start, end, perms, u64::from_str_radix(off, 16).ok()?, rest))
        })();
        if let Some((start, end, perms, off, rest)) = header {
            let s2 = (start & !(pg - 1)).max(prev_end);
            let e2 = end.checked_add(pg - 1).map_or(end, |x| x & !(pg - 1));
            skip = s2 >= e2;
            if skip {
                continue;
            }
            prev_end = e2;
            size = e2 - s2;
            let off2 = off.saturating_sub(start - s2);
            out.extend_from_slice(format!("{:08x}-{:08x} {} {:08x} {}", s2, e2, perms, off2, rest).as_bytes());
            continue;
        }
        if skip {
            continue;
        }
        if smaps {
            let key = line.split(|&c| c == b':').next().unwrap_or(b"");
            let v = match key {
                b"Size" => Some(size / 1024),
                b"KernelPageSize" | b"MMUPageSize" => Some(pg / 1024),
                _ => None,
            };
            if let Some(v) = v {
                out.extend_from_slice(format!("{:<16}{:>8} kB\n", format!("{}:", std::str::from_utf8(key).unwrap_or("")), v).as_bytes());
                continue;
            }
        }
        out.extend_from_slice(line);
    }
    out
}

/// open/openat de maps o smaps de este proceso con paginas del guest mayores, despues de abrir el del host (`fd`): si
/// la ruta es esa, un memfd con el contenido reescrito (`rewrite_maps`) en lugar de `fd`, con su O_CLOEXEC. Cualquier
/// otro caso (o sin memfd) devuelve `fd` tal cual. Se mira la ruta solo despues de que el kernel la leyo sin error.
pub fn proc_maps_after_open(fd: i64, path: u64, flags: u64) -> i64 {
    let pg = crate::mem::guest_page();
    if fd < 0 || pg <= host_page() {
        return fd;
    }
    let p = unsafe { std::ffi::CStr::from_ptr(path as *const std::os::raw::c_char) }.to_bytes();
    let Some(smaps) = proc_maps_kind(p) else { return fd };
    match maps_memfd(fd, pg, smaps, flags & 0o2000000 != 0) {
        Some(m) => {
            host_syscall(3, [fd as u64, 0, 0, 0, 0, 0]);
            m
        }
        None => fd,
    }
}

/// memfd con el contenido de maps/smaps (leido de `fd`) visto con paginas de `pg`.
fn maps_memfd(fd: i64, pg: u64, smaps: bool, cloexec: bool) -> Option<i64> {
    let mut data = Vec::with_capacity(64 << 10);
    let mut buf = [0u8; 16384];
    loop {
        let r = host_syscall(0, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]);
        if r < 0 {
            return None;
        }
        if r == 0 {
            break;
        }
        data.extend_from_slice(&buf[..r as usize]);
    }
    let text = rewrite_maps(&data, pg, smaps);
    let m = host_syscall(319, [if smaps { c"smaps".as_ptr() } else { c"maps".as_ptr() } as u64, cloexec as u64, 0, 0, 0, 0]);
    if m < 0 {
        return None;
    }
    let mut done = 0usize;
    while done < text.len() {
        let w = host_syscall(1, [m as u64, text[done..].as_ptr() as u64, (text.len() - done) as u64, 0, 0, 0]);
        if w <= 0 {
            host_syscall(3, [m as u64, 0, 0, 0, 0, 0]);
            return None;
        }
        done += w as usize;
    }
    host_syscall(8, [m as u64, 0, 0, 0, 0, 0]);
    Some(m)
}

/// Copia `src` a memoria del guest en `dst` como lo haria el kernel: -EFAULT si no se puede escribir
/// (`process_vm_writev` sobre el propio proceso; sin ella, copia directa).
fn copy_to_guest(dst: u64, src: &[u8]) -> i64 {
    #[repr(C)]
    struct Iov {
        base: u64,
        len: usize,
    }
    let local = Iov { base: src.as_ptr() as u64, len: src.len() };
    let remote = Iov { base: dst, len: src.len() };
    let r = host_syscall(311, [unsafe { getpid() } as u64, &local as *const Iov as u64, 1, &remote as *const Iov as u64, 1, 0]);
    if r == src.len() as i64 {
        return 0;
    }
    if r == -ENOSYS || r == -1 {
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
        return 0;
    }
    -EFAULT
}

/// mincore con paginas de `pg`: inicio alineado, un byte por pagina del guest (residente si lo esta alguna de sus
/// paginas del host).
fn mincore_pages(a: &[u64; 6], pg: u64) -> i64 {
    let (start, vec) = (a[0], a[2]);
    if start % pg != 0 {
        return -EINVAL;
    }
    let Some(len) = round_len(a[1], pg) else { return -ENOMEM };
    if start.checked_add(len).is_none() {
        return -ENOMEM;
    }
    let per = (pg / host_page()) as usize;
    let n = len / pg;
    let mut hb = [0u8; 4096];
    let mut gb = [0u8; 1024];
    let chunk = (hb.len() / per).min(gb.len()) as u64;
    let mut done = 0u64;
    while done < n {
        let k = (n - done).min(chunk);
        let r = host_syscall(27, [start + done * pg, k * pg, hb.as_mut_ptr() as u64, 0, 0, 0]);
        if r != 0 {
            return r;
        }
        for i in 0..k as usize {
            gb[i] = hb[i * per..(i + 1) * per].iter().fold(0, |x, &b| x | (b & 1));
        }
        let r = copy_to_guest(vec + done, &gb[..k as usize]);
        if r != 0 {
            return r;
        }
        done += k;
    }
    0
}

/// mlock/munlock/mlock2 con paginas de `pg`: el kernel bloquea paginas enteras (inicio hacia abajo, fin hacia arriba).
fn lock_range(a: &mut [u64; 6], pg: u64) {
    let off = a[0] & (pg - 1);
    a[0] -= off;
    a[1] = a[1].wrapping_add(off).wrapping_add(pg - 1) & !(pg - 1);
}

/// Final del segmento de datos del guest (`brk`): el valor pedido (como el kernel, que guarda el exacto) y su inicio.
static BRK: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((0, 0));

/// brk con paginas de `pg`: el host extiende hasta la pagina del guest (en el dispositivo es accesible hasta el
/// final de su pagina de 16 KiB) y el guest ve el valor que pidio; el inicio queda alineado a su pagina.
fn brk_pages(want: u64, pg: u64) -> i64 {
    let mut g = crate::monitor::lock(&BRK);
    if g.0 == 0 {
        let h = host_syscall(12, [0, 0, 0, 0, 0, 0]) as u64;
        let start = (h + pg - 1) & !(pg - 1);
        if start != h && host_syscall(12, [start, 0, 0, 0, 0, 0]) as u64 != start {
            return h as i64;
        }
        *g = (start, start);
    }
    let (start, cur) = *g;
    if want < start {
        return cur as i64;
    }
    let Some(top) = round_len(want, pg) else { return cur as i64 };
    if host_syscall(12, [top, 0, 0, 0, 0, 0]) as u64 != top {
        return cur as i64;
    }
    g.1 = want;
    want as i64
}

/// munmap / mprotect / madvise / msync del guest con paginas de `pg`: comprobaciones del kernel y longitud
/// redondeada. Some(error o resultado) si no hay que llamar al host; si no, la longitud que se le pasa.
fn range_pages(nr: u64, a: &[u64; 6], pg: u64) -> Result<u64, i64> {
    let (start, len) = (a[0], a[1]);
    match nr {
        // munmap: inicio alineado, longitud no nula
        215 => {
            if start % pg != 0 {
                return Err(-EINVAL);
            }
            match round_len(len, pg) {
                Some(l) if l != 0 => Ok(l),
                _ => Err(-EINVAL),
            }
        }
        // mprotect: inicio alineado; longitud 0 no hace nada
        226 => {
            if start % pg != 0 {
                return Err(-EINVAL);
            }
            if len == 0 {
                return Err(0);
            }
            match round_len(len, pg) {
                Some(l) if start.checked_add(l).is_some() => Ok(l),
                _ => Err(-ENOMEM),
            }
        }
        // madvise: inicio alineado; la longitud que desborda es EINVAL (0 la valida el host)
        233 => {
            if start % pg != 0 {
                return Err(-EINVAL);
            }
            match round_len(len, pg) {
                Some(l) if start.checked_add(l).is_some() && (l != 0 || len == 0) => Ok(l),
                _ => Err(-EINVAL),
            }
        }
        // msync: banderas, inicio alineado, MS_ASYNC con MS_SYNC
        227 => {
            let flags = a[2];
            if flags & !7 != 0 || start % pg != 0 || (flags & 1 != 0 && flags & 4 != 0) {
                return Err(-EINVAL);
            }
            round_len(len, pg).ok_or(-ENOMEM)
        }
        _ => Ok(len),
    }
}

/// mremap del guest con paginas de `pg`.
fn mremap_pages(mut a: [u64; 6], pg: u64) -> i64 {
    if pg <= host_page() {
        return host_syscall(25, a);
    }
    if a[0] % pg != 0 {
        return -EINVAL;
    }
    let (Some(old), Some(new)) = (round_len(a[1], pg), round_len(a[2], pg)) else { return -EINVAL };
    if new == 0 {
        return -EINVAL;
    }
    if a[3] & MREMAP_FIXED != 0 && a[4] % pg != 0 {
        return -EINVAL;
    }
    a[1] = old;
    a[2] = new;
    let r = host_syscall(25, a);
    if !ok(r) || r as u64 % pg == 0 || a[3] & MREMAP_FIXED != 0 {
        return r;
    }
    // movido a una direccion sin alinear: otra vez, a una reserva alineada
    let at = match reserve_aligned(new, pg) {
        Ok(at) => at,
        Err(_) => return r, // sin sitio: queda donde esta (alineado a la pagina del host)
    };
    let r2 = host_syscall(25, [r as u64, new, new, MREMAP_MAYMOVE | MREMAP_FIXED, at, 0]);
    if ok(r2) {
        r2
    } else {
        host_syscall(11, [at, new, 0, 0, 0, 0]);
        r
    }
}

pub fn translate(c: &mut Cpu, n: u64, mut a: [u64; 6]) -> i64 {
    let mask = 0x00FF_FFFF_FFFF_FFFFu64;
    let _ = mask;
    match n {
        56 => {
            // openat(dirfd, path, flags, mode); /proc/cpuinfo y demas archivos de la CPU, emulados (`procemu`); maps/smaps
            // de este proceso con paginas del guest mayores
            if let Some(r) = crate::procemu::try_open(a[0], a[1], a[2]) {
                return r;
            }
            let fl = a[2];
            a[2] = open_flags_to_host(a[2]);
            proc_maps_after_open(host_syscall(257, a), a[1], fl)
        }
        25 => {
            // fcntl(fd, cmd, arg)
            match a[1] {
                4 => {
                    a[2] = open_flags_to_host(a[2]);
                    host_syscall(72, a)
                }
                3 => {
                    let r = host_syscall(72, a);
                    if r >= 0 {
                        open_flags_from_host(r as u64) as i64
                    } else {
                        r
                    }
                }
                _ => host_syscall(72, a),
            }
        }
        80 | 79 => {
            // fstat(fd, buf) / newfstatat(dirfd, path, buf, flags)
            let mut tmp = [0u8; 144];
            let (nr, args, out) = if n == 80 {
                (5, [a[0], tmp.as_mut_ptr() as u64, 0, 0, 0, 0], a[1])
            } else {
                (262, [a[0], a[1], tmp.as_mut_ptr() as u64, a[3], 0, 0], a[2])
            };
            let r = host_syscall(nr, args);
            if r >= 0 {
                unsafe { stat_to_guest(tmp.as_ptr(), out as *mut u8) };
            }
            r
        }
        59 | 24 => {
            // pipe2 / dup3: O_CLOEXEC / O_NONBLOCK / O_DIRECT
            let i = if n == 59 { 1 } else { 2 };
            a[i] = open_flags_to_host(a[i]);
            host_syscall(if n == 59 { 293 } else { 292 }, a)
        }
        21 => {
            // epoll_ctl(epfd, op, fd, event*)
            if a[3] == 0 {
                return host_syscall(233, a);
            }
            let g = a[3] as *const u8;
            let ev = unsafe { XEpoll { events: std::ptr::read_unaligned(g as *const u32), data: std::ptr::read_unaligned(g.add(8) as *const u64) } };
            a[3] = &ev as *const _ as u64;
            host_syscall(233, a)
        }
        22 => {
            // epoll_pwait(epfd, events*, maxevents, timeout, sigmask*, sigsetsize)
            let max = (a[2] as i32).max(0) as usize;
            let mut tmp = vec![XEpoll { events: 0, data: 0 }; max.max(1)];
            let out = a[1];
            a[1] = tmp.as_mut_ptr() as u64;
            let r = host_syscall(281, a);
            if r > 0 {
                for i in 0..r as usize {
                    let e = tmp[i];
                    unsafe {
                        std::ptr::write_unaligned((out + 16 * i as u64) as *mut u32, e.events);
                        std::ptr::write_unaligned((out + 16 * i as u64 + 4) as *mut u32, 0);
                        std::ptr::write_unaligned((out + 16 * i as u64 + 8) as *mut u64, e.data);
                    }
                }
            }
            r
        }
        134 => {
            // rt_sigaction(sig, act*, oact*, sigsetsize): estructura del kernel arm64 {handler, flags, restorer, mask}
            // (arm64 define SA_RESTORER; el restorer del guest no se usa: sus manejadores vuelven por la ranura de
            // retorno, y en oact se devuelve 0)
            if a[3] != 8 {
                return -EINVAL;
            }
            let new = if a[1] != 0 {
                let p = a[1] as *const u64;
                unsafe { Some(GuestAct { handler: *p, flags: *p.add(1), mask: *p.add(3) }) }
            } else {
                None
            };
            match sig::guest_sigaction(a[0] as i32, new) {
                Ok(old) => {
                    if a[2] != 0 {
                        let p = a[2] as *mut u64;
                        unsafe {
                            *p = old.handler;
                            *p.add(1) = old.flags;
                            *p.add(2) = 0;
                            *p.add(3) = old.mask;
                        }
                    }
                    0
                }
                Err(e) => -(e as i64),
            }
        }
        135 => {
            // rt_sigprocmask(how, set*, oset*, sigsetsize): directa al nucleo, sin los filtros de libsigchain ni bionic
            if a[3] != 8 {
                return -EINVAL;
            }
            crate::libc_hle::guest_mask_call(a[0], a[1], a[2], true)
        }
        139 => -ENOSYS, // rt_sigreturn no existe para el guest (los manejadores vuelven por la ranura de retorno)
        293 => -ENOSYS, // rseq: lo gestiona el traductor, el guest no puede registrarlo
        93 => {
            // exit: termina el hilo como el nucleo, sin codigo guest (ni destructores ni cleanup); solo se libera el
            // estado del puente
            unsafe { crate::libc_hle::thread_exit_syscall(a[0]) }
        }
        94 => unsafe { _exit(a[0] as i32) },
        220 => {
            // clone: solo semantica fork/vfork (sin CLONE_VM de hilos); los hilos pasan por pthread_create
            let flags = a[0];
            const CLONE_VM: u64 = 0x100;
            const CLONE_THREAD: u64 = 0x10000;
            if flags & CLONE_THREAD != 0 {
                return -ENOSYS;
            }
            let _ = CLONE_VM;
            let p = unsafe { fork() };
            if p < 0 {
                -(errno() as i64)
            } else {
                p as i64
            }
        }
        222 => {
            // PROT_BTI / PROT_MTE (arm64) y PROT_EXEC: el codigo guest se traduce leyendolo, nunca lo ejecuta el
            // host; si el host llama a una direccion guest, el fallo de ejecucion se redirige a un trampolin
            let exec = a[2] & 4 != 0;
            a[2] &= !0x34;
            let pg = crate::mem::guest_page();
            let r = mmap_pages(a, pg);
            if ok(r) {
                // se recuerda si el guest la pidio ejecutable (unica via para que una region anonima cuente como
                // "ejecutable por el guest"); un mapeo nuevo sin exec borra lo que hubiera en ese rango
                crate::mem::guest_prot(r as u64, round_len(a[1], pg).unwrap_or(a[1]), exec);
            }
            r
        }
        226 | 215 | 233 | 227 => {
            // mprotect, munmap, madvise, msync: con paginas del guest mayores, las comprobaciones de su kernel
            let pg = crate::mem::guest_page();
            if pg > host_page() {
                match range_pages(n, &a, pg) {
                    Ok(l) => a[1] = l,
                    Err(r) => return r,
                }
            }
            match n {
                226 => {
                    let exec = a[2] & 4 != 0;
                    a[2] &= !0x34;
                    let r = host_syscall(10, a);
                    if r == 0 {
                        crate::mem::guest_prot(a[0], a[1], exec);
                    }
                    r
                }
                215 => {
                    let r = host_syscall(11, a);
                    if r == 0 {
                        crate::mem::guest_unmap(a[0], a[1]);
                    }
                    r
                }
                233 => host_syscall(28, a),
                _ => guest_syscall(26, a), // msync puede esperar al disco: reiniciable
            }
        }
        232 | 228 | 229 | 284 | 214 if crate::mem::guest_page() > host_page() => {
            // mincore, mlock, munlock, mlock2 y brk con paginas del guest mayores
            let pg = crate::mem::guest_page();
            match n {
                232 => mincore_pages(&a, pg),
                214 => brk_pages(a[0], pg),
                _ => {
                    lock_range(&mut a, pg);
                    guest_syscall(map_nr(n).unwrap_or(-1), a)
                }
            }
        }
        216 => {
            // mremap: cambia mapeos (las paginas ya comprobadas escribibles por el monitor dejan de valer)
            let r = mremap_pages(a, crate::mem::guest_page());
            crate::monitor::prot_changed();
            r
        }
        160 => {
            // uname: el guest debe ver aarch64
            let r = host_syscall(63, a);
            if r >= 0 {
                let m = (a[0] + 4 * 65) as *mut u8;
                let s = b"aarch64\0";
                unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), m, s.len()) };
            }
            r
        }
        167 => {
            // prctl: opciones solo-arm64 (PAC, MTE, tagged addr)
            match a[0] {
                55 | 56 | 57 | 58 | 59 | 60 | 61 | 62 => -EINVAL,
                _ => host_syscall(157, a),
            }
        }
        168 => host_syscall(309, a),
        // lectura/escritura a memoria del guest: sin conversion
        _ => match map_nr(n) {
            Some(x) => guest_syscall(x, a),
            None => {
                eprintln!("[heddle] syscall arm64 {} ({}) sin traduccion (ENOSYS) pc={}", n, crate::diag::syscall_name(n), crate::elf::describe_addr(c.pc));
                -ENOSYS
            }
        },
    }
}

#[allow(dead_code)]
fn _efault() -> i64 {
    -EFAULT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Paginas de 16 KiB como un kernel arm64 de 16 KiB: alineacion exigida, longitudes redondeadas y mapeos nuevos
    /// (tambien los que mueve mremap) alineados.
    #[test]
    fn paginas_de_16k_como_el_kernel() {
        const P: u64 = 16384;
        let anon = |addr: u64, len: u64, flags: u64, off: u64| mmap_pages([addr, len, 3, 0x22 | flags, u64::MAX, off], P);
        for _ in 0..16 {
            let r = anon(0, 5000, 0, 0);
            assert!(ok(r) && r as u64 % P == 0, "{:#x}", r);
            // la longitud se redondeo a 16 KiB: toda la pagina es accesible
            unsafe { *((r as u64 + P - 1) as *mut u8) = 1 };
            assert_eq!(range_pages(215, &[r as u64, 5000, 0, 0, 0, 0], P), Ok(P));
            assert_eq!(host_syscall(11, [r as u64, P, 0, 0, 0, 0]), 0);
        }
        assert_eq!(anon(0, 0, 0, 0), -EINVAL);
        assert_eq!(anon(0, 4096, 0, 4096), -EINVAL, "desplazamiento sin alinear a 16 KiB");
        assert_eq!(anon(0x7000_0000_1000, 4096, MAP_FIXED, 0), -EINVAL);
        assert_eq!(anon(0x7000_0000_1000, 4096, MAP_FIXED_NOREPLACE, 0), -EINVAL);
        assert_eq!(anon(0, u64::MAX - 100, 0, 0), -ENOMEM);
        // munmap, mprotect, madvise, msync
        assert_eq!(range_pages(215, &[0x1000, 4096, 0, 0, 0, 0], P), Err(-EINVAL));
        assert_eq!(range_pages(215, &[0x4000, 0, 0, 0, 0, 0], P), Err(-EINVAL));
        assert_eq!(range_pages(226, &[0x4000, 0, 0, 0, 0, 0], P), Err(0));
        assert_eq!(range_pages(226, &[0x4000, 1, 0, 0, 0, 0], P), Ok(P));
        assert_eq!(range_pages(226, &[0x5000, 1, 0, 0, 0, 0], P), Err(-EINVAL));
        assert_eq!(range_pages(233, &[0x5000, 1, 0, 0, 0, 0], P), Err(-EINVAL));
        assert_eq!(range_pages(233, &[0x4000, P + 1, 0, 0, 0, 0], P), Ok(2 * P));
        assert_eq!(range_pages(227, &[0x4000, 1, 4, 0, 0, 0], P), Ok(P));
        assert_eq!(range_pages(227, &[0x4000, 1, 5, 0, 0, 0], P), Err(-EINVAL));
        assert_eq!(range_pages(227, &[0x2000, 1, 4, 0, 0, 0], P), Err(-EINVAL));
        // mremap que crece y se mueve: sigue alineado
        let r = anon(0, P, 0, 0) as u64;
        let mut cur = r;
        for k in 2..10u64 {
            let n = mremap_pages([cur, (k - 1) * P, k * P, MREMAP_MAYMOVE, 0, 0], P);
            assert!(ok(n) && n as u64 % P == 0, "{:#x}", n);
            cur = n as u64;
        }
        assert_eq!(mremap_pages([cur + 4096, P, 2 * P, MREMAP_MAYMOVE, 0, 0], P), -EINVAL);
        assert_eq!(host_syscall(11, [cur, 9 * P, 0, 0, 0, 0]), 0);
    }

    /// rt_sigaction crudo con la estructura del kernel arm64: la mascara va despues de sa_restorer.
    #[test]
    fn rt_sigaction_crudo_con_sa_restorer() {
        const SIGWINCH: u64 = 28;
        const SA_RESTORER: u64 = 0x0400_0000;
        let mut c = Cpu::new();
        let act: [u64; 4] = [1, SA_RESTORER, 0xDEAD_BEEF, 1 << 4]; // SIG_IGN, restorer, mascara {SIGTRAP}
        let mut old = [u64::MAX; 4];
        let dfl: [u64; 4] = [0; 4];
        assert_eq!(translate(&mut c, 134, [SIGWINCH, act.as_ptr() as u64, 0, 8, 0, 0]), 0);
        assert_eq!(translate(&mut c, 134, [SIGWINCH, dfl.as_ptr() as u64, old.as_mut_ptr() as u64, 8, 0, 0]), 0);
        assert_eq!(old, [1, SA_RESTORER, 0, 1 << 4]);
        assert_eq!(translate(&mut c, 134, [SIGWINCH, 0, 0, 4, 0, 0]), -EINVAL);
    }
}
