//! Pruebas de la cache de registros del JIT (paso 5, ver la cabecera de jit.rs, "Registros").
//!
//! * Diferencial: programas aleatorios de 1 a 12 instrucciones (enteras en linea, loads/stores a un bufer, saltos
//!   condicionales hacia delante dentro del programa, instrucciones que van al helper, FP/SIMD con registros base)
//!   ejecutados con el interprete de referencia (`interp::step`), con el JIT con cache (encadenado y sin encadenar),
//!   con el JIT sin cache (`regs = false`, el codigo de antes) y con `run_n` (modo contado). Se compara bit a bit:
//!   x0-x30, sp, pc, NZCV materializadas, v0-v31, FPSR, TPIDR y el bufer de memoria.
//! * Fallo sincrono: un load/store a una pagina PROT_NONE dentro del bloque; el manejador SIGSEGV copia el `Cpu` del
//!   hilo (lo que leen `fault_host`/`host_handler`/`build_frame`) y se compara con el interprete parado justo antes
//!   de la instruccion que falla. Luego el acceso se repite y el estado final se compara igual.
//! * Microbenchmark: bucles internos copiados del banco (mat4 y entero_saltos) con y sin cache.

use super::*;

pub(super) struct Rng(pub u64);
impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn coin(&mut self) -> bool {
        self.next() & 1 == 0
    }
}

const SVC: u32 = 0xD400_0001;
/// Bufer de datos de cada hilo y desplazamiento de las bases dentro de el.
const BUF: usize = 4096;
const MID: u64 = 2048;
/// Registros base de los accesos a memoria (no los escribe ninguna instruccion generada salvo el write-back) e
/// indices de la forma con registro (x23: 0..63; x22: negativo pequeno, para SXTW).
const BASES: [u32; 4] = [24, 25, 26, 31];
/// Registro base de la pagina que falla (prueba de fallo sincrono).
const FBASE: u32 = 27;

fn sfb(r: &mut Rng) -> u32 {
    r.coin() as u32
}
/// Registro de datos x0..x7 (los que se escriben).
fn dreg(r: &mut Rng) -> u32 {
    r.below(8) as u32
}
/// Registro fuente: x0..x7 o, a veces, 31 (XZR en estas formas).
fn src(r: &mut Rng) -> u32 {
    if r.below(10) == 0 {
        31
    } else {
        dreg(r)
    }
}
fn interesting(r: &mut Rng) -> u64 {
    match r.below(10) {
        0 => 0,
        1 => u64::MAX,
        2 => 1,
        3 => 0x8000_0000_0000_0000,
        4 => 0x7FFF_FFFF,
        5 => 0x8000_0000,
        6 => r.below(256),
        7 => 0xFFFF_FFFF,
        8 => (r.below(64) as i64 - 32) as u64,
        _ => r.next(),
    }
}

/// Instruccion entera "en linea" (sin memoria ni saltos).
fn gen_alu(r: &mut Rng) -> u32 {
    loop {
        let sf = sfb(r);
        let w = match r.below(16) {
            0 | 1 => {
                // ADD/SUB(S) inmediato
                let (op, s) = (sfb(r), (r.below(3) == 0) as u32);
                let sh = (r.below(4) == 0) as u32;
                let mut imm = r.below(4096) as u32;
                if r.coin() {
                    imm = r.below(8) as u32;
                }
                let (mut rd, mut rn) = (dreg(r), if r.below(6) == 0 { 31 } else { dreg(r) });
                let mut sf = sf;
                let mut sh = sh;
                if s == 1 && r.below(4) == 0 {
                    rd = 31; // CMP/CMN
                } else if s == 0 && r.below(16) == 0 {
                    // ADD/SUB SP, SP, #16*k (64 bits: la pila no sale del bufer)
                    rd = 31;
                    rn = 31;
                    sf = 1;
                    sh = 0;
                    imm = 16 * (1 + r.below(3) as u32);
                }
                (sf << 31) | (op << 30) | (s << 29) | (0b100010 << 23) | (sh << 22) | (imm << 10) | (rn << 5) | rd
            }
            2 | 3 => {
                // ADD/SUB(S) registro desplazado
                let (op, s) = (sfb(r), (r.below(3) == 0) as u32);
                let sh = r.below(3) as u32;
                let amt = r.below(if sf == 1 { 64 } else { 32 }) as u32;
                let amt = if r.coin() { 0 } else { amt };
                let rd = if s == 1 && r.below(4) == 0 { 31 } else { dreg(r) };
                (sf << 31) | (op << 30) | (s << 29) | (0b01011 << 24) | (sh << 22) | (src(r) << 16) | (amt << 10) | (src(r) << 5) | rd
            }
            4 => {
                // ADD/SUB(S) registro extendido (rn puede ser SP)
                let (op, s) = (sfb(r), (r.below(3) == 0) as u32);
                let opt = r.below(8) as u32;
                let amt = r.below(5) as u32;
                let rn = if r.below(6) == 0 { 31 } else { dreg(r) };
                let rd = if s == 1 && r.below(4) == 0 { 31 } else { dreg(r) };
                (sf << 31) | (op << 30) | (s << 29) | (0b01011001 << 21) | (src(r) << 16) | (opt << 13) | (amt << 10) | (rn << 5) | rd
            }
            5 => {
                // logica con inmediato
                let opc = r.below(4) as u32;
                let n = if sf == 1 { sfb(r) } else { 0 };
                let lim = if sf == 1 { 64 } else { 32 };
                let (immr, imms) = (r.below(lim) as u32, r.below(lim) as u32);
                let rd = if opc == 3 && r.below(4) == 0 { 31 } else { dreg(r) };
                (sf << 31) | (opc << 29) | (0b100100 << 23) | (n << 22) | (immr << 16) | (imms << 10) | (src(r) << 5) | rd
            }
            6 | 7 => {
                // logica con registro desplazado (AND/ORR/EOR/ANDS, con y sin negar)
                let opc = r.below(4) as u32;
                let n = sfb(r);
                let sh = r.below(4) as u32;
                let amt = if r.coin() { 0 } else { r.below(if sf == 1 { 64 } else { 32 }) as u32 };
                let rd = if opc == 3 && r.below(4) == 0 { 31 } else { dreg(r) };
                (sf << 31) | (opc << 29) | (0b01010 << 24) | (sh << 22) | (n << 21) | (src(r) << 16) | (amt << 10) | (src(r) << 5) | rd
            }
            8 => {
                // MOVN/MOVZ/MOVK
                let opc = [0u32, 2, 3][r.below(3) as usize];
                let hw = r.below(if sf == 1 { 4 } else { 2 }) as u32;
                let imm = (if r.coin() { r.below(16) } else { r.next() & 0xFFFF }) as u32;
                (sf << 31) | (opc << 29) | (0b100101 << 23) | (hw << 21) | (imm << 5) | dreg(r)
            }
            9 => {
                // SBFM/BFM/UBFM
                let opc = r.below(3) as u32;
                let lim = if sf == 1 { 64 } else { 32 };
                (sf << 31) | (opc << 29) | (0b100110 << 23) | (sf << 22) | ((r.below(lim) as u32) << 16) | ((r.below(lim) as u32) << 10) | (src(r) << 5) | dreg(r)
            }
            10 => {
                // EXTR
                let lim = if sf == 1 { 64 } else { 32 };
                (sf << 31) | (0b00100111 << 23) | (sf << 22) | (src(r) << 16) | ((r.below(lim) as u32) << 10) | (src(r) << 5) | dreg(r)
            }
            11 => {
                // CSEL/CSINC/CSINV/CSNEG
                let (op, o2) = (sfb(r), sfb(r));
                (sf << 31) | (op << 30) | (0b11010100 << 21) | (src(r) << 16) | ((r.below(16) as u32) << 12) | (o2 << 10) | (src(r) << 5) | dreg(r)
            }
            12 => {
                // MADD/MSUB, SMADDL/UMADDL/SMSUBL/UMSUBL, SMULH/UMULH
                match r.below(3) {
                    0 => (sf << 31) | (0b0011011000 << 21) | (src(r) << 16) | (sfb(r) << 15) | (src(r) << 10) | (src(r) << 5) | dreg(r),
                    1 => (1 << 31) | (0b0011011 << 24) | (sfb(r) << 23) | (0b01 << 21) | (src(r) << 16) | (sfb(r) << 15) | (src(r) << 10) | (src(r) << 5) | dreg(r),
                    _ => (0b10011011 << 24) | (sfb(r) << 23) | (0b10 << 21) | (src(r) << 16) | (0b11111 << 10) | (src(r) << 5) | dreg(r),
                }
            }
            13 => {
                // LSLV/LSRV/ASRV/RORV y REV
                if r.below(3) == 0 {
                    (if sf == 1 { 0xDAC0_0C00 } else { 0x5AC0_0800 }) | (src(r) << 5) | dreg(r)
                } else {
                    (sf << 31) | (0b0011010110 << 21) | (src(r) << 16) | (0b0010 << 12) | ((r.below(4) as u32) << 10) | (src(r) << 5) | dreg(r)
                }
            }
            14 => {
                // ADR/ADRP, MRS/MSR TPIDR_EL0, NOP
                match r.below(4) {
                    0 => ((r.below(2) as u32) << 31) | 0x1000_0000 | ((r.below(4) as u32) << 29) | ((r.below(64) as u32) << 5) | dreg(r),
                    1 => 0xD53B_D040 | dreg(r),
                    2 => 0xD51B_D040 | src(r),
                    _ => 0xD503_201F,
                }
            }
            _ => gen_helper(r),
        };
        if !matches!(decode(w), Op::Undef(_)) {
            return w;
        }
    }
}

