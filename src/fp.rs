//! Punto flotante escalar y despacho de FP/NEON.
//!
//! `FpOp` guarda la palabra de instruccion; la decodificacion fina se hace al ejecutar
//! (estas instrucciones se ejecutan por helper; los caminos calientes se inlinean en el JIT).

use crate::cpu::*;
use crate::decode::Op;
use crate::interp::Flow;
use crate::softfp::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FpOp(pub u32);

pub fn decode_fp(w: u32) -> Op {
    if is_valid(w) {
        Op::Fp(FpOp(w))
    } else {
        Op::Undef(w)
    }
}

#[inline]
pub fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
pub fn bit(w: u32, b: u32) -> bool {
    (w >> b) & 1 != 0
}

/// Valida una palabra sin ejecutarla: se ejecuta contra un Cpu temporal y se mira si dice Undef.
fn is_valid(w: u32) -> bool {
    // Los loads/stores SIMD necesitan memoria; para ellos se valida por patron en `ldst_valid`.
    if crate::neon::is_simd_ldst(w) {
        return crate::neon::ldst_valid(w);
    }
    let mut c = Cpu::blank();
    // direccion segura no hace falta: solo las instrucciones de datos llegan aqui (y sin reservar memoria: el
    // interprete decodifica tambien dentro de un manejador de senal)
    !matches!(exec(&mut c, w), Flow::Undef(_))
}

pub fn exec(c: &mut Cpu, w: u32) -> Flow {
    if crate::neon::is_simd_ldst(w) {
        return crate::neon::exec_ldst(c, w);
    }
    let top = bits(w, 28, 24);
    let r = match top {
        0b11110 => {
            if bit(w, 29) {
                None // SIMD escalar U=1 u otros
            } else if bit(w, 30) {
                None
            } else {
                scalar_fp(c, w)
            }
        }
        0b11111 => {
            if !bit(w, 29) && !bit(w, 30) && !bit(w, 31) {
                fp_dp3(c, w)
            } else {
                None
            }
        }
        _ => None,
    };
    match r {
        Some(f) => f,
        None => crate::neon::exec_simd(c, w),
    }
}

/// Aritmetica escalar de media precision (FEAT_FP16, HWCAP FPHP) en el modelo de CPU.
#[inline]
fn fp16() -> bool {
    crate::feat::need(crate::feat::F_FP16)
}

pub fn ftype(t: u32) -> Option<Ft> {
    match t {
        0 => Some(Ft::S),
        1 => Some(Ft::D),
        3 => Some(Ft::H),
        _ => None,
    }
}

#[inline]
fn getf(c: &Cpu, r: u32, ft: Ft) -> u64 {
    c.v[r as usize][0] & ft.mask()
}
#[inline]
fn setf(c: &mut Cpu, r: u32, v: u64) {
    c.v[r as usize] = [v, 0];
}
#[inline]
fn xr(c: &Cpu, r: u32) -> u64 {
    if r == 31 {
        0
    } else {
        c.x[r as usize]
    }
}
#[inline]
fn xw(c: &mut Cpu, r: u32, v: u64) {
    if r != 31 {
        c.x[r as usize] = v;
    }
}

/// Ejecuta `f` con el entorno FP y acumula las flags en FPSR.
pub fn with_env<R>(c: &mut Cpu, f: impl FnOnce(&mut Env) -> R) -> R {
    let mut env = Env::enter(c.fpcr);
    let r = f(&mut env);
    let fl = env.leave();
    c.fpsr |= fl;
    r
}

pub fn vfp_expand_imm(ft: Ft, imm8: u32) -> u64 {
    let b7 = (imm8 >> 7) & 1;
    let b6 = (imm8 >> 6) & 1;
    let b54 = (imm8 >> 4) & 3;
    let eb = ft.exp_bits();
    let fb = ft.frac_bits();
    let exp: u64 = (((b6 ^ 1) as u64) << (eb - 1)) | (if b6 == 1 { ((1u64 << (eb - 3)) - 1) << 2 } else { 0 }) | b54 as u64;
    let frac = ((imm8 & 0xF) as u64) << (fb - 4);
    ((b7 as u64) << (ft.bits() - 1)) | (exp << fb) | frac
}

fn cond_flags(c: &Cpu, cond: u32) -> bool {
    c.cond_holds(cond as u8)
}

