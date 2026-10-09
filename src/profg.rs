//! Perfilador de las instrucciones FP/SIMD que caen al interprete (h_exec), solo bajo `jit::PROF_ON`.
//!
//! Clasifica la palabra de instruccion por grupo de codificacion del ARM ARM (seccion "Data Processing -- Scalar
//! Floating-Point and Advanced SIMD" y "Loads and Stores") y, dentro del grupo, por el campo de opcode; ademas
//! cuenta la RAZON de la caida. Los contadores son un arreglo fijo de atomicos (cota: GRUPOS x 256 x RAZONES) y
//! no cuestan nada con el perfilador apagado. No toca el codigo generado ni el comportamiento del guest.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub const GRUPOS: usize = 34;
pub const SUBS: usize = 256;
pub const RAZONES: usize = 9;

/// Razones de caida a h_exec.
pub const R_NO_TRAD: usize = 0; // el JIT no traduce esta forma (no hay ruta rapida)
pub const R_FPCR: usize = 1; // FPCR != 0 al ejecutar
pub const R_IOC: usize = 2; // la ruta rapida se descarto y el interprete levanto IOC (invalido)
pub const R_UFC: usize = 3; // idem con UFC (underflow)
pub const R_NAN: usize = 4; // resultado NaN (sin flag: NaN silencioso propagado)
pub const R_CERO: usize = 5; // resultado cero
pub const R_PEQ: usize = 6; // resultado pequeno (exponente <= 1: denormal o < 2*minnormal)
pub const R_FCMP: usize = 7; // FCMP/FCMPE con operando NaN (desordenado)
pub const R_OTRO: usize = 8; // ruta rapida descartada sin causa identificable en el resultado
pub const RAZON_NOMBRE: [&str; RAZONES] =
    ["no_traducida", "FPCR!=0", "IOC", "UFC", "NaN", "cero", "pequeno", "fcmp_NaN", "otro"];

/// Origen de la llamada a h_exec (se guarda junto al Op, ver `jit::OpR`).
pub const O_GENERAL: u8 = 0; // sitio de "todo lo demas" (no traducida)
pub const O_RAPIDA: u8 = 1; // jitfp::Fx::finish: ruta rapida descartada
pub const O_FCMP: u8 = 2; // jitfp::fcmp

pub static PROF_FPG: [AtomicU64; GRUPOS * SUBS * RAZONES] = [const { AtomicU64::new(0) }; GRUPOS * SUBS * RAZONES];

pub const GRUPO_NOMBRE: [&str; GRUPOS] = [
    "FP data-proc 1 source",
    "FP data-proc 2 source",
    "FP data-proc 3 source",
    "FP compare",
    "FP cond compare",
    "FP cond select",
    "FP immediate",
    "FP<->int conversion",
    "FP<->fixed conversion",
    "SIMD three same",
    "SIMD three different",
    "SIMD two-reg misc",
    "SIMD across lanes",
    "SIMD copy",
    "SIMD permute",
    "SIMD extract",
    "SIMD table lookup",
    "SIMD by element",
    "SIMD modified imm",
    "SIMD shift by imm",
    "SIMD three-reg extension",
    "SIMD scalar three same",
    "SIMD scalar three different",
    "SIMD scalar two-reg misc",
    "SIMD scalar pairwise",
    "SIMD scalar copy",
    "SIMD scalar by element",
    "SIMD scalar shift by imm",
    "crypto",
    "SIMD ld/st multiple structures",
    "SIMD ld/st single structure",
    "FP ld/st reg/pair/literal",
    "SIMD otro",
    "FP otro",
];

#[inline]
fn b(w: u32, hi: u32, lo: u32) -> u32 {
    (w >> lo) & ((1u32 << (hi - lo + 1)) - 1)
}

