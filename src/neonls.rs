//! Cargas y almacenamientos SIMD/FP: LDR/STR (B,H,S,D,Q), LDP/STP, literal y estructuras
//! LD1..LD4 / ST1..ST4 (multiples, de un elemento y replicadas).

use crate::cpu::*;
use crate::decode::{Addr, Ext};
use crate::interp::{eff_addr, post_wb, rsp, rx, wsp, Flow, TBI_MASK};
use crate::monitor;
use crate::neon::{lane, rv, setlane, wv};

#[inline]
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(w: u32, b: u32) -> bool {
    (w >> b) & 1 != 0
}

#[derive(Clone, Copy, Debug)]
pub enum Wb {
    None,
    Imm(u64),
    Reg(u32),
}

#[derive(Clone, Copy, Debug)]
pub enum Ls {
    /// size = log2(bytes) 0..=4
    Reg { load: bool, size: u8, rt: u32, rn: u32, addr: Addr },
    Lit { size: u8, rt: u32, imm: i64 },
    Pair { load: bool, size: u8, rt: u32, rt2: u32, rn: u32, addr: Addr },
    Multi { load: bool, q: bool, esl: u8, nregs: u32, selem: u32, rt: u32, rn: u32, wb: Wb },
    Single { load: bool, rep: bool, q: bool, selem: u32, esl: u8, index: usize, rt: u32, rn: u32, wb: Wb },
}

/// Todas las instrucciones de carga/almacenamiento con V=1.
pub fn is_simd_ldst(w: u32) -> bool {
    bit(w, 27) && !bit(w, 25) && bit(w, 26)
}

fn sx(v: u64, n: u32) -> i64 {
    ((v << (64 - n)) as i64) >> (64 - n)
}