fn scalar_fp(c: &mut Cpu, w: u32) -> Option<Flow> {
    let sf = bit(w, 31);
    let t = bits(w, 23, 22);
    let rn = bits(w, 9, 5);
    let rd = bits(w, 4, 0);
    if bit(w, 21) {
        if bits(w, 15, 10) == 0 {
            return Some(fp_int_conv(c, w, sf, t, rn, rd));
        }
        if sf {
            return Some(Flow::Undef(w));
        }
        let ft = match ftype(t) {
            Some(f) => f,
            None => return Some(Flow::Undef(w)),
        };
        // media precision (FEAT_FP16) salvo FCVT desde H (ARMv8.0: fp_dp1 con opcode 4/5)
        let fcvt_from_h = bits(w, 14, 10) == 0b10000 && matches!(bits(w, 20, 15), 4 | 5);
        if ft == Ft::H && !fcvt_from_h && !fp16() {
            return Some(Flow::Undef(w));
        }
        let rm = bits(w, 20, 16);
        if bits(w, 14, 10) == 0b10000 {
            return Some(fp_dp1(c, w, ft, rn, rd));
        }
        if bits(w, 13, 10) == 0b1000 {
            // FCMP / FCMPE
            if bits(w, 15, 14) != 0 || bits(w, 2, 0) != 0 {
                return Some(Flow::Undef(w));
            }
            let zero = bit(w, 3);
            let sig = bit(w, 4);
            let a = getf(c, rn, ft);
            let b = if zero { 0 } else { getf(c, rm, ft) };
            let nzcv = with_env(c, |e| fcmp(e, ft, a, b, sig));
            c.set_flags(nzcv);
            return Some(Flow::Next);
        }
        if bits(w, 12, 10) == 0b100 {
            // FMOV imm
            if bits(w, 9, 5) != 0 || bits(w, 31, 29) != 0 {
                return Some(Flow::Undef(w));
            }
            let v = vfp_expand_imm(ft, bits(w, 20, 13));
            setf(c, rd, v);
            return Some(Flow::Next);
        }
        match bits(w, 11, 10) {
            0b01 => {
                // FCCMP / FCCMPE
                let cond = bits(w, 15, 12);
                let sig = bit(w, 4);
                let nzcv = bits(w, 3, 0) as u64;
                if cond_flags(c, cond) {
                    let a = getf(c, rn, ft);
                    let b = getf(c, rm, ft);
                    let f = with_env(c, |e| fcmp(e, ft, a, b, sig));
                    c.set_flags(f);
                } else {
                    c.set_flags(nzcv << 28);
                }
                return Some(Flow::Next);
            }
            0b11 => {
                let cond = bits(w, 15, 12);
                let v = if cond_flags(c, cond) { getf(c, rn, ft) } else { getf(c, rm, ft) };
                setf(c, rd, v);
                return Some(Flow::Next);
            }
            0b10 => {
                let op = bits(w, 15, 12);
                let a = getf(c, rn, ft);
                let b = getf(c, rm, ft);
                let r = with_env(c, |e| match op {
                    0 => Some(fmul(e, ft, a, b)),
                    1 => Some(fdiv(e, ft, a, b)),
                    2 => Some(fadd(e, ft, a, b)),
                    3 => Some(fsub(e, ft, a, b)),
                    4 => Some(fmax(e, ft, a, b, false, true)),
                    5 => Some(fmax(e, ft, a, b, false, false)),
                    6 => Some(fmax(e, ft, a, b, true, true)),
                    7 => Some(fmax(e, ft, a, b, true, false)),
                    8 => {
                        let p = fmul(e, ft, a, b);
                        // FNMUL: niega el producto (un NaN tambien cambia de signo)
                        Some(fneg(ft, p))
                    }
                    _ => None,
                });
                return Some(match r {
                    Some(v) => {
                        setf(c, rd, v);
                        Flow::Next
                    }
                    None => Flow::Undef(w),
                });
            }
            _ => {}
        }
        return Some(Flow::Undef(w));
    }
    // bit21 = 0: conversiones de coma fija
    if bits(w, 15, 10) != 0 || true {
        return Some(fixed_conv(c, w, sf, t, rn, rd));
    }
    #[allow(unreachable_code)]
    None
}

