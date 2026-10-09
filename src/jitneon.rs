//! Rutas traducidas del JIT para SIMD ENTERO sin flags de FP: LD1/ST1 multiples (1 a 4 registros, con y sin
//! post-indice), EXT, REV16/32/64, INS, DUP, ZIP/UZP/TRN, MOVI/MVNI/ORR/BIC/FMOV (inmediato vectorial) y los pares
//! ADDP/SMAXP/SMINP/UMAXP/UMINP.
//!
//! Equivalencia exacta por construccion: ninguna de estas instrucciones toca FPSR ni depende de FPCR (son
//! reordenaciones de bytes, copias o enteros), asi que no hay ruta lenta: o se traduce o se devuelve false y la
//! instruccion va al interprete de referencia (`neon.rs`/`neonls.rs`/`neonx.rs`). Las permutaciones se expresan como
//! una tabla de 16 bytes (origen, indice) calculada al traducir con las mismas formulas que el interprete y se
//! ejecutan con `pshufb` (SSSE3); sin SSSE3 (o SSE4.1 para los maximos/minimos de pares) no se traduce.
//!
//! Memoria del guest:
//!  - Cargas (LD1): movups/movq directos desde la direccion con la mascara TBI aplicada, como el resto de cargas del
//!    JIT. Todas las lecturas ocurren antes de escribir ningun registro (un fallo no deja estado a medias).
//!  - Almacenamientos (ST1): nunca se escribe memoria del guest sin pasar por `monitor::store`: cada 8 bytes de la
//!    memoria resultante son UNA llamada a `h_store` (tamano 3), igual que STR/STP de Q. El resultado en memoria es
//!    el de los stores por elemento del interprete (los bytes son los mismos); solo cambia la granularidad de los
//!    stores internos, que en ARM tampoco es observable salvo por la atomicidad por elemento, que un store de 8 bytes
//!    cumple con creces.
//!  - El post-indice se calcula con el valor SIN enmascarar del registro base (como el interprete: el byte alto
//!    se conserva) y se escribe tras acceder a memoria.

use super::*;
use crate::neonls::{self, Ls, Wb};

const O_V: i32 = offset_of!(Cpu, v) as i32;

fn vo(r: u32) -> i32 {
    O_V + 16 * r as i32
}
#[inline]
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(w: u32, b: u32) -> bool {
    (w >> b) & 1 != 0
}

pub fn ssse3_available() -> bool {
    std::is_x86_feature_detected!("ssse3")
}
fn sse41_ok() -> bool {
    ssse3_available() && std::is_x86_feature_detected!("sse4.1")
}

/// Origen de un byte del resultado: (0 = Vn, 1 = Vm, otro = cero; indice 0..15).
type Map = [(u8, u8); 16];
const ZERO: (u8, u8) = (2, 0);

// ---------------------------------------------------------------------------------------------
// Ensamblado de ayuda
// ---------------------------------------------------------------------------------------------

fn vld(a: &mut Asm, xmm: u8, r: u32, q: bool) {
    if q {
        a.sse_rm(0, false, &[0x0F, 0x10], xmm, RBX, vo(r)); // movups
    } else {
        a.sse_rm(0xF3, false, &[0x0F, 0x7E], xmm, RBX, vo(r)); // movq xmm, m64 (cero arriba)
    }
}
/// Escribe xmm en v[r]; con q = false solo 64 bits y el resto a cero (como `wv(.., false)`).
fn vst(a: &mut Asm, xmm: u8, r: u32, q: bool) {
    if q {
        a.sse_rm(0, false, &[0x0F, 0x11], xmm, RBX, vo(r)); // movups
    } else {
        a.sse_rm(0x66, false, &[0x0F, 0xD6], xmm, RBX, vo(r)); // movq m64, xmm
        a.mov_mi64(RBX, vo(r) + 8, 0);
    }
}
fn movaps(a: &mut Asm, dst: u8, src: u8) {
    a.sse_rr(0, false, &[0x0F, 0x28], dst, src);
}
/// Carga una mascara de 16 bytes en `dst` usando `tmp` y RAX.
fn mask_load(a: &mut Asm, dst: u8, tmp: u8, m: [u8; 16]) {
    let lo = u64::from_le_bytes(m[0..8].try_into().unwrap());
    let hi = u64::from_le_bytes(m[8..16].try_into().unwrap());
    a.mov_ri(RAX, lo);
    a.sse_rr(0x66, true, &[0x0F, 0x6E], dst, RAX); // movq xmm, rax
    if hi == lo {
        a.sse_rr(0x66, false, &[0x0F, 0x6C], dst, dst); // punpcklqdq
    } else {
        a.mov_ri(RAX, hi);
        a.sse_rr(0x66, true, &[0x0F, 0x6E], tmp, RAX);
        a.sse_rr(0x66, false, &[0x0F, 0x6C], dst, tmp);
    }
}

