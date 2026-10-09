//! Interprete de referencia de A64. Es la especificacion ejecutable del traductor: el JIT se valida
//! contra el en pruebas diferenciales, y este contra Unicorn (QEMU).

use crate::cpu::*;
use crate::decode::*;
use crate::monitor;

/// Mascara TBI: Android usa el byte alto del puntero como etiqueta (heap tagging/MTE).
pub const TBI_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Next,
    Jump(u64),
    Svc(u16),
    Brk(u16),
    Undef(u32),
    /// IC IVAU / IC IALLU: hay que invalidar codigo traducido (addr, 0 = todo)
    ICacheFlush(u64),
}

#[inline]
fn mask(sf: bool) -> u64 {
    if sf {
        u64::MAX
    } else {
        0xFFFF_FFFF
    }
}

#[inline]
fn ror(v: u64, amt: u32, sf: bool) -> u64 {
    if sf {
        v.rotate_right(amt & 63)
    } else {
        (v as u32).rotate_right(amt & 31) as u64
    }
}

pub fn shift_val(v: u64, sh: Shift, amt: u32, sf: bool) -> u64 {
    let v = v & mask(sf);
    let bits = if sf { 64 } else { 32 };
    let amt = amt % bits;
    match sh {
        Shift::Lsl => (v << amt) & mask(sf),
        Shift::Lsr => v >> amt,
        Shift::Asr => {
            if sf {
                ((v as i64) >> amt) as u64
            } else {
                (((v as u32) as i32) >> amt) as u32 as u64
            }
        }
        Shift::Ror => ror(v, amt, sf),
    }
}

pub fn ext_val(v: u64, ext: Ext, amt: u32) -> u64 {
    let e = match ext {
        Ext::Uxtb => v as u8 as u64,
        Ext::Uxth => v as u16 as u64,
        Ext::Uxtw => v as u32 as u64,
        Ext::Uxtx => v,
        Ext::Sxtb => v as u8 as i8 as i64 as u64,
        Ext::Sxth => v as u16 as i16 as i64 as u64,
        Ext::Sxtw => v as u32 as i32 as i64 as u64,
        Ext::Sxtx => v,
    };
    e << amt
}

#[inline]
pub(crate) fn rx(c: &Cpu, r: Reg) -> u64 {
    if r == 31 {
        0
    } else {
        c.x[r as usize]
    }
}
#[inline]
pub(crate) fn wx(c: &mut Cpu, r: Reg, v: u64) {
    if r != 31 {
        c.x[r as usize] = v;
    }
}
#[inline]
pub(crate) fn rsp(c: &Cpu, r: Reg) -> u64 {
    c.x[r as usize]
}
#[inline]
pub(crate) fn wsp(c: &mut Cpu, r: Reg, v: u64) {
    c.x[r as usize] = v;
}

fn crc32_update(mut crc: u32, data: u64, nbytes: u32, poly: u32) -> u32 {
    for i in 0..nbytes {
        crc ^= ((data >> (8 * i)) & 0xFF) as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ poly } else { crc >> 1 };
        }
    }
    crc
}

#[inline]
fn sext_to(v: u64, size: u8, to: u8) -> u64 {
    let bits = 8u32 << size;
    let s = (((v << (64 - bits)) as i64) >> (64 - bits)) as u64;
    if to == 32 {
        s & 0xFFFF_FFFF
    } else {
        s
    }
}

/// Direccion efectiva y write-back. Devuelve la direccion de acceso (ya con TBI aplicado).
pub(crate) fn eff_addr(c: &mut Cpu, rn: Reg, addr: Addr) -> u64 {
    let base = rsp(c, rn);
    let a = match addr {
        Addr::Off(i) => base.wrapping_add(i as u64),
        // pre-indice: la base se escribe despues del acceso (`post_wb`): si el acceso falla, queda sin cambiar
        Addr::Pre(i) => base.wrapping_add(i as u64),
        Addr::Post(_) => base,
        Addr::Reg { rm, ext, amt } => base.wrapping_add(ext_val(rx(c, rm), ext, amt as u32)),
    };
    a & TBI_MASK
}

/// Escritura de la base de un pre o post-indice, despues del acceso (como en el pseudocodigo del Arm ARM: un acceso
/// que falla no la cambia).
pub(crate) fn post_wb(c: &mut Cpu, rn: Reg, addr: Addr, base_before: u64) {
    if let Addr::Post(i) | Addr::Pre(i) = addr {
        wsp(c, rn, base_before.wrapping_add(i as u64));
    }
}

/// Exclusivos, atomicos y accesos ordenados (LDAR/STLR/LDAPR): direccion alineada a `n` bytes o SIGBUS BUS_ADRALN en
/// esta instruccion (ver `sig::align_fault`), antes de cualquier acceso o de mirar el monitor.
#[inline]
fn check_align(a: u64, n: u64) {
    if a & (n - 1) != 0 {
        crate::sig::align_fault(a)
    }
}

#[inline]
unsafe fn load(addr: u64, size: u8) -> u64 {
    monitor::rd(addr as usize, size)
}

#[inline]
fn store(addr: u64, size: u8, v: u64) {
    monitor::store(addr as usize, size, v);
}