/// Instrucciones enteras que el JIT ejecuta con el interprete (h_exec): pueden escribir cualquier registro.
fn gen_helper(r: &mut Rng) -> u32 {
    let sf = sfb(r);
    match r.below(5) {
        // CFINV/XAFLAG/AXFLAG, RMIF, SETF8/16 (FlagM): leen y escriben NZCV en el helper
        4 => match r.below(3) {
            0 => 0xD500_401F | (r.below(3) as u32) << 5,
            1 => 0xBA00_0400 | ((r.below(64) as u32) << 15) | (src(r) << 5) | (r.below(16) as u32),
            _ => 0x3A00_080D | (sfb(r) << 14) | (src(r) << 5),
        },
        // UDIV/SDIV
        0 => (sf << 31) | 0x1AC0_0800 | (src(r) << 16) | ((r.below(2) as u32) << 10) | (src(r) << 5) | dreg(r),
        // ADC/ADCS/SBC/SBCS
        1 => (sf << 31) | (sfb(r) << 30) | (sfb(r) << 29) | 0x1A00_0000 | (src(r) << 16) | (src(r) << 5) | dreg(r),
        // CCMP/CCMN (registro o inmediato)
        2 => (sf << 31) | (sfb(r) << 30) | (1 << 29) | (0b11010010 << 21) | ((r.below(32) as u32) << 16) | ((r.below(16) as u32) << 12) | (sfb(r) << 11) | (src(r) << 5) | (r.below(16) as u32),
        // CLZ/CLS
        _ => (if sf == 1 { 0xDAC0_1000 } else { 0x5AC0_1000 }) | ((r.below(2) as u32) << 10) | (src(r) << 5) | dreg(r),
    }
}

/// Load/store entero al bufer (bases x24-x26 y SP; desplazamientos pequenos).
fn gen_mem(r: &mut Rng, rn: u32) -> u32 {
    loop {
        let size = r.below(4) as u32;
        let load = r.coin();
        // opc: 0 STR, 1 LDR, 2 LDRS a 64, 3 LDRS a 32
        let opc = if !load {
            0
        } else if size < 3 && r.below(3) == 0 {
            if size < 2 && r.coin() {
                3
            } else {
                2
            }
        } else {
            1
        };
        let rt = if load { dreg(r) } else { src(r) };
        let w = match r.below(5) {
            0 => (size << 30) | 0x3900_0000 | (opc << 22) | ((r.below(16) as u32) << 10) | (rn << 5) | rt,
            1 => {
                // LDUR/STUR, post-indice y pre-indice
                let ty = [0u32, 1, 3][r.below(3) as usize];
                let imm9 = (r.below(128) as i32 - 64) as u32 & 0x1FF;
                (size << 30) | 0x3800_0000 | (opc << 22) | (imm9 << 12) | (ty << 10) | (rn << 5) | rt
            }
            2 => {
                // desplazamiento con registro: LSL/UXTX (x23), UXTW (w23), SXTW (w22)
                let (opt, rm) = [(3u32, 23u32), (2, 23), (6, 22)][r.below(3) as usize];
                (size << 30) | 0x3820_0800 | (opc << 22) | (rm << 16) | (opt << 13) | (sfb(r) << 12) | (rn << 5) | rt
            }
            3 => {
                // LDP/STP/LDPSW (desplazamiento, pre y post)
                let l = load as u32;
                let opc2 = if l == 1 && r.below(4) == 0 { 1 } else { [0u32, 2][r.below(2) as usize] };
                let ty = 1 + r.below(3) as u32;
                let imm7 = (r.below(16) as i32 - 8) as u32 & 0x7F;
                let rt = if l == 1 { dreg(r) } else { src(r) };
                let mut rt2 = if l == 1 { dreg(r) } else { src(r) };
                if l == 1 && rt2 == rt {
                    rt2 = (rt + 1) & 7;
                }
                (opc2 << 30) | 0x2800_0000 | (ty << 23) | (l << 22) | (imm7 << 15) | (rt2 << 10) | (rn << 5) | rt
            }
            // LDARB: las bases se mueven con pre/post-indice y un LDAR mayor desalineado es SIGBUS (lo cubre
            // `desalineado_es_sigbus_en_el_pc_exacto`)
            _ => 0x08DF_FC00 | (rn << 5) | dreg(r),
        };
        if !matches!(decode(w), Op::Undef(_) | Op::Prefetch) {
            return w;
        }
    }
}

