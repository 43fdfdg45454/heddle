//! Aritmetica de punto flotante con la semantica de AArch64 (FPCR/FPSR, NaN, redondeo).
//!
//! Las operaciones +,-,*,/,sqrt,fma se ejecutan con las instrucciones SSE del host con MXCSR
//! configurado desde el FPCR guest (modo de redondeo, FZ). Los NaN, los casos invalidos, las
//! conversiones y el redondeo a media precision se resuelven por software (bit a bit).

use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
use std::hint::black_box as bb;

pub const IOC: u64 = 1;
pub const DZC: u64 = 2;
pub const OFC: u64 = 4;
pub const UFC: u64 = 8;
pub const IXC: u64 = 16;
pub const IDC: u64 = 0x80;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ft {
    H,
    S,
    D,
}

impl Ft {
    pub fn bits(self) -> u32 {
        match self {
            Ft::H => 16,
            Ft::S => 32,
            Ft::D => 64,
        }
    }
    pub fn frac_bits(self) -> u32 {
        match self {
            Ft::H => 10,
            Ft::S => 23,
            Ft::D => 52,
        }
    }
    pub fn exp_bits(self) -> u32 {
        match self {
            Ft::H => 5,
            Ft::S => 8,
            Ft::D => 11,
        }
    }
    pub fn bias(self) -> i32 {
        match self {
            Ft::H => 15,
            Ft::S => 127,
            Ft::D => 1023,
        }
    }
    pub fn mask(self) -> u64 {
        match self {
            Ft::H => 0xFFFF,
            Ft::S => 0xFFFF_FFFF,
            Ft::D => u64::MAX,
        }
    }
    pub fn sign_bit(self) -> u64 {
        1u64 << (self.bits() - 1)
    }
}

/// Entorno de una instruccion FP: FPCR de entrada y flags de excepcion acumuladas.
pub struct Env {
    pub fpcr: u64,
    pub flags: u64,
    old_csr: u32,
}

impl Env {
    /// Prepara MXCSR segun FPCR (redondeo, FZ) con todas las excepciones enmascaradas.
    pub fn enter(fpcr: u64) -> Env {
        let rc = match (fpcr >> 22) & 3 {
            0 => 0u32,
            1 => 2, // hacia +inf
            2 => 1, // hacia -inf
            _ => 3,
        };
        let mut csr = 0x1F80 | (rc << 13);
        if fpcr & (1 << 24) != 0 {
            csr |= 0x8000 | 0x0040; // FTZ + DAZ
        }
        let old = unsafe { _mm_getcsr() };
        unsafe { _mm_setcsr(csr) };
        Env { fpcr, flags: 0, old_csr: old }
    }
    /// Entorno sin tocar MXCSR (para rutas puramente por software).
    pub fn enter_soft(fpcr: u64) -> Env {
        Env { fpcr, flags: 0, old_csr: 0 }
    }
    /// Recoge las flags del host y restaura MXCSR. Devuelve las flags FPSR acumuladas.
    pub fn leave(mut self) -> u64 {
        self.drain(0x3F);
        unsafe { _mm_setcsr(self.old_csr) };
        self.flags
    }
    /// Pasa las flags de MXCSR (filtradas por `keep`) a FPSR y las limpia.
    pub fn drain(&mut self, keep: u32) {
        let m = unsafe { _mm_getcsr() };
        let f = m & 0x3F;
        if f & keep & 1 != 0 {
            self.flags |= IOC;
        }
        if f & keep & 4 != 0 {
            self.flags |= DZC;
        }
        if f & keep & 8 != 0 {
            self.flags |= OFC;
        }
        if f & keep & 16 != 0 {
            self.flags |= UFC;
        }
        if f & keep & 32 != 0 && !(self.fz() && f & 16 != 0) {
            self.flags |= IXC;
        }
        if f & keep & 16 != 0 && self.fz() {
            // (UFC ya anotado arriba)
        }
        if f & keep & 2 != 0 && self.fpcr & (1 << 24) != 0 {
            self.flags |= IDC;
        }
        unsafe { _mm_setcsr(m & !0x3F) };
    }
    pub fn dn(&self) -> bool {
        self.fpcr & (1 << 25) != 0
    }
    pub fn rmode(&self) -> u8 {
        ((self.fpcr >> 22) & 3) as u8
    }
    pub fn fz(&self) -> bool {
        self.fpcr & (1 << 24) != 0
    }
    pub fn fz16(&self) -> bool {
        self.fpcr & (1 << 19) != 0
    }
}

// ------------------------------------------------------------------------------------------
// Clasificacion y NaN
// ------------------------------------------------------------------------------------------

pub fn is_nan(ft: Ft, x: u64) -> bool {
    let fb = ft.frac_bits();
    let e = (x >> fb) & ((1 << ft.exp_bits()) - 1);
    e == (1 << ft.exp_bits()) - 1 && (x & ((1u64 << fb) - 1)) != 0
}
pub fn is_snan(ft: Ft, x: u64) -> bool {
    is_nan(ft, x) && (x >> (ft.frac_bits() - 1)) & 1 == 0
}
pub fn is_inf(ft: Ft, x: u64) -> bool {
    let fb = ft.frac_bits();
    let e = (x >> fb) & ((1 << ft.exp_bits()) - 1);
    e == (1 << ft.exp_bits()) - 1 && (x & ((1u64 << fb) - 1)) == 0
}
pub fn is_zero(ft: Ft, x: u64) -> bool {
    x & (ft.mask() >> 1) == 0
}
pub fn is_denorm(ft: Ft, x: u64) -> bool {
    let fb = ft.frac_bits();
    let e = (x >> fb) & ((1 << ft.exp_bits()) - 1);
    e == 0 && (x & ((1u64 << fb) - 1)) != 0
}
pub fn quiet(ft: Ft, x: u64) -> u64 {
    x | (1u64 << (ft.frac_bits() - 1))
}
pub fn default_nan(ft: Ft) -> u64 {
    // NaN silencioso positivo, payload 0
    (((1u64 << ft.exp_bits()) - 1) << ft.frac_bits()) | (1u64 << (ft.frac_bits() - 1))
}
pub fn inf(ft: Ft, neg: bool) -> u64 {
    (((1u64 << ft.exp_bits()) - 1) << ft.frac_bits()) | if neg { ft.sign_bit() } else { 0 }
}
pub fn max_norm(ft: Ft, neg: bool) -> u64 {
    ((((1u64 << ft.exp_bits()) - 2) << ft.frac_bits()) | ((1u64 << ft.frac_bits()) - 1)) | if neg { ft.sign_bit() } else { 0 }
}

