//! Biblioteca guest incrustada: codigo ARM64 con lo que NO se puede reenviar al host: long double de 128 bits y
//! numeros complejos de la libm, y la conversion texto <-> long double de la libc (strtold, wcstold y el vfprintf de
//! bionic para las conversiones %L, ver ldio.rs). Se compila con guest/build.sh a partir de la libm y la libc de
//! bionic y exporta exactamente lo que la tabla de firmas marca como ABI incompatible en libm.so, las funciones de
//! libc.so que devuelven long double y los internos `__heddle_*` (formateador de %L y las dos rutas de
//! strtold para las pruebas), que resolve() no entrega.
//!
//! El guest llama a estas funciones como codigo ARM normal (traducido): no cruzan la frontera. Lo que ellas mismas
//! necesitan y si es reenviable (sqrt, exp... en double) lo importan del host por la via habitual.

use std::sync::{Arc, OnceLock};

static BLOB: &[u8] = include_bytes!(env!("HEDDLE_GUEST_BLOB"));

/// Directorio privado de la app (lo entrega ART al inicializar el puente): alternativa si no hay memfd.
static PRIVATE_DIR: OnceLock<String> = OnceLock::new();

pub fn set_private_dir(d: &str) {
    if !d.is_empty() {
        let _ = PRIVATE_DIR.set(d.to_string());
    }
}

/// Directorio privado de la app, si ART lo dio.
pub fn private_dir() -> Option<&'static str> {
    PRIVATE_DIR.get().map(|s| s.as_str())
}

/// Hay biblioteca guest incrustada en este binario?
pub fn available() -> bool {
    !BLOB.is_empty()
}

/// Donde quedo el bloque incrustado para el cargador.
enum Blob {
    /// memfd: se carga por su descriptor (`ANDROID_DLEXT_USE_LIBRARY_FD`). Reabrirlo por `/proc/self/fd/N` falla en
    /// las apps de Android (la politica de SELinux no deja abrir ese enlace; el cargador lo decia como `library
    /// "/proc/self/fd/N" not found`).
    Fd(i32),
    /// archivo en el directorio privado o en el temporal
    Path(String),
}

/// Deja el bloque incrustado donde el cargador pueda mapearlo.
fn materialize() -> Option<Blob> {
    use crate::sys::*;
    // 1) memfd_create (Linux 3.17+): sin tocar el disco
    let fd = unsafe { syscall(319, b"libheddle_guest.so\0".as_ptr(), 0usize) } as i32;
    if fd >= 0 {
        let mut off = 0usize;
        while off < BLOB.len() {
            let n = unsafe { write(fd, BLOB[off..].as_ptr() as *const std::os::raw::c_void, BLOB.len() - off) };
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        if off == BLOB.len() {
            return Some(Blob::Fd(fd));
        }
        unsafe { close(fd) };
    }
    // 2) kernels sin memfd_create (o filtrado): un archivo en el directorio privado de la app o en el temporal
    let mut dirs: Vec<String> = Vec::new();
    if let Some(d) = PRIVATE_DIR.get() {
        dirs.push(d.clone());
    }
    if let Ok(d) = std::env::var("TMPDIR") {
        dirs.push(d);
    }
    dirs.push("/tmp".into());
    for d in dirs {
        let p = format!("{}/libheddle_guest-{}.so", d.trim_end_matches('/'), unsafe { getpid() });
        if std::fs::write(&p, BLOB).is_ok() {
            return Some(Blob::Path(p));
        }
    }
    None
}

fn module() -> Option<&'static Arc<crate::elf::Module>> {
    static M: OnceLock<Option<Arc<crate::elf::Module>>> = OnceLock::new();
    M.get_or_init(|| {
        if BLOB.is_empty() {
            return None;
        }
        let blob = materialize().or_else(|| {
            eprintln!("[heddle] no se pudo preparar la biblioteca guest (ni memfd ni archivo): se usan las aproximaciones");
            None
        })?;
        let r = match blob {
            Blob::Fd(fd) => {
                let ext = crate::elf::ExtInfo { flags: crate::elf::DLEXT_USE_LIBRARY_FD, library_fd: fd, ..Default::default() };
                crate::elf::open_lib("libheddle_guest.so", 0, crate::namespace::anonymous(), None, Some(&ext)).map(|d| match d {
                    crate::elf::Dep::M(m) => Some(m),
                    _ => None,
                })
            }
            Blob::Path(p) => crate::elf::load_library(&p),
        };
        match r {
            Ok(Some(m)) => Some(m),
            Ok(None) => None,
            Err(e) => {
                eprintln!("[heddle] no se pudo cargar la biblioteca guest: {}", e);
                None
            }
        }
    })
    .as_ref()
}