/// FP/SIMD: movimientos entre bancos (escriben x en jitfp), aritmetica escalar y loads/stores con registro base.
fn gen_fp(r: &mut Rng, rn: u32) -> u32 {
    let v = |r: &mut Rng| r.below(4) as u32;
    match r.below(11) {
        0 => 0x9E66_0000 | (v(r) << 5) | dreg(r),      // FMOV Xd, Dn
        1 => 0x9E67_0000 | (src(r) << 5) | v(r),       // FMOV Dd, Xn
        2 => 0x9E62_0000 | (src(r) << 5) | v(r),       // SCVTF Dd, Xn
        3 => 0x9E78_0000 | (v(r) << 5) | dreg(r),      // FCVTZS Xd, Dn
        4 => 0x1E60_2800 | (v(r) << 16) | (v(r) << 5) | v(r), // FADD Dd, Dn, Dm
        5 => 0x3DC0_0000 | ((r.below(4) as u32) << 10) | (rn << 5) | v(r), // LDR Qt, [xn, #16k]
        6 => 0x3D80_0000 | ((r.below(4) as u32) << 10) | (rn << 5) | v(r), // STR Qt, [xn, #16k]
        7 => 0xFC40_0400 | (([0xFF0u32, 0xFF8, 8, 16][r.below(4) as usize] & 0x1FF) << 12) | (rn << 5) | v(r), // LDR Dt, [xn], #imm
        8 => {
            if r.coin() {
                0x4CDF_7000 | (rn << 5) | v(r) // LD1 {vt.16b}, [xn], #16
            } else {
                0x4CC0_7000 | (23 << 16) | (rn << 5) | v(r) // LD1 {vt.16b}, [xn], x23
            }
        }
        9 => 0x4C00_7000 | (rn << 5) | v(r), // ST1 {vt.16b}, [xn]
        _ => {
            if r.coin() {
                0x3CE0_6800 | (23 << 16) | (rn << 5) | v(r) // LDR Qt, [xn, x23]
            } else {
                0x3CA0_6800 | (23 << 16) | (rn << 5) | v(r) // STR Qt, [xn, x23]
            }
        }
    }
}

/// Salto hacia delante desde `i` hasta `j` (j > i, j <= indice del SVC).
fn gen_branch(r: &mut Rng, i: usize, j: usize) -> u32 {
    let d = (j - i) as u32;
    match r.below(4) {
        0 => 0x5400_0000 | ((d & 0x7FFFF) << 5) | (r.below(15) as u32), // B.cond
        1 => (sfb(r) << 31) | 0x3400_0000 | (sfb(r) << 24) | ((d & 0x7FFFF) << 5) | dreg(r), // CBZ/CBNZ
        2 => {
            let b = r.below(64) as u32;
            ((b >> 5) << 31) | 0x3600_0000 | (sfb(r) << 24) | ((b & 31) << 19) | ((d & 0x3FFF) << 5) | dreg(r) // TBZ/TBNZ
        }
        _ => 0x1400_0000 | (d & 0x03FF_FFFF), // B
    }
}

/// Programa aleatorio de `n` instrucciones mas el SVC final.
pub(super) fn gen_prog(r: &mut Rng, n: usize) -> Vec<u32> {
    let mut p = Vec::with_capacity(n + 1);
    for i in 0..n {
        let base = BASES[r.below(4) as usize];
        let w = match r.below(100) {
            0..=54 => gen_alu(r),
            55..=74 => gen_mem(r, base),
            75..=84 => {
                let j = i + 1 + r.below((n - i) as u64) as usize;
                gen_branch(r, i, j)
            }
            85..=90 => gen_helper(r),
            _ => gen_fp(r, base),
        };
        p.push(w);
    }
    p.push(SVC);
    p
}

/// Estado comparado.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct St {
    pub x: [u64; 32],
    pub pc: u64,
    pub nzcv: u64,
    pub v: [[u64; 2]; 32],
    pub fpsr: u64,
    pub tpidr: u64,
    pub mem: Vec<u8>,
}

fn snap(c: &Cpu, mem: &[u8]) -> St {
    St { x: c.x, pc: c.pc, nzcv: c.flags(), v: c.v, fpsr: c.fpsr, tpidr: c.tpidr, mem: mem.to_vec() }
}

/// Estado inicial aleatorio (se aplica igual a cada variante).
#[derive(Clone)]
pub(super) struct Init {
    x: [u64; 32],
    nzcv: u64,
    lf: (u64, u64, u64),
    v: [[u64; 2]; 32],
    fpsr: u64,
    tpidr: u64,
}

fn gen_init(r: &mut Rng, base: u64) -> Init {
    let mut x = [0u64; 32];
    for (i, xi) in x.iter_mut().enumerate() {
        *xi = interesting(r);
        let _ = i;
    }
    x[22] = (-(1 + r.below(64) as i64)) as u64;
    x[23] = r.below(64);
    for b in [24usize, 25, 26] {
        x[b] = base + MID + 8 * r.below(16) - 64;
    }
    x[31] = base + MID;
    let mut v = [[0u64; 2]; 32];
    for (i, vi) in v.iter_mut().enumerate() {
        let d = |r: &mut Rng| match r.below(6) {
            0 => (r.below(2000) as f64 - 1000.0).to_bits(),
            1 => f64::NAN.to_bits(),
            2 => (r.below(1 << 20) as f64 * 1e10).to_bits(),
            3 => 0,
            _ => r.next(),
        };
        *vi = [d(r), d(r)];
        let _ = i;
    }
    let lf = if r.coin() { (0, 0, 0) } else { (1 + r.below(6), interesting(r), interesting(r)) };
    Init { x, nzcv: (r.below(16)) << 28, lf, v, fpsr: if r.below(4) == 0 { r.next() & 0x9F } else { 0 }, tpidr: r.next() }
}