/// (grupo, sub) de una palabra FP/SIMD. `sub` < 256.
pub fn clasifica(w: u32) -> (usize, usize) {
    let u = b(w, 29, 29) as usize;
    let size = b(w, 23, 22) as usize;
    let q = b(w, 30, 30) as usize;
    // Cargas y almacenamientos (bits 27 = 1 y 25 = 0)
    if b(w, 27, 27) == 1 && b(w, 25, 25) == 0 && b(w, 26, 26) == 1 {
        return match b(w, 29, 28) {
            0 => {
                if b(w, 24, 24) == 0 {
                    (29, (b(w, 15, 12) | b(w, 22, 22) << 4 | b(w, 23, 23) << 5) as usize)
                } else {
                    (30, (b(w, 15, 13) | b(w, 21, 21) << 3 | b(w, 22, 22) << 4 | b(w, 23, 23) << 5) as usize)
                }
            }
            g => (31, (b(w, 31, 30) | (g << 2) | b(w, 22, 22) << 4 | b(w, 24, 24) << 5 | b(w, 21, 21) << 6) as usize),
        };
    }
    // Crypto
    if w & 0xFF3E_0C00 == 0x4E28_0800 || w & 0xFF20_8C00 == 0x5E00_0000 || w & 0xFF3E_0C00 == 0x5E28_0800 || b(w, 31, 24) == 0xCE {
        return (28, (b(w, 31, 24) as usize) & 0xFF);
    }
    // Escalar FP (bits 28:24 = 11110 / 11111, bit 31 = M, bit 29 = S)
    if w & 0x5F00_0000 == 0x1F00_0000 {
        return (2, (b(w, 21, 21) << 1 | b(w, 15, 15)) as usize | (b(w, 23, 22) as usize) << 2);
    }
    if w & 0x5F00_0000 == 0x1E00_0000 {
        let ft = b(w, 23, 22) as usize;
        if b(w, 21, 21) == 0 {
            return (8, b(w, 18, 16) as usize | ft << 3 | (b(w, 20, 19) as usize) << 5);
        }
        if w & 0x0000_FC00 == 0 {
            return (7, b(w, 18, 16) as usize | (b(w, 20, 19) as usize) << 3 | ft << 5 | (b(w, 31, 31) as usize) << 7);
        }
        if w & 0x0000_7C00 == 0x4000 {
            return (0, b(w, 20, 15) as usize | ft << 6);
        }
        if w & 0x0000_3C00 == 0x2000 {
            return (3, b(w, 4, 3) as usize | ft << 2);
        }
        if w & 0x0000_1C00 == 0x1000 {
            return (6, ft);
        }
        return match b(w, 11, 10) {
            1 => (4, b(w, 4, 4) as usize | ft << 1),
            2 => (1, b(w, 15, 12) as usize | ft << 4),
            3 => (5, ft),
            _ => (33, 0),
        };
    }
    // SIMD escalar (01 U 11110 ...)
    if w & 0xDF00_0000 == 0x5E00_0000 || w & 0xDF00_0000 == 0x5F00_0000 {
        if w & 0xDF20_0400 == 0x5E20_0400 {
            return (21, b(w, 15, 11) as usize | u << 5 | size << 6);
        }
        if w & 0xDF20_0C00 == 0x5E20_0000 {
            return (22, b(w, 15, 12) as usize | u << 4 | size << 5);
        }
        if w & 0xDF3E_0C00 == 0x5E20_0800 {
            return (23, b(w, 16, 12) as usize | u << 5 | size << 6);
        }
        if w & 0xDF3E_0C00 == 0x5E30_0800 {
            return (24, b(w, 16, 12) as usize | u << 5 | size << 6);
        }
        if w & 0xDFE0_8400 == 0x5E00_0400 {
            return (25, b(w, 14, 11) as usize);
        }
        if w & 0xDF00_0400 == 0x5F00_0000 {
            return (26, b(w, 15, 12) as usize | u << 4 | size << 5);
        }
        if w & 0xDF00_0400 == 0x5F00_0400 && b(w, 22, 19) != 0 {
            return (27, b(w, 15, 11) as usize | u << 5);
        }
        return (32, b(w, 28, 21) as usize);
    }
    // SIMD vectorial (0 Q U 0111x ...)
    if w & 0x9E00_0000 == 0x0E00_0000 {
        if w & 0x9F20_0400 == 0x0E20_0400 {
            return (9, b(w, 15, 11) as usize | u << 5 | size << 6);
        }
        if w & 0x9F20_0C00 == 0x0E20_0000 {
            return (10, b(w, 15, 12) as usize | u << 4 | size << 5);
        }
        if w & 0x9F3E_0C00 == 0x0E20_0800 {
            return (11, b(w, 16, 12) as usize | u << 5 | size << 6);
        }
        if w & 0x9F3E_0C00 == 0x0E30_0800 {
            return (12, b(w, 16, 12) as usize | u << 5 | size << 6);
        }
        if w & 0x9FE0_8400 == 0x0E00_0400 {
            return (13, b(w, 14, 11) as usize | u << 4);
        }
        if w & 0xBF20_8C00 == 0x0E00_0800 {
            return (14, b(w, 14, 12) as usize | size << 3 | q << 5);
        }
        if w & 0xBF20_8400 == 0x2E00_0000 {
            return (15, q);
        }
        if w & 0xBF20_8C00 == 0x0E00_0000 {
            return (16, b(w, 14, 12) as usize);
        }
        if w & 0x9F20_8400 == 0x0E00_8400 {
            return (20, b(w, 14, 11) as usize | u << 4 | size << 5);
        }
        if w & 0x9F00_0400 == 0x0F00_0000 {
            return (17, b(w, 15, 12) as usize | u << 4 | size << 5);
        }
        if w & 0x9FF8_0400 == 0x0F00_0400 {
            return (18, 0); // el sub fino (cmode, op) lo da clasifica_fino
        }
        if w & 0x9F80_0400 == 0x0F00_0400 {
            return (19, b(w, 15, 11) as usize | u << 5 | q << 6);
        }
        return (32, b(w, 28, 21) as usize);
    }
    (33, b(w, 28, 21) as usize)
}

