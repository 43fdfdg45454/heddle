//! NEON de punto flotante (vectores de f32/f64): tres operandos, dos registros, reducciones.

use crate::cpu::*;
use crate::fp::{bit, bits, with_env};
use crate::interp::Flow;
use crate::neon::*;
use crate::softfp::*;

pub fn two_const(ft: Ft) -> u64 {
    match ft {
        Ft::H => 0x4000,
        Ft::S => 0x4000_0000,
        Ft::D => 0x4000_0000_0000_0000,
    }
}
pub fn three_const(ft: Ft) -> u64 {
    match ft {
        Ft::H => 0x4200,
        Ft::S => 0x4040_0000,
        Ft::D => 0x4008_0000_0000_0000,
    }
}
pub fn half_const(ft: Ft) -> u64 {
    match ft {
        Ft::H => 0x3800,
        Ft::S => 0x3F00_0000,
        Ft::D => 0x3FE0_0000_0000_0000,
    }
}

/// FMULX: como FMUL pero 0 * inf = +-2.0
pub fn fmulx(e: &mut Env, ft: Ft, a: u64, b: u64) -> u64 {
    let (a, b) = (fz_in(e, ft, a), fz_in(e, ft, b));
    if is_nan(ft, a) || is_nan(ft, b) {
        return fmul(e, ft, a, b);
    }
    if (is_inf(ft, a) && is_zero(ft, b)) || (is_zero(ft, a) && is_inf(ft, b)) {
        return two_const(ft) | ((a ^ b) & ft.sign_bit());
    }
    fmul(e, ft, a, b)
}

/// FRECPS: 2 - a*b (fusionado)
pub fn frecps(e: &mut Env, ft: Ft, a: u64, b: u64) -> u64 {
    let (a, b) = (fz_in(e, ft, a), fz_in(e, ft, b));
    let na = fneg(ft, a);
    if is_nan(ft, a) || is_nan(ft, b) {
        return ffma(e, ft, two_const(ft), na, b);
    }
    if (is_inf(ft, a) && is_zero(ft, b)) || (is_zero(ft, a) && is_inf(ft, b)) {
        return two_const(ft);
    }
    ffma(e, ft, two_const(ft), na, b)
}

/// FRSQRTS: (3 - a*b) / 2
pub fn frsqrts(e: &mut Env, ft: Ft, a: u64, b: u64) -> u64 {
    let (a, b) = (fz_in(e, ft, a), fz_in(e, ft, b));
    let na = fneg(ft, a);
    if is_nan(ft, a) || is_nan(ft, b) {
        return ffma(e, ft, three_const(ft), na, b);
    }
    if (is_inf(ft, a) && is_zero(ft, b)) || (is_zero(ft, a) && is_inf(ft, b)) {
        return match ft {
            Ft::H => 0x3E00,
            Ft::S => 0x3FC0_0000,
            Ft::D => 0x3FF8_0000_0000_0000,
        };
    }
    // (3 - a*b)/2 = 1.5 - (a/2)*b con un unico redondeo. La mitad se aplica a un operando
    // normal (exacta, restando 1 al exponente); si ambos son diminutos el producto es despreciable.
    let (sh, mask): (u32, u64) = match ft {
        Ft::H => (10, 0x1F),
        Ft::S => (23, 0xFF),
        Ft::D => (52, 0x7FF),
    };
    let one_half = match ft {
        Ft::H => 0x3E00,
        Ft::S => 0x3FC0_0000,
        Ft::D => 0x3FF8_0000_0000_0000,
    };
    let exp = |x: u64| (x >> sh) & mask;
    let dec = |x: u64| x - (1u64 << sh);
    if is_inf(ft, a) || is_inf(ft, b) {
        ffma(e, ft, one_half, fneg(ft, a), b)
    } else if exp(a) >= 2 {
        ffma(e, ft, one_half, fneg(ft, dec(a)), b)
    } else if exp(b) >= 2 {
        ffma(e, ft, one_half, fneg(ft, a), dec(b))
    } else {
        ffma(e, ft, one_half, fneg(ft, a), b)
    }
}

fn fmul_half(e: &mut Env, ft: Ft, r: u64) -> u64 {
    if is_nan(ft, r) {
        return r;
    }
    fmul(e, ft, r, half_const(ft))
}