fn apply(c: &mut Cpu, s: &Init, pc: u64) {
    c.x = s.x;
    c.set_flags(s.nzcv);
    if s.lf.0 != 0 {
        c.lf_kind = s.lf.0;
        c.lf_a = s.lf.1;
        c.lf_b = s.lf.2;
    }
    c.v = s.v;
    c.fpsr = s.fpsr;
    c.fpcr = 0;
    c.tpidr = s.tpidr;
    c.pc = pc;
}

/// Interprete hasta el SVC final (sin ejecutarlo). Devuelve las instrucciones ejecutadas.
fn run_interp(c: &mut Cpu, svc: u64) -> usize {
    let mut n = 0;
    while c.pc != svc {
        match interp::step(c) {
            Flow::Next => {}
            f => panic!("flujo inesperado del interprete {:?} en {:#x}", f, c.pc),
        }
        n += 1;
        assert!(n < 1000);
    }
    n
}

/// Resultado de un programa en todas las variantes: None si todas coinciden, o una descripcion de la primera diferencia.
pub(super) struct Dif {
    /// bufer de memoria del hilo (las bases lo apuntan)
    mem: Vec<u8>,
    tmpl: Vec<u8>,
    jits: [Jit; 4],
    pub blocks: u64,
}

impl Dif {
    pub fn new(r: &mut Rng) -> Dif {
        let mut tmpl = vec![0u8; BUF];
        for b in tmpl.iter_mut() {
            *b = r.next() as u8;
        }
        let mk = |regs: bool, chain: bool| {
            let mut j = Jit::new();
            j.regs = regs;
            j.chain = chain;
            j
        };
        Dif { mem: vec![0u8; BUF], tmpl, jits: [mk(true, true), mk(true, false), mk(false, true), mk(true, true)], blocks: 0 }
    }
    pub fn flush(&mut self) {
        for j in self.jits.iter_mut() {
            j.flush_all();
        }
    }
    /// Ejecuta `code` (ya en su direccion definitiva) en las variantes y compara.
    pub fn check(&mut self, code: &[u32], init: &Init) -> Option<String> {
        let start = code.as_ptr() as u64;
        let svc = start + 4 * (code.len() as u64 - 1);
        // referencia: interprete
        self.mem.copy_from_slice(&self.tmpl);
        let mut c = Cpu::new();
        apply(&mut c, init, start);
        let n = run_interp(&mut c, svc);
        c.pc = svc + 4; // el SVC no tiene otro efecto
        let refst = snap(&c, &self.mem);
        let names = ["jit cache+encadenado", "jit cache sin encadenar", "jit SIN cache", "run_n con cache"];
        for k in 0..4 {
            self.mem.copy_from_slice(&self.tmpl);
            let mut c = Cpu::new();
            apply(&mut c, init, start);
            let c0 = self.jits[k].compiled;
            if k == 3 {
                if n > 0 {
                    self.jits[k].run_n(&mut c, n).unwrap();
                }
                c.pc += 4;
            } else {
                let ev = self.jits[k].run(&mut c);
                if ev != Event::Svc(0) {
                    return Some(format!("{}: evento {:?}", names[k], ev));
                }
            }
            if k == 0 {
                self.blocks += self.jits[k].compiled - c0;
            }
            let st = snap(&c, &self.mem);
            if st != refst {
                return Some(describe(names[k], &refst, &st));
            }
        }
        None
    }
}

fn describe(name: &str, a: &St, b: &St) -> String {
    let mut s = format!("{} difiere del interprete:", name);
    for i in 0..32 {
        if a.x[i] != b.x[i] {
            s += &format!(" x{}: {:#x} != {:#x};", i, a.x[i], b.x[i]);
        }
    }
    if a.pc != b.pc {
        s += &format!(" pc {:#x} != {:#x};", a.pc, b.pc);
    }
    if a.nzcv != b.nzcv {
        s += &format!(" nzcv {:#x} != {:#x};", a.nzcv, b.nzcv);
    }
    for i in 0..32 {
        if a.v[i] != b.v[i] {
            s += &format!(" v{}: {:x?} != {:x?};", i, a.v[i], b.v[i]);
        }
    }
    if a.fpsr != b.fpsr {
        s += &format!(" fpsr {:#x} != {:#x};", a.fpsr, b.fpsr);
    }
    if a.tpidr != b.tpidr {
        s += " tpidr;";
    }
    if a.mem != b.mem {
        let i = (0..a.mem.len()).find(|&i| a.mem[i] != b.mem[i]).unwrap();
        s += &format!(" memoria desde +{};", i);
    }
    s
}

/// Ejecuta `progs` programas aleatorios en un hilo. Devuelve (diferencias, bloques traducidos con cache).
pub(super) fn run_dif(progs: usize, seed: u64, verbose: bool) -> (u64, u64) {
    let mut r = Rng(seed | 1);
    let mut d = Dif::new(&mut r);
    // los programas van en direcciones distintas (la cache del Jit va por pc); al dar la vuelta se vacia todo
    const STRIDE: usize = 16;
    const SLOTS: usize = 1 << 16;
    let mut arena = vec![0u32; STRIDE * SLOTS + 16];
    let a0 = ((arena.as_ptr() as usize + 63) & !63) - arena.as_ptr() as usize;
    let a0 = a0 / 4;
    let mut diffs = 0u64;
    for p in 0..progs {
        let s = p % SLOTS;
        if s == 0 && p > 0 {
            d.flush();
        }
        let n = 1 + r.below(12) as usize;
        let prog = gen_prog(&mut r, n);
        let off = a0 + s * STRIDE;
        arena[off..off + prog.len()].copy_from_slice(&prog);
        let base = d.mem.as_ptr() as u64;
        let init = gen_init(&mut r, base);
        let code = &arena[off..off + prog.len()];
        if let Some(e) = d.check(code, &init) {
            diffs += 1;
            if verbose && diffs <= 10 {
                let ws: Vec<String> = prog.iter().map(|w| format!("{:#010x}", w)).collect();
                println!("DIF programa [{}]: {}", ws.join(", "), e);
            }
        }
    }
    (diffs, d.blocks)
}

/// Version acotada (segundos): 30 000 programas.
#[test]
fn diferencial_registros() {
    let (d, b) = run_dif(30_000, 0x5eed_5001, true);
    println!("DIF registros: 30000 programas, {} bloques traducidos (con cache), {} diferencias", b, d);
    assert_eq!(d, 0);
}