fn atom_apply(op: AtomOp, old: u64, rs: u64, size: u8) -> u64 {
    let bits = 8u32 << size;
    let m = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
    let s = |x: u64| -> i64 { ((x << (64 - bits)) as i64) >> (64 - bits) };
    let (old, rs) = (old & m, rs & m);
    let r = match op {
        AtomOp::Add => old.wrapping_add(rs),
        AtomOp::Clr => old & !rs,
        AtomOp::Eor => old ^ rs,
        AtomOp::Set => old | rs,
        AtomOp::Smax => {
            if s(old) >= s(rs) {
                old
            } else {
                rs
            }
        }
        AtomOp::Smin => {
            if s(old) <= s(rs) {
                old
            } else {
                rs
            }
        }
        AtomOp::Umax => old.max(rs),
        AtomOp::Umin => old.min(rs),
        AtomOp::Swp => rs,
    };
    r & m
}

/// Frecuencia del contador generico (CNTFRQ_EL0): 19,2 MHz, la de los SoC Qualcomm (la mas comun en Android; otros
/// usan 26 o 24,576 MHz). `/proc/cpuinfo` la refleja en BogoMIPS (frecuencia / 500 000 = 38.40).
pub const CNTFRQ: u64 = 19_200_000;

/// CNTVCT_EL0: el reloj monotono del host en ticks de `CNTFRQ` (ns * 19,2e6 / 1e9 = ns * 12 / 625).
pub fn cntvct() -> u64 {
    let mut ts = [0i64; 2];
    extern "C" {
        fn clock_gettime(clk: i32, ts: *mut i64) -> i32;
    }
    unsafe { clock_gettime(1, ts.as_mut_ptr()) };
    let ns = (ts[0] as u64) * 1_000_000_000 + ts[1] as u64;
    // floor(ns * 12 / 625) sin desbordar
    ns / 625 * 12 + ns % 625 * 12 / 625
}

pub fn read_sysreg(c: &Cpu, sysreg: u16) -> u64 {
    match sysreg {
        0xDE82 => c.tpidr,
        0xDA10 => c.flags(),
        0xDA20 => c.fpcr,
        0xDA21 => c.fpsr,
        0xDF00 => CNTFRQ,
        0xDF02 => cntvct(),
        0xD801 => crate::feat::ctr(),   // CTR_EL0 del modelo (lineas de 64 B)
        0xD807 => crate::feat::dczid(), // DCZID_EL0: DC ZVA habilitado, bloque de 64 B
        crate::feat::SSBS => c.ssbs(),
        // espacio de identificacion (MIDR, ID_AA64*) como lo emula Linux; TPIDRRO_EL0 vale 0 en un proceso arm64
        _ => crate::feat::id_reg(sysreg).unwrap_or(0),
    }
}

/// Bits de FPCR que se pueden escribir (MSR, `fesetenv`, `uc_mcontext` al volver de una senal): AHP, DN, FZ,
/// RMode y FZ16 (FEAT_FP16). Las habilitaciones de trampas (IOE..IDE) son RAZ/WI porque heddle no implementa las
/// trampas de coma flotante; Len/Stride son RES0 sin AArch32 (ID_AA64PFR0_EL1.EL0 = AArch64 solo).
pub const FPCR_RW: u64 = 0x07C8_0000;
/// Bits de FPSR que se pueden escribir: N, Z, C, V, QC y las flags acumuladas (IOC..IXC, IDC).
pub const FPSR_RW: u64 = 0xF800_009F;

pub fn write_sysreg(c: &mut Cpu, sysreg: u16, v: u64) {
    match sysreg {
        0xDE82 => c.tpidr = v,
        0xDA10 => c.set_flags(v),
        0xDA20 => c.fpcr = v & FPCR_RW,
        0xDA21 => c.fpsr = v & FPSR_RW,
        crate::feat::SSBS => c.set_ssbs(v),
        _ => {}
    }
}

