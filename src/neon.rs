//! NEON / Advanced SIMD: motor por lineas (lanes) sobre u128.

use crate::cpu::*;
use crate::fp::{bit, bits};
use crate::interp::Flow;
use crate::softfp::*;

/// Vector de carriles de capacidad fija (16: los bytes de un registro de 128 bits). El interprete no reserva memoria:
/// tambien ejecuta los manejadores de senal del guest (ver sig.rs), donde no se puede llamar a malloc.
#[derive(Clone, Copy)]
pub struct Lanes {
    a: [u64; 16],
    n: usize,
}

impl Lanes {
    pub const fn new() -> Lanes {
        Lanes { a: [0; 16], n: 0 }
    }
    pub fn with_capacity(_n: usize) -> Lanes {
        Lanes::new()
    }
    #[inline]
    pub fn push(&mut self, v: u64) {
        self.a[self.n] = v;
        self.n += 1;
    }
}

impl std::ops::Deref for Lanes {
    type Target = [u64];
    fn deref(&self) -> &[u64] {
        &self.a[..self.n]
    }
}

impl FromIterator<u64> for Lanes {
    fn from_iter<I: IntoIterator<Item = u64>>(it: I) -> Lanes {
        let mut l = Lanes::new();
        for v in it {
            l.push(v);
        }
        l
    }
}

impl IntoIterator for Lanes {
    type Item = u64;
    type IntoIter = std::iter::Take<std::array::IntoIter<u64, 16>>;
    fn into_iter(self) -> Self::IntoIter {
        self.a.into_iter().take(self.n)
    }
}

pub type V = u128;

#[inline]
pub fn rv(c: &Cpu, r: u32) -> V {
    c.v[r as usize][0] as u128 | ((c.v[r as usize][1] as u128) << 64)
}
#[inline]
pub fn wv(c: &mut Cpu, r: u32, v: V, full: bool) {
    let v = if full { v } else { v & (u64::MAX as u128) };
    c.v[r as usize] = [v as u64, (v >> 64) as u64];
}
#[inline]
pub fn mk(esb: u32) -> u128 {
    if esb >= 128 {
        u128::MAX
    } else {
        (1u128 << esb) - 1
    }
}
#[inline]
pub fn lane(v: V, esb: u32, i: usize) -> u64 {
    ((v >> (esb as usize * i)) & mk(esb)) as u64
}
#[inline]
pub fn setlane(v: &mut V, esb: u32, i: usize, x: u64) {
    let sh = esb as usize * i;
    *v = (*v & !(mk(esb) << sh)) | (((x as u128) & mk(esb)) << sh);
}
#[inline]
pub fn sx(x: u64, esb: u32) -> i64 {
    let s = 64 - esb;
    ((x << s) as i64) >> s
}

/// Contexto de ejecucion de una instruccion SIMD.
pub struct Sd<'a> {
    pub c: &'a mut Cpu,
    pub qc: bool,
}

pub fn sat_signed(v: i128, esb: u32, qc: &mut bool) -> u64 {
    let max = (1i128 << (esb - 1)) - 1;
    let min = -(1i128 << (esb - 1));
    if v > max {
        *qc = true;
        max as u64
    } else if v < min {
        *qc = true;
        min as u64
    } else {
        v as u64
    }
}
pub fn sat_unsigned(v: i128, esb: u32, qc: &mut bool) -> u64 {
    let max = ((1u128 << esb) - 1) as i128;
    if v > max {
        *qc = true;
        max as u64
    } else if v < 0 {
        *qc = true;
        0
    } else {
        v as u64
    }
}