pub fn decode(w: u32) -> Option<Ls> {
    if !is_simd_ldst(w) {
        return None;
    }
    let rt = bits(w, 4, 0);
    let rn = bits(w, 9, 5);
    let q = bit(w, 30);
    let load = bit(w, 22);
    // ---- estructuras: bit31 = 0, bits[29:25] = 00110 ----
    if bits(w, 29, 25) == 0b00110 {
        if bit(w, 31) {
            return None;
        }
        let post = bit(w, 23);
        let rm = bits(w, 20, 16);
        if bit(w, 24) {
            // un elemento
            let opcode = bits(w, 15, 13);
            let s = bit(w, 12) as usize;
            let size = bits(w, 11, 10);
            let r = bit(w, 21) as u32;
            let selem = (((opcode & 1) << 1) | r) + 1;
            let (rep, esl, index);
            if opcode >= 6 {
                if !load || s != 0 {
                    return None;
                }
                rep = true;
                esl = size as u8;
                index = 0;
            } else {
                rep = false;
                match opcode >> 1 {
                    0 => {
                        esl = 0;
                        index = ((q as usize) << 3) | (s << 2) | size as usize;
                    }
                    1 => {
                        if size & 1 != 0 {
                            return None;
                        }
                        esl = 1;
                        index = ((q as usize) << 2) | (s << 1) | (size >> 1) as usize;
                    }
                    // 32 bits: size = 00; 64 bits: size = 01 y S = 0; size = 1x no existe
                    _ => match (size, s) {
                        (0, _) => {
                            esl = 2;
                            index = ((q as usize) << 1) | s;
                        }
                        (1, 0) => {
                            esl = 3;
                            index = q as usize;
                        }
                        _ => return None,
                    },
                }
            }
            let wb = if post {
                if rm == 31 {
                    Wb::Imm((selem as u64) << esl)
                } else {
                    Wb::Reg(rm)
                }
            } else {
                if rm != 0 {
                    return None;
                }
                Wb::None
            };
            return Some(Ls::Single { load, rep, q, selem, esl, index, rt, rn, wb });
        }
        // multiples
        if bit(w, 21) {
            return None;
        }
        let opcode = bits(w, 15, 12);
        let size = bits(w, 11, 10) as u8;
        let (nregs, selem) = match opcode {
            0b0000 => (4, 4),
            0b0010 => (4, 1),
            0b0100 => (3, 3),
            0b0110 => (3, 1),
            0b0111 => (1, 1),
            0b1000 => (2, 2),
            0b1010 => (2, 1),
            _ => return None,
        };
        if selem > 1 && size == 3 && !q {
            return None;
        }
        let wb = if post {
            if rm == 31 {
                Wb::Imm(nregs as u64 * if q { 16 } else { 8 })
            } else {
                Wb::Reg(rm)
            }
        } else {
            if rm != 0 {
                return None;
            }
            Wb::None
        };
        return Some(Ls::Multi { load, q, esl: size, nregs, selem, rt, rn, wb });
    }
    let size = bits(w, 31, 30);
    // ---- literal: bits[29:27] = 011, bit24 = 0 ----
    if bits(w, 29, 27) == 0b011 {
        if bit(w, 24) || size == 3 {
            return None;
        }
        let imm = sx(bits(w, 23, 5) as u64, 19) << 2;
        return Some(Ls::Lit { size: 2 + size as u8, rt, imm });
    }
    // ---- pares: bits[29:27] = 101 ----
    if bits(w, 29, 27) == 0b101 {
        if size == 3 {
            return None;
        }
        let sz = 2 + size as u8;
        let rt2 = bits(w, 14, 10);
        let imm = sx(bits(w, 21, 15) as u64, 7) << sz;
        let addr = match bits(w, 24, 23) {
            0b00 | 0b10 => Addr::Off(imm),
            0b01 => Addr::Post(imm),
            _ => Addr::Pre(imm),
        };
        return Some(Ls::Pair { load, size: sz, rt, rt2, rn, addr });
    }
    // ---- registro unico: bits[29:27] = 111 ----
    if bits(w, 29, 27) == 0b111 {
        let opc = bits(w, 23, 22);
        let sz = match (size, opc) {
            (s, 0) | (s, 1) => s as u8,
            (0, 2) | (0, 3) => 4,
            _ => return None,
        };
        let load = opc & 1 != 0;
        let addr = if bit(w, 24) {
            Addr::Off((bits(w, 21, 10) as i64) << sz)
        } else if !bit(w, 21) {
            let imm9 = sx(bits(w, 20, 12) as u64, 9);
            match bits(w, 11, 10) {
                0b00 => Addr::Off(imm9),
                0b01 => Addr::Post(imm9),
                0b11 => Addr::Pre(imm9),
                _ => return None,
            }
        } else {
            if bits(w, 11, 10) != 0b10 {
                return None;
            }
            let ext = match bits(w, 15, 13) {
                0b010 => Ext::Uxtw,
                0b011 => Ext::Uxtx,
                0b110 => Ext::Sxtw,
                0b111 => Ext::Sxtx,
                _ => return None,
            };
            Addr::Reg { rm: bits(w, 20, 16) as u8, ext, amt: if bit(w, 12) { sz } else { 0 } }
        };
        return Some(Ls::Reg { load, size: sz, rt, rn, addr });
    }
    None
}

pub fn ldst_valid(w: u32) -> bool {
    decode(w).is_some()
}

#[inline]
fn rd(a: u64, esl: u8) -> u64 {
    unsafe { monitor::rd(a as usize, esl) }
}
#[inline]
fn st(a: u64, esl: u8, v: u64) {
    monitor::store(a as usize, esl, v);
}

fn load_vec(a: u64, size: u8) -> u128 {
    if size == 4 {
        rd(a, 3) as u128 | ((rd(a + 8, 3) as u128) << 64)
    } else {
        rd(a, size) as u128
    }
}
fn store_vec(a: u64, size: u8, v: u128) {
    if size == 4 {
        st(a, 3, v as u64);
        st(a + 8, 3, (v >> 64) as u64);
    } else {
        st(a, size, v as u64);
    }
}

fn do_wb(c: &mut Cpu, rn: u32, wb: Wb, base_before: u64) {
    match wb {
        Wb::None => {}
        Wb::Imm(i) => wsp(c, rn as u8, base_before.wrapping_add(i)),
        Wb::Reg(rm) => {
            let m = rx(c, rm as u8);
            wsp(c, rn as u8, base_before.wrapping_add(m))
        }
    }
}

