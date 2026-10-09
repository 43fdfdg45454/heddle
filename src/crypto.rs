//! Extensiones criptograficas: AES, SHA1, SHA256, PMULL (8b/64b), SHA512 y SHA3 (EOR3/BCAX/RAX1/XAR).
//! SM3/SM4 no estan implementados (no se anuncian en HWCAP). Cada grupo da SIGILL si el modelo de CPU no tiene su
//! extension (`feat::need`).

use crate::cpu::*;
use crate::interp::Flow;
use crate::neon::{rv, wv, V};
use crate::feat::{need, F_AES, F_PMULL, F_SHA1, F_SHA256, F_SHA3, F_SHA512};
use std::sync::OnceLock;

pub fn is_crypto(w: u32) -> bool {
    let top = w >> 24;
    let b = |hi: u32, lo: u32| (w >> lo) & ((1u32 << (hi - lo + 1)) - 1);
    match top {
        0x4E => b(21, 17) == 0b10100 && b(11, 10) == 0b10 && b(23, 22) == 0,
        0x5E => (b(21, 17) == 0b10100 && b(11, 10) == 0b10) || (b(21, 21) == 0 && b(15, 15) == 0 && b(11, 10) == 0),
        0xCE => true,
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// AES
// ---------------------------------------------------------------------------------------------
fn xtime(x: u8) -> u8 {
    (x << 1) ^ if x & 0x80 != 0 { 0x1B } else { 0 }
}
fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a = xtime(a);
        b >>= 1;
    }
    p
}

fn sboxes() -> &'static ([u8; 256], [u8; 256]) {
    static T: OnceLock<([u8; 256], [u8; 256])> = OnceLock::new();
    T.get_or_init(|| {
        let mut s = [0u8; 256];
        let mut inv = [0u8; 256];
        for x in 0..256usize {
            // inverso multiplicativo en GF(2^8)
            let mut r = 0u8;
            if x != 0 {
                for y in 1..256usize {
                    if gmul(x as u8, y as u8) == 1 {
                        r = y as u8;
                        break;
                    }
                }
            }
            let mut b = r;
            let mut res = 0x63u8;
            for _ in 0..5 {
                res ^= b;
                b = b.rotate_left(1);
            }
            // res = r ^ rol1 ^ rol2 ^ rol3 ^ rol4 ^ 0x63
            s[x] = res;
            inv[res as usize] = x as u8;
        }
        (s, inv)
    })
}

fn to_bytes(v: V) -> [u8; 16] {
    v.to_le_bytes()
}
fn from_bytes(b: [u8; 16]) -> V {
    u128::from_le_bytes(b)
}

const SHIFT_ROWS: [usize; 16] = [0, 5, 10, 15, 4, 9, 14, 3, 8, 13, 2, 7, 12, 1, 6, 11];
const INV_SHIFT_ROWS: [usize; 16] = [0, 13, 10, 7, 4, 1, 14, 11, 8, 5, 2, 15, 12, 9, 6, 3];

fn mix_columns(s: [u8; 16], inverse: bool) -> [u8; 16] {
    let mut o = [0u8; 16];
    for c in 0..4 {
        let a = [s[4 * c], s[4 * c + 1], s[4 * c + 2], s[4 * c + 3]];
        let m = if inverse { [14u8, 11, 13, 9] } else { [2u8, 3, 1, 1] };
        for r in 0..4 {
            let mut v = 0u8;
            for k in 0..4 {
                v ^= gmul(a[k], m[(k + 4 - r) % 4]);
            }
            o[4 * c + r] = v;
        }
    }
    o
}

// ---------------------------------------------------------------------------------------------
// SHA
// ---------------------------------------------------------------------------------------------
fn l4(v: V) -> [u32; 4] {
    [v as u32, (v >> 32) as u32, (v >> 64) as u32, (v >> 96) as u32]
}
fn p4(a: [u32; 4]) -> V {
    a[0] as u128 | ((a[1] as u128) << 32) | ((a[2] as u128) << 64) | ((a[3] as u128) << 96)
}
fn ch(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (!x & z)
}
fn par(x: u32, y: u32, z: u32) -> u32 {
    x ^ y ^ z
}
fn maj(x: u32, y: u32, z: u32) -> u32 {
    (x & y) ^ (x & z) ^ (y & z)
}

