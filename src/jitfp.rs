//! Rutas rapidas inline del JIT para FP escalar y cargas/almacenamientos SIMD/FP.
//!
//! Equivalencia estricta: cada operacion se ejecuta con SSE con MXCSR por defecto (RN, todo enmascarado) y
//! solo se acepta el resultado si FPCR == 0 y la operacion no produjo invalido (IE), underflow (UE), ni
//! NaN, ni redondeo a la menor normal con inexacto (caso "tininess before rounding" de ARM). En cualquier
//! otro caso se descarta y se ejecuta la instruccion con el interprete de referencia (softfp).

use super::*;
use crate::fp::FpOp;
use crate::neonls::{self, Ls};

const O_V: i32 = offset_of!(Cpu, v) as i32;
const O_FPCR: i32 = offset_of!(Cpu, fpcr) as i32;
const O_FPSR: i32 = offset_of!(Cpu, fpsr) as i32;
const O_NZCV: i32 = offset_of!(Cpu, nzcv) as i32;
const O_MX_W: i32 = O_AUX + 40;
const O_MX_R: i32 = O_AUX + 48;

pub const KNOWN_RAW: u64 = 100;

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

pub fn fma_available() -> bool {
    std::is_x86_feature_detected!("fma")
}
pub fn sse41_available() -> bool {
    std::is_x86_feature_detected!("sse4.1")
}

impl Asm {
    pub(super) fn sse_rm(&mut self, pfx: u8, w: bool, opc: &[u8], reg: u8, base: u8, disp: i32) {
        if pfx != 0 {
            self.u8(pfx);
        }
        self.rex(w, reg, base, false);
        for &o in opc {
            self.u8(o);
        }
        self.modrm_mem(reg, base, disp);
    }
    pub(super) fn sse_rr(&mut self, pfx: u8, w: bool, opc: &[u8], reg: u8, rm: u8) {
        if pfx != 0 {
            self.u8(pfx);
        }
        self.rex(w, reg, rm, false);
        for &o in opc {
            self.u8(o);
        }
        self.modrm_rr(reg, rm);
    }
    /// VEX.LIG.66.0F38.Wx op xmm_dst, xmm_src1, xmm_src2   (FMA3; solo xmm0..xmm7)
    fn vex_fma(&mut self, opc: u8, w: bool, dst: u8, src1: u8, src2: u8) {
        self.u8(0xC4);
        self.u8(0xE2); // R=X=B=1 (invertidos), mmmmm = 0F38
        self.u8(((w as u8) << 7) | ((!src1 & 0xF) << 3) | 0x01); // vvvv, L=0, pp=66
        self.u8(opc);
        self.modrm_rr(dst, src2);
    }
}

struct Fx<'c, 'a> {
    cx: &'c mut Ctx<'a>,
    fix: Vec<usize>,
    restore_csr: bool,
}

impl<'c, 'a> Fx<'c, 'a> {
    /// Prologo: FPCR debe ser 0. MXCSR esta limpio y por defecto (lo fija Jit::run); las flags
    /// pegajosas se acumulan sin coste y se pasan a FPSR con `fold_mxcsr`.
    fn begin(&mut self) {
        let a = &mut self.cx.a;
        a.op_rm(true, &[0x83], 7, RBX, O_FPCR); // cmp qword [fpcr], 0
        a.u8(0);
        let j = a.jcc_fwd(CC_NE);
        self.fix.push(j);
    }
    fn jfix(&mut self, cc: u8) {
        let j = self.cx.a.jcc_fwd(cc);
        self.fix.push(j);
    }
    /// Comprobaciones de resultado en xmm0: NaN (cubre IE) y, si `check_min`, resultado "pequeno"
    /// (|r| < 2*minnormal, incluido cero) que podria necesitar UFC (tininess antes del redondeo).
    fn end(&mut self, d: bool, check_nan: bool, check_min: bool) {
        let a = &mut self.cx.a;
        if check_nan {
            if d {
                a.sse_rr(0x66, false, &[0x0F, 0x2E], 0, 0);
            } else {
                a.sse_rr(0, false, &[0x0F, 0x2E], 0, 0);
            }
            self.jfix(CC_P);
        }
        let a = &mut self.cx.a;
        if check_min {
            if d {
                a.sse_rr(0x66, true, &[0x0F, 0x7E], 0, RAX); // movq rax, xmm0
                a.shift_ri(true, 4, RAX, 1);
                a.mov_ri(R8, 0x0040_0000_0000_0000);
                a.alu_rr(true, 0x39, RAX, R8);
            } else {
                a.sse_rr(0x66, false, &[0x0F, 0x7E], 0, RAX); // movd eax, xmm0
                a.shift_ri(false, 4, RAX, 1);
                a.alu_ri(false, 7, RAX, 0x0200_0000);
            }
            self.jfix(CC_B);
        }
    }
    /// FMUL escalar (resultado en xmm0, operandos en v[rn], v[rm]): como `end(d, true, true)` pero acepta el cero
    /// EXACTO. Razon: si un operando es +-0 (y el otro no es NaN ni inf: eso da NaN y ya cayo por la guarda de NaN),
    /// el producto es +-0 sin error, ARM (FPMul) y x86 (mulss/mulsd) dan el signo xor de los operandos y ninguno
    /// marca flags (DE de x86 por un subnormal no se pasa a FPSR). Un cero con ambos operandos distintos de cero es
    /// un underflow inexacto (ARM: UFC+IXC con tininess antes del redondeo): sigue cayendo al helper, igual que
    /// todo resultado distinto de cero con |r| < 2*minnormal.
    fn end_fmul(&mut self, d: bool, rn: u32, rm: u32) {
        self.end(d, true, false);
        let a = &mut self.cx.a;
        if d {
            a.sse_rr(0x66, true, &[0x0F, 0x7E], 0, RAX); // movq rax, xmm0
            a.shift_ri(true, 4, RAX, 1); // |r| << 1
            a.mov_ri(R8, 0x0040_0000_0000_0000);
            a.alu_rr(true, 0x39, RAX, R8);
        } else {
            a.sse_rr(0x66, false, &[0x0F, 0x7E], 0, RAX); // movd eax, xmm0
            a.shift_ri(false, 4, RAX, 1);
            a.alu_ri(false, 7, RAX, 0x0200_0000);
        }
        let ok1 = a.jcc_fwd(CC_AE); // no pequeno: se acepta
        a.alu_rr(d, 0x85, RAX, RAX);
        self.jfix(CC_NE); // pequeno distinto de cero: helper
        let a = &mut self.cx.a;
        a.mov_rm(RAX, RBX, vo(rn), d);
        a.shift_ri(d, 4, RAX, 1);
        let ok2 = a.jcc_fwd(CC_E); // n = +-0: cero exacto
        a.mov_rm(RAX, RBX, vo(rm), d);
        a.shift_ri(d, 4, RAX, 1);
        self.jfix(CC_NE); // m != +-0 (y n != +-0): cero por underflow -> helper
        let a = &mut self.cx.a;
        a.patch_here(ok1);
        a.patch_here(ok2);
    }
    /// Cierra: salta la ruta lenta y la emite (helper del interprete).
    fn finish(mut self, w: u32, pc: u64, idx: usize) {
        let known = self.cx.known;
        let done = self.cx.a.jmp_fwd();
        for f in std::mem::take(&mut self.fix) {
            self.cx.a.patch_here(f);
        }
        if self.restore_csr {
            // descarta las flags que la ruta rapida (descartada) haya podido dejar
            self.cx.a.op_rm(false, &[0x0F, 0xAE], 2, RBX, O_MX_R); // ldmxcsr [saved]
        }
        self.cx.call_helper_exec_o(pc, Op::Fp(FpOp(w)), idx, crate::profg::O_RAPIDA);
        self.cx.known = known;
        self.cx.a.patch_here(done);
    }
    fn ld(&mut self, xmm: u8, r: u32, d: bool) {
        if d {
            self.cx.a.sse_rm(0xF2, false, &[0x0F, 0x10], xmm, RBX, vo(r)); // movsd
        } else {
            self.cx.a.sse_rm(0xF3, false, &[0x0F, 0x10], xmm, RBX, vo(r)); // movss
        }
    }
    /// Escribe xmm en v[rd] (escalar) y pone a cero el resto del registro.
    fn st(&mut self, xmm: u8, rd: u32, d: bool) {
        let a = &mut self.cx.a;
        if d {
            a.sse_rm(0x66, false, &[0x0F, 0xD6], xmm, RBX, vo(rd)); // movq [mem], xmm
        } else {
            a.sse_rr(0x66, false, &[0x0F, 0x7E], xmm, RAX); // movd eax, xmm
            a.mov_mr(RBX, vo(rd), RAX, true);
        }
        a.mov_mi64(RBX, vo(rd) + 8, 0);
    }
}

const CC_P: u8 = 0xA;

pub(super) fn try_emit(cx: &mut Ctx, w: u32, pc: u64, idx: usize) -> bool {
    if super::jitneon::try_emit(cx, w) {
        return true;
    }
    if crate::neon::is_simd_ldst(w) {
        return ldst(cx, w, pc, idx);
    }
    if !bit(w, 31) && bits(w, 28, 24) == 0b01110 && bit(w, 21) && bit(w, 10) {
        return simd3(cx, w, pc, idx);
    }
    // FMAXNMV/FMAXV/FMINNMV/FMINV (across lanes, .4s): 0 Q 1 01110 a sz 11000 opcode 10 Rn Rd
    if !bit(w, 31) && bit(w, 29) && bits(w, 28, 24) == 0b01110 && bits(w, 21, 17) == 0b11000 && bits(w, 11, 10) == 0b10 {
        return minmax_reduce(cx, w, pc, idx, false);
    }
    // FMAXNMP/FMAXP/FMINNMP/FMINP escalar (par de carriles): 01 1 11110 a sz 11000 opcode 10 Rn Rd
    if bits(w, 31, 29) == 0b011 && bits(w, 28, 24) == 0b11110 && bits(w, 21, 17) == 0b11000 && bits(w, 11, 10) == 0b10 {
        return minmax_reduce(cx, w, pc, idx, true);
    }
    // SIMD "by element" (vector: 0 Q U 01111; escalar: 01 U 11111), bit 10 = 0. Con size = 1x (S/D) el bit 23
    // vale 1, lo que lo separa de "shift by immediate"/"modified immediate" (bit 23 = 0 alli).
    if !bit(w, 31) && bits(w, 28, 24) == 0b01111 && !bit(w, 10) && bit(w, 23) {
        return by_elem(cx, w, pc, idx, false);
    }
    if bits(w, 31, 30) == 0b01 && bits(w, 28, 24) == 0b11111 && !bit(w, 10) && bit(w, 23) {
        return by_elem(cx, w, pc, idx, true);
    }
    if bits(w, 31, 29) == 0 && bits(w, 28, 24) == 0b11111 {
        return fp_dp3(cx, w, pc, idx);
    }
    if bits(w, 28, 24) == 0b11110 && !bit(w, 29) && !bit(w, 30) {
        return scalar(cx, w, pc, idx);
    }
    false
}