/// Resultado de un desplazamiento variable (SSHL/USHL y variantes). `a` es el dato (con signo si `signed`),
/// `sh` el contador con signo (byte bajo de b).
fn var_shift(a: i128, sh: i64, esb: u32, signed: bool, round: bool, sat: bool, qc: &mut bool) -> u64 {
    let m = mk(esb) as i128;
    if sh >= 0 {
        // izquierda
        let r: i128 = if a == 0 {
            0
        } else if sh >= 62 {
            (1i128 << 100) * if a < 0 { -1 } else { 1 }
        } else {
            a << sh
        };
        if sat {
            return if signed { sat_signed(r, esb, qc) } else { sat_unsigned(r, esb, qc) };
        }
        if sh as u32 >= esb {
            return 0;
        }
        (r & m) as u64
    } else {
        let rsh = (-sh) as u32;
        let rs = rsh.min(127);
        let r = if round {
            let half = 1i128 << (rs - 1);
            (a + half) >> rs
        } else {
            a >> rs
        };
        if sat {
            return if signed { sat_signed(r, esb, qc) } else { sat_unsigned(r, esb, qc) };
        }
        (r & m) as u64
    }
}

fn val_s(x: u64, esb: u32) -> i128 {
    sx(x, esb) as i128
}
fn val_u(x: u64) -> i128 {
    x as i128
}

fn clmul8(a: u64, b: u64) -> u64 {
    let mut r = 0u64;
    for i in 0..8 {
        if (b >> i) & 1 == 1 {
            r ^= a << i;
        }
    }
    r & 0xFF
}

// ---------------------------------------------------------------------------------------------
// Despacho
// ---------------------------------------------------------------------------------------------

pub fn exec_simd(c: &mut Cpu, w: u32) -> Flow {
    let top = bits(w, 28, 24);
    let b31 = bit(w, 31);
    let b30 = bit(w, 30);
    // Crypto AES / SHA (dos/tres registros)
    if crate::crypto::is_crypto(w) {
        if bits(w, 31, 24) == 0xCE {
            return crate::crypto::exec_crypto3(c, w);
        }
        return crate::crypto::exec_crypto(c, w);
    }
    match top {
        0b01110 | 0b01111 => {}
        0b11110 | 0b11111 => {
            if b31 || !b30 {
                return Flow::Undef(w);
            }
            return crate::neonx::exec_scalar(c, w);
        }
        _ => return Flow::Undef(w),
    }
    if b31 {
        return Flow::Undef(w);
    }
    let q = b30;
    if top == 0b01111 {
        if bit(w, 10) {
            // desplazamiento con inmediato / inmediato modificado: 0 Q U 011110 ...; con el bit 23 a 1, sin asignar
            if bit(w, 23) {
                return Flow::Undef(w);
            }
            if bits(w, 22, 19) == 0 {
                return crate::neonx::mod_imm(c, w, q);
            }
            return crate::neonx::shift_imm(c, w, q, false);
        }
        return crate::neonx::indexed(c, w, q, false);
    }
    // top == 01110
    if bit(w, 21) {
        if !bit(w, 10) {
            if !bit(w, 11) {
                return three_diff(c, w, q);
            }
            return match bits(w, 20, 17) {
                0b0000 => two_reg_misc(c, w, q),
                0b1000 => across(c, w, q),
                0b1100 => crate::neonx::two_reg_misc_fp16(c, w, q),
                _ => Flow::Undef(w),
            };
        }
        return three_same(c, w, q);
    }
    if bit(w, 10) {
        if bit(w, 15) {
            return crate::neonx::three_reg_ext(c, w, q);
        }
        if bit(w, 22) {
            return crate::neonx::three_same_fp16(c, w, q);
        }
        if bits(w, 23, 22) == 0 {
            return copy(c, w, q);
        }
        return Flow::Undef(w);
    }
    if bit(w, 15) {
        return Flow::Undef(w);
    }
    if bit(w, 29) {
        if bits(w, 23, 22) == 0 {
            return ext(c, w, q);
        }
        return Flow::Undef(w);
    }
    if !bit(w, 11) {
        return tbl(c, w, q);
    }
    permute(c, w, q)
}

fn finish(c: &mut Cpu, sd_qc: bool) {
    if sd_qc {
        c.fpsr |= 1 << 27;
    }
}