/// Variante de 'clasifica' para modified imm: cmode y op en el sub (se corrige aqui para no ocupar mas bits).
fn sub_modimm(w: u32) -> usize {
    (b(w, 15, 12) | b(w, 29, 29) << 4) as usize
}

pub fn clasifica_fino(w: u32) -> (usize, usize) {
    let (g, s) = clasifica(w);
    if g == 18 {
        (g, sub_modimm(w))
    } else {
        (g, s)
    }
}

// ----------------------------------------------------------------------------------------------
// Razon de la caida
// ----------------------------------------------------------------------------------------------

/// Clasifica por el resultado (el interprete ya lo dejo en `v`) por que se descarto la ruta rapida.
/// `fpcr`/`fpsr0`: estado previo a la ejecucion; `fpsr1`: posterior.
pub fn razon(origen: u8, w: u32, fpcr: u64, fpsr0: u64, fpsr1: u64, v: &[[u64; 2]]) -> usize {
    if origen == O_GENERAL {
        return R_NO_TRAD;
    }
    if fpcr != 0 {
        return R_FPCR;
    }
    if origen == O_FCMP {
        return R_FCMP;
    }
    let nuevo = fpsr1 & !fpsr0;
    if nuevo & 1 != 0 {
        return R_IOC;
    }
    if nuevo & 8 != 0 {
        return R_UFC;
    }
    let rd = (w & 31) as usize;
    let (g, _) = clasifica(w);
    // (anchura del elemento en bits, numero de lanes) solo para operaciones FP que escriben un registro vectorial
    let q = (w >> 30) & 1;
    let sz = (w >> 22) & 1;
    let ft = (w >> 22) & 3;
    let esc = |ft: u32| -> Option<(u32, u32)> {
        match ft {
            0 => Some((32, 1)),
            1 => Some((64, 1)),
            _ => None,
        }
    };
    let vec_fp = |sz: u32, q: u32| -> Option<(u32, u32)> {
        if sz == 1 {
            Some((64, q + 1))
        } else {
            Some((32, 2 * (q + 1)))
        }
    };
    let op5 = (w >> 11) & 0x1F;
    let op5b = (w >> 12) & 0x1F;
    let op4 = (w >> 12) & 0xF;
    let el = match g {
        0 | 1 | 2 => esc(ft),
        9 if op5 >= 0x18 => vec_fp(sz, q),
        21 if op5 >= 0x18 => esc(sz),
        11 if op5b >= 0x16 || (op5b >= 0xC && (w >> 23) & 1 == 1) => vec_fp(sz, q),
        17 if matches!(op4, 1 | 5 | 9) => vec_fp(sz, q),
        26 if matches!(op4, 1 | 5 | 9) => esc(sz),
        _ => None,
    };
    let Some((bits, lanes)) = el else { return R_OTRO };
    let mut r = R_OTRO;
    let reg = v[rd];
    for l in 0..lanes {
        let (exp, man, emax) = if bits == 64 {
            let x = reg[l as usize];
            (((x >> 52) & 0x7FF) as u32, x & ((1u64 << 52) - 1), 0x7FFu32)
        } else {
            let x = (reg[(l / 2) as usize] >> (32 * (l % 2))) as u32;
            (((x >> 23) & 0xFF) as u32, (x & 0x7F_FFFF) as u64, 0xFFu32)
        };
        let c = if exp == emax && man != 0 {
            R_NAN
        } else if exp == 0 && man == 0 {
            R_CERO
        } else if exp <= 1 {
            R_PEQ
        } else {
            R_OTRO
        };
        // prioridad NaN > pequeno > cero > otro
        let pr = |x: usize| match x {
            R_NAN => 3,
            R_PEQ => 2,
            R_CERO => 1,
            _ => 0,
        };
        if pr(c) > pr(r) {
            r = c;
        }
    }
    r
}