fn scalar(cx: &mut Ctx, w: u32, pc: u64, idx: usize) -> bool {
    let sf = bit(w, 31);
    let t = bits(w, 23, 22);
    let (rn, rd, rm) = (bits(w, 9, 5), bits(w, 4, 0), bits(w, 20, 16));
    if t > 1 {
        return false;
    }
    let d = t == 1;
    if !bit(w, 21) {
        return false; // coma fija
    }
    if bits(w, 15, 10) == 0 {
        return int_conv(cx, w, pc, idx, sf, d, rn, rd);
    }
    if sf {
        return false;
    }
    if bits(w, 14, 10) == 0b10000 {
        return dp1(cx, w, pc, idx, d, rn, rd);
    }
    if bits(w, 13, 10) == 0b1000 {
        return fcmp(cx, w, pc, idx, d, rn, rm);
    }
    if bits(w, 12, 10) == 0b100 {
        // FMOV imm
        let v = crate::fp::vfp_expand_imm(if d { crate::softfp::Ft::D } else { crate::softfp::Ft::S }, bits(w, 20, 13));
        cx.a.mov_ri(RAX, v);
        cx.a.mov_mr(RBX, vo(rd), RAX, true);
        cx.a.mov_mi64(RBX, vo(rd) + 8, 0);
        return true;
    }
    match bits(w, 11, 10) {
        0b11 => {
            // FCSEL
            let cond = bits(w, 15, 12) as u8;
            match cx.emit_cond(cond) {
                None => {
                    // AL / NV: siempre n
                    ld_gp(cx, RAX, rn, d);
                }
                Some(cc) => {
                    ld_gp(cx, RAX, rm, d);
                    ld_gp(cx, RDX, rn, d);
                    cx.a.cmov(true, cc, RAX, RDX);
                }
            }
            cx.a.mov_mr(RBX, vo(rd), RAX, true);
            cx.a.mov_mi64(RBX, vo(rd) + 8, 0);
            true
        }
        0b10 => arith2(cx, w, pc, idx, d, rn, rm, rd),
        0b01 => fccmp(cx, w, pc, idx, d, rn, rm),
        _ => false,
    }
}

/// FCCMP/FCCMPE: si la condicion se cumple, NZCV = comparacion de Vn y Vm; si no, NZCV = el inmediato. Misma
/// infraestructura que FCMP: con FPCR != 0 o algun NaN (IOC) la instruccion va al helper; el resto es exacto y no
/// toca FPSR (la comparacion de numeros no ordenados es la unica que senaliza).
fn fccmp(cx: &mut Ctx, w: u32, pc: u64, idx: usize, d: bool, rn: u32, rm: u32) -> bool {
    let cond = bits(w, 15, 12) as u8;
    let nzcv_imm = (bits(w, 3, 0) as u64) << 28;
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.cx.a.op_rm(true, &[0x83], 7, RBX, O_FPCR); // cmp qword [fpcr], 0
    f.cx.a.u8(0);
    f.jfix(CC_NE);
    // condicion: deja las flags x86 y devuelve el cc que significa "se cumple"
    let cc = f.cx.emit_cond(cond);
    let mut false_path = None;
    if let Some(cc) = cc {
        false_path = Some(f.cx.a.jcc_fwd(cc ^ 1));
    }
    f.ld(0, rn, d);
    f.ld(1, rm, d);
    if d {
        f.cx.a.sse_rr(0x66, false, &[0x0F, 0x2E], 0, 1);
    } else {
        f.cx.a.sse_rr(0, false, &[0x0F, 0x2E], 0, 1);
    }
    f.jfix(CC_P); // NaN -> ruta lenta
    let a = &mut f.cx.a;
    a.mov_ri(RAX, 0x2000_0000); // mayor: C
    a.mov_ri(RCX, 0x8000_0000); // menor: N
    a.cmov(false, CC_B, RAX, RCX);
    a.mov_ri(RCX, 0x6000_0000); // igual: Z|C
    a.cmov(false, CC_E, RAX, RCX);
    a.mov_mr(RBX, O_NZCV, RAX, true);
    let end1 = if false_path.is_some() { Some(a.jmp_fwd()) } else { None };
    if let Some(fp) = false_path {
        a.patch_here(fp);
        a.mov_ri(RAX, nzcv_imm);
        a.mov_mr(RBX, O_NZCV, RAX, true);
    }
    if let Some(e) = end1 {
        a.patch_here(e);
    }
    a.mov_mi64(RBX, O_KIND, 0);
    f.cx.known = 0;
    let done = f.cx.a.jmp_fwd();
    for x in std::mem::take(&mut f.fix) {
        f.cx.a.patch_here(x);
    }
    f.cx.call_helper_exec_o(pc, Op::Fp(FpOp(w)), idx, crate::profg::O_FCMP);
    f.cx.a.patch_here(done);
    f.cx.known = KNOWN_RAW;
    true
}

/// Carga el escalar (32 o 64 bits, extendido con ceros) en un registro entero.
fn ld_gp(cx: &mut Ctx, r: u8, v: u32, d: bool) {
    cx.a.mov_rm(r, RBX, vo(v), d);
}

fn arith2(cx: &mut Ctx, w: u32, pc: u64, idx: usize, d: bool, rn: u32, rm: u32, rd: u32) -> bool {
    let op = bits(w, 15, 12);
    let (opc, nan_min) = match op {
        0 => (0x59u8, true),  // FMUL
        1 => (0x5E, true),    // FDIV
        2 => (0x58, true),    // FADD
        3 => (0x5C, true),    // FSUB
        4 | 6 => (0x5F, false), // FMAX / FMAXNM
        5 | 7 => (0x5D, false), // FMIN / FMINNM
        _ => return false,    // FNMUL: ruta lenta
    };
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.begin();
    f.ld(0, rn, d);
    let pfx = if d { 0xF2 } else { 0xF3 };
    if !nan_min {
        // FMAX/FMIN: NaN o operandos iguales (ceros con signo) -> ruta lenta
        f.ld(1, rm, d);
        if d {
            f.cx.a.sse_rr(0x66, false, &[0x0F, 0x2E], 0, 1);
        } else {
            f.cx.a.sse_rr(0, false, &[0x0F, 0x2E], 0, 1);
        }
        f.jfix(CC_P);
        // Operandos iguales (ucomis ZF=1 sin NaN). Antes iba al helper: eran las "otro" que caian en Android (por
        // ejemplo fmin(1.0, 1.0) o fmax(x, x) al acotar valores). Con valores numericamente iguales el resultado es
        // exacto y sin flags; solo importa el signo del cero: FMAX(+0,-0) = +0 (AND de los bits de signo) y
        // FMIN(+0,-0) = -0 (OR). Con bits identicos AND/OR devuelven el mismo valor. (maxss/minss no sirven: con dos
        // ceros devuelven siempre el segundo operando.)
        let ne = f.cx.a.jcc_fwd(CC_NE);
        {
            let a = &mut f.cx.a;
            a.mov_rm(RAX, RBX, vo(rn), d);
            a.mov_rm(RCX, RBX, vo(rm), d);
            a.alu_rr(d, if matches!(op, 4 | 6) { 0x21 } else { 0x09 }, RAX, RCX); // FMAX: and ; FMIN: or
            a.mov_mr(RBX, vo(rd), RAX, true);
            a.mov_mi64(RBX, vo(rd) + 8, 0);
        }
        let eq_done = f.cx.a.jmp_fwd();
        f.cx.a.patch_here(ne);
        f.cx.a.sse_rr(pfx, false, &[0x0F, opc], 0, 1);
        f.end(d, false, false);
        f.st(0, rd, d);
        f.cx.a.patch_here(eq_done);
        f.finish(w, pc, idx);
        return true;
    } else {
        f.cx.a.sse_rm(pfx, false, &[0x0F, opc], 0, RBX, vo(rm));
        // FADD/FSUB no necesitan la guarda de resultado pequeno: si la suma exacta de dos numeros es subnormal
        // (o cero), es representable sin error, asi que no hay inexacto y ARM no marca UFC (con UFE=0 exige
        // tiny E inexacto) ni IXC; el cero exacto lleva el mismo signo en ambos con RN (+0 salvo (-0)+(-0), y
        // x-x = +0). Lo comprueba la prueba diferencial `dif`.
        if op == 0 {
            f.end_fmul(d, rn, rm);
        } else {
            f.end(d, true, op <= 1);
        }
    }
    f.st(0, rd, d);
    f.finish(w, pc, idx);
    true
}

fn dp1(cx: &mut Ctx, w: u32, pc: u64, idx: usize, d: bool, rn: u32, rd: u32) -> bool {
    let op = bits(w, 20, 15);
    match op {
        0 => {
            // FMOV
            ld_gp(cx, RAX, rn, d);
            cx.a.mov_mr(RBX, vo(rd), RAX, true);
            cx.a.mov_mi64(RBX, vo(rd) + 8, 0);
            true
        }
        1 | 2 => {
            // FABS / FNEG: operacion de bit
            ld_gp(cx, RAX, rn, d);
            let b = if d { 63 } else { 31 };
            cx.a.op_rr(d, &[0x0F, 0xBA], if op == 1 { 6 } else { 7 }, RAX, false); // btr / btc
            cx.a.u8(b);
            cx.a.mov_mr(RBX, vo(rd), RAX, true);
            cx.a.mov_mi64(RBX, vo(rd) + 8, 0);
            true
        }
        3 => {
            let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
            f.begin();
            let pfx = if d { 0xF2 } else { 0xF3 };
            f.cx.a.sse_rm(pfx, false, &[0x0F, 0x51], 0, RBX, vo(rn)); // sqrts?
            f.end(d, true, false);
            f.st(0, rd, d);
            f.finish(w, pc, idx);
            true
        }
        4 | 5 => {
            // FCVT: 4 = a S, 5 = a D (el origen es el tipo de la instruccion)
            let to_d = op == 5;
            if to_d == d {
                return false;
            }
            let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
            f.begin();
            f.ld(0, rn, d);
            if d {
                f.cx.a.sse_rr(0xF2, false, &[0x0F, 0x5A], 0, 0); // cvtsd2ss
            } else {
                f.cx.a.sse_rr(0xF3, false, &[0x0F, 0x5A], 0, 0); // cvtss2sd
            }
            f.end(to_d, true, !to_d);
            f.st(0, rd, to_d);
            f.finish(w, pc, idx);
            true
        }
        8 | 9 | 10 | 11 | 14 | 15 => {
            if !sse41_available() {
                return false;
            }
            // FRINTN=8 P=9 M=10 Z=11 X=14 I=15 -> roundsd imm: 0 nearest, 2 ceil(P)->2, 1 floor(M)->1, 3 trunc
            let imm: u8 = match op {
                8 => 0 | 8,
                9 => 2 | 8,
                10 => 1 | 8,
                11 => 3 | 8,
                14 => 4,       // usa MXCSR (RN), senaliza inexacto
                _ => 4 | 8,    // FRINTI: modo actual, sin inexacto
            };
            let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
            f.begin();
            f.ld(0, rn, d);
            // roundsd/roundss xmm0, xmm0, imm
            f.cx.a.sse_rr(0x66, false, &[0x0F, 0x3A, if d { 0x0B } else { 0x0A }], 0, 0);
            f.cx.a.u8(imm);
            f.end(d, true, false);
            f.st(0, rd, d);
            f.finish(w, pc, idx);
            true
        }
        _ => false,
    }
}

