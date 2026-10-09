//! long double (binary128) en stdio: lo unico de printf/scanf que el host no puede hacer.
//!
//! En arm64 `long double` es binary128 y en x86-64 es el formato x87 de 80 bits: el host no puede leer ni escribir
//! uno. La conversion texto <-> long double es la de bionic compilada en la biblioteca guest (`guest/stdio_ld.cpp`,
//! gdtoa y vfprintf de bionic) y se ejecuta como codigo ARM traducido; todo lo demas sigue en el host:
//!
//! - printf (`fmt::format`) formatea cada conversion con el snprintf del host y solo las `%L[aAeEfFgG]` con
//!   `fmt_ld` (el vfprintf de bionic de la biblioteca guest, una conversion por llamada). Sin `%L` no cambia nada.
//! - wprintf y familia: sin `%L` se reenvian al host como antes (las variadicas) o con el formateador ancho de aqui
//!   (`format_w`, las de `va_list`, que el host no puede recibir); con `%L`, ese formateador.
//! - scanf y familia (estrecha y ancha): sin una conversion `%L` de coma flotante que asigne, se reenvian al host como
//!   antes. Con ella, el formato se parte: los tramos sin `%L` los lee el scanf del host (con un `%lln` al final para
//!   saber cuanto consumio, sin espacios del formato y con sus `%n` corregidos) y cada `%L` se lee como el
//!   `vfscanf`/`vfwscanf` de bionic (CT_FLOAT: espacios, `parsefloat` y `strtold`/`wcstold` de la biblioteca guest).
//!   El valor devuelto y los `%n` siguen las reglas de bionic.
//!
//! Sin biblioteca guest incrustada se vuelve a la aproximacion en double.

use crate::cpu::Cpu;
use crate::fmt::{ArgSrc, RegArgs, VaList};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::OnceLock;

extern "C" {
    fn fgetc(f: *mut c_void) -> c_int;
    fn ungetc(c: c_int, f: *mut c_void) -> c_int;
    fn fgetwc(f: *mut c_void) -> u32;
    fn ungetwc(c: u32, f: *mut c_void) -> u32;
    fn fputwc(c: u32, f: *mut c_void) -> u32;
    fn flockfile(f: *mut c_void);
    fn funlockfile(f: *mut c_void);
    fn iswspace(c: u32) -> c_int;
    fn iswalnum(c: u32) -> c_int;
    fn vsscanf(s: *const c_char, fmt: *const c_char, ap: *mut HostVaList) -> c_int;
    fn vfscanf(f: *mut c_void, fmt: *const c_char, ap: *mut HostVaList) -> c_int;
    fn vswscanf(s: *const u32, fmt: *const u32, ap: *mut HostVaList) -> c_int;
    fn vfwscanf(f: *mut c_void, fmt: *const u32, ap: *mut HostVaList) -> c_int;
    fn swprintf(s: *mut u32, n: usize, fmt: *const u32, ...) -> c_int;
    fn fwprintf(f: *mut c_void, fmt: *const u32, ...) -> c_int;
    fn strerror(e: c_int) -> *const c_char;
}

const WEOF: u32 = u32::MAX;
const EINVAL: i32 = 22;
const EILSEQ: i32 = 84;

// ---------------------------------------------------------------------------------------------
// Funciones de la biblioteca guest
// ---------------------------------------------------------------------------------------------

fn guest_fn(cell: &'static OnceLock<Option<u64>>, name: &str) -> Option<u64> {
    *cell.get_or_init(|| crate::guestlib::lookup(name))
}

fn qw(q: u128) -> [u64; 2] {
    [q as u64, (q >> 64) as u64]
}

/// Formatea una conversion `%L[aAeEfFgG]` (`spec`, sin `*`: anchura y precision ya resueltas) con el vfprintf de
/// bionic de la biblioteca guest. None: no hay biblioteca guest.
pub fn fmt_ld(c: &mut Cpu, spec: &str, q: u128) -> Option<Vec<u8>> {
    static F: OnceLock<Option<u64>> = OnceLock::new();
    let f = guest_fn(&F, "__heddle_snprintf_ld")?;
    if let Some(v) = fmt_ld_fast(c, spec, q) {
        return Some(v);
    }
    fmt_ld_guest(c, f, spec, q)
}

/// Ruta rapida de `fmt_ld`: si el binary128 vale exactamente un double, bionic da los mismos caracteres con
/// `__ldtoa`/`__hldtoa` que con `__dtoa`/`__hdtoa` sobre ese double (mismo valor, mismo redondeo decimal; signo,
/// `inf` y `nan` salen de `signflag` igual en los dos), y el double se formatea como cualquier `%f` del guest (con el
/// printf del host). Excepciones, a la ruta de bionic: redondeo distinto del mas cercano (gdtoa usa FLT_ROUNDS),
/// la bandera `'`, `%a` de un subnormal de double (en long double es normal: `0x1.8p-1070`, no `0x0.0...`) y `%a`
/// con precision (al recortar cifras hexadecimales, `dorounding` de hdtoa.c no redondea los empates al par como el
/// printf del host: `%.3La` de 0x1.0dd8p+14 es `0x1.0ddp+14` en bionic).
pub(crate) fn fmt_ld_fast(c: &Cpu, spec: &str, q: u128) -> Option<Vec<u8>> {
    if (c.fpcr >> 22) & 3 != 0 || spec.contains('\'') {
        return None;
    }
    let b = spec.as_bytes();
    let conv = *b.last()?;
    if b.len() < 3 || b[b.len() - 2] != b'L' {
        return None;
    }
    let exp = (q >> 112) & 0x7fff;
    let neg = (q >> 127) != 0;
    let d = if exp == 0x7fff {
        let m = if q & ((1u128 << 112) - 1) == 0 { f64::INFINITY } else { f64::NAN };
        if neg { -m } else { m }
    } else {
        let d = crate::fmt::quad_to_f64(q);
        if crate::fmt::f64_to_quad(d) != q {
            return None;
        }
        if (conv | 0x20) == b'a' && (spec.contains('.') || (d != 0.0 && d.abs() < f64::MIN_POSITIVE)) {
            return None;
        }
        d
    };
    // la NaN del double lleva el signo pedido (f64::NAN es positiva; -NAN la niega)
    let d = if d.is_nan() { f64::from_bits(d.to_bits() & !(1 << 63) | (neg as u64) << 63) } else { d };
    Some(crate::fmt::fmt1_dbl(&format!("{}{}", &spec[..spec.len() - 2], conv as char), d))
}