fn fp_dp1(c: &mut Cpu, w: u32, ft: Ft, rn: u32, rd: u32) -> Flow {
    if bits(w, 31, 29) != 0 {
        return Flow::Undef(w);
    }
    let op = bits(w, 20, 15);
    let a = getf(c, rn, ft);
    let r: Option<u64> = with_env(c, |e| match op {
        0 => Some(a),
        1 => Some(fabs(ft, a)),
        2 => Some(fneg(ft, a)),
        3 => Some(fsqrt(e, ft, a)),
        4 | 5 | 7 => {
            let to = match op {
                4 => Ft::S,
                5 => Ft::D,
                _ => Ft::H,
            };
            if to == ft {
                None
            } else {
                Some(convert(e, ft, to, a))
            }
        }
        8 => Some(frint(e, ft, a, 0, false)),
        9 => Some(frint(e, ft, a, 1, false)),
        10 => Some(frint(e, ft, a, 2, false)),
        11 => Some(frint(e, ft, a, 3, false)),
        12 => Some(frint(e, ft, a, 4, false)),
        14 => {
            let rm = e.rmode();
            Some(frint(e, ft, a, rm, true))
        }
        15 => {
            let rm = e.rmode();
            Some(frint(e, ft, a, rm, false))
        }
        16..=19 if ft != Ft::H && crate::feat::need(crate::feat::F_FRINTTS) => {
            let nbits = if op < 18 { 32 } else { 64 };
            let mode = if op & 1 == 0 { 3 } else { e.rmode() };
            let exact = true;
            Some(frint_n(e, ft, a, mode, nbits, exact))
        }
        _ => None,
    });
    match r {
        Some(v) => {
            let dest_ft = match op {
                4 => Ft::S,
                5 => Ft::D,
                7 => Ft::H,
                _ => ft,
            };
            setf(c, rd, v & dest_ft.mask());
            Flow::Next
        }
        None => Flow::Undef(w),
    }
}

fn fp_dp3(c: &mut Cpu, w: u32) -> Option<Flow> {
    let ft = match ftype(bits(w, 23, 22)) {
        Some(Ft::H) if !fp16() => return Some(Flow::Undef(w)),
        Some(f) => f,
        None => return Some(Flow::Undef(w)),
    };
    let o1 = bit(w, 21);
    let o0 = bit(w, 15);
    let rm = bits(w, 20, 16);
    let ra = bits(w, 14, 10);
    let rn = bits(w, 9, 5);
    let rd = bits(w, 4, 0);
    let (n, m, a) = (getf(c, rn, ft), getf(c, rm, ft), getf(c, ra, ft));
    let r = with_env(c, |e| match (o1, o0) {
        (false, false) => ffma(e, ft, a, n, m),
        (false, true) => ffma(e, ft, a, fneg(ft, n), m),
        (true, false) => ffma(e, ft, fneg(ft, a), fneg(ft, n), m),
        (true, true) => ffma(e, ft, fneg(ft, a), n, m),
    });
    setf(c, rd, r);
    Some(Flow::Next)
}