/// dst = bytes de (n = xmm0, m = xmm1) segun `map`. Usa xmm4, xmm5 y xmm6 de trabajo; `dst` debe ser distinto de 0, 1, 4, 5, 6.
fn gather(a: &mut Asm, dst: u8, map: &Map) {
    let (mut ma, mut mb) = ([0x80u8; 16], [0x80u8; 16]);
    for i in 0..16 {
        match map[i].0 {
            0 => ma[i] = map[i].1,
            1 => mb[i] = map[i].1,
            _ => {}
        }
    }
    let has_a = ma.iter().any(|&b| b != 0x80);
    let has_b = mb.iter().any(|&b| b != 0x80);
    if has_a {
        movaps(a, dst, 0);
        mask_load(a, 4, 6, ma);
        a.sse_rr(0x66, false, &[0x0F, 0x38, 0x00], dst, 4); // pshufb dst, mask
    } else {
        a.sse_rr(0x66, false, &[0x0F, 0xEF], dst, dst); // pxor
    }
    if has_b {
        movaps(a, 5, 1);
        mask_load(a, 4, 6, mb);
        a.sse_rr(0x66, false, &[0x0F, 0x38, 0x00], 5, 4);
        a.sse_rr(0x66, false, &[0x0F, 0xEB], dst, 5); // por
    }
}

/// Wn/Xn -> carril `idx` de tamano 2^size de v[rd] (el valor sale de RAX).
fn st_lane(a: &mut Asm, disp: i32, size: u8) {
    match size {
        0 => a.op_rm(false, &[0x88], RAX, RBX, disp),
        1 => {
            a.u8(0x66);
            a.op_rm(false, &[0x89], RAX, RBX, disp)
        }
        2 => a.op_rm(false, &[0x89], RAX, RBX, disp),
        _ => a.mov_mr(RBX, disp, RAX, true),
    }
}

// ---------------------------------------------------------------------------------------------
// Punto de entrada
// ---------------------------------------------------------------------------------------------

/// true si la palabra se tradujo (el codigo ya esta emitido); false si no es una de estas formas o no se puede
/// traducir (el llamador sigue con las demas rutas y, al final, con el interprete).
pub(super) fn try_emit(cx: &mut Ctx, w: u32) -> bool {
    if crate::neon::is_simd_ldst(w) {
        return ldst_multi(cx, w);
    }
    if bit(w, 31) {
        return false;
    }
    let q = bit(w, 30);
    let top = bits(w, 28, 24);
    if top == 0b01111 {
        if bit(w, 10) && bits(w, 23, 19) == 0 {
            return mod_imm(cx, w, q);
        }
        return false;
    }
    if top != 0b01110 {
        return false;
    }
    if bit(w, 21) {
        if !bit(w, 10) {
            if bit(w, 11) && bits(w, 20, 17) == 0 {
                return rev(cx, w, q);
            }
            return false;
        }
        return pairwise(cx, w, q);
    }
    if bit(w, 10) {
        if !bit(w, 15) && bits(w, 23, 22) == 0 {
            return copy(cx, w, q);
        }
        return false;
    }
    if bit(w, 15) {
        return false;
    }
    if bit(w, 29) {
        if bits(w, 23, 22) == 0 {
            return ext(cx, w, q);
        }
        return false;
    }
    if !bit(w, 11) {
        return false; // TBL/TBX
    }
    permute(cx, w, q)
}

// ---------------------------------------------------------------------------------------------
// LD1 / ST1 (multiples, un solo registro por elemento: selem = 1)
// ---------------------------------------------------------------------------------------------

fn wb_emit(cx: &mut Ctx, rn: u32, wb: Wb) {
    match wb {
        Wb::None => {}
        // via ld_x/st_x (cache de registros de jit.rs; aqui fuera del codigo lineal: st_x invalida la ranura)
        Wb::Imm(i) => {
            cx.ld_x(RCX, rn as u8, false);
            cx.a.lea(RCX, RCX, i as i32, true);
            cx.st_x(rn as u8, RCX, false);
        }
        Wb::Reg(rm) => {
            cx.ld_x(RCX, rn as u8, false);
            cx.ld_x(RDX, rm as u8, false);
            cx.a.alu_rr(true, 0x01, RCX, RDX);
            cx.st_x(rn as u8, RCX, false);
        }
    }
}