/// FPProcessNaN: devuelve el NaN resultante (silenciado, o NaN por defecto si DN) y marca IOC si era SNaN.
pub fn process_nan(env: &mut Env, ft: Ft, x: u64) -> u64 {
    if is_snan(ft, x) {
        env.flags |= IOC;
    }
    if env.dn() {
        default_nan(ft)
    } else {
        quiet(ft, x)
    }
}

/// FPProcessNaNs para 2 operandos: Some(resultado) si alguno es NaN.
pub fn nan2(env: &mut Env, ft: Ft, a: u64, b: u64) -> Option<u64> {
    let (sa, sb) = (is_snan(ft, a), is_snan(ft, b));
    let (na, nb) = (is_nan(ft, a), is_nan(ft, b));
    if sa {
        Some(process_nan(env, ft, a))
    } else if sb {
        Some(process_nan(env, ft, b))
    } else if na {
        Some(process_nan(env, ft, a))
    } else if nb {
        Some(process_nan(env, ft, b))
    } else {
        None
    }
}

/// FPProcessNaNs3
pub fn nan3(env: &mut Env, ft: Ft, a: u64, b: u64, c: u64) -> Option<u64> {
    let v = [a, b, c];
    for &x in &v {
        if is_snan(ft, x) {
            return Some(process_nan(env, ft, x));
        }
    }
    for &x in &v {
        if is_nan(ft, x) {
            return Some(process_nan(env, ft, x));
        }
    }
    None
}

/// Resultado invalido: NaN por defecto + IOC
pub fn invalid(env: &mut Env, ft: Ft) -> u64 {
    env.flags |= IOC;
    default_nan(ft)
}

// ------------------------------------------------------------------------------------------
// Half <-> otros, redondeo generico
// ------------------------------------------------------------------------------------------

pub fn h2f64(h: u64) -> f64 {
    let s = (h >> 15) & 1;
    let e = ((h >> 10) & 0x1F) as i32;
    let m = (h & 0x3FF) as f64;
    let v = if e == 0 {
        m * 2f64.powi(-24)
    } else if e == 31 {
        if m == 0.0 {
            f64::INFINITY
        } else {
            f64::NAN
        }
    } else {
        (1.0 + m / 1024.0) * 2f64.powi(e - 15)
    };
    if s == 1 {
        -v
    } else {
        v
    }
}

pub fn to_f64(ft: Ft, x: u64) -> f64 {
    match ft {
        Ft::H => h2f64(x),
        Ft::S => f32::from_bits(x as u32) as f64,
        Ft::D => f64::from_bits(x),
    }
}

/// Redondea y empaqueta (sign, m, exp, sticky): valor = m/2^63 * 2^exp con m normalizado (bit 63 = 1).
pub fn pack(env: &mut Env, ft: Ft, sign: bool, m: u64, exp: i32, sticky: bool) -> u64 {
    let p = ft.frac_bits() + 1; // bits de mantisa con el implicito
    let emax_b = (1i32 << ft.exp_bits()) - 1;
    let mut be = exp + ft.bias();
    let mut shift = 64 - p;
    let tiny = be < 1;
    if tiny {
        shift += (1 - be) as u32;
    }
    let sgn = if sign { ft.sign_bit() } else { 0 };
    // FZ: los resultados subnormales se descartan (se tratan como cero) con UFC
    let fz = match ft {
        Ft::H => env.fz16(),
        _ => env.fz(),
    };
    if tiny && fz {
        env.flags |= UFC;
        return sgn;
    }
    let (mut top, rem, half): (u64, u64, u64);
    if shift > 64 {
        top = 0;
        rem = 1;
        half = 2; // nunca llega a la mitad
    } else if shift == 64 {
        top = 0;
        rem = m;
        half = 1u64 << 63;
    } else {
        top = m >> shift;
        rem = m & ((1u64 << shift) - 1);
        half = 1u64 << (shift - 1);
    }
    let inexact = rem != 0 || sticky;
    let gt = rem > half || (rem == half && sticky);
    let eq = rem == half && !sticky;
    let up = match env.rmode() {
        0 => gt || (eq && top & 1 == 1),
        1 => !sign && inexact,
        2 => sign && inexact,
        _ => false,
    };
    if up {
        top += 1;
    }
    if tiny {
        if inexact {
            env.flags |= UFC | IXC;
        }
        return sgn | top; // si top llega a 1<<(p-1) pasa a ser el minimo normal
    }
    if top == 1u64 << p {
        top >>= 1;
        be += 1;
    }
    if be >= emax_b {
        env.flags |= OFC | IXC;
        let to_inf = match env.rmode() {
            0 => true,
            1 => !sign,
            2 => sign,
            _ => false,
        };
        return if to_inf { inf(ft, sign) } else { max_norm(ft, sign) };
    }
    if inexact {
        env.flags |= IXC;
    }
    sgn | ((be as u64) << ft.frac_bits()) | (top & ((1u64 << ft.frac_bits()) - 1))
}

/// Descompone un numero finito no cero en (sign, m normalizado, exp). Los subnormales se normalizan.
pub fn unpack(ft: Ft, x: u64) -> (bool, u64, i32) {
    let fb = ft.frac_bits();
    let sign = x & ft.sign_bit() != 0;
    let e = ((x >> fb) & ((1 << ft.exp_bits()) - 1)) as i32;
    let f = x & ((1u64 << fb) - 1);
    let (mant, ex) = if e == 0 { (f, 1 - ft.bias()) } else { (f | (1u64 << fb), e - ft.bias()) };
    let lz = mant.leading_zeros() as i32;
    let m = mant << lz;
    // valor = mant * 2^(ex - fb) = m * 2^(ex - fb - lz) = (m/2^63) * 2^(ex - fb - lz + 63)
    (sign, m, ex - fb as i32 - lz + 63)
}