fn fp_int_conv(c: &mut Cpu, w: u32, sf: bool, t: u32, rn: u32, rd: u32) -> Flow {
    if bit(w, 29) || bit(w, 30) {
        return Flow::Undef(w);
    }
    let rmode = bits(w, 20, 19);
    let opc = bits(w, 18, 16);
    let width = if sf { 64 } else { 32 };
    // FMOV entre GP y FP
    match (opc, rmode) {
        (6, 0) | (7, 0) => {
            let to_fp = opc == 7;
            let ok = match (sf, t) {
                (false, 0) | (true, 1) => true,
                (false, 3) | (true, 3) => fp16(),
                _ => false,
            };
            if !ok {
                return Flow::Undef(w);
            }
            if to_fp {
                let v = xr(c, rn);
                let m = match t {
                    0 => 0xFFFF_FFFF,
                    3 => 0xFFFF,
                    _ => u64::MAX,
                };
                setf(c, rd, v & m);
            } else {
                let m = match t {
                    0 => 0xFFFF_FFFF,
                    3 => 0xFFFF,
                    _ => u64::MAX,
                };
                let v = c.v[rn as usize][0] & m;
                xw(c, rd, v);
            }
            return Flow::Next;
        }
        (6, 1) | (7, 1) => {
            // FMOV Xd, Vn.D[1] / FMOV Vd.D[1], Xn
            if !(sf && t == 2) {
                return Flow::Undef(w);
            }
            if opc == 7 {
                let v = xr(c, rn);
                c.v[rd as usize][1] = v;
            } else {
                let v = c.v[rn as usize][1];
                xw(c, rd, v);
            }
            return Flow::Next;
        }
        (6, 3) => {
            // FJCVTZS
            if sf || t != 1 || !crate::feat::need(crate::feat::F_JSCVT) {
                return Flow::Undef(w);
            }
            let a = c.v[rn as usize][0];
            let (r, z) = with_env(c, |e| fjcvtzs(e, a));
            xw(c, rd, r as u64);
            c.set_flags(if z { 0x4000_0000 } else { 0 });
            return Flow::Next;
        }
        _ => {}
    }
    let ft = match ftype(t) {
        Some(Ft::H) if !fp16() => return Flow::Undef(w),
        Some(f) => f,
        None => return Flow::Undef(w),
    };
    match opc {
        2 | 3 if rmode == 0 => {
            // SCVTF / UCVTF (registro entero -> fp)
            let v = xr(c, rn);
            let signed = opc == 2;
            let r = with_env(c, |e| int_to_fp(e, ft, v, signed, width, 0));
            setf(c, rd, r);
            Flow::Next
        }
        0 | 1 | 4 | 5 => {
            // FCVT*: rmode 00 N, 01 P, 10 M, 11 Z; opc 4/5 solo con rmode 00 (A)
            let (rm, signed) = match (opc, rmode) {
                (0, m) | (1, m) => (m as u8, opc == 0),
                (4, 0) => (4, true),
                (5, 0) => (4, false),
                _ => return Flow::Undef(w),
            };
            // mapeo rmode ARM -> modo de redondeo del helper (0 N, 1 P, 2 M, 3 Z, 4 A)
            let a = getf(c, rn, ft);
            let r = with_env(c, |e| fcvt_int(e, ft, a, rm, signed, width, 0));
            xw(c, rd, r);
            Flow::Next
        }
        _ => Flow::Undef(w),
    }
}

fn fixed_conv(c: &mut Cpu, w: u32, sf: bool, t: u32, rn: u32, rd: u32) -> Flow {
    if bit(w, 29) || bit(w, 30) || bits(w, 28, 24) != 0b11110 {
        return Flow::Undef(w);
    }
    let ft = match ftype(t) {
        Some(Ft::H) if !fp16() => return Flow::Undef(w),
        Some(f) => f,
        None => return Flow::Undef(w),
    };
    let rmode = bits(w, 20, 19);
    let opc = bits(w, 18, 16);
    let scale = bits(w, 15, 10);
    if !sf && scale < 32 {
        return Flow::Undef(w);
    }
    let fbits = 64 - scale;
    let width = if sf { 64 } else { 32 };
    match (rmode, opc) {
        (0, 2) | (0, 3) => {
            let v = xr(c, rn);
            let signed = opc == 2;
            let r = with_env(c, |e| int_to_fp(e, ft, v, signed, width, fbits));
            setf(c, rd, r);
            Flow::Next
        }
        (3, 0) | (3, 1) => {
            let a = getf(c, rn, ft);
            let signed = opc == 0;
            let r = with_env(c, |e| fcvt_int(e, ft, a, 3, signed, width, fbits));
            xw(c, rd, r);
            Flow::Next
        }
        _ => Flow::Undef(w),
    }
}

/// FJCVTZS: conversion a entero de 32 bits con semantica JavaScript. Devuelve (valor, Z).
fn fjcvtzs(e: &mut Env, a: u64) -> (u32, bool) {
    let ft = Ft::D;
    let a = crate::softfp::fz_in(e, ft, a);
    if is_nan(ft, a) || is_inf(ft, a) {
        e.flags |= IOC;
        return (0, false);
    }
    let x = f64::from_bits(a);
    let t = rnd(e, x, 3);
    let exact = t == x;
    let m = t.rem_euclid(4294967296.0);
    let v = m as u64 as u32;
    e.drain(0);
    let in_range = t >= -2147483648.0 && t <= 2147483647.0;
    if !in_range {
        e.flags |= IOC;
        return (v, false);
    }
    if !exact {
        e.flags |= IXC;
    }
    // -0.0 no se considera exacto (Z = 0)
    (v, exact && !(x == 0.0 && x.is_sign_negative()))
}
