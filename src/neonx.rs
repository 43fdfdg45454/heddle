//! NEON: inmediatos modificados, desplazamientos con inmediato, elemento indexado, SDOT/UDOT,
//! FCMLA/FCADD, SQRDMLAH/SQRDMLSH y todas las formas escalares ("SIMD escalar").

use crate::cpu::*;
use crate::feat::{need, F_ASIMDHP, F_DOTPROD, F_FCMA, F_FHM, F_RDM};
use crate::fp::{bit, bits, vfp_expand_imm, with_env};
use crate::interp::Flow;
use crate::neon::*;
use crate::softfp::*;

fn rep(x: u64, esb: u32) -> u64 {
    let mut r = 0u64;
    let mut i = 0;
    while i < 64 {
        r |= (x & mk(esb) as u64) << i;
        i += esb;
    }
    r
}

fn finish_qc(c: &mut Cpu, qc: bool) {
    if qc {
        c.fpsr |= 1 << 27;
    }
}

fn ft_of(esb: u32) -> Ft {
    match esb {
        16 => Ft::H,
        32 => Ft::S,
        _ => Ft::D,
    }
}

// ------------------------------------------------------------------------------------------
// MOVI / MVNI / ORR / BIC / FMOV (inmediato vectorial)
// ------------------------------------------------------------------------------------------
/// Expande el inmediato modificado de MOVI/MVNI/ORR/BIC/FMOV (vector): (imm de 64 bits ya invertido si procede, es
/// ORR/BIC, bit `op`). None = codificacion no definida. La usan el interprete y el JIT (src/jitneon.rs).
pub fn mod_imm_expand(w: u32, q: bool) -> Option<(u64, bool, bool)> {
    let op = bit(w, 29);
    let cmode = bits(w, 15, 12);
    let o2 = bit(w, 11);
    let imm8 = (bits(w, 18, 16) << 5) | bits(w, 9, 5);
    if o2 && !(cmode == 0b1111 && !op) {
        return None;
    }
    let i = imm8 as u64;
    let logic = (cmode & 1 == 1) && (cmode >> 2) != 0b11; // ORR / BIC
    let mut invert = false;
    let imm: u64 = match cmode >> 1 {
        0b000 => rep(i, 32),
        0b001 => rep(i << 8, 32),
        0b010 => rep(i << 16, 32),
        0b011 => rep(i << 24, 32),
        0b100 => rep(i, 16),
        0b101 => rep(i << 8, 16),
        0b110 => {
            if cmode & 1 == 0 {
                rep((i << 8) | 0xFF, 32)
            } else {
                rep((i << 16) | 0xFFFF, 32)
            }
        }
        _ => {
            if cmode & 1 == 0 {
                if !op {
                    rep(i, 8)
                } else {
                    let mut r = 0u64;
                    for b in 0..8 {
                        if (i >> b) & 1 == 1 {
                            r |= 0xFFu64 << (8 * b);
                        }
                    }
                    r
                }
            } else if !op {
                if o2 {
                    // FMOV (vector, media precision): FEAT_FP16
                    if !need(F_ASIMDHP) {
                        return None;
                    }
                    rep(vfp_expand_imm(Ft::H, imm8), 16)
                } else {
                    rep(vfp_expand_imm(Ft::S, imm8), 32)
                }
            } else {
                if !q {
                    return None;
                }
                vfp_expand_imm(Ft::D, imm8)
            }
        }
    };
    if (cmode >> 1) != 0b111 && op && !logic {
        invert = true;
    }
    Some((if invert { !imm } else { imm }, logic, op))
}

pub fn mod_imm(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let rd = bits(w, 4, 0);
    let (imm, logic, op) = match mod_imm_expand(w, q) {
        Some(x) => x,
        None => return Flow::Undef(w),
    };
    let imm128 = imm as u128 | ((imm as u128) << 64);
    let d = rv(c, rd);
    let r = if logic {
        if !op {
            d | imm128
        } else {
            d & !imm128
        }
    } else {
        imm128
    };
    wv(c, rd, r, q);
    Flow::Next
}