/// Comparacion de lanes: kind 0 EQ, 1 GE, 2 GT; `abs` para FACxx. Devuelve true/false.
pub fn fcmp_lane(e: &mut Env, ft: Ft, a: u64, b: u64, kind: u8, abs: bool) -> bool {
    let (a, b) = (fz_in(e, ft, a), fz_in(e, ft, b));
    if is_nan(ft, a) || is_nan(ft, b) {
        if kind != 0 || is_snan(ft, a) || is_snan(ft, b) {
            e.flags |= IOC;
        }
        return false;
    }
    let (mut x, mut y) = (to_f64(ft, a), to_f64(ft, b));
    if abs {
        x = x.abs();
        y = y.abs();
    }
    match kind {
        0 => x == y,
        1 => x >= y,
        _ => x > y,
    }
}

fn ones(ft: Ft) -> u64 {
    ft.mask()
}

pub fn three_same_fp(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let a_bit = bit(w, 23);
    let sz = bit(w, 22);
    let opc = bits(w, 15, 11);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let ft = if sz { Ft::D } else { Ft::S };
    if sz && !q {
        return Flow::Undef(w);
    }
    let esb = ft.bits();
    let n = (if q { 128 } else { 64 }) / esb as usize;
    let (a, b, d) = (rv(c, rn), rv(c, rm), rv(c, rd));
    // validar combinacion
    let valid = match (u, opc) {
        (false, 0b11000) | (false, 0b11001) | (false, 0b11010) | (false, 0b11110) | (false, 0b11111) => true,
        (false, 0b11011) | (false, 0b11100) => !a_bit,
        (true, 0b11000) | (true, 0b11010) | (true, 0b11100) | (true, 0b11101) | (true, 0b11110) => true,
        (true, 0b11011) | (true, 0b11111) => !a_bit,
        _ => false,
    };
    if !valid {
        return crate::neonx::three_same_fhm(c, w, q);
    }
    let pairwise = u && matches!(opc, 0b11000 | 0b11010 | 0b11110) && !(opc == 0b11010 && a_bit);
    let mut r: V = 0;
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for i in 0..n {
            let (x, y) = if pairwise {
                let h = n / 2;
                let (src, j) = if i < h { (a, 2 * i) } else { (b, 2 * (i - h)) };
                (lane(src, esb, j), lane(src, esb, j + 1))
            } else {
                (lane(a, esb, i), lane(b, esb, i))
            };
            let dd = lane(d, esb, i);
            let v = match (u, opc) {
                (false, 0b11000) => fmax(e, ft, x, y, true, !a_bit),
                (true, 0b11000) => fmax(e, ft, x, y, true, !a_bit),
                (false, 0b11001) => {
                    if !a_bit {
                        ffma(e, ft, dd, x, y)
                    } else {
                        ffma(e, ft, dd, fneg(ft, x), y)
                    }
                }
                (false, 0b11010) => {
                    if !a_bit {
                        fadd(e, ft, x, y)
                    } else {
                        fsub(e, ft, x, y)
                    }
                }
                (true, 0b11010) => {
                    if !a_bit {
                        fadd(e, ft, x, y)
                    } else {
                        let t = fsub(e, ft, x, y);
                        fabs(ft, t)
                    }
                }
                (false, 0b11011) => fmulx(e, ft, x, y),
                (true, 0b11011) => fmul(e, ft, x, y),
                (false, 0b11100) => {
                    if fcmp_lane(e, ft, x, y, 0, false) {
                        ones(ft)
                    } else {
                        0
                    }
                }
                (true, 0b11100) => {
                    if fcmp_lane(e, ft, x, y, if a_bit { 2 } else { 1 }, false) {
                        ones(ft)
                    } else {
                        0
                    }
                }
                (true, 0b11101) => {
                    if fcmp_lane(e, ft, x, y, if a_bit { 2 } else { 1 }, true) {
                        ones(ft)
                    } else {
                        0
                    }
                }
                (false, 0b11110) | (true, 0b11110) => fmax(e, ft, x, y, false, !a_bit),
                (false, 0b11111) => {
                    if !a_bit {
                        frecps(e, ft, x, y)
                    } else {
                        frsqrts(e, ft, x, y)
                    }
                }
                (true, 0b11111) => fdiv(e, ft, x, y),
                _ => 0,
            };
            e.drain(0x3F);
            out.push(v);
        }
        out
    });
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

/// Redondea un lane `ft` a entero con modo (0 N, 1 P, 2 M, 3 Z, 4 A)
fn cvt_mode(op_a: bool, base: u32) -> u8 {
    // opcodes 11000/11001/11010/11011: a=0 -> N/M/N/M..., a=1 -> P/Z/P/Z
    let _ = (op_a, base);
    0
}

pub fn two_reg_fp(c: &mut Cpu, w: u32, q: bool) -> Flow {
    two_reg_fp_ft(c, w, q, false)
}