/// Completa: 8 hilos x 300 000 programas (2,4 M de programas):
/// `cargo test --release --lib jit::regs_tests::diferencial_registros_completa -- --ignored --nocapture --test-threads=1`
#[test]
#[ignore]
fn diferencial_registros_completa() {
    const T: u64 = 8;
    const P: usize = 300_000;
    let res: Vec<(u64, u64)> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..T).map(|t| s.spawn(move || run_dif(P, 0x5eed_5100 + 7919 * t, true))).collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let d: u64 = res.iter().map(|x| x.0).sum();
    let b: u64 = res.iter().map(|x| x.1).sum();
    println!("DIF registros completa: {} programas, {} bloques traducidos (con cache), {} diferencias", T as usize * P, b, d);
    assert_eq!(d, 0);
}

// ---------------------------------------------------------------------------------------------
// Fallo sincrono
// ---------------------------------------------------------------------------------------------

#[repr(C)]
struct SigAct {
    handler: usize,
    mask: [u64; 16],
    flags: i32,
    restorer: usize,
}
extern "C" {
    fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
}

static F_CPU: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static F_PAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static F_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Jit del caso en curso y pc guest exacto que calcula el manejador con `Jit::fault_pc` (u64::MAX: no lo encontro)
static F_JIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static F_EXACT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static F_FPSR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Copia del Cpu tomada en el manejador (solo un hilo usa esto: la prueba es secuencial).
static mut F_SNAP: Option<(St, u64)> = None;