// ------------------------------------------------------------------------------------------
// Desplazamientos con inmediato
// ------------------------------------------------------------------------------------------
pub fn shift_imm(c: &mut Cpu, w: u32, q: bool, s: bool) -> Flow {
    let u = bit(w, 29);
    let immh = bits(w, 22, 19);
    let immb = bits(w, 18, 16);
    let opc = bits(w, 15, 11);
    let (rn, rd) = (bits(w, 9, 5), bits(w, 4, 0));
    if immh == 0 {
        return Flow::Undef(w);
    }
    let hs = 31 - immh.leading_zeros();
    let immhb = ((immh << 3) | immb) as i64;
    let src = rv(c, rn);
    let dst = rv(c, rd);
    let mut qc = false;
    match opc {
        0b00000 | 0b00010 | 0b00100 | 0b00110 | 0b01000 | 0b01010 | 0b01100 | 0b01110 => {
            if (opc == 0b01000 || opc == 0b01100) && !u {
                return Flow::Undef(w);
            }
            let esb = 8u32 << hs;
            if hs == 3 && !q && !s {
                return Flow::Undef(w);
            }
            if s && hs != 3 && !matches!(opc, 0b01100 | 0b01110) {
                return Flow::Undef(w);
            }
            let n = if s { 1 } else { (if q { 128 } else { 64 }) / esb as usize };
            let rsh = 2 * esb as i64 - immhb;
            let lsh = immhb - esb as i64;
            let m = mk(esb) as u64;
            let mut r: V = 0;
            for i in 0..n {
                let a = lane(src, esb, i);
                let d = lane(dst, esb, i);
                let av: i128 = if u { a as i128 } else { sx(a, esb) as i128 };
                let v: u64 = match opc {
                    0b00000 => (av >> rsh) as u64,
                    0b00010 => d.wrapping_add((av >> rsh) as u64),
                    0b00100 => ((av + (1i128 << (rsh - 1))) >> rsh) as u64,
                    0b00110 => d.wrapping_add(((av + (1i128 << (rsh - 1))) >> rsh) as u64),
                    0b01000 => {
                        // SRI
                        let mask = if rsh as u32 >= esb { 0 } else { m >> rsh };
                        (d & !mask) | (((a as u128) >> rsh) as u64 & mask)
                    }
                    0b01010 => {
                        if !u {
                            ((a as u128) << lsh) as u64
                        } else {
                            let mask = (m << lsh) & m;
                            (d & !mask) | (((a as u128) << lsh) as u64 & mask)
                        }
                    }
                    0b01100 => sat_unsigned(sx(a, esb) as i128 * (1i128 << lsh), esb, &mut qc),
                    _ => {
                        if u {
                            sat_unsigned(a as i128 * (1i128 << lsh), esb, &mut qc)
                        } else {
                            sat_signed(sx(a, esb) as i128 * (1i128 << lsh), esb, &mut qc)
                        }
                    }
                };
                setlane(&mut r, esb, i, v);
            }
            wv(c, rd, r, q && !s);
        }
        0b10000..=0b10011 => {
            if hs == 3 {
                return Flow::Undef(w);
            }
            if s && !(u && opc <= 0b10001) && !(opc >= 0b10010) {
                return Flow::Undef(w);
            }
            let d_esb = 8u32 << hs;
            let s_esb = d_esb * 2;
            let n = if s { 1 } else { 64 / d_esb as usize };
            let rsh = 2 * d_esb as i64 - immhb;
            let round = opc & 1 == 1;
            let mut r: V = 0;
            for i in 0..n {
                let a = lane(src, s_esb, i);
                let av: i128 = if (opc >= 0b10010 && u) || false { a as i128 } else { sx(a, s_esb) as i128 };
                let t = if round { (av + (1i128 << (rsh - 1))) >> rsh } else { av >> rsh };
                let v = match (opc >> 1, u) {
                    (0b1000, false) => t as u64,
                    (0b1000, true) => sat_unsigned(t, d_esb, &mut qc),
                    (_, false) => sat_signed(t, d_esb, &mut qc),
                    (_, true) => sat_unsigned(t, d_esb, &mut qc),
                };
                setlane(&mut r, d_esb, i, v);
            }
            if s {
                wv(c, rd, r, false);
            } else if q {
                let lo = dst & (u64::MAX as u128);
                wv(c, rd, lo | (r << 64), true);
            } else {
                wv(c, rd, r, false);
            }
        }
        0b10100 => {
            if hs == 3 || s {
                return Flow::Undef(w);
            }
            let esb = 8u32 << hs;
            let n = 64 / esb as usize;
            let lsh = immhb - esb as i64;
            let half = if q { src >> 64 } else { src & (u64::MAX as u128) };
            let mut r: V = 0;
            for i in 0..n {
                let a = lane(half, esb, i);
                let av: i128 = if u { a as i128 } else { sx(a, esb) as i128 };
                setlane(&mut r, 2 * esb, i, (av << lsh) as u64);
            }
            wv(c, rd, r, true);
        }
        0b11100 | 0b11111 => {
            // conversiones de punto fijo
            let esb = match hs {
                1 if need(F_ASIMDHP) => 16,
                2 => 32,
                3 => 64,
                _ => return Flow::Undef(w),
            };
            if esb == 64 && !q && !s {
                return Flow::Undef(w);
            }
            let ft = ft_of(esb);
            let n = if s { 1 } else { (if q { 128 } else { 64 }) / esb as usize };
            let fbits = (2 * esb as i64 - immhb) as u32;
            let mut r: V = 0;
            let res = with_env(c, |e| {
                let mut out = crate::neon::Lanes::new();
                for i in 0..n {
                    let a = lane(src, esb, i);
                    let v = if opc == 0b11100 {
                        let iv = if u { a } else { sx(a, esb) as u64 };
                        int_to_fp(e, ft, iv, !u, 64, fbits)
                    } else {
                        fcvt_int(e, ft, a, 3, !u, esb, fbits)
                    };
                    e.drain(0x3F);
                    out.push(v);
                }
                out
            });
            for (i, v) in res.into_iter().enumerate() {
                setlane(&mut r, esb, i, v);
            }
            wv(c, rd, r, q && !s);
        }
        _ => return Flow::Undef(w),
    }
    finish_qc(c, qc);
    Flow::Next
}