pub fn cuenta(origen: u8, w: u32, fpcr: u64, fpsr0: u64, fpsr1: u64, v: &[[u64; 2]]) {
    let (g, s) = clasifica_fino(w);
    let r = razon(origen, w, fpcr, fpsr0, fpsr1, v);
    PROF_FPG[(g * SUBS + s) * RAZONES + r].fetch_add(1, Relaxed);
}

// ----------------------------------------------------------------------------------------------
// Nombres
// ----------------------------------------------------------------------------------------------

fn sz_letra(sz: usize) -> &'static str {
    ["b", "h", "s", "d"][sz & 3]
}

fn tres_iguales(sub: usize) -> String {
    let op = sub & 0x1F;
    let u = (sub >> 5) & 1;
    let size = sub >> 6;
    if op >= 0x18 {
        let b23 = size >> 1;
        let n = match (u, b23, op) {
            (0, 0, 0x18) => "FMAXNM",
            (0, 0, 0x19) => "FMLA",
            (0, 0, 0x1A) => "FADD",
            (0, 0, 0x1B) => "FMULX",
            (0, 0, 0x1C) => "FCMEQ",
            (0, 0, 0x1E) => "FMAX",
            (0, 0, 0x1F) => "FRECPS",
            (0, 1, 0x18) => "FMINNM",
            (0, 1, 0x19) => "FMLS",
            (0, 1, 0x1A) => "FSUB",
            (0, 1, 0x1E) => "FMIN",
            (0, 1, 0x1F) => "FRSQRTS",
            (1, 0, 0x18) => "FMAXNMP",
            (1, 0, 0x1A) => "FADDP",
            (1, 0, 0x1B) => "FMUL",
            (1, 0, 0x1C) => "FCMGE",
            (1, 0, 0x1D) => "FACGE",
            (1, 0, 0x1E) => "FMAXP",
            (1, 0, 0x1F) => "FDIV",
            (1, 1, 0x18) => "FMINNMP",
            (1, 1, 0x1A) => "FABD",
            (1, 1, 0x1C) => "FCMGT",
            (1, 1, 0x1D) => "FACGT",
            (1, 1, 0x1E) => "FMINP",
            _ => return format!("fp_opc{:#x}_U{}_b23={}", op, u, b23),
        };
        return format!("{}.{}", n, if size & 1 == 1 { "d" } else { "s" });
    }
    let n = match (u, op) {
        (0, 0) => "SHADD",
        (1, 0) => "UHADD",
        (0, 1) => "SQADD",
        (1, 1) => "UQADD",
        (0, 2) => "SRHADD",
        (1, 2) => "URHADD",
        (0, 3) => ["AND", "BIC", "ORR", "ORN"][size],
        (1, 3) => ["EOR", "BSL", "BIT", "BIF"][size],
        (0, 4) => "SHSUB",
        (1, 4) => "UHSUB",
        (0, 5) => "SQSUB",
        (1, 5) => "UQSUB",
        (0, 6) => "CMGT",
        (1, 6) => "CMHI",
        (0, 7) => "CMGE",
        (1, 7) => "CMHS",
        (0, 8) => "SSHL",
        (1, 8) => "USHL",
        (0, 9) => "SQSHL",
        (1, 9) => "UQSHL",
        (0, 10) => "SRSHL",
        (1, 10) => "URSHL",
        (0, 11) => "SQRSHL",
        (1, 11) => "UQRSHL",
        (0, 12) => "SMAX",
        (1, 12) => "UMAX",
        (0, 13) => "SMIN",
        (1, 13) => "UMIN",
        (0, 14) => "SABD",
        (1, 14) => "UABD",
        (0, 15) => "SABA",
        (1, 15) => "UABA",
        (0, 16) => "ADD",
        (1, 16) => "SUB",
        (0, 17) => "CMTST",
        (1, 17) => "CMEQ",
        (0, 18) => "MLA",
        (1, 18) => "MLS",
        (0, 19) => "MUL",
        (1, 19) => "PMUL",
        (0, 20) => "SMAXP",
        (1, 20) => "UMAXP",
        (0, 21) => "SMINP",
        (1, 21) => "UMINP",
        (0, 22) => "SQDMULH",
        (1, 22) => "SQRDMULH",
        (0, 23) => "ADDP",
        _ => return format!("opc{:#x}_U{}", op, u),
    };
    if op == 3 {
        n.to_string()
    } else {
        format!("{}.{}", n, sz_letra(size))
    }
}