/// Lo que hacen `fault_host`/`native_bridge_signal` antes de entregar la senal al guest: `fault_in_store` y leer el
/// Cpu del hilo (aqui se copia en lugar de construir el marco). Despues desprotege la pagina para que el acceso se
/// repita al volver.
extern "C" fn segv(_sig: i32, _info: *mut u8, uc: *mut std::ffi::c_void) {
    unsafe {
        crate::monitor::fault_in_store(uc);
        let c = &*(F_CPU.load(std::sync::atomic::Ordering::SeqCst) as *const Cpu);
        *std::ptr::addr_of_mut!(F_SNAP) = Some((snap(c, &[]), c.pc));
        // pc exacto de la instruccion que fallo (lo que ve un manejador guest en su ucontext, sig::sync_fault)
        let j = &*(F_JIT.load(std::sync::atomic::Ordering::SeqCst) as *const Jit);
        let rip = *((uc as *const u8).add(40 + 8 * 16) as *const u64);
        F_EXACT.store(j.fault_pc(c.host_sp, rip).unwrap_or(u64::MAX), std::sync::atomic::Ordering::SeqCst);
        crate::sys::syscall(10, F_PAGE.load(std::sync::atomic::Ordering::SeqCst) as i64, 4096i64, 3i64); // RW
    }
    F_HITS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Instruccion que accede a la pagina que falla (base x27). Con pre-indice tambien: la base se escribe despues del
/// acceso (si falla, el manejador la ve sin cambiar y al repetir no se suma dos veces).
fn gen_fault(r: &mut Rng) -> u32 {
    let rn = FBASE;
    loop {
        let w = match r.below(14) {
            9 => (r.below(4) as u32) << 30 | 0x3800_0C00 | ((r.below(64) as u32) << 12) | (rn << 5) | src(r), // STR pre
            10 => (r.below(4) as u32) << 30 | 0x3840_0C00 | ((r.below(64) as u32) << 12) | (rn << 5) | dreg(r), // LDR pre
            11 => 0xA980_0000 | ((r.below(8) as u32) << 15) | (src(r) << 10) | (rn << 5) | src(r), // STP x pre
            12 => {
                let rt = dreg(r);
                0xA9C0_0000 | ((r.below(8) as u32) << 15) | (((rt + 1) & 7) << 10) | (rn << 5) | rt // LDP x pre
            }
            13 => 0x3C80_0C00 | ((r.below(64) as u32) << 12) | (rn << 5) | r.below(4) as u32, // STR q pre
            0 => (r.below(4) as u32) << 30 | 0x3900_0000 | ((r.below(16) as u32) << 10) | (rn << 5) | src(r), // STR
            1 => (r.below(4) as u32) << 30 | 0x3940_0000 | ((r.below(16) as u32) << 10) | (rn << 5) | dreg(r), // LDR
            2 => (r.below(4) as u32) << 30 | 0x3800_0400 | ((r.below(64) as u32) << 12) | (rn << 5) | src(r), // STR post
            3 => (r.below(4) as u32) << 30 | 0x3840_0400 | ((r.below(64) as u32) << 12) | (rn << 5) | dreg(r), // LDR post
            4 => 0xA900_0000 | ((r.below(8) as u32) << 15) | (src(r) << 10) | (rn << 5) | src(r), // STP x
            5 => {
                let rt = dreg(r);
                0xA940_0000 | ((r.below(8) as u32) << 15) | (((rt + 1) & 7) << 10) | (rn << 5) | rt // LDP x
            }
            6 => 0x3D80_0000 | ((r.below(4) as u32) << 10) | (rn << 5) | r.below(4) as u32, // STR q
            7 => 0x3DC0_0000 | ((r.below(4) as u32) << 10) | (rn << 5) | r.below(4) as u32, // LDR q
            _ => 0x4CDF_7000 | (rn << 5) | r.below(4) as u32, // LD1 post
        };
        if !matches!(decode(w), Op::Undef(_) | Op::Prefetch) {
            return w;
        }
    }
}

/// Un hilo: `cases` bloques [prefijo de 0..8 instrucciones enteras/helper/FP-registro][acceso que falla][0..3 mas][SVC].
fn run_fault_cases(cases: usize, seed: u64, regs: bool) -> (u64, u64) {
    let mut r = Rng(seed | 1);
    let mut d = Dif::new(&mut r);
    let pages = vec![0u8; 3 * 4096];
    let page = (pages.as_ptr() as usize + 4095) & !4095;
    F_PAGE.store(page, std::sync::atomic::Ordering::SeqCst);
    let act = SigAct { handler: segv as usize, mask: [0; 16], flags: 4 | 0x4000_0000 /* SA_SIGINFO | SA_NODEFER */, restorer: 0 };
    let mut old = SigAct { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
    assert_eq!(unsafe { sigaction(11, &act, &mut old) }, 0);
    // hilo guest: Cpu con la marca del monitor (en modo fence, store rapido y fault_in_store)
    let t = crate::rt::ensure_thread(256 << 10);
    let monflag = unsafe { &(*t).mon_flag as *const _ as usize };
    let mut jit = Jit::new();
    jit.regs = regs;
    let mut code = vec![0u32; 64];
    let mut fails = 0u64;
    let mut checked = 0u64;
    for k in 0..cases {
        // cada caso en su propia direccion: vaciar el Jit cada vez es lo simple (el coste no importa aqui)
        jit.flush_all();
        let pre = r.below(9) as usize;
        let post = r.below(4) as usize;
        let mut prog = Vec::new();
        for _ in 0..pre {
            prog.push(match r.below(6) {
                0 => gen_helper(&mut r),
                1 => gen_fp(&mut r, 0), // los loads/stores se sustituyen abajo
                _ => gen_alu(&mut r),
            });
        }
        // sin loads/stores de FP en el prefijo (gen_fp con base 0 seria x0): solo las formas sin memoria
        for w in prog.iter_mut() {
            if (*w >> 25) & 0b101 == 0b100 {
                *w = gen_alu(&mut r);
            }
        }
        let fi = prog.len();
        prog.push(gen_fault(&mut r));
        for _ in 0..post {
            prog.push(gen_alu(&mut r));
        }
        prog.push(SVC);
        code[..prog.len()].copy_from_slice(&prog);
        let start = code.as_ptr() as u64;
        let svc = start + 4 * (prog.len() as u64 - 1);
        let mut init = gen_init(&mut r, d.mem.as_ptr() as u64);
        init.x[FBASE as usize] = page as u64 + 64 * r.below(32);
        // referencia: interprete hasta la instruccion que falla (pagina accesible) y hasta el final
        unsafe { crate::sys::syscall(10, page as i64, 4096i64, 3i64) };
        unsafe { std::ptr::write_bytes(page as *mut u8, 0x5A, 4096) };
        d.mem.copy_from_slice(&d.tmpl);
        let mut c = Cpu::new();
        apply(&mut c, &init, start);
        let mut before = None;
        let mut steps = 0;
        while c.pc != svc {
            if c.pc == start + 4 * fi as u64 {
                before = Some(snap(&c, &[]));
            }
            assert!(matches!(interp::step(&mut c), Flow::Next));
            steps += 1;
            assert!(steps < 100);
        }
        let Some(before) = before else { continue }; // un salto del prefijo... no hay saltos: siempre se llega
        c.pc = svc + 4;
        let fin_ref = snap(&c, &d.mem);
        let page_ref = unsafe { std::slice::from_raw_parts(page as *const u8, 4096).to_vec() };
        // JIT con la pagina protegida
        unsafe { std::ptr::write_bytes(page as *mut u8, 0x5A, 4096) };
        d.mem.copy_from_slice(&d.tmpl);
        let mut c = Cpu::new();
        apply(&mut c, &init, start);
        c.monflag = monflag;
        F_CPU.store(&*c as *const Cpu as usize, std::sync::atomic::Ordering::SeqCst);
        F_JIT.store(&jit as *const Jit as usize, std::sync::atomic::Ordering::SeqCst);
        F_EXACT.store(0, std::sync::atomic::Ordering::SeqCst);
        unsafe { *std::ptr::addr_of_mut!(F_SNAP) = None };
        assert_eq!(unsafe { crate::sys::syscall(10, page as i64, 4096i64, 0i64) }, 0); // PROT_NONE
        let h0 = F_HITS.load(std::sync::atomic::Ordering::SeqCst);
        let ev = jit.run(&mut c);
        assert_eq!(ev, Event::Svc(0));
        assert_eq!(F_HITS.load(std::sync::atomic::Ordering::SeqCst), h0 + 1, "el acceso no fallo");
        let (seen, seen_pc) = unsafe { (*std::ptr::addr_of!(F_SNAP)).clone().unwrap() };
        // pc del Cpu en el fallo: inicio del bloque o el de la ultima instruccion que fue al helper (documentado)
        let mut want = before.clone();
        want.pc = seen_pc;
        // FPSR: las flags acumuladas de las rutas rapidas FP viven en MXCSR hasta fold_mxcsr (al salir de Jit::run);
        // limitacion previa, independiente de la cache (se cuenta aparte y se comprueba en el estado final)
        if seen.fpsr != want.fpsr {
            F_FPSR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            want.fpsr = seen.fpsr;
        }
        let fin = snap(&c, &d.mem);
        let page_now = unsafe { std::slice::from_raw_parts(page as *const u8, 4096).to_vec() };
        checked += 1;
        let exact = F_EXACT.load(std::sync::atomic::Ordering::SeqCst);
        let exact_ok = exact == start + 4 * fi as u64;
        if seen != want || fin != fin_ref || page_now != page_ref || !exact_ok {
            fails += 1;
            if fails <= 10 {
                let ws: Vec<String> = prog.iter().map(|w| format!("{:#010x}", w)).collect();
                let e = if !exact_ok {
                    format!("pc exacto {:#x}, esperado {:#x}", exact, start + 4 * fi as u64)
                } else if seen != want {
                    describe("Cpu en el manejador", &want, &seen)
                } else if fin != fin_ref {
                    describe("estado final", &fin_ref, &fin)
                } else {
                    "pagina".into()
                };
                println!("FALLO caso {} [{}] (instr {}): {}", k, ws.join(", "), fi, e);
            }
        }
    }
    unsafe { sigaction(11, &old, std::ptr::null_mut()) };
    (fails, checked)
}

fn fault_test(regs: bool) {
    let f0 = F_FPSR.load(std::sync::atomic::Ordering::Relaxed);
    let (f, n) = run_fault_cases(3000, 0xfa17_0001 + regs as u64, true);
    let fp = F_FPSR.load(std::sync::atomic::Ordering::Relaxed) - f0;
    println!("FALLO sincrono (regs={}, monitor={}): {} casos, {} diferencias (FPSR aun en MXCSR en el fallo: {})", regs, crate::monitor::mode_name(), n, f, fp);
    assert!(n > 2000);
    assert_eq!(f, 0);
}

/// Fallo sincrono con cache y sin ella, en el modo del monitor del proceso y en `fence` y `bloqueo`. Siempre en un
/// proceso hijo: `run_fault_cases` sustituye el manejador de SIGSEGV de todo el proceso, y una prueba en paralelo que
/// dependa del de heddle (redireccion de ejecucion) fallaria.
#[test]
fn fallo_sincrono_ve_el_cpu_completo() {
    if std::env::var_os("HEDDLE_FALLO_HIJO").is_some() {
        fault_test(true);
        fault_test(false);
        return;
    }
    let modos: &[Option<&str>] = if std::env::var_os("HEDDLE_MONITOR").is_some() { &[None] } else { &[None, Some("fence"), Some("bloqueo")] };
    for m in modos {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["jit::regs_tests::fallo_sincrono_ve_el_cpu_completo", "--exact", "--test-threads=1", "--nocapture"]);
        cmd.env("HEDDLE_FALLO_HIJO", "1");
        if let Some(m) = m {
            cmd.env("HEDDLE_MONITOR", m);
        }
        let out = cmd.output().unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        print!("{}", so);
        assert!(out.status.success() && so.contains("1 passed"), "modo {:?}:\n{}\n{}", m, so, String::from_utf8_lossy(&out.stderr));
    }
}

// ---------------------------------------------------------------------------------------------
// Falta de alineacion: SIGBUS BUS_ADRALN en el pc exacto
// ---------------------------------------------------------------------------------------------

static A_CPU: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static A_JIT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// (senal, si_code, si_addr, pc exacto, x en el manejador) del ultimo SIGBUS (prueba secuencial)
static mut A_SEEN: Option<(i32, i32, u64, u64, [u64; 32])> = None;

/// Manejador de la prueba: anota lo que veria el manejador guest y reanuda tras la instruccion (como `sync_fault` +
/// `resume_at` con un ucontext cuyo pc se avanzo).
extern "C" fn sigbus(sig: i32, info: *mut u8, uc: *mut std::ffi::c_void) {
    use std::sync::atomic::Ordering::SeqCst;
    unsafe {
        let c = &mut *(A_CPU.load(SeqCst) as *mut Cpu);
        let g = (uc as *mut u8).add(40) as *mut u64;
        let j = A_JIT.load(SeqCst);
        let pc = if j != 0 { (*(j as *const Jit)).fault_pc(c.host_sp, *g.add(16)).unwrap_or(u64::MAX) } else { c.pc };
        *std::ptr::addr_of_mut!(A_SEEN) = Some((sig, *(info.add(8) as *const i32), *(info.add(16) as *const u64), pc, c.x));
        c.pc = pc.wrapping_add(4);
        *g.add(15) = c.host_sp;
        if j != 0 {
            *g.add(16) = heddle_jit_abort as *const () as u64;
        } else {
            *g.add(16) = heddle_block_abort as *const () as u64;
            *g.add(11) = c as *mut Cpu as u64;
        }
    }
}

extern "C" fn step_one(cp: *mut Cpu) -> u64 {
    assert!(matches!(interp::step(unsafe { &mut *cp }), Flow::Next));
    1
}

/// Exclusivos, atomicos y LDAR/STLR desalineados (sin FEAT_LSE2): SIGBUS BUS_ADRALN con si_addr = la direccion, en el
/// pc de la instruccion y sin haber cambiado nada (Arm ARM, AArch64.CheckAlignment; en los exclusivos antes de mirar
/// el monitor, AArch64.ExclusiveMonitorsPass). Con el JIT (LDAR en linea y el resto por el helper) y el interprete.
///
/// En un proceso hijo: instala un manejador de SIGBUS del proceso, que otras pruebas en paralelo cambian (sigaction
/// del guest, la accion de debuggerd); con `cargo test` de muchos hilos el proceso moria por SIGBUS al azar.
#[test]
fn desalineado_es_sigbus_en_el_pc_exacto() {
    if std::env::var_os("HEDDLE_DESALINEADO_HIJO").is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["jit::regs_tests::desalineado_es_sigbus_en_el_pc_exacto", "--exact", "--test-threads=1", "--nocapture"])
            .env("HEDDLE_DESALINEADO_HIJO", "1")
            .output()
            .unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && so.contains("1 passed"), "{}\n{}", so, String::from_utf8_lossy(&out.stderr));
        return;
    }
    use std::sync::atomic::Ordering::SeqCst;
    let act = SigAct { handler: sigbus as usize, mask: [0; 16], flags: 4 | 0x4000_0000, restorer: 0 };
    let mut old = SigAct { handler: 0, mask: [0; 16], flags: 0, restorer: 0 };
    assert_eq!(unsafe { sigaction(7, &act, &mut old) }, 0);
    let buf = vec![0u64; 64];
    let base = (buf.as_ptr() as u64 + 63) & !63;
    // (palabra, alineacion exigida): x27 = base, x1/x2 datos, w3 estado
    let casos: [(u32, u64); 12] = [
        (0xC85F_7F61, 8),  // ldxr x1, [x27]
        (0x885F_7F61, 4),  // ldxr w1, [x27]
        (0x485F_7F61, 2),  // ldxrh w1, [x27]
        (0xC803_7F61, 8),  // stxr w3, x1, [x27]
        (0xC87F_0B61, 16), // ldxp x1, x2, [x27]
        (0xC8DF_FF61, 8),  // ldar x1, [x27]
        (0x48DF_FF61, 2),  // ldarh w1, [x27]
        (0xC89F_FF61, 8),  // stlr x1, [x27]
        (0xF821_0361, 8),  // ldadd x1, x1, [x27]
        (0xC8A1_7F62, 8),  // cas x1, x2, [x27]
        (0x4820_7F62, 16), // casp x0, x1, x2, x3, [x27]
        (0xF8BF_C361, 8),  // ldapr x1, [x27]
    ];
    for jit_mode in [true, false] {
        let mut jit = Jit::new();
        for (w, al) in casos {
            for off in [1u64, al / 2, 0] {
                let code = [w, SVC];
                let a = base + 8 + off;
                let mut c = Cpu::new();
                c.x[27] = a;
                c.x[1] = 0x1111;
                c.x[2] = 0x2222;
                c.x[3] = 0x3333;
                c.pc = code.as_ptr() as u64;
                let before = c.x;
                unsafe { *std::ptr::addr_of_mut!(A_SEEN) = None };
                A_CPU.store(&mut *c as *mut Cpu as usize, SeqCst);
                if jit_mode {
                    jit.flush_all();
                    A_JIT.store(&jit as *const Jit as usize, SeqCst);
                    assert_eq!(jit.run(&mut c), Event::Svc(0));
                } else {
                    A_JIT.store(0, SeqCst);
                    unsafe { heddle_call_block(&mut *c, step_one) };
                }
                let seen = unsafe { (*std::ptr::addr_of_mut!(A_SEEN)).take() };
                let que = format!("{:08x} con direccion {:#x} (jit={})", w, a, jit_mode);
                if a % al == 0 || (off == 0 && al == 1) {
                    assert!(seen.is_none(), "alineado no debe fallar: {}", que);
                    continue;
                }
                let (sig, code_, addr, pc, x) = seen.unwrap_or_else(|| panic!("sin SIGBUS: {}", que));
                assert_eq!((sig, code_, addr), (7, 1, a), "{}", que);
                assert_eq!(pc, code.as_ptr() as u64, "pc exacto: {}", que);
                assert_eq!(x, before, "registros en el manejador: {}", que);
            }
        }
    }
    unsafe { sigaction(7, &old, std::ptr::null_mut()) };
}

