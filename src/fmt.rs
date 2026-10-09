//! Motor de printf para el guest: lee argumentos segun AAPCS64 (registros/pila o va_list) y formatea cada
//! conversion con el snprintf del host (un argumento por llamada), de modo que el resultado coincide con la libc.
//! Las conversiones de long double (`%L[aAeEfFgG]`, binary128) no las puede formatear el host: van al vfprintf de
//! bionic compilado en la biblioteca guest (`ldio::fmt_ld`).

use crate::cpu::Cpu;
use crate::sys::*;

pub trait ArgSrc {
    fn int(&mut self) -> u64;
    fn fp(&mut self) -> u64; // bits de un double
    fn quad(&mut self) -> u128; // long double (binary128)
}

/// Argumentos variadicos pasados en registros x/v y pila (llamada directa a una funcion variadica).
pub struct RegArgs {
    x: [u64; 8],
    v: [[u64; 2]; 8],
    gi: usize,
    gf: usize,
    sp: u64,
}

impl RegArgs {
    pub fn new(c: &Cpu, named_ints: usize) -> RegArgs {
        let mut x = [0; 8];
        x.copy_from_slice(&c.x[0..8]);
        let mut v = [[0; 2]; 8];
        v.copy_from_slice(&c.v[0..8]);
        RegArgs { x, v, gi: named_ints, gf: 0, sp: c.x[31] }
    }
    fn stack(&mut self) -> u64 {
        let p = self.sp as *const u64;
        self.sp += 8;
        unsafe { *p }
    }
}

impl ArgSrc for RegArgs {
    fn int(&mut self) -> u64 {
        if self.gi < 8 {
            self.gi += 1;
            self.x[self.gi - 1]
        } else {
            self.stack()
        }
    }
    fn fp(&mut self) -> u64 {
        if self.gf < 8 {
            self.gf += 1;
            self.v[self.gf - 1][0]
        } else {
            self.stack()
        }
    }
    fn quad(&mut self) -> u128 {
        if self.gf < 8 {
            self.gf += 1;
            let r = self.v[self.gf - 1];
            (r[0] as u128) | ((r[1] as u128) << 64)
        } else {
            self.sp = (self.sp + 15) & !15;
            let p = self.sp as *const u64;
            self.sp += 16;
            unsafe { (*p as u128) | ((*p.add(1) as u128) << 64) }
        }
    }
}

/// va_list AAPCS64: { void *stack; void *gr_top; void *vr_top; int gr_offs; int vr_offs; }
pub struct VaList {
    p: *mut u8,
}

impl VaList {
    pub fn new(p: u64) -> VaList {
        VaList { p: p as *mut u8 }
    }
    unsafe fn stack(&mut self) -> *const u64 {
        let sp = *(self.p as *const u64);
        *(self.p as *mut u64) = sp + 8;
        sp as *const u64
    }
}

impl ArgSrc for VaList {
    fn int(&mut self) -> u64 {
        unsafe {
            let gr_offs = *(self.p.add(24) as *const i32);
            if gr_offs >= 0 {
                return *self.stack();
            }
            let top = *(self.p.add(8) as *const u64);
            *(self.p.add(24) as *mut i32) = gr_offs + 8;
            if gr_offs + 8 > 0 {
                // el registro no cabia: pasa a la pila
                *(self.p.add(24) as *mut i32) = 0;
                return *self.stack();
            }
            *((top as i64 + gr_offs as i64) as *const u64)
        }
    }
    fn fp(&mut self) -> u64 {
        unsafe {
            let vr_offs = *(self.p.add(28) as *const i32);
            if vr_offs >= 0 {
                return *self.stack();
            }
            let top = *(self.p.add(16) as *const u64);
            *(self.p.add(28) as *mut i32) = vr_offs + 16;
            if vr_offs + 16 > 0 {
                *(self.p.add(28) as *mut i32) = 0;
                return *self.stack();
            }
            *((top as i64 + vr_offs as i64) as *const u64)
        }
    }
    fn quad(&mut self) -> u128 {
        unsafe {
            let vr_offs = *(self.p.add(28) as *const i32);
            if vr_offs >= 0 {
                let sp = (*(self.p as *const u64) + 15) & !15;
                *(self.p as *mut u64) = sp + 16;
                let q = sp as *const u64;
                return (*q as u128) | ((*q.add(1) as u128) << 64);
            }
            let top = *(self.p.add(16) as *const u64);
            *(self.p.add(28) as *mut i32) = vr_offs + 16;
            let q = (top as i64 + vr_offs as i64) as *const u64;
            (*q as u128) | ((*q.add(1) as u128) << 64)
        }
    }
}