/// Convierte entre formatos (FCVT). Resuelve NaN, inf, cero.
/// Conversion de precision de una instruccion (FCVT, FCVTN/FCVTL, FCVTXN): FPConvert desempaqueta con FPUnpackCV y
/// redondea con FPRoundCV, que no aplican FZ16 (ni a una entrada ni a un resultado half). FZ si se aplica a S/D.
/// Con FPCR.AHP, el half de la conversion es el formato alternativo de ARM (`convert_ahp`).
pub fn convert(env: &mut Env, from: Ft, to: Ft, x: u64) -> u64 {
    let fpcr = env.fpcr;
    if fpcr & AHP != 0 {
        return convert_ahp(env, from, to, x);
    }
    env.fpcr &= !(1 << 19);
    let r = convert_fz16(env, from, to, x);
    env.fpcr = fpcr;
    r
}

/// FPCR.AHP (bit 26).
pub const AHP: u64 = 1 << 26;

/// FCVT/FCVTL/FCVTN con AHP: half en el formato alternativo (sin infinitos ni NaN; exponente 31 es un numero
/// normal, hasta 131008). Solo las conversiones lo usan: las instrucciones de datos de FEAT_FP16 son siempre IEEE.
/// - Desde half (FPUnpackCV con AHP): el exponente 31 vale 2^16 * (1 + f/1024); el resto es como IEEE. Exacto.
/// - A half (FPConvert, `alt_hp`): NaN -> cero con signo e IOC (tambien con DN y con NaN silenciosa); infinito ->
///   signo:111...1 (el maximo) e IOC; si al redondear el exponente llega a 32 (FPRoundBase, rama de half
///   alternativo) -> signo:111...1, IOC y sin IXC ni OFC. Lo demas, como IEEE (sin FZ16: FPRoundCV).
#[cold]
#[inline(never)]
fn convert_ahp(env: &mut Env, from: Ft, to: Ft, x: u64) -> u64 {
    let fpcr = env.fpcr;
    env.fpcr &= !(1 << 19);
    let r = if from != Ft::H && to != Ft::H {
        convert_fz16(env, from, to, x) // FCVT S<->D, FCVTXN: AHP no interviene
    } else if from == Ft::H {
        let sign = x & 0x8000 != 0;
        if (x >> 10) & 31 == 31 {
            pack(env, to, sign, ((x & 0x3FF) | 0x400) << 53, 16, false)
        } else {
            convert_fz16(env, from, to, x)
        }
    } else if is_nan(from, x) || is_inf(from, x) {
        env.flags |= IOC;
        let sg = if x & from.sign_bit() != 0 { 0x8000 } else { 0 };
        if is_nan(from, x) { sg } else { sg | 0x7FFF }
    } else {
        let (sign, m, e) = unpack(from, x);
        if is_zero(from, x) || is_denorm(from, x) || e < 15 {
            // por debajo de 2^15 los dos formatos redondean igual (el exponente 31 no se alcanza)
            convert_fz16(env, from, to, x)
        } else {
            // |x| >= 2^15: se redondea x/2 en IEEE (normal, exacto al escalar) y se sube el exponente; el
            // desbordamiento IEEE de x/2 es exactamente el exponente 32 de FPRoundBase
            let old = env.flags;
            let h = pack(env, Ft::H, sign, m, e - 1, false);
            if env.flags & OFC != 0 {
                env.flags = old | IOC;
                (h & 0x8000) | 0x7FFF
            } else {
                h + 0x400
            }
        }
    };
    env.fpcr = fpcr;
    r
}

/// Como `convert` pero aplicando FZ16 al resultado half (redondeo de una operacion aritmetica half hecha en f64).
fn convert_fz16(env: &mut Env, from: Ft, to: Ft, x: u64) -> u64 {
    if is_nan(from, x) {
        if is_snan(from, x) {
            env.flags |= IOC;
        }
        if env.dn() {
            return default_nan(to);
        }
        // payload: se conservan los bits altos de la fraccion; se fuerza el bit de silenciado
        let fb_from = from.frac_bits();
        let fb_to = to.frac_bits();
        let frac = x & ((1u64 << fb_from) - 1);
        let nf = if fb_to >= fb_from { frac << (fb_to - fb_from) } else { frac >> (fb_from - fb_to) };
        let sign = if x & from.sign_bit() != 0 { to.sign_bit() } else { 0 };
        return sign | (((1u64 << to.exp_bits()) - 1) << fb_to) | nf | (1u64 << (fb_to - 1));
    }
    let sign = x & from.sign_bit() != 0;
    if is_inf(from, x) {
        return inf(to, sign);
    }
    if is_zero(from, x) {
        return if sign { to.sign_bit() } else { 0 };
    }
    // FZ en la entrada: un subnormal de entrada se trata como cero (con IDC)
    let in_fz = match from {
        Ft::H => false,
        _ => env.fz(),
    };
    if is_denorm(from, x) && in_fz {
        env.flags |= IDC;
        return if sign { to.sign_bit() } else { 0 };
    }
    let (s, m, e) = unpack(from, x);
    pack(env, to, s, m, e, false)
}

// ------------------------------------------------------------------------------------------
// Aritmetica
// ------------------------------------------------------------------------------------------