// ---------------------------------------------------------------------------------------------
// Microbenchmark
// ---------------------------------------------------------------------------------------------

/// Bucle interno de mat4 del banco (b_mat4, 35 instrucciones, 2048 iteraciones por llamada).
const MAT4: [u32; 35] = [
    0xad7f0121, 0xad7f0d62, 0x6e26dc00, 0x6e26dc21, 0x4e23cce0, 0x4e22cce1, 0xacc21123, 0xacc21562, 0x6e26dc63, 0x4e30c400,
    0x6e26dc84, 0x4e30c421, 0x4e22cce3, 0x4eb1c400, 0x3ce86aa2, 0x4e25cce4, 0x4eb1c421, 0x4e30c463, 0x4fa29005, 0x4e30c484,
    0xad000141, 0x4eb1c463, 0x4f821025, 0x4eb1c484, 0x4f821865, 0xad011143, 0x9101014a, 0x4fa21885, 0x4e22ce45, 0x4e33c4a2,
    0x4eb4c442, 0x3ca86aa2, 0x91004108, 0xf140211f, 0x54fffbc1,
];
/// Bucle principal de entero_saltos del banco (64 instrucciones: xorshift, UMULH, MSUB, BFI, FMOV/INS, stores).
const ENTERO: [u32; 64] = [
    0xca1536af, 0xf1000529, 0xca4f1def, 0xca0f45f0, 0xca10360f, 0xd342fe01, 0xca4f1def, 0x9bc77c21, 0xca0f45f1, 0xca11362f,
    0xd342fe22, 0x53027c21, 0xca4f1def, 0x9bc77c42, 0xca0f45e0, 0x1b0ac030, 0xca00340f, 0x9bcb7c04, 0x53027c42, 0x9e670201,
    0xca4f1def, 0x1b0ac451, 0xca0f45ef, 0xd34bfc81, 0xca0f35e3, 0x9bcb7de5, 0x4e181e21, 0xca431c63, 0x1b0c8020, 0xca034463,
    0xd34bfca2, 0x9e670002, 0xca033466, 0x1b0cbc4f, 0xca461cc6, 0xca0644c4, 0x4e181de2, 0x9bcd7c6f, 0xca043481, 0x9bcd7c80,
    0xca411c30, 0x4e811841, 0xd345fdef, 0xca104611, 0x12000e10, 0xd345fc00, 0x1b0e8def, 0xca113631, 0x4ea08421, 0x1b0e9000,
    0xca511e31, 0x510051ef, 0x3c9e8101, 0xca114621, 0x331c1630, 0x51005011, 0x293f450f, 0xca013421, 0xca411c21, 0x2a012e10,
    0xca014435, 0x3216020f, 0x7801c50f, 0x54fff821,
];