fn sha1_hash(x: V, y: u32, w: V, f: fn(u32, u32, u32) -> u32) -> V {
    let mut xs = l4(x);
    let mut y = y;
    let ws = l4(w);
    for e in 0..4 {
        let t = f(xs[1], xs[2], xs[3]);
        y = y.wrapping_add(xs[0].rotate_left(5)).wrapping_add(t).wrapping_add(ws[e]);
        xs[1] = xs[1].rotate_left(30);
        // <Y, X> = ROL(Y:X, 32)
        let ny = xs[3];
        xs = [y, xs[0], xs[1], xs[2]];
        y = ny;
    }
    p4(xs)
}

fn sha256_hash(x: V, y: V, w: V, part1: bool) -> V {
    let mut xs = l4(x);
    let mut ys = l4(y);
    let ws = l4(w);
    let s0 = |v: u32| v.rotate_right(2) ^ v.rotate_right(13) ^ v.rotate_right(22);
    let s1 = |v: u32| v.rotate_right(6) ^ v.rotate_right(11) ^ v.rotate_right(25);
    for e in 0..4 {
        let chs = ch(ys[0], ys[1], ys[2]);
        let mj = maj(xs[0], xs[1], xs[2]);
        let t = ys[3].wrapping_add(s1(ys[0])).wrapping_add(chs).wrapping_add(ws[e]);
        xs[3] = t.wrapping_add(xs[3]);
        ys[3] = t.wrapping_add(s0(xs[0])).wrapping_add(mj);
        // ROL(Y:X, 32)
        let (nx, ny) = ([ys[3], xs[0], xs[1], xs[2]], [xs[3], ys[0], ys[1], ys[2]]);
        xs = nx;
        ys = ny;
    }
    if part1 {
        p4(xs)
    } else {
        p4(ys)
    }
}