/// ARM detecta "tiny" antes del redondeo; x86 lo hace despues. Si el resultado redondeado es
/// exactamente el minimo normal y fue inexacto, se repite la operacion con redondeo a cero:
/// si ese resultado es menor que el minimo normal, el valor exacto era diminuto. Sin FZ falta UE.
/// Con FZ (FTZ en MXCSR) ARM ademas lo vacia: FPRoundBase compara el exponente sin redondear con el
/// minimo y devuelve cero con signo y solo UFC; x86 deja el minimo normal (inexacto). Devuelve el
/// resultado corregido.
fn tiny_fix(ft: Ft, r: u64, redo: impl Fn() -> u64) -> u64 {
    let (min, sign) = match ft {
        Ft::S => (0x0080_0000u64, 0x8000_0000u64),
        Ft::D => (0x0010_0000_0000_0000u64, 0x8000_0000_0000_0000u64),
        Ft::H => return r,
    };
    if r & !sign != min {
        return r;
    }
    let csr = unsafe { _mm_getcsr() };
    if csr & 0x20 == 0 || csr & 0x10 != 0 {
        return r;
    }
    let ftz = csr & 0x8000 != 0;
    unsafe { _mm_setcsr(csr | 0x6000) };
    let z = redo();
    let tiny = z & !sign < min;
    // (con FZ, `drain` no anota IXC si hay UE: solo UFC, como FPRoundBase)
    unsafe { _mm_setcsr(if tiny { csr | 0x10 } else { csr }) };
    if tiny && ftz {
        r & sign
    } else {
        r
    }
}

macro_rules! arith2 {
    ($name:ident, $op:tt) => {
        pub fn $name(env: &mut Env, ft: Ft, a: u64, b: u64) -> u64 {
            let (a, b) = (fz_in(env, ft, a), fz_in(env, ft, b));
            if let Some(r) = nan2(env, ft, a, b) {
                return r;
            }
            match ft {
                Ft::D => {
                    let r = bb(f64::from_bits(a)) $op bb(f64::from_bits(b));
                    let r = bb(r);
                    if r.is_nan() {
                        return invalid(env, Ft::D);
                    }
                    tiny_fix(Ft::D, r.to_bits(), || (f64::from_bits(a) $op f64::from_bits(b)).to_bits())
                }
                Ft::S => {
                    let r = bb(f32::from_bits(a as u32)) $op bb(f32::from_bits(b as u32));
                    let r = bb(r);
                    if r.is_nan() {
                        return invalid(env, Ft::S);
                    }
                    tiny_fix(Ft::S, r.to_bits() as u64, || (f32::from_bits(a as u32) $op f32::from_bits(b as u32)).to_bits() as u64)
                }
                Ft::H => {
                    let r = bb(h2f64(a)) $op bb(h2f64(b));
                    let r = bb(r);
                    env.drain(1 | 4 | 32);
                    if r.is_nan() {
                        return invalid(env, Ft::H);
                    }
                    f64_to_h(env, r)
                }
            }
        }
    };
}

arith2!(fadd, +);
arith2!(fsub, -);
arith2!(fmul, *);

/// f64 (resultado de una operacion en f64 de operandos half) -> half redondeado
pub fn f64_to_h(env: &mut Env, r: f64) -> u64 {
    convert_fz16(env, Ft::D, Ft::H, r.to_bits())
}

pub fn fdiv(env: &mut Env, ft: Ft, a: u64, b: u64) -> u64 {
    let (a, b) = (fz_in(env, ft, a), fz_in(env, ft, b));
    if let Some(r) = nan2(env, ft, a, b) {
        return r;
    }
    // 0/0 e inf/inf son invalidos; x/0 (x finito no cero) es division por cero
    let (za, zb) = (is_zero(ft, a), is_zero(ft, b));
    let (ia, ib) = (is_inf(ft, a), is_inf(ft, b));
    if (za && zb) || (ia && ib) {
        return invalid(env, ft);
    }
    match ft {
        Ft::D => {
            let r = bb(bb(f64::from_bits(a)) / bb(f64::from_bits(b)));
            tiny_fix(Ft::D, r.to_bits(), || (f64::from_bits(a) / f64::from_bits(b)).to_bits())
        }
        Ft::S => {
            let r = bb(bb(f32::from_bits(a as u32)) / bb(f32::from_bits(b as u32)));
            tiny_fix(Ft::S, r.to_bits() as u64, || (f32::from_bits(a as u32) / f32::from_bits(b as u32)).to_bits() as u64)
        }
        Ft::H => {
            let r = bb(bb(h2f64(a)) / bb(h2f64(b)));
            env.drain(1 | 4 | 32);
            if is_zero(ft, b) || r.is_infinite() && !ia {
                // x/0 o desbordamiento: el redondeo de f64 a half decide
            }
            f64_to_h(env, r)
        }
    }
}

pub fn fsqrt(env: &mut Env, ft: Ft, a: u64) -> u64 {
    let a = fz_in(env, ft, a);
    if is_nan(ft, a) {
        return process_nan(env, ft, a);
    }
    if is_zero(ft, a) {
        return a;
    }
    if a & ft.sign_bit() != 0 {
        return invalid(env, ft);
    }
    match ft {
        Ft::D => bb(bb(f64::from_bits(a)).sqrt()).to_bits(),
        Ft::S => bb(bb(f32::from_bits(a as u32)).sqrt()).to_bits() as u64,
        Ft::H => {
            let r = bb(bb(h2f64(a)).sqrt());
            env.drain(1 | 4 | 32);
            f64_to_h(env, r)
        }
    }
}

/// a*b + c con un unico redondeo (FMADD). `c` es el sumando.
pub fn ffma(env: &mut Env, ft: Ft, c: u64, a: u64, b: u64) -> u64 {
    let (c, a, b) = (fz_in(env, ft, c), fz_in(env, ft, a), fz_in(env, ft, b));
    // NaN: caso especial 0*inf + QNaN es invalido (ARM)
    let (ia, ib) = (is_inf(ft, a), is_inf(ft, b));
    let (za, zb) = (is_zero(ft, a), is_zero(ft, b));
    let inf_times_zero = (ia && zb) || (za && ib);
    if is_nan(ft, c) && !is_snan(ft, c) && !is_nan(ft, a) && !is_nan(ft, b) && inf_times_zero {
        return invalid(env, ft);
    }
    if let Some(r) = nan3(env, ft, c, a, b) {
        return r;
    }
    if inf_times_zero {
        return invalid(env, ft);
    }
    let prod_inf = ia || ib;
    let prod_neg = (a ^ b) & ft.sign_bit() != 0;
    if prod_inf {
        if is_inf(ft, c) && ((c & ft.sign_bit() != 0) != prod_neg) {
            return invalid(env, ft);
        }
        return inf(ft, prod_neg);
    }
    if is_inf(ft, c) {
        return c;
    }
    match ft {
        Ft::D => {
            let r = bb(bb(f64::from_bits(a)).mul_add(bb(f64::from_bits(b)), bb(f64::from_bits(c)))).to_bits();
            tiny_fix(Ft::D, r, || f64::from_bits(a).mul_add(f64::from_bits(b), f64::from_bits(c)).to_bits())
        }
        Ft::S => {
            let r = bb(bb(f32::from_bits(a as u32)).mul_add(bb(f32::from_bits(b as u32)), bb(f32::from_bits(c as u32)))).to_bits() as u64;
            tiny_fix(Ft::S, r, || f32::from_bits(a as u32).mul_add(f32::from_bits(b as u32), f32::from_bits(c as u32)).to_bits() as u64)
        }
        Ft::H => {
            let r = bb(bb(h2f64(a)).mul_add(bb(h2f64(b)), bb(h2f64(c))));
            env.drain(1 | 4 | 32);
            f64_to_h(env, r)
        }
    }
}