fn bench_loop(words: &[u32], regs: bool, setup: &dyn Fn(&mut Cpu), iters: u64, reps: usize) -> f64 {
    let mut code = words.to_vec();
    code.push(SVC);
    let mut j = Jit::new();
    j.regs = regs;
    j.chain = true;
    let mut best = f64::MAX;
    for _ in 0..reps {
        let mut c = Cpu::new();
        setup(&mut c);
        c.pc = code.as_ptr() as u64;
        clear_mxcsr();
        let t0 = std::time::Instant::now();
        assert_eq!(j.run(&mut c), Event::Svc(0));
        let ns = t0.elapsed().as_nanos() as f64 / (iters * words.len() as u64) as f64;
        best = best.min(ns);
    }
    best
}

/// `cargo test --release --lib jit::regs_tests::bench_registros -- --ignored --nocapture --test-threads=1`
#[test]
#[ignore]
fn bench_registros() {
    // mat4: x9/x11 recorren dos bufers de 2048 x 64 B (post-indice), x10 escribe otro, x21 + x8 el vector
    let a = vec![0x3F80_0000u32; 2048 * 16 + 64];
    let b = vec![0x3F00_0000u32; 2048 * 16 + 64];
    let mut cbuf = vec![0u32; 2048 * 16 + 64];
    let mut vbuf = vec![0x3F80_0000u32; 0x8000 / 4 + 16];
    let (pa, pb, pc_, pv) = (a.as_ptr() as u64, b.as_ptr() as u64, cbuf.as_mut_ptr() as u64, vbuf.as_mut_ptr() as u64);
    let mat4 = move |c: &mut Cpu| {
        c.x[8] = 0;
        c.x[9] = pa + 0x20;
        c.x[11] = pb + 0x20;
        c.x[10] = pc_;
        c.x[21] = pv;
        for r in 0..32 {
            let f = (0.25f32 + r as f32 * 0.125).to_bits() as u64;
            c.v[r] = [f | f << 32, f | f << 32];
        }
    };
    // entero_saltos: x9 iteraciones; x8 avanza 28 B por iteracion
    const IT: u64 = 20_000;
    let mut ebuf = vec![0u8; IT as usize * 28 + 256];
    let pe = ebuf.as_mut_ptr() as u64;
    let entero = move |c: &mut Cpu| {
        c.x[9] = IT;
        c.x[8] = pe + 0x20;
        c.x[21] = 0x9E37_79B9_7F4A_7C15;
        c.x[7] = 0xCCCC_CCCC_CCCC_CCCD;
        c.x[10] = 9000;
        c.x[11] = 0x3A41_D3B4_D3F2_A2B1;
        c.x[12] = 9000;
        c.x[13] = 0xC7CE_0C7C_E0C7_CE0D;
        c.x[14] = 41;
    };
    // instrucciones de los bucles que el JIT manda al interprete (bloque [w, SVC]: el SVC cuenta 1)
    for (words, setup) in [(&MAT4[..], &mat4 as &dyn Fn(&mut Cpu)), (&ENTERO[..], &entero as &dyn Fn(&mut Cpu))] {
        for &w in words {
            if matches!(decode(w), Op::BCond { .. }) {
                continue;
            }
            let code = [w, SVC];
            let mut j = Jit::new();
            let mut c = Cpu::new();
            setup(&mut c);
            c.pc = code.as_ptr() as u64;
            let h0 = HELPER_CALLS.load(std::sync::atomic::Ordering::Relaxed);
            let _ = j.run(&mut c);
            let h = HELPER_CALLS.load(std::sync::atomic::Ordering::Relaxed) - h0;
            if h > 1 {
                println!("BENCH al interprete: {:#010x} {:?}", w, decode(w));
            }
        }
    }
    for rep in 0..2 {
        for regs in [false, true] {
            let m = bench_loop(&MAT4, regs, &mat4, 2048, 20);
            let e = bench_loop(&ENTERO, regs, &entero, IT, 10);
            println!("BENCH ronda {} cache={}: mat4 {:.3} ns/instr | entero_saltos {:.3} ns/instr", rep, regs, m, e);
        }
    }
    drop((a, b, cbuf, vbuf, ebuf));
}