fn three_same(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 15, 11);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    if opc >= 0b11000 {
        return crate::neonfp::three_same_fp(c, w, q);
    }
    let a = rv(c, rn);
    let b = rv(c, rm);
    let d = rv(c, rd);
    if opc == 0b00011 {
        let r = match (u, size) {
            (false, 0) => a & b,
            (false, 1) => a & !b,
            (false, 2) => a | b,
            (false, 3) => a | !b,
            (true, 0) => a ^ b,
            (true, 1) => (d & a) | (!d & b),
            (true, 2) => d ^ ((d ^ a) & b),
            _ => d ^ ((d ^ a) & !b),
        };
        wv(c, rd, r, q);
        return Flow::Next;
    }
    let esb = 8u32 << size;
    if size == 3 && !q {
        return Flow::Undef(w);
    }
    let n = (if q { 128 } else { 64 }) / esb as usize;
    let mut qc = false;
    // validez por tamano
    let ok = match opc {
        0b00000 | 0b00010 | 0b00100 => size < 3,
        0b01100 | 0b01101 | 0b01110 | 0b01111 | 0b10010 | 0b10011 | 0b10100 | 0b10101 => size < 3,
        0b10110 => size == 1 || size == 2,
        0b10111 => !u,
        _ => true,
    } && !(opc == 0b10011 && u && size != 0);
    if !ok {
        return Flow::Undef(w);
    }
    let mut r: V = 0;
    // pares
    if matches!(opc, 0b10100 | 0b10101 | 0b10111) {
        let h = n / 2;
        for i in 0..n {
            let (src, j) = if i < h { (a, 2 * i) } else { (b, 2 * (i - h)) };
            let (x, y) = (lane(src, esb, j), lane(src, esb, j + 1));
            let v = match (opc, u) {
                (0b10111, _) => x.wrapping_add(y),
                (0b10100, false) => {
                    if sx(x, esb) >= sx(y, esb) {
                        x
                    } else {
                        y
                    }
                }
                (0b10100, true) => x.max(y),
                (0b10101, false) => {
                    if sx(x, esb) <= sx(y, esb) {
                        x
                    } else {
                        y
                    }
                }
                _ => x.min(y),
            };
            setlane(&mut r, esb, i, v);
        }
        wv(c, rd, r, q);
        return Flow::Next;
    }
    for i in 0..n {
        let x = lane(a, esb, i);
        let y = lane(b, esb, i);
        let dd = lane(d, esb, i);
        let (sa, sb) = (val_s(x, esb), val_s(y, esb));
        let (ua, ub) = (val_u(x), val_u(y));
        let m = mk(esb) as i128;
        let v: u64 = match (opc, u) {
            (0b00000, false) => ((sa + sb) >> 1) as u64,
            (0b00000, true) => ((ua + ub) >> 1) as u64,
            (0b00010, false) => ((sa + sb + 1) >> 1) as u64,
            (0b00010, true) => ((ua + ub + 1) >> 1) as u64,
            (0b00100, false) => ((sa - sb) >> 1) as u64,
            (0b00100, true) => ((ua - ub) >> 1) as u64,
            (0b00001, false) => sat_signed(sa + sb, esb, &mut qc),
            (0b00001, true) => sat_unsigned(ua + ub, esb, &mut qc),
            (0b00101, false) => sat_signed(sa - sb, esb, &mut qc),
            (0b00101, true) => sat_unsigned(ua - ub, esb, &mut qc),
            (0b00110, false) => (sa > sb) as u64 * mk(esb) as u64,
            (0b00110, true) => (ua > ub) as u64 * mk(esb) as u64,
            (0b00111, false) => (sa >= sb) as u64 * mk(esb) as u64,
            (0b00111, true) => (ua >= ub) as u64 * mk(esb) as u64,
            (0b01000, _) => var_shift(if u { ua } else { sa }, sx(y, 8) as i64, esb, !u, false, false, &mut qc),
            (0b01001, _) => var_shift(if u { ua } else { sa }, sx(y, 8) as i64, esb, !u, false, true, &mut qc),
            (0b01010, _) => var_shift(if u { ua } else { sa }, sx(y, 8) as i64, esb, !u, true, false, &mut qc),
            (0b01011, _) => var_shift(if u { ua } else { sa }, sx(y, 8) as i64, esb, !u, true, true, &mut qc),
            (0b01100, false) => (sa.max(sb)) as u64,
            (0b01100, true) => ua.max(ub) as u64,
            (0b01101, false) => (sa.min(sb)) as u64,
            (0b01101, true) => ua.min(ub) as u64,
            (0b01110, false) => (sa - sb).abs() as u64,
            (0b01110, true) => (ua - ub).abs() as u64,
            (0b01111, false) => dd.wrapping_add((sa - sb).abs() as u64),
            (0b01111, true) => dd.wrapping_add((ua - ub).abs() as u64),
            (0b10000, false) => x.wrapping_add(y),
            (0b10000, true) => x.wrapping_sub(y),
            (0b10001, false) => ((x & y) != 0) as u64 * mk(esb) as u64,
            (0b10001, true) => (x == y) as u64 * mk(esb) as u64,
            (0b10010, false) => dd.wrapping_add(x.wrapping_mul(y)),
            (0b10010, true) => dd.wrapping_sub(x.wrapping_mul(y)),
            (0b10011, false) => x.wrapping_mul(y),
            (0b10011, true) => clmul8(x, y),
            (0b10110, false) => {
                let p = 2 * sa * sb;
                sat_signed(p >> esb, esb, &mut qc)
            }
            (0b10110, true) => {
                let p = 2 * sa * sb + (1i128 << (esb - 1));
                sat_signed(p >> esb, esb, &mut qc)
            }
            _ => return Flow::Undef(w),
        };
        let _ = m;
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    finish(c, qc);
    Flow::Next
}