/// Redondeo a entero por manipulacion de bits (independiente de MXCSR y sin flags espurias).
pub fn rnd(env: &mut Env, x: f64, mode: u8) -> f64 {
    env.drain(0x3F);
    let r = rnd_inner(x, mode);
    env.drain(0);
    r
}

fn rnd_inner(x: f64, mode: u8) -> f64 {
    let b = x.to_bits();
    let e = ((b >> 52) & 0x7FF) as i32 - 1023;
    if e >= 52 || x.is_nan() || x.is_infinite() {
        return x;
    }
    let sign = b >> 63 != 0;
    let t = if e < 0 { f64::from_bits(b & (1u64 << 63)) } else { f64::from_bits(b & !((1u64 << (52 - e)) - 1)) };
    // t = truncado; diferencia exacta
    let frac = (x - t).abs();
    if frac == 0.0 {
        return t;
    }
    let away = if sign { t - 1.0 } else { t + 1.0 };
    let away = if away == 0.0 && sign { -0.0 } else { away };
    match mode {
        3 => t,
        1 => {
            if sign {
                t
            } else {
                away
            }
        }
        2 => {
            if sign {
                away
            } else {
                t
            }
        }
        4 => {
            if frac >= 0.5 {
                away
            } else {
                t
            }
        }
        _ => {
            if frac > 0.5 {
                away
            } else if frac < 0.5 {
                t
            } else {
                // empate: al par
                let ti = t.abs();
                let odd = ti < 4503599627370496.0 && (ti as u64) & 1 == 1;
                if odd {
                    away
                } else {
                    t
                }
            }
        }
    }
}

/// FPUnpack: un subnormal de entrada se lee como cero con signo con FZ (S/D, anotando IDC) o con FZ16 (H, sin IDC:
/// FPUnpackBase solo da InputDenorm en 32 y 64 bits). Las conversiones de precision (FCVT, FPUnpackCV) no aplican
/// FZ16: ver `convert`.
fn flushes(env: &mut Env, ft: Ft, x: u64) -> bool {
    if !is_denorm(ft, x) {
        return false;
    }
    if ft == Ft::H {
        return env.fz16();
    }
    if env.fz() {
        env.flags |= IDC;
        return true;
    }
    false
}

/// Operando de entrada tras FPUnpack (ver `flushes`).
pub fn fz_in(env: &mut Env, ft: Ft, x: u64) -> u64 {
    if flushes(env, ft, x) {
        return x & ft.sign_bit();
    }
    x
}

pub fn fneg(ft: Ft, a: u64) -> u64 {
    a ^ ft.sign_bit()
}
pub fn fabs(ft: Ft, a: u64) -> u64 {
    a & (ft.mask() >> 1)
}

fn lt_val(ft: Ft, a: u64, b: u64) -> bool {
    // comparacion numerica de dos valores no NaN
    let (x, y) = (to_f64(ft, a), to_f64(ft, b));
    x < y
}

pub fn fmax(env: &mut Env, ft: Ft, a: u64, b: u64, num: bool, is_max: bool) -> u64 {
    // num = variante "NM" (NaN silencioso se trata como dato ausente)
    let (a, b) = (fz_in(env, ft, a), fz_in(env, ft, b));
    if num {
        let (qa, qb) = (is_nan(ft, a) && !is_snan(ft, a), is_nan(ft, b) && !is_snan(ft, b));
        if !is_snan(ft, a) && !is_snan(ft, b) {
            if qa && !qb {
                return b;
            }
            if qb && !qa {
                return a;
            }
        }
    }
    if let Some(r) = nan2(env, ft, a, b) {
        return r;
    }
    if is_zero(ft, a) && is_zero(ft, b) {
        let (na, nb) = (a & ft.sign_bit() != 0, b & ft.sign_bit() != 0);
        return if is_max {
            if na && nb {
                a
            } else if !na {
                a
            } else {
                b
            }
        } else if na {
            a
        } else if nb {
            b
        } else {
            a
        };
    }
    let a_lt_b = lt_val(ft, a, b);
    if is_max {
        if a_lt_b {
            b
        } else {
            a
        }
    } else if a_lt_b {
        a
    } else {
        b
    }
}

/// Comparacion: devuelve NZCV (bits 31..28). `signal`: FCMPE (QNaN tambien da IOC)
pub fn fcmp(env: &mut Env, ft: Ft, a: u64, b: u64, signal: bool) -> u64 {
    let (a, b) = (fz_in(env, ft, a), fz_in(env, ft, b));
    if is_nan(ft, a) || is_nan(ft, b) {
        if is_snan(ft, a) || is_snan(ft, b) || signal {
            env.flags |= IOC;
        }
        return 0x3 << 28; // no ordenado: C=1, V=1
    }
    let (x, y) = (to_f64(ft, a), to_f64(ft, b));
    if x == y {
        0x6 << 28
    } else if x < y {
        0x8 << 28
    } else {
        0x2 << 28
    }
}

// ------------------------------------------------------------------------------------------
// Redondeo a entero (FRINT*) y conversiones fp <-> entero
// ------------------------------------------------------------------------------------------