pub fn exec_crypto(c: &mut Cpu, w: u32) -> Flow {
    let b = |hi: u32, lo: u32| (w >> lo) & ((1u32 << (hi - lo + 1)) - 1);
    let (rd, rn, rm) = (b(4, 0), b(9, 5), b(20, 16));
    let top = w >> 24;
    if b(21, 17) == 0b10100 && b(11, 10) == 0b10 {
        let opc = b(16, 12);
        if top == 0x4E {
            if !need(F_AES) {
                return Flow::Undef(w);
            }
            let (d, n) = (rv(c, rd), rv(c, rn));
            let (sb, isb) = sboxes();
            let r = match opc {
                0b00100 | 0b00101 => {
                    let st = to_bytes(d ^ n);
                    let mut o = [0u8; 16];
                    for i in 0..16 {
                        if opc == 0b00100 {
                            o[i] = sb[st[SHIFT_ROWS[i]] as usize];
                        } else {
                            o[i] = isb[st[INV_SHIFT_ROWS[i]] as usize];
                        }
                    }
                    from_bytes(o)
                }
                0b00110 => from_bytes(mix_columns(to_bytes(n), false)),
                0b00111 => from_bytes(mix_columns(to_bytes(n), true)),
                _ => return Flow::Undef(w),
            };
            wv(c, rd, r, true);
            return Flow::Next;
        }
        // SHA de dos registros (0x5E)
        if b(23, 22) != 0 || !need(if opc == 0b00010 { F_SHA256 } else { F_SHA1 }) {
            return Flow::Undef(w);
        }
        let (d, n) = (rv(c, rd), rv(c, rn));
        match opc {
            0b00000 => {
                let v = (n as u32).rotate_left(30);
                wv(c, rd, v as u128, true);
            }
            0b00001 => {
                // SHA1SU1
                let t = l4(d);
                let y = l4(n);
                let t = [t[0] ^ y[1], t[1] ^ y[2], t[2] ^ y[3], t[3]];
                let r = [t[0].rotate_left(1), t[1].rotate_left(1), t[2].rotate_left(1), t[3].rotate_left(1) ^ t[0].rotate_left(2)];
                wv(c, rd, p4(r), true);
            }
            0b00010 => {
                // SHA256SU0
                let x = l4(d);
                let y = l4(n);
                let t = [x[1], x[2], x[3], y[0]];
                let sg = |v: u32| v.rotate_right(7) ^ v.rotate_right(18) ^ (v >> 3);
                let r = [x[0].wrapping_add(sg(t[0])), x[1].wrapping_add(sg(t[1])), x[2].wrapping_add(sg(t[2])), x[3].wrapping_add(sg(t[3]))];
                wv(c, rd, p4(r), true);
            }
            _ => return Flow::Undef(w),
        }
        return Flow::Next;
    }
    // SHA de tres registros (0x5E): size=00, bit21=0, bit15=0, bits 11:10 = 00
    let opc = b(14, 12);
    if b(23, 22) != 0 || !need(if opc < 4 { F_SHA1 } else { F_SHA256 }) {
        return Flow::Undef(w);
    }
    let (d, n, m) = (rv(c, rd), rv(c, rn), rv(c, rm));
    let r = match opc {
        0b000 => sha1_hash(d, n as u32, m, ch),
        0b001 => sha1_hash(d, n as u32, m, par),
        0b010 => sha1_hash(d, n as u32, m, maj),
        0b011 => {
            // SHA1SU0: Vd ^ Vm ^ (Vn<63:0> : Vd<127:64>)
            let t = (d >> 64) | ((n & (u64::MAX as u128)) << 64);
            d ^ m ^ t
        }
        0b100 => sha256_hash(d, n, m, true),
        0b101 => sha256_hash(n, d, m, false),
        _ => {
            // SHA256SU1
            let (x, y, z) = (l4(d), l4(n), l4(m));
            let t0 = [y[1], y[2], y[3], z[0]];
            let sg = |v: u32| v.rotate_right(17) ^ v.rotate_right(19) ^ (v >> 10);
            let w0 = x[0].wrapping_add(sg(z[2])).wrapping_add(t0[0]);
            let w1 = x[1].wrapping_add(sg(z[3])).wrapping_add(t0[1]);
            let w2 = x[2].wrapping_add(sg(w0)).wrapping_add(t0[2]);
            let w3 = x[3].wrapping_add(sg(w1)).wrapping_add(t0[3]);
            p4([w0, w1, w2, w3])
        }
    };
    if opc == 0b111 {
        return Flow::Undef(w);
    }
    wv(c, rd, r, true);
    Flow::Next
}

fn l2(v: V) -> [u64; 2] {
    [v as u64, (v >> 64) as u64]
}
fn p2(a: [u64; 2]) -> V {
    a[0] as u128 | ((a[1] as u128) << 64)
}