// ------------------------------------------------------------------------------------------
// Elemento indexado
// ------------------------------------------------------------------------------------------
pub fn indexed(c: &mut Cpu, w: u32, q: bool, s: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let l = bit(w, 21) as usize;
    let m = bit(w, 20) as usize;
    let rm4 = bits(w, 19, 16);
    let opc = bits(w, 15, 12);
    let h = bit(w, 11) as usize;
    let (rn, rd) = (bits(w, 9, 5), bits(w, 4, 0));
    if matches!((u, opc), (false, 0b0000) | (false, 0b0100) | (true, 0b1000) | (true, 0b1100)) {
        if s || size != 2 || !need(F_FHM) {
            return Flow::Undef(w);
        }
        return crate::neonh::fhm_indexed(c, w, q);
    }
    let fp_op = matches!((u, opc), (false, 0b0001) | (false, 0b0101) | (false, 0b1001) | (true, 0b1001));
    let fcmla = u && matches!(opc, 0b0001 | 0b0011 | 0b0101 | 0b0111);
    if fp_op || fcmla {
        // FCMLA: FEAT_FCMA; media precision (FCMLA size=01, FMLA/FMUL... size=00): FEAT_FP16
        let hp = if size == if fcmla { 1 } else { 0 } { F_ASIMDHP } else { 0 };
        if !need(hp | if fcmla { F_FCMA } else { 0 }) {
            return Flow::Undef(w);
        }
        return indexed_fp(c, w, q, s, u, size, l, m, rm4, opc, h, rn, rd, fcmla);
    }
    // SQRDMLAH / SQRDMLSH (por elemento, vectorial y escalar): FEAT_RDM
    if u && matches!(opc, 0b1101 | 0b1111) && !need(F_RDM) {
        return Flow::Undef(w);
    }
    if s && !matches!((u, opc), (false, 0b0011) | (false, 0b0111) | (false, 0b1011) | (false, 0b1100) | (false, 0b1101) | (true, 0b1101) | (true, 0b1111)) {
        return Flow::Undef(w);
    }
    // SDOT / UDOT
    if opc == 0b1110 {
        if s || size != 2 || !need(F_DOTPROD) {
            return Flow::Undef(w);
        }
        let idx = (h << 1) | l;
        let rm = (m << 4) | rm4 as usize;
        let (a, b, d) = (rv(c, rn), rv(c, rd), rv(c, rm as u32));
        let _ = b;
        let grp = (d >> (32 * idx)) as u32;
        let a_ = rv(c, rn);
        let acc = rv(c, rd);
        let nl = if q { 4 } else { 2 };
        let mut r: V = 0;
        for i in 0..nl {
            let mut sum: i64 = lane(acc, 32, i) as i64;
            for j in 0..4 {
                let x = lane(a_, 8, 4 * i + j);
                let y = ((grp >> (8 * j)) & 0xFF) as u64;
                sum += if u { (x * y) as i64 } else { sx(x, 8) * sx(y, 8) };
            }
            setlane(&mut r, 32, i, sum as u64);
        }
        let _ = a;
        wv(c, rd, r, q);
        return Flow::Next;
    }
    let (esb, idx, rm) = match size {
        1 => (16u32, (h << 2) | (l << 1) | m, rm4 as usize),
        2 => (32u32, (h << 1) | l, (m << 4) | rm4 as usize),
        _ => return Flow::Undef(w),
    };
    let valid = matches!(
        (u, opc),
        (true, 0b0000) | (true, 0b0100) | (false, 0b1000) | (false, 0b0010) | (true, 0b0010) | (false, 0b0110) | (true, 0b0110) | (false, 0b1010) | (true, 0b1010)
            | (false, 0b0011) | (false, 0b0111) | (false, 0b1011) | (false, 0b1100) | (false, 0b1101) | (true, 0b1101) | (true, 0b1111)
    );
    if !valid {
        return Flow::Undef(w);
    }
    let a = rv(c, rn);
    let d = rv(c, rd);
    let y = lane(rv(c, rm as u32), esb, idx);
    let mut qc = false;
    let long = matches!(opc, 0b0010 | 0b0110 | 0b1010 | 0b0011 | 0b0111 | 0b1011);
    let mut r: V = 0;
    if long {
        let wb = esb * 2;
        let n = if s { 1 } else { 64 / esb as usize };
        let half = if q && !s { a >> 64 } else { a & (u64::MAX as u128) };
        let sgn = !u;
        for i in 0..n {
            let x = lane(half, esb, i);
            let (xv, yv): (i128, i128) = if sgn { (sx(x, esb) as i128, sx(y, esb) as i128) } else { (x as i128, y as i128) };
            let dd = lane(d, wb, i);
            let ddv: i128 = if sgn { sx(dd, wb) as i128 } else { dd as i128 };
            let v = match opc {
                0b0010 => dd.wrapping_add((xv * yv) as u64),
                0b0110 => dd.wrapping_sub((xv * yv) as u64),
                0b1010 => (xv * yv) as u64,
                0b1011 => sat_signed(2 * xv * yv, wb, &mut qc),
                0b0011 => {
                    let p = sat_signed(2 * xv * yv, wb, &mut qc);
                    sat_signed(ddv + sx(p, wb) as i128, wb, &mut qc)
                }
                _ => {
                    let p = sat_signed(2 * xv * yv, wb, &mut qc);
                    sat_signed(ddv - sx(p, wb) as i128, wb, &mut qc)
                }
            };
            setlane(&mut r, wb, i, v);
        }
        wv(c, rd, r, true);
    } else {
        let n = if s { 1 } else { (if q { 128 } else { 64 }) / esb as usize };
        for i in 0..n {
            let x = lane(a, esb, i);
            let dd = lane(d, esb, i);
            let (xs, ys, ds) = (sx(x, esb) as i128, sx(y, esb) as i128, sx(dd, esb) as i128);
            let v = match (u, opc) {
                (true, 0b0000) => dd.wrapping_add(x.wrapping_mul(y)),
                (true, 0b0100) => dd.wrapping_sub(x.wrapping_mul(y)),
                (false, 0b1000) => x.wrapping_mul(y),
                (false, 0b1100) => sat_signed((2 * xs * ys) >> esb, esb, &mut qc),
                (false, 0b1101) => sat_signed((2 * xs * ys + (1i128 << (esb - 1))) >> esb, esb, &mut qc),
                (true, 0b1101) => sat_signed(((ds << esb) + 2 * xs * ys + (1i128 << (esb - 1))) >> esb, esb, &mut qc),
                _ => sat_signed(((ds << esb) - 2 * xs * ys + (1i128 << (esb - 1))) >> esb, esb, &mut qc),
            };
            setlane(&mut r, esb, i, v);
        }
        wv(c, rd, r, q && !s);
    }
    finish_qc(c, qc);
    Flow::Next
}