pub fn exec_ldst(c: &mut Cpu, w: u32) -> Flow {
    let ls = match decode(w) {
        Some(l) => l,
        None => return Flow::Undef(w),
    };
    match ls {
        Ls::Reg { load, size, rt, rn, addr } => {
            let base_before = rsp(c, rn as u8);
            let a = eff_addr(c, rn as u8, addr);
            if load {
                let v = load_vec(a, size);
                wv(c, rt, v, true);
            } else {
                let v = rv(c, rt);
                store_vec(a, size, v);
            }
            post_wb(c, rn as u8, addr, base_before);
        }
        Ls::Lit { size, rt, imm } => {
            let a = c.pc.wrapping_add(imm as u64) & TBI_MASK;
            let v = load_vec(a, size);
            wv(c, rt, v, true);
        }
        Ls::Pair { load, size, rt, rt2, rn, addr } => {
            let base_before = rsp(c, rn as u8);
            let a = eff_addr(c, rn as u8, addr);
            let step = 1u64 << size;
            if load {
                let v1 = load_vec(a, size);
                let v2 = load_vec(a + step, size);
                wv(c, rt, v1, true);
                wv(c, rt2, v2, true);
            } else {
                let (v1, v2) = (rv(c, rt), rv(c, rt2));
                store_vec(a, size, v1);
                store_vec(a + step, size, v2);
            }
            post_wb(c, rn as u8, addr, base_before);
        }
        Ls::Multi { load, q, esl, nregs, selem, rt, rn, wb } => {
            let base_before = rsp(c, rn as u8);
            let a = base_before & TBI_MASK;
            let esb = 8u32 << esl;
            let dbytes: usize = if q { 16 } else { 8 };
            let elems = dbytes >> esl;
            let ebytes = 1u64 << esl;
            let mut off = 0u64;
            if load {
                let mut regs = [0u128; 4];
                if selem == 1 {
                    for r in 0..nregs as usize {
                        for e in 0..elems {
                            setlane(&mut regs[r], esb, e, rd(a + off, esl));
                            off += ebytes;
                        }
                    }
                } else {
                    for e in 0..elems {
                        for r in 0..selem as usize {
                            setlane(&mut regs[r], esb, e, rd(a + off, esl));
                            off += ebytes;
                        }
                    }
                }
                for r in 0..nregs {
                    wv(c, (rt + r) % 32, regs[r as usize], q);
                }
            } else {
                let mut regs = [0u128; 4];
                for r in 0..nregs {
                    regs[r as usize] = rv(c, (rt + r) % 32);
                }
                if selem == 1 {
                    for r in 0..nregs as usize {
                        for e in 0..elems {
                            st(a + off, esl, lane(regs[r], esb, e));
                            off += ebytes;
                        }
                    }
                } else {
                    for e in 0..elems {
                        for r in 0..selem as usize {
                            st(a + off, esl, lane(regs[r], esb, e));
                            off += ebytes;
                        }
                    }
                }
            }
            do_wb(c, rn, wb, base_before);
        }
        Ls::Single { load, rep, q, selem, esl, index, rt, rn, wb } => {
            let base_before = rsp(c, rn as u8);
            let a = base_before & TBI_MASK;
            let esb = 8u32 << esl;
            let ebytes = 1u64 << esl;
            if load {
                let mut vals = [0u64; 4];
                for s in 0..selem as usize {
                    vals[s] = rd(a + s as u64 * ebytes, esl);
                }
                for s in 0..selem {
                    let r = (rt + s) % 32;
                    let v = vals[s as usize];
                    if rep {
                        let n = (if q { 16 } else { 8 }) >> esl;
                        let mut d = 0u128;
                        for e in 0..n {
                            setlane(&mut d, esb, e, v);
                        }
                        wv(c, r, d, q);
                    } else {
                        let mut d = rv(c, r);
                        setlane(&mut d, esb, index, v);
                        wv(c, r, d, true);
                    }
                }
            } else {
                for s in 0..selem {
                    let v = lane(rv(c, (rt + s) % 32), esb, index);
                    st(a + s as u64 * ebytes, esl, v);
                }
            }
            do_wb(c, rn, wb, base_before);
        }
    }
    Flow::Next
}