fn dos_regs(sub: usize) -> String {
    let op = sub & 0x1F;
    let u = (sub >> 5) & 1;
    let size = sub >> 6;
    let b23 = size >> 1;
    let fp = match (u, b23, op) {
        (0, 1, 0x0C) => "FCMGT0",
        (0, 1, 0x0D) => "FCMEQ0",
        (0, 1, 0x0E) => "FCMLT0",
        (0, 1, 0x0F) => "FABS",
        (0, 1, 0x1A) => "FCVTPS",
        (0, 1, 0x1B) => "FCVTZS",
        (0, 1, 0x1D) => "FRECPE",
        (0, 0, 0x16) => "FCVTN",
        (0, 0, 0x17) => "FCVTL",
        (0, 0, 0x18) => "FRINTN",
        (0, 0, 0x19) => "FRINTM",
        (0, 0, 0x1A) => "FCVTNS",
        (0, 0, 0x1B) => "FCVTMS",
        (0, 0, 0x1C) => "FCVTAS",
        (0, 0, 0x1D) => "SCVTF",
        (1, 1, 0x0C) => "FCMGE0",
        (1, 1, 0x0D) => "FCMLE0",
        (1, 1, 0x0F) => "FNEG",
        (1, 1, 0x19) => "FRINTI",
        (1, 1, 0x1A) => "FCVTPU",
        (1, 1, 0x1B) => "FCVTZU",
        (1, 1, 0x1D) => "FRSQRTE",
        (1, 1, 0x1F) => "FSQRT",
        (1, 0, 0x16) => "FCVTXN",
        (1, 0, 0x18) => "FRINTA",
        (1, 0, 0x19) => "FRINTX",
        (1, 0, 0x1A) => "FCVTNU",
        (1, 0, 0x1B) => "FCVTMU",
        (1, 0, 0x1C) => "FCVTAU",
        (1, 0, 0x1D) => "UCVTF",
        _ => "",
    };
    if !fp.is_empty() {
        return format!("{}.{}", fp, if size & 1 == 1 { "d" } else { "s" });
    }
    let n = match (u, op) {
        (0, 0) => "REV64",
        (0, 1) => "REV16",
        (0, 2) => "SADDLP",
        (0, 3) => "SUQADD",
        (0, 4) => "CLS",
        (0, 5) => "CNT",
        (0, 6) => "SADALP",
        (0, 7) => "SQABS",
        (0, 8) => "CMGT0",
        (0, 9) => "CMEQ0",
        (0, 10) => "CMLT0",
        (0, 11) => "ABS",
        (0, 0x12) => "XTN",
        (0, 0x14) => "SQXTN",
        (1, 0) => "REV32",
        (1, 2) => "UADDLP",
        (1, 3) => "USQADD",
        (1, 4) => "CLZ",
        (1, 5) => "NOT/RBIT",
        (1, 6) => "UADALP",
        (1, 7) => "SQNEG",
        (1, 8) => "CMGE0",
        (1, 9) => "CMLE0",
        (1, 11) => "NEG",
        (1, 0x12) => "SQXTUN/SHLL",
        (1, 0x14) => "UQXTN",
        _ => return format!("opc{:#x}_U{}", op, u),
    };
    format!("{}.{}", n, sz_letra(size))
}