/// rmode: 0 N (pares), 1 P, 2 M, 3 Z, 4 A (lejos del cero). `exact` marca IXC (FRINTX).
pub fn frint(env: &mut Env, ft: Ft, a: u64, rmode: u8, exact: bool) -> u64 {
    if is_nan(ft, a) {
        return process_nan(env, ft, a);
    }
    if is_inf(ft, a) || is_zero(ft, a) {
        return a;
    }
    if flushes(env, ft, a) {
        return a & ft.sign_bit();
    }
    let x = to_f64(ft, a);
    let r = rnd(env, x, rmode);
    if exact && r != x {
        env.flags |= IXC;
    }
    // el resultado conserva el signo del operando (p. ej. -0.3 -> -0.0)
    let r = if r == 0.0 && x.is_sign_negative() { -0.0 } else { r };
    match ft {
        Ft::D => r.to_bits(),
        Ft::S => {
            let v = (r as f32).to_bits() as u64;
            env.drain(0);
            v
        }
        Ft::H => {
            let mut e2 = Env { fpcr: env.fpcr, flags: 0, old_csr: 0 };
            let v = f64_to_h(&mut e2, r);
            v
        }
    }
}

/// FCVT{N,P,M,Z,A}{S,U}: de fp a entero de `width` bits (32/64) con saturacion.
pub fn fcvt_int(env: &mut Env, ft: Ft, a: u64, rmode: u8, signed: bool, width: u32, fbits: u32) -> u64 {
    if is_nan(ft, a) {
        env.flags |= IOC;
        return 0;
    }
    let neg = a & ft.sign_bit() != 0;
    let (lo, hi): (f64, f64) = if signed {
        (-(2f64.powi(width as i32 - 1)), 2f64.powi(width as i32 - 1))
    } else {
        (0.0, 2f64.powi(width as i32))
    };
    let satmax = if signed { (1u64 << (width - 1)) - 1 } else if width == 64 { u64::MAX } else { (1u64 << width) - 1 };
    let satmin = if signed { (1u64 << (width - 1)).wrapping_neg() & if width == 64 { u64::MAX } else { (1u64 << width) - 1 } } else { 0 };
    if is_inf(ft, a) {
        env.flags |= IOC;
        return if neg { satmin } else { satmax };
    }
    let mut x = to_f64(ft, a);
    if fbits > 0 {
        env.drain(0x3F);
        x = bb(x) * 2f64.powi(fbits as i32);
        env.drain(0);
    }
    if flushes(env, ft, a) {
        x = 0.0;
    }
    let r = rnd(env, x, rmode);
    if r >= hi || r < lo {
        env.flags |= IOC;
        return if r < lo { satmin } else { satmax };
    }
    if r != x {
        env.flags |= IXC;
    }
    let v: u64 = if signed { (r as i64) as u64 } else { r as u64 };
    env.drain(0);
    if width == 32 {
        v & 0xFFFF_FFFF
    } else {
        v
    }
}

/// SCVTF/UCVTF: entero (de `width` bits) -> fp, con `fbits` bits fraccionarios.
pub fn int_to_fp(env: &mut Env, ft: Ft, v: u64, signed: bool, width: u32, fbits: u32) -> u64 {
    let (neg, mag) = if signed {
        let s = if width == 32 { v as u32 as i32 as i64 } else { v as i64 };
        (s < 0, s.unsigned_abs())
    } else {
        (false, if width == 32 { v & 0xFFFF_FFFF } else { v })
    };
    if mag == 0 {
        return 0;
    }
    let lz = mag.leading_zeros() as i32;
    let m = mag << lz;
    let exp = 63 - lz - fbits as i32;
    pack(env, ft, neg, m, exp, false)
}

/// FRECPE / FRSQRTE: estimaciones segun el pseudocodigo de ARM (tablas por formula).
pub fn recip_estimate(a: u64) -> u64 {
    // a: 9 bits (1xxxxxxxx) en [256, 511]; devuelve 9 bits en [256, 511]
    let a = a * 2 + 1;
    let b = (1u64 << 19) / a;
    (b + 1) / 2
}

pub fn recip_sqrt_estimate(a: u64) -> u64 {
    // a en [128, 511]
    let a = if a < 256 { a * 2 + 1 } else { ((a >> 1) << 1 | 1) * 2 };
    let mut b = 512u64;
    while a * (b + 1) * (b + 1) < (1u64 << 28) {
        b += 1;
    }
    (b + 1) / 2
}

pub fn frecpe(env: &mut Env, ft: Ft, a: u64) -> u64 {
    if is_nan(ft, a) {
        return process_nan(env, ft, a);
    }
    let sign = a & ft.sign_bit() != 0;
    let sg = if sign { ft.sign_bit() } else { 0 };
    if is_inf(ft, a) {
        return sg;
    }
    if is_zero(ft, a) {
        env.flags |= DZC;
        return inf(ft, sign);
    }
    let fb = ft.frac_bits();
    let eb = ft.exp_bits();
    let frac = a & ((1u64 << fb) - 1);
    let mut e = ((a >> fb) & ((1 << eb) - 1)) as i32;
    let maxexp = (1i32 << eb) - 1;
    // subnormal muy pequeno -> desbordamiento
    if e == 0 && flushes(env, ft, a) {
        env.flags |= DZC;
        return inf(ft, sign);
    }
    if e == 0 && frac >> (fb - 2) == 0 {
        env.flags |= OFC | IXC;
        let to_inf = match env.rmode() {
            0 => true,
            1 => !sign,
            2 => sign,
            _ => false,
        };
        return if to_inf { inf(ft, sign) } else { max_norm(ft, sign) };
    }
    if e == 0 && flushes(env, ft, a) {
        env.flags |= DZC;
        return inf(ft, sign);
    }
    // FPRecipEstimate: con FZ (FZ16 en half) y |a| >= 2^14/2^126/2^1022 el resultado seria subnormal: cero y UFC
    let fzr = if ft == Ft::H { env.fz16() } else { env.fz() };
    if fzr && e >= maxexp / 2 + maxexp / 2 - 1 {
        env.flags |= UFC;
        return sg;
    }
    let mut f = frac;
    if e == 0 {
        // normalizar subnormal
        if f >> (fb - 1) == 0 {
            f <<= 2;
            e = -1;
        } else {
            f <<= 1;
            e = 0;
        }
        f &= (1u64 << fb) - 1;
        // tras normalizar: exponente efectivo e
    }
    let scaled = (1u64 << 8) | (f >> (fb - 8));
    let ri = recip_estimate(scaled);
    let mut rf = (ri & 0xFF) << (fb - 8);
    let mut re = 2 * (maxexp / 2) - 1 - e; // 2*bias... ver pseudocodigo: result_exp = 253 - exp (S)
    if re == 0 || re == -1 {
        // resultado subnormal
        rf = (rf >> 1) | (1u64 << (fb - 1));
        if re == -1 {
            rf >>= 1;
        }
        re = 0;
    }
    sg | ((re as u64) << fb) | rf
}