/// `fmt_ld` sin la ruta rapida (pruebas).
pub(crate) fn fmt_ld_slow(c: &mut Cpu, spec: &str, q: u128) -> Option<Vec<u8>> {
    static F: OnceLock<Option<u64>> = OnceLock::new();
    let f = guest_fn(&F, "__heddle_snprintf_ld")?;
    fmt_ld_guest(c, f, spec, q)
}

fn fmt_ld_guest(c: &mut Cpu, f: u64, spec: &str, q: u128) -> Option<Vec<u8>> {
    let mut sp = spec.as_bytes().to_vec();
    sp.push(0);
    let mut buf = vec![0u8; 128];
    loop {
        let (r, _) = crate::rt::call_guest_on_q(c, f, &[buf.as_mut_ptr() as u64, buf.len() as u64, sp.as_ptr() as u64], &[qw(q)]);
        let n = r as i32;
        if n < 0 {
            return Some(Vec::new());
        }
        if (n as usize) < buf.len() {
            buf.truncate(n as usize);
            return Some(buf);
        }
        buf = vec![0u8; n as usize + 1];
    }
}

/// `strtold` (o `wcstold`) de la biblioteca guest sobre la cadena terminada en NUL `s`. Devuelve el valor y el
/// numero de caracteres consumidos. None: no hay biblioteca guest.
fn guest_strtold(c: &mut Cpu, s: u64, wide: bool) -> Option<(u128, usize)> {
    static S: OnceLock<Option<u64>> = OnceLock::new();
    static W: OnceLock<Option<u64>> = OnceLock::new();
    let f = if wide { guest_fn(&W, "wcstold")? } else { guest_fn(&S, "strtold")? };
    let mut end: u64 = 0;
    let (_, v) = crate::rt::call_guest_on_q(c, f, &[s, &mut end as *mut u64 as u64], &[]);
    let q = (v[0] as u128) | ((v[1] as u128) << 64);
    Some((q, ((end - s) / if wide { 4 } else { 1 }) as usize))
}

// ---------------------------------------------------------------------------------------------
// Deteccion barata de una conversion %L de coma flotante
// ---------------------------------------------------------------------------------------------

/// El formato (estrecho o ancho, hasta el NUL) contiene una `L`? Filtro previo: sin `L` no hay nada que hacer.
fn has_l(p: u64, wide: bool) -> bool {
    if p == 0 {
        return false;
    }
    if wide {
        let mut q = p as *const u32;
        unsafe {
            while *q != 0 {
                if *q == b'L' as u32 {
                    return true;
                }
                q = q.add(1);
            }
        }
        false
    } else {
        unsafe { std::ffi::CStr::from_ptr(p as *const c_char) }.to_bytes().contains(&b'L')
    }
}

fn read_fmt(p: u64, wide: bool) -> Vec<u32> {
    let mut v = Vec::new();
    if p == 0 {
        return v;
    }
    unsafe {
        if wide {
            let mut q = p as *const u32;
            while *q != 0 {
                v.push(*q);
                q = q.add(1);
            }
        } else {
            v.extend(std::ffi::CStr::from_ptr(p as *const c_char).to_bytes().iter().map(|&b| b as u32));
        }
    }
    v
}