/// binary128 -> binary64 (redondeo al mas cercano, par). Perdida de precision: solo sin biblioteca guest.
pub fn quad_to_f64(q: u128) -> f64 {
    let sign = (q >> 127) as u64;
    let exp = ((q >> 112) & 0x7fff) as i32;
    let frac = q & ((1u128 << 112) - 1);
    let sbit = sign << 63;
    if exp == 0x7fff {
        return if frac == 0 { f64::from_bits(sbit | 0x7ff0_0000_0000_0000) } else { f64::from_bits(sbit | 0x7ff8_0000_0000_0000 | ((frac >> 60) as u64 & 0x000f_ffff_ffff_ffff)) };
    }
    if exp == 0 && frac == 0 {
        return f64::from_bits(sbit);
    }
    // valor = 1.frac * 2^(exp-16383)  (los subnormales de quad son < 2^-16382: underflow a 0 en double)
    let e = exp - 16383;
    if exp == 0 {
        return f64::from_bits(sbit);
    }
    let m: u128 = (1u128 << 112) | frac; // 113 bits
    let mut be = e + 1023;
    if be >= 0x7ff {
        return f64::from_bits(sbit | 0x7ff0_0000_0000_0000);
    }
    // numero de bits a descartar: 112-52 = 60 (normal); mas si es subnormal en double
    let mut shift = 60;
    if be <= 0 {
        shift += (1 - be) as u32;
        be = 0;
        if shift >= 126 {
            return f64::from_bits(sbit);
        }
    }
    let mut r = (m >> shift) as u64;
    let rem = m & ((1u128 << shift) - 1);
    let half = 1u128 << (shift - 1);
    if rem > half || (rem == half && (r & 1) == 1) {
        r += 1;
    }
    let bits = if be == 0 { r } else { ((be as u64) << 52) + (r - (1u64 << 52)) };
    f64::from_bits(sbit | bits)
}

pub fn f64_to_quad(d: f64) -> u128 {
    let b = d.to_bits();
    let sign = ((b >> 63) as u128) << 127;
    let exp = ((b >> 52) & 0x7ff) as i32;
    let frac = (b & 0x000f_ffff_ffff_ffff) as u128;
    if exp == 0x7ff {
        return sign | (0x7fffu128 << 112) | (frac << 60);
    }
    if exp == 0 {
        if frac == 0 {
            return sign;
        }
        let lz = frac.leading_zeros() as i32 - (128 - 52);
        // frac * 2^-1074 = 1.x * 2^(-1074 + 51 - lz)
        let e = -1074 + (51 - lz);
        let m = (frac << (lz + 1)) & ((1u128 << 52) - 1);
        return sign | (((e + 16383) as u128) << 112) | (m << 60);
    }
    sign | (((exp - 1023 + 16383) as u128) << 112) | (frac << 60)
}

fn fmt1_int(spec: &str, v: i64) -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let c = std::ffi::CString::new(spec).unwrap();
    let n = unsafe { snprintf(buf.as_mut_ptr() as *mut _, buf.len(), c.as_ptr(), v) };
    finish(buf, n)
}
pub(crate) fn fmt1_dbl(spec: &str, v: f64) -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let c = std::ffi::CString::new(spec).unwrap();
    let mut n = unsafe { snprintf(buf.as_mut_ptr() as *mut _, buf.len(), c.as_ptr(), v) };
    if n as usize >= buf.len() {
        buf = vec![0u8; n as usize + 1];
        n = unsafe { snprintf(buf.as_mut_ptr() as *mut _, buf.len(), c.as_ptr(), v) };
    }
    finish(buf, n)
}
fn fmt1_ptr(spec: &str, v: u64) -> Vec<u8> {
    let mut buf = vec![0u8; 512];
    let c = std::ffi::CString::new(spec).unwrap();
    let mut n = unsafe { snprintf(buf.as_mut_ptr() as *mut _, buf.len(), c.as_ptr(), v) };
    if n as usize >= buf.len() {
        buf = vec![0u8; n as usize + 1];
        n = unsafe { snprintf(buf.as_mut_ptr() as *mut _, buf.len(), c.as_ptr(), v) };
    }
    finish(buf, n)
}
fn finish(mut buf: Vec<u8>, n: i32) -> Vec<u8> {
    buf.truncate(n.max(0) as usize);
    buf
}

/// `%n` en la familia printf (también wprintf, las `_chk` y lo que formatea con ellas: `__android_log_print`,
/// `syslog`, `err`/`warn`): bionic lo prohíbe y aborta en vfprintf.cpp/vfwprintf.cpp con
/// `__fortify_fatal("%%n not allowed on Android")`, que es `async_safe_fatal_va_list("FORTIFY", ...)` (el mensaje y
/// un salto de línea en stderr con `writev`, el mensaje en logcat con prioridad FATAL y etiqueta `libc`,
/// `android_set_abort_message`) seguido de `abort()`. Igual aquí, con las funciones del host.
pub fn printf_n_fatal() -> ! {
    const MSG: &str = "FORTIFY: %n not allowed on Android";
    #[repr(C)]
    struct IoVec {
        base: *const u8,
        len: usize,
    }
    extern "C" {
        fn writev(fd: std::os::raw::c_int, iov: *const IoVec, n: std::os::raw::c_int) -> isize;
        fn abort() -> !;
        #[cfg(target_os = "android")]
        fn android_set_abort_message(msg: *const std::os::raw::c_char);
    }
    let iov = [IoVec { base: MSG.as_ptr(), len: MSG.len() }, IoVec { base: b"\n".as_ptr(), len: 1 }];
    // TEMP_FAILURE_RETRY
    while unsafe { writev(2, iov.as_ptr(), 2) } < 0 && errno() == 4 {}
    crate::bridge::alog_libc_fatal(MSG);
    #[cfg(target_os = "android")]
    unsafe {
        android_set_abort_message(b"FORTIFY: %n not allowed on Android\0".as_ptr() as *const _)
    };
    unsafe { abort() }
}