fn indexed_fp(c: &mut Cpu, w: u32, q: bool, s: bool, u: bool, size: u32, l: usize, m: usize, rm4: u32, opc: u32, h: usize, rn: u32, rd: u32, fcmla: bool) -> Flow {
    if fcmla {
        if s {
            return Flow::Undef(w);
        }
        let (esb, idx, rm) = match size {
            // con Q=0 solo hay dos pares complejos: H=1 se sale del vector
            1 if h == 1 && !q => return Flow::Undef(w),
            1 => (16u32, (h << 1) | l, (m << 4) | rm4 as usize),
            2 => {
                if l != 0 || !q {
                    return Flow::Undef(w);
                }
                (32u32, h, (m << 4) | rm4 as usize)
            }
            _ => return Flow::Undef(w),
        };
        let rot = (opc >> 1) & 3;
        let ft = ft_of(esb);
        let (a, d, mvv) = (rv(c, rn), rv(c, rd), rv(c, rm as u32));
        let npairs = (if q { 128 } else { 64 }) / (2 * esb as usize);
        let (m_re, m_im) = (lane(mvv, esb, 2 * idx), lane(mvv, esb, 2 * idx + 1));
        let mut r: V = 0;
        let res = with_env(c, |e| {
            let mut out = crate::neon::Lanes::new();
            for p in 0..npairs {
                let (n_re, n_im) = (lane(a, esb, 2 * p), lane(a, esb, 2 * p + 1));
                let (d_re, d_im) = (lane(d, esb, 2 * p), lane(d, esb, 2 * p + 1));
                let (e1, e2, e3, e4) = match rot {
                    0 => (n_re, m_re, n_re, m_im),
                    1 => (n_im, fneg(ft, m_im), n_im, m_re),
                    2 => (n_re, fneg(ft, m_re), n_re, fneg(ft, m_im)),
                    _ => (n_im, m_im, n_im, fneg(ft, m_re)),
                };
                let re = ffma(e, ft, d_re, e1, e2);
                e.drain(0x3F);
                let im = ffma(e, ft, d_im, e3, e4);
                e.drain(0x3F);
                out.push(re);
                out.push(im);
            }
            out
        });
        for (i, v) in res.into_iter().enumerate() {
            setlane(&mut r, esb, i, v);
        }
        wv(c, rd, r, q);
        return Flow::Next;
    }
    let (esb, idx, rm) = match size {
        0 => (16u32, (h << 2) | (l << 1) | m, rm4 as usize),
        2 => (32u32, (h << 1) | l, (m << 4) | rm4 as usize),
        3 => {
            if l != 0 {
                return Flow::Undef(w);
            }
            (64u32, h, (m << 4) | rm4 as usize)
        }
        _ => return Flow::Undef(w),
    };
    if esb == 64 && !q && !s {
        return Flow::Undef(w);
    }
    let ft = ft_of(esb);
    let (a, d, mvv) = (rv(c, rn), rv(c, rd), rv(c, rm as u32));
    let y = lane(mvv, esb, idx);
    let n = if s { 1 } else { (if q { 128 } else { 64 }) / esb as usize };
    let mut r: V = 0;
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for i in 0..n {
            let x = lane(a, esb, i);
            let dd = lane(d, esb, i);
            let v = match (u, opc) {
                (false, 0b0001) => ffma(e, ft, dd, x, y),
                (false, 0b0101) => ffma(e, ft, dd, fneg(ft, x), y),
                (false, 0b1001) => fmul(e, ft, x, y),
                _ => crate::neonfp::fmulx(e, ft, x, y),
            };
            e.drain(0x3F);
            out.push(v);
        }
        out
    });
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q && !s);
    Flow::Next
}