fn ldst_multi(cx: &mut Ctx, w: u32) -> bool {
    let (load, q, nregs, selem, rt, rn, wb) = match neonls::decode(w) {
        Some(Ls::Multi { load, q, nregs, selem, rt, rn, wb, .. }) => (load, q, nregs, selem, rt, rn, wb),
        _ => return false,
    };
    if selem != 1 {
        return false; // LD2/3/4 y ST2/3/4 (intercalado): interprete
    }
    let nbytes: i32 = if q { 16 } else { 8 };
    cx.ld_x(RAX, rn as u8, false);
    cx.mask_tbi(RAX);
    if load {
        for r in 0..nregs {
            let off = r as i32 * nbytes;
            if q {
                cx.a.sse_rm(0, false, &[0x0F, 0x10], r as u8, RAX, off); // movups xmm_r, [rax+off]
            } else {
                cx.a.sse_rm(0xF3, false, &[0x0F, 0x7E], r as u8, RAX, off); // movq
            }
        }
        wb_emit(cx, rn, wb);
        for r in 0..nregs {
            vst(&mut cx.a, r as u8, (rt + r) % 32, q);
        }
    } else {
        cx.a.mov_mr(RBX, O_AUX_SCRATCH, RAX, true);
        let per = nbytes / 8; // trozos de 8 bytes por registro
        for k in 0..(nregs as i32 * per) {
            let reg = (rt + (k / per) as u32) % 32;
            cx.a.mov_rm(RDI, RBX, O_AUX_SCRATCH, true);
            if k != 0 {
                cx.a.lea(RDI, RDI, 8 * k, true);
            }
            cx.a.mov_rm(RDX, RBX, vo(reg) + 8 * (k % per), true);
            cx.call_store(3);
        }
        wb_emit(cx, rn, wb);
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Permutaciones: EXT, REV, ZIP/UZP/TRN
// ---------------------------------------------------------------------------------------------

/// Emite: xmm0 = Vn, xmm1 = Vm (solo si se usa), xmm2 = permutacion segun `map`, v[rd] = xmm2.
fn emit_perm(cx: &mut Ctx, rn: u32, rm: u32, rd: u32, q: bool, map: &Map) {
    vld(&mut cx.a, 0, rn, q);
    if map.iter().any(|e| e.0 == 1) {
        vld(&mut cx.a, 1, rm, q);
    }
    gather(&mut cx.a, 2, map);
    vst(&mut cx.a, 2, rd, q);
}

fn nb(q: bool) -> usize {
    if q {
        16
    } else {
        8
    }
}

fn ext(cx: &mut Ctx, w: u32, q: bool) -> bool {
    if !ssse3_available() {
        return false;
    }
    let imm4 = bits(w, 14, 11) as usize;
    if !q && imm4 >= 8 {
        return false;
    }
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let n = nb(q);
    let mut map: Map = [ZERO; 16];
    for i in 0..n {
        let j = i + imm4;
        map[i] = if j < n { (0, j as u8) } else { (1, (j - n) as u8) };
    }
    emit_perm(cx, rn, rm, rd, q, &map);
    true
}

fn rev(cx: &mut Ctx, w: u32, q: bool) -> bool {
    if !ssse3_available() {
        return false;
    }
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 16, 12);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    let eb = 1usize << size;
    let cont = match (opc, u) {
        (0, false) => 8,
        (0, true) => 4,
        (1, false) if size == 0 => 2,
        _ => return false,
    };
    if eb >= cont {
        return false;
    }
    let per = cont / eb;
    let mut map: Map = [ZERO; 16];
    for i in 0..nb(q) {
        let g = i / cont;
        let e = (i % cont) / eb;
        let k = i % eb;
        map[i] = (0, (g * cont + (per - 1 - e) * eb + k) as u8);
    }
    emit_perm(cx, rn, rn, rd, q, &map);
    true
}

fn permute(cx: &mut Ctx, w: u32, q: bool) -> bool {
    let size = bits(w, 23, 22);
    let opc = bits(w, 14, 12);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    if (size == 3 && !q) || opc == 0 || opc == 4 {
        return false;
    }
    // ZIP: desempaquetado directo
    if opc == 0b011 || opc == 0b111 {
        const LO: [u8; 4] = [0x60, 0x61, 0x62, 0x6C];
        const HI: [u8; 4] = [0x68, 0x69, 0x6A, 0x6D];
        let a = &mut cx.a;
        vld(a, 0, rn, q);
        vld(a, 1, rm, q);
        if q {
            let t = if opc == 7 { HI } else { LO };
            a.sse_rr(0x66, false, &[0x0F, t[size as usize]], 0, 1);
        } else {
            a.sse_rr(0x66, false, &[0x0F, LO[size as usize]], 0, 1);
            if opc == 7 {
                a.sse_rr(0x66, false, &[0x0F, 0x73], 3, 0); // psrldq xmm0, 8
                a.u8(8);
            }
        }
        vst(a, 0, rd, q);
        return true;
    }
    if !ssse3_available() {
        return false;
    }
    let eb = 1usize << size;
    let n = nb(q) / eb;
    let h = n / 2;
    let mut map: Map = [ZERO; 16];
    for i in 0..n {
        let (src, e) = match opc {
            0b001 | 0b101 => {
                let p = (opc >> 2) as usize;
                if i < h {
                    (0u8, 2 * i + p)
                } else {
                    (1u8, 2 * (i - h) + p)
                }
            }
            _ => {
                // TRN1 / TRN2
                let p = (opc >> 2) as usize;
                let k = i / 2;
                (if i % 2 == 0 { 0u8 } else { 1u8 }, 2 * k + p)
            }
        };
        for k in 0..eb {
            map[i * eb + k] = (src, (e * eb + k) as u8);
        }
    }
    emit_perm(cx, rn, rm, rd, q, &map);
    true
}

// ---------------------------------------------------------------------------------------------
// Pares: ADDP, SMAXP/UMAXP, SMINP/UMINP
// ---------------------------------------------------------------------------------------------

fn pairwise(cx: &mut Ctx, w: u32, q: bool) -> bool {
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let opc = bits(w, 15, 11);
    let (rd, rn, rm) = (bits(w, 4, 0), bits(w, 9, 5), bits(w, 20, 16));
    let op: &[u8] = match (opc, u, size) {
        (0b10111, false, 0) => &[0x0F, 0xFC],
        (0b10111, false, 1) => &[0x0F, 0xFD],
        (0b10111, false, 2) => &[0x0F, 0xFE],
        (0b10111, false, 3) => &[0x0F, 0xD4],
        (0b10100, false, 0) => &[0x0F, 0x38, 0x3C],
        (0b10100, false, 1) => &[0x0F, 0xEE],
        (0b10100, false, 2) => &[0x0F, 0x38, 0x3D],
        (0b10101, false, 0) => &[0x0F, 0x38, 0x38],
        (0b10101, false, 1) => &[0x0F, 0xEA],
        (0b10101, false, 2) => &[0x0F, 0x38, 0x39],
        (0b10100, true, 0) => &[0x0F, 0xDE],
        (0b10100, true, 1) => &[0x0F, 0x38, 0x3E],
        (0b10100, true, 2) => &[0x0F, 0x38, 0x3F],
        (0b10101, true, 0) => &[0x0F, 0xDA],
        (0b10101, true, 1) => &[0x0F, 0x38, 0x3A],
        (0b10101, true, 2) => &[0x0F, 0x38, 0x3B],
        _ => return false,
    };
    if size == 3 && !q {
        return false;
    }
    if !sse41_ok() {
        return false;
    }
    let eb = 1usize << size;
    let n = nb(q) / eb;
    let h = n / 2;
    let mk = |p: usize| -> Map {
        let mut map: Map = [ZERO; 16];
        for i in 0..n {
            let (src, e) = if i < h { (0u8, 2 * i + p) } else { (1u8, 2 * (i - h) + p) };
            for k in 0..eb {
                map[i * eb + k] = (src, (e * eb + k) as u8);
            }
        }
        map
    };
    vld(&mut cx.a, 0, rn, q);
    vld(&mut cx.a, 1, rm, q);
    gather(&mut cx.a, 2, &mk(0));
    gather(&mut cx.a, 3, &mk(1));
    cx.a.sse_rr(0x66, false, op, 2, 3);
    vst(&mut cx.a, 2, rd, q);
    true
}

// ---------------------------------------------------------------------------------------------
// INS / DUP
// ---------------------------------------------------------------------------------------------

fn copy(cx: &mut Ctx, w: u32, q: bool) -> bool {
    let imm5 = bits(w, 20, 16);
    let imm4 = bits(w, 14, 11);
    let op = bit(w, 29);
    let (rd, rn) = (bits(w, 4, 0), bits(w, 9, 5));
    if imm5 == 0 {
        return false;
    }
    let size = imm5.trailing_zeros();
    if size > 3 {
        return false;
    }
    let idx = (imm5 >> (size + 1)) as i32;
    let sz = size as u8;
    if op {
        // INS (elemento): solo Q = 1
        if !q {
            return false;
        }
        let sidx = (imm4 >> size) as i32;
        cx.a.load_ext(RAX, RBX, vo(rn) + (sidx << size), sz, 0);
        st_lane(&mut cx.a, vo(rd) + (idx << size), sz);
        return true;
    }
    match imm4 {
        0b0011 => {
            // INS (general)
            if !q {
                return false;
            }
            cx.ld_x(RAX, rn as u8, true);
            st_lane(&mut cx.a, vo(rd) + (idx << size), sz);
            true
        }
        0b0000 | 0b0001 => {
            // DUP (elemento / general)
            if size == 3 && !q {
                return false;
            }
            if imm4 == 0 {
                cx.a.load_ext(RAX, RBX, vo(rn) + (idx << size), sz, 0);
            } else {
                cx.ld_x(RAX, rn as u8, true);
            }
            let a = &mut cx.a;
            match size {
                3 => {
                    a.sse_rr(0x66, true, &[0x0F, 0x6E], 0, RAX); // movq xmm0, rax
                    a.sse_rr(0x66, false, &[0x0F, 0x6C], 0, 0); // punpcklqdq
                }
                _ => {
                    a.sse_rr(0x66, false, &[0x0F, 0x6E], 0, RAX); // movd xmm0, eax
                    if size == 0 {
                        a.sse_rr(0x66, false, &[0x0F, 0x60], 0, 0); // punpcklbw
                    }
                    if size <= 1 {
                        a.sse_rr(0xF2, false, &[0x0F, 0x70], 0, 0); // pshuflw xmm0, xmm0, 0
                        a.u8(0);
                    }
                    a.sse_rr(0x66, false, &[0x0F, 0x70], 0, 0); // pshufd xmm0, xmm0, 0
                    a.u8(0);
                }
            }
            vst(a, 0, rd, q);
            true
        }
        _ => false, // UMOV / SMOV: interprete
    }
}

// ---------------------------------------------------------------------------------------------
// MOVI / MVNI / ORR / BIC / FMOV (inmediato vectorial)
// ---------------------------------------------------------------------------------------------

fn mod_imm(cx: &mut Ctx, w: u32, q: bool) -> bool {
    let rd = bits(w, 4, 0);
    let (imm, logic, op) = match crate::neonx::mod_imm_expand(w, q) {
        Some(x) => x,
        None => return false,
    };
    let a = &mut cx.a;
    if !logic {
        a.mov_ri(RAX, imm);
        a.mov_mr(RBX, vo(rd), RAX, true);
        if q {
            a.mov_mr(RBX, vo(rd) + 8, RAX, true);
        } else {
            a.mov_mi64(RBX, vo(rd) + 8, 0);
        }
        return true;
    }
    // ORR: d | imm ; BIC: d & !imm. Con Q = 0 solo cambia la mitad baja y la alta se pone a cero.
    let (opc, val) = if op { (0x21u8, !imm) } else { (0x09, imm) };
    a.mov_ri(RAX, val);
    a.op_rm(true, &[opc], RAX, RBX, vo(rd));
    if q {
        a.op_rm(true, &[opc], RAX, RBX, vo(rd) + 8);
    } else {
        a.mov_mi64(RBX, vo(rd) + 8, 0);
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Prueba diferencial: bloque traducido frente a `interp::exec`
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::super::{BlockFn, Jit, HELPER_CALLS};
    use crate::cpu::Cpu;
    use crate::decode::Op;
    use crate::interp::Flow;
    use std::sync::atomic::Ordering::Relaxed;

    pub struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        pub fn coin(&mut self) -> bool {
            self.next() & 1 != 0
        }
    }

    /// 64 bits de datos con patrones que revelan errores de permutacion (bytes repetidos, rampas, extremos).
    fn pat64(r: &mut Rng) -> u64 {
        match r.below(12) {
            0 => 0,
            1 => u64::MAX,
            2 => 0x8000_0000_0000_0000 >> r.below(64),
            3 => 0x0101_0101_0101_0101 * (r.next() & 0xFF),
            4 => 0x0001_0001_0001_0001 * (r.next() & 0xFFFF),
            5 => 0x7F7F_7F7F_7F7F_7F7F ^ (r.next() & r.next() & 0x0101_0101_0101_0101),
            6 => 0x0706_0504_0302_0100u64.wrapping_add(0x0808_0808_0808_0808 * r.below(4)),
            7 => r.next() & 0x00FF_00FF_00FF_00FF,
            8 => r.below(256) * 0x0000_0001_0000_0001,
            _ => r.next(),
        }
    }

    /// Un valor FP de un registro escalar: especiales y bordes (el compare/min/max solo mira signo, ceros, NaN, inf).
    fn fp_lane(r: &mut Rng, d: bool) -> u64 {
        let (eb, mb) = if d { (11u32, 52u32) } else { (8, 23) };
        let sg = (r.coin() as u64) << (eb + mb);
        let emax = (1u64 << eb) - 1;
        let m = r.next() & ((1u64 << mb) - 1);
        match r.below(12) {
            0 | 1 => sg,                                                          // +-0
            2 => sg | (emax << mb),                                                // +-inf
            3 => sg | (emax << mb) | (1 << (mb - 1)) | m,                         // qNaN
            4 => sg | (emax << mb) | (m.max(1) >> 1).max(1),                      // sNaN
            5 => sg | m.max(1),                                                    // subnormal
            6 => sg | (1 << mb),                                                   // minnormal
            7 => sg | ((1u64 << (eb - 1)) - 1) << mb,                              // +-1.0 (exp = bias)
            8 => sg | ((emax - 1) << mb) | m,                                      // grande
            _ => sg | ((1 + r.below(emax - 1)) << mb) | m,
        }
    }

    #[derive(Clone, Copy, PartialEq)]
    enum Data {
        Int,
        /// escalar FP (s/d): operandos correlacionados (iguales, opuestos, ceros)
        Fp(bool),
        /// carga/almacenamiento: buffer propio
        Mem,
    }

    pub struct Form {
        pub name: String,
        words: Vec<u32>,
        data: Data,
        /// instrucciones previas posibles en el mismo bloque (para el estado perezoso de flags); 0 = ninguna
        prefixes: Vec<u32>,
    }

    fn form(name: &str, data: Data, words: Vec<u32>) -> Form {
        Form { name: name.to_string(), words, data, prefixes: vec![0] }
    }

    /// Palabras validas (el interprete no las trata como indefinidas) de una lista candidata.
    fn valid(words: Vec<u32>) -> Vec<u32> {
        words
            .into_iter()
            .filter(|&w| {
                let mut c = Cpu::new();
                // direcciones de las formas de memoria: no se toca memoria, solo se decide si esta definida
                c.x[31] = 0;
                !matches!(decode_only(&mut c, w), Flow::Undef(_))
            })
            .collect()
    }
    fn decode_only(c: &mut Cpu, w: u32) -> Flow {
        // las formas de memoria no se ejecutan aqui: se decide por el decodificador del propio interprete
        if crate::neon::is_simd_ldst(w) {
            return if crate::neonls::ldst_valid(w) { Flow::Next } else { Flow::Undef(w) };
        }
        crate::interp::exec(c, &crate::decode::decode(w))
    }

    const REGS: [(u32, u32, u32); 8] = [(0, 1, 2), (3, 3, 4), (5, 6, 5), (7, 8, 8), (9, 10, 17), (31, 30, 31), (16, 16, 16), (2, 2, 2)];

    pub fn forms() -> Vec<Form> {
        let mut v: Vec<Form> = Vec::new();
        // ---- LD1 / ST1 (multiples, 1 a 4 registros) ----
        for (load, ln) in [(true, "LD1"), (false, "ST1")] {
            for q in [false, true] {
                for nregs in 1..=4u32 {
                    let opc = [0b0111, 0b1010, 0b0110, 0b0010][nregs as usize - 1];
                    let mut words = Vec::new();
                    for size in 0..4u32 {
                        for (post, rm) in [(0u32, 0u32), (1, 31), (1, 2), (1, 5), (1, 30)] {
                            for (rt, rn) in [(0u32, 1u32), (7, 28), (30, 29), (29, 31), (31, 2), (3, 3), (4, 30)] {
                                words.push(((q as u32) << 30) | (0b0011000 << 23) | (post << 23) | ((load as u32) << 22) | (rm << 16) | (opc << 12) | (size << 10) | (rn << 5) | rt);
                            }
                        }
                    }
                    v.push(form(&format!("{} {{{} reg}} {}", ln, nregs, if q { "128 bits" } else { "64 bits" }), Data::Mem, valid(words)));
                }
            }
        }
        // ---- EXT ----
        for q in [false, true] {
            let mut words = Vec::new();
            for imm4 in 0..16u32 {
                for &(rd, rn, rm) in &REGS {
                    words.push(0x2E00_0000 | ((q as u32) << 30) | (rm << 16) | (imm4 << 11) | (rn << 5) | rd);
                }
            }
            v.push(form(&format!("EXT {}", if q { "16b" } else { "8b" }), Data::Int, valid(words)));
        }
        // ---- REV64 / REV32 / REV16 ----
        for (nm, u, opc, sizes) in [("REV64", 0u32, 0u32, 0..3u32), ("REV32", 1, 0, 0..2), ("REV16", 0, 1, 0..1)] {
            for q in [false, true] {
                let mut words = Vec::new();
                for size in sizes.clone() {
                    for &(rd, rn, _) in &REGS {
                        words.push(0x0E20_0800 | ((q as u32) << 30) | (u << 29) | (size << 22) | (opc << 12) | (rn << 5) | rd);
                    }
                }
                v.push(form(&format!("{} {}", nm, if q { "128 bits" } else { "64 bits" }), Data::Int, valid(words)));
            }
        }
        // ---- ZIP / UZP / TRN ----
        for (nm, opc) in [("UZP1", 1u32), ("TRN1", 2), ("ZIP1", 3), ("UZP2", 5), ("TRN2", 6), ("ZIP2", 7)] {
            for q in [false, true] {
                let mut words = Vec::new();
                for size in 0..4u32 {
                    for &(rd, rn, rm) in &REGS {
                        words.push(0x0E00_0800 | ((q as u32) << 30) | (size << 22) | (rm << 16) | (opc << 12) | (rn << 5) | rd);
                    }
                }
                v.push(form(&format!("{} {}", nm, if q { "128 bits" } else { "64 bits" }), Data::Int, valid(words)));
            }
        }
        // ---- INS / DUP ----
        {
            let (mut ie, mut ig) = (Vec::new(), Vec::new());
            for s in 0..4u32 {
                for idx in 0..(16u32 >> s) {
                    let imm5 = (idx << (s + 1)) | (1 << s);
                    for &(rd, rn, _) in &REGS {
                        ig.push(0x4E00_1C00 | (imm5 << 16) | (rn << 5) | rd);
                        for sidx in [0u32, (16u32 >> s) - 1, (idx + 1) % (16 >> s)] {
                            ie.push(0x6E00_0400 | (imm5 << 16) | ((sidx << s) << 11) | (rn << 5) | rd);
                        }
                    }
                }
            }
            v.push(form("INS (elemento)", Data::Int, valid(ie)));
            v.push(form("INS (general)", Data::Int, valid(ig)));
            for q in [false, true] {
                let (mut de, mut dg) = (Vec::new(), Vec::new());
                for s in 0..4u32 {
                    for idx in 0..(16u32 >> s) {
                        let imm5 = (idx << (s + 1)) | (1 << s);
                        for &(rd, rn, _) in &REGS {
                            de.push(0x0E00_0400 | ((q as u32) << 30) | (imm5 << 16) | (rn << 5) | rd);
                            dg.push(0x0E00_0C00 | ((q as u32) << 30) | (imm5 << 16) | (rn << 5) | rd);
                        }
                    }
                }
                let t = if q { "128 bits" } else { "64 bits" };
                v.push(form(&format!("DUP (elemento) {}", t), Data::Int, valid(de)));
                v.push(form(&format!("DUP (general) {}", t), Data::Int, valid(dg)));
            }
        }
        // ---- MOVI / MVNI / ORR / BIC / FMOV (vector) ----
        {
            let mut r = Rng(0x1234_5678_9abc_def1);
            for q in [false, true] {
                for (nm, logic) in [("MOVI/MVNI/FMOV", false), ("ORR/BIC", true)] {
                    let mut words = Vec::new();
                    for cmode in 0..16u32 {
                        if logic != ((cmode & 1 == 1) && (cmode >> 2) != 0b11) {
                            continue;
                        }
                        for op in 0..2u32 {
                            for _ in 0..24 {
                                let imm8 = r.below(256) as u32;
                                for rd in [0u32, 9, 31] {
                                    words.push(0x0F00_0400 | ((q as u32) << 30) | (op << 29) | ((imm8 >> 5) << 16) | (cmode << 12) | ((imm8 & 31) << 5) | rd);
                                }
                            }
                        }
                    }
                    if !logic {
                        // FMOV (vector, half): o2 = 1, cmode = 1111, op = 0
                        for imm8 in [0u32, 0x70, 0xFF, 0x35] {
                            words.push(0x0F00_0C00 | 0x800 | ((q as u32) << 30) | ((imm8 >> 5) << 16) | (0b1111 << 12) | ((imm8 & 31) << 5) | 1);
                        }
                    }
                    v.push(form(&format!("{} {}", nm, if q { "128 bits" } else { "64 bits" }), Data::Int, valid(words)));
                }
            }
        }
        // ---- pares ----
        for (nm, opc, u) in [("ADDP", 0b10111u32, 0u32), ("SMAXP", 0b10100, 0), ("UMAXP", 0b10100, 1), ("SMINP", 0b10101, 0), ("UMINP", 0b10101, 1)] {
            for q in [false, true] {
                let mut words = Vec::new();
                for size in 0..4u32 {
                    for &(rd, rn, rm) in &REGS {
                        words.push(0x0E20_0400 | ((q as u32) << 30) | (u << 29) | (size << 22) | (rm << 16) | (opc << 11) | (rn << 5) | rd);
                    }
                }
                v.push(form(&format!("{} {}", nm, if q { "128 bits" } else { "64 bits" }), Data::Int, valid(words)));
            }
        }
        // ---- FMAX/FMIN/FMAXNM/FMINNM escalares ----
        for (nm, op) in [("FMAX", 4u32), ("FMIN", 5), ("FMAXNM", 6), ("FMINNM", 7)] {
            for d in [false, true] {
                let words = REGS.iter().map(|&(rd, rn, rm)| 0x1E20_0800 | ((d as u32) << 22) | (rm << 16) | (op << 12) | (rn << 5) | rd).collect();
                v.push(form(&format!("{} escalar {}", nm, if d { "d" } else { "s" }), Data::Fp(d), words));
            }
        }
        // ---- FCCMP / FCCMPE ----
        for (nm, e) in [("FCCMP", 0u32), ("FCCMPE", 1)] {
            for d in [false, true] {
                let mut words = Vec::new();
                for cond in 0..16u32 {
                    for &(_, rn, rm) in &REGS[..4] {
                        words.push(0x1E20_0400 | ((d as u32) << 22) | (rm << 16) | (cond << 12) | (rn << 5) | (e << 4) | (cond.wrapping_mul(7) & 15));
                    }
                }
                let mut f = form(&format!("{} {}", nm, if d { "d" } else { "s" }), Data::Fp(d), words);
                // sin flags previas conocidas / tras FCMP (kind RAW) / tras SUBS (kind SUB64)
                f.prefixes = vec![0, 0x1E22_2020, 0xEB02_003F, 0xAB02_003F];
                v.push(f);
            }
        }
        // ---- FCSEL tras FCMP (regresion: cond_from_raw calculaba N^V con el desplazamiento al reves) ----
        for d in [false, true] {
            let mut words = Vec::new();
            for cond in 0..16u32 {
                for &(rd, rn, rm) in &REGS[..4] {
                    words.push(0x1E20_0C00 | ((d as u32) << 22) | (rm << 16) | (cond << 12) | (rn << 5) | rd);
                }
            }
            let mut f = form(&format!("FCSEL {} tras FCMP (regresion)", if d { "d" } else { "s" }), Data::Fp(d), words);
            f.prefixes = vec![0x1E22_2020];
            v.push(f);
        }
        v
    }

    const SVC: u32 = 0xD400_0001;
    const BUF: usize = 1024;

    /// Ejecuta `n` casos por forma. Devuelve las diferencias totales. `filter`: subcadena del nombre de forma.
    pub fn run(n: usize, seed: u64, verbose: bool, filter: &str) -> u64 {
        let mut r = Rng(seed | 1);
        let mut jit = Jit::new();
        jit.chain = false;
        let mut total = 0u64;
        let mut keep: Vec<Vec<u32>> = Vec::new();
        let mut buf = vec![0u8; BUF];
        for f in forms() {
            if !filter.is_empty() && !f.name.contains(filter) {
                continue;
            }
            assert!(!f.words.is_empty(), "forma sin palabras: {}", f.name);
            // un bloque por (palabra, prefijo)
            let np = f.prefixes.len();
            // bloques de 16 bytes alineados a 16: ninguno cruza una pagina (el JIT corta los bloques en la frontera)
            let mut code = vec![0u32; 4 * f.words.len() * np + 4];
            let off = ((16 - (code.as_ptr() as usize & 15)) & 15) / 4;
            let mut fns: Vec<(u32, u32, BlockFn)> = Vec::new();
            for (i, &w) in f.words.iter().enumerate() {
                for (j, &p) in f.prefixes.iter().enumerate() {
                    let base = off + 4 * (i * np + j);
                    // sin prefijo: NOP en su lugar
                    code[base] = if p == 0 { 0xD503_201F } else { p };
                    code[base + 1] = w;
                    code[base + 2] = SVC;
                    code[base + 3] = 0xD503_201F;
                }
            }
            let cbase = code.as_ptr() as u64 + 4 * off as u64;
            for (i, &w) in f.words.iter().enumerate() {
                for (j, &p) in f.prefixes.iter().enumerate() {
                    let pc = cbase + 16 * (i * np + j) as u64;
                    fns.push((w, p, jit.lookup(pc)));
                }
            }
            let (mut diffs, mut transl) = (0u64, 0u64);
            for c in 0..n {
                let (w, p, bf) = &fns[c % fns.len()];
                let (w, p) = (*w, *p);
                let blk = cbase + 16 * (c % fns.len()) as u64;
                let (rn, rm) = ((w >> 5) & 31, (w >> 16) & 31);
                let mut a = Cpu::new();
                for x in 0..32 {
                    a.v[x] = [pat64(&mut r), pat64(&mut r)];
                    a.x[x] = if r.below(8) == 0 { 0 } else { r.next() };
                }
                a.nzcv = (r.next() & 0xF) << 28;
                if let Data::Fp(d) = f.data {
                    // operandos correlacionados: iguales, opuestos, ceros de signos distintos, aleatorios
                    let sb = 1u64 << if d { 63 } else { 31 };
                    let mask = if d { u64::MAX } else { 0xFFFF_FFFF };
                    let x = fp_lane(&mut r, d);
                    let y = match r.below(6) {
                        0 | 1 => x,
                        2 => x ^ sb,
                        3 => (r.coin() as u64 * sb) & mask,
                        _ => fp_lane(&mut r, d),
                    };
                    let x = if r.below(5) == 0 { (r.coin() as u64 * sb) & mask } else { x };
                    a.v[rn as usize][0] = x;
                    a.v[rm as usize][0] = if rn == rm { x } else { y };
                    if rn == rm {
                        a.v[rn as usize][0] = x;
                    }
                    a.v[rn as usize][1] = 0;
                    a.v[rm as usize][1] = 0;
                    // por si el prefijo es SUBS x1,x2 / FCMP: valores iguales a veces
                    if r.below(3) == 0 {
                        a.x[2] = a.x[1];
                    }
                }
                let mut base_ptr = 0u64;
                if f.data == Data::Mem {
                    for b in buf.iter_mut() {
                        *b = r.next() as u8;
                    }
                    // la zona accesible empieza lejos de los bordes (hasta 64 bytes de acceso)
                    base_ptr = buf.as_ptr() as u64 + 256 + r.below(256);
                    let tag = if r.below(4) == 0 { (r.next() & 0xFF) << 56 } else { 0 };
                    a.x[rn as usize] = base_ptr | tag;
                    if (w >> 23) & 1 == 1 && rm != 31 && rm != rn {
                        // post-indice por registro: desplazamiento pequeno (positivo o negativo) o cualquiera
                        a.x[rm as usize] = match r.below(3) {
                            0 => r.below(300),
                            1 => (-(r.below(300) as i64)) as u64,
                            _ => r.next(),
                        };
                    }
                    // el registro de datos x[rt] es irrelevante; que las bases de otros no apunten fuera: no se usan
                }
                a.fpcr = 0;
                a.fpsr = if r.below(16) == 0 { r.next() & 0x9F } else { 0 };
                let mut b = Cpu::new();
                b.v = a.v;
                b.x = a.x;
                b.nzcv = a.nzcv;
                b.fpcr = a.fpcr;
                b.fpsr = a.fpsr;
                let (v0, x0) = (a.v, a.x);
                let snap = buf.clone();
                // interprete
                a.pc = 0x1000;
                if p != 0 {
                    let fl = crate::interp::exec(&mut a, &crate::decode::decode(p));
                    assert!(matches!(fl, Flow::Next), "{:#x}: prefijo no Next", p);
                }
                let op: Op = crate::decode::decode(w);
                let fl = crate::interp::exec(&mut a, &op);
                assert!(matches!(fl, Flow::Next), "{:#x}: el interprete no devolvio Next", w);
                let mem_a = buf.clone();
                buf.copy_from_slice(&snap);
                // JIT
                b.pc = blk;
                super::super::clear_mxcsr();
                let hc = HELPER_CALLS.load(Relaxed);
                let st = bf(&mut *b);
                let t = HELPER_CALLS.load(Relaxed) - hc == 1;
                super::super::fold_mxcsr(&mut b);
                assert_eq!(st, 1, "{:#x}: el bloque no termino en el SVC", w);
                transl += t as u64;
                let same = a.v == b.v && a.x == b.x && a.fpsr == b.fpsr && a.flags() == b.flags() && mem_a == buf;
                if !same {
                    diffs += 1;
                    if verbose && diffs <= 6 {
                        println!(
                            "DIF {} w={:#010x} prefijo={:#x} base={:#x}\n  v0 n={:016x}_{:016x} m={:016x}_{:016x} d={:016x}_{:016x}\n  interp d={:016x}_{:016x} fpsr={:#x} fl={:#x}\n  jit    d={:016x}_{:016x} fpsr={:#x} fl={:#x}  x_igual={} mem_igual={}",
                            f.name, w, p, base_ptr, v0[rn as usize][1], v0[rn as usize][0], v0[rm as usize][1], v0[rm as usize][0], v0[(w & 31) as usize][1], v0[(w & 31) as usize][0],
                            a.v[(w & 31) as usize][1], a.v[(w & 31) as usize][0], a.fpsr, a.flags(), b.v[(w & 31) as usize][1], b.v[(w & 31) as usize][0], b.fpsr, b.flags(),
                            a.x == b.x, mem_a == buf
                        );
                        let _ = x0;
                    }
                }
            }
            keep.push(code);
            if verbose {
                println!("DIF3B2 {:<28} casos {:>9} diferencias {} | traducidos (sin helper) {:.1} %", f.name, n, diffs, 100.0 * transl as f64 / n as f64);
            }
            total += diffs;
        }
        total
    }

    #[test]
    fn diferencial_neon_suite() {
        assert_eq!(run(20_000, 0x5eed_3b20, true, ""), 0, "el JIT difiere del interprete");
    }

    /// `cargo test --release --lib jitneon::tests::diferencial_neon_completa -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn diferencial_neon_completa() {
        assert_eq!(run(1_000_000, 0x5eed_3b21, true, ""), 0, "el JIT difiere del interprete");
    }
}