fn three_diff(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 15, 12);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    if size == 3 {
        if opc == 0b1110 && !u {
            return crate::crypto::pmull64(c, w, q);
        }
        return Flow::Undef(w);
    }
    let esb = 8u32 << size;
    let wb = esb * 2;
    let n = 64 / esb as usize;
    let a = rv(c, rn);
    let b = rv(c, rm);
    let d = rv(c, rd);
    let half = |v: V| -> V { if q { v >> 64 } else { v & (u64::MAX as u128) } };
    let mut qc = false;
    let mut r: V = 0;
    let sgn = !u;
    let ext = |x: u64, bits_: u32| -> i128 {
        if sgn {
            sx(x, bits_) as i128
        } else {
            x as i128
        }
    };
    match opc {
        0b0000 | 0b0010 | 0b0101 | 0b0111 | 0b1000 | 0b1010 | 0b1100 | 0b1001 | 0b1011 | 0b1101 => {
            // SQDMLAL/SQDMLSL/SQDMULL: solo U=0 y elementos de 16 o 32 bits
            if matches!(opc, 0b1001 | 0b1011 | 0b1101) && (u || size == 0) {
                return Flow::Undef(w);
            }
            let (ha, hb) = (half(a), half(b));
            for i in 0..n {
                let x = ext(lane(ha, esb, i), esb);
                let y = ext(lane(hb, esb, i), esb);
                let dd = lane(d, wb, i);
                let ddv = if sgn { sx(dd, wb) as i128 } else { dd as i128 };
                let v: u64 = match opc {
                    0b0000 => (x + y) as u64,
                    0b0010 => (x - y) as u64,
                    0b0101 => dd.wrapping_add((x - y).abs() as u64),
                    0b0111 => (x - y).abs() as u64,
                    0b1000 => dd.wrapping_add((x * y) as u64),
                    0b1010 => dd.wrapping_sub((x * y) as u64),
                    0b1100 => (x * y) as u64,
                    0b1101 => {
                        let p = sat_signed(2 * x * y, wb, &mut qc);
                        p
                    }
                    0b1001 => {
                        let p = sat_signed(2 * x * y, wb, &mut qc) as i64 as i128;
                        sat_signed(ddv + p, wb, &mut qc)
                    }
                    _ => {
                        let p = sat_signed(2 * x * y, wb, &mut qc) as i64 as i128;
                        sat_signed(ddv - p, wb, &mut qc)
                    }
                };
                setlane(&mut r, wb, i, v);
            }
        }
        0b0001 | 0b0011 => {
            let hb = half(b);
            for i in 0..n {
                let x = if sgn { sx(lane(a, wb, i), wb) as i128 } else { lane(a, wb, i) as i128 };
                let y = ext(lane(hb, esb, i), esb);
                let v = if opc == 0b0001 { x + y } else { x - y };
                setlane(&mut r, wb, i, v as u64);
            }
        }
        0b0100 | 0b0110 => {
            // ADDHN/SUBHN (+ redondeo si U)
            let rnd: i128 = if u { 1i128 << (esb - 1) } else { 0 };
            let nn = 128 / wb as usize;
            let mut nr: V = 0;
            for i in 0..nn {
                let x = lane(a, wb, i) as i128;
                let y = lane(b, wb, i) as i128;
                let t = if opc == 0b0100 { x + y + rnd } else { x - y + rnd };
                setlane(&mut nr, esb, i, ((t >> esb) & mk(esb) as i128) as u64);
            }
            if q {
                r = (d & (u64::MAX as u128)) | (nr << 64);
            } else {
                r = nr & (u64::MAX as u128);
            }
            wv(c, rd, r, true);
            return Flow::Next;
        }
        0b1110 => {
            if u || size != 0 {
                return Flow::Undef(w);
            }
            let (ha, hb) = (half(a), half(b));
            for i in 0..8 {
                let (x, y) = (lane(ha, 8, i), lane(hb, 8, i));
                let mut p = 0u64;
                for k in 0..8 {
                    if (y >> k) & 1 == 1 {
                        p ^= x << k;
                    }
                }
                setlane(&mut r, 16, i, p);
            }
        }
        _ => return Flow::Undef(w),
    }
    wv(c, rd, r, true);
    finish(c, qc);
    Flow::Next
}