pub fn two_reg_fp_ft(c: &mut Cpu, w: u32, q: bool, h: bool) -> Flow {
    let u = bit(w, 29);
    let a_bit = bit(w, 23);
    let sz = bit(w, 22);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    let _ = cvt_mode(a_bit, 0);
    let src = rv(c, rn);
    let d = rv(c, rd);
    // conversiones de ancho distinto
    if !h && (opc == 0b10110 || opc == 0b10111) {
        return fcvt_widen_narrow(c, w, q, u, sz, opc, rd, src, d);
    }
    let ft = if h { Ft::H } else if sz { Ft::D } else { Ft::S };
    if sz && !q && !h {
        return Flow::Undef(w);
    }
    let esb = ft.bits();
    let n = (if q { 128 } else { 64 }) / esb as usize;
    // validar
    let valid = match (u, opc) {
        (false, 0b01100..=0b01111) => a_bit,
        (true, 0b01100) | (true, 0b01101) | (true, 0b01111) => a_bit,
        (false, 0b11000) | (false, 0b11001) | (false, 0b11010) | (false, 0b11011) => true,
        (false, 0b11100) => !a_bit,
        (false, 0b11101) => true,
        (false, 0b11110) | (false, 0b11111) => !a_bit,
        (true, 0b11000) => !a_bit,
        (true, 0b11001) => true,
        (true, 0b11010) | (true, 0b11011) => true,
        (true, 0b11100) => !a_bit,
        (true, 0b11101) => true,
        (true, 0b11110) => !a_bit,
        (true, 0b11111) => true,
        _ => false,
    };
    let valid = valid && !(h && matches!((u, opc), (false, 0b11110) | (false, 0b11111) | (true, 0b11110) | (true, 0b11111)) && !(u && opc == 0b11111 && a_bit));
    if !valid {
        return Flow::Undef(w);
    }
    // FRINT32Z/FRINT64Z/FRINT32X/FRINT64X: FEAT_FRINTTS
    if matches!(opc, 0b11110 | 0b11111) && !a_bit && !crate::feat::need(crate::feat::F_FRINTTS) {
        return Flow::Undef(w);
    }
    let mut r: V = 0;
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for i in 0..n {
            let x = lane(src, esb, i);
            let zero = 0u64;
            let v = match (u, opc) {
                (false, 0b01100) => if fcmp_lane(e, ft, x, zero, 2, false) { ones(ft) } else { 0 },
                (false, 0b01101) => if fcmp_lane(e, ft, x, zero, 0, false) { ones(ft) } else { 0 },
                (false, 0b01110) => {
                    // FCMLT zero: 0 > x
                    if fcmp_lane(e, ft, zero, x, 2, false) { ones(ft) } else { 0 }
                }
                (false, 0b01111) => fabs(ft, x),
                (true, 0b01100) => if fcmp_lane(e, ft, x, zero, 1, false) { ones(ft) } else { 0 },
                (true, 0b01101) => if fcmp_lane(e, ft, zero, x, 1, false) { ones(ft) } else { 0 },
                (true, 0b01111) => fneg(ft, x),
                (false, 0b11000) => frint(e, ft, x, if a_bit { 1 } else { 0 }, false),
                (false, 0b11001) => frint(e, ft, x, if a_bit { 3 } else { 2 }, false),
                (true, 0b11000) => frint(e, ft, x, 4, false),
                (true, 0b11001) => {
                    let rm = e.rmode();
                    frint(e, ft, x, rm, !a_bit)
                }
                (false, 0b11010) => fcvt_int(e, ft, x, if a_bit { 1 } else { 0 }, true, esb, 0),
                (false, 0b11011) => fcvt_int(e, ft, x, if a_bit { 3 } else { 2 }, true, esb, 0),
                (true, 0b11010) => fcvt_int(e, ft, x, if a_bit { 1 } else { 0 }, false, esb, 0),
                (true, 0b11011) => fcvt_int(e, ft, x, if a_bit { 3 } else { 2 }, false, esb, 0),
                (false, 0b11100) => fcvt_int(e, ft, x, 4, true, esb, 0),
                (true, 0b11100) => fcvt_int(e, ft, x, 4, false, esb, 0),
                (false, 0b11101) => {
                    if !a_bit {
                        int_to_fp(e, ft, if h { sx(x, 16) as u64 } else { x }, true, if h { 64 } else { esb }, 0)
                    } else {
                        { let xx = fz_in(e, ft, x); frecpe(e, ft, xx) }
                    }
                }
                (true, 0b11101) => {
                    if !a_bit {
                        int_to_fp(e, ft, x, false, if h { 64 } else { esb }, 0)
                    } else {
                        { let xx = fz_in(e, ft, x); frsqrte(e, ft, xx) }
                    }
                }
                (true, 0b11111) => {
                    if a_bit {
                        fsqrt(e, ft, x)
                    } else {
                        let rm = e.rmode();
                        frint_n(e, ft, x, rm, 64, true)
                    }
                }
                (false, 0b11110) => frint_n(e, ft, x, 3, 32, true),
                (false, 0b11111) => frint_n(e, ft, x, 3, 64, true),
                (true, 0b11110) => {
                    let rm = e.rmode();
                    frint_n(e, ft, x, rm, 32, true)
                }
                _ => x,
            };
            e.drain(0x3F);
            out.push(v);
        }
        out
    });
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