fn is_float_conv(ch: u32) -> bool {
    matches!(char::from_u32(ch), Some('a' | 'A' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G'))
}

/// Hay una conversion `%L[aAeEfFgG]` en un formato de printf?
/// El formato ancho (cadena guest terminada en NUL) tiene una conversion `%n`? Recorre las directivas como
/// `printf_has_ld`, sin copiar el formato.
fn wfmt_has_n(p: u64) -> bool {
    if p == 0 {
        return false;
    }
    let at = |i: usize| unsafe { *(p as *const u32).add(i) };
    let mut i = 0;
    while at(i) != 0 {
        if at(i) != b'%' as u32 {
            i += 1;
            continue;
        }
        i += 1;
        while char::from_u32(at(i)).is_some_and(|ch| "-+ #0'*.$123456789hlLqjzt".contains(ch)) {
            i += 1;
        }
        match at(i) {
            0 => break,
            x if x == b'n' as u32 => return true,
            _ => i += 1,
        }
    }
    false
}

fn printf_has_ld(f: &[u32]) -> bool {
    let mut i = 0;
    while i < f.len() {
        if f[i] != b'%' as u32 {
            i += 1;
            continue;
        }
        i += 1;
        let mut l = false;
        while i < f.len() && char::from_u32(f[i]).is_some_and(|ch| "-+ #0'*.$123456789hlLqjzt".contains(ch)) {
            l |= f[i] == b'L' as u32;
            i += 1;
        }
        if i < f.len() {
            if l && is_float_conv(f[i]) {
                return true;
            }
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// scanf: directivas (reglas de vfscanf/vfwscanf de bionic)
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Dk {
    /// espacios del formato (uno o mas seguidos)
    Ws,
    /// caracter literal o `%%`
    Lit,
    /// conversion: consume `args` destinos (0 o 1); `ld`: `%L[aAeEfFgG]` que asigna; `n`: `%n` que asigna
    Conv { args: usize, ld: bool, n: bool, width: usize },
    /// `%` al final del formato (bionic devuelve EOF)
    End,
}

#[derive(Clone, Copy, Debug)]
struct Dir {
    a: usize,
    b: usize,
    k: Dk,
    /// modificadores de longitud (para el tamano de `%n`): hh, h, l, ll/q, j, z, t
    len: [u8; 2],
}

fn is_space(ch: u32, wide: bool) -> bool {
    if wide {
        unsafe { iswspace(ch) != 0 }
    } else {
        matches!(ch, 0x20 | 0x09..=0x0d)
    }
}

fn directives(f: &[u32], wide: bool) -> Vec<Dir> {
    let mut v = Vec::new();
    let mut i = 0;
    let ch = |j: usize| f.get(j).copied().unwrap_or(0);
    while i < f.len() {
        let c = f[i];
        if is_space(c, wide) {
            let mut j = i + 1;
            while j < f.len() && is_space(f[j], wide) {
                j += 1;
            }
            v.push(Dir { a: i, b: j, k: Dk::Ws, len: [0; 2] });
            i = j;
            continue;
        }
        if c != b'%' as u32 {
            v.push(Dir { a: i, b: i + 1, k: Dk::Lit, len: [0; 2] });
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let (mut sup, mut l, mut width) = (false, false, 0usize);
        let mut len = [0u8; 2];
        loop {
            let m = ch(j);
            match char::from_u32(m).unwrap_or('\0') {
                '*' => sup = true,
                'L' => l = true,
                'j' | 'm' | 'q' | 't' | 'z' => len = [m as u8, 0],
                'h' | 'l' => {
                    if ch(j + 1) == m {
                        j += 1;
                        len = [m as u8, m as u8];
                    } else {
                        len = [m as u8, 0];
                    }
                }
                'w' => {
                    if ch(j + 1) == b'f' as u32 {
                        j += 1;
                    }
                    while (b'0' as u32..=b'9' as u32).contains(&ch(j + 1)) {
                        j += 1;
                    }
                }
                d @ '0'..='9' => width = width.saturating_mul(10).saturating_add(d as usize - '0' as usize),
                _ => break,
            }
            j += 1;
        }
        let conv = ch(j);
        if j >= f.len() {
            v.push(Dir { a: i, b: f.len(), k: Dk::End, len });
            break;
        }
        j += 1;
        let k = if conv == b'%' as u32 {
            Dk::Lit
        } else {
            if conv == b'[' as u32 {
                // __sccl: tras '[' y '^' opcional, el primer caracter siempre es del conjunto; luego hasta ']'
                if ch(j) == b'^' as u32 {
                    j += 1;
                }
                if j < f.len() {
                    j += 1;
                }
                while j < f.len() && f[j] != b']' as u32 {
                    j += 1;
                }
                j = (j + 1).min(f.len());
            }
            let n = conv == b'n' as u32;
            Dk::Conv { args: if sup { 0 } else { 1 }, ld: l && !sup && is_float_conv(conv), n: n && !sup, width }
        };
        v.push(Dir { a: i, b: j, k, len });
        i = j;
    }
    v
}

/// Cuantos destinos consume un formato de scanf.
pub fn scanf_nargs(fmt: &[u8]) -> usize {
    let f: Vec<u32> = fmt.iter().map(|&b| b as u32).collect();
    directives(&f, false).iter().map(|d| if let Dk::Conv { args, .. } = d.k { args } else { 0 }).sum()
}

/// va_list x86-64 SysV con todos los argumentos en el area de desbordamiento.
#[repr(C)]
pub struct HostVaList {
    gp_offset: u32,
    fp_offset: u32,
    overflow: *mut u64,
    reg_save: *mut u64,
}

impl HostVaList {
    fn new(args: &mut Vec<u64>) -> HostVaList {
        args.push(0);
        HostVaList { gp_offset: 48, fp_offset: 304, overflow: args.as_mut_ptr(), reg_save: std::ptr::null_mut() }
    }
}

// ---------------------------------------------------------------------------------------------
// scanf: entrada
// ---------------------------------------------------------------------------------------------

enum Input {
    /// cadena guest (sscanf/swscanf) y caracteres ya consumidos
    Str(u64, usize),
    /// FILE* del host
    File(*mut c_void),
}

struct Scan {
    wide: bool,
    inp: Input,
}

impl Scan {
    fn get(&mut self) -> Option<u32> {
        match &mut self.inp {
            Input::Str(p, pos) => {
                let ch = unsafe { if self.wide { *((*p as *const u32).add(*pos)) } else { *((*p as *const u8).add(*pos)) as u32 } };
                if ch == 0 {
                    None
                } else {
                    *pos += 1;
                    Some(ch)
                }
            }
            Input::File(f) => unsafe {
                if self.wide {
                    let c = fgetwc(*f);
                    (c != WEOF).then_some(c)
                } else {
                    let c = fgetc(*f);
                    (c >= 0).then_some(c as u32)
                }
            },
        }
    }
    fn unget(&mut self, ch: u32) {
        match &mut self.inp {
            Input::Str(_, pos) => *pos -= 1,
            Input::File(f) => unsafe {
                if self.wide {
                    ungetwc(ch, *f);
                } else {
                    ungetc(ch as c_int, *f);
                }
            },
        }
    }

    /// El scanf del host sobre lo que queda de la entrada.
    fn host(&mut self, fmt: &[u32], args: &mut Vec<u64>) -> i32 {
        let mut va = HostVaList::new(args);
        unsafe {
            if self.wide {
                let mut w = fmt.to_vec();
                w.push(0);
                match self.inp {
                    Input::Str(p, pos) => vswscanf((p as *const u32).add(pos), w.as_ptr(), &mut va),
                    Input::File(f) => vfwscanf(f, w.as_ptr(), &mut va),
                }
            } else {
                let mut n: Vec<u8> = fmt.iter().map(|&c| c as u8).collect();
                n.push(0);
                match self.inp {
                    Input::Str(p, pos) => vsscanf((p as *const c_char).add(pos), n.as_ptr() as *const c_char, &mut va),
                    Input::File(f) => vfscanf(f, n.as_ptr() as *const c_char, &mut va),
                }
            }
        }
    }

    /// `parsefloat`/`wparsefloat` de bionic (stdio/parsefloat.c) sobre `get`/`unget`: el texto mas largo que es un
    /// numero valido, hasta `width` caracteres; devuelve a la entrada lo leido despues.
    fn parsefloat(&mut self, width: usize) -> Vec<u32> {
        #[derive(PartialEq)]
        enum S {
            Start,
            GotSign,
            Inf,
            Nan,
            MaybeHex,
            Digits,
            Frac,
            Exp,
            ExpDigits,
        }
        let wide = self.wide;
        let isdigit = |c: u32| (b'0' as u32..=b'9' as u32).contains(&c);
        let isxdigit = |c: u32| isdigit(c) || (b'a' as u32..=b'f' as u32).contains(&c) || (b'A' as u32..=b'F' as u32).contains(&c);
        let isalnum = |c: u32| {
            if wide {
                unsafe { iswalnum(c) != 0 }
            } else {
                isdigit(c) || (b'a' as u32..=b'z' as u32).contains(&c) || (b'A' as u32..=b'Z' as u32).contains(&c)
            }
        };
        let is = |c: u32, s: &str| s.bytes().any(|b| b as u32 == c);
        let mut buf: Vec<u32> = Vec::new();
        let mut commit: isize = -1;
        let mut state = S::Start;
        let mut infnanpos: i32 = 0;
        let (mut gotmantdig, mut ishex) = (false, false);
        let mut pending: Option<u32> = None; // caracter leido que no pasa a buf
        while buf.len() < width {
            let Some(c) = self.get() else { break };
            let p = buf.len() as isize;
            // `reswitch`: un estado puede reexaminar el mismo caracter
            let accept = loop {
                match state {
                    S::Start => {
                        state = S::GotSign;
                        if is(c, "-+") {
                            break true;
                        }
                    }
                    S::GotSign => {
                        match char::from_u32(c).unwrap_or('\0') {
                            '0' => {
                                state = S::MaybeHex;
                                commit = p;
                            }
                            'I' | 'i' => state = S::Inf,
                            'N' | 'n' => state = S::Nan,
                            _ => {
                                state = S::Digits;
                                continue;
                            }
                        }
                        break true;
                    }
                    S::Inf => {
                        if infnanpos > 6 || (c != b"nfinity"[infnanpos as usize] as u32 && c != b"NFINITY"[infnanpos as usize] as u32) {
                            break false;
                        }
                        if infnanpos == 1 || infnanpos == 6 {
                            commit = p;
                        }
                        infnanpos += 1;
                        break true;
                    }
                    S::Nan => {
                        match infnanpos {
                            -1 => break false,
                            0 => {
                                if !is(c, "Aa") {
                                    break false;
                                }
                            }
                            1 => {
                                if !is(c, "Nn") {
                                    break false;
                                }
                                commit = p;
                            }
                            2 => {
                                if c != b'(' as u32 {
                                    break false;
                                }
                            }
                            _ => {
                                if c == b')' as u32 {
                                    commit = p;
                                    infnanpos = -2;
                                } else if !isalnum(c) && c != b'_' as u32 {
                                    break false;
                                }
                            }
                        }
                        infnanpos += 1;
                        break true;
                    }
                    S::MaybeHex => {
                        state = S::Digits;
                        if is(c, "Xx") {
                            ishex = true;
                            break true;
                        }
                        gotmantdig = true;
                    }
                    S::Digits => {
                        if (ishex && isxdigit(c)) || isdigit(c) {
                            gotmantdig = true;
                        } else {
                            state = S::Frac;
                            if c != b'.' as u32 {
                                continue;
                            }
                        }
                        if gotmantdig {
                            commit = p;
                        }
                        break true;
                    }
                    S::Frac => {
                        if (is(c, "Ee") && !ishex) || (is(c, "Pp") && ishex) {
                            if !gotmantdig {
                                break false;
                            }
                            state = S::Exp;
                        } else if (ishex && isxdigit(c)) || isdigit(c) {
                            commit = p;
                            gotmantdig = true;
                        } else {
                            break false;
                        }
                        break true;
                    }
                    S::Exp => {
                        state = S::ExpDigits;
                        if is(c, "-+") {
                            break true;
                        }
                    }
                    S::ExpDigits => {
                        if isdigit(c) {
                            commit = p;
                            break true;
                        }
                        break false;
                    }
                }
            };
            if !accept {
                pending = Some(c);
                break;
            }
            buf.push(c);
        }
        // parsedone: lo leido tras el ultimo punto valido vuelve a la entrada, en orden inverso
        if let Some(c) = pending {
            self.unget(c);
        }
        let keep = (commit + 1) as usize;
        while buf.len() > keep {
            let c = buf.pop().unwrap();
            self.unget(c);
        }
        buf
    }
}

/// Escribe `v` en un destino de `%n` con el tamano de sus modificadores (vfscanf de bionic).
fn store_n(p: u64, len: [u8; 2], v: i64) {
    if p == 0 {
        return;
    }
    unsafe {
        match &len {
            b"hh" => *(p as *mut i8) = v as i8,
            [b'h', 0] => *(p as *mut i16) = v as i16,
            [b'l', 0] | [b'z', 0] | [b't', 0] | b"ll" | [b'q', 0] | [b'j', 0] => *(p as *mut i64) = v,
            _ => *(p as *mut i32) = v as i32,
        }
    }
}

/// scanf con conversiones `%L` de coma flotante (ver la cabecera del modulo). `args`: los destinos, en orden.
fn scan_ld(c: &mut Cpu, sc: &mut Scan, f: &[u32], args: &[u64]) -> i32 {
    let wide = sc.wide;
    let dirs = directives(f, wide);
    let (mut nassigned, mut nconv, mut nread) = (0i32, 0usize, 0i64);
    let mut ai = 0usize;
    // tramo pendiente para el host: formato, destinos, `%n` a corregir (indice del destino, puntero guest, tamano)
    let mut seg: Vec<u32> = Vec::new();
    let mut seg_args: Vec<u64> = Vec::new();
    let mut seg_fix: Vec<(usize, u64, [u8; 2])> = Vec::new();
    let mut seg_conv = 0usize;
    // bionic: fallo de entrada -> EOF si aun no hay nada (asignado en vfscanf, convertido en vfwscanf)
    let input_failure = |nassigned: i32, nconv: usize| -> i32 {
        if (wide && nconv == 0) || (!wide && nassigned == 0) {
            -1
        } else {
            nassigned
        }
    };
    let lock = if let Input::File(fp) = sc.inp { Some(fp) } else { None };
    if let Some(fp) = lock {
        unsafe { flockfile(fp) };
    }
    let r = 'run: {
        let mut i = 0;
        while i <= dirs.len() {
            let d = dirs.get(i).copied();
            let host_part = matches!(d.map(|d| d.k), Some(Dk::Lit) | Some(Dk::Conv { ld: false, .. }));
            if let (true, Some(d)) = (host_part, d) {
                if let Dk::Conv { args: na, n, .. } = d.k {
                    seg_conv += 1;
                    if n {
                        seg_fix.push((seg_args.len(), args.get(ai).copied().unwrap_or(0), d.len));
                        seg.extend("%lln".chars().map(|ch| ch as u32));
                        seg_args.push(0);
                        ai += 1;
                        i += 1;
                        continue;
                    }
                    for _ in 0..na {
                        seg_args.push(args.get(ai).copied().unwrap_or(0));
                        ai += 1;
                    }
                }
                seg.extend_from_slice(&f[d.a..d.b]);
                i += 1;
                continue;
            }
            // fin del tramo del host
            if !seg.is_empty() {
                let mut tmp = vec![i64::MIN; seg_fix.len() + 1];
                for (k, (pos, _, _)) in seg_fix.iter().enumerate() {
                    seg_args[*pos] = &mut tmp[k] as *mut i64 as u64;
                }
                seg_args.push(tmp.as_mut_ptr().wrapping_add(seg_fix.len()) as u64);
                seg.extend("%lln".chars().map(|ch| ch as u32));
                let r = sc.host(&seg, &mut seg_args);
                for (k, (_, p, len)) in seg_fix.iter().enumerate() {
                    if tmp[k] != i64::MIN {
                        store_n(*p, *len, tmp[k] + nread);
                    }
                }
                let used = tmp[seg_fix.len()];
                if used == i64::MIN {
                    // el tramo no termino: fallo de coincidencia, o de entrada
                    break 'run if r < 0 { input_failure(nassigned, nconv) } else { nassigned + r };
                }
                nassigned += r;
                nconv += seg_conv;
                nread += used;
                if let Input::Str(_, pos) = &mut sc.inp {
                    *pos += used as usize;
                }
                seg.clear();
                seg_args.clear();
                seg_fix.clear();
                seg_conv = 0;
            }
            let Some(d) = d else { break 'run nassigned };
            match d.k {
                Dk::Ws => {
                    // como bionic: vfscanf cuenta los espacios saltados en %n; vfwscanf no
                    while let Some(ch) = sc.get() {
                        if !is_space(ch, wide) {
                            sc.unget(ch);
                            break;
                        }
                        if !wide {
                            nread += 1;
                        }
                    }
                }
                Dk::End => break 'run -1,
                Dk::Conv { width, .. } => {
                    // CT_FLOAT con LONGDBL: espacios iniciales (al menos un caracter o fallo de entrada)
                    let Some(mut ch) = sc.get() else { break 'run input_failure(nassigned, nconv) };
                    while is_space(ch, wide) {
                        nread += 1;
                        match sc.get() {
                            Some(x) => ch = x,
                            None => break 'run input_failure(nassigned, nconv),
                        }
                    }
                    sc.unget(ch);
                    let width = if width == 0 || width > 512 { 512 } else { width };
                    let text = sc.parsefloat(width);
                    if text.is_empty() {
                        break 'run nassigned;
                    }
                    let dst = args.get(ai).copied().unwrap_or(0);
                    ai += 1;
                    let got = if wide {
                        let mut w = text.clone();
                        w.push(0);
                        guest_strtold(c, w.as_ptr() as u64, true)
                    } else {
                        let mut n: Vec<u8> = text.iter().map(|&ch| ch as u8).collect();
                        n.push(0);
                        guest_strtold(c, n.as_ptr() as u64, false)
                    };
                    let Some((q, used)) = got else { break 'run nassigned };
                    if used != text.len() {
                        // bionic: abort() si strtold no consume exactamente lo que parsefloat acepto
                        crate::bridge::alog_fatal("FALLO scanf %L: strtold no consume el texto de parsefloat");
                        unsafe { crate::sys::abort() };
                    }
                    if dst != 0 {
                        unsafe { std::ptr::write_unaligned(dst as *mut [u64; 2], qw(q)) };
                    }
                    nassigned += 1;
                    nconv += 1;
                    nread += text.len() as i64;
                }
                Dk::Lit => {}
            }
            i += 1;
        }
        nassigned
    };
    if let Some(fp) = lock {
        unsafe { funlockfile(fp) };
    }
    r
}

fn host_stdin() -> *mut c_void {
    let p = crate::boundary::symbol_addr("stdin");
    if p == 0 {
        std::ptr::null_mut()
    } else {
        unsafe { *(p as *const *mut c_void) }
    }
}

/// Variante de la familia scanf.
#[derive(Clone, Copy)]
pub struct ScanKind {
    /// nombre de la funcion (reenvio al host de la variadica sin `%L`)
    pub name: &'static str,
    pub wide: bool,
    /// 's' cadena (x0), 'f' FILE* (x0), 'i' stdin
    pub src: char,
    pub va: bool,
}

/// Funcion del host para el reenvio sin `%L` (se busca una vez por variante).
pub type HostCache = OnceLock<Option<crate::boundary::HostFn>>;

fn forward_cached(c: &mut Cpu, name: &str, host: &HostCache) {
    match *host.get_or_init(|| crate::boundary::HostFn::symbol(name)) {
        Some(h) => crate::libc_hle::forward(c, h, None),
        None => c.x[0] = u64::MAX,
    }
}

/// HLE de sscanf/fscanf/scanf, sus `v*` y sus versiones anchas.
pub fn scanf(c: &mut Cpu, k: ScanKind, host: &HostCache) {
    let lead = if k.src == 'i' { 0 } else { 1 };
    let fmtp = c.x[lead];
    let slow = guest_available() && has_l(fmtp, k.wide);
    let f = if slow || k.va { read_fmt(fmtp, k.wide) } else { Vec::new() };
    let dirs = if slow || k.va { directives(&f, k.wide) } else { Vec::new() };
    let slow = slow && dirs.iter().any(|d| matches!(d.k, Dk::Conv { ld: true, .. }));
    if !slow && !k.va {
        // sin %L: como siempre, el scanf del host con los argumentos tal cual
        forward_cached(c, k.name, host);
        return;
    }
    if fmtp == 0 || (k.va && c.x[lead + 1] == 0) {
        c.x[0] = u64::MAX;
        return;
    }
    let nargs: usize = dirs.iter().map(|d| if let Dk::Conv { args, .. } = d.k { args } else { 0 }).sum();
    let mut args: Vec<u64> = Vec::with_capacity(nargs + 1);
    if k.va {
        let mut a = VaList::new(c.x[lead + 1]);
        args.extend((0..nargs).map(|_| a.int()));
    } else {
        let mut a = RegArgs::new(c, lead + 1);
        args.extend((0..nargs).map(|_| a.int()));
    }
    let inp = match k.src {
        's' => Input::Str(c.x[0], 0),
        'f' => Input::File(c.x[0] as *mut c_void),
        _ => Input::File(host_stdin()),
    };
    if matches!(inp, Input::Str(0, _)) || matches!(inp, Input::File(p) if p.is_null()) {
        c.x[0] = u64::MAX;
        return;
    }
    let mut sc = Scan { wide: k.wide, inp };
    let r = if slow {
        scan_ld(c, &mut sc, &f, &args)
    } else {
        // va_list sin %L: el v*scanf del host con un va_list SysV construido con los destinos
        sc.host(&f, &mut args)
    };
    c.x[0] = r as i64 as u64;
}

fn guest_available() -> bool {
    crate::guestlib::available()
}

// ---------------------------------------------------------------------------------------------
// wprintf y familia
// ---------------------------------------------------------------------------------------------

/// Una conversion con el swprintf del host. Err: error de codificacion o salida desmesurada.
fn fmt1_w(spec: &str, arg: W1) -> Result<Vec<u32>, ()> {
    let mut wspec: Vec<u32> = spec.chars().map(|ch| ch as u32).collect();
    wspec.push(0);
    let mut buf = vec![0u32; 256];
    loop {
        crate::libc_hle::set_errno(0);
        let n = unsafe {
            match arg {
                W1::I(v) => swprintf(buf.as_mut_ptr(), buf.len(), wspec.as_ptr(), v),
                W1::D(v) => swprintf(buf.as_mut_ptr(), buf.len(), wspec.as_ptr(), v),
            }
        };
        if n >= 0 {
            buf.truncate(n as usize);
            return Ok(buf);
        }
        if crate::sys::errno() == EILSEQ || buf.len() >= 1 << 22 {
            return Err(());
        }
        let l = buf.len() * 4;
        buf = vec![0u32; l];
    }
}

#[derive(Clone, Copy)]
enum W1 {
    I(i64),
    D(f64),
}

/// Formateador de wprintf: como `fmt::format`, sobre caracteres anchos (swprintf del host por conversion; `%L`
/// con la biblioteca guest). Err: el wprintf de bionic devolveria -1.
fn format_w(c: &mut Cpu, f: &[u32], a: &mut dyn ArgSrc) -> Result<Vec<u32>, ()> {
    // fmt1_w usa errno para distinguir EILSEQ: se repone el del guest al terminar
    let saved = crate::sys::errno();
    let r = format_w_in(c, f, a, saved);
    if r.is_ok() {
        crate::libc_hle::set_errno(saved);
    }
    r
}

fn format_w_in(c: &mut Cpu, f: &[u32], a: &mut dyn ArgSrc, errno0: i32) -> Result<Vec<u32>, ()> {
    let mut out: Vec<u32> = Vec::new();
    let mut i = 0;
    let chr = |j: usize| f.get(j).and_then(|&x| char::from_u32(x)).unwrap_or('\0');
    while i < f.len() {
        if f[i] != b'%' as u32 {
            out.push(f[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= f.len() {
            break;
        }
        if chr(i) == '%' {
            out.push(b'%' as u32);
            i += 1;
            continue;
        }
        let mut spec = String::from("%");
        while i < f.len() && "-+ #0'".contains(chr(i)) {
            spec.push(chr(i));
            i += 1;
        }
        if chr(i) == '*' {
            spec += &(a.int() as i32).to_string();
            i += 1;
        } else {
            while chr(i).is_ascii_digit() {
                spec.push(chr(i));
                i += 1;
            }
        }
        if chr(i) == '.' {
            spec.push('.');
            i += 1;
            if chr(i) == '*' {
                let p = a.int() as i32;
                if p >= 0 {
                    spec += &p.to_string();
                } else {
                    spec.pop();
                }
                i += 1;
            } else {
                while chr(i).is_ascii_digit() {
                    spec.push(chr(i));
                    i += 1;
                }
            }
        }
        let mut len = String::new();
        while "hlLqjzt".contains(chr(i)) && i < f.len() {
            len.push(chr(i));
            i += 1;
        }
        if i >= f.len() {
            break;
        }
        let conv = chr(i);
        i += 1;
        match conv {
            'd' | 'i' => {
                let raw = a.int();
                let v: i64 = match len.as_str() {
                    "hh" => raw as i8 as i64,
                    "h" => raw as i16 as i64,
                    "l" | "ll" | "q" | "j" | "z" | "t" => raw as i64,
                    _ => raw as i32 as i64,
                };
                out.extend(fmt1_w(&format!("{}lld", spec), W1::I(v))?);
            }
            'u' | 'x' | 'X' | 'o' => {
                let raw = a.int();
                let v: u64 = match len.as_str() {
                    "hh" => raw as u8 as u64,
                    "h" => raw as u16 as u64,
                    "l" | "ll" | "q" | "j" | "z" | "t" => raw,
                    _ => raw as u32 as u64,
                };
                out.extend(fmt1_w(&format!("{}ll{}", spec, conv), W1::I(v as i64))?);
            }
            'c' | 'C' => {
                let raw = a.int();
                if len == "l" || conv == 'C' {
                    out.extend(fmt1_w(&format!("{}lc", spec), W1::I(raw as u32 as i64))?);
                } else {
                    out.extend(fmt1_w(&format!("{}c", spec), W1::I(raw as i32 as i64))?);
                }
            }
            's' | 'S' => {
                let p = a.int();
                let wide_s = len == "l" || conv == 'S';
                let p = if p == 0 { if wide_s { NULL_W.as_ptr() as u64 } else { b"(null)\0".as_ptr() as u64 } } else { p };
                out.extend(fmt1_w(&format!("{}{}", spec, if wide_s { "ls" } else { "s" }), W1::I(p as i64))?);
            }
            'p' => {
                let p = a.int();
                out.extend(fmt1_w(&format!("{}p", spec), W1::I(p as i64))?);
            }
            'n' => crate::fmt::printf_n_fatal(),
            'f' | 'F' | 'e' | 'E' | 'g' | 'G' | 'a' | 'A' => {
                if len == "L" {
                    let q = a.quad();
                    match fmt_ld(c, &format!("{}L{}", spec, conv), q) {
                        // la salida de una conversion de coma flotante es ASCII: ancha = estrecha
                        Some(s) => out.extend(s.iter().map(|&b| b as u32)),
                        None => out.extend(fmt1_w(&format!("{}{}", spec, conv), W1::D(crate::fmt::quad_to_f64(q)))?),
                    }
                } else {
                    out.extend(fmt1_w(&format!("{}{}", spec, conv), W1::D(f64::from_bits(a.fp())))?);
                }
            }
            'm' => {
                let s = unsafe { std::ffi::CStr::from_ptr(strerror(errno0)) };
                out.extend(s.to_bytes().iter().map(|&b| b as u32));
            }
            other => {
                out.push(b'%' as u32);
                out.extend(spec[1..].chars().map(|ch| ch as u32));
                out.push(other as u32);
            }
        }
    }
    Ok(out)
}

static NULL_W: [u32; 7] = [b'(' as u32, b'n' as u32, b'u' as u32, b'l' as u32, b'l' as u32, b')' as u32, 0];

/// Variante de la familia wprintf.
#[derive(Clone, Copy)]
pub struct WPrintKind {
    pub name: &'static str,
    /// 'o' stdout, 'f' FILE* (x0), 's' cadena (x0, tamano x1)
    pub dst: char,
    pub va: bool,
}

fn host_stdout() -> *mut c_void {
    let p = crate::boundary::symbol_addr("stdout");
    if p == 0 {
        std::ptr::null_mut()
    } else {
        unsafe { *(p as *const *mut c_void) }
    }
}

/// HLE de wprintf/fwprintf/swprintf y sus `v*`.
pub fn wprintf(c: &mut Cpu, k: WPrintKind, host: &HostCache) {
    let named = match k.dst {
        'o' => 1,
        'f' => 2,
        _ => 3,
    };
    let fmtp = c.x[named - 1];
    if wfmt_has_n(fmtp) {
        // vfwprintf de bionic: __fortify_fatal (tambien cuando el host formatea: glibc escribiria)
        crate::fmt::printf_n_fatal();
    }
    if !k.va && !(guest_available() && has_l(fmtp, true) && printf_has_ld(&read_fmt(fmtp, true))) {
        // sin %L: como siempre, el wprintf del host con los argumentos tal cual
        forward_cached(c, k.name, host);
        return;
    }
    let f = read_fmt(fmtp, true);
    let (s, n, fp) = (c.x[0], c.x[1], c.x[0] as *mut c_void);
    if k.dst == 's' && n == 0 {
        // vswprintf de bionic: n == 0 -> EINVAL antes de formatear
        crate::libc_hle::set_errno(EINVAL);
        c.x[0] = u64::MAX;
        return;
    }
    let out = if k.va {
        let mut a = VaList::new(c.x[named]);
        format_w(c, &f, &mut a)
    } else {
        let mut a = RegArgs::new(c, named);
        format_w(c, &f, &mut a)
    };
    let Ok(mut out) = out else {
        crate::libc_hle::set_errno(EILSEQ);
        c.x[0] = u64::MAX;
        return;
    };
    let total = out.len();
    let pct_ls: [u32; 4] = [b'%' as u32, b'l' as u32, b's' as u32, 0];
    let r = unsafe {
        match k.dst {
            // vswprintf de bionic: la salida pasa a multibyte y vuelve con mbsrtowcs (hasta un NUL); n caracteres
            // o mas -> EOVERFLOW y -1. Lo mismo hace el swprintf del host con "%ls".
            's' => {
                out.push(0);
                let r = swprintf(s as *mut u32, n as usize, pct_ls.as_ptr(), out.as_ptr());
                if r < 0 {
                    // bionic deja los n - 1 primeros caracteres y el NUL (mbsrtowcs y s[n - 1] = 0); glibc < 2.37
                    // no termina la cadena al desbordar
                    if n > 0 && out.len() >= n as usize {
                        std::ptr::copy_nonoverlapping(out.as_ptr(), s as *mut u32, n as usize - 1);
                        *(s as *mut u32).add(n as usize - 1) = 0;
                    }
                    -1
                } else {
                    total as i64
                }
            }
            _ => {
                let fp = if k.dst == 'o' { host_stdout() } else { fp };
                // cada caracter va al FILE (fputwc en bionic), tambien los NUL
                let mut ok = true;
                for (j, part) in out.split(|&ch| ch == 0).enumerate() {
                    if j > 0 && fputwc(0, fp) == WEOF {
                        ok = false;
                        break;
                    }
                    if !part.is_empty() {
                        let mut z = part.to_vec();
                        z.push(0);
                        if fwprintf(fp, pct_ls.as_ptr(), z.as_ptr()) < 0 {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    total as i64
                } else {
                    -1
                }
            }
        }
    };
    c.x[0] = r as u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<u32> {
        s.chars().map(|c| c as u32).collect()
    }

    #[test]
    fn directivas_de_scanf_como_bionic() {
        let d = directives(&w("%d %Lf%*Lg%n%5Lf x%%%[]a]%lln%"), false);
        let k: Vec<Dk> = d.iter().map(|d| d.k).collect();
        use Dk::*;
        assert_eq!(
            k,
            vec![
                Conv { args: 1, ld: false, n: false, width: 0 },
                Ws,
                Conv { args: 1, ld: true, n: false, width: 0 },
                Conv { args: 0, ld: false, n: false, width: 0 },
                Conv { args: 1, ld: false, n: true, width: 0 },
                Conv { args: 1, ld: true, n: false, width: 5 },
                Ws,
                Lit,
                Lit,
                Conv { args: 1, ld: false, n: false, width: 0 },
                Conv { args: 1, ld: false, n: true, width: 0 },
                End
            ]
        );
        assert_eq!(d[10].len, *b"ll");
        assert!(printf_has_ld(&w("x %-+12.3Lf")) && printf_has_ld(&w("%*.*La")) && !printf_has_ld(&w("%Ld %lf L")));
    }
}
