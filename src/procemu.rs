//! Archivos del sistema que describen la CPU, emulados desde `feat` para que el guest vea lo mismo que en un ARM64
//! con Linux: `/proc/cpuinfo`, `/proc/self/auxv` (y `/proc/<pid>/auxv`, `/proc/thread-self/auxv`,
//! `/proc/self/task/<tid>/auxv`) y `/sys/devices/system/cpu/cpuN/regs/identification/{midr_el1,revidr_el1}`.
//!
//! Se interceptan al abrirse: `open`/`openat`/`__open_2`/`creat` (HLE), el `svc` `openat` y `fopen`/`freopen`
//! (HLE sobre `fdopen`). El contenido se escribe en un `memfd` sellado y el guest recibe un descriptor de solo lectura
//! reabierto desde `/proc/self/fd` (su propio desplazamiento, sin escritura posible). Sin `memfd_create`, un archivo
//! en el directorio privado de la app (o el temporal) que se borra en cuanto esta abierto.
//!
//! Formatos de Linux 6.6: `c_show` de `arch/arm64/kernel/cpuinfo.c` (un bloque por CPU en linea; los nombres de
//! `Features` en el orden de `hwcap_str`) y `CPUREGS_ATTR_RO` (`"0x%016llx\n"`).

use crate::syscall::host_syscall;

const AT_FDCWD: u64 = (-100i64) as u64;
const O_ACCMODE: u64 = 3;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;
const O_PATH: u64 = 0o10000000;
/// O_DIRECTORY de arm64 (en x86-64 es 0o200000).
const A_DIRECTORY: u64 = 0o40000;

const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;

/// Que archivo emulado nombra una ruta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    CpuInfo,
    Auxv,
    Midr(u32),
    Revidr(u32),
}

/// Ultimos componentes que pueden nombrar un archivo emulado: solo con ellos se resuelve una ruta relativa.
const LEAVES: [&[u8]; 4] = [b"cpuinfo", b"auxv", b"midr_el1", b"revidr_el1"];