/// Formatea `fmt` (cadena C guest) con los argumentos de `a`. `c`: la `Cpu` de la HLE en curso (las conversiones
/// `%L` ejecutan codigo de la biblioteca guest sobre ella).
pub fn format(c: &mut Cpu, fmt: &[u8], a: &mut dyn ArgSrc) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        let ch = fmt[i];
        if ch != b'%' {
            out.push(ch);
            i += 1;
            continue;
        }
        i += 1;
        if i >= fmt.len() {
            break;
        }
        if fmt[i] == b'%' {
            out.push(b'%');
            i += 1;
            continue;
        }
        // %[flags][width][.prec][length]conv
        let mut spec = String::from("%");
        while i < fmt.len() && b"-+ #0'".contains(&fmt[i]) {
            spec.push(fmt[i] as char);
            i += 1;
        }
        if i < fmt.len() && fmt[i] == b'*' {
            let w = a.int() as i32;
            spec += &w.to_string();
            i += 1;
        } else {
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                spec.push(fmt[i] as char);
                i += 1;
            }
        }
        if i < fmt.len() && fmt[i] == b'.' {
            spec.push('.');
            i += 1;
            if i < fmt.len() && fmt[i] == b'*' {
                let p = a.int() as i32;
                if p >= 0 {
                    spec += &p.to_string();
                } else {
                    spec.pop(); // precision negativa = ausente
                }
                i += 1;
            } else {
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    spec.push(fmt[i] as char);
                    i += 1;
                }
            }
        }
        // longitud
        let mut len = String::new();
        while i < fmt.len() && b"hlLqjzt".contains(&fmt[i]) {
            len.push(fmt[i] as char);
            i += 1;
        }
        if i >= fmt.len() {
            break;
        }
        let conv = fmt[i];
        i += 1;
        match conv {
            b'd' | b'i' => {
                let raw = a.int();
                let v: i64 = match len.as_str() {
                    "hh" => raw as i8 as i64,
                    "h" => raw as i16 as i64,
                    "l" | "ll" | "q" | "j" | "z" | "t" => raw as i64,
                    _ => raw as i32 as i64,
                };
                out.extend(fmt1_int(&format!("{}lld", spec), v));
            }
            b'u' | b'x' | b'X' | b'o' => {
                let raw = a.int();
                let v: u64 = match len.as_str() {
                    "hh" => raw as u8 as u64,
                    "h" => raw as u16 as u64,
                    "l" | "ll" | "q" | "j" | "z" | "t" => raw,
                    _ => raw as u32 as u64,
                };
                out.extend(fmt1_int(&format!("{}ll{}", spec, conv as char), v as i64));
            }
            b'c' => {
                let raw = a.int();
                if len == "l" {
                    // %lc: caracter ancho -> UTF-8 mediante el host
                    out.extend(fmt1_int(&format!("{}lc", spec), raw as i32 as i64));
                } else {
                    out.extend(fmt1_int(&format!("{}c", spec), raw as u8 as i64));
                }
            }
            b's' => {
                let p = a.int();
                if len == "l" {
                    out.extend(fmt1_ptr(&format!("{}ls", spec), p));
                } else if p == 0 {
                    out.extend(fmt1_ptr(&format!("{}s", spec), b"(null)\0".as_ptr() as u64));
                } else {
                    out.extend(fmt1_ptr(&format!("{}s", spec), p));
                }
            }
            b'p' => {
                let p = a.int();
                out.extend(fmt1_ptr(&format!("{}p", spec), p));
            }
            b'n' => printf_n_fatal(),
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                if len == "L" {
                    let q = a.quad();
                    match crate::ldio::fmt_ld(c, &format!("{}L{}", spec, conv as char), q) {
                        Some(s) => out.extend(s),
                        // sin biblioteca guest: aproximacion en double
                        None => out.extend(fmt1_dbl(&format!("{}{}", spec, conv as char), quad_to_f64(q))),
                    }
                } else {
                    out.extend(fmt1_dbl(&format!("{}{}", spec, conv as char), f64::from_bits(a.fp())));
                }
            }
            b'm' => {
                let e = errno();
                let s = unsafe { std::ffi::CStr::from_ptr(strerror(e)) };
                out.extend_from_slice(s.to_bytes());
            }
            other => {
                // conversion desconocida: se copia literal
                out.push(b'%');
                out.extend_from_slice(spec.as_bytes()[1..].iter().as_slice());
                out.push(other);
            }
        }
    }
    out
}

extern "C" {
    fn strerror(e: i32) -> *const std::os::raw::c_char;
}