// ------------------------------------------------------------------------------------------
// Tres registros "extra": SQRDMLAH/SQRDMLSH, SDOT/UDOT, FCMLA, FCADD
// ------------------------------------------------------------------------------------------
pub fn three_reg_ext(c: &mut Cpu, w: u32, q: bool) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 14, 11);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let (a, b, d) = (rv(c, rn), rv(c, rm), rv(c, rd));
    let mut qc = false;
    // RDM (SQRDMLAH/SQRDMLSH), DotProd (SDOT/UDOT), FCMA (FCMLA/FCADD; en media precision ademas FP16)
    let ext = match (u, opc) {
        (true, 0b0000) | (true, 0b0001) => F_RDM,
        (_, 0b0010) => F_DOTPROD,
        (true, _) if size == 1 => F_FCMA | F_ASIMDHP,
        (true, _) => F_FCMA,
        _ => 0,
    };
    if !need(ext) {
        return Flow::Undef(w);
    }
    match (u, opc) {
        (true, 0b0000) | (true, 0b0001) => {
            if size != 1 && size != 2 {
                return Flow::Undef(w);
            }
            let esb = 8u32 << size;
            let n = (if q { 128 } else { 64 }) / esb as usize;
            let mut r: V = 0;
            for i in 0..n {
                let (x, y, dd) = (sx(lane(a, esb, i), esb) as i128, sx(lane(b, esb, i), esb) as i128, sx(lane(d, esb, i), esb) as i128);
                let p = 2 * x * y;
                let t = if opc == 0 { (dd << esb) + p } else { (dd << esb) - p };
                setlane(&mut r, esb, i, sat_signed((t + (1i128 << (esb - 1))) >> esb, esb, &mut qc));
            }
            wv(c, rd, r, q);
            finish_qc(c, qc);
            Flow::Next
        }
        (_, 0b0010) => {
            if size != 2 {
                return Flow::Undef(w);
            }
            let nl = if q { 4 } else { 2 };
            let mut r: V = 0;
            for i in 0..nl {
                let mut sum: i64 = lane(d, 32, i) as i64;
                for j in 0..4 {
                    let (x, y) = (lane(a, 8, 4 * i + j), lane(b, 8, 4 * i + j));
                    sum += if u { (x * y) as i64 } else { sx(x, 8) * sx(y, 8) };
                }
                setlane(&mut r, 32, i, sum as u64);
            }
            wv(c, rd, r, q);
            Flow::Next
        }
        (true, o) if (o >> 2) == 0b10 || (o & 0b1001) == 0b1000 && false => {
            let _ = o;
            fcmla_vec(c, w, q, size, opc & 3, rd, a, b, d)
        }
        (true, o) if (o & 0b1101) == 0b1100 => fcadd_vec(c, w, q, size, (opc >> 1) & 1, rd, a, b),
        _ => Flow::Undef(w),
    }
}

fn fcmla_vec(c: &mut Cpu, w: u32, q: bool, size: u32, rot: u32, rd: u32, a: V, b: V, d: V) -> Flow {
    if size == 0 || (size == 3 && !q) {
        return Flow::Undef(w);
    }
    let esb = 8u32 << size;
    let ft = ft_of(esb);
    let npairs = (if q { 128 } else { 64 }) / (2 * esb as usize);
    let mut r: V = 0;
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for p in 0..npairs {
            let (n_re, n_im) = (lane(a, esb, 2 * p), lane(a, esb, 2 * p + 1));
            let (m_re, m_im) = (lane(b, esb, 2 * p), lane(b, esb, 2 * p + 1));
            let (d_re, d_im) = (lane(d, esb, 2 * p), lane(d, esb, 2 * p + 1));
            let (e1, e2, e3, e4) = match rot {
                0 => (n_re, m_re, n_re, m_im),
                1 => (n_im, fneg(ft, m_im), n_im, m_re),
                2 => (n_re, fneg(ft, m_re), n_re, fneg(ft, m_im)),
                _ => (n_im, m_im, n_im, fneg(ft, m_re)),
            };
            let re = ffma(e, ft, d_re, e1, e2);
            e.drain(0x3F);
            let im = ffma(e, ft, d_im, e3, e4);
            e.drain(0x3F);
            out.push(re);
            out.push(im);
        }
        out
    });
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