pub fn frsqrte(env: &mut Env, ft: Ft, a: u64) -> u64 {
    if is_nan(ft, a) {
        return process_nan(env, ft, a);
    }
    let sign = a & ft.sign_bit() != 0;
    if is_zero(ft, a) {
        env.flags |= DZC;
        return inf(ft, sign);
    }
    if sign {
        return invalid(env, ft);
    }
    if is_inf(ft, a) {
        return 0;
    }
    let fb = ft.frac_bits();
    let eb = ft.exp_bits();
    let frac = a & ((1u64 << fb) - 1);
    let mut e = ((a >> fb) & ((1 << eb) - 1)) as i32;
    let mut f = frac;
    if e == 0 && flushes(env, ft, a) {
        env.flags |= DZC;
        return inf(ft, false);
    }
    if e == 0 {
        while f >> (fb - 1) == 0 {
            f <<= 1;
            e -= 1;
        }
        f <<= 1;
        f &= (1u64 << fb) - 1;
    }
    let t = if e & 1 == 0 { 256 | ((f >> (fb - 8)) & 0xFF) } else { 128 | ((f >> (fb - 7)) & 0x7F) };
    let ri = recip_sqrt_estimate(t);
    let rf = (ri & 0xFF) << (fb - 8);
    let bias3 = 3 * ((1i32 << (eb - 1)) - 1);
    let re = (bias3 - 1 - e) / 2;
    ((re as u64) << fb) | rf
}