fn por_elemento(sub: usize) -> String {
    let op = sub & 0xF;
    let u = (sub >> 4) & 1;
    let size = sub >> 5;
    let n = match (u, op) {
        (0, 0) => "MLA",
        (0, 1) => "FMLA",
        (0, 2) => "SMLAL",
        (0, 3) => "SQDMLAL",
        (0, 4) => "MLS",
        (0, 5) => "FMLS",
        (0, 6) => "SMLSL",
        (0, 7) => "SQDMLSL",
        (0, 8) => "MUL",
        (0, 9) => "FMUL",
        (0, 0xA) => "SMULL",
        (0, 0xB) => "SQDMULL",
        (0, 0xC) => "SQDMULH",
        (0, 0xD) => "SQRDMULH",
        (1, 0) => "MLA(U)",
        (1, 2) => "UMLAL",
        (1, 4) => "MLS(U)",
        (1, 6) => "UMLSL",
        (1, 8) => "MUL(U)",
        (1, 9) => "FMULX",
        (1, 0xA) => "UMULL",
        _ => return format!("opc{:#x}_U{}", op, u),
    };
    format!("{}.{}", n, sz_letra(size))
}

/// Nombre del mnemonico (o del campo opcode) de la entrada `sub` del grupo `g`.
pub fn nombre(g: usize, sub: usize) -> String {
    let ft = |x: usize| ["s", "d", "?", "h"][x & 3];
    match g {
        0 => {
            let op = sub & 0x3F;
            let n = match op {
                0 => "FMOV",
                1 => "FABS",
                2 => "FNEG",
                3 => "FSQRT",
                4 | 5 | 7 => "FCVT",
                8 => "FRINTN",
                9 => "FRINTP",
                10 => "FRINTM",
                11 => "FRINTZ",
                12 => "FRINTA",
                14 => "FRINTX",
                15 => "FRINTI",
                _ => return format!("opc{:#x}.{}", op, ft(sub >> 6)),
            };
            format!("{}.{}", n, ft(sub >> 6))
        }
        1 => {
            let op = sub & 0xF;
            let n = ["FMUL", "FDIV", "FADD", "FSUB", "FMAX", "FMIN", "FMAXNM", "FMINNM", "FNMUL"].get(op).copied().unwrap_or("?");
            format!("{}.{}", n, ft(sub >> 4))
        }
        2 => format!("{}.{}", ["FMADD", "FMSUB", "FNMADD", "FNMSUB"][sub & 3], ft(sub >> 2)),
        3 => format!("{}{}.{}", if sub & 2 != 0 { "FCMPE" } else { "FCMP" }, if sub & 1 != 0 { "0" } else { "" }, ft(sub >> 2)),
        4 => format!("{}.{}", if sub & 1 != 0 { "FCCMPE" } else { "FCCMP" }, ft(sub >> 1)),
        5 => format!("FCSEL.{}", ft(sub)),
        6 => format!("FMOV#.{}", ft(sub)),
        7 => {
            let op = sub & 7;
            let rm = (sub >> 3) & 3;
            let n = match op {
                0 => ["FCVTNS", "FCVTPS", "FCVTMS", "FCVTZS"][rm],
                1 => ["FCVTNU", "FCVTPU", "FCVTMU", "FCVTZU"][rm],
                2 => "SCVTF",
                3 => "UCVTF",
                4 => "FCVTAS",
                5 => "FCVTAU",
                6 => "FMOV(x<-v)",
                7 => "FMOV(v<-x)",
                _ => "?",
            };
            format!("{}.{}{}", n, ft(sub >> 5), if sub >> 7 != 0 { ".x" } else { ".w" })
        }
        8 => format!("fixed_opc{}.{}rm{}", sub & 7, ft((sub >> 3) & 3), sub >> 5),
        9 | 21 => tres_iguales(sub),
        11 | 23 => dos_regs(sub),
        17 | 26 => por_elemento(sub),
        10 | 22 => format!("opc{:#x}_U{}.{}", sub & 0xF, (sub >> 4) & 1, sz_letra(sub >> 5)),
        12 | 24 => {
            let op = sub & 0x1F;
            let n = match op {
                3 => "SADDLV/UADDLV",
                0xA => "SMAXV/UMAXV",
                0x1A => "SMINV/UMINV",
                0x1B => "ADDV/ADDP",
                0xC => "FMAXNMV",
                0xF => "FMAXV/FMINV",
                0xD => "ADDP(scalar)",
                _ => "",
            };
            if n.is_empty() {
                format!("opc{:#x}_U{}.{}", op, (sub >> 5) & 1, sz_letra(sub >> 6))
            } else {
                format!("{}_U{}.{}", n, (sub >> 5) & 1, sz_letra(sub >> 6))
            }
        }
        13 | 25 => {
            let i4 = sub & 0xF;
            let n = match (sub >> 4, i4) {
                (0, 0) => "DUP(elem)",
                (0, 1) => "DUP(gen)",
                (0, 3) => "INS(gen)",
                (0, 5) => "SMOV",
                (0, 7) => "UMOV",
                (1, _) => "INS(elem)",
                _ => "?",
            };
            n.to_string()
        }
        14 => format!("{}.{}", ["?", "UZP1", "TRN1", "ZIP1", "?", "UZP2", "TRN2", "ZIP2"][sub & 7], sz_letra((sub >> 3) & 3)),
        15 => "EXT".to_string(),
        16 => format!("TBL/TBX len{}", sub & 7),
        18 => format!("MOVI/MVNI/ORR/BIC/FMOV cmode={:#x} op={}", sub & 0xF, sub >> 4),
        19 => {
            let op = sub & 0x1F;
            let u = (sub >> 5) & 1;
            let n = match op {
                0 => "SSHR/USHR",
                2 => "SSRA/USRA",
                4 => "SRSHR/URSHR",
                6 => "SRSRA/URSRA",
                8 => "SRI",
                0xA => "SHL/SLI",
                0xC => "SQSHLU",
                0xE => "SQSHL/UQSHL",
                0x10 => "SHRN/SQSHRUN",
                0x11 => "RSHRN/SQRSHRUN",
                0x12 => "SQSHRN/UQSHRN",
                0x13 => "SQRSHRN/UQRSHRN",
                0x14 => "SSHLL/USHLL",
                0x1C => "SCVTF/UCVTF(fixed)",
                0x1F => "FCVTZS/FCVTZU(fixed)",
                _ => "",
            };
            if n.is_empty() {
                format!("opc{:#x}_U{}", op, u)
            } else {
                format!("{}_U{}", n, u)
            }
        }
        27 => format!("opc{:#x}_U{}", sub & 0x1F, (sub >> 5) & 1),
        20 => format!("opc{:#x}_U{}.{}", sub & 0xF, (sub >> 4) & 1, sz_letra(sub >> 5)),
        28 => format!("crypto b31:24={:#x}", sub),
        29 => format!("{}{} opc{:#x}", if sub & 0x10 != 0 { "LD" } else { "ST" }, if sub & 0x20 != 0 { "(post)" } else { "" }, sub & 0xF),
        30 => format!("{}{} opc{}", if sub & 0x10 != 0 { "LD" } else { "ST" }, if sub & 0x20 != 0 { "(post)" } else { "" }, sub & 7),
        31 => {
            let g2 = (sub >> 2) & 3;
            let l = if sub & 0x10 != 0 { "LDR" } else { "STR" };
            let k = match g2 {
                1 => "lit/pair?",
                2 => "pair",
                _ => {
                    if sub & 0x20 != 0 {
                        "uimm"
                    } else if sub & 0x40 != 0 {
                        "reg"
                    } else {
                        "imm/unsc"
                    }
                }
            };
            format!("{} {} size{}", l, k, sub & 3)
        }
        _ => format!("campo={:#x}", sub),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(w: u32) -> (String, String) {
        let (g, s) = clasifica_fino(w);
        (GRUPO_NOMBRE[g].to_string(), nombre(g, s))
    }

    #[test]
    fn clasifica_codificaciones_conocidas() {
        assert_eq!(n(0x1E62_2820), ("FP data-proc 2 source".into(), "FADD.d".into()));
        assert_eq!(n(0x1E22_0820), ("FP data-proc 2 source".into(), "FMUL.s".into()));
        assert_eq!(n(0x1F42_0C20), ("FP data-proc 3 source".into(), "FMADD.d".into()));
        assert_eq!(n(0x1E65_4020), ("FP data-proc 1 source".into(), "FRINTM.d".into()));
        assert_eq!(n(0x1E22_C020), ("FP data-proc 1 source".into(), "FCVT.s".into()));
        assert_eq!(n(0x1E62_2020), ("FP compare".into(), "FCMP.d".into()));
        assert_eq!(n(0x1E22_0020), ("FP<->int conversion".into(), "SCVTF.s.w".into()));
        assert_eq!(n(0x4E22_CC20), ("SIMD three same".into(), "FMLA.s".into()));
        assert_eq!(n(0x4EA2_8420), ("SIMD three same".into(), "ADD.s".into()));
        assert_eq!(n(0x4E22_1C20), ("SIMD three same".into(), "AND".into()));
        assert_eq!(n(0x6E22_DC20), ("SIMD three same".into(), "FMUL.s".into()));
        assert_eq!(n(0x4F80_9020), ("SIMD by element".into(), "FMUL.s".into()));
        assert_eq!(n(0x4EA0_F820), ("SIMD two-reg misc".into(), "FABS.s".into()));
        assert_eq!(n(0x4C40_A020).0, "SIMD ld/st multiple structures");
        assert_eq!(n(0x4E08_0C20).0, "SIMD copy");
        assert_eq!(n(0x4E28_4820).0, "crypto");
        // las razones: FPCR distinto de cero
        assert_eq!(razon(O_RAPIDA, 0x1E62_2820, 1, 0, 0, &[[0; 2]; 32]), R_FPCR);
        assert_eq!(razon(O_GENERAL, 0x1E62_2820, 0, 0, 0, &[[0; 2]; 32]), R_NO_TRAD);
        assert_eq!(razon(O_RAPIDA, 0x1E62_2820, 0, 0, 0, &[[0; 2]; 32]), R_CERO);
        let mut v = [[0u64; 2]; 32];
        v[0][0] = 0x7FF8_0000_0000_0000;
        assert_eq!(razon(O_RAPIDA, 0x1E62_2820, 0, 0, 0, &v), R_NAN);
    }
}