fn fcadd_vec(c: &mut Cpu, w: u32, q: bool, size: u32, rot: u32, rd: u32, a: V, b: V) -> Flow {
    if size == 0 || (size == 3 && !q) {
        return Flow::Undef(w);
    }
    let esb = 8u32 << size;
    let ft = ft_of(esb);
    let npairs = (if q { 128 } else { 64 }) / (2 * esb as usize);
    let mut r: V = 0;
    let res = with_env(c, |e| {
        let mut out = crate::neon::Lanes::new();
        for p in 0..npairs {
            let (n_re, n_im) = (lane(a, esb, 2 * p), lane(a, esb, 2 * p + 1));
            let (m_re, m_im) = (lane(b, esb, 2 * p), lane(b, esb, 2 * p + 1));
            let (re, im) = if rot == 0 {
                (fadd(e, ft, n_re, fneg(ft, m_im)), fadd(e, ft, n_im, m_re))
            } else {
                (fadd(e, ft, n_re, m_im), fadd(e, ft, n_im, fneg(ft, m_re)))
            };
            e.drain(0x3F);
            out.push(re);
            out.push(im);
        }
        out
    });
    for (i, v) in res.into_iter().enumerate() {
        setlane(&mut r, esb, i, v);
    }
    wv(c, rd, r, q);
    Flow::Next
}

// ------------------------------------------------------------------------------------------
// SIMD escalar
// ------------------------------------------------------------------------------------------

/// Ejecuta la forma vectorial equivalente de una instruccion escalar. Los operandos se
/// replican (lane 0) en todos los lanes, asi flags y QC coinciden con los del lane 0.
fn bcast_run(c: &mut Cpu, w: u32, src_b: u32, dst_b: u32, q: bool) -> Flow {
    let has_rm = !(bit(w, 21) && bits(w, 11, 10) == 0b10);
    // Registros de trabajo: se guardan, se usan y se restauran (sin aliasing con rn/rm/rd).
    const TN: usize = 29;
    const TM: usize = 30;
    const TD: usize = 28;
    let (rd, rn, rm) = (bits(w, 4, 0) as usize, bits(w, 9, 5) as usize, bits(w, 20, 16) as usize);
    let (sn, sm, sd) = (c.v[rn], c.v[rm], c.v[rd]);
    let saved = [c.v[TN], c.v[TM], c.v[TD]];
    let bc = |v: [u64; 2], b: u32| -> [u64; 2] {
        let x = (v[0] as u128) & mk(b);
        let mut r: u128 = 0;
        let mut i = 0;
        while i < 128 {
            r |= x << i;
            i += b;
        }
        [r as u64, (r >> 64) as u64]
    };
    c.v[TN] = bc(sn, src_b);
    if has_rm {
        c.v[TM] = bc(sm, src_b);
    }
    c.v[TD] = bc(sd, dst_b);
    let mut w2 = w & !(1 << 28) & !(1 << 31);
    w2 &= !(31 << 0) & !(31 << 5);
    w2 |= (TD as u32) | ((TN as u32) << 5);
    if has_rm {
        w2 = (w2 & !(31 << 16)) | ((TM as u32) << 16);
    }
    if q {
        w2 |= 1 << 30;
    } else {
        w2 &= !(1 << 30);
    }
    let f = exec_simd(c, w2);
    let res = (rv(c, TD as u32) & mk(dst_b)) as u64;
    c.v[TN] = saved[0];
    c.v[TM] = saved[1];
    c.v[TD] = saved[2];
    match f {
        Flow::Undef(_) => Flow::Undef(w),
        other => {
            c.v[rd] = [res, 0];
            other
        }
    }
}

