//! NEON de media precision (FP16 vectorial) y extension FHM (FMLAL/FMLSL).

use crate::cpu::*;
use crate::fp::{bit, bits, with_env};
use crate::interp::Flow;
use crate::neon::*;
use crate::neonfp::{fcmp_lane, fmulx, frecps, frsqrts};
use crate::softfp::*;

const H: Ft = Ft::H;

/// half -> single por bits, exacto y sin flags (los subnormales half son normales en single).
pub fn h2s_bits(h: u64) -> u64 {
    let sign = ((h >> 15) & 1) as u32;
    let exp = ((h >> 10) & 0x1F) as u32;
    let frac = (h & 0x3FF) as u32;
    let r: u32 = if exp == 0 {
        if frac == 0 {
            sign << 31
        } else {
            let f = (frac as f32) * f32::from_bits(0x3380_0000); // 2^-24
            f.to_bits() | (sign << 31)
        }
    } else if exp == 31 {
        (sign << 31) | 0x7F80_0000 | (frac << 13)
    } else {
        (sign << 31) | ((exp + 112) << 23) | (frac << 13)
    };
    r as u64
}

pub fn three_same_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let a = bit(w, 23);
    if bits(w, 22, 21) != 0b10 || bits(w, 15, 14) != 0 {
        return Flow::Undef(w);
    }
    let opc = bits(w, 13, 11);
    let valid = match (u, a, opc) {
        (false, false, 0) | (false, false, 1) | (false, false, 2) | (false, false, 3) | (false, false, 4) | (false, false, 6) | (false, false, 7) => true,
        (false, true, 0) | (false, true, 1) | (false, true, 2) | (false, true, 6) | (false, true, 7) => true,
        (true, false, 0) | (true, false, 2) | (true, false, 3) | (true, false, 4) | (true, false, 5) | (true, false, 6) | (true, false, 7) => true,
        (true, true, 0) | (true, true, 2) | (true, true, 4) | (true, true, 5) | (true, true, 6) => true,
        _ => false,
    };
    if !valid {
        return Flow::Undef(w);
    }
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let n = if q { 8 } else { 4 };
    let (av, bv, dv) = (rv(c, rn), rv(c, rm), rv(c, rd));
    let pairwise = u && (matches!(opc, 0 | 6) || (opc == 2 && !a));
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for i in 0..n {
            let (x, y) = if pairwise {
                let hh = n / 2;
                let (src, j) = if i < hh { (av, 2 * i) } else { (bv, 2 * (i - hh)) };
                (lane(src, 16, j), lane(src, 16, j + 1))
            } else {
                (lane(av, 16, i), lane(bv, 16, i))
            };
            let dd = lane(dv, 16, i);
            let ones = 0xFFFFu64;
            let t = |b: bool| if b { ones } else { 0 };
            let v = match (u, a, opc) {
                (false, _, 0) | (true, _, 0) => fmax(e, H, x, y, true, !a),
                (false, _, 1) => {
                    if !a {
                        ffma(e, H, dd, x, y)
                    } else {
                        ffma(e, H, dd, fneg(H, x), y)
                    }
                }
                (false, false, 2) | (true, false, 2) => fadd(e, H, x, y),
                (false, true, 2) => fsub(e, H, x, y),
                (true, true, 2) => {
                    let d = fsub(e, H, x, y);
                    fabs(H, d)
                }
                (false, _, 3) => fmulx(e, H, x, y),
                (true, _, 3) => fmul(e, H, x, y),
                (false, _, 4) => t(fcmp_lane(e, H, x, y, 0, false)),
                (true, false, 4) => t(fcmp_lane(e, H, x, y, 1, false)),
                (true, true, 4) => t(fcmp_lane(e, H, x, y, 2, false)),
                (true, false, 5) => t(fcmp_lane(e, H, x, y, 1, true)),
                (true, true, 5) => t(fcmp_lane(e, H, x, y, 2, true)),
                (false, _, 6) | (true, _, 6) => fmax(e, H, x, y, false, !a),
                (false, false, 7) => frecps(e, H, x, y),
                (false, true, 7) => frsqrts(e, H, x, y),
                (true, false, 7) => fdiv(e, H, x, y),
                _ => 0,
            };
            e.drain(0x3F);
            out.push(v);
        }
        out
    });
    let mut r: V = 0;
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, 16, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

pub fn two_reg_misc_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    // 0 Q U 01110 a 1111 00 opcode 10 Rn Rd: el bit 22 es parte del grupo
    if !bit(w, 22) {
        return Flow::Undef(w);
    }
    crate::neonfp::two_reg_fp_ft(c, w, q, true)
}

pub fn across_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let a_bit = bit(w, 23);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    if u || bit(w, 22) || !matches!(opc, 0b01100 | 0b01111) {
        return Flow::Undef(w);
    }
    let num = opc == 0b01100;
    let is_max = !a_bit;
    let a = rv(c, rn);
    let n = if q { 8 } else { 4 };
    let lanes: crate::neon::Lanes = (0..n).map(|i| lane(a, 16, i)).collect();
    let r = with_env(c, |e| {
        fn red(e: &mut Env, v: &[u64], num: bool, is_max: bool) -> u64 {
            if v.len() == 1 {
                return v[0];
            }
            let h = v.len() / 2;
            let x = red(e, &v[..h], num, is_max);
            let y = red(e, &v[h..], num, is_max);
            fmax(e, Ft::H, x, y, num, is_max)
        }
        red(e, &lanes, num, is_max)
    });
    wv(c, rd, r as u128, false);
    Flow::Next
}

/// FMLAL/FMLSL/FMLAL2/FMLSL2 (vectorial)
pub fn three_same_fhm(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let a = bit(w, 23);
    let opc = bits(w, 15, 11);
    let ok = bit(w, 21) && !bit(w, 22) && ((!u && opc == 0b11101) || (u && opc == 0b11001));
    if !ok {
        return Flow::Undef(w);
    }
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    fhm_core(c, q, u, a, rd, rn, rm, None)
}

/// Misma operacion con elemento indexado de Vm (H:L:M).
pub fn fhm_indexed(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let opc = bits(w, 15, 12);
    let sub = opc & 4 != 0;
    let idx = ((bit(w, 11) as usize) << 2) | ((bit(w, 21) as usize) << 1) | bit(w, 20) as usize;
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 19, 16));
    fhm_core(c, q, u, sub, rd, rn, rm, Some(idx))
}

fn fhm_core(c: &mut Cpu, q: bool, is2: bool, sub: bool, rd: u32, rn: u32, rm: u32, idx: Option<usize>) -> Flow {
    let nl = if q { 4 } else { 2 };
    let off = if is2 { nl } else { 0 };
    let (nv, mv, dv) = (rv(c, rn), rv(c, rm), rv(c, rd));
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for i in 0..nl {
            let mut x = lane(nv, 16, off + i);
            if sub {
                x ^= 0x8000;
            }
            let y = match idx {
                Some(k) => lane(mv, 16, k),
                None => lane(mv, 16, off + i),
            };
            let d = lane(dv, 32, i);
            // FPMulAddH: los operandos half pasan por FPUnpack (FZ16 los vacia), el sumando single por FZ
            let (x, y) = (crate::softfp::fz_in(e, Ft::H, x), crate::softfp::fz_in(e, Ft::H, y));
            let v = ffma(e, Ft::S, d, h2s_bits(x), h2s_bits(y));
            e.drain(0x3F);
            out.push(v);
        }
        out
    });
    let mut r: V = 0;
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, 32, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}