/// Normaliza una ruta absoluta de forma lexica (`//`, `.`, `..`).
fn normalize(p: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in p.split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = Vec::with_capacity(p.len());
    for c in parts {
        out.push(b'/');
        out.extend_from_slice(c);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

fn readlink(p: &str) -> Option<Vec<u8>> {
    let mut buf = [0u8; 4096];
    let path = std::ffi::CString::new(p).ok()?;
    let n = host_syscall(267, [AT_FDCWD, path.as_ptr() as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0]);
    (n > 0 && (n as usize) < buf.len()).then(|| buf[..n as usize].to_vec())
}

fn num(s: &[u8]) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.iter().all(u8::is_ascii_digit) || (s.len() > 1 && s[0] == b'0') {
        return None;
    }
    std::str::from_utf8(s).ok()?.parse().ok()
}

/// `n` es este proceso o uno de sus hilos (`/proc/<n>` describe este proceso)?
fn ours(n: u32) -> bool {
    if n as i64 == host_syscall(39, [0; 6]) {
        return true;
    }
    let p = format!("/proc/self/task/{}\0", n);
    host_syscall(269, [AT_FDCWD, p.as_ptr() as u64, 0, 0, 0, 0]) == 0
}

/// CPUs en linea (`/sys/devices/system/cpu/online`, como `for_each_online_cpu`); si no se puede leer, las de la
/// afinidad del hilo.
pub fn online_cpus() -> Vec<u32> {
    if let Some(s) = read_host("/sys/devices/system/cpu/online") {
        let mut v = Vec::new();
        for r in String::from_utf8_lossy(&s).trim().split(',') {
            let mut it = r.split('-');
            let a = it.next().and_then(|x| x.trim().parse::<u32>().ok());
            let b = it.next().and_then(|x| x.trim().parse::<u32>().ok()).or(a);
            if let (Some(a), Some(b)) = (a, b) {
                if b >= a && b < 4096 {
                    v.extend(a..=b);
                }
            }
        }
        if !v.is_empty() {
            return v;
        }
    }
    let mut set = [0u64; 64];
    let n = host_syscall(204, [0, std::mem::size_of_val(&set) as u64, set.as_mut_ptr() as u64, 0, 0, 0]);
    let v: Vec<u32> = if n > 0 { (0..(n as u32 * 8).min(4096)).filter(|&i| set[i as usize / 64] >> (i % 64) & 1 != 0).collect() } else { Vec::new() };
    if v.is_empty() {
        vec![0]
    } else {
        v
    }
}

/// Clasifica una ruta absoluta ya normalizada.
fn classify_abs(p: &[u8]) -> Option<Kind> {
    if p == b"/proc/cpuinfo" {
        return Some(Kind::CpuInfo);
    }
    let parts: Vec<&[u8]> = p.split(|&b| b == b'/').skip(1).collect();
    match parts.as_slice() {
        [b"proc", b"self" | b"thread-self", b"auxv"] => Some(Kind::Auxv),
        [b"proc", pid, b"auxv"] if num(pid).is_some_and(ours) => Some(Kind::Auxv),
        [b"proc", b"self", b"task", tid, b"auxv"] if num(tid).is_some_and(ours) => Some(Kind::Auxv),
        [b"proc", pid, b"task", tid, b"auxv"] if num(pid).is_some_and(ours) && num(tid).is_some_and(ours) => Some(Kind::Auxv),
        [b"sys", b"devices", b"system", b"cpu", cpu, b"regs", b"identification", leaf] => {
            let n = num(cpu.strip_prefix(b"cpu")?)?;
            if !online_cpus().contains(&n) {
                return None;
            }
            match *leaf {
                b"midr_el1" => Some(Kind::Midr(n)),
                b"revidr_el1" => Some(Kind::Revidr(n)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Archivo emulado que nombra `path` (relativo a `dirfd`, como `openat`), si alguno. Una ruta relativa solo se
/// resuelve si su ultimo componente puede serlo (no cuesta nada en las demas aperturas).
pub fn classify(dirfd: u64, path: u64) -> Option<Kind> {
    if path == 0 {
        return None;
    }
    let p = unsafe { std::ffi::CStr::from_ptr(path as *const std::os::raw::c_char) }.to_bytes();
    let leaf = p.rsplit(|&b| b == b'/').next().unwrap_or(p);
    if !LEAVES.contains(&leaf) {
        return None;
    }
    if p.first() == Some(&b'/') {
        return classify_abs(&normalize(p));
    }
    let base = if dirfd as i32 == -100 { readlink("/proc/self/cwd")? } else { readlink(&format!("/proc/self/fd/{}", dirfd as i32))? };
    if base.first() != Some(&b'/') {
        return None;
    }
    let mut full = base;
    full.push(b'/');
    full.extend_from_slice(p);
    classify_abs(&normalize(&full))
}

/// BogoMIPS de Linux arm64: `loops_per_jiffy` es la frecuencia del temporizador entre HZ.
fn bogomips() -> String {
    let f = crate::interp::CNTFRQ;
    format!("{}.{:02}", f / 500_000, f / 5_000 % 100)
}

/// Texto de `/proc/cpuinfo` para el modelo elegido (todas las CPUs en linea con el mismo MIDR).
pub fn cpuinfo() -> String {
    let midr = crate::feat::midr();
    let feats = crate::feat::features_line(crate::feat::hwcap(), crate::feat::hwcap2());
    let mut s = String::new();
    for i in online_cpus() {
        s += &format!(
            "processor\t: {}\nBogoMIPS\t: {}\nFeatures\t: {}\nCPU implementer\t: 0x{:02x}\nCPU architecture: 8\n\
             CPU variant\t: 0x{:x}\nCPU part\t: 0x{:03x}\nCPU revision\t: {}\n\n",
            i,
            bogomips(),
            feats,
            midr >> 24 & 0xff,
            midr >> 20 & 0xf,
            midr >> 4 & 0xfff,
            midr & 0xf
        );
    }
    // algunos nucleos de fabricante anaden esta linea al final; Linux no (solo si el perfil la da)
    if let Some(h) = crate::feat::hardware() {
        s += &format!("Hardware\t: {}\n", h);
    }
    s
}

fn read_host(p: &str) -> Option<Vec<u8>> {
    let path = std::ffi::CString::new(p).ok()?;
    let fd = host_syscall(257, [AT_FDCWD, path.as_ptr() as u64, O_CLOEXEC, 0, 0, 0]);
    if fd < 0 {
        return None;
    }
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = host_syscall(0, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]);
        if n <= 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
    }
    host_syscall(3, [fd as u64, 0, 0, 0, 0, 0]);
    Some(out)
}

/// `/proc/self/auxv` del guest: las entradas del proceso en su orden, cada una con el valor de `getauxval` del
/// guest (las que el guest no ve se omiten) y AT_HWCAP2 aunque el host no lo de.
pub fn auxv() -> Vec<u8> {
    let host = read_host("/proc/self/auxv").unwrap_or_default();
    let mut out = Vec::with_capacity(host.len() + 16);
    let mut push = |t: u64, v: u64| {
        out.extend_from_slice(&t.to_ne_bytes());
        out.extend_from_slice(&v.to_ne_bytes());
    };
    let mut hwcap2 = false;
    for e in host.chunks_exact(16) {
        let t = u64::from_ne_bytes(e[..8].try_into().unwrap());
        let v = u64::from_ne_bytes(e[8..].try_into().unwrap());
        if t == 0 {
            break;
        }
        if t == 26 {
            hwcap2 = true;
        }
        if let Some(g) = crate::libc_hle::guest_auxv_entry(t, v) {
            push(t, g);
        }
    }
    if !hwcap2 {
        push(26, crate::feat::hwcap2());
    }
    push(0, 0);
    out
}

/// Contenido del archivo emulado.
pub fn content(k: Kind) -> Vec<u8> {
    match k {
        Kind::CpuInfo => cpuinfo().into_bytes(),
        Kind::Auxv => auxv(),
        Kind::Midr(_) => format!("0x{:016x}\n", crate::feat::midr()).into_bytes(),
        Kind::Revidr(_) => format!("0x{:016x}\n", crate::feat::revidr()).into_bytes(),
    }
}

fn write_all(fd: i64, data: &[u8]) -> bool {
    let mut off = 0;
    while off < data.len() {
        let n = host_syscall(1, [fd as u64, data[off..].as_ptr() as u64, (data.len() - off) as u64, 0, 0, 0]);
        if n <= 0 {
            return false;
        }
        off += n as usize;
    }
    true
}

/// Descriptor de solo lectura con `data`: memfd sellado reabierto, o archivo borrado en el directorio privado.
fn readonly_fd(name: &str, data: &[u8], extra: u64) -> i64 {
    let cname = format!("{}\0", name);
    // MFD_CLOEXEC | MFD_ALLOW_SEALING
    let m = host_syscall(319, [cname.as_ptr() as u64, 3, 0, 0, 0, 0]);
    if m >= 0 {
        let mut r = -5;
        if write_all(m, data) {
            // F_ADD_SEALS: SEAL | SHRINK | GROW | WRITE
            host_syscall(72, [m as u64, 1033, 0xf, 0, 0, 0]);
            let p = format!("/proc/self/fd/{}\0", m);
            r = host_syscall(257, [AT_FDCWD, p.as_ptr() as u64, extra, 0, 0, 0]);
        }
        host_syscall(3, [m as u64, 0, 0, 0, 0, 0]);
        if r >= 0 {
            return r;
        }
    }
    let mut dirs: Vec<String> = crate::guestlib::private_dir().into_iter().map(String::from).collect();
    if let Ok(d) = std::env::var("TMPDIR") {
        dirs.push(d);
    }
    dirs.push("/tmp".into());
    let mut err = -5;
    for d in dirs {
        let p = format!("{}/heddle-{}-{}-{}\0", d.trim_end_matches('/'), name, host_syscall(39, [0; 6]), host_syscall(186, [0; 6]));
        // O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600
        let w = host_syscall(257, [AT_FDCWD, p.as_ptr() as u64, 2 | O_CREAT | O_EXCL | O_CLOEXEC, 0o600, 0, 0]);
        if w < 0 {
            err = w;
            continue;
        }
        let ok = write_all(w, data);
        let r = if ok { host_syscall(257, [AT_FDCWD, p.as_ptr() as u64, extra, 0, 0, 0]) } else { -5 };
        host_syscall(263, [AT_FDCWD, p.as_ptr() as u64, 0, 0, 0, 0]);
        host_syscall(3, [w as u64, 0, 0, 0, 0, 0]);
        if r >= 0 {
            return r;
        }
        err = r;
    }
    err
}

/// Abre el archivo emulado con los `flags` del guest (arm64): descriptor o `-errno`. Los archivos son de solo
/// lectura para la app (0444): pedir escritura da EACCES, como en el dispositivo.
pub fn open(k: Kind, flags: u64) -> i64 {
    if flags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
        return -EEXIST;
    }
    if flags & A_DIRECTORY != 0 {
        return -ENOTDIR;
    }
    if flags & O_PATH == 0 && flags & O_ACCMODE != 0 {
        return -EACCES;
    }
    let name = match k {
        Kind::CpuInfo => "cpuinfo",
        Kind::Auxv => "auxv",
        Kind::Midr(_) => "midr_el1",
        Kind::Revidr(_) => "revidr_el1",
    };
    readonly_fd(name, &content(k), flags & (O_CLOEXEC | O_NONBLOCK | O_PATH))
}

/// Para las aperturas del guest (`open*` HLE y `svc openat`): `Some(fd o -errno)` si la ruta es un archivo emulado.
#[inline]
pub fn try_open(dirfd: u64, path: u64, flags: u64) -> Option<i64> {
    classify(dirfd, path).map(|k| open(k, flags))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leer(fd: i64) -> Vec<u8> {
        assert!(fd >= 0, "fd {}", fd);
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = host_syscall(0, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]);
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
        }
        host_syscall(3, [fd as u64, 0, 0, 0, 0, 0]);
        out
    }

    fn c(p: &str) -> std::ffi::CString {
        std::ffi::CString::new(p).unwrap()
    }

    #[test]
    fn rutas_emuladas() {
        let k = |p: &str| classify(AT_FDCWD, c(p).as_ptr() as u64);
        assert_eq!(k("/proc/cpuinfo"), Some(Kind::CpuInfo));
        assert_eq!(k("//proc/./cpuinfo"), Some(Kind::CpuInfo));
        assert_eq!(k("/proc/self/../cpuinfo"), Some(Kind::CpuInfo));
        assert_eq!(k("/proc/self/auxv"), Some(Kind::Auxv));
        assert_eq!(k("/proc/thread-self/auxv"), Some(Kind::Auxv));
        let pid = host_syscall(39, [0; 6]);
        let tid = host_syscall(186, [0; 6]);
        assert_eq!(k(&format!("/proc/{}/auxv", pid)), Some(Kind::Auxv));
        assert_eq!(k(&format!("/proc/self/task/{}/auxv", tid)), Some(Kind::Auxv));
        assert_eq!(k("/proc/1/auxv"), None);
        assert_eq!(k("/proc/cpuinfo2"), None);
        assert_eq!(k("/proc/meminfo"), None);
        let cpu = online_cpus()[0];
        assert_eq!(k(&format!("/sys/devices/system/cpu/cpu{}/regs/identification/midr_el1", cpu)), Some(Kind::Midr(cpu)));
        assert_eq!(k(&format!("/sys/devices/system/cpu/cpu{}/regs/identification/revidr_el1", cpu)), Some(Kind::Revidr(cpu)));
        assert_eq!(k("/sys/devices/system/cpu/cpu99999/regs/identification/midr_el1"), None);
        // relativa a un directorio abierto
        let d = host_syscall(257, [AT_FDCWD, c("/proc").as_ptr() as u64, 0o200000 | O_CLOEXEC, 0, 0, 0]);
        assert!(d >= 0);
        assert_eq!(classify(d as u64, c("cpuinfo").as_ptr() as u64), Some(Kind::CpuInfo));
        assert_eq!(classify(d as u64, c("self/auxv").as_ptr() as u64), Some(Kind::Auxv));
        assert_eq!(classify(d as u64, c("version").as_ptr() as u64), None);
        host_syscall(3, [d as u64, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn cpuinfo_como_linux_arm64() {
        let s = String::from_utf8(leer(try_open(AT_FDCWD, c("/proc/cpuinfo").as_ptr() as u64, O_CLOEXEC).unwrap())).unwrap();
        let cpus = online_cpus();
        assert_eq!(s.matches("processor\t: ").count(), cpus.len());
        let feats = crate::feat::features_line(crate::feat::hwcap(), crate::feat::hwcap2());
        let midr = crate::feat::midr();
        let bloque = format!(
            "BogoMIPS\t: 38.40\nFeatures\t: {}\nCPU implementer\t: 0x{:02x}\nCPU architecture: 8\nCPU variant\t: 0x{:x}\n\
             CPU part\t: 0x{:03x}\nCPU revision\t: {}\n\n",
            feats,
            midr >> 24,
            midr >> 20 & 0xf,
            midr >> 4 & 0xfff,
            midr & 0xf
        );
        assert_eq!(s, cpus.iter().map(|i| format!("processor\t: {}\n{}", i, bloque)).collect::<String>());
        assert!(s.ends_with("\n\n"));
    }

    #[test]
    fn solo_lectura() {
        let p = c("/proc/cpuinfo");
        assert_eq!(try_open(AT_FDCWD, p.as_ptr() as u64, 1), Some(-EACCES));
        assert_eq!(try_open(AT_FDCWD, p.as_ptr() as u64, 2), Some(-EACCES));
        assert_eq!(try_open(AT_FDCWD, p.as_ptr() as u64, A_DIRECTORY), Some(-ENOTDIR));
        let fd = try_open(AT_FDCWD, p.as_ptr() as u64, O_CLOEXEC).unwrap();
        assert!(fd >= 0);
        let n = host_syscall(1, [fd as u64, b"x".as_ptr() as u64, 1, 0, 0, 0]);
        assert_eq!(n, -9, "escribir en un descriptor de solo lectura: EBADF");
        // el desplazamiento es propio: una segunda apertura empieza de cero
        let a = leer(fd);
        let b = leer(try_open(AT_FDCWD, p.as_ptr() as u64, 0).unwrap());
        assert_eq!(a, b);
    }

    #[test]
    fn auxv_y_midr() {
        let a = leer(try_open(AT_FDCWD, c("/proc/self/auxv").as_ptr() as u64, 0).unwrap());
        assert_eq!(a.len() % 16, 0);
        let e: Vec<(u64, u64)> =
            a.chunks_exact(16).map(|e| (u64::from_ne_bytes(e[..8].try_into().unwrap()), u64::from_ne_bytes(e[8..].try_into().unwrap()))).collect();
        assert_eq!(e.last(), Some(&(0, 0)));
        let get = |t: u64| e.iter().find(|x| x.0 == t).map(|x| x.1);
        assert_eq!(get(16), Some(crate::feat::hwcap()));
        assert_eq!(get(26), Some(crate::feat::hwcap2()));
        assert_eq!(get(33), None, "sin vDSO del host");
        let plat = get(15).unwrap();
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(plat as *const std::os::raw::c_char) }.to_bytes(), b"aarch64");
        for &(t, v) in &e[..e.len() - 1] {
            assert_eq!(crate::libc_hle::guest_auxv_entry(t, unsafe { crate::sys::getauxval(t) }), Some(v), "AT {}", t);
        }
        let cpu = online_cpus()[0];
        let m = leer(try_open(AT_FDCWD, c(&format!("/sys/devices/system/cpu/cpu{}/regs/identification/midr_el1", cpu)).as_ptr() as u64, 0).unwrap());
        assert_eq!(m, format!("0x{:016x}\n", crate::feat::midr()).into_bytes());
        assert_eq!(m.len(), 19);
    }
}