pub fn exec_scalar(c: &mut Cpu, w: u32) -> Flow {
    let top = bits(w, 28, 24);
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    if top == 0b11111 {
        if bit(w, 10) {
            // desplazamiento escalar con inmediato: 01 U 111110 ...; con el bit 23 a 1, sin asignar
            if bit(w, 23) {
                return Flow::Undef(w);
            }
            return shift_imm(c, w, true, true);
        }
        return indexed(c, w, true, true);
    }
    if bit(w, 21) {
        match bits(w, 11, 10) {
            0b01 | 0b11 => {
                // tres registros iguales
                let opc = bits(w, 15, 11);
                let fp = opc >= 0b11000;
                let (sb, ok) = if fp {
                    let sz = bit(w, 22);
                    let a = bit(w, 23);
                    let ok = match (u, opc) {
                        (false, 0b11011) | (false, 0b11100) => !a,
                        (true, 0b11100) | (true, 0b11101) => true,
                        (true, 0b11010) => a,
                        (false, 0b11111) => true,
                        _ => false,
                    };
                    (if sz { 64 } else { 32 }, ok)
                } else {
                    let big = size == 3;
                    let ok = match opc {
                        0b00001 | 0b01001 | 0b01011 | 0b00101 => true,
                        0b00110 | 0b00111 | 0b01000 | 0b01010 | 0b10000 | 0b10001 => big,
                        0b10110 => !u && (size == 1 || size == 2) || u && (size == 1 || size == 2),
                        _ => false,
                    };
                    (8u32 << size, ok)
                };
                if !ok || bits(w, 11, 10) != 0b01 && false {
                    return Flow::Undef(w);
                }
                return bcast_run(c, w, sb, sb, true);
            }
            0b00 => {
                let opc = bits(w, 15, 12);
                if u || !matches!(opc, 0b1001 | 0b1011 | 0b1101) || size == 0 || size == 3 {
                    return Flow::Undef(w);
                }
                return bcast_run(c, w, 8 << size, 16 << size, false);
            }
            _ => {
                return scalar_2misc(c, w);
            }
        }
    }
    // bit21 == 0
    if bit(w, 10) {
        // tres registros FP16 escalar
        if bit(w, 22) && bits(w, 15, 14) == 0 {
            if !need(F_ASIMDHP) {
                return Flow::Undef(w);
            }
            let a = bit(w, 23);
            let opc = bits(w, 13, 11);
            let ok = matches!(
                (u, a, opc),
                (false, false, 3) | (false, false, 4) | (false, false, 7) | (false, true, 7) | (true, false, 4) | (true, false, 5) | (true, true, 2) | (true, true, 4) | (true, true, 5)
            );
            if !ok {
                return Flow::Undef(w);
            }
            return bcast_run(c, w, 16, 16, true);
        }
        // DUP escalar
        if !u && bits(w, 23, 22) == 0 && !bit(w, 15) && bits(w, 14, 11) == 0 {
            let imm5 = bits(w, 20, 16);
            if imm5 == 0 {
                return Flow::Undef(w);
            }
            let sz = imm5.trailing_zeros();
            if sz > 3 {
                return Flow::Undef(w);
            }
            let idx = (imm5 >> (sz + 1)) as usize;
            let esb = 8u32 << sz;
            let v = lane(rv(c, bits(w, 9, 5)), esb, idx);
            wv(c, bits(w, 4, 0), v as u128, false);
            return Flow::Next;
        }
        // SQRDMLAH / SQRDMLSH escalar
        if u && bit(w, 15) && matches!(bits(w, 14, 11), 0b0000 | 0b0001) && (size == 1 || size == 2) {
            if !need(F_RDM) {
                return Flow::Undef(w);
            }
            return bcast_run(c, w, 8 << size, 8 << size, true);
        }
        return Flow::Undef(w);
    }
    Flow::Undef(w)
}