fn two_reg_misc(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    let esb = 8u32 << size;
    let a = rv(c, rn);
    let d = rv(c, rd);
    let n = (if q { 128 } else { 64 }) / esb as usize;
    let mut qc = false;
    let mut r: V = 0;
    // operaciones de punto flotante
    let fp = matches!(opc, 0b10110 | 0b10111 | 0b11000..=0b11111) || (matches!(opc, 0b01100..=0b01111) && size >= 2);
    // (FCMGT etc. exigen size[1] = 1; con size[1] = 0 estas codificaciones no existen)
    if fp && !(opc == 0b10110 && !u && false) {
        if matches!(opc, 0b01100..=0b01111) && size < 2 {
            return Flow::Undef(w);
        }
        if opc == 0b11100 && size >= 2 {
            // URECPE / URSQRTE: entero de 32 bits
            if size != 2 {
                return Flow::Undef(w);
            }
            let es = 32;
            let nn = (if q { 128 } else { 64 }) / es as usize;
            for i in 0..nn {
                let x = lane(a, es, i);
                let v = if !u { urecpe(x as u32) } else { ursqrte(x as u32) };
                setlane(&mut r, es, i, v as u64);
            }
            wv(c, rd, r, q);
            return Flow::Next;
        }
        return crate::neonfp::two_reg_fp(c, w, q);
    }
    if size == 3 && !q && !matches!(opc, 0b00011 | 0b00111 | 0b01000..=0b01011) {
        // size = 3 solo con Q = 1 salvo ABS/NEG/CMxx...
    }
    match opc {
        0b00000 => {
            // REV64 (U=0) / REV32 (U=1)
            let cont = if u { 32 } else { 64 };
            if esb >= cont {
                return Flow::Undef(w);
            }
            let per = (cont / esb) as usize;
            for i in 0..n {
                let g = i / per;
                let j = per - 1 - (i % per);
                setlane(&mut r, esb, i, lane(a, esb, g * per + j));
            }
        }
        0b00001 => {
            if u || size != 0 {
                return Flow::Undef(w);
            }
            for i in 0..n {
                let g = i / 2;
                setlane(&mut r, 8, i, lane(a, 8, g * 2 + (1 - i % 2)));
            }
        }
        0b00010 | 0b00110 => {
            if size == 3 {
                return Flow::Undef(w);
            }
            let wb = esb * 2;
            let nn = (if q { 128 } else { 64 }) / wb as usize;
            for i in 0..nn {
                let (x, y) = (lane(a, esb, 2 * i), lane(a, esb, 2 * i + 1));
                let s = if u { x as u128 + y as u128 } else { (sx(x, esb) as i128 + sx(y, esb) as i128) as u128 };
                let mut v = s as u64;
                if opc == 0b00110 {
                    v = lane(d, wb, i).wrapping_add(v);
                }
                setlane(&mut r, wb, i, v);
            }
        }
        0b00011 => {
            if size == 3 && !q {
                return Flow::Undef(w);
            }
            for i in 0..n {
                let (x, dd) = (lane(a, esb, i), lane(d, esb, i));
                let v = if !u {
                    sat_signed(sx(dd, esb) as i128 + x as i128, esb, &mut qc)
                } else {
                    sat_unsigned(dd as i128 + sx(x, esb) as i128, esb, &mut qc)
                };
                setlane(&mut r, esb, i, v);
            }
        }
        0b00100 => {
            if size == 3 {
                return Flow::Undef(w);
            }
            for i in 0..n {
                let x = lane(a, esb, i);
                let v = if u {
                    if x == 0 {
                        esb as u64
                    } else {
                        (x.leading_zeros() - (64 - esb)) as u64
                    }
                } else {
                    let s = sx(x, esb);
                    let t = (s ^ (s >> 1)) as u64 & mk(esb) as u64;
                    if t == 0 {
                        esb as u64 - 1
                    } else {
                        (t.leading_zeros() - (64 - esb)) as u64 - 1
                    }
                };
                setlane(&mut r, esb, i, v);
            }
        }
        0b00101 => {
            if size != 0 && !(u && size == 1) {
                return Flow::Undef(w);
            }
            let nb = if q { 16 } else { 8 };
            for i in 0..nb {
                let x = lane(a, 8, i);
                let v = if !u {
                    x.count_ones() as u64
                } else if size == 0 {
                    !x & 0xFF
                } else {
                    (x as u8).reverse_bits() as u64
                };
                setlane(&mut r, 8, i, v);
            }
        }
        0b00111 => {
            if size == 3 && !q {
                return Flow::Undef(w);
            }
            for i in 0..n {
                let s = sx(lane(a, esb, i), esb) as i128;
                let v = sat_signed(if u { -s } else { s.abs() }, esb, &mut qc);
                setlane(&mut r, esb, i, v);
            }
        }
        0b01000..=0b01011 => {
            if size == 3 && !q {
                return Flow::Undef(w);
            }
            for i in 0..n {
                let x = lane(a, esb, i);
                let s = sx(x, esb);
                let t = |b: bool| -> u64 { if b { mk(esb) as u64 } else { 0 } };
                let v = match (opc, u) {
                    (0b01000, false) => t(s > 0),
                    (0b01001, false) => t(s == 0),
                    (0b01010, false) => t(s < 0),
                    (0b01011, false) => (s as i128).abs() as u64,
                    (0b01000, true) => t(s >= 0),
                    (0b01001, true) => t(s <= 0),
                    (0b01011, true) => x.wrapping_neg(),
                    _ => return Flow::Undef(w),
                };
                setlane(&mut r, esb, i, v);
            }
        }
        0b10010 | 0b10100 => {
            // XTN / SQXTN / SQXTUN / UQXTN
            if size == 3 {
                return Flow::Undef(w);
            }
            let wb = esb * 2;
            let nn = 64 / esb as usize;
            let mut nr: V = 0;
            for i in 0..nn {
                let x = lane(a, wb, i);
                let v = match (opc, u) {
                    (0b10010, false) => x & mk(esb) as u64,
                    (0b10100, false) => sat_signed(sx(x, wb) as i128, esb, &mut qc),
                    (0b10010, true) => sat_unsigned(sx(x, wb) as i128, esb, &mut qc),
                    _ => sat_unsigned(x as i128, esb, &mut qc),
                };
                setlane(&mut nr, esb, i, v);
            }
            let res = if q { (d & (u64::MAX as u128)) | (nr << 64) } else { nr & (u64::MAX as u128) };
            wv(c, rd, res, true);
            finish(c, qc);
            return Flow::Next;
        }
        0b10011 => {
            // SHLL / SHLL2
            if !u || size == 3 {
                return Flow::Undef(w);
            }
            let wb = esb * 2;
            let nn = 64 / esb as usize;
            let src = if q { a >> 64 } else { a & (u64::MAX as u128) };
            for i in 0..nn {
                setlane(&mut r, wb, i, lane(src, esb, i) << esb);
            }
            wv(c, rd, r, true);
            return Flow::Next;
        }
        _ => return Flow::Undef(w),
    }
    wv(c, rd, r, q);
    finish(c, qc);
    Flow::Next
}