/// 0xCE: SHA512, SHA3 (EOR3, RAX1, XAR, BCAX)
pub fn exec_crypto3(c: &mut Cpu, w: u32) -> Flow {
    let b = |hi: u32, lo: u32| (w >> lo) & ((1u32 << (hi - lo + 1)) - 1);
    let (rd, rn, rm, ra) = (b(4, 0), b(9, 5), b(20, 16), b(14, 10));
    let op0 = b(23, 21);
    let (d, n, m, a) = (rv(c, rd), rv(c, rn), rv(c, rm), rv(c, ra));
    // SHA512H/H2/SU1 (op0=011, bits 15:12=1000) y SHA512SU0 (op0=110); el resto, SHA3
    let sha512 = (op0 == 0b011 && b(15, 12) == 0b1000) || op0 == 0b110;
    if !need(if sha512 { F_SHA512 } else { F_SHA3 }) {
        return Flow::Undef(w);
    }
    let r: V = match op0 {
        0b000 if !bit(w, 15) => n ^ m ^ a,       // EOR3
        0b001 if !bit(w, 15) => n ^ (m & !a),    // BCAX
        0b100 => {
            // XAR: bits 15:10 = imm6
            let imm = b(15, 10);
            let t = l2(n ^ m);
            p2([t[0].rotate_right(imm), t[1].rotate_right(imm)])
        }
        0b011 if b(15, 10) == 0b100011 => {
            // RAX1
            let mm = l2(m);
            n ^ p2([mm[0].rotate_left(1), mm[1].rotate_left(1)])
        }
        0b011 if b(15, 12) == 0b1000 && b(11, 10) < 4 && b(15, 15) == 1 => {
            let x = l2(n);
            let y = l2(m);
            let wv_ = l2(d);
            let big1 = |v: u64| v.rotate_right(14) ^ v.rotate_right(18) ^ v.rotate_right(41);
            let big0 = |v: u64| v.rotate_right(28) ^ v.rotate_right(34) ^ v.rotate_right(39);
            match b(11, 10) {
                0 => {
                    // SHA512H
                    let hi = ((y[1] & x[0]) ^ (!y[1] & x[1])).wrapping_add(big1(y[1])).wrapping_add(wv_[1]);
                    let tmp = hi.wrapping_add(y[0]);
                    let lo = ((tmp & y[1]) ^ (!tmp & x[0])).wrapping_add(big1(tmp)).wrapping_add(wv_[0]);
                    p2([lo, hi])
                }
                1 => {
                    // SHA512H2
                    let hi = ((x[0] & y[1]) ^ (x[0] & y[0]) ^ (y[1] & y[0])).wrapping_add(big0(y[0])).wrapping_add(wv_[1]);
                    let lo = ((hi & y[0]) ^ (hi & y[1]) ^ (y[1] & y[0])).wrapping_add(big0(hi)).wrapping_add(wv_[0]);
                    p2([lo, hi])
                }
                2 => {
                    // SHA512SU1
                    let s1 = |v: u64| v.rotate_right(19) ^ v.rotate_right(61) ^ (v >> 6);
                    let hi = wv_[1].wrapping_add(s1(x[1])).wrapping_add(y[1]);
                    let lo = wv_[0].wrapping_add(s1(x[0])).wrapping_add(y[0]);
                    p2([lo, hi])
                }
                _ => return Flow::Undef(w),
            }
        }
        0b110 if b(15, 10) == 0b100000 && rm == 0 => {
            // SHA512SU0
            let x = l2(n);
            let wd = l2(d);
            let s0 = |v: u64| v.rotate_right(1) ^ v.rotate_right(8) ^ (v >> 7);
            p2([wd[0].wrapping_add(s0(wd[1])), wd[1].wrapping_add(s0(x[0]))])
        }
        _ => return Flow::Undef(w),
    };
    wv(c, rd, r, true);
    Flow::Next
}

fn bit(w: u32, b: u32) -> bool {
    (w >> b) & 1 != 0
}

fn clmul64(a: u64, b: u64) -> u128 {
    let mut r = 0u128;
    for i in 0..64 {
        if (b >> i) & 1 == 1 {
            r ^= (a as u128) << i;
        }
    }
    r
}

/// PMULL / PMULL2 con elementos de 64 bits (resultado de 128).
pub fn pmull64(c: &mut Cpu, w: u32, q: bool) -> Flow {
    if !need(F_PMULL) {
        return Flow::Undef(w);
    }
    let (rd, rn, rm) = ((w & 31) as u32, ((w >> 5) & 31) as u32, ((w >> 16) & 31) as u32);
    let (a, b) = (rv(c, rn), rv(c, rm));
    let (x, y) = if q { ((a >> 64) as u64, (b >> 64) as u64) } else { (a as u64, b as u64) };
    wv(c, rd, clmul64(x, y), true);
    Flow::Next
}