#[allow(clippy::too_many_arguments)]
fn fcvt_widen_narrow(c: &mut Cpu, w: u32, q: bool, u: bool, sz: bool, opc: u32, rd: u32, src: V, d: V) -> Flow {
    let a_bit = bit(w, 23);
    if a_bit {
        return Flow::Undef(w);
    }
    if opc == 0b10110 && u {
        // FCVTXN: solo D -> S
        if !sz {
            return Flow::Undef(w);
        }
        let res = with_env(c, |e| {
            let mut out = [0u64; 2];
            for i in 0..2 {
                let x = lane(src, 64, i);
                let mut e2 = Env::enter_soft(e.fpcr | (3 << 22));
                let v = convert(&mut e2, Ft::D, Ft::S, x);
                let fl = e2.flags;
                e.flags |= fl & !(IXC);
                let mut v = v;
                if fl & IXC != 0 {
                    e.flags |= IXC;
                    if !is_inf(Ft::S, v) {
                        v |= 1;
                    }
                }
                out[i] = v;
            }
            out
        });
        let mut nr: V = 0;
        setlane(&mut nr, 32, 0, res[0]);
        setlane(&mut nr, 32, 1, res[1]);
        let r = if q { (d & (u64::MAX as u128)) | (nr << 64) } else { nr & (u64::MAX as u128) };
        wv(c, rd, r, true);
        return Flow::Next;
    }
    if u {
        return Flow::Undef(w);
    }
    if opc == 0b10110 {
        // FCVTN: sz=0 S->H (4 lanes), sz=1 D->S (2 lanes)
        let (from, to, nsrc, wsrc, wdst) = if sz { (Ft::D, Ft::S, 2usize, 64u32, 32u32) } else { (Ft::S, Ft::H, 4, 32, 16) };
        let res = with_env(c, |e| (0..nsrc).map(|i| convert(e, from, to, lane(src, wsrc, i))).collect::<crate::neon::Lanes>());
        let mut nr: V = 0;
        for (i, v) in res.into_iter().enumerate() {
            setlane(&mut nr, wdst, i, v);
        }
        let r = if q { (d & (u64::MAX as u128)) | (nr << 64) } else { nr & (u64::MAX as u128) };
        wv(c, rd, r, true);
        Flow::Next
    } else {
        // FCVTL: H->S o S->D; fuente = mitad baja o alta
        let (from, to, nsrc, wsrc, wdst) = if sz { (Ft::S, Ft::D, 2usize, 32u32, 64u32) } else { (Ft::H, Ft::S, 4, 16, 32) };
        let half = if q { src >> 64 } else { src & (u64::MAX as u128) };
        let res = with_env(c, |e| (0..nsrc).map(|i| convert(e, from, to, lane(half, wsrc, i))).collect::<crate::neon::Lanes>());
        let mut r: V = 0;
        for (i, v) in res.into_iter().enumerate() {
            setlane(&mut r, wdst, i, v);
        }
        wv(c, rd, r, true);
        Flow::Next
    }
}

pub fn across_fp(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let a_bit = bit(w, 23);
    let sz = bit(w, 22);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    if !u || sz || !q || !matches!(opc, 0b01100 | 0b01111) {
        return crate::neonx::across_fp16(c, w, q);
    }
    let num = opc == 0b01100;
    let is_max = !a_bit;
    let a = rv(c, rn);
    let lanes: crate::neon::Lanes = (0..4).map(|i| lane(a, 32, i)).collect();
    let r = with_env(c, |e| {
        fn red(e: &mut Env, v: &[u64], num: bool, is_max: bool) -> u64 {
            if v.len() == 1 {
                return v[0];
            }
            let h = v.len() / 2;
            let x = red(e, &v[..h], num, is_max);
            let y = red(e, &v[h..], num, is_max);
            fmax(e, Ft::S, x, y, num, is_max)
        }
        red(e, &lanes, num, is_max)
    });
    wv(c, rd, r as u128, false);
    Flow::Next
}