fn fcmp(cx: &mut Ctx, w: u32, pc: u64, idx: usize, d: bool, rn: u32, rm: u32) -> bool {
    if bits(w, 15, 14) != 0 || bits(w, 2, 0) != 0 {
        return false;
    }
    let zero = bit(w, 3);
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.cx.a.op_rm(true, &[0x83], 7, RBX, O_FPCR); // cmp qword [fpcr], 0
    f.cx.a.u8(0);
    f.jfix(CC_NE);
    f.ld(0, rn, d);
    if zero {
        f.cx.a.sse_rr(0, false, &[0x0F, 0x57], 1, 1); // xorps xmm1, xmm1
    } else {
        f.ld(1, rm, d);
    }
    if d {
        f.cx.a.sse_rr(0x66, false, &[0x0F, 0x2E], 0, 1);
    } else {
        f.cx.a.sse_rr(0, false, &[0x0F, 0x2E], 0, 1);
    }
    f.jfix(CC_P); // NaN -> ruta lenta (IOC / FCMPE)
    let a = &mut f.cx.a;
    a.mov_ri(RAX, 0x2000_0000); // mayor: C
    a.mov_ri(RCX, 0x8000_0000); // menor: N
    a.cmov(false, CC_B, RAX, RCX);
    a.mov_ri(RCX, 0x6000_0000); // igual: Z|C
    a.cmov(false, CC_E, RAX, RCX);
    a.mov_mr(RBX, O_NZCV, RAX, true);
    a.mov_mi64(RBX, O_KIND, 0);
    let known = f.cx.known;
    let _ = known;
    f.cx.known = 0;
    // ruta lenta
    let done = f.cx.a.jmp_fwd();
    for x in std::mem::take(&mut f.fix) {
        f.cx.a.patch_here(x);
    }
    f.cx.call_helper_exec_o(pc, Op::Fp(FpOp(w)), idx, crate::profg::O_FCMP);
    f.cx.a.patch_here(done);
    // en ambos caminos la memoria tiene kind = RAW
    f.cx.known = KNOWN_RAW;
    true
}

fn int_conv(cx: &mut Ctx, w: u32, pc: u64, idx: usize, sf: bool, d: bool, rn: u32, rd: u32) -> bool {
    if bit(w, 29) || bit(w, 30) {
        return false;
    }
    let rmode = bits(w, 20, 19);
    let opc = bits(w, 18, 16);
    match (rmode, opc) {
        (0, 6) => {
            // FMOV Wd/Xd, Sn/Dn
            if sf != d {
                return false;
            }
            ld_gp(cx, RAX, rn, d);
            cx.st_x(rd as u8, RAX, true);
            true
        }
        (0, 7) => {
            // FMOV Sd/Dd, Wn/Xn
            if sf != d {
                return false;
            }
            cx.ld_x(RAX, rn as u8, true);
            if !d {
                cx.a.mov_rr(RAX, RAX, false);
            }
            cx.a.mov_mr(RBX, vo(rd), RAX, true);
            cx.a.mov_mi64(RBX, vo(rd) + 8, 0);
            true
        }
        (0, 2) | (0, 3) => {
            // SCVTF / UCVTF
            let signed = opc == 2;
            let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
            f.begin();
            f.cx.ld_x(RAX, rn as u8, true);
            let pfx = if d { 0xF2 } else { 0xF3 };
            if !signed && sf {
                // X sin signo: solo si el bit alto es 0
                f.cx.a.alu_rr(true, 0x85, RAX, RAX);
                f.jfix(CC_S);
            }
            if !signed && !sf {
                f.cx.a.mov_rr(RAX, RAX, false); // extiende con ceros
                f.cx.a.sse_rr(pfx, true, &[0x0F, 0x2A], 0, RAX);
            } else {
                f.cx.a.sse_rr(pfx, sf, &[0x0F, 0x2A], 0, RAX);
            }
            f.end(d, false, false);
            f.st(0, rd, d);
            f.finish(w, pc, idx);
            true
        }
        (3, 0) | (3, 1) => {
            // FCVTZS / FCVTZU
            let signed = opc == 0;
            let mut f = Fx { cx, fix: Vec::new(), restore_csr: !signed };
            if !signed {
                // guarda MXCSR antes de la conversion: si se descarta, el cvtt pudo dejar PE espurio
                f.cx.a.op_rm(false, &[0x0F, 0xAE], 3, RBX, O_MX_R); // stmxcsr [saved]
            }
            f.begin();
            let pfx = if d { 0xF2 } else { 0xF3 };
            f.ld(0, rn, d);
            // cvtts?2si r64 siempre (para unsigned W evita el problema de rango); signed usa el ancho de destino
            let w64 = sf || !signed;
            f.cx.a.sse_rr(pfx, w64, &[0x0F, 0x2C], R9, 0);
            if signed {
                if sf {
                    f.cx.a.mov_ri(RCX, 0x8000_0000_0000_0000);
                    f.cx.a.alu_rr(true, 0x39, R9, RCX);
                } else {
                    f.cx.a.alu_ri(false, 7, R9, i32::MIN);
                }
                f.jfix(CC_E);
            } else if sf {
                f.cx.a.alu_rr(true, 0x85, R9, R9);
                f.jfix(CC_S);
            } else {
                f.cx.a.mov_ri(RCX, 0xFFFF_FFFF);
                f.cx.a.alu_rr(true, 0x39, R9, RCX);
                f.jfix(CC_A);
            }
            f.end(d, false, false);
            // IE ya descartado: el valor es valido
            if !sf {
                f.cx.a.mov_rr(R9, R9, false);
            }
            f.cx.st_x(rd as u8, R9, true);
            f.finish(w, pc, idx);
            true
        }
        _ => false,
    }
}

fn fp_dp3(cx: &mut Ctx, w: u32, pc: u64, idx: usize) -> bool {
    let t = bits(w, 23, 22);
    if t > 1 || !fma_available() {
        return false;
    }
    let d = t == 1;
    let (o1, o0) = (bit(w, 21), bit(w, 15));
    let (rm, ra, rn, rd) = (bits(w, 20, 16), bits(w, 14, 10), bits(w, 9, 5), bits(w, 4, 0));
    // FMADD (0,0): a + n*m   -> vfmadd231
    // FMSUB (0,1): a - n*m   -> vfnmadd231
    // FNMADD(1,0): -a - n*m  -> vfnmsub231
    // FNMSUB(1,1): n*m - a   -> vfmsub231
    let opc = match (o1, o0) {
        (false, false) => 0xB9u8,
        (false, true) => 0xBD,
        (true, false) => 0xBF,
        (true, true) => 0xBB,
    };
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.begin();
    f.ld(0, ra, d);
    f.ld(1, rn, d);
    f.ld(2, rm, d);
    f.cx.a.vex_fma(opc, d, 0, 1, 2);
    f.end(d, true, true);
    f.st(0, rd, d);
    f.finish(w, pc, idx);
    true
}

// ---------------------------------------------------------------------------------------------
// Cargas / almacenamientos SIMD/FP
// ---------------------------------------------------------------------------------------------