/// Ejecuta una instruccion ya decodificada. `c.pc` es la direccion de esta instruccion.
pub fn exec(c: &mut Cpu, op: &Op) -> Flow {
    match *op {
        Op::Undef(w) => return Flow::Undef(w),
        Op::Nop | Op::Prefetch => {}
        Op::Adr { rd, imm, page } => {
            let base = if page { c.pc & !0xFFF } else { c.pc };
            wx(c, rd, base.wrapping_add(imm as u64));
        }
        Op::AddSubImm { sf, sub, s, rd, rn, imm } => {
            let a = rsp(c, rn) & mask(sf);
            let (res, f) = if sub { add_flags(a, !imm & mask(sf), 1, sf) } else { add_flags(a, imm, 0, sf) };
            if s {
                c.set_flags(f);
                wx(c, rd, res);
            } else {
                wsp(c, rd, res);
            }
        }
        Op::LogImm { sf, op, rd, rn, imm } => {
            let a = rx(c, rn) & mask(sf);
            let r = match op {
                LogOp::And | LogOp::Ands => a & imm,
                LogOp::Orr => a | imm,
                LogOp::Eor => a ^ imm,
            };
            if op == LogOp::Ands {
                c.set_flags(logic_flags(r, sf));
                wx(c, rd, r);
            } else {
                wsp(c, rd, r);
            }
        }
        Op::MovWide { sf, kind, rd, imm, hw } => {
            let sh = hw as u32 * 16;
            let v = (imm as u64) << sh;
            let r = match kind {
                0 => !v & mask(sf),
                2 => v,
                _ => (rx(c, rd) & !(0xFFFFu64 << sh) | v) & mask(sf),
            };
            wx(c, rd, r);
        }
        Op::Bitfield { sf, op, rd, rn, immr, imms } => {
            let ds = if sf { 64 } else { 32 };
            let (wmask, tmask) = match decode_bit_masks(sf as u32, imms as u32, immr as u32, false, ds) {
                Some(x) => x,
                None => return Flow::Undef(0),
            };
            let src = rx(c, rn) & mask(sf);
            let dst = rx(c, rd) & mask(sf);
            let rot = ror(src, immr as u32, sf);
            let r = match op {
                BfOp::Ubfm => rot & wmask & tmask,
                BfOp::Sbfm => {
                    let bot = rot & wmask;
                    let bit = (src >> imms) & 1;
                    let top = if bit == 1 { mask(sf) } else { 0 };
                    (top & !tmask) | (bot & tmask)
                }
                BfOp::Bfm => {
                    let bot = (dst & !wmask) | (rot & wmask);
                    (dst & !tmask) | (bot & tmask)
                }
            };
            wx(c, rd, r & mask(sf));
        }
        Op::Extr { sf, rd, rn, rm, lsb } => {
            let hi = rx(c, rn) & mask(sf);
            let lo = rx(c, rm) & mask(sf);
            let r = if lsb == 0 {
                lo
            } else if sf {
                (lo >> lsb) | (hi << (64 - lsb))
            } else {
                ((lo >> lsb) | (hi << (32 - lsb))) & 0xFFFF_FFFF
            };
            wx(c, rd, r);
        }
        Op::AddSubShift { sf, sub, s, rd, rn, rm, shift, amt } => {
            let a = rx(c, rn) & mask(sf);
            let b = shift_val(rx(c, rm), shift, amt as u32, sf);
            let (res, f) = if sub { add_flags(a, !b & mask(sf), 1, sf) } else { add_flags(a, b, 0, sf) };
            if s {
                c.set_flags(f);
            }
            wx(c, rd, res);
        }
        Op::AddSubExt { sf, sub, s, rd, rn, rm, ext, amt } => {
            let a = rsp(c, rn) & mask(sf);
            let b = ext_val(rx(c, rm), ext, amt as u32) & mask(sf);
            let (res, f) = if sub { add_flags(a, !b & mask(sf), 1, sf) } else { add_flags(a, b, 0, sf) };
            if s {
                c.set_flags(f);
                wx(c, rd, res);
            } else {
                wsp(c, rd, res);
            }
        }
        Op::LogShift { sf, op, inv, rd, rn, rm, shift, amt } => {
            let a = rx(c, rn) & mask(sf);
            let mut b = shift_val(rx(c, rm), shift, amt as u32, sf);
            if inv {
                b = !b & mask(sf);
            }
            let r = match op {
                LogOp::And | LogOp::Ands => a & b,
                LogOp::Orr => a | b,
                LogOp::Eor => a ^ b,
            };
            if op == LogOp::Ands {
                c.set_flags(logic_flags(r, sf));
            }
            wx(c, rd, r);
        }
        Op::AddSubCarry { sf, sub, s, rd, rn, rm } => {
            let a = rx(c, rn) & mask(sf);
            let mut b = rx(c, rm) & mask(sf);
            if sub {
                b = !b & mask(sf);
            }
            let cin = (c.flags() >> 29) & 1;
            let (res, f) = add_flags(a, b, cin, sf);
            if s {
                c.set_flags(f);
            }
            wx(c, rd, res);
        }
        Op::CondCmp { sf, neg, imm, rn, rm, cond, nzcv } => {
            if c.cond_holds(cond) {
                let a = rx(c, rn) & mask(sf);
                let b = if imm { rm as u64 } else { rx(c, rm) & mask(sf) };
                let (_, f) = if neg { add_flags(a, b, 0, sf) } else { add_flags(a, !b & mask(sf), 1, sf) };
                c.set_flags(f);
            } else {
                c.set_flags((nzcv as u64) << 28);
            }
        }
        Op::CondSel { sf, op, rd, rn, rm, cond } => {
            let a = rx(c, rn) & mask(sf);
            let b = rx(c, rm) & mask(sf);
            let r = if c.cond_holds(cond) {
                a
            } else {
                match op {
                    CselOp::Csel => b,
                    CselOp::Csinc => b.wrapping_add(1) & mask(sf),
                    CselOp::Csinv => !b & mask(sf),
                    CselOp::Csneg => b.wrapping_neg() & mask(sf),
                }
            };
            wx(c, rd, r);
        }
        Op::Madd { sf, sub, rd, rn, rm, ra } => {
            let p = rx(c, rn).wrapping_mul(rx(c, rm));
            let a = rx(c, ra);
            let r = if sub { a.wrapping_sub(p) } else { a.wrapping_add(p) };
            wx(c, rd, r & mask(sf));
        }
        Op::MaddL { signed, sub, rd, rn, rm, ra } => {
            let (x, y) = if signed {
                (rx(c, rn) as u32 as i32 as i64 as u64, rx(c, rm) as u32 as i32 as i64 as u64)
            } else {
                (rx(c, rn) as u32 as u64, rx(c, rm) as u32 as u64)
            };
            let p = x.wrapping_mul(y);
            let a = rx(c, ra);
            wx(c, rd, if sub { a.wrapping_sub(p) } else { a.wrapping_add(p) });
        }
        Op::MulH { signed, rd, rn, rm } => {
            let (x, y) = (rx(c, rn), rx(c, rm));
            let r = if signed {
                (((x as i64 as i128) * (y as i64 as i128)) >> 64) as u64
            } else {
                (((x as u128) * (y as u128)) >> 64) as u64
            };
            wx(c, rd, r);
        }
        Op::Div { sf, signed, rd, rn, rm } => {
            let (x, y) = (rx(c, rn) & mask(sf), rx(c, rm) & mask(sf));
            let r = if y == 0 {
                0
            } else if signed {
                if sf {
                    (x as i64).wrapping_div(y as i64) as u64
                } else {
                    (x as u32 as i32).wrapping_div(y as u32 as i32) as u32 as u64
                }
            } else {
                x / y
            };
            wx(c, rd, r);
        }
        Op::ShiftV { sf, shift, rd, rn, rm } => {
            let amt = (rx(c, rm) as u32) % if sf { 64 } else { 32 };
            let r = shift_val(rx(c, rn), shift, amt, sf);
            wx(c, rd, r);
        }
        Op::Dp1 { sf, op, rd, rn } => {
            let v = rx(c, rn) & mask(sf);
            let r = match op {
                Dp1Op::Rbit => {
                    if sf {
                        v.reverse_bits()
                    } else {
                        (v as u32).reverse_bits() as u64
                    }
                }
                Dp1Op::Rev16 => {
                    let t = ((v & 0x00FF_00FF_00FF_00FF) << 8) | ((v >> 8) & 0x00FF_00FF_00FF_00FF);
                    t & mask(sf)
                }
                Dp1Op::Rev32 => {
                    let lo = (v as u32).swap_bytes() as u64;
                    let hi = ((v >> 32) as u32).swap_bytes() as u64;
                    (hi << 32) | lo
                }
                Dp1Op::Rev => {
                    if sf {
                        v.swap_bytes()
                    } else {
                        (v as u32).swap_bytes() as u64
                    }
                }
                Dp1Op::Clz => {
                    if sf {
                        v.leading_zeros() as u64
                    } else {
                        (v as u32).leading_zeros() as u64
                    }
                }
                Dp1Op::Cls => {
                    if sf {
                        (((v ^ (((v as i64) >> 1) as u64)).leading_zeros()) - 1) as u64
                    } else {
                        ((((v as u32) ^ (((v as u32 as i32) >> 1) as u32)).leading_zeros()) - 1) as u64
                    }
                }
            };
            wx(c, rd, r);
        }
        Op::Crc32 { castagnoli, size, rd, rn, rm } => {
            let nbytes = 1u32 << size;
            let poly = if castagnoli { 0x82F6_3B78 } else { 0xEDB8_8320 };
            let data = rx(c, rm);
            let r = crc32_update(rx(c, rn) as u32, data, nbytes, poly);
            wx(c, rd, r as u64);
        }
        Op::B { imm } => return Flow::Jump(c.pc.wrapping_add(imm as u64)),
        Op::Bl { imm } => {
            c.x[30] = c.pc + 4;
            return Flow::Jump(c.pc.wrapping_add(imm as u64));
        }
        Op::Br { rn } => return Flow::Jump(rx(c, rn)),
        Op::Blr { rn } => {
            let t = rx(c, rn);
            c.x[30] = c.pc + 4;
            return Flow::Jump(t);
        }
        Op::Ret { rn } => return Flow::Jump(rx(c, rn)),
        Op::BCond { cond, imm } => {
            if c.cond_holds(cond) {
                return Flow::Jump(c.pc.wrapping_add(imm as u64));
            }
        }
        Op::Cbz { sf, nz, rt, imm } => {
            let z = rx(c, rt) & mask(sf) == 0;
            if z != nz {
                return Flow::Jump(c.pc.wrapping_add(imm as u64));
            }
        }
        Op::Tbz { nz, rt, bit, imm } => {
            let set = (rx(c, rt) >> bit) & 1 == 1;
            if set == nz {
                return Flow::Jump(c.pc.wrapping_add(imm as u64));
            }
        }
        Op::Load { size, sext, rt, rn, addr } => {
            let base_before = rsp(c, rn);
            let a = eff_addr(c, rn, addr);
            let mut v = unsafe { load(a, size) };
            if sext != 0 {
                v = sext_to(v, size, sext);
            }
            post_wb(c, rn, addr, base_before);
            wx(c, rt, v);
        }
        Op::Store { size, rt, rn, addr } => {
            let base_before = rsp(c, rn);
            let v = rx(c, rt);
            let a = eff_addr(c, rn, addr);
            store(a, size, v);
            post_wb(c, rn, addr, base_before);
        }
        Op::LoadLit { size, sext, rt, imm } => {
            let a = c.pc.wrapping_add(imm as u64) & TBI_MASK;
            let mut v = unsafe { load(a, size) };
            if sext != 0 {
                v = sext_to(v, 2, 64);
            }
            wx(c, rt, v);
        }
        Op::LoadPair { size, sext, rt, rt2, rn, addr } => {
            let base_before = rsp(c, rn);
            let a = eff_addr(c, rn, addr);
            let step = 1u64 << size;
            let mut v1 = unsafe { load(a, size) };
            let mut v2 = unsafe { load(a + step, size) };
            if sext {
                v1 = sext_to(v1, 2, 64);
                v2 = sext_to(v2, 2, 64);
            }
            post_wb(c, rn, addr, base_before);
            wx(c, rt, v1);
            wx(c, rt2, v2);
        }
        Op::StorePair { size, rt, rt2, rn, addr } => {
            let base_before = rsp(c, rn);
            let (v1, v2) = (rx(c, rt), rx(c, rt2));
            let a = eff_addr(c, rn, addr);
            let step = 1u64 << size;
            store(a, size, v1);
            store(a + step, size, v2);
            post_wb(c, rn, addr, base_before);
        }
        Op::Ldx { size, rt, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let v = monitor::ldx(&mut c.mon, a as usize, size);
            wx(c, rt, v);
        }
        Op::Stx { size, rs, rt, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let v = rx(c, rt);
            let ok = monitor::stx(&mut c.mon, a as usize, size, v);
            wx(c, rs, if ok { 0 } else { 1 });
        }
        Op::Ldxp { size, rt, rt2, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 2 << size);
            let (lo, hi) = monitor::ldxp(&mut c.mon, a as usize, size);
            wx(c, rt, lo);
            wx(c, rt2, hi);
        }
        Op::Stxp { size, rs, rt, rt2, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 2 << size);
            let (lo, hi) = (rx(c, rt), rx(c, rt2));
            let ok = monitor::stxp(&mut c.mon, a as usize, size, lo, hi);
            wx(c, rs, if ok { 0 } else { 1 });
        }
        Op::Ldar { size, rt, rn } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let v = unsafe { load(a, size) };
            wx(c, rt, v);
        }
        Op::Stlr { size, rt, rn } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let v = rx(c, rt);
            store(a, size, v);
            std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
        }
        Op::Atomic { size, op, rs, rt, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let r = rx(c, rs);
            let old = monitor::rmw(a as usize, size, |o| Some(atom_apply(op, o, r, size)));
            wx(c, rt, old);
        }
        Op::Cas { size, rs, rt, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 1 << size);
            let bits = 8u32 << size;
            let m = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let cmp = rx(c, rs) & m;
            let new = rx(c, rt) & m;
            let old = monitor::rmw(a as usize, size, |o| if o & m == cmp { Some(new) } else { None });
            wx(c, rs, old & m);
        }
        Op::Casp { size, rs, rt, rn, .. } => {
            let a = rsp(c, rn) & TBI_MASK;
            check_align(a, 2 << size);
            let bits = 8u32 << size;
            let m = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
            let (c0, c1) = (rx(c, rs) & m, rx(c, rs + 1) & m);
            let (n0, n1) = (rx(c, rt) & m, rx(c, rt + 1) & m);
            let (o0, o1) = monitor::rmw_pair(a as usize, size, |a0, a1| {
                if a0 & m == c0 && a1 & m == c1 {
                    Some((n0, n1))
                } else {
                    None
                }
            });
            wx(c, rs, o0 & m);
            wx(c, rs + 1, o1 & m);
        }
        Op::Barrier(b) => match b {
            Barrier::Clrex => monitor::clrex(&mut c.mon),
            Barrier::Dmb | Barrier::Dsb => std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst),
            Barrier::Isb => {}
        },
        Op::Svc { imm } => return Flow::Svc(imm),
        Op::Brk { imm } => return Flow::Brk(imm),
        Op::Mrs { rt, sysreg } => {
            if sysreg == 0xDA21 {
                crate::jit::fold_mxcsr(c);
            }
            let v = read_sysreg(c, sysreg);
            wx(c, rt, v);
        }
        Op::Msr { rt, sysreg } => {
            let v = rx(c, rt);
            write_sysreg(c, sysreg, v);
            if sysreg == 0xDA21 {
                unsafe { core::arch::x86_64::_mm_setcsr(core::arch::x86_64::_mm_getcsr() & !0x3F) };
            }
        }
        Op::MsrImm { op1: 3, crm, .. } => c.set_ssbs(((crm & 1) as u64) << 12), // MSR SSBS, #imm
        Op::MsrImm { op2, .. } => {
            // FlagM / FlagM2 (pseudocodigo del Arm ARM); el decodificador solo deja pasar estas tres
            let f = c.flags();
            let (n, z, cy, v) = ((f >> 31) & 1, (f >> 30) & 1, (f >> 29) & 1, (f >> 28) & 1);
            let (n, z, cy, v) = match op2 {
                0 => (n, z, cy ^ 1, v),                                     // CFINV
                1 => ((cy ^ 1) & (z ^ 1), z & cy, cy | z, (cy ^ 1) & z), // XAFLAG
                _ => (0, z | v, cy & (v ^ 1), 0),                          // AXFLAG
            };
            c.set_flags((n << 31) | (z << 30) | (cy << 29) | (v << 28));
        }
        Op::Rmif { rn, lsb, mask } => {
            let t = rx(c, rn).rotate_right(lsb as u32);
            let m = (mask as u64) << 28;
            c.set_flags((c.flags() & !m) | ((t << 28) & m));
        }
        Op::Setf { rn, w16 } => {
            let t = rx(c, rn);
            let msb = if w16 { 15 } else { 7 };
            let (n, z, v) = ((t >> msb) & 1, (t & ((2u64 << msb) - 1) == 0) as u64, ((t >> (msb + 1)) ^ (t >> msb)) & 1);
            c.set_flags((n << 31) | (z << 30) | (c.flags() & (1 << 29)) | (v << 28));
        }
        Op::Sys { op1, crn, crm, op2, rt } => {
            let a = rx(c, rt) & TBI_MASK;
            match (op1, crn, crm, op2) {
                (3, 7, 4, 1) => {
                    // DC ZVA: poner a cero el bloque de 64 B
                    let base = a & !63;
                    for i in 0..8 {
                        store(base + i * 8, 3, 0);
                    }
                }
                (3, 7, 5, 1) => return Flow::ICacheFlush(a),
                (0, 7, 5, 0) => return Flow::ICacheFlush(0),
                _ => {}
            }
        }
        Op::Fp(f) => return crate::fp::exec(c, f.0),
    }
    Flow::Next
}