/// FRINT32Z/X, FRINT64Z/X: redondea a entero dentro del rango de `nbits` bits.
pub fn frint_n(env: &mut Env, ft: Ft, a: u64, mode: u8, nbits: u32, exact: bool) -> u64 {
    let lo = -(2f64.powi(nbits as i32 - 1));
    let sat = |env: &mut Env| -> u64 {
        env.flags |= IOC;
        match ft {
            Ft::D => lo.to_bits(),
            _ => (lo as f32).to_bits() as u64,
        }
    };
    if is_nan(ft, a) || is_inf(ft, a) {
        return sat(env);
    }
    let a = fz_in(env, ft, a);
    let x = to_f64(ft, a);
    let r = rnd(env, x, mode);
    if r >= -lo || r < lo {
        return sat(env);
    }
    if exact && r != x {
        env.flags |= IXC;
    }
    let r = if r == 0.0 && x.is_sign_negative() { -0.0 } else { r };
    match ft {
        Ft::D => r.to_bits(),
        _ => (r as f32).to_bits() as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FZ: u64 = 1 << 24;
    const DN: u64 = 1 << 25;
    const FZ16: u64 = 1 << 19;

    fn run(fpcr: u64, f: impl FnOnce(&mut Env) -> u64) -> (u64, u64) {
        let mut e = Env::enter(fpcr);
        let r = f(&mut e);
        (r, e.leave())
    }

    /// FZ: un resultado cuyo valor exacto queda por debajo del minimo normal se vacia aunque al redondear diera el
    /// minimo normal (FPRoundBase compara el exponente sin redondear): cero con signo y solo UFC. Sin FZ, el minimo
    /// normal con UFC|IXC (tininess antes del redondeo).
    #[test]
    fn fz_vacia_por_el_valor_sin_redondear() {
        // a*b + c con c = -minimo normal y a*b diminuto positivo (FNMSUB de la discrepancia con Unicorn)
        let (a, b, c) = (0x201a_8e3f_a317_b57a, 0x0010_0000_0000_0000, 0x8010_0000_0000_0000);
        assert_eq!(run(FZ | DN, |e| ffma(e, Ft::D, c, a, b)), (0x8000_0000_0000_0000, UFC));
        assert_eq!(run(0, |e| ffma(e, Ft::D, c, a, b)), (0x8010_0000_0000_0000, UFC | IXC));
        // FMUL S: (1 - 2^-24) * 2^-126 redondea (empate a par) al minimo normal
        let (a, b) = (0x1fff_ffff, 0x2000_0000);
        assert_eq!(run(FZ, |e| fmul(e, Ft::S, a, b)), (0, UFC));
        assert_eq!(run(0, |e| fmul(e, Ft::S, a, b)), (0x0080_0000, UFC | IXC));
        // FDIV D: cociente exacto subnormal (x86 tambien lo ve diminuto): con FZ, cero y UFC
        let (a, b) = (0x0010_0000_0000_0000, 0x3ff0_0000_0000_0001);
        assert_eq!(run(FZ, |e| fdiv(e, Ft::D, a, b)), (0, UFC));
        // con el valor exacto ya normal no cambia nada
        assert_eq!(run(FZ, |e| fmul(e, Ft::S, 0x2000_0000, 0x2000_0000)), (0x0080_0000, 0));
    }

    /// FZ16: un subnormal half de entrada se lee como cero, sin IDC (FPUnpackBase). Las conversiones de precision
    /// (FCVT: FPUnpackCV/FPRoundCV) no aplican FZ16 ni a la entrada ni a la salida.
    /// FPCR.AHP: half alternativo en FCVT (FPConvert, FPUnpackCV, FPRoundBase) y solo ahi.
    #[test]
    fn ahp_en_las_conversiones() {
        let c = |fpcr, from, to, x| run(AHP | fpcr, |e| convert(e, from, to, x));
        // desde half: el exponente 31 es un numero (2^16 .. 131008), sin flags
        assert_eq!(c(0, Ft::H, Ft::S, 0x7C00), (0x4780_0000, 0));
        assert_eq!(c(0, Ft::H, Ft::S, 0x7FFF), (0x47FF_E000, 0));
        assert_eq!(c(0, Ft::H, Ft::D, 0xFC01), (0xC0F0_0400_0000_0000, 0));
        assert_eq!(c(0, Ft::H, Ft::S, 0x3C00), (0x3F80_0000, 0));
        assert_eq!(c(FZ16, Ft::H, Ft::S, 0x0001), (0x3380_0000, 0)); // FZ16 no se aplica a FCVT
        // a half: el rango llega a 131008
        assert_eq!(c(0, Ft::S, Ft::H, 0x4780_0000), (0x7C00, 0));
        assert_eq!(c(0, Ft::S, Ft::H, 0x47FF_E000), (0x7FFF, 0));
        assert_eq!(c(0, Ft::S, Ft::H, 0x47FF_E800), (0x7FFF, IXC)); // 131024: redondea abajo
        assert_eq!(c(0, Ft::S, Ft::H, 0x47FF_D000), (0x7FFE, IXC)); // 130976: empate, al par
        assert_eq!(c(0, Ft::S, Ft::H, 0x47FF_F000), (0x7FFF, IOC)); // 131040: empate hacia 2^17 -> desborda
        assert_eq!(c(3 << 22, Ft::S, Ft::H, 0x47FF_F000), (0x7FFF, IXC)); // RZ: 131008 inexacto
        assert_eq!(c(3 << 22, Ft::D, Ft::H, 0xC100_0000_0000_0000), (0xFFFF, IOC)); // -2^17 con RZ
        assert_eq!(c(0, Ft::S, Ft::H, 0x477F_F000), (0x7C00, IXC)); // 65520: 2^16 (en IEEE seria infinito)
        // infinitos y NaN: el maximo o cero, con IOC (tambien NaN silenciosa y con DN)
        assert_eq!(c(0, Ft::S, Ft::H, 0x7F80_0000), (0x7FFF, IOC));
        assert_eq!(c(0, Ft::D, Ft::H, 0xFFF0_0000_0000_0000), (0xFFFF, IOC));
        assert_eq!(c(0, Ft::S, Ft::H, 0x7FC0_0000), (0x0000, IOC));
        assert_eq!(c(DN, Ft::S, Ft::H, 0xFFC0_0001), (0x8000, IOC));
        // por debajo del rango, como IEEE: subnormal exacto, FZ de la entrada S
        assert_eq!(c(0, Ft::S, Ft::H, 0x3380_0000), (0x0001, 0));
        assert_eq!(c(FZ, Ft::S, Ft::H, 0x0000_0001), (0x0000, IDC));
        // sin AHP no cambia nada; y la aritmetica half (FEAT_FP16) siempre es IEEE
        assert_eq!(run(0, |e| convert(e, Ft::S, Ft::H, 0x7F80_0000)), (0x7C00, 0));
        assert_eq!(run(AHP, |e| fadd(e, Ft::H, 0x7C00, 0x3C00)), (0x7C00, 0));
    }

    #[test]
    fn frecpe_con_fz_da_cero_y_ufc() {
        // 1/65504 es subnormal en half: con FZ16, +0 y UFC (FPRecipEstimate); sin FZ16, subnormal sin UFC
        assert_eq!(run(FZ16, |e| frecpe(e, Ft::H, 0x7bff)), (0x0000, UFC));
        assert_eq!(run(FZ16, |e| frecpe(e, Ft::H, 0xf400)), (0x8000, UFC)); // -2^14
        assert_eq!(run(FZ16, |e| frecpe(e, Ft::H, 0x73ff)).1, 0); // < 2^14
        assert_ne!(run(0, |e| frecpe(e, Ft::H, 0x7bff)).0, 0);
        assert_eq!(run(FZ, |e| frecpe(e, Ft::S, 0x7f00_0000)), (0, UFC)); // 2^127
        assert_eq!(run(FZ, |e| frecpe(e, Ft::D, 0x7fd0_0000_0000_0000)), (0, UFC)); // 2^1022
        assert_eq!(run(FZ, |e| frecpe(e, Ft::S, 0x7e80_0000)), (0, UFC)); // 2^126: el limite entra
        assert_eq!(run(FZ, |e| frecpe(e, Ft::S, 0x7e7f_ffff)).1, 0);
    }

    #[test]
    fn fz16_en_la_entrada_y_no_en_fcvt() {
        assert_eq!(run(FZ16, |e| fadd(e, Ft::H, 0x0001, 0x0000)), (0x0000, 0));
        assert_eq!(run(FZ16, |e| fadd(e, Ft::H, 0x8001, 0x8000)), (0x8000, 0));
        assert_eq!(run(0, |e| fadd(e, Ft::H, 0x0001, 0x0000)), (0x0001, 0));
        // FZ no afecta a half
        assert_eq!(run(FZ, |e| fadd(e, Ft::H, 0x0001, 0x0000)), (0x0001, 0));
        // comparacion y multiplicacion-suma fusionada tambien leen cero
        assert_eq!(run(FZ16, |e| ffma(e, Ft::H, 0x0000, 0x0001, 0x3c00)), (0x0000, 0));
        // FCVT: H -> S conserva el subnormal; S -> H redondea a subnormal (UFC|IXC) sin vaciar
        assert_eq!(run(FZ16, |e| convert(e, Ft::H, Ft::S, 0x0001)), (0x3380_0000, 0));
        assert_eq!(run(FZ16, |e| convert(e, Ft::S, Ft::H, 0x3380_0000)), (0x0001, 0));
        assert_eq!(run(FZ16, |e| convert(e, Ft::S, Ft::H, 0x33c0_0000)), (0x0002, UFC | IXC));
        // con FZ, la entrada single subnormal de FCVT si se vacia (IDC)
        assert_eq!(run(FZ, |e| convert(e, Ft::S, Ft::H, 0x0000_0001)), (0x0000, IDC));
    }
}