thread_local! {
    /// La propia carga de la biblioteca guest resuelve sus importaciones: no debe volver a entrar aqui.
    static LOADING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Direccion guest de `name` si la biblioteca guest la exporta.
pub fn lookup(name: &str) -> Option<u64> {
    if BLOB.is_empty() || LOADING.with(|l| l.get()) {
        return None;
    }
    LOADING.with(|l| l.set(true));
    let m = module();
    LOADING.with(|l| l.set(false));
    m?.lookup_local(name)
}

#[cfg(test)]
mod tests {
    use crate::libc_hle::resolve;
    use crate::rt::call_guest_q;

    fn q(hi: u64, lo: u64) -> [u64; 2] {
        [lo, hi]
    }

    /// binary128 de un entero pequeno exacto
    fn qi(n: u64) -> [u64; 2] {
        let f = crate::fmt::f64_to_quad(n as f64);
        [f as u64, (f >> 64) as u64]
    }

    fn need() -> bool {
        if !super::available() {
            // En el CI la biblioteca guest se compila antes: ahi su ausencia es un fallo.
            assert!(std::env::var_os("HEDDLE_REQUIRE_GUEST").is_none(), "falta la biblioteca guest incrustada");
            eprintln!("(biblioteca guest no compilada: prueba omitida)");
            return false;
        }
        true
    }

    #[test]
    fn long_double_con_precision_de_128_bits() {
        if !need() {
            return;
        }
        // sqrtl(2): los 113 bits de mantisa, no los 53 de un double
        let f = resolve("sqrtl").unwrap();
        assert!(!crate::hle::is_hle(f), "sqrtl debe ser codigo guest (ARM), no una aproximacion del puente");
        let (_, v0, _) = call_guest_q(f, &[], &[qi(2)]);
        assert_eq!(v0, q(0x3fff_6a09_e667_f3bc, 0xc908_b2fb_1366_ea95), "sqrtl(2) = {:016x}{:016x}", v0[1], v0[0]);
        // una aproximacion en double dejaria a cero los 60 bits bajos
        assert_ne!(v0[0] & 0x0fff_ffff_ffff_ffff, 0);
        // powl(2, 10) = 1024 exacto; fmal(2, 3, 4) = 10
        let (_, p, _) = call_guest_q(resolve("powl").unwrap(), &[], &[qi(2), qi(10)]);
        assert_eq!(p, qi(1024));
        let (_, m, _) = call_guest_q(resolve("fmal").unwrap(), &[], &[qi(2), qi(3), qi(4)]);
        assert_eq!(m, qi(10));
        // long double -> entero
        let (r, _, _) = call_guest_q(resolve("llroundl").unwrap(), &[], &[qi(41)]);
        assert_eq!(r, 41);
    }

    #[test]
    fn complejos_por_la_biblioteca_guest() {
        if !need() {
            return;
        }
        // cexp(i*pi) = -1 + 0i : en AAPCS64 un complejo double viaja en d0 (real) y d1 (imaginaria)
        let f = resolve("cexp").unwrap();
        assert!(!crate::hle::is_hle(f));
        let (_, re, im) = call_guest_q(f, &[], &[[0f64.to_bits(), 0], [std::f64::consts::PI.to_bits(), 0]]);
        assert!((f64::from_bits(re[0]) + 1.0).abs() < 1e-15 && f64::from_bits(im[0]).abs() < 1e-15);
        // cabs(3 + 4i) = 5
        let (_, r, _) = call_guest_q(resolve("cabs").unwrap(), &[], &[[3f64.to_bits(), 0], [4f64.to_bits(), 0]]);
        assert_eq!(f64::from_bits(r[0]), 5.0);
    }

    #[test]
    fn solo_lo_inevitable_va_al_guest() {
        if !need() {
            return;
        }
        // lo reenviable sigue yendo al host (slot HLE), aunque la biblioteca guest lo use internamente
        for n in ["sqrt", "sin", "pow", "exp", "strlen", "malloc"] {
            assert!(crate::hle::is_hle(resolve(n).unwrap()), "{} debe ir al host", n);
        }
        // y la biblioteca guest exporta exactamente lo que la tabla marca como ABI incompatible en libm y, de libc, lo
        // que devuelve long double
        use crate::sigs::{K, SIGS};
        for s in SIGS.iter().filter(|s| s.lib == "libm.so" || s.lib == "libc.so") {
            let en_guest = super::lookup(s.name).is_some();
            let debe = match s.lib {
                "libm.so" => matches!(s.k, K::Unsafe(_)),
                _ => matches!(s.k, K::Unsafe("retorno long double")),
            };
            assert_eq!(en_guest, debe, "{}: {:?}", s.name, s.k);
        }
        // los internos no son API: resolve() no los entrega al guest
        for n in ["__heddle_snprintf_ld", "__heddle_strtold_fast", "__heddle_strtold_slow"] {
            assert!(super::lookup(n).is_some(), "{}", n);
            assert!(resolve(n).is_none(), "{}", n);
        }
    }

    #[test]
    fn strtold_y_printf_l_como_bionic() {
        if !need() {
            return;
        }
        // strtold("0.1") con los 113 bits (resultado de bionic en arm64)
        let f = resolve("strtold").unwrap();
        assert!(!crate::hle::is_hle(f), "strtold debe ser codigo guest");
        let s = b"0.1\0";
        let (_, v0, _) = call_guest_q(f, &[s.as_ptr() as u64, 0], &[]);
        assert_eq!(v0, q(0x3ffb_9999_9999_9999, 0x9999_9999_9999_999a));
        // %L con el vfprintf de bionic de la biblioteca guest
        let c: *mut crate::cpu::Cpu = &mut *crate::rt::cur().cpu;
        let x = (v0[0] as u128) | ((v0[1] as u128) << 64);
        let fmt = |spec: &str| String::from_utf8(crate::ldio::fmt_ld(unsafe { &mut *c }, spec, x).unwrap()).unwrap();
        assert_eq!(fmt("%.40Lf"), "0.1000000000000000000000000000000000048148");
        assert_eq!(fmt("%.35Le"), "1.00000000000000000000000000000000005e-01");
        assert_eq!(fmt("%La"), "0x1.999999999999999999999999999ap-4");
        // salida mas larga que el primer bufer (128): LDBL_MAX entero tiene 4933 cifras
        let max = crate::ldio::fmt_ld(unsafe { &mut *c }, "%.0Lf", (0x7ffe_ffff_ffff_ffffu128 << 64) | 0xffff_ffff_ffff_ffff).unwrap();
        assert_eq!(max.len(), 4933);
        assert!(max.starts_with(b"118973149535723176508575932662800701619646905264169404552969888421216"));
    }

    /// Las rutas rapidas de strtold (guest) y de printf %L (host) dan los mismos bits que la ruta de bionic, en un
    /// barrido aleatorio (`HEDDLE_LD_SWEEP=n` lo agranda) y en casos limite; informa de que fraccion toma la rapida.
    #[test]
    fn rutas_rapidas_de_long_double_como_bionic() {
        if !need() {
            return;
        }
        let n: usize = std::env::var("HEDDLE_LD_SWEEP").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        let mut st = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = move || {
            st ^= st << 13;
            st ^= st >> 7;
            st ^= st << 17;
            st
        };
        // --- strtold: rapida (si la toma) == bionic == strtold publica, valor y final
        let fast = super::lookup("__heddle_strtold_fast").unwrap();
        let slow = super::lookup("__heddle_strtold_slow").unwrap();
        let publ = resolve("strtold").unwrap();
        let edge = [
            "0", "-0", "+0.000e99", "-0.0e5", "1e", "1e+", "1e-x", "5.", ".5", "-.5e1", "00x5", "0x1p3", "1.5.3", "1e0000001",
            "1e000001", "inf", "-nan", " 1.5", "+", "-", ".", "e5", "1234567890123456789012345678901234",
            "12345678901234567890123456789012345", "1234567890123456789012345678901234000000e-6",
            "9999999999999999999999999999999999e14", "9999999999999999999999999999999999e-48", "1e48", "1e49", "1e-48",
            "1e-49", "123e47", "0.000000000000000000000000000000000000000000000001", "3.4028236692093846346337460743176821e38",
            "1.00000000000000000000000000000000005", "100000000000000000000000000000000000000000000000000000", "7e-4950",
            "1.18973149535723176508575932662800702e4932",
        ];
        let mut cats = [("decimal corto", 0usize, 0usize), ("{:e} de un double", 0, 0), ("hasta 40 cifras, 10^-70..10^70", 0, 0), ("limite", 0, 0)];
        let total = n * 3 + edge.len();
        for i in 0..total {
            let (cat, s) = if i >= n * 3 {
                (3, edge[i - n * 3].to_string())
            } else {
                let cat = i % 3;
                let s = match cat {
                    0 => {
                        let a = rnd() % 1_000_000;
                        let b = rnd() % 1_000_000;
                        let e = (rnd() % 61) as i64 - 30;
                        match rnd() % 3 {
                            0 => format!("{}.{}", a, b),
                            1 => format!("{}e{}", a, e),
                            _ => format!("-{}.{:06}e{:+}", a % 1000, b, e),
                        }
                    }
                    1 => {
                        let d = f64::from_bits(rnd() & 0x7fff_ffff_ffff_ffff);
                        if d.is_finite() { format!("{:e}", d) } else { "1".into() }
                    }
                    _ => {
                        let nd = 1 + (rnd() % 40) as usize;
                        let mut d: String = (0..nd).map(|_| (b'0' + (rnd() % 10) as u8) as char).collect();
                        if rnd() % 4 == 0 {
                            d = format!("000{}000", d);
                        }
                        let dot = (rnd() as usize) % (d.len() + 1);
                        d.insert(dot, '.');
                        let e = (rnd() % 141) as i64 - 70;
                        let sign = ["", "-", "+"][(rnd() % 3) as usize];
                        let tail = ["", "x", "e", "e+", "L"][(rnd() % 5) as usize];
                        if d == "." { d = "0.".into(); }
                        format!("{}{}e{}{}", sign, d, e, tail)
                    }
                };
                (cat, s)
            };
            let cs = std::ffi::CString::new(s.clone()).unwrap();
            let p = cs.as_ptr() as u64;
            let (mut e1, mut e2, mut e3) = (0u64, 0u64, 0u64);
            let mut out = [0u64; 2];
            let (ok, _, _) = call_guest_q(fast, &[p, &mut e1 as *mut u64 as u64, out.as_mut_ptr() as u64], &[]);
            let (_, v2, _) = call_guest_q(slow, &[p, &mut e2 as *mut u64 as u64], &[]);
            let (_, v3, _) = call_guest_q(publ, &[p, &mut e3 as *mut u64 as u64], &[]);
            assert_eq!((v3, e3), (v2, e2), "strtold({:?})", s);
            cats[cat].1 += 1;
            if ok as u32 != 0 {
                assert_eq!((out, e1), (v2, e2), "ruta rapida de strtold({:?})", s);
                cats[cat].2 += 1;
            }
        }
        for (name, t, f) in cats {
            eprintln!("strtold, {}: {} de {} por la ruta rapida ({:.1} %)", name, f, t, 100.0 * f as f64 / t as f64);
        }
        // --- printf %L: rapida (si la toma) == vfprintf de bionic
        let c: *mut crate::cpu::Cpu = &mut *crate::rt::cur().cpu;
        let specs = [
            "%Lf", "%.0Lf", "%.1Lf", "%.3Lf", "%.17Lf", "%.30Lf", "%#.0Lf", "%LF", "%Le", "%.0Le", "%.20Le", "%.40LE", "%Lg",
            "%.0Lg", "%.17Lg", "%#Lg", "%.40LG", "%+12.4Lf", "%-30.10Le", "%010.3Lf", "% .5LE", "%La", "%.0La", "%.1La",
            "%.5LA", "%#.0La", "%20La", "%-+24.3La", "%.20La",
        ];
        let mut pc = [("double exacto", 0usize, 0usize), ("binary128 cualquiera", 0, 0)];
        let mut vals: Vec<(usize, u128)> = Vec::new();
        for i in 0..n {
            let d = match i % 4 {
                0 => f64::from_bits(rnd()),
                1 => (rnd() % 100_000) as f64 / [1.0, 2.0, 4.0, 8.0, 1024.0, 3.0][(rnd() % 6) as usize] + 0.5,
                2 => f64::from_bits(rnd() % 0x0010_0000_0000_0000), // subnormales
                _ => ((rnd() % 2_000_001) as f64 - 1e6) * 10f64.powi((rnd() % 41) as i32 - 20),
            };
            vals.push((0, crate::fmt::f64_to_quad(d)));
            // binary128 cualquiera con exponente en +-2^200 (los extremos, ~4900 cifras con %f, son lentos aqui y no
            // aportan: la ruta rapida no los toma)
            let e = (16383 - 200 + rnd() % 401) as u128;
            vals.push((1, (rnd() as u128 >> 63) << 127 | e << 112 | ((rnd() as u128) << 64 | rnd() as u128) & ((1u128 << 112) - 1)));
        }
        let inf = 0x7fffu128 << 112;
        for v in [0, 1u128 << 127, inf, inf | 1u128 << 127, inf | 1 << 111 | 0xabc, inf | 1u128 << 127 | 1 << 111 | 5, inf | 1] {
            vals.push((0, v));
        }
        for (k, v) in vals {
            for sp in specs {
                let f = crate::ldio::fmt_ld_fast(unsafe { &*c }, sp, v);
                let sl = crate::ldio::fmt_ld_slow(unsafe { &mut *c }, sp, v).unwrap();
                pc[k].1 += 1;
                if let Some(f) = f {
                    assert_eq!(String::from_utf8_lossy(&f), String::from_utf8_lossy(&sl), "{} de {:032x}", sp, v);
                    pc[k].2 += 1;
                }
            }
        }
        for (name, t, f) in pc {
            eprintln!("printf %L, {}: {} de {} por la ruta rapida ({:.1} %)", name, f, t, 100.0 * f as f64 / t as f64);
        }
    }
}