fn urecpe(x: u32) -> u32 {
    if x & 0x8000_0000 == 0 {
        return 0xFFFF_FFFF;
    }
    let a = ((x >> 23) & 0x1FF) as u64; // 256..511
    let r = recip_estimate(a);
    (((r & 0x1FF) as u32) << 23) & 0xFFFF_FFFF
}

fn ursqrte(x: u32) -> u32 {
    if x & 0xC000_0000 == 0 {
        return 0xFFFF_FFFF;
    }
    let a = ((x >> 23) & 0x1FF) as u64;
    let a = if x & 0x8000_0000 != 0 { a } else { (x >> 23) as u64 & 0x1FF };
    let r = recip_sqrt_estimate(a);
    ((r & 0x1FF) as u32) << 23
}

fn across(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    let a = rv(c, rn);
    let esb = 8u32 << size;
    if matches!(opc, 0b00011 | 0b01010 | 0b11010 | 0b11011) {
        if size == 3 || (size == 2 && !q) {
            return Flow::Undef(w);
        }
        if opc == 0b11011 && u {
            return Flow::Undef(w);
        }
        let n = (if q { 128 } else { 64 }) / esb as usize;
        let lanes: Lanes = (0..n).map(|i| lane(a, esb, i)).collect();
        let (res, rb): (u64, u32) = match opc {
            0b00011 => {
                // SADDLV/UADDLV
                let s: i128 = lanes.iter().map(|&x| if u { x as i128 } else { sx(x, esb) as i128 }).sum();
                (s as u64, esb * 2)
            }
            0b11011 => (lanes.iter().fold(0u64, |acc, &x| acc.wrapping_add(x)), esb),
            0b01010 => {
                let v = if u { *lanes.iter().max().unwrap() } else { lanes.iter().map(|&x| sx(x, esb)).max().unwrap() as u64 };
                (v, esb)
            }
            _ => {
                let v = if u { *lanes.iter().min().unwrap() } else { lanes.iter().map(|&x| sx(x, esb)).min().unwrap() as u64 };
                (v, esb)
            }
        };
        let r = (res as u128) & mk(rb);
        wv(c, rd, r, false);
        return Flow::Next;
    }
    crate::neonfp::across_fp(c, w, q)
}