fn scalar_2misc(c: &mut Cpu, w: u32) -> Flow {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let a = bit(w, 23);
    let sz = bit(w, 22);
    let sel = bits(w, 20, 17);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    if (sel == 0b1100 || (sel == 0b1000 && !u && !bit(w, 22))) && !need(F_ASIMDHP) {
        return Flow::Undef(w);
    }
    if sel == 0b1100 {
        // FP16 escalar, dos registros
        let ok = match (u, opc) {
            (false, 0b01100) | (false, 0b01101) | (false, 0b01110) | (true, 0b01100) | (true, 0b01101) => a,
            (false, 0b11010) | (false, 0b11011) | (true, 0b11010) | (true, 0b11011) => true,
            (false, 0b11100) | (true, 0b11100) => !a,
            (false, 0b11101) | (true, 0b11101) => true,
            (false, 0b11111) => a,
            _ => false,
        };
        if !ok || !bit(w, 22) {
            return Flow::Undef(w);
        }
        if !u && opc == 0b11111 {
            let x = lane(rv(c, rn), 16, 0);
            let r = with_env(c, |e| {
                let v = frecpx(e, Ft::H, x);
                e.drain(0x3F);
                v
            });
            wv(c, rd, r as u128, false);
            return Flow::Next;
        }
        return bcast_run(c, w, 16, 16, true);
    }
    if sel == 0b1000 && !u && !bit(w, 22) {
        // pareados FP16 escalares
        if !matches!(opc, 0b01100 | 0b01101 | 0b01111) || (opc == 0b01101 && a) {
            return Flow::Undef(w);
        }
        let src = rv(c, rn);
        let (x, y) = (lane(src, 16, 0), lane(src, 16, 1));
        let r = with_env(c, |e| {
            let v = match opc {
                0b01100 => fmax(e, Ft::H, x, y, true, !a),
                0b01101 => fadd(e, Ft::H, x, y),
                _ => fmax(e, Ft::H, x, y, false, !a),
            };
            e.drain(0x3F);
            v
        });
        wv(c, rd, r as u128, false);
        return Flow::Next;
    }
    if sel == 0b1000 {
        // pareados escalares
        let src = rv(c, rn);
        match (u, opc) {
            (false, 0b11011) => {
                if size != 3 {
                    return Flow::Undef(w);
                }
                let v = lane(src, 64, 0).wrapping_add(lane(src, 64, 1));
                wv(c, rd, v as u128, false);
                return Flow::Next;
            }
            (true, 0b01100) | (true, 0b01101) | (true, 0b01111) => {
                let ft = if sz { Ft::D } else { Ft::S };
                let esb = ft.bits();
                let (x, y) = (lane(src, esb, 0), lane(src, esb, 1));
                let r = with_env(c, |e| {
                    let v = match opc {
                        0b01100 => fmax(e, ft, x, y, true, !a),
                        0b01101 => fadd(e, ft, x, y),
                        _ => fmax(e, ft, x, y, false, !a),
                    };
                    e.drain(0x3F);
                    v
                });
                if opc == 0b01101 && a {
                    return Flow::Undef(w);
                }
                wv(c, rd, r as u128, false);
                return Flow::Next;
            }
            _ => return Flow::Undef(w),
        }
    }
    if sel != 0 {
        return Flow::Undef(w);
    }
    // enteras
    match (u, opc) {
        (false, 0b00011) | (true, 0b00011) | (false, 0b00111) | (true, 0b00111) => return bcast_run(c, w, 8 << size, 8 << size, true),
        (false, 0b01000) | (true, 0b01000) | (false, 0b01001) | (true, 0b01001) | (false, 0b01010) | (false, 0b01011) | (true, 0b01011) => {
            if size != 3 {
                return Flow::Undef(w);
            }
            return bcast_run(c, w, 64, 64, true);
        }
        (false, 0b10100) | (true, 0b10100) | (true, 0b10010) => {
            if size == 3 {
                return Flow::Undef(w);
            }
            return bcast_run(c, w, 16 << size, 8 << size, false);
        }
        _ => {}
    }
    // FP
    let ok = match (u, opc) {
        (false, 0b01100) | (false, 0b01101) | (false, 0b01110) | (true, 0b01100) | (true, 0b01101) => a,
        (false, 0b11010) | (false, 0b11011) | (true, 0b11010) | (true, 0b11011) => true,
        (false, 0b11100) | (true, 0b11100) => !a,
        (false, 0b11101) | (true, 0b11101) => true,
        (false, 0b11111) => a,
        (true, 0b10110) => !a && sz,
        _ => false,
    };
    if !ok {
        return Flow::Undef(w);
    }
    if !u && opc == 0b11111 {
        // FRECPX
        let ft = if sz { Ft::D } else { Ft::S };
        let x = lane(rv(c, rn), ft.bits(), 0);
        let r = with_env(c, |e| {
            let v = frecpx(e, ft, x);
            e.drain(0x3F);
            v
        });
        wv(c, rd, r as u128, false);
        return Flow::Next;
    }
    if u && opc == 0b10110 {
        return bcast_run(c, w, 64, 32, false);
    }
    let b = if sz { 64 } else { 32 };
    bcast_run(c, w, b, b, true)
}

fn frecpx(e: &mut Env, ft: Ft, a: u64) -> u64 {
    let a = fz_in(e, ft, a);
    if is_nan(ft, a) {
        return process_nan(e, ft, a);
    }
    let (eb, fb) = (ft.exp_bits(), ft.frac_bits());
    let sign = a & ft.sign_bit();
    let exp = (a >> fb) & ((1u64 << eb) - 1);
    let nexp = if exp == 0 { (1u64 << eb) - 2 } else { !exp & ((1u64 << eb) - 1) };
    sign | (nexp << fb)
}

// FP16 vectorial y FHM: ver neonh.rs
// Media precision vectorial (FEAT_FP16, HWCAP ASIMDHP) y FMLAL/FMLSL (FEAT_FHM): SIGILL si el modelo no las tiene.
pub fn three_same_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if !need(F_ASIMDHP) {
        return Flow::Undef(w);
    }
    crate::neonh::three_same_fp16(c, w, q)
}
pub fn two_reg_misc_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if !need(F_ASIMDHP) {
        return Flow::Undef(w);
    }
    crate::neonh::two_reg_misc_fp16(c, w, q)
}
pub fn three_same_fhm(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if !need(F_FHM) {
        return Flow::Undef(w);
    }
    crate::neonh::three_same_fhm(c, w, q)
}
pub fn across_fp16(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if !need(F_ASIMDHP) {
        return Flow::Undef(w);
    }
    crate::neonh::across_fp16(c, w, q)
}
