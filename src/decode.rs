//! Decodificador A64 (AArch64). Convierte una palabra de 32 bits en un `Op`.
//!
//! Cobertura de la etapa 1: enteros, saltos, load/store (incluye exclusivas, LSE, pares, literal),
//! sistema (hints, barreras, MRS/MSR, SVC, BRK, SYS). Punto flotante y NEON van en `decode_fp`
//! (se amplían en etapas siguientes). Lo no reconocido devuelve `Op::Undef(word)`.

pub type Reg = u8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shift {
    Lsl,
    Lsr,
    Asr,
    Ror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    Uxtb,
    Uxth,
    Uxtw,
    Uxtx,
    Sxtb,
    Sxth,
    Sxtw,
    Sxtx,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogOp {
    And,
    Orr,
    Eor,
    Ands,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BfOp {
    Sbfm,
    Bfm,
    Ubfm,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CselOp {
    Csel,
    Csinc,
    Csinv,
    Csneg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dp1Op {
    Rbit,
    Rev16,
    Rev32,
    Rev,
    Clz,
    Cls,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtomOp {
    Add,
    Clr,
    Eor,
    Set,
    Smax,
    Smin,
    Umax,
    Umin,
    Swp,
}

/// Modo de direccionamiento de load/store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Addr {
    /// [base, #imm]
    Off(i64),
    /// [base, #imm]!
    Pre(i64),
    /// [base], #imm
    Post(i64),
    /// [base, Rm, ext #amt] (amt ya es el desplazamiento efectivo en bits: 0 o log2(size))
    Reg { rm: Reg, ext: Ext, amt: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Barrier {
    Dmb,
    Dsb,
    Isb,
    Clrex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Undef(u32),
    Nop,
    // ---- inmediatos ----
    Adr { rd: Reg, imm: i64, page: bool },
    AddSubImm { sf: bool, sub: bool, s: bool, rd: Reg, rn: Reg, imm: u64 },
    LogImm { sf: bool, op: LogOp, rd: Reg, rn: Reg, imm: u64 },
    MovWide { sf: bool, kind: u8, rd: Reg, imm: u16, hw: u8 }, // kind: 0 N, 2 Z, 3 K
    Bitfield { sf: bool, op: BfOp, rd: Reg, rn: Reg, immr: u8, imms: u8 },
    Extr { sf: bool, rd: Reg, rn: Reg, rm: Reg, lsb: u8 },
    // ---- registro ----
    AddSubShift { sf: bool, sub: bool, s: bool, rd: Reg, rn: Reg, rm: Reg, shift: Shift, amt: u8 },
    AddSubExt { sf: bool, sub: bool, s: bool, rd: Reg, rn: Reg, rm: Reg, ext: Ext, amt: u8 },
    LogShift { sf: bool, op: LogOp, inv: bool, rd: Reg, rn: Reg, rm: Reg, shift: Shift, amt: u8 },
    AddSubCarry { sf: bool, sub: bool, s: bool, rd: Reg, rn: Reg, rm: Reg },
    CondCmp { sf: bool, neg: bool, imm: bool, rn: Reg, rm: u8, cond: u8, nzcv: u8 },
    CondSel { sf: bool, op: CselOp, rd: Reg, rn: Reg, rm: Reg, cond: u8 },
    Madd { sf: bool, sub: bool, rd: Reg, rn: Reg, rm: Reg, ra: Reg },
    MaddL { signed: bool, sub: bool, rd: Reg, rn: Reg, rm: Reg, ra: Reg },
    MulH { signed: bool, rd: Reg, rn: Reg, rm: Reg },
    Div { sf: bool, signed: bool, rd: Reg, rn: Reg, rm: Reg },
    ShiftV { sf: bool, shift: Shift, rd: Reg, rn: Reg, rm: Reg },
    Dp1 { sf: bool, op: Dp1Op, rd: Reg, rn: Reg },
    Crc32 { castagnoli: bool, size: u8, rd: Reg, rn: Reg, rm: Reg },
    // ---- saltos ----
    B { imm: i64 },
    Bl { imm: i64 },
    Br { rn: Reg },
    Blr { rn: Reg },
    Ret { rn: Reg },
    BCond { cond: u8, imm: i64 },
    Cbz { sf: bool, nz: bool, rt: Reg, imm: i64 },
    Tbz { nz: bool, rt: Reg, bit: u8, imm: i64 },
    // ---- load/store enteros ----
    /// size = log2(bytes); sext: 0 = sin extension, 32/64 = extension con signo al ancho dado.
    Load { size: u8, sext: u8, rt: Reg, rn: Reg, addr: Addr },
    Store { size: u8, rt: Reg, rn: Reg, addr: Addr },
    LoadLit { size: u8, sext: u8, rt: Reg, imm: i64 }, // size 2 (w) /3 (x); sext 64 => LDRSW
    LoadPair { size: u8, sext: bool, rt: Reg, rt2: Reg, rn: Reg, addr: Addr }, // size 2/3; sext => LDPSW
    StorePair { size: u8, rt: Reg, rt2: Reg, rn: Reg, addr: Addr },
    Prefetch,
    // ---- exclusivas / ordenadas ----
    Ldx { size: u8, acq: bool, rt: Reg, rn: Reg },
    Stx { size: u8, rel: bool, rs: Reg, rt: Reg, rn: Reg },
    Ldxp { size: u8, acq: bool, rt: Reg, rt2: Reg, rn: Reg },
    Stxp { size: u8, rel: bool, rs: Reg, rt: Reg, rt2: Reg, rn: Reg },
    Ldar { size: u8, rt: Reg, rn: Reg },
    Stlr { size: u8, rt: Reg, rn: Reg },
    // ---- LSE ----
    Atomic { size: u8, op: AtomOp, acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg },
    Cas { size: u8, acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg },
    Casp { size: u8, acq: bool, rel: bool, rs: Reg, rt: Reg, rn: Reg },
    // ---- sistema ----
    Barrier(Barrier),
    Svc { imm: u16 },
    Brk { imm: u16 },
    Mrs { rt: Reg, sysreg: u16 },
    Msr { rt: Reg, sysreg: u16 },
    /// MSR (inmediato) sobre PSTATE que heddle implementa: solo CFINV (op2=0, FlagM), XAFLAG (1) y AXFLAG (2,
    /// FlagM2), con op1=0 y CRm=0. El resto (SMSTART/SMSTOP, DIT, SSBS, TCO, PAN, UAO, SPSel, DAIFSet/Clr...) es
    /// `Undef`: en EL0 de Linux no existe, no es accesible o heddle no anuncia la extension (`feat`).
    MsrImm { op1: u8, crm: u8, op2: u8 },
    /// RMIF Xn, #lsb, #mask (FlagM)
    Rmif { rn: Reg, lsb: u8, mask: u8 },
    /// SETF8 / SETF16 Wn (FlagM)
    Setf { rn: Reg, w16: bool },
    /// SYS: op1,CRn,CRm,op2 empaquetados (dc zva, ic ivau, dc cvau...)
    Sys { op1: u8, crn: u8, crm: u8, op2: u8, rt: Reg },
    // ---- FP / NEON (decode_fp) ----
    Fp(crate::fp::FpOp),
}

#[inline]
fn bits(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}
#[inline]
fn bit(w: u32, b: u32) -> bool {
    (w >> b) & 1 != 0
}
#[inline]
fn sx(v: u64, nbits: u32) -> i64 {
    let s = 64 - nbits;
    ((v << s) as i64) >> s
}

/// DecodeBitMasks de la especificacion ARM. Devuelve (wmask, tmask) o None si es invalido.
pub fn decode_bit_masks(n: u32, imms: u32, immr: u32, immediate: bool, datasize: u32) -> Option<(u64, u64)> {
    let v = (n << 6) | (!imms & 0x3f);
    if v == 0 {
        return None;
    }
    let len = 31 - v.leading_zeros(); // HighestSetBit(N:NOT(imms))
    if len < 1 {
        return None;
    }
    if datasize < (1 << len) {
        return None;
    }
    let levels = (1u32 << len) - 1;
    if immediate && (imms & levels) == levels {
        return None;
    }
    let s = imms & levels;
    let r = immr & levels;
    let d = s.wrapping_sub(r) & levels;
    let esize = 1u32 << len;
    let ones = |n: u32| -> u64 {
        if n >= 64 {
            u64::MAX
        } else {
            (1u64 << n) - 1
        }
    };
    let emask = ones(esize);
    let welem = ones(s + 1);
    let telem = ones(d + 1);
    let wr = if r == 0 { welem } else { ((welem >> r) | (welem << (esize - r))) & emask };
    let rep = |e: u64| -> u64 {
        let mut out = 0u64;
        let mut i = 0;
        while i < datasize {
            out |= e << i;
            i += esize;
        }
        if datasize == 32 {
            out & 0xFFFF_FFFF
        } else {
            out
        }
    };
    Some((rep(wr), rep(telem)))
}

fn cond_ok(c: u32) -> u8 {
    c as u8
}

pub fn decode(w: u32) -> Op {
    let op0 = bits(w, 28, 25);
    match op0 {
        0b1000 | 0b1001 => decode_dp_imm(w),
        0b1010 | 0b1011 => decode_branch(w),
        0b0100 | 0b0110 | 0b1100 | 0b1110 => decode_ldst(w),
        0b0101 | 0b1101 => decode_dp_reg(w),
        0b0111 | 0b1111 => crate::fp::decode_fp(w),
        _ => Op::Undef(w),
    }
}

fn decode_dp_imm(w: u32) -> Op {
    let sf = bit(w, 31);
    let rd = bits(w, 4, 0) as Reg;
    let rn = bits(w, 9, 5) as Reg;
    match bits(w, 25, 23) {
        0b000 | 0b001 => {
            let immlo = bits(w, 30, 29) as u64;
            let immhi = bits(w, 23, 5) as u64;
            let imm = sx((immhi << 2) | immlo, 21);
            if sf {
                Op::Adr { rd, imm: imm << 12, page: true }
            } else {
                Op::Adr { rd, imm, page: false }
            }
        }
        0b010 => {
            let sh = bit(w, 22);
            let imm12 = bits(w, 21, 10) as u64;
            Op::AddSubImm {
                sf,
                sub: bit(w, 30),
                s: bit(w, 29),
                rd,
                rn,
                imm: if sh { imm12 << 12 } else { imm12 },
            }
        }
        0b100 => {
            let n = bits(w, 22, 22);
            if !sf && n == 1 {
                return Op::Undef(w);
            }
            let immr = bits(w, 21, 16);
            let imms = bits(w, 15, 10);
            let opc = match bits(w, 30, 29) {
                0 => LogOp::And,
                1 => LogOp::Orr,
                2 => LogOp::Eor,
                _ => LogOp::Ands,
            };
            match decode_bit_masks(n, imms, immr, true, if sf { 64 } else { 32 }) {
                Some((wmask, _)) => Op::LogImm { sf, op: opc, rd, rn, imm: wmask },
                None => Op::Undef(w),
            }
        }
        0b101 => {
            let opc = bits(w, 30, 29);
            let hw = bits(w, 22, 21) as u8;
            if opc == 1 || (!sf && hw > 1) {
                return Op::Undef(w);
            }
            Op::MovWide { sf, kind: opc as u8, rd, imm: bits(w, 20, 5) as u16, hw }
        }
        0b110 => {
            let n = bit(w, 22);
            if n != sf {
                return Op::Undef(w);
            }
            let immr = bits(w, 21, 16) as u8;
            let imms = bits(w, 15, 10) as u8;
            if !sf && (immr >= 32 || imms >= 32) {
                return Op::Undef(w);
            }
            let op = match bits(w, 30, 29) {
                0 => BfOp::Sbfm,
                1 => BfOp::Bfm,
                2 => BfOp::Ubfm,
                _ => return Op::Undef(w),
            };
            Op::Bitfield { sf, op, rd, rn, immr, imms }
        }
        0b111 => {
            let n = bit(w, 22);
            if n != sf || bit(w, 21) || bits(w, 30, 29) != 0 {
                return Op::Undef(w);
            }
            let lsb = bits(w, 15, 10) as u8;
            if !sf && lsb >= 32 {
                return Op::Undef(w);
            }
            Op::Extr { sf, rd, rn, rm: bits(w, 20, 16) as Reg, lsb }
        }
        _ => Op::Undef(w),
    }
}

fn decode_branch(w: u32) -> Op {
    let top3 = bits(w, 31, 29);
    match top3 {
        0b000 | 0b100 => {
            let imm = sx(bits(w, 25, 0) as u64, 26) << 2;
            if bit(w, 31) {
                Op::Bl { imm }
            } else {
                Op::B { imm }
            }
        }
        0b001 | 0b101 => {
            if !bit(w, 25) {
                // CBZ/CBNZ
                Op::Cbz {
                    sf: bit(w, 31),
                    nz: bit(w, 24),
                    rt: bits(w, 4, 0) as Reg,
                    imm: sx(bits(w, 23, 5) as u64, 19) << 2,
                }
            } else {
                // TBZ/TBNZ
                let b5 = bits(w, 31, 31);
                let b40 = bits(w, 23, 19);
                Op::Tbz {
                    nz: bit(w, 24),
                    rt: bits(w, 4, 0) as Reg,
                    bit: ((b5 << 5) | b40) as u8,
                    imm: sx(bits(w, 18, 5) as u64, 14) << 2,
                }
            }
        }
        0b010 => {
            // B.cond: 0101010 0 imm19 0 cond
            if bits(w, 31, 24) == 0b0101_0100 && !bit(w, 4) {
                Op::BCond { cond: cond_ok(bits(w, 3, 0)), imm: sx(bits(w, 23, 5) as u64, 19) << 2 }
            } else {
                Op::Undef(w)
            }
        }
        0b110 => decode_sys_branch(w),
        _ => Op::Undef(w),
    }
}

fn decode_sys_branch(w: u32) -> Op {
    // exception generation: 11010100 opc[23:21] imm16 op2[4:2] LL[1:0]
    if bits(w, 31, 24) == 0b1101_0100 {
        let opc = bits(w, 23, 21);
        let ll = bits(w, 1, 0);
        let op2 = bits(w, 4, 2);
        let imm = bits(w, 20, 5) as u16;
        if op2 == 0 {
            if opc == 0 && ll == 1 {
                return Op::Svc { imm };
            }
            if opc == 1 && ll == 0 {
                return Op::Brk { imm };
            }
        }
        return Op::Undef(w);
    }
    // sistema: 1101010100 L op0 op1 CRn CRm op2 Rt
    if bits(w, 31, 22) == 0b1101_0101_00 {
        let l = bit(w, 21);
        let op0 = bits(w, 20, 19);
        let op1 = bits(w, 18, 16) as u8;
        let crn = bits(w, 15, 12) as u8;
        let crm = bits(w, 11, 8) as u8;
        let op2 = bits(w, 7, 5) as u8;
        let rt = bits(w, 4, 0) as Reg;
        if !l && op0 == 0 {
            // hints, barreras, MSR (imm)
            if crn == 2 && op1 == 3 {
                return Op::Nop; // HINT: NOP, YIELD, WFE, SEV, PAC*, BTI, ...
            }
            if crn == 3 {
                return match op2 {
                    2 => Op::Barrier(Barrier::Clrex),
                    4 => Op::Barrier(Barrier::Dsb),
                    5 => Op::Barrier(Barrier::Dmb),
                    6 => Op::Barrier(Barrier::Isb),
                    7 if crm == 0 && crate::feat::need(crate::feat::F_SB) => Op::Nop, // SB (barrera de especulacion)
                    _ => Op::Undef(w),
                };
            }
            if crn == 4 {
                // CFINV/XAFLAG/AXFLAG (op1 = 000) y MSR SSBS, #imm (op1 = 011, op2 = 001: PSTATE.SSBS = CRm<0>)
                let ok = rt == 31
                    && ((op1 == 0 && crm == 0 && ((op2 == 0 && crate::feat::need(crate::feat::F_FLAGM)) || ((op2 == 1 || op2 == 2) && crate::feat::need(crate::feat::F_FLAGM2))))
                        || (op1 == 3 && op2 == 1 && crate::feat::need(crate::feat::F_SSBS)));
                return if ok { Op::MsrImm { op1, crm, op2 } } else { Op::Undef(w) };
            }
            return Op::Undef(w);
        }
        if op0 == 1 {
            // SYS (L=0) accesibles en EL0 de Linux (SCTLR_EL1.UCI/DZE): DC ZVA, DC CVAC, DC CVAU, DC CIVAC, IC IVAU, y
            // DC CVAP / DC CVADP si el modelo tiene DPB / DPB2 (se ejecutan como DC CVAC).
            // El resto (IC IALLU, DC IVAC, TLBI, AT, GCS...) es UNDEFINED en EL0. SYSL no existe en EL0.
            let el0 = match (op1, crn, crm, op2) {
                (3, 7, 4, 1) | (3, 7, 10, 1) | (3, 7, 11, 1) | (3, 7, 14, 1) | (3, 7, 5, 1) => true,
                (3, 7, 12, 1) => crate::feat::need(crate::feat::F_DPB),
                (3, 7, 13, 1) => crate::feat::need(crate::feat::F_DPB2),
                _ => false,
            };
            if !l && el0 {
                return Op::Sys { op1, crn, crm, op2, rt };
            }
            return Op::Undef(w);
        }
        if op0 >= 2 {
            let sysreg = ((op0 as u16) << 14)
                | ((op1 as u16) << 11)
                | ((crn as u16) << 7)
                | ((crm as u16) << 3)
                | op2 as u16;
            // sysreg = op0<<14 | op1<<11 | crn<<7 | crm<<3 | op2 (p. ej. TPIDR_EL0 = 0xDE82)
            // solo los registros accesibles en EL0 de Linux con las extensiones anunciadas (`feat`); el resto, SIGILL
            return match l {
                true if crate::feat::readable(sysreg) => Op::Mrs { rt, sysreg },
                false if crate::feat::writable(sysreg) => Op::Msr { rt, sysreg },
                _ => Op::Undef(w),
            };
        }
        return Op::Undef(w);
    }
    // branch (register): 1101011 opc[24:21] op2[20:16] op3[15:10] Rn[9:5] op4[4:0]
    if bits(w, 31, 25) == 0b1101_011 {
        let opc = bits(w, 24, 21);
        let op2 = bits(w, 20, 16);
        let op3 = bits(w, 15, 10);
        let rn = bits(w, 9, 5) as Reg;
        let op4 = bits(w, 4, 0);
        if op2 != 0b11111 {
            return Op::Undef(w);
        }
        // con PAC (op3=000010/000011: BRAA, BLRAA, RETAA, ERETAA...): heddle no implementa FEAT_PAuth (ningun modelo
        // la anuncia, `feat::IMPL`), asi que son UNDEFINED como en un procesador sin ella. Las formas de pista
        // (PACIASP, AUTIASP, XPACLRI...) estan en el espacio HINT y siguen siendo NOP.
        let pac = op3 == 0b000010 || op3 == 0b000011;
        if pac {
            if matches!(opc, 0b0000 | 0b0001 | 0b0010 | 0b1000 | 0b1001) {
                let _ = crate::feat::need(crate::feat::F_PAUTH);
            }
            return Op::Undef(w);
        }
        if op3 != 0 {
            return Op::Undef(w);
        }
        if op4 != 0 {
            return Op::Undef(w);
        }
        return match opc {
            0b0000 => Op::Br { rn },
            0b0001 => Op::Blr { rn },
            0b0010 => Op::Ret { rn },
            _ => Op::Undef(w),
        };
    }
    Op::Undef(w)
}

fn shift_of(v: u32) -> Shift {
    match v & 3 {
        0 => Shift::Lsl,
        1 => Shift::Lsr,
        2 => Shift::Asr,
        _ => Shift::Ror,
    }
}

fn ext_of(v: u32) -> Ext {
    match v & 7 {
        0 => Ext::Uxtb,
        1 => Ext::Uxth,
        2 => Ext::Uxtw,
        3 => Ext::Uxtx,
        4 => Ext::Sxtb,
        5 => Ext::Sxth,
        6 => Ext::Sxtw,
        _ => Ext::Sxtx,
    }
}

fn decode_dp_reg(w: u32) -> Op {
    let sf = bit(w, 31);
    let rd = bits(w, 4, 0) as Reg;
    let rn = bits(w, 9, 5) as Reg;
    let rm = bits(w, 20, 16) as Reg;
    // FlagM: RMIF (1 0 1 11010000 imm6 00001 Rn 0 mask) y SETF8/SETF16 (0 0 1 11010000 000000 sz 0010 Rn 0 1101)
    if w & 0xFFE0_7C10 == 0xBA00_0400 {
        if !crate::feat::need(crate::feat::F_FLAGM) {
            return Op::Undef(w);
        }
        return Op::Rmif { rn, lsb: bits(w, 20, 15) as u8, mask: bits(w, 3, 0) as u8 };
    }
    if w & 0xFFFF_BC1F == 0x3A00_080D {
        if !crate::feat::need(crate::feat::F_FLAGM) {
            return Op::Undef(w);
        }
        return Op::Setf { rn, w16: bit(w, 14) };
    }
    if !bit(w, 28) {
        // op1 = 0
        if !bit(w, 24) {
            // logical shifted
            let amt = bits(w, 15, 10) as u8;
            if !sf && amt >= 32 {
                return Op::Undef(w);
            }
            let op = match bits(w, 30, 29) {
                0 => LogOp::And,
                1 => LogOp::Orr,
                2 => LogOp::Eor,
                _ => LogOp::Ands,
            };
            return Op::LogShift { sf, op, inv: bit(w, 21), rd, rn, rm, shift: shift_of(bits(w, 23, 22)), amt };
        }
        if !bit(w, 21) {
            // add/sub shifted
            let amt = bits(w, 15, 10) as u8;
            let shift = shift_of(bits(w, 23, 22));
            if shift == Shift::Ror || (!sf && amt >= 32) {
                return Op::Undef(w);
            }
            return Op::AddSubShift { sf, sub: bit(w, 30), s: bit(w, 29), rd, rn, rm, shift, amt };
        }
        // add/sub extended: opt[23:22]=00
        if bits(w, 23, 22) != 0 {
            return Op::Undef(w);
        }
        let amt = bits(w, 12, 10) as u8;
        if amt > 4 {
            return Op::Undef(w);
        }
        return Op::AddSubExt { sf, sub: bit(w, 30), s: bit(w, 29), rd, rn, rm, ext: ext_of(bits(w, 15, 13)), amt };
    }
    // op1 = 1
    match bits(w, 24, 21) {
        0b0000 => {
            if bits(w, 15, 10) != 0 {
                return Op::Undef(w);
            }
            Op::AddSubCarry { sf, sub: bit(w, 30), s: bit(w, 29), rd, rn, rm }
        }
        0b0010 => {
            // conditional compare: S=1, o2[10]=0, o3[4]=0
            if !bit(w, 29) || bit(w, 10) || bit(w, 4) {
                return Op::Undef(w);
            }
            Op::CondCmp {
                sf,
                neg: !bit(w, 30),
                imm: bit(w, 11),
                rn,
                rm: bits(w, 20, 16) as u8,
                cond: bits(w, 15, 12) as u8,
                nzcv: bits(w, 3, 0) as u8,
            }
        }
        0b0100 => {
            if bit(w, 29) {
                return Op::Undef(w);
            }
            let op2 = bits(w, 11, 10);
            let op = match (bit(w, 30), op2) {
                (false, 0) => CselOp::Csel,
                (false, 1) => CselOp::Csinc,
                (true, 0) => CselOp::Csinv,
                (true, 1) => CselOp::Csneg,
                _ => return Op::Undef(w),
            };
            Op::CondSel { sf, op, rd, rn, rm, cond: bits(w, 15, 12) as u8 }
        }
        0b0110 => {
            if bit(w, 29) {
                return Op::Undef(w);
            }
            let opcode = bits(w, 15, 10);
            if !bit(w, 30) {
                // 2 source
                return match opcode {
                    0b000010 => Op::Div { sf, signed: false, rd, rn, rm },
                    0b000011 => Op::Div { sf, signed: true, rd, rn, rm },
                    0b001000 => Op::ShiftV { sf, shift: Shift::Lsl, rd, rn, rm },
                    0b001001 => Op::ShiftV { sf, shift: Shift::Lsr, rd, rn, rm },
                    0b001010 => Op::ShiftV { sf, shift: Shift::Asr, rd, rn, rm },
                    0b001011 => Op::ShiftV { sf, shift: Shift::Ror, rd, rn, rm },
                    0b010000..=0b010111 => {
                        let sz = (opcode & 3) as u8;
                        let c = opcode & 4 != 0;
                        // CRC32X/CX requieren sf=1 y los demas sf=0
                        if (sz == 3) != sf || !crate::feat::need(crate::feat::F_CRC32) {
                            return Op::Undef(w);
                        }
                        Op::Crc32 { castagnoli: c, size: sz, rd, rn, rm }
                    }
                    _ => Op::Undef(w),
                };
            }
            // 1 source
            if bits(w, 20, 16) != 0 {
                return Op::Undef(w);
            }
            let op = match (opcode, sf) {
                (0b000000, _) => Dp1Op::Rbit,
                (0b000001, _) => Dp1Op::Rev16,
                (0b000010, true) => Dp1Op::Rev32,
                (0b000010, false) => Dp1Op::Rev,
                (0b000011, true) => Dp1Op::Rev,
                (0b000100, _) => Dp1Op::Clz,
                (0b000101, _) => Dp1Op::Cls,
                _ => return Op::Undef(w),
            };
            Op::Dp1 { sf, op, rd, rn }
        }
        0b1000..=0b1111 => {
            // 3 source: op31 = bits[23:21], o0 = bit 15
            if bits(w, 30, 29) != 0 {
                return Op::Undef(w);
            }
            let ra = bits(w, 14, 10) as Reg;
            let o0 = bit(w, 15);
            match bits(w, 23, 21) {
                0b000 => Op::Madd { sf, sub: o0, rd, rn, rm, ra },
                0b001 if sf => Op::MaddL { signed: true, sub: o0, rd, rn, rm, ra },
                0b101 if sf => Op::MaddL { signed: false, sub: o0, rd, rn, rm, ra },
                0b010 if sf && !o0 && ra == 31 => Op::MulH { signed: true, rd, rn, rm },
                0b110 if sf && !o0 && ra == 31 => Op::MulH { signed: false, rd, rn, rm },
                _ => Op::Undef(w),
            }
        }
        _ => Op::Undef(w),
    }
}

fn decode_ldst(w: u32) -> Op {
    let size = bits(w, 31, 30) as u8;
    let v = bit(w, 26);
    let rt = bits(w, 4, 0) as Reg;
    let rn = bits(w, 9, 5) as Reg;

    // ---- LD/ST estructuras SIMD (multiples y de un elemento) ----
    if !bit(w, 31) && bits(w, 29, 25) == 0b00110 {
        return crate::fp::decode_fp(w);
    }

    // ---- exclusivas / ordenadas / CAS: bits[29:24] = 001000 ----
    if bits(w, 29, 24) == 0b001000 && !v {
        let o2 = bit(w, 23);
        let l = bit(w, 22);
        let o1 = bit(w, 21);
        let rs = bits(w, 20, 16) as Reg;
        let o0 = bit(w, 15);
        let rt2 = bits(w, 14, 10) as Reg;
        return match (o2, o1) {
            (false, false) => {
                if l {
                    if rs != 31 || rt2 != 31 {
                        return Op::Undef(w);
                    }
                    Op::Ldx { size, acq: o0, rt, rn }
                } else {
                    Op::Stx { size, rel: o0, rs, rt, rn }
                }
            }
            (false, true) => {
                if bit(w, 31) {
                    // pares exclusivos: size[30] = 0 -> 32 bit, 1 -> 64 bit
                    let sz = if bit(w, 30) { 3 } else { 2 };
                    if l {
                        if rs != 31 {
                            return Op::Undef(w);
                        }
                        Op::Ldxp { size: sz, acq: o0, rt, rt2, rn }
                    } else {
                        Op::Stxp { size: sz, rel: o0, rs, rt, rt2, rn }
                    }
                } else {
                    // CASP
                    // Rs y Rt impares: UNDEFINED
                    if rt2 != 31 || rs & 1 != 0 || rt & 1 != 0 || !crate::feat::need(crate::feat::F_LSE) {
                        return Op::Undef(w);
                    }
                    Op::Casp { size: if bit(w, 30) { 3 } else { 2 }, acq: l, rel: o0, rs, rt, rn }
                }
            }
            (true, false) => {
                // o0 = 0: LDLAR/STLLR (FEAT_LOR, ARMv8.1); sin regiones LOR configuradas valen LDAR/STLR
                if rs != 31 || rt2 != 31 || (!o0 && !crate::feat::need(crate::feat::F_LOR)) {
                    return Op::Undef(w);
                }
                if l {
                    Op::Ldar { size, rt, rn }
                } else {
                    Op::Stlr { size, rt, rn }
                }
            }
            (true, true) => {
                if rt2 != 31 || !crate::feat::need(crate::feat::F_LSE) {
                    return Op::Undef(w);
                }
                Op::Cas { size, acq: l, rel: o0, rs, rt, rn }
            }
        };
    }

    // ---- literal: opc 011 V 00 ----
    if bits(w, 29, 27) == 0b011 && bits(w, 25, 24) == 0 {
        let imm = sx(bits(w, 23, 5) as u64, 19) << 2;
        if v {
            return crate::fp::decode_fp(w);
        }
        return match size {
            0 => Op::LoadLit { size: 2, sext: 0, rt, imm },
            1 => Op::LoadLit { size: 3, sext: 0, rt, imm },
            2 => Op::LoadLit { size: 2, sext: 64, rt, imm },
            _ => Op::Prefetch,
        };
    }

    // ---- pares: opc 101 V ----
    if bits(w, 29, 27) == 0b101 {
        if v {
            return crate::fp::decode_fp(w);
        }
        let l = bit(w, 22);
        let imm7 = sx(bits(w, 21, 15) as u64, 7);
        let rt2 = bits(w, 14, 10) as Reg;
        let (sz, sext) = match size {
            0 => (2u8, false),
            // LDPSW; no hay forma sin asignacion (bits 24:23 = 00) ni almacenamiento (STGP es de MTE)
            1 => {
                if l && bits(w, 24, 23) != 0 {
                    (2, true)
                } else {
                    return Op::Undef(w);
                }
            }
            2 => (3, false),
            _ => return Op::Undef(w),
        };
        let off = imm7 << sz;
        let addr = match bits(w, 24, 23) {
            0b00 | 0b10 => Addr::Off(off),
            0b01 => Addr::Post(off),
            _ => Addr::Pre(off),
        };
        return if l {
            Op::LoadPair { size: sz, sext, rt, rt2, rn, addr }
        } else {
            Op::StorePair { size: sz, rt, rt2, rn, addr }
        };
    }

    // ---- registro: size 111 V 00 ----
    if bits(w, 29, 27) == 0b111 && !bit(w, 25) {
        if v {
            return crate::fp::decode_fp(w);
        }
        if !bit(w, 24) && bit(w, 21) && bits(w, 11, 10) == 0b00 {
            // LSE atomics: A[23] R[22] 1 Rs o3[15] opc[14:12] 00 Rn Rt
            let a = bit(w, 23);
            let r = bit(w, 22);
            let rs = bits(w, 20, 16) as Reg;
            let o3 = bit(w, 15);
            let opc3 = bits(w, 14, 12);
            if o3 && opc3 == 0b100 && rs == 31 && a && !r {
                // LDAPR (FEAT_LRCPC)
                return if crate::feat::need(crate::feat::F_LRCPC) { Op::Ldar { size, rt, rn } } else { Op::Undef(w) };
            }
            if !crate::feat::need(crate::feat::F_LSE) {
                return Op::Undef(w);
            }
            if o3 {
                if opc3 == 0 {
                    return Op::Atomic { size, op: AtomOp::Swp, acq: a, rel: r, rs, rt, rn };
                }
                return Op::Undef(w);
            }
            let op = match opc3 {
                0 => AtomOp::Add,
                1 => AtomOp::Clr,
                2 => AtomOp::Eor,
                3 => AtomOp::Set,
                4 => AtomOp::Smax,
                5 => AtomOp::Smin,
                6 => AtomOp::Umax,
                _ => AtomOp::Umin,
            };
            return Op::Atomic { size, op, acq: a, rel: r, rs, rt, rn };
        }
        let opc = bits(w, 23, 22);
        if size == 3 && bit(w, 23) && !bit(w, 24) && bit(w, 21) && bit(w, 10) {
            // LDRAA/LDRAB (M = bit 23, W = bit 11): FEAT_PAuth, que heddle no implementa
            let _ = crate::feat::need(crate::feat::F_PAUTH);
            return Op::Undef(w);
        }
        let (is_load, sext) = match (opc, size) {
            (0, _) => (false, 0u8),
            (1, _) => (true, 0),
            // PRFM (inmediato), PRFUM y PRFM (registro); el resto de modos con size=11 opc=10 no existe
            (2, 3) => {
                let ok = bit(w, 24)
                    || (!bit(w, 21) && bits(w, 11, 10) == 0b00)
                    || (bit(w, 21) && bits(w, 11, 10) == 0b10 && bit(w, 14));
                return if ok { Op::Prefetch } else { Op::Undef(w) };
            }
            (2, _) => (true, 64),
            (3, 0) | (3, 1) => (true, 32),
            _ => return Op::Undef(w),
        };
        let mk = |addr: Addr| -> Op {
            if is_load {
                Op::Load { size, sext, rt, rn, addr }
            } else {
                Op::Store { size, rt, rn, addr }
            }
        };
        if bit(w, 24) {
            let imm = (bits(w, 21, 10) as i64) << size;
            return mk(Addr::Off(imm));
        }
        if !bit(w, 21) {
            let imm9 = sx(bits(w, 20, 12) as u64, 9);
            return match bits(w, 11, 10) {
                0b00 | 0b10 => mk(Addr::Off(imm9)),
                0b01 => mk(Addr::Post(imm9)),
                _ => mk(Addr::Pre(imm9)),
            };
        }
        match bits(w, 11, 10) {
            0b10 => {
                let ext = ext_of(bits(w, 15, 13));
                if !matches!(ext, Ext::Uxtw | Ext::Uxtx | Ext::Sxtw | Ext::Sxtx) {
                    return Op::Undef(w);
                }
                let s = bit(w, 12);
                mk(Addr::Reg { rm: bits(w, 20, 16) as Reg, ext, amt: if s { size } else { 0 } })
            }
            _ => Op::Undef(w),
        }
    } else {
        Op::Undef(w)
    }
}