fn copy(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let imm5 = bits(w, 20, 16);
    let imm4 = bits(w, 14, 11);
    let op = bit(w, 29);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    if imm5 == 0 {
        return Flow::Undef(w);
    }
    let size = imm5.trailing_zeros();
    if size > 3 {
        return Flow::Undef(w);
    }
    let esb = 8u32 << size;
    let idx = (imm5 >> (size + 1)) as usize;
    let xr = |c: &Cpu, r: u32| -> u64 {
        if r == 31 {
            0
        } else {
            c.x[r as usize]
        }
    };
    if op {
        // INS (elemento): solo Q = 1
        if !q {
            return Flow::Undef(w);
        }
        let sidx = (imm4 >> size) as usize;
        if (sidx + 1) * esb as usize > 128 {
            return Flow::Undef(w);
        }
        let mut d = rv(c, rd);
        let x = lane(rv(c, rn), esb, sidx);
        setlane(&mut d, esb, idx, x);
        wv(c, rd, d, true);
        return Flow::Next;
    }
    match imm4 {
        0b0000 | 0b0001 => {
            if size == 3 && !q {
                return Flow::Undef(w);
            }
            let x = if imm4 == 0 { lane(rv(c, rn), esb, idx) } else { xr(c, rn) & mk(esb) as u64 };
            let n = (if q { 128 } else { 64 }) / esb as usize;
            let mut r: V = 0;
            for i in 0..n {
                setlane(&mut r, esb, i, x);
            }
            wv(c, rd, r, q);
        }
        0b0011 => {
            if !q {
                return Flow::Undef(w);
            }
            let mut d = rv(c, rd);
            setlane(&mut d, esb, idx, xr(c, rn));
            wv(c, rd, d, true);
        }
        0b0101 | 0b0111 => {
            let signed = imm4 == 0b0101;
            if signed {
                if (q && size == 3) || (!q && size >= 2) {
                    return Flow::Undef(w);
                }
            } else if (q && size != 3) || (!q && size == 3) {
                return Flow::Undef(w);
            }
            let x = lane(rv(c, rn), esb, idx);
            let v = if signed {
                let s = sx(x, esb) as u64;
                if q {
                    s
                } else {
                    s & 0xFFFF_FFFF
                }
            } else {
                x
            };
            if rd != 31 {
                c.x[rd as usize] = v;
            }
        }
        _ => return Flow::Undef(w),
    }
    Flow::Next
}