fn ldst(cx: &mut Ctx, w: u32, _pc: u64, _idx: usize) -> bool {
    let ls = match neonls::decode(w) {
        Some(l) => l,
        None => return false,
    };
    match ls {
        Ls::Reg { load, size, rt, rn, addr } => {
            cx.emit_ea(rn as u8, addr);
            cx.mask_tbi(RAX);
            if load {
                if size == 4 {
                    // OJO: post_wb_keep usa R9 como temporal; la mitad alta debe ir en otro registro. (Con R9, un
                    // `ldr qN, [xM], #imm` dejaba el puntero base en los 64 bits altos del registro vectorial.)
                    cx.a.mov_rm(RDX, RAX, 0, true);
                    cx.a.mov_rm(RCX, RAX, 8, true);
                    cx.post_wb_keep(rn as u8, addr);
                    cx.a.mov_mr(RBX, vo(rt), RDX, true);
                    cx.a.mov_mr(RBX, vo(rt) + 8, RCX, true);
                } else {
                    cx.a.load_ext(RDX, RAX, 0, size, 0);
                    cx.post_wb_keep(rn as u8, addr);
                    cx.a.mov_mr(RBX, vo(rt), RDX, true);
                    cx.a.mov_mi64(RBX, vo(rt) + 8, 0);
                }
            } else if size == 4 {
                cx.a.mov_mr(RBX, O_AUX_SCRATCH, RAX, true);
                cx.a.mov_rr(RDI, RAX, true);
                cx.a.mov_rm(RDX, RBX, vo(rt), true);
                cx.call_store(3);
                cx.a.mov_rm(RDI, RBX, O_AUX_SCRATCH, true);
                cx.a.lea(RDI, RDI, 8, true);
                cx.a.mov_rm(RDX, RBX, vo(rt) + 8, true);
                cx.call_store(3);
                cx.post_wb(rn as u8, addr);
            } else {
                cx.a.mov_rr(RDI, RAX, true);
                cx.a.mov_rm(RDX, RBX, vo(rt), true);
                cx.call_store(size);
                cx.post_wb(rn as u8, addr);
            }
            true
        }
        Ls::Lit { size, rt, imm } => {
            let a = _pc.wrapping_add(imm as u64) & TBI_MASK;
            cx.a.mov_ri(RAX, a);
            if size == 4 {
                cx.a.mov_rm(RDX, RAX, 0, true);
                cx.a.mov_rm(R9, RAX, 8, true);
                cx.a.mov_mr(RBX, vo(rt), RDX, true);
                cx.a.mov_mr(RBX, vo(rt) + 8, R9, true);
            } else {
                cx.a.load_ext(RDX, RAX, 0, size, 0);
                cx.a.mov_mr(RBX, vo(rt), RDX, true);
                cx.a.mov_mi64(RBX, vo(rt) + 8, 0);
            }
            true
        }
        Ls::Pair { load, size, rt, rt2, rn, addr } => {
            cx.emit_ea(rn as u8, addr);
            cx.mask_tbi(RAX);
            let step = 1i32 << size;
            if load {
                // todo se lee antes de escribir (rt puede coincidir con la base solo en enteros)
                if size == 4 {
                    cx.a.mov_rm(RDX, RAX, 0, true);
                    cx.a.mov_rm(RCX, RAX, 8, true);
                    cx.a.mov_rm(R8, RAX, 16, true);
                    cx.a.mov_rm(R10, RAX, 24, true);
                    cx.post_wb_keep(rn as u8, addr);
                    cx.a.mov_mr(RBX, vo(rt), RDX, true);
                    cx.a.mov_mr(RBX, vo(rt) + 8, RCX, true);
                    cx.a.mov_mr(RBX, vo(rt2), R8, true);
                    cx.a.mov_mr(RBX, vo(rt2) + 8, R10, true);
                } else {
                    cx.a.load_ext(RDX, RAX, 0, size, 0);
                    cx.a.load_ext(RCX, RAX, step, size, 0);
                    cx.post_wb_keep(rn as u8, addr);
                    cx.a.mov_mr(RBX, vo(rt), RDX, true);
                    cx.a.mov_mi64(RBX, vo(rt) + 8, 0);
                    cx.a.mov_mr(RBX, vo(rt2), RCX, true);
                    cx.a.mov_mi64(RBX, vo(rt2) + 8, 0);
                }
            } else {
                cx.a.mov_mr(RBX, O_AUX_SCRATCH, RAX, true);
                let parts: Vec<(i32, i32)> = if size == 4 {
                    vec![(0, vo(rt)), (8, vo(rt) + 8), (16, vo(rt2)), (24, vo(rt2) + 8)]
                } else {
                    vec![(0, vo(rt)), (step, vo(rt2))]
                };
                let sz = if size == 4 { 3 } else { size };
                for (off, src) in parts {
                    cx.a.mov_rm(RDI, RBX, O_AUX_SCRATCH, true);
                    if off != 0 {
                        cx.a.lea(RDI, RDI, off, true);
                    }
                    cx.a.mov_rm(RDX, RBX, src, true);
                    cx.call_store(sz);
                }
                cx.post_wb(rn as u8, addr);
            }
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// SIMD "three same" (vector, enteros/logicas/FP basicas)
// ---------------------------------------------------------------------------------------------

impl<'c, 'a> Fx<'c, 'a> {
    fn vld(&mut self, xmm: u8, r: u32, q: bool) {
        if q {
            self.cx.a.sse_rm(0, false, &[0x0F, 0x10], xmm, RBX, vo(r)); // movups
        } else {
            self.cx.a.sse_rm(0xF3, false, &[0x0F, 0x7E], xmm, RBX, vo(r)); // movq xmm, m64 (cero arriba)
        }
    }
    fn vst(&mut self, xmm: u8, r: u32, q: bool) {
        let a = &mut self.cx.a;
        if q {
            a.sse_rm(0, false, &[0x0F, 0x11], xmm, RBX, vo(r)); // movups
        } else {
            a.sse_rm(0x66, false, &[0x0F, 0xD6], xmm, RBX, vo(r)); // movq m64, xmm
            a.mov_mi64(RBX, vo(r) + 8, 0);
        }
    }
    /// Escalar (`s`): solo el carril 0 con movss/movsd, resto a cero; vector: 64 o 128 bits.
    fn ld_es(&mut self, xmm: u8, r: u32, s: bool, d: bool, q: bool) {
        if s {
            self.ld(xmm, r, d);
        } else {
            self.vld(xmm, r, q);
        }
    }
    fn pp(&mut self, pfx: u8, opc: &[u8], dst: u8, src: u8) {
        self.cx.a.sse_rr(pfx, false, opc, dst, src);
    }
    /// cmpps/cmppd dst, src, imm
    fn cmpf(&mut self, d: bool, dst: u8, src: u8, imm: u8) {
        self.cx.a.sse_rr(if d { 0x66 } else { 0 }, false, &[0x0F, 0xC2], dst, src);
        self.cx.a.u8(imm);
    }
    /// movmskps/pd eax, xmm ; test eax,eax ; jnz lenta
    fn bail_if_any(&mut self, d: bool, xmm: u8) {
        self.cx.a.sse_rr(if d { 0x66 } else { 0 }, false, &[0x0F, 0x50], RAX, xmm);
        self.cx.a.alu_rr(false, 0x85, RAX, RAX);
        self.jfix(CC_NE);
    }
    /// Lenta si algun carril del resultado (xmm0) es NaN.
    fn vec_nan(&mut self, d: bool) {
        self.pp(0, &[0x0F, 0x28], 3, 0); // movaps xmm3, xmm0
        self.cmpf(d, 3, 3, 3); // unord
        self.bail_if_any(d, 3);
    }
    /// Lenta si algun carril tiene |res| < 8*minnormal (constantes 0x0200_0000 / 0x0040_0000_0000_0000 leidas como
    /// float: 2^-123 / 2^-1019; cubre con margen el |res| < 2*minnormal necesario) y no esta en la mascara `exempt` (xmm7).
    fn vec_tiny(&mut self, d: bool) {
        let pf = if d { 0x66 } else { 0 };
        // xmm4 = |res| ; xmm5 = umbral
        self.pp(0x66, &[0x0F, 0x76], 4, 4); // pcmpeqd xmm4, xmm4
        self.cx.a.sse_rr(0x66, false, &[0x0F, if d { 0x73 } else { 0x72 }], 2, 4); // psrl?/q xmm4, 1
        self.cx.a.u8(1);
        self.pp(pf, &[0x0F, 0x54], 4, 0); // andps xmm4, xmm0
        if d {
            self.cx.a.mov_ri(RAX, 0x0040_0000_0000_0000);
            self.cx.a.sse_rr(0x66, true, &[0x0F, 0x6E], 5, RAX);
            self.pp(0x66, &[0x0F, 0x6C], 5, 5); // punpcklqdq
        } else {
            self.cx.a.mov_ri(RAX, 0x0200_0000);
            self.cx.a.sse_rr(0x66, false, &[0x0F, 0x6E], 5, RAX);
            self.pp(0, &[0x0F, 0xC6], 5, 5);
            self.cx.a.u8(0);
        }
        self.cmpf(d, 4, 5, 1); // abs < umbral
        self.pp(pf, &[0x0F, 0x55], 7, 4); // andnps xmm7, xmm4 -> ~exempt & tiny
        self.bail_if_any(d, 7);
    }
}

/// Seleccion de mutacion para la validacion de la prueba diferencial (solo en pruebas; 0 = codigo correcto).
#[cfg(test)]
fn mm_mut() -> u32 {
    std::env::var("HEDDLE_MM_MUT").ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}
#[cfg(not(test))]
fn mm_mut() -> u32 {
    0
}

impl<'c, 'a> Fx<'c, 'a> {
    /// FMAXNM/FMINNM/FMAX/FMIN sobre los carriles de xmm0 (a) y xmm1 (b); resultado en xmm2 (clobbera xmm3).
    ///
    /// Semantica ARM sin NaN (FPCR = 0, sin FZ/DN): max(a, b) = a < b ? b : a (numerico), y con dos ceros FMAX da +0 si
    /// alguno es +0 y FMIN da -0 si alguno es -0. Con valores numericamente iguales y distintos de cero los bits son
    /// identicos. Las instrucciones maxps/minps NO son equivalentes: con NaN devuelven el segundo operando (ARM: FMAX/FMIN
    /// propagan la NaN quietada, FMAXNM/FMINNM devuelven el numero; las senalizantes dan IOC) y con dos ceros (o
    /// cualquier par iguales) devuelven siempre `b`, sin distinguir el signo. Por eso:
    ///
    /// 1. GUARDA: cmpunordps(a, b) -> si algun carril es NaN (silenciosa o senalizante) -> helper. Cubre todos los casos
    ///    de NaN de las cuatro operaciones y su IOC sin reproducir las reglas de ARM. (La SNaN deja IE en MXCSR, que
    ///    fold_mxcsr pasa a IOC: el helper marca el mismo IOC con la SNaN.)
    /// 2. maxps/minps sobre carriles sin NaN: resultado correcto salvo en los carriles con a == b (cmpeqps, que incluye
    ///    +0 == -0).
    /// 3. En esos carriles el resultado es (a AND b) para el maximo y (a OR b) para el minimo: bits identicos -> mismo
    ///    valor; +0 y -0 -> el AND de los signos da +0 (FMAX), el OR da -0 (FMIN). Se mezcla con and/andn/or (sin
    ///    blendvps, asi no hace falta SSE4.1).
    ///
    /// Sin flags: ninguna operacion marca IDC (FZ = 0), IXC ni UFC; el DE de x86 con un subnormal no se pasa a FPSR.
    /// maxps/minps respetan el orden total sobre numeros finitos e infinitos y subnormales (DAZ = 0).
    fn minmax(&mut self, d: bool, is_min: bool) {
        let pf = if d { 0x66 } else { 0 };
        let m = mm_mut();
        if m != 3 {
            self.pp(0, &[0x0F, 0x28], 2, 0); // movaps xmm2, xmm0
            self.cmpf(d, 2, 1, 3); // unord
            self.bail_if_any(d, 2);
        }
        self.pp(0, &[0x0F, 0x28], 2, 0);
        self.cmpf(d, 2, 1, 0); // xmm2 = (a == b)
        self.pp(0, &[0x0F, 0x28], 3, 0);
        // xmm3 = a AND b (max) / a OR b (min); mutacion 2: confunde el signo del cero
        self.pp(pf, &[0x0F, if is_min != (m == 2) { 0x56 } else { 0x54 }], 3, 1);
        self.pp(pf, &[0x0F, if is_min { 0x5D } else { 0x5F }], 0, 1); // minps/maxps xmm0, xmm1
        if m == 1 {
            // mutacion 1: maxps/minps directo
            self.pp(0, &[0x0F, 0x28], 2, 0);
            return;
        }
        self.pp(pf, &[0x0F, 0x54], 3, 2); // xmm3 = eq & z
        self.pp(pf, &[0x0F, 0x55], 2, 0); // xmm2 = ~eq & r
        self.pp(pf, &[0x0F, 0x56], 2, 3); // xmm2 |= xmm3
    }
}

/// FMAXNMV/FMAXV/FMINNMV/FMINV (.4s) y FMAXNMP/FMAXP/FMINNMP/FMINP escalares (par de carriles). Sin NaN el maximo y
/// el minimo (con la regla del cero de `minmax`) son conmutativos y asociativos, asi que la reduccion en arbol da el
/// mismo resultado que el orden del interprete; con NaN en cualquier carril (guarda de `minmax`, ronda 1 cubre los 4
/// carriles) -> helper. FP16 (.8h, sz = 1 en across) y las formas reservadas: helper.
fn minmax_reduce(cx: &mut Ctx, w: u32, pc: u64, idx: usize, scalar: bool) -> bool {
    let q = bit(w, 30);
    let u = bit(w, 29);
    let (a_bit, sz) = (bit(w, 23), bit(w, 22));
    let opc = bits(w, 16, 12);
    let (rn, rd) = (bits(w, 9, 5), bits(w, 4, 0));
    if !u || !matches!(opc, 0b01100 | 0b01111) {
        return false;
    }
    if !scalar && (!q || sz) {
        return false; // .2s con Q=0 reservado; sz=1 con Q=1 reservado
    }
    let d = scalar && sz;
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.begin();
    if scalar {
        f.vld(0, rn, d); // .2s: movq (carriles altos a cero); .2d: movups
        f.pp(0, &[0x0F, 0x28], 1, 0);
        if d {
            f.pp(0x66, &[0x0F, 0x15], 1, 1); // unpckhpd xmm1, xmm1
        } else {
            f.pp(0, &[0x0F, 0xC6], 1, 1); // shufps xmm1, xmm1, 1
            f.cx.a.u8(1);
        }
        f.minmax(d, a_bit);
    } else {
        f.vld(0, rn, true);
        f.pp(0, &[0x0F, 0x28], 1, 0);
        f.pp(0, &[0x0F, 0xC6], 1, 1); // shufps xmm1, xmm1, 0xEE -> [v2, v3, v2, v3]
        f.cx.a.u8(0xEE);
        f.minmax(false, a_bit);
        f.pp(0, &[0x0F, 0x28], 0, 2);
        f.pp(0, &[0x0F, 0x28], 1, 2);
        f.pp(0, &[0x0F, 0xC6], 1, 1); // splat del carril 1
        f.cx.a.u8(0x55);
        f.minmax(false, a_bit);
    }
    f.st(2, rd, d);
    f.finish(w, pc, idx);
    true
}

fn simd3(cx: &mut Ctx, w: u32, pc: u64, idx: usize) -> bool {
    let q = bit(w, 30);
    let u = bit(w, 29);
    let size = bits(w, 23, 22);
    let (rm, op, rn, rd) = (bits(w, 20, 16), bits(w, 15, 11), bits(w, 9, 5), bits(w, 4, 0));
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    match op {
        0b00011 => {
            // logicas bit a bit
            f.vld(0, rn, q);
            f.vld(1, rm, q);
            let (fin, _) = match (u, size) {
                (false, 0) => {
                    f.pp(0x66, &[0x0F, 0xDB], 0, 1);
                    (0, 0)
                }
                (false, 1) => {
                    f.pp(0x66, &[0x0F, 0xDF], 1, 0); // xmm1 = ~m & n
                    (1, 0)
                }
                (false, 2) => {
                    f.pp(0x66, &[0x0F, 0xEB], 0, 1);
                    (0, 0)
                }
                (false, _) => {
                    f.pp(0x66, &[0x0F, 0x76], 2, 2); // todo unos
                    f.pp(0x66, &[0x0F, 0xEF], 1, 2);
                    f.pp(0x66, &[0x0F, 0xEB], 0, 1);
                    (0, 0)
                }
                (true, 0) => {
                    f.pp(0x66, &[0x0F, 0xEF], 0, 1);
                    (0, 0)
                }
                (true, 1) => {
                    // BSL: ((n ^ m) & d) ^ m
                    f.vld(2, rd, q);
                    f.pp(0x66, &[0x0F, 0xEF], 0, 1);
                    f.pp(0x66, &[0x0F, 0xDB], 0, 2);
                    f.pp(0x66, &[0x0F, 0xEF], 0, 1);
                    (0, 0)
                }
                (true, 2) => {
                    // BIT: d ^ ((d ^ n) & m)
                    f.vld(2, rd, q);
                    f.pp(0x66, &[0x0F, 0xEF], 0, 2);
                    f.pp(0x66, &[0x0F, 0xDB], 0, 1);
                    f.pp(0x66, &[0x0F, 0xEF], 0, 2);
                    (0, 0)
                }
                (true, _) => {
                    // BIF: d ^ ((d ^ n) & ~m)
                    f.vld(2, rd, q);
                    f.pp(0x66, &[0x0F, 0xEF], 0, 2);
                    f.pp(0x66, &[0x0F, 0xDF], 1, 0);
                    f.pp(0x66, &[0x0F, 0xEF], 1, 2);
                    (1, 0)
                }
            };
            f.vst(fin, rd, q);
            true
        }
        0b10000 | 0b10001 | 0b00110 | 0b00111 | 0b10011 | 0b01100 | 0b01101 => {
            if size == 3 && !q {
                return false;
            }
            let sz = size as usize;
            let sse41 = sse41_available();
            // (prefijo-opcode, intercambiar operandos, invertir resultado)
            let (opc, swap, inv): (&[u8], bool, bool) = match (op, u) {
                (0b10000, false) => (&[[0x0F, 0xFC], [0x0F, 0xFD], [0x0F, 0xFE], [0x0F, 0xD4]][sz], false, false),
                (0b10000, true) => (&[[0x0F, 0xF8], [0x0F, 0xF9], [0x0F, 0xFA], [0x0F, 0xFB]][sz], false, false),
                (0b10001, true) => {
                    if sz == 3 {
                        if !sse41 {
                            return false;
                        }
                        (&[0x0F, 0x38, 0x29], false, false)
                    } else {
                        (&[[0x0F, 0x74], [0x0F, 0x75], [0x0F, 0x76]][sz], false, false)
                    }
                }
                (0b00110, false) => {
                    if sz == 3 {
                        return false; // pcmpgtq necesita SSE4.2: ruta lenta
                    }
                    (&[[0x0F, 0x64], [0x0F, 0x65], [0x0F, 0x66]][sz], false, false)
                }
                (0b00111, false) => {
                    if sz == 3 {
                        return false;
                    }
                    (&[[0x0F, 0x64], [0x0F, 0x65], [0x0F, 0x66]][sz], true, true) // n>=m == !(m>n)
                }
                (0b10011, false) => match sz {
                    1 => (&[0x0F, 0xD5], false, false),
                    2 if sse41 => (&[0x0F, 0x38, 0x40], false, false),
                    _ => return false,
                },
                (0b01100, false) | (0b01101, false) | (0b01100, true) | (0b01101, true) => {
                    if sz == 3 {
                        return false;
                    }
                    let mx = op == 0b01100;
                    match (u, mx, sz) {
                        (false, true, 1) => (&[0x0F, 0xEE], false, false),
                        (false, false, 1) => (&[0x0F, 0xEA], false, false),
                        (true, true, 0) => (&[0x0F, 0xDE], false, false),
                        (true, false, 0) => (&[0x0F, 0xDA], false, false),
                        _ if !sse41 => return false,
                        (false, true, 0) => (&[0x0F, 0x38, 0x3C], false, false),
                        (false, false, 0) => (&[0x0F, 0x38, 0x38], false, false),
                        (false, true, 2) => (&[0x0F, 0x38, 0x3D], false, false),
                        (false, false, 2) => (&[0x0F, 0x38, 0x39], false, false),
                        (true, true, 1) => (&[0x0F, 0x38, 0x3E], false, false),
                        (true, false, 1) => (&[0x0F, 0x38, 0x3A], false, false),
                        (true, true, 2) => (&[0x0F, 0x38, 0x3F], false, false),
                        (true, false, 2) => (&[0x0F, 0x38, 0x3B], false, false),
                        _ => return false,
                    }
                }
                _ => return false,
            };
            f.vld(0, rn, q);
            f.vld(1, rm, q);
            if swap {
                f.pp(0x66, opc, 1, 0);
                if inv {
                    f.pp(0x66, &[0x0F, 0x76], 2, 2);
                    f.pp(0x66, &[0x0F, 0xEF], 1, 2);
                }
                f.vst(1, rd, q);
            } else {
                f.pp(0x66, opc, 0, 1);
                f.vst(0, rd, q);
            }
            true
        }
        0b11000 | 0b11110 => {
            // FMAXNM/FMINNM (11000), FMAX/FMIN (11110); U=1: pares (FMAXNMP, FMAXP, ...). Ver `Fx::minmax`.
            let d = bit(w, 22);
            if d && !q {
                return false;
            }
            let is_min = bit(w, 23);
            f.begin();
            if u {
                // pares: pares pares (evens) e impares (odds) de la concatenacion n:m
                if d {
                    f.vld(0, rn, true);
                    f.vld(4, rm, true);
                    f.pp(0, &[0x0F, 0x28], 1, 0);
                    f.pp(0x66, &[0x0F, 0x14], 0, 4); // unpcklpd -> [n0, m0]
                    f.pp(0x66, &[0x0F, 0x15], 1, 4); // unpckhpd -> [n1, m1]
                } else if q {
                    f.vld(4, rn, true);
                    f.vld(5, rm, true);
                    f.pp(0, &[0x0F, 0x28], 0, 4);
                    f.pp(0, &[0x0F, 0xC6], 0, 5); // shufps xmm0, xmm5, 0x88 -> [n0, n2, m0, m2]
                    f.cx.a.u8(0x88);
                    f.pp(0, &[0x0F, 0xC6], 4, 5);
                    f.cx.a.u8(0xDD); // [n1, n3, m1, m3]
                    f.pp(0, &[0x0F, 0x28], 1, 4);
                } else {
                    f.vld(4, rn, false);
                    f.vld(5, rm, false);
                    f.pp(0x66, &[0x0F, 0x6C], 4, 5); // punpcklqdq -> [n0, n1, m0, m1]
                    f.pp(0, &[0x0F, 0x28], 0, 4);
                    f.pp(0, &[0x0F, 0xC6], 0, 0);
                    f.cx.a.u8(0x08); // [c0, c2, ., .]
                    f.pp(0, &[0x0F, 0xC6], 4, 4);
                    f.cx.a.u8(0x0D); // [c1, c3, ., .]
                    f.pp(0, &[0x0F, 0x28], 1, 4);
                }
            } else {
                f.vld(0, rn, q);
                f.vld(1, rm, q);
            }
            f.minmax(d, is_min);
            f.vst(2, rd, q);
            f.finish(w, pc, idx);
            true
        }
        0b11010 | 0b11011 | 0b11001 => {
            // FP vectorial: FADD/FSUB (11010), FMUL (11011,U=1), FMLA/FMLS (11001, U=0)
            let d = bit(w, 22);
            if d && !q {
                return false;
            }
            let hi = bit(w, 23);
            let pf = if d { 0x66 } else { 0 };
            match (op, u, hi) {
                (0b11010, false, _) => {
                    f.begin();
                    f.vld(0, rn, q);
                    f.vld(1, rm, q);
                    f.pp(pf, &[0x0F, if hi { 0x5C } else { 0x58 }], 0, 1);
                    f.vec_nan(d);
                    f.vst(0, rd, q);
                }
                (0b11011, true, false) => {
                    f.begin();
                    f.vld(0, rn, q);
                    f.vld(1, rm, q);
                    f.pp(0, &[0x0F, 0x28], 2, 0); // copia de n
                    f.pp(pf, &[0x0F, 0x59], 0, 1);
                    f.vec_nan(d);
                    f.pp(0x66, &[0x0F, 0xEF], 6, 6); // xmm6 = 0
                    f.cmpf(d, 2, 6, 0);
                    f.cmpf(d, 1, 6, 0);
                    f.pp(0x66, &[0x0F, 0xEB], 2, 1); // exento = n==0 | m==0
                    f.pp(0, &[0x0F, 0x28], 7, 2);
                    f.vec_tiny(d);
                    f.vst(0, rd, q);
                }
                (0b11001, false, _) => {
                    if !fma_available() {
                        return false;
                    }
                    f.begin();
                    f.vld(0, rd, q); // acumulador
                    f.vld(1, rn, q);
                    f.vld(2, rm, q);
                    f.pp(0, &[0x0F, 0x28], 6, 0); // copia de a
                    f.cx.a.vex_fma(if hi { 0xBC } else { 0xB8 }, d, 0, 1, 2);
                    f.vec_nan(d);
                    // exento = (a==0) & (n==0 | m==0)
                    f.pp(0x66, &[0x0F, 0xEF], 7, 7);
                    f.cmpf(d, 6, 7, 0);
                    f.cmpf(d, 1, 7, 0);
                    f.cmpf(d, 2, 7, 0);
                    f.pp(0x66, &[0x0F, 0xEB], 1, 2);
                    f.pp(0x66, &[0x0F, 0xDB], 1, 6);
                    f.pp(0, &[0x0F, 0x28], 7, 1);
                    f.vec_tiny(d);
                    f.vst(0, rd, q);
                }
                _ => return false,
            }
            f.finish(w, pc, idx);
            true
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------------------------
// SIMD "by element": FMUL / FMULX / FMLA / FMLS con el elemento v[rm][idx] (vector .2s/.4s/.2d y escalar s/d)
// ---------------------------------------------------------------------------------------------

/// Traduce FMUL, FMULX, FMLA y FMLS por elemento (S y D; FP16 queda al helper). El elemento se carga con
/// movss/movsd desde su carril y se duplica con shufps; luego es la misma ruta rapida que "three same":
///
/// - FPCR = 0 (`begin`): con DN/FZ/RMode/AHP o trampas activas el resultado o las flags difieren.
/// - FMUL/FMULX = mulps/mulpd (un redondeo RN, como FPMul). FMLA = vfmadd231, FMLS = vfnmadd231 (FMA3): ARM
///   FPMulAdd es FUSIONADO (un solo redondeo de a + n*m exacto); vfnmadd niega el producto exacto, asi que
///   -(n*m)+a = a+(-n)*m bit a bit, incluido el signo del cero. Sin FMA en el host: helper (no se aproxima con
///   mul+add, que redondea dos veces). FMA se consulta una vez (CPUID cacheado) al traducir.
/// - NaN en cualquier carril del resultado -> helper (`vec_nan`). Cubre: (1) un operando NaN: x86 devuelve el
///   primer operando NaN quietado y ARM prioriza las senalizantes (FPProcessNaNs) y marca IOC; (2) 0*inf (y en
///   FMLA inf-inf): ARM da la NaN por defecto con IOC; x86 da NaN con IE, que no se pasa a FPSR. FMULX solo
///   difiere de FMUL en 0*inf (= +-2.0 sin IOC), que en x86 produce NaN: cae al helper, asi que FMULX traducido
///   es exactamente FMUL en todos los casos que acepta.
/// - Resultado pequeno (incluido cero) -> helper (`vec_tiny`; su umbral es 8*minnormal, mas conservador que el
///   necesario |r| < 2*minnormal, exp <= 1): ARM detecta tininess
///   ANTES de redondear y x86 DESPUES, asi que en el borde de la menor normal UFC difiere; ademas un subnormal
///   inexacto lleva UFC en ARM y UE en x86, que no se pliega. Exentos: los carriles con cero EXACTO (FMUL: n = 0
///   o m = 0; FMLA/FMLS: a = 0 y (n = 0 o m = 0)): el producto es +-0 sin error, con el signo xor en ambos, y
///   la suma de ceros sigue la misma regla de signo con RN (-0 solo si ambos son -0). Sin flags en ninguno.
/// - Infinitos sin NaN: inf*x (x finito distinto de 0) = inf con signo xor y sin flags en ambos; inf+finito
///   = inf. Desbordamiento: OE+PE en x86 = OFC+IXC en ARM (los pliega `fold_mxcsr`), resultado inf con RN.
/// - Si se descarta la ruta rapida, MXCSR puede quedar con PE/OE de carriles cuyo resultado es el mismo que el
///   del interprete: el helper marca entonces las mismas IXC/OFC, asi que no aparecen flags de mas.
/// - .2s: los carriles altos de n, a y del elemento duplicado son 0 (exentos, sin flags); .2d con Q=0 y D con
///   L=1 son UNDEFINED: helper.
fn by_elem(cx: &mut Ctx, w: u32, pc: u64, idx: usize, s: bool) -> bool {
    let q = bit(w, 30);
    let u = bit(w, 29);
    let d = bit(w, 22);
    let (l, h) = (bit(w, 21) as u32, bit(w, 11) as u32);
    let (rm, opc, rn, rd) = (bits(w, 20, 16), bits(w, 15, 12), bits(w, 9, 5), bits(w, 4, 0));
    // 0 = FMUL, 1 = FMULX, 2 = FMLA, 3 = FMLS
    let kind = match (u, opc) {
        (false, 0b1001) => 0,
        (true, 0b1001) => 1,
        (false, 0b0001) => 2,
        (false, 0b0101) => 3,
        _ => return false,
    };
    let lane = if d {
        if l != 0 {
            return false;
        }
        h
    } else {
        (h << 1) | l
    };
    if d && !q && !s {
        return false;
    }
    if kind >= 2 && !fma_available() {
        return false;
    }
    let full = q && !s;
    let pf = if d { 0x66 } else { 0 };
    let mut f = Fx { cx, fix: Vec::new(), restore_csr: false };
    f.begin();
    // elemento en xmm1 (FMUL) o xmm2 (FMLA): movss/movsd desde su carril (pone a cero el resto)
    let xm = if kind >= 2 { 2 } else { 1 };
    if d {
        f.cx.a.sse_rm(0xF2, false, &[0x0F, 0x10], xm, RBX, vo(rm) + 8 * lane as i32);
    } else {
        f.cx.a.sse_rm(0xF3, false, &[0x0F, 0x10], xm, RBX, vo(rm) + 4 * lane as i32);
    }
    if !s {
        // D: (e, e); S con Q: (e, e, e, e); S sin Q: (e, e, 0, 0)
        f.pp(0, &[0x0F, 0xC6], xm, xm);
        f.cx.a.u8(if d { 0x44 } else if q { 0x00 } else { 0x50 });
    }
    // n (y a) con el ancho de la instruccion; escalar: solo el carril 0, resto a cero
    if kind < 2 {
        f.ld_es(0, rn, s, d, q);
        f.pp(0, &[0x0F, 0x28], 2, 0); // copia de n
        f.pp(pf, &[0x0F, 0x59], 0, 1); // mulps/mulpd
        f.vec_nan(d);
        f.pp(0x66, &[0x0F, 0xEF], 6, 6); // xmm6 = 0
        f.cmpf(d, 2, 6, 0);
        f.cmpf(d, 1, 6, 0);
        f.pp(0x66, &[0x0F, 0xEB], 2, 1); // exento = n==0 | m==0
        f.pp(0, &[0x0F, 0x28], 7, 2);
        f.vec_tiny(d);
    } else {
        f.ld_es(0, rd, s, d, q); // acumulador
        f.ld_es(1, rn, s, d, q);
        f.pp(0, &[0x0F, 0x28], 6, 0); // copia de a
        f.cx.a.vex_fma(if kind == 3 { 0xBC } else { 0xB8 }, d, 0, 1, 2);
        f.vec_nan(d);
        // exento = (a==0) & (n==0 | m==0)
        f.pp(0x66, &[0x0F, 0xEF], 7, 7);
        f.cmpf(d, 6, 7, 0);
        f.cmpf(d, 1, 7, 0);
        f.cmpf(d, 2, 7, 0);
        f.pp(0x66, &[0x0F, 0xEB], 1, 2);
        f.pp(0x66, &[0x0F, 0xDB], 1, 6);
        f.pp(0, &[0x0F, 0x28], 7, 1);
        f.vec_tiny(d);
    }
    if s {
        f.st(0, rd, d);
    } else {
        f.vst(0, rd, full);
    }
    f.finish(w, pc, idx);
    true
}

#[cfg(test)]
mod tests {
    /// `ldr qN, [xM], #imm` (carga de 128 bits con post-indice): el JIT debe dar lo mismo que el interprete.
    /// Regresion: la mitad alta del registro quedaba con el puntero base.
    #[test]
    fn carga_de_128_bits_con_post_indice() {
        use crate::sys::*;
        // str q0, [sp, #-16]! ; ldrb w8, [sp, #15] ; and w8, w8, #0x7f ; strb w8, [sp, #15] ; ldr q0, [sp], #16 ; ret
        let words: [u32; 6] = [0x3c9f0fe0, 0x39403fe8, 0x12001908, 0x39003fe8, 0x3cc107e0, 0xd65f03c0];
        let p = unsafe { mmap(std::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) };
        for (i, w) in words.iter().enumerate() {
            unsafe { *((p as *mut u32).add(i)) = *w };
        }
        let input = [0x1111_2222_3333_4444u64, 0xc000_0000_0000_0000];
        let (_, jit, _) = crate::rt::call_guest_q(p as u64, &[], &[input]);
        let mut c = crate::cpu::Cpu::new();
        let st = vec![0u64; 64];
        c.x[31] = st.as_ptr() as u64 + 256;
        c.v[0] = input;
        c.pc = p as u64;
        for _ in 0..5 {
            let _ = crate::interp::step(&mut c);
        }
        assert_eq!(c.v[0], [0x1111_2222_3333_4444, 0x4000_0000_0000_0000], "interprete");
        assert_eq!(jit, c.v[0], "el JIT difiere del interprete");
        assert_eq!(c.x[31], st.as_ptr() as u64 + 256, "sp debe volver a su valor");
    }

    /// Prueba diferencial de las rutas rapidas de by_elem (FMUL/FMULX/FMLA/FMLS por elemento) y de FMUL/FADD/FSUB
    /// escalares: cada caso se ejecuta por el bloque traducido (instruccion + SVC) y por `interp::exec`, y se
    /// comparan bit a bit los 32 registros vectoriales (destino y no destino) y FPSR.
    mod dif {
        use super::super::super::{clear_mxcsr, fold_mxcsr, BlockFn, Jit, HELPER_CALLS};
        use crate::cpu::Cpu;
        use std::sync::atomic::Ordering::Relaxed;

        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 >> 12;
                self.0 ^= self.0 << 25;
                self.0 ^= self.0 >> 27;
                self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
            }
            fn below(&mut self, n: u64) -> u64 {
                self.next() % n
            }
            fn coin(&mut self) -> bool {
                self.next() & 1 != 0
            }
        }

        /// (bits de exponente, bits de mantisa)
        fn fmt(d: bool) -> (u32, u32) {
            if d {
                (11, 52)
            } else {
                (8, 23)
            }
        }
        fn mk(d: bool, sign: bool, e: u64, m: u64) -> u64 {
            let (eb, mb) = fmt(d);
            ((sign as u64) << (eb + mb)) | (e << mb) | (m & ((1u64 << mb) - 1))
        }
        fn expo(d: bool, v: u64) -> u64 {
            let (eb, mb) = fmt(d);
            (v >> mb) & ((1 << eb) - 1)
        }

        /// Un valor de un carril: aleatorio, normal, subnormal, +-0, +-inf, NaN silenciosa y senalizante, borde de la
        /// menor normal (y de 2*minnormal), potencias de dos, borde del maximo, enteros pequenos y valores cerca de 1.
        fn lane(r: &mut Rng, d: bool) -> u64 {
            let (eb, mb) = fmt(d);
            let emax = (1u64 << eb) - 1;
            let bias = emax >> 1;
            let mm = (1u64 << mb) - 1;
            let sg = r.coin();
            match r.below(15) {
                0 | 1 => r.next() & if d { u64::MAX } else { 0xFFFF_FFFF },
                2 | 3 => mk(d, sg, 1 + r.below(emax - 1), r.next()),
                4 => {
                    let m = (r.next() & mm) >> r.below(mb as u64);
                    mk(d, sg, 0, m.max(1))
                }
                5 => mk(d, sg, 0, 0),
                6 => mk(d, sg, emax, 0),
                7 => mk(d, sg, emax, (1 << (mb - 1)) | r.next()),
                8 => mk(d, sg, emax, ((r.next() & (mm >> 1)) >> r.below(mb as u64)).max(1)),
                9 => {
                    // minnormal o 2*minnormal +- 4 ulps (en bits, sin signo)
                    let base = (1 + r.below(2)) << mb;
                    let k = r.below(9) as i64 - 4;
                    mk(d, sg, 0, 0) | (base as i64 + k) as u64
                }
                10 => {
                    if r.below(4) == 0 {
                        mk(d, sg, 0, 1 << r.below(mb as u64))
                    } else {
                        mk(d, sg, 1 + r.below(emax - 1), 0)
                    }
                }
                11 => mk(d, sg, emax - 1, mm - r.below(4)),
                12 => {
                    // entero pequeno por 2^k (k pequeno): cancelaciones exactas
                    let n = 1 + r.below(64);
                    let v = if d { ((n as f64) * (2f64).powi(r.below(9) as i32 - 4)).to_bits() } else { ((n as f32) * (2f32).powi(r.below(9) as i32 - 4)).to_bits() as u64 };
                    v | ((sg as u64) << (eb + mb))
                }
                13 => mk(d, sg, bias - 2 + r.below(5), r.next()),
                _ => mk(d, sg, 1 + r.below(emax - 1), r.next() >> r.below(mb as u64 + 1)),
            }
        }

        /// Valor cuyo producto con `y` cae cerca de la menor normal (subnormal, borde, cero por underflow) o del
        /// desbordamiento. Si `y` es especial, un carril cualquiera.
        fn target(r: &mut Rng, d: bool, y: u64) -> u64 {
            let (eb, mb) = fmt(d);
            let emax = (1i64 << eb) - 1;
            let bias = emax >> 1;
            let ey = expo(d, y) as i64;
            if ey == 0 || ey == emax {
                return lane(r, d);
            }
            let t = if r.below(4) == 0 { emax - 4 + r.below(5) as i64 } else { -(mb as i64) - 3 + r.below(mb as u64 + 7) as i64 };
            let en = t - ey + bias;
            if en < 1 || en >= emax {
                return lane(r, d);
            }
            let m = if r.below(4) == 0 { (1u64 << mb) - 1 - r.below(3) } else if r.below(4) == 0 { r.below(3) } else { r.next() };
            mk(d, r.coin(), en as u64, m)
        }

        /// -(n*y) redondeado al formato, +- unos ulps: FMLA/FMLS con cancelacion casi total (resultados pequenos,
        /// cero exacto y por underflow).
        fn cancel(r: &mut Rng, d: bool, n: u64, y: u64, neg: bool) -> u64 {
            let p = if d {
                (f64::from_bits(n) * f64::from_bits(y)).to_bits()
            } else {
                ((f32::from_bits(n as u32) as f64 * f32::from_bits(y as u32) as f64) as f32).to_bits() as u64
            };
            let sb = 1u64 << (fmt(d).0 + fmt(d).1);
            let p = if neg { p } else { p ^ sb };
            let k = r.below(5) as i64 - 2;
            (p as i64).wrapping_add(k) as u64 & if d { u64::MAX } else { 0xFFFF_FFFF }
        }

        fn get(v: &[u64; 2], d: bool, i: usize) -> u64 {
            if d {
                v[i]
            } else {
                (v[i / 2] >> (32 * (i % 2))) & 0xFFFF_FFFF
            }
        }
        fn set(v: &mut [u64; 2], d: bool, i: usize, x: u64) {
            if d {
                v[i] = x;
            } else {
                let sh = 32 * (i % 2);
                v[i / 2] = (v[i / 2] & !(0xFFFF_FFFFu64 << sh)) | ((x & 0xFFFF_FFFF) << sh);
            }
        }

        #[derive(Clone, Copy, PartialEq)]
        enum K {
            Fmul,
            Fmulx,
            Fmla,
            Fmls,
            /// escalar FP dp2: FMUL, FADD, FSUB
            S2(u32),
            /// FMAXNM/FMINNM/FMAX/FMIN y variantes: 0 = vector (incluye pares), 1 = resultado escalar (across, par escalar)
            Mm(u8),
        }

        struct Form {
            name: String,
            k: K,
            d: bool,
            words: Vec<u32>,
        }

        fn by_elem_word(k: K, s: bool, q: bool, d: bool, idx: u32, rd: u32, rn: u32, rm: u32) -> u32 {
            let (u, opc) = match k {
                K::Fmul => (0, 0b1001),
                K::Fmulx => (1, 0b1001),
                K::Fmla => (0, 0b0001),
                _ => (0, 0b0101),
            };
            let (h, l) = if d { (idx, 0) } else { (idx >> 1, idx & 1) };
            let top = if s { (1 << 30) | (0b11111 << 24) } else { ((q as u32) << 30) | (0b01111 << 24) };
            top | (u << 29) | ((2 + d as u32) << 22) | (l << 21) | ((rm & 31) << 16) | (opc << 12) | (h << 11) | (rn << 5) | rd
        }

        const REGS: [(u32, u32, u32); 7] = [(0, 1, 2), (3, 3, 4), (5, 6, 5), (7, 8, 8), (9, 10, 17), (31, 30, 31), (16, 16, 16)];

        fn forms() -> Vec<Form> {
            let mut v = Vec::new();
            for (k, kn) in [(K::Fmul, "FMUL"), (K::Fmulx, "FMULX"), (K::Fmla, "FMLA"), (K::Fmls, "FMLS")] {
                for (s, q, d, sn) in [(false, false, false, "2s"), (false, true, false, "4s"), (false, true, true, "2d"), (true, false, false, "s (escalar)"), (true, false, true, "d (escalar)")] {
                    let mut words = Vec::new();
                    for idx in 0..if d { 2 } else { 4 } {
                        for &(rd, rn, rm) in &REGS {
                            words.push(by_elem_word(k, s, q, d, idx, rd, rn, rm));
                        }
                    }
                    v.push(Form { name: format!("{} por elemento {}", kn, sn), k, d, words });
                }
            }
            // FMAXNM/FMINNM/FMAX/FMIN vectoriales (U=0), pares (U=1), across-lanes y par escalar
            for (opc, on) in [(0b11000u32, "NM"), (0b11110, "")] {
                for u in [0u32, 1] {
                    for mn in [0u32, 1] {
                        for (q, d, sn) in [(false, false, "2s"), (true, false, "4s"), (true, true, "2d")] {
                            let words = REGS
                                .iter()
                                .map(|&(rd, rn, rm)| (q as u32) << 30 | u << 29 | 0b01110 << 24 | mn << 23 | (d as u32) << 22 | 1 << 21 | rm << 16 | opc << 11 | 1 << 10 | rn << 5 | rd)
                                .collect();
                            let nm = format!("FM{}{}{}{} {}", if mn == 0 { "AX" } else { "IN" }, on, if u == 1 { "P" } else { "" }, "", sn);
                            v.push(Form { name: nm, k: K::Mm(0), d, words });
                        }
                    }
                }
            }
            for (opc, on) in [(0b01100u32, "NM"), (0b01111, "")] {
                for mn in [0u32, 1] {
                    // across .4s
                    let words = REGS.iter().map(|&(rd, rn, _)| 1 << 30 | 1 << 29 | 0b01110 << 24 | mn << 23 | 0b11000 << 17 | opc << 12 | 0b10 << 10 | rn << 5 | rd).collect();
                    v.push(Form { name: format!("FM{}{}V .4s", if mn == 0 { "AX" } else { "IN" }, on), k: K::Mm(1), d: false, words });
                    // par escalar s y d
                    for d in [false, true] {
                        let words = REGS.iter().map(|&(rd, rn, _)| 0b01 << 30 | 1 << 29 | 0b11110 << 24 | mn << 23 | (d as u32) << 22 | 0b11000 << 17 | opc << 12 | 0b10 << 10 | rn << 5 | rd).collect();
                        v.push(Form { name: format!("FM{}{}P escalar {}", if mn == 0 { "AX" } else { "IN" }, on, if d { "d" } else { "s" }), k: K::Mm(1), d, words });
                    }
                }
            }
            for (op, on) in [(0u32, "FMUL"), (2, "FADD"), (3, "FSUB")] {
                for d in [false, true] {
                    let words = REGS.iter().map(|&(rd, rn, rm)| 0x1E20_0800 | ((d as u32) << 22) | (rm << 16) | (op << 12) | (rn << 5) | rd).collect();
                    v.push(Form { name: format!("{} escalar {}", on, if d { "d" } else { "s" }), k: K::S2(op), d, words });
                }
            }
            v
        }

        fn regs(w: u32) -> (u32, u32, u32) {
            ((w >> 16) & 31, (w >> 5) & 31, w & 31)
        }
        fn elem_idx(w: u32, d: bool) -> usize {
            let (h, l) = ((w >> 11) & 1, (w >> 21) & 1);
            (if d { h } else { (h << 1) | l }) as usize
        }

        /// Ejecuta `n` casos por forma; devuelve el numero total de diferencias (imprime las primeras).
        pub fn run(n: usize, seed: u64, verbose: bool) -> u64 {
            let mut r = Rng(seed | 1);
            let mut jit = Jit::new();
            jit.chain = false;
            let mut total = 0u64;
            // el codigo de todas las formas vive hasta el final: la cache del Jit va por pc y un vector liberado
            // podria reaparecer en la misma direccion con otra instruccion
            let mut keep: Vec<Vec<u32>> = Vec::new();
            for f in forms() {
                // cada palabra seguida de SVC #0, en un bloque propio
                let mut code = vec![0u32; 2 * f.words.len()];
                let mut fns: Vec<(u32, BlockFn, crate::decode::Op)> = Vec::new();
                for (i, &w) in f.words.iter().enumerate() {
                    code[2 * i] = w;
                    code[2 * i + 1] = 0xD400_0001;
                }
                for (i, &w) in f.words.iter().enumerate() {
                    let pc = code.as_ptr() as u64 + 8 * i as u64;
                    fns.push((w, jit.lookup(pc), crate::decode::decode(w)));
                }
                let h0 = HELPER_CALLS.load(Relaxed);
                let (mut diffs, mut flags) = (0u64, [0u64; 8]);
                // casos por la ruta rapida: total, con algun carril cero, con algun carril inf, con OFC, con IXC
                let mut rap = [0u64; 5];
                let d = f.d;
                let nl = if d { 2 } else { 4 };
                for c in 0..n {
                    let (w, bf, op) = &fns[c % fns.len()];
                    let (rm, rn, rd) = regs(*w);
                    let mut a = Cpu::new();
                    for x in 0..32 {
                        a.v[x] = [r.next(), r.next()];
                    }
                    for i in 0..nl {
                        let mut m = a.v[rm as usize];
                        set(&mut m, d, i, lane(&mut r, d));
                        a.v[rm as usize] = m;
                    }
                    let y = match f.k {
                        K::S2(_) => get(&a.v[rm as usize], d, 0),
                        _ => get(&a.v[rm as usize], d, elem_idx(*w, d)),
                    };
                    for i in 0..nl {
                        let x = match (f.k, r.below(8)) {
                            (K::S2(op), 0..=2) if op != 0 => {
                                // FADD/FSUB: n igual u opuesto a m, o a pocos ulps
                                let sb = 1u64 << (fmt(d).0 + fmt(d).1);
                                let k = if r.coin() { 0 } else { r.below(5) as i64 - 2 };
                                ((y ^ if r.coin() { sb } else { 0 }) as i64).wrapping_add(k) as u64 & if d { u64::MAX } else { 0xFFFF_FFFF }
                            }
                            (K::Mm(_), _) => lane(&mut r, d),
                            (_, 0..=3) => target(&mut r, d, y),
                            _ => lane(&mut r, d),
                        };
                        let mut v = a.v[rn as usize];
                        set(&mut v, d, i, x);
                        a.v[rn as usize] = v;
                    }
                    if let K::Mm(_) = f.k {
                        // casos dirigidos: la mitad sin NaN (para que la ruta rapida se ejecute), ceros +-0, infinitos,
                        // n = +-m (incluye ceros de signo distinto) y carril vecino igual u opuesto (pares)
                        let sb = 1u64 << (fmt(d).0 + fmt(d).1);
                        let emax = (1u64 << fmt(d).0) - 1;
                        let nonan = r.coin();
                        let mut v = a.v[rn as usize];
                        let mut mv = a.v[rm as usize];
                        for i in 0..nl {
                            for which in 0..2 {
                                let vv = if which == 0 { &mut v } else { &mut mv };
                                let mut x = get(vv, d, i);
                                if nonan && expo(d, x) == emax && (x & ((1u64 << fmt(d).1) - 1)) != 0 {
                                    x = mk(d, r.coin(), 1 + r.below(emax - 1), r.next());
                                }
                                match r.below(8) {
                                    0 | 1 => x &= sb,
                                    2 => x = mk(d, r.coin(), emax, 0),
                                    _ => {}
                                }
                                set(vv, d, i, x);
                            }
                            if r.below(4) == 0 {
                                let mi = get(&mv, d, i);
                                set(&mut v, d, i, mi ^ if r.coin() { sb } else { 0 });
                            }
                            if r.below(4) == 0 && i > 0 {
                                let xi = get(&v, d, i - 1);
                                set(&mut v, d, i, xi ^ if r.coin() { sb } else { 0 });
                            }
                        }
                        a.v[rm as usize] = mv;
                        a.v[rn as usize] = v;
                        if rm == rn {
                            a.v[rm as usize] = v;
                        }
                    }
                    if matches!(f.k, K::Fmla | K::Fmls) {
                        let ycur = get(&a.v[rm as usize], d, elem_idx(*w, d));
                        for i in 0..nl {
                            let nv = get(&a.v[rn as usize], d, i);
                            let x = if r.below(3) == 0 { cancel(&mut r, d, nv, ycur, f.k == K::Fmls) } else { lane(&mut r, d) };
                            let mut v = a.v[rd as usize];
                            set(&mut v, d, i, x);
                            a.v[rd as usize] = v;
                        }
                    }
                    // FPCR = 0 y FPSR limpio; 1/64 con FPCR != 0 (FZ, DN, RMode) y 1/16 con FPSR previo
                    a.fpcr = if r.below(64) == 0 { [1 << 24, 1 << 25, 1 << 22, 2 << 22, 3 << 22][r.below(5) as usize] } else { 0 };
                    a.fpsr = if r.below(16) == 0 { r.next() & 0x9F } else { 0 };
                    let mut b = Cpu::new();
                    b.v = a.v;
                    b.fpcr = a.fpcr;
                    b.fpsr = a.fpsr;
                    let v0 = a.v;
                    let fpsr0 = a.fpsr;
                    // interprete
                    a.pc = 0x1000;
                    let fl = crate::interp::exec(&mut a, op);
                    assert!(matches!(fl, crate::interp::Flow::Next), "{:#x}: el interprete no devolvio Next", w);
                    // JIT
                    b.pc = code.as_ptr() as u64;
                    clear_mxcsr();
                    let hc = HELPER_CALLS.load(Relaxed);
                    let st = bf(&mut *b);
                    let rapida = HELPER_CALLS.load(Relaxed) - hc == 1; // solo el SVC
                    fold_mxcsr(&mut b);
                    assert_eq!(st, 1, "{:#x}: el bloque no termino en el SVC", w);
                    let nuevas = a.fpsr & !fpsr0;
                    if rapida {
                        let nr = if matches!(f.k, K::S2(_) | K::Mm(1)) || (*w >> 28) & 1 == 1 { 1 } else if d || (*w >> 30) & 1 == 1 { nl } else { 2 };
                        let res: Vec<u64> = (0..nr).map(|i| get(&a.v[rd as usize], d, i)).collect();
                        let sb = 1u64 << (fmt(d).0 + fmt(d).1);
                        let inf = mk(d, false, (1 << fmt(d).0) - 1, 0);
                        rap[0] += 1;
                        rap[1] += res.iter().any(|&x| x & !sb == 0) as u64;
                        rap[2] += res.iter().any(|&x| x & !sb == inf) as u64;
                        rap[3] += (nuevas & 4 != 0) as u64;
                        rap[4] += (nuevas & 16 != 0) as u64;
                    }
                    for (j, bit) in [0u64, 1, 2, 3, 4, 7].iter().enumerate() {
                        if nuevas & (1 << bit) != 0 {
                            flags[j] += 1;
                        }
                    }
                    if a.v != b.v || a.fpsr != b.fpsr {
                        diffs += 1;
                        if verbose && diffs <= 8 {
                            let rdv = rd as usize;
                            println!(
                                "DIF {} w={:#010x} fpcr={:#x} n={:016x}_{:016x} m={:016x}_{:016x} a={:016x}_{:016x} | interp d={:016x}_{:016x} fpsr={:#x} | jit d={:016x}_{:016x} fpsr={:#x}",
                                f.name, w, b.fpcr, v0[rn as usize][1], v0[rn as usize][0], v0[rm as usize][1], v0[rm as usize][0], v0[rdv][1], v0[rdv][0],
                                a.v[rdv][1], a.v[rdv][0], a.fpsr, b.v[rdv][1], b.v[rdv][0], b.fpsr
                            );
                        }
                    }
                }
                keep.push(code);
                // el SVC final del bloque tambien pasa por h_exec: una llamada por caso que no cuenta
                let h = (HELPER_CALLS.load(Relaxed) - h0).saturating_sub(n as u64);
                if verbose {
                    println!(
                        "DIF {:<28} casos {:>9} diferencias {} | al helper {:.1} % | ruta rapida {} (cero {} inf {} OFC {} IXC {}) | interprete con IOC {} DZC {} OFC {} UFC {} IXC {} IDC {}",
                        f.name, n, diffs, 100.0 * h as f64 / n as f64, rap[0], rap[1], rap[2], rap[3], rap[4], flags[0], flags[1], flags[2], flags[3], flags[4], flags[5]
                    );
                }
                total += diffs;
            }
            total
        }
    }

    /// Version acotada (segundos) de la prueba diferencial; la completa (2 M de casos por forma) va con `--ignored`.
    #[test]
    fn diferencial_by_elem_y_escalares() {
        assert_eq!(dif::run(20_000, 0x5eed_0001, true), 0, "el JIT difiere del interprete");
    }

    /// `cargo test --release --lib jitfp::tests::diferencial_completa -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn diferencial_completa() {
        assert_eq!(dif::run(2_000_000, 0x5eed_0002, true), 0, "el JIT difiere del interprete");
    }
}