/// Decodifica y ejecuta la instruccion en `c.pc`, avanzando el PC. Devuelve el flujo especial
/// (Svc/Brk/Undef/ICacheFlush) o `Next` si sigue la ejecucion normal.
pub fn step(c: &mut Cpu) -> Flow {
    // lectura protegida: una pagina que ya no se puede leer es un fallo de instruccion para el guest
    let Some(w) = crate::monitor::read_code(c.pc & TBI_MASK) else {
        crate::sig::guest_fault(c, 11, 1 /* SEGV_MAPERR */, c.pc, "instruccion en memoria ilegible", c.pc);
        return Flow::Jump(c.pc);
    };
    let op = decode(w);
    match exec(c, &op) {
        Flow::Next => {
            c.pc += 4;
            Flow::Next
        }
        Flow::Jump(t) => {
            c.pc = t;
            Flow::Next
        }
        f @ Flow::ICacheFlush(_) => {
            c.pc += 4;
            f
        }
        f @ Flow::Svc(_) => {
            c.pc += 4;
            f
        }
        f => f,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feat;

    fn run(c: &mut Cpu, w: u32) -> Flow {
        exec(c, &decode(w))
    }

    fn nzcv(f: u64) -> [bool; 4] {
        [f >> 31 & 1 == 1, f >> 30 & 1 == 1, f >> 29 & 1 == 1, f >> 28 & 1 == 1]
    }

    fn pack([n, z, c, v]: [bool; 4]) -> u64 {
        (n as u64) << 31 | (z as u64) << 30 | (c as u64) << 29 | (v as u64) << 28
    }

    /// CFINV, XAFLAG y AXFLAG con las 16 entradas, contra el pseudocodigo del Arm ARM (C7.2).
    #[test]
    fn flagm_como_el_pseudocodigo() {
        for f in 0..16u64 {
            let [n, z, c, v] = nzcv(f << 28);
            let casos = [
                (0xD500_401F, [n, z, !c, v]),                     // CFINV
                (0xD500_403F, [!c && !z, z && c, c || z, !c && z]), // XAFLAG
                (0xD500_405F, [false, z || v, c && !v, false]),    // AXFLAG
            ];
            for (w, esperado) in casos {
                let mut cpu = Cpu::new();
                cpu.set_flags(f << 28);
                assert!(matches!(run(&mut cpu, w), Flow::Next));
                assert_eq!(cpu.flags(), pack(esperado), "{:08x} con nzcv={:04b}", w, f);
            }
        }
        // con flags perezosas (tras un SUBS) tambien
        let mut cpu = Cpu::new();
        cpu.x[1] = 1;
        run(&mut cpu, 0xF100_043F); // cmp x1, #1: Z=1 C=1
        run(&mut cpu, 0xD500_401F); // cfinv
        assert_eq!(cpu.flags(), 0x4000_0000);
    }

    #[test]
    fn rmif_y_setf() {
        let mut cpu = Cpu::new();
        // rmif x0, #4, #0b1010 con x0 = 0xF0: (x0 ror 4)<3:0> = 0b1111 -> N y C
        cpu.x[0] = 0xF0;
        cpu.set_flags(0x5000_0000);
        run(&mut cpu, 0xBA00_0000 | 4 << 15 | 1 << 10 | 0b1010);
        assert_eq!(cpu.flags(), 0xF000_0000);
        // setf8 w0 con 0x80: N=1 Z=0 V=1 (bit 8 = 0 distinto del 7); C se conserva
        cpu.x[0] = 0x80;
        cpu.set_flags(0x2000_0000);
        run(&mut cpu, 0x3A00_080D);
        assert_eq!(cpu.flags(), 0xB000_0000);
        // setf16 w0 con 0x1_0000: los 16 bits bajos son 0 -> Z=1; bit 16 != bit 15 -> V=1
        cpu.x[0] = 0x1_0000;
        cpu.set_flags(0);
        run(&mut cpu, 0x3A00_480D);
        assert_eq!(cpu.flags(), 0x5000_0000);
    }

    /// Lo que EL0 no puede usar o heddle no implementa es SIGILL, no un NOP.
    #[test]
    fn sistema_no_accesible_es_indefinido() {
        let indefinidas = [
            0xD503_477F, // SMSTART
            0xD503_467F, // SMSTOP
            0xD503_437F, // SMSTART SM (MSR SVCR)
            0xD503_415F, // MSR DIT, #1
            0xD503_409F, // MSR TCO, #0
            0xD503_46DF, // MSR DAIFSet, #6
            0xD503_46FF, // MSR DAIFClr, #6
            0xD500_419F, // MSR PAN, #1
            0xD500_407F, // MSR UAO, #0
            0xD500_40BF, // MSR SPSel, #0
            0xD503_401F, // CFINV con op1=3: no asignado
            0xD50B_401F, // MSR (inmediato) op1=3 CRm=0 op2=0: no asignado
            0xD53B_2400, // MRS RNDR
            0xD53B_4220, // MRS DAIF
            0xD53B_E020, // MRS CNTPCT_EL0
            0xD538_0020, // MRS de CRm=0 op2=1 (sin emular en Linux)
            0xD538_0100, // MRS ID_PFR0_EL1 (AArch32)
            0xD539_0000, // MRS CCSIDR_EL1
            0xD539_00E0, // MRS AIDR_EL1
            0xD51B_D063, // MSR TPIDRRO_EL0
            0xD51B_0023, // MSR CTR_EL0
            0xD518_0003, // MSR MIDR_EL1
            0xD508_751F, // IC IALLU
            0xD508_7620, // DC IVAC
            0xD508_831F, // TLBI VMALLE1IS
        ];
        for w in indefinidas {
            assert!(matches!(decode(w), Op::Undef(_)), "{:08x} -> {:?}", w, decode(w));
        }
        // NOP, SB, DC ZVA, IC IVAU, DC CVAC/CVAU/CIVAC, MSR FPCR, MRS TPIDRRO_EL0, CFINV/XAFLAG/AXFLAG, DC CVAP/CVADP,
        // MSR SSBS #1, MRS/MSR SSBS (modelo `max` en las pruebas)
        let validas = [
            0xD503_201F, 0xD503_30FF, 0xD50B_7420, 0xD50B_7520, 0xD50B_7A20, 0xD50B_7B20, 0xD50B_7E20, 0xD51B_4403, 0xD53B_D063,
            0xD500_401F, 0xD500_403F, 0xD500_405F, 0xD50B_7C20, 0xD50B_7D20, 0xD503_413F, 0xD53B_42C0, 0xD51B_42C0,
        ];
        for w in validas {
            assert!(!matches!(decode(w), Op::Undef(_)), "{:08x}", w);
        }
    }

    /// FEAT_SSBS/SSBS2 (modelo `max`): PSTATE.SSBS empieza a 1 como en Linux, MSR SSBS #imm escribe CRm<0>, el registro
    /// SSBS lo da en el bit 12 y MSR SSBS, Xt solo toma ese bit.
    #[test]
    fn ssbs_como_el_pseudocodigo() {
        let mut c = Cpu::new();
        let mrs = |c: &mut Cpu| {
            exec(c, &decode(0xD53B_42C0)); // mrs x0, ssbs
            c.x[0]
        };
        assert_eq!(mrs(&mut c), 1 << 12);
        exec(&mut c, &decode(0xD503_403F)); // msr ssbs, #0
        assert_eq!(mrs(&mut c), 0);
        exec(&mut c, &decode(0xD503_4F3F)); // msr ssbs, #15: CRm<0> = 1
        assert_eq!(mrs(&mut c), 1 << 12);
        c.x[0] = !(1u64 << 12);
        exec(&mut c, &decode(0xD51B_42C0)); // msr ssbs, x0
        assert_eq!((mrs(&mut c), c.ssbs()), (0, 0));
        c.reset();
        assert_eq!(c.ssbs(), 1 << 12);
    }

    /// MSR FPCR guarda AHP, DN, FZ, RMode y FZ16 (FEAT_FP16): antes se perdia FZ16. Las habilitaciones de trampas y
    /// Len/Stride son RAZ/WI.
    #[test]
    fn msr_fpcr_guarda_fz16_y_ahp() {
        let mut cpu = Cpu::new();
        cpu.x[0] = u64::MAX;
        run(&mut cpu, 0xD51B_4400); // msr fpcr, x0
        run(&mut cpu, 0xD53B_4401); // mrs x1, fpcr
        assert_eq!(cpu.x[1], FPCR_RW);
        assert_ne!(cpu.x[1] & (1 << 19), 0);
        assert_ne!(cpu.x[1] & (1 << 26), 0);
        assert_eq!(cpu.x[1], 0x07C8_0000);
        cpu.x[0] = u64::MAX;
        run(&mut cpu, 0xD51B_4420); // msr fpsr, x0
        run(&mut cpu, 0xD53B_4421); // mrs x1, fpsr
        assert_eq!(cpu.x[1], 0xF800_009F);
    }

    /// Codificaciones reservadas que heddle ejecutaba y Unicorn (y el Arm ARM) rechazan: SIGILL al guest.
    #[test]
    fn codificaciones_reservadas_son_indefinidas() {
        let casos = [
            0x6842_B8EB, // LDNP con opc=01: no asignado (solo LDPSW usa opc=01, y no en la forma no temporal)
            0xF8A5_1844, // PRFM (registro) con option<1>=0: no asignado
            0xF889_763F, // PRFM (registro) con option<1>=0 (otra forma)
            0x0D40_BB7B, // LD1 {Vt.s}[i] con size<1>=1: UNDEFINED (pseudocodigo de LD1, estructura unica)
            0x0D8A_89B5, // ST1 {Vt.s}[i] con size<1>=1 (post-indice)
            0x0DBF_8C93, // LD/ST estructura unica con size<1>=1
            0x4DAA_9BE8, // LD/ST estructura unica con size<1>=1
            0x4E35_D0A8, // SQDMULL2 con size=00: UNDEFINED ("if size == '00' || size == '11'")
            0x0E36_D362, // SQDMULL con size=00
        ];
        for w in casos {
            let mut cpu = Cpu::new();
            assert!(matches!(run(&mut cpu, w), Flow::Undef(_)), "{:08x}", w);
        }
        // las vecinas validas siguen decodificando: LDP, PRFM con LSL, SQDMULL .4s
        for w in [0xA842_B8EBu32, 0xF8A5_7844, 0x0E76_D362] {
            assert!(!matches!(decode(w), Op::Undef(_)), "{:08x}", w);
        }
    }

    /// MRS del espacio de identificacion: los valores de `feat`, coherentes con AT_HWCAP.
    #[test]
    fn mrs_de_identificacion_coherente_con_hwcap() {
        let mut cpu = Cpu::new();
        let mrs = |cpu: &mut Cpu, crm: u32, op2: u32| {
            run(cpu, 0xD538_0000 | crm << 8 | op2 << 5 | 3);
            cpu.x[3]
        };
        assert_eq!(mrs(&mut cpu, 0, 0), feat::midr());
        assert_eq!(mrs(&mut cpu, 0, 5), 1 << 31);
        let pfr0 = mrs(&mut cpu, 4, 0);
        let isar0 = mrs(&mut cpu, 6, 0);
        let isar1 = mrs(&mut cpu, 6, 1);
        let r = feat::init().regs;
        assert_eq!((pfr0, isar0, isar1), (r.pfr0, r.isar0, r.isar1));
        let h = feat::hwcap();
        let campo = |r: u64, s: u32| (r >> s) & 0xF;
        assert_eq!(campo(pfr0, 16) != 0xF, h & 1 != 0); // FP
        assert_eq!(campo(pfr0, 20) != 0xF, h & 2 != 0); // ASIMD
        assert_eq!(campo(pfr0, 16) == 1, h & 1 << 9 != 0); // FPHP
        assert_eq!(campo(isar0, 4) >= 1, h & 1 << 3 != 0); // AES
        assert_eq!(campo(isar0, 4) >= 2, h & 1 << 4 != 0); // PMULL
        assert_eq!(campo(isar0, 20) >= 2, h & 1 << 8 != 0); // ATOMICS
        assert_eq!(campo(isar0, 52) >= 1, h & 1 << 27 != 0); // FLAGM
        assert_eq!(campo(isar0, 52) >= 2, feat::hwcap2() & 1 << 7 != 0); // FLAGM2
        assert_eq!(campo(isar1, 36) >= 1, h & 1 << 29 != 0); // SB
        assert_eq!(campo(isar1, 32) >= 1, feat::hwcap2() & 1 << 8 != 0); // FRINT
        assert_eq!(campo(pfr0, 32), 0); // sin SVE
        assert_eq!(mrs(&mut cpu, 4, 4), 0); // ZFR0
        assert_eq!(mrs(&mut cpu, 7, 7), 0); // reservado: RAZ
        // lo que anuncia, se decodifica
        assert!(!matches!(decode(0xD500_401F), Op::Undef(_)));
        assert!(!matches!(decode(0xD503_30FF), Op::Undef(_))); // SB
        assert!(!matches!(decode(0x1E28_4000), Op::Undef(_))); // FRINT32Z s0, s0
        // TPIDRRO_EL0 se lee como 0
        run(&mut cpu, 0xD53B_D063);
        assert_eq!(cpu.x[3], 0);
        // CTR_EL0, DCZID_EL0 y CNTFRQ_EL0 del modelo
        run(&mut cpu, 0xD53B_0023);
        assert_eq!(cpu.x[3], feat::ctr());
        run(&mut cpu, 0xD53B_00E3);
        assert_eq!(cpu.x[3], 4);
        run(&mut cpu, 0xD53B_E003);
        assert_eq!(cpu.x[3], 19_200_000);
        // CNTVCT_EL0 avanza a CNTFRQ: ~19 200 ticks en 1 ms
        run(&mut cpu, 0xD53B_E043);
        let t0 = cpu.x[3];
        std::thread::sleep(std::time::Duration::from_millis(2));
        run(&mut cpu, 0xD53B_E043);
        let d = cpu.x[3] - t0;
        assert!((38_400..38_400 * 50).contains(&d), "{}", d);
    }
}