fn permute(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let size = bits(w, 23, 22);
    let opc = bits(w, 14, 12);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    if size == 3 && !q {
        return Flow::Undef(w);
    }
    if opc == 0 || opc == 4 {
        return Flow::Undef(w);
    }
    let esb = 8u32 << size;
    let n = (if q { 128 } else { 64 }) / esb as usize;
    let (a, b) = (rv(c, rn), rv(c, rm));
    let mut r: V = 0;
    let h = n / 2;
    for i in 0..n {
        let v = match opc {
            0b001 | 0b101 => {
                let p = (opc >> 2) as usize; // 0 = UZP1 (pares), 1 = UZP2 (impares)
                if i < h {
                    lane(a, esb, 2 * i + p)
                } else {
                    lane(b, esb, 2 * (i - h) + p)
                }
            }
            0b010 | 0b110 => {
                let p = (opc >> 2) as usize;
                let k = i / 2;
                if i % 2 == 0 {
                    lane(a, esb, 2 * k + p)
                } else {
                    lane(b, esb, 2 * k + p)
                }
            }
            0b011 | 0b111 => {
                let base = if opc == 0b111 { h } else { 0 };
                let k = i / 2 + base;
                if i % 2 == 0 {
                    lane(a, esb, k)
                } else {
                    lane(b, esb, k)
                }
            }
            _ => return Flow::Undef(w),
        };
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

fn ext(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let imm4 = bits(w, 14, 11) as usize;
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    if !q && imm4 >= 8 {
        return Flow::Undef(w);
    }
    let (a, b) = (rv(c, rn), rv(c, rm));
    let r = if q {
        if imm4 == 0 {
            a
        } else {
            (a >> (8 * imm4)) | (b << (128 - 8 * imm4))
        }
    } else {
        let lo = a & (u64::MAX as u128);
        let hi = b & (u64::MAX as u128);
        let cat = lo | (hi << 64);
        (cat >> (8 * imm4)) & (u64::MAX as u128)
    };
    wv(c, rd, r, q);
    Flow::Next
}

fn tbl(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if bits(w, 23, 22) != 0 {
        return Flow::Undef(w);
    }
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let len = bits(w, 14, 13) as usize + 1;
    let tbx = bit(w, 12);
    let idx = rv(c, rm);
    let d = rv(c, rd);
    let mut r: V = 0;
    let n = if q { 16 } else { 8 };
    for i in 0..n {
        let x = lane(idx, 8, i) as usize;
        let v = if x < 16 * len {
            let reg = (rn as usize + x / 16) % 32;
            lane(rv(c, reg as u32), 8, x % 16)
        } else if tbx {
            lane(d, 8, i)
        } else {
            0
        };
        setlane(&mut r, 8, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

// Carga/almacenamiento SIMD: ver neonls.rs
pub fn is_simd_ldst(w: u32) -> bool {
    crate::neonls::is_simd_ldst(w)
}
pub fn ldst_valid(w: u32) -> bool {
    crate::neonls::ldst_valid(w)
}
pub fn exec_ldst(c: &mut Cpu, w: u32) -> Flow {
    crate::neonls::exec_ldst(c, w)
}
