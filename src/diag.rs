//! Diagnostico sin reservas de memoria (se puede usar dentro de un manejador de senal):
//!  * `classify`: que es una palabra A64 que heddle no ejecuta. Distingue una instruccion de ARMv8/ARMv9 que heddle no
//!    implementa (con su mnemonico y la extension, p. ej. `SM4E [FEAT_SM4]`), una codificacion indefinida en la
//!    arquitectura (UDF, grupos no asignados) y una que no se reconoce. Solo sirve para el mensaje: el guest recibe
//!    SIGILL igual que en un procesador ARM sin esa extension.
//!  * `syscall_name`: nombre de una llamada al sistema arm64 (numeracion generica, `asm-generic/unistd.h`).
//!
//! La tabla `T` se obtuvo comparando el decodificador de heddle con el desensamblador de LLVM (`-mattr=+all`) sobre
//! millones de palabras al azar de los grupos asignados: cada fila es la mascara de bits que fija el mnemonico. SVE y
//! SME se nombran solo como grupo. Las familias con muchas variantes (MOPS, RCW, exclusivas, BC.cond) se calculan.

use std::fmt;

/// Veredicto sobre una palabra que heddle no ejecuta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Codificacion asignada en ARMv8/ARMv9 (instruccion de una extension conocida) que heddle no implementa.
    NoSoportada,
    /// Indefinida en la arquitectura: UDF o grupo de codificacion no asignado.
    Indefinida,
    /// En un grupo asignado pero fuera de la tabla: indefinida en ARMv8/ARMv9 o de una extension no catalogada.
    NoReconocida,
    /// De una extension que heddle implementa pero el modelo de CPU o el perfil (`feat`) no anuncia: SIGILL como en
    /// ese procesador.
    AusenteEnModelo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ops {
    No,
    /// registro de sistema `S<op0>_<op1>_C<n>_C<m>_<op2>`
    SysReg,
    /// inmediato de 16 bits (bits 20:5)
    Imm16,
}

/// Descripcion de una palabra A64 (ver `classify`). `Display` escribe el mensaje completo.
#[derive(Clone, Copy, Debug)]
pub struct Insn {
    pub word: u32,
    pub verdict: Verdict,
    name: [&'static str; 3],
    /// extension de la arquitectura (`FEAT_*`) o motivo
    pub feat: &'static str,
    /// grupo de codificacion de primer nivel (clase de operandos)
    pub class: &'static str,
    ops: Ops,
}

/// Mnemonico con sus operandos fijos (registro de sistema, inmediato), sin reservar memoria.
pub struct Mnemonic<'a>(&'a Insn);

impl fmt::Display for Mnemonic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let i = self.0;
        for p in i.name {
            f.write_str(p)?;
        }
        let w = i.word;
        match i.ops {
            Ops::No => Ok(()),
            Ops::SysReg => write!(f, " S{}_{}_C{}_C{}_{}", (w >> 19) & 3, (w >> 16) & 7, (w >> 12) & 15, (w >> 8) & 15, (w >> 5) & 7),
            Ops::Imm16 => write!(f, " #{:#x}", (w >> 5) & 0xFFFF),
        }
    }
}

impl Insn {
    pub fn mnemonic(&self) -> Mnemonic<'_> {
        Mnemonic(self)
    }
}

impl fmt::Display for Insn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = self.word;
        match self.verdict {
            Verdict::NoSoportada => write!(f, "instruccion no soportada por heddle: {} ({:#010x}) [{}; {}]", self.mnemonic(), w, self.feat, self.class),
            Verdict::Indefinida if self.name[0].is_empty() => write!(f, "instruccion indefinida en ARMv8/ARMv9 ({:#010x}) [{}]", w, self.class),
            Verdict::Indefinida => write!(f, "instruccion indefinida en ARMv8/ARMv9: {} ({:#010x}) [{}]", self.mnemonic(), w, self.feat),
            Verdict::NoReconocida => {
                write!(f, "instruccion no reconocida por heddle ({:#010x}) [{}; indefinida en ARMv8/ARMv9 o de una extension sin catalogar]", w, self.class)
            }
            Verdict::AusenteEnModelo => {
                write!(f, "instruccion de {} ({:#010x}), ausente en el modelo de CPU {} [{}]", self.feat, w, crate::feat::name(), self.class)
            }
        }
    }
}

const CLASS: [&str; 16] = [
    "grupo reservado",
    "grupo no asignado",
    "SVE",
    "grupo no asignado",
    "carga/almacenamiento",
    "datos con registros",
    "carga/almacenamiento",
    "SIMD y FP",
    "datos con inmediato",
    "datos con inmediato",
    "saltos, excepciones y sistema",
    "saltos, excepciones y sistema",
    "carga/almacenamiento",
    "datos con registros",
    "carga/almacenamiento",
    "SIMD y FP",
];

const NO_CANONICA: &str = "codificacion no canonica: campos fijos con otro valor, CONSTRAINED UNPREDICTABLE";
const COND: [&str; 16] = ["EQ", "NE", "HS", "LO", "MI", "PL", "VS", "VC", "HI", "LS", "GE", "LT", "GT", "LE", "AL", "NV"];
/// sufijo de orden de memoria indexado por (A, L) = bits 23:22 (familias RCW y LSE128)
const AL: [&str; 4] = ["", "L", "A", "AL"];
const SZ: [&str; 4] = ["B", "H", "", ""];
const CPY_SUF: [&str; 16] = ["", "WT", "RT", "T", "WN", "WTWN", "RTWN", "TWN", "RN", "WTRN", "RTRN", "TRN", "N", "WTN", "RTN", "TN"];

fn ins(w: u32, v: Verdict, name: [&'static str; 3], feat: &'static str, ops: Ops) -> Insn {
    let class = if (w >> 25) & 15 == 0 && w >> 31 == 1 { "SME" } else { CLASS[((w >> 25) & 15) as usize] };
    Insn { word: w, verdict: v, name, feat, class, ops }
}

fn known(w: u32, name: [&'static str; 3], feat: &'static str) -> Insn {
    ins(w, Verdict::NoSoportada, name, feat, Ops::No)
}

/// Que es la palabra `w` (pensado para las que heddle no ejecuta; con una que si ejecuta la respuesta no es fiable).
pub fn classify(w: u32) -> Insn {
    // una que heddle implementa pero el modelo de CPU no anuncia: el decodificador anota la extension que falto
    if matches!(crate::decode::decode(w), crate::decode::Op::Undef(_)) {
        if let (true, Some(f)) = crate::feat::probe_all(|| !matches!(crate::decode::decode(w), crate::decode::Op::Undef(_))) {
            return ins(w, Verdict::AusenteEnModelo, ["", "", ""], f, Ops::No);
        }
    }
    let op1 = (w >> 25) & 15;
    let bit = |b: u32| (w >> b) & 1;
    match op1 {
        0 if w >> 31 == 1 => return known(w, ["SME", "", ""], "FEAT_SME (no soportado)"),
        0 if w >> 16 == 0 => return ins(w, Verdict::Indefinida, ["UDF", "", ""], "permanentemente indefinida", Ops::Imm16),
        0 | 1 | 3 => return ins(w, Verdict::Indefinida, ["", "", ""], "", Ops::No),
        2 => return known(w, ["SVE", "", ""], "FEAT_SVE/SVE2 (no soportado)"),
        _ => {}
    }
    // exclusivas y ordenadas: LDXR/STLR... con campos fijos (Rs, Rt2) que no valen 11111, y LORegions
    if w & 0x3F00_0000 == 0x0800_0000 {
        let (o2, l, o1, o0) = (bit(23), bit(22), bit(21), bit(15));
        let sz = SZ[(w >> 30) as usize];
        let lo = (l * 2 + o0) as usize;
        return match (o2, o1) {
            (1, 1) => known(w, ["CAS", ["", "L", "A", "AL"][lo], sz], NO_CANONICA),
            (0, 1) if w >> 31 == 0 => known(w, ["CASP", ["", "L", "A", "AL"][lo], ""], NO_CANONICA),
            (0, 1) => known(w, [["STXP", "STLXP", "LDXP", "LDAXP"][lo], "", ""], NO_CANONICA),
            (0, _) => known(w, [["STXR", "STLXR", "LDXR", "LDAXR"][lo], sz, ""], NO_CANONICA),
            _ => known(w, [["STLLR", "STLR", "LDLAR", "LDAR"][lo], sz, ""], if o0 == 0 { "FEAT_LOR" } else { NO_CANONICA }),
        };
    }
    // MOPS: CPYF*/CPY*/SET*/SETG* (prologo, cuerpo, epilogo y opciones)
    if w & 0xFB20_0C00 == 0x1900_0400 {
        let g = bit(26) == 1;
        let op2 = ((w >> 12) & 15) as usize;
        let stage = (w >> 22) & 3;
        if stage < 3 {
            let base = if g { ["CPYP", "CPYM", "CPYE"] } else { ["CPYFP", "CPYFM", "CPYFE"] };
            return known(w, [base[stage as usize], CPY_SUF[op2], ""], "FEAT_MOPS");
        }
        if op2 >> 2 < 3 {
            let base = if g { ["SETGP", "SETGM", "SETGE"] } else { ["SETP", "SETM", "SETE"] };
            return known(w, [base[op2 >> 2], ["", "T", "N", "TN"][op2 & 3], ""], "FEAT_MOPS");
        }
    }
    // RCW* (FEAT_THE): S = bit 30, orden de memoria en los bits 23:22
    let s = if bit(30) == 1 { "RCWS" } else { "RCW" };
    let al = AL[((w >> 22) & 3) as usize];
    if w & 0xBF20_FC00 == 0x1920_0800 {
        return known(w, [s, "CAS", al], "FEAT_THE");
    }
    if w & 0xBF21_FC01 == 0x1920_0C00 {
        return known(w, [s, "CASP", al], "FEAT_THE");
    }
    for (v, op) in [(0x3820_9000, "CLR"), (0x3820_B000, "SET"), (0x3820_A000, "SWP"), (0x1920_9000, "CLRP"), (0x1920_B000, "SETP"), (0x1920_A000, "SWPP")] {
        if w & 0xBF20_FC00 == v {
            return known(w, [s, op, al], "FEAT_THE");
        }
    }
    // LSE128
    for (v, op) in [(0x1920_1000, "LDCLRP"), (0x1920_3000, "LDSETP"), (0x1920_8000, "SWPP")] {
        if w & 0xFF20_FC00 == v {
            return known(w, [op, al, ""], "FEAT_LSE128");
        }
    }
    if w & 0xFF00_0010 == 0x5400_0010 {
        return known(w, ["BC.", COND[(w & 15) as usize], ""], "FEAT_HBC");
    }
    for &(mask, val, name, feat, ops) in T {
        if w & mask == val {
            return ins(w, Verdict::NoSoportada, [name, "", ""], feat, ops);
        }
    }
    ins(w, Verdict::NoReconocida, ["", "", ""], "", Ops::No)
}

/// (mascara, valor, mnemonico, extension, operandos), de la mas especifica a la menos: gana la primera que encaja.
#[rustfmt::skip]
const T: &[(u32, u32, &str, &str, Ops)] = &[
    (0xffffffff, 0xd508751f, "IC IALLU", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffff, 0xd508711f, "IC IALLUIS", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffe0, 0xd5087620, "DC IVAC", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffe0, 0xd5087640, "DC ISW", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffe0, 0xd5087a40, "DC CSW", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffe0, 0xd5087e40, "DC CISW", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffe0, 0xd50b7c20, "DC CVAP", "FEAT_DPB", Ops::No),
    (0xffffffe0, 0xd50b7d20, "DC CVADP", "FEAT_DPB2", Ops::No),
    (0xffffffe0, 0xd50b7460, "DC GVA", "FEAT_MTE", Ops::No),
    (0xffffffe0, 0xd50b7480, "DC GZVA", "FEAT_MTE", Ops::No),
    (0xffffffe0, 0xd50b7380, "CFP RCTX", "FEAT_SPECRES", Ops::No),
    (0xffffffe0, 0xd50b73a0, "DVP RCTX", "FEAT_SPECRES", Ops::No),
    (0xffffffe0, 0xd50b73e0, "CPP RCTX", "FEAT_SPECRES", Ops::No),
    (0xffffffe0, 0xd50b7700, "GCSPUSHM", "FEAT_GCS", Ops::No),
    (0xffffffe0, 0xd50b7740, "GCSSS1", "FEAT_GCS", Ops::No),
    (0xfffff9ff, 0xd503417f, "SMSTART", "FEAT_SME (no soportado)", Ops::No),
    (0xfffff9ff, 0xd503407f, "SMSTOP", "FEAT_SME (no soportado)", Ops::No),
    (0xfffff0ff, 0xd503405f, "MSR DIT, #imm", "FEAT_DIT", Ops::No),
    (0xfffff0ff, 0xd503403f, "MSR SSBS, #imm", "FEAT_SSBS", Ops::No),
    (0xfffff0ff, 0xd503409f, "MSR TCO, #imm", "FEAT_MTE", Ops::No),
    (0xfffff0ff, 0xd50340df, "MSR DAIFSet, #imm", "solo EL1 en Linux (SCTLR_EL1.UMA=0)", Ops::No),
    (0xfffff0ff, 0xd50340ff, "MSR DAIFClr, #imm", "solo EL1 en Linux (SCTLR_EL1.UMA=0)", Ops::No),
    (0xfffff0ff, 0xd500409f, "MSR PAN, #imm", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xfffff0ff, 0xd500407f, "MSR UAO, #imm", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xfffff0ff, 0xd50040bf, "MSR SPSel, #imm", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xfff8f000, 0xd5088000, "TLBI", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xfff8fe00, 0xd5087800, "AT", "solo EL1 (UNDEFINED en EL0)", Ops::No),
    (0xffffffff, 0xd503307f, "TCOMMIT", "FEAT_TME", Ops::No),
    (0xffffffff, 0xdac1bbfe, "AUTIA171615", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac1bffe, "AUTIB171615", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac18bfe, "PACIA171615", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac18ffe, "PACIB171615", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac183fe, "PACNBIASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac187fe, "PACNBIBSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac1a3fe, "PACIASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xdac1a7fe, "PACIBSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffff, 0xd69f0bff, "ERETAA", "FEAT_PAuth", Ops::No),
    (0xffffffff, 0xd65f0bff, "RETAA", "FEAT_PAuth", Ops::No),
    (0xffffffff, 0xd65f0fff, "RETAB", "FEAT_PAuth", Ops::No),
    (0xfffffc1f, 0xd61f081f, "BRAAZ", "FEAT_PAuth", Ops::No),
    (0xfffffc1f, 0xd61f0c1f, "BRABZ", "FEAT_PAuth", Ops::No),
    (0xfffffc1f, 0xd63f081f, "BLRAAZ", "FEAT_PAuth", Ops::No),
    (0xfffffc1f, 0xd63f0c1f, "BLRABZ", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xd71f0800, "BRAA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xd71f0c00, "BRAB", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xd73f0800, "BLRAA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xd73f0c00, "BLRAB", "FEAT_PAuth", Ops::No),
    (0xffffffff, 0xd69f0fff, "ERETAB", "FEAT_PAuth", Ops::No),
    (0xfffff3ff, 0xd503323f, "DSB (nXS)", "FEAT_XS", Ops::No),
    (0xfffffc1f, 0xdac1901e, "AUTIASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xfffffc1f, 0xdac1941e, "AUTIBSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xfffffc1f, 0x3a00080d, "SETF8", "FEAT_FlagM", Ops::No),
    (0xfffffc1f, 0x3a00480d, "SETF16", "FEAT_FlagM", Ops::No),
    (0xffffffe0, 0xd65f0be0, "RETAASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffe0, 0xd65f0fe0, "RETABSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffffffe0, 0xd5031000, "WFET", "FEAT_WFxT", Ops::No),
    (0xffffffe0, 0xd5031020, "WFIT", "FEAT_WFxT", Ops::No),
    (0xffffffe0, 0xd5233060, "TSTART", "FEAT_TME", Ops::No),
    (0xffffffe0, 0xd5233160, "TTEST", "FEAT_TME", Ops::No),
    (0xffffffe0, 0xd52b7720, "GCSPOPM", "FEAT_GCS", Ops::No),
    (0xffffffe0, 0xdac123e0, "PACIZA", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac127e0, "PACIZB", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac12be0, "PACDZA", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac12fe0, "PACDZB", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac133e0, "AUTIZA", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac137e0, "AUTIZB", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac13be0, "AUTDZA", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac13fe0, "AUTDZB", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac143e0, "XPACI", "FEAT_PAuth", Ops::No),
    (0xffffffe0, 0xdac147e0, "XPACD", "FEAT_PAuth", Ops::No),
    (0xfffffc01, 0xf83f9000, "ST64B", "FEAT_LS64", Ops::No),
    (0xfffffc01, 0xf83fd000, "LD64B", "FEAT_LS64", Ops::No),
    (0xfffffc00, 0x0ea16800, "BFCVTN", "FEAT_BF16", Ops::No),
    (0xfffffc00, 0x1e634000, "BFCVT", "FEAT_BF16", Ops::No),
    (0xfffffc00, 0x2e217800, "F1CVTL", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x2e617800, "F2CVTL", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x2ea17800, "BF1CVTL", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x2ee17800, "BF2CVTL", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x4ea16800, "BFCVTN2", "FEAT_BF16", Ops::No),
    (0xfffffc00, 0x6e217800, "F1CVTL2", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x6e617800, "F2CVTL2", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x6ea17800, "BF1CVTL2", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0x6ee17800, "BF2CVTL2", "FEAT_FP8", Ops::No),
    (0xfffffc00, 0xcec08400, "SM4E", "FEAT_SM4", Ops::No),
    (0xfffffc00, 0xd91f0c00, "GCSSTR", "FEAT_GCS", Ops::No),
    (0xfffffc00, 0xd91f1c00, "GCSSTTR", "FEAT_GCS", Ops::No),
    (0xfffffc00, 0xd9200000, "STZGM", "FEAT_MTE", Ops::No),
    (0xfffffc00, 0xd9a00000, "STGM", "FEAT_MTE", Ops::No),
    (0xfffffc00, 0xd9e00000, "LDGM", "FEAT_MTE", Ops::No),
    (0xfffffc00, 0xdac10000, "PACIA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac10400, "PACIB", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac10800, "PACDA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac10c00, "PACDB", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac11000, "AUTIA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac11400, "AUTIB", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac11800, "AUTDA", "FEAT_PAuth", Ops::No),
    (0xfffffc00, 0xdac11c00, "AUTDB", "FEAT_PAuth", Ops::No),
    (0xbffffc00, 0x0d018400, "STL1", "FEAT_LRCPC3", Ops::No),
    (0xbffffc00, 0x0d418400, "LDAP1", "FEAT_LRCPC3", Ops::No),
    (0x7ffffc00, 0x5ac01800, "CTZ", "FEAT_CSSC", Ops::No),
    (0x7ffffc00, 0x5ac01c00, "CNT", "FEAT_CSSC", Ops::No),
    (0x7ffffc00, 0x5ac02000, "ABS", "FEAT_CSSC", Ops::No),
    (0xbffffc00, 0x99800800, "STLR", "FEAT_LRCPC3", Ops::No),
    (0xbffffc00, 0x99c00800, "LDAPR", "FEAT_LRCPC3", Ops::No),
    (0xffe0fc01, 0xf820a000, "ST64BV0", "FEAT_LS64_ACCDATA", Ops::No),
    (0xffe0fc01, 0xf820b000, "ST64BV", "FEAT_LS64_V", Ops::No),
    (0xffe0fc00, 0x0e00c400, "FMLALLBB", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x0e40c400, "FMLALLBT", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x0ec0fc00, "FMLALB (FP8)", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x2ec0fc00, "BFMLALB", "FEAT_BF16", Ops::No),
    (0xffe0fc00, 0x4e00c400, "FMLALLTB", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x4e00f400, "FCVTN2 (FP8)", "FEAT_FP8", Ops::No),
    (0xffe0fc00, 0x4e40c400, "FMLALLTT", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x4e80a400, "SMMLA", "FEAT_I8MM", Ops::No),
    (0xffe0fc00, 0x4e80ac00, "USMMLA", "FEAT_I8MM", Ops::No),
    (0xffe0fc00, 0x4ec0fc00, "FMLALT (FP8)", "FEAT_FP8FMA", Ops::No),
    (0xffe0fc00, 0x6e40ec00, "BFMMLA", "FEAT_BF16", Ops::No),
    (0xffe0fc00, 0x6e80a400, "UMMLA", "FEAT_I8MM", Ops::No),
    (0xffe0fc00, 0x6ec0fc00, "BFMLALT", "FEAT_BF16", Ops::No),
    (0xffe0fc00, 0x9ac00000, "SUBP", "FEAT_MTE", Ops::No),
    (0xffe0fc00, 0x9ac01000, "IRG", "FEAT_MTE", Ops::No),
    (0xffe0fc00, 0x9ac01400, "GMI", "FEAT_MTE", Ops::No),
    (0xffe0fc00, 0x9ac03000, "PACGA", "FEAT_PAuth", Ops::No),
    (0xffe07c10, 0xba000400, "RMIF", "FEAT_FlagM", Ops::No),
    (0xffe0fc00, 0xbac00000, "SUBPS", "FEAT_MTE", Ops::No),
    (0xffe0fc00, 0xce60c000, "SM3PARTW1", "FEAT_SM3", Ops::No),
    (0xffe0fc00, 0xce60c400, "SM3PARTW2", "FEAT_SM3", Ops::No),
    (0xffe0fc00, 0xce60c800, "SM4EKEY", "FEAT_SM4", Ops::No),
    (0xbfe0fc00, 0x0e809c00, "USDOT", "FEAT_I8MM", Ops::No),
    (0xbfe0fc00, 0x0ec01c00, "FAMAX", "FEAT_FAMINMAX", Ops::No),
    (0x7fe0fc00, 0x1ac06000, "SMAX", "FEAT_CSSC", Ops::No),
    (0x7fe0fc00, 0x1ac06400, "UMAX", "FEAT_CSSC", Ops::No),
    (0x7fe0fc00, 0x1ac06800, "SMIN", "FEAT_CSSC", Ops::No),
    (0x7fe0fc00, 0x1ac06c00, "UMIN", "FEAT_CSSC", Ops::No),
    (0xbfe0fc00, 0x2e40fc00, "BFDOT", "FEAT_BF16", Ops::No),
    (0xbfe0fc00, 0x2ec01c00, "FAMIN", "FEAT_FAMINMAX", Ops::No),
    (0xbfe0fc00, 0x2ec03c00, "FSCALE", "FEAT_FP8", Ops::No),
    (0xffe0001f, 0x5500001f, "RETAASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffe0001f, 0x5520001f, "RETABSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffe0001f, 0xd4000002, "HVC", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xd4000003, "SMC", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xd4400000, "HLT", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xd4600000, "TCANCEL", "FEAT_TME", Ops::Imm16),
    (0xffe0001f, 0xd4a00001, "DCPS1", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xd4a00002, "DCPS2", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xd4a00003, "DCPS3", "excepcion de EL1+ o de depuracion", Ops::Imm16),
    (0xffe0001f, 0xf380001f, "AUTIASPPC", "FEAT_PAuth_LR", Ops::No),
    (0xffe0001f, 0xf3a0001f, "AUTIBSPPC", "FEAT_PAuth_LR", Ops::No),
    (0xbfa0fc00, 0x0e00f400, "FCVTN (FP8)", "FEAT_FP8", Ops::No),
    (0xbfa0fc00, 0x0e00fc00, "FDOT (FP8)", "FEAT_FP8DOT", Ops::No),
    (0xffe0fc00, 0x0ea0dc00, "FAMAX", "FEAT_FAMINMAX", Ops::No),
    (0xffa0fc00, 0x4ea0dc00, "FAMAX", "FEAT_FAMINMAX", Ops::No),
    (0xffc0f400, 0x0fc00000, "FMLALB (FP8)", "FEAT_FP8FMA", Ops::No),
    (0xffc0f400, 0x0fc0f000, "BFMLALB", "FEAT_BF16", Ops::No),
    (0xffe0fc00, 0x2ea0dc00, "FAMIN", "FEAT_FAMINMAX", Ops::No),
    (0xffa0fc00, 0x6ea0dc00, "FAMIN", "FEAT_FAMINMAX", Ops::No),
    (0xffe0fc00, 0x2ea0fc00, "FSCALE", "FEAT_FP8", Ops::No),
    (0xffa0fc00, 0x6ea0fc00, "FSCALE", "FEAT_FP8", Ops::No),
    (0xffc0f400, 0x2f008000, "FMLALLBB", "FEAT_FP8FMA", Ops::No),
    (0xffc0f400, 0x2f408000, "FMLALLBT", "FEAT_FP8FMA", Ops::No),
    (0xffc0f400, 0x4fc00000, "FMLALT (FP8)", "FEAT_FP8FMA", Ops::No),
    (0xffc0f400, 0x4fc0f000, "BFMLALT", "FEAT_BF16", Ops::No),
    (0xffc0f400, 0x6f008000, "FMLALLTB", "FEAT_FP8FMA", Ops::No),
    (0xffc0f400, 0x6f408000, "FMLALLTT", "FEAT_FP8FMA", Ops::No),
    (0xbfe0ec00, 0x99000800, "STILP", "FEAT_LRCPC3", Ops::No),
    (0xbfe0ec00, 0x99400800, "LDIAPP", "FEAT_LRCPC3", Ops::No),
    (0xffe0cc00, 0xce408000, "SM3TT1A", "FEAT_SM3", Ops::No),
    (0xffe0cc00, 0xce408400, "SM3TT1B", "FEAT_SM3", Ops::No),
    (0xffe0cc00, 0xce408800, "SM3TT2A", "FEAT_SM3", Ops::No),
    (0xffe0cc00, 0xce408c00, "SM3TT2B", "FEAT_SM3", Ops::No),
    (0xbfc0f400, 0x0f00f000, "SUDOT", "FEAT_I8MM", Ops::No),
    (0xbfc0f400, 0x0f40f000, "BFDOT", "FEAT_BF16", Ops::No),
    (0xbfc0f400, 0x0f80f000, "USDOT", "FEAT_I8MM", Ops::No),
    (0xffe09c00, 0x4e401000, "LUTI4", "FEAT_LUT", Ops::No),
    (0xffe0bc00, 0x4e402000, "LUTI4", "FEAT_LUT", Ops::No),
    (0xffe0e000, 0x9a002000, "ADDPT", "FEAT_CPA", Ops::No),
    (0xffe0e000, 0xda002000, "SUBPT", "FEAT_CPA", Ops::No),
    (0xbf80f400, 0x0f000000, "FDOT (FP8)", "FEAT_FP8DOT", Ops::No),
    (0x7ffc0000, 0x11c00000, "SMAX", "FEAT_CSSC", Ops::No),
    (0x7ffc0000, 0x11c40000, "UMAX", "FEAT_CSSC", Ops::No),
    (0x7ffc0000, 0x11c80000, "SMIN", "FEAT_CSSC", Ops::No),
    (0x7ffc0000, 0x11cc0000, "UMIN", "FEAT_CSSC", Ops::No),
    (0xffe00c00, 0x19000000, "STLURB", "FEAT_LRCPC2", Ops::No),
    (0xffe00c00, 0x19400000, "LDAPURB", "FEAT_LRCPC2", Ops::No),
    (0xffa09c00, 0x4e801000, "LUTI2", "FEAT_LUT", Ops::No),
    (0xffe09c00, 0x4ec00000, "LUTI2", "FEAT_LUT", Ops::No),
    (0xffe00c00, 0x59000000, "STLURH", "FEAT_LRCPC2", Ops::No),
    (0xffe00c00, 0x59400000, "LDAPURH", "FEAT_LRCPC2", Ops::No),
    (0xffe00c00, 0x99800000, "LDAPURSW", "FEAT_LRCPC2", Ops::No),
    (0xfff80000, 0xd5280000, "SYSL", "registro de sistema", Ops::SysReg),
    (0xfff80000, 0xd5480000, "SYSP", "FEAT_SYSREG128", Ops::SysReg),
    (0xffe00c00, 0xd9600000, "LDG", "FEAT_MTE", Ops::No),
    (0xffa00c00, 0x19800000, "LDAPURSB", "FEAT_LRCPC2", Ops::No),
    (0xffa00c00, 0x59800000, "LDAPURSH", "FEAT_LRCPC2", Ops::No),
    (0xbfe00c00, 0x99000000, "STLUR", "FEAT_LRCPC2", Ops::No),
    (0xbfe00c00, 0x99400000, "LDAPUR", "FEAT_LRCPC2", Ops::No),
    (0xffe08000, 0x9b400000, "SMULH", "codificacion no canonica", Ops::No),
    (0xffe08000, 0x9b600000, "MADDPT", "FEAT_CPA", Ops::No),
    (0xffe08000, 0x9b608000, "MSUBPT", "FEAT_CPA", Ops::No),
    (0xffe08000, 0x9bc00000, "UMULH", "codificacion no canonica", Ops::No),
    (0xffe08000, 0xce400000, "SM3SS1", "FEAT_SM3", Ops::No),
    (0xffffffe0, 0xd53b2400, "MRS", "FEAT_RNG (RNDR)", Ops::SysReg),
    (0xffffffe0, 0xd53b2420, "MRS", "FEAT_RNG (RNDRRS)", Ops::SysReg),
    (0xffffffe0, 0xd51b4220, "MSR", "DAIF (solo EL1 en Linux)", Ops::SysReg),
    (0xffffffe0, 0xd53b4220, "MRS", "DAIF (solo EL1 en Linux)", Ops::SysReg),
    (0xffffffe0, 0xd51b4240, "MSR", "FEAT_SME (SVCR)", Ops::SysReg),
    (0xffffffe0, 0xd53b4240, "MRS", "FEAT_SME (SVCR)", Ops::SysReg),
    (0xffffffe0, 0xd53be020, "MRS", "CNTPCT_EL0 (Linux no lo da a EL0)", Ops::SysReg),
    (0xffe80000, 0xd5000000, "MSR", "registro de sistema", Ops::SysReg),
    (0xffe80000, 0xd5200000, "MRS", "registro de sistema", Ops::SysReg),
    (0xffe00001, 0xd5400000, "MSRR", "FEAT_SYSREG128", Ops::SysReg),
    (0xffe00001, 0xd5600000, "MRRS", "FEAT_SYSREG128", Ops::SysReg),
    (0xffe00400, 0xd9200400, "STG", "FEAT_MTE", Ops::No),
    (0xffe00c00, 0xd9200800, "STG", "FEAT_MTE", Ops::No),
    (0xffe00400, 0xd9600400, "STZG", "FEAT_MTE", Ops::No),
    (0xffe00c00, 0xd9600800, "STZG", "FEAT_MTE", Ops::No),
    (0xffe00400, 0xd9a00400, "ST2G", "FEAT_MTE", Ops::No),
    (0xffe00c00, 0xd9a00800, "ST2G", "FEAT_MTE", Ops::No),
    (0xffe00400, 0xd9e00400, "STZ2G", "FEAT_MTE", Ops::No),
    (0xffe00c00, 0xd9e00800, "STZ2G", "FEAT_MTE", Ops::No),
    (0xffa00400, 0xf8200400, "LDRAA", "FEAT_PAuth", Ops::No),
    (0xffa00400, 0xf8a00400, "LDRAB", "FEAT_PAuth", Ops::No),
    (0x3fe00c00, 0x1d000800, "STLUR", "FEAT_LRCPC3", Ops::No),
    (0xffe00c00, 0x1d800800, "STLUR", "FEAT_LRCPC3", Ops::No),
    (0x3fe00c00, 0x1d400800, "LDAPUR", "FEAT_LRCPC3", Ops::No),
    (0xffe00c00, 0x1dc00800, "LDAPUR", "FEAT_LRCPC3", Ops::No),
    (0xffc00000, 0x91800000, "ADDG", "FEAT_MTE", Ops::No),
    (0xffc00000, 0xd1800000, "SUBG", "FEAT_MTE", Ops::No),
    (0xffc00000, 0x68800000, "STGP", "FEAT_MTE", Ops::No),
    (0xffc00000, 0x69000000, "STGP", "FEAT_MTE", Ops::No),
    (0xffc00000, 0x69800000, "STGP", "FEAT_MTE", Ops::No),
    (0xfff80000, 0xd5080000, "SYS", "operacion de sistema no accesible en EL0", Ops::SysReg),
    (0xfff80000, 0xd5180000, "MSR", "registro de sistema no accesible en EL0", Ops::SysReg),
    (0xfff80000, 0xd5380000, "MRS", "registro de sistema no accesible en EL0", Ops::SysReg),
];

/// Nombre de la llamada al sistema arm64 `n` ("?" si no existe en la numeracion generica).
pub fn syscall_name(n: u64) -> &'static str {
    match SYSCALLS.get(n as usize) {
        Some(s) if !s.is_empty() => s,
        _ => "?",
    }
}

/// Numeracion generica de Linux (`asm-generic/unistd.h`), la de arm64, hasta 467 (`open_tree_attr`).
#[rustfmt::skip]
const SYSCALLS: [&str; 468] = [
    "io_setup", "io_destroy", "io_submit", "io_cancel", "io_getevents", "setxattr", "lsetxattr", "fsetxattr",
    "getxattr", "lgetxattr", "fgetxattr", "listxattr", "llistxattr", "flistxattr", "removexattr", "lremovexattr",
    "fremovexattr", "getcwd", "lookup_dcookie", "eventfd2", "epoll_create1", "epoll_ctl", "epoll_pwait", "dup",
    "dup3", "fcntl", "inotify_init1", "inotify_add_watch", "inotify_rm_watch", "ioctl", "ioprio_set", "ioprio_get",
    "flock", "mknodat", "mkdirat", "unlinkat", "symlinkat", "linkat", "renameat", "umount2", "mount", "pivot_root",
    "nfsservctl", "statfs", "fstatfs", "truncate", "ftruncate", "fallocate", "faccessat", "chdir", "fchdir", "chroot",
    "fchmod", "fchmodat", "fchownat", "fchown", "openat", "close", "vhangup", "pipe2", "quotactl", "getdents64",
    "lseek", "read", "write", "readv", "writev", "pread64", "pwrite64", "preadv", "pwritev", "sendfile", "pselect6",
    "ppoll", "signalfd4", "vmsplice", "splice", "tee", "readlinkat", "newfstatat", "fstat", "sync", "fsync",
    "fdatasync", "sync_file_range", "timerfd_create", "timerfd_settime", "timerfd_gettime", "utimensat", "acct",
    "capget", "capset", "personality", "exit", "exit_group", "waitid", "set_tid_address", "unshare", "futex",
    "set_robust_list", "get_robust_list", "nanosleep", "getitimer", "setitimer", "kexec_load", "init_module",
    "delete_module", "timer_create", "timer_gettime", "timer_getoverrun", "timer_settime", "timer_delete",
    "clock_settime", "clock_gettime", "clock_getres", "clock_nanosleep", "syslog", "ptrace", "sched_setparam",
    "sched_setscheduler", "sched_getscheduler", "sched_getparam", "sched_setaffinity", "sched_getaffinity",
    "sched_yield", "sched_get_priority_max", "sched_get_priority_min", "sched_rr_get_interval", "restart_syscall",
    "kill", "tkill", "tgkill", "sigaltstack", "rt_sigsuspend", "rt_sigaction", "rt_sigprocmask", "rt_sigpending",
    "rt_sigtimedwait", "rt_sigqueueinfo", "rt_sigreturn", "setpriority", "getpriority", "reboot", "setregid",
    "setgid", "setreuid", "setuid", "setresuid", "getresuid", "setresgid", "getresgid", "setfsuid", "setfsgid",
    "times", "setpgid", "getpgid", "getsid", "setsid", "getgroups", "setgroups", "uname", "sethostname",
    "setdomainname", "getrlimit", "setrlimit", "getrusage", "umask", "prctl", "getcpu", "gettimeofday",
    "settimeofday", "adjtimex", "getpid", "getppid", "getuid", "geteuid", "getgid", "getegid", "gettid", "sysinfo",
    "mq_open", "mq_unlink", "mq_timedsend", "mq_timedreceive", "mq_notify", "mq_getsetattr", "msgget", "msgctl",
    "msgrcv", "msgsnd", "semget", "semctl", "semtimedop", "semop", "shmget", "shmctl", "shmat", "shmdt", "socket",
    "socketpair", "bind", "listen", "accept", "connect", "getsockname", "getpeername", "sendto", "recvfrom",
    "setsockopt", "getsockopt", "shutdown", "sendmsg", "recvmsg", "readahead", "brk", "munmap", "mremap", "add_key",
    "request_key", "keyctl", "clone", "execve", "mmap", "fadvise64", "swapon", "swapoff", "mprotect", "msync",
    "mlock", "munlock", "mlockall", "munlockall", "mincore", "madvise", "remap_file_pages", "mbind", "get_mempolicy",
    "set_mempolicy", "migrate_pages", "move_pages", "rt_tgsigqueueinfo", "perf_event_open", "accept4", "recvmmsg", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "wait4", "prlimit64", "fanotify_init",
    "fanotify_mark", "name_to_handle_at", "open_by_handle_at", "clock_adjtime", "syncfs", "setns", "sendmmsg",
    "process_vm_readv", "process_vm_writev", "kcmp", "finit_module", "sched_setattr", "sched_getattr", "renameat2",
    "seccomp", "getrandom", "memfd_create", "bpf", "execveat", "userfaultfd", "membarrier", "mlock2",
    "copy_file_range", "preadv2", "pwritev2", "pkey_mprotect", "pkey_alloc", "pkey_free", "statx", "io_pgetevents",
    "rseq", "kexec_file_load", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "pidfd_send_signal",
    "io_uring_setup", "io_uring_enter", "io_uring_register", "open_tree", "move_mount", "fsopen", "fsconfig",
    "fsmount", "fspick", "pidfd_open", "clone3", "close_range", "openat2", "pidfd_getfd", "faccessat2",
    "process_madvise", "epoll_pwait2", "mount_setattr", "quotactl_fd", "landlock_create_ruleset", "landlock_add_rule",
    "landlock_restrict_self", "memfd_secret", "process_mrelease", "futex_waitv", "set_mempolicy_home_node",
    "cachestat", "fchmodat2", "map_shadow_stack", "futex_wake", "futex_wait", "futex_requeue", "statmount",
    "listmount", "lsm_get_self_attr", "lsm_set_self_attr", "lsm_list_modules", "mseal", "setxattrat", "getxattrat",
    "listxattrat", "removexattrat", "open_tree_attr",
];

/// Texto con tope fijo en la pila, para formatear un mensaje sin reservar memoria (lo que no cabe se descarta).
pub struct Buf {
    b: [u8; 320],
    n: usize,
}

impl Buf {
    pub const fn new() -> Self {
        Buf { b: [0; 320], n: 0 }
    }
    pub fn as_str(&self) -> &str {
        // solo se copian cadenas UTF-8 completas o recortadas en un limite de caracter
        std::str::from_utf8(&self.b[..self.n]).unwrap_or("")
    }
}

impl Default for Buf {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut k = s.len().min(self.b.len() - self.n);
        while !s.is_char_boundary(k) {
            k -= 1;
        }
        self.b[self.n..self.n + k].copy_from_slice(&s.as_bytes()[..k]);
        self.n += k;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    fn name(w: u32) -> String {
        classify(w).mnemonic().to_string()
    }

    /// Palabras de referencia (codificadas con llvm-mc -mattr=+all).
    #[test]
    fn nombra_extensiones_conocidas() {
        let casos: &[(u32, &str, &str)] = &[
            (0xcec08400, "SM4E", "FEAT_SM4"),
            (0xce48415b, "SM3SS1", "FEAT_SM3"),
            (0x4e82a420, "SMMLA", "FEAT_I8MM"),
            (0x6e42fc20, "BFDOT", "FEAT_BF16"),
            (0x1e634020, "BFCVT", "FEAT_BF16"),
            (0x4ea2dc20, "FAMAX", "FEAT_FAMINMAX"),
            (0x4e821020, "LUTI2", "FEAT_LUT"),
            (0xdac02020, "ABS", "FEAT_CSSC"),
            (0x11cc0c20, "UMIN", "FEAT_CSSC"),
            (0x99404020, "LDAPUR", "FEAT_LRCPC2"),
            (0x1dc00800, "LDAPUR", "FEAT_LRCPC3"),
            (0xd9411840, "LDIAPP", "FEAT_LRCPC3"),
            (0x690b0f87, "STGP", "FEAT_MTE"),
            (0x9adf1041, "IRG", "FEAT_MTE"),
            (0xf8200441, "LDRAA", "FEAT_PAuth"),
            (0xdac143e1, "XPACI", "FEAT_PAuth"),
            (0xdac1901e, "AUTIASPPC", "FEAT_PAuth_LR"),
            (0xd5031001, "WFET", "FEAT_WFxT"),
            (0xd503323f, "DSB (nXS)", "FEAT_XS"),
            (0xd5233063, "TSTART", "FEAT_TME"),
            (0xf83fd020, "LD64B", "FEAT_LS64"),
            (0x3a00082d, "SETF8", "FEAT_FlagM"),
            (0x9a173dea, "ADDPT", "FEAT_CPA"),
            (0x1d04055e, "CPYP", "FEAT_MOPS"),
            (0x1909778a, "CPYFPTWN", "FEAT_MOPS"),
            (0x19c01722, "SETPT", "FEAT_MOPS"),
            (0x1dd0b587, "SETGETN", "FEAT_MOPS"),
            (0x59f40f30, "RCWSCASPAL", "FEAT_THE"),
            (0x38f3936a, "RCWCLRAL", "FEAT_THE"),
            (0x19eb1379, "LDCLRPAL", "FEAT_LSE128"),
            (0x193d8170, "SWPP", "FEAT_LSE128"),
            (0x5408c4f0, "BC.EQ", "FEAT_HBC"),
            (0x88d8034b, "LDLAR", "FEAT_LOR"),
            (0x08800000, "STLLRB", "FEAT_LOR"),
            (0xd56eb4d8, "MRRS S1_6_C11_C4_6", "FEAT_SYSREG128"),
            (0xd4098b62, "HVC #0x4c5b", "excepcion de EL1+ o de depuracion"),
            // sistema: lo que EL0 no puede usar o heddle no anuncia (SIGILL, no NOP ni 0)
            (0xd503477f, "SMSTART", "FEAT_SME (no soportado)"),
            (0xd503415f, "MSR DIT, #imm", "FEAT_DIT"),
            (0xd503409f, "MSR TCO, #imm", "FEAT_MTE"),
            (0xd50346df, "MSR DAIFSet, #imm", "solo EL1 en Linux (SCTLR_EL1.UMA=0)"),
            (0xd50040bf, "MSR SPSel, #imm", "solo EL1 (UNDEFINED en EL0)"),
            (0xd53b2400, "MRS S3_3_C2_C4_0", "FEAT_RNG (RNDR)"),
            (0xd53be020, "MRS S3_3_C14_C0_1", "CNTPCT_EL0 (Linux no lo da a EL0)"),
            (0xd5380100, "MRS S3_0_C0_C1_0", "registro de sistema no accesible en EL0"),
            (0xd51bd063, "MSR S3_3_C13_C0_3", "registro de sistema no accesible en EL0"),
            (0xd508751f, "IC IALLU", "solo EL1 (UNDEFINED en EL0)"),
            (0xd50b7c20, "DC CVAP", "FEAT_DPB"),
        ];
        for &(w, n, f) in casos {
            let i = classify(w);
            assert_eq!((i.verdict, name(w).as_str(), i.feat), (Verdict::NoSoportada, n, f), "{:#010x}", w);
        }
    }

    #[test]
    fn exclusivas_no_canonicas() {
        // LDXR x4, [x12] con Rs != 11111; LDARH con Rs/Rt2 distintos; CASALB con Rt2 != 11111
        for (w, n) in [(0xc8581984u32, "LDXR"), (0x48dce01c, "LDARH"), (0x08e4d169, "CASALB"), (0x0872c004, "CASPAL"), (0x8869b26d, "LDAXP")] {
            let i = classify(w);
            assert_eq!((name(w).as_str(), i.feat), (n, NO_CANONICA), "{:#010x}", w);
        }
    }

    #[test]
    fn grupos_y_codificaciones_indefinidas() {
        assert_eq!(classify(0x0420_0000).mnemonic().to_string(), "SVE");
        assert_eq!(classify(0x0420_0000).class, "SVE");
        assert_eq!(classify(0xc080_0000).mnemonic().to_string(), "SME");
        assert_eq!(classify(0xc080_0000).class, "SME");
        let u = classify(0);
        assert_eq!((u.verdict, name(0).as_str()), (Verdict::Indefinida, "UDF #0x0"));
        assert_eq!(classify(0x0000_4115).verdict, Verdict::Indefinida);
        for w in [0x0001_0000u32, 0x0200_0000, 0x0600_0000, 0x8600_0000] {
            assert_eq!(classify(w).verdict, Verdict::Indefinida, "{:#010x}", w);
        }
        // grupo asignado sin fila en la tabla
        assert_eq!(classify(0x02a4_9467 | 0x0800_0000).verdict, Verdict::NoReconocida);
    }

    /// Cada fila de la tabla es alcanzable: ninguna fila anterior ni familia calculada la tapa.
    #[test]
    fn filas_alcanzables() {
        for &(mask, val, n, f, _) in T {
            assert_eq!(val & !mask, 0, "{}: valor fuera de la mascara", n);
            let i = classify(val);
            assert_eq!((i.name[0], i.feat), (n, f), "{:#010x}", val);
        }
    }

    #[test]
    fn mensaje_sin_reservas() {
        let mut b = Buf::new();
        write!(b, "{} en pc=0x1000", classify(0xcec08400)).unwrap();
        assert_eq!(b.as_str(), "instruccion no soportada por heddle: SM4E (0xcec08400) [FEAT_SM4; SIMD y FP] en pc=0x1000");
        let mut b = Buf::new();
        write!(b, "{}", classify(0x0200_0000)).unwrap();
        assert_eq!(b.as_str(), "instruccion indefinida en ARMv8/ARMv9 (0x02000000) [grupo no asignado]");
        let mut b = Buf::new();
        write!(b, "{}", classify(0)).unwrap();
        assert_eq!(b.as_str(), "instruccion indefinida en ARMv8/ARMv9: UDF #0x0 (0x00000000) [permanentemente indefinida]");
        // el bufer recorta sin pasarse ni partir un caracter
        let mut b = Buf::new();
        for _ in 0..400 {
            b.write_str("n").unwrap();
        }
        assert_eq!(b.as_str().len(), 320);
        let mut b = Buf::new();
        for _ in 0..319 {
            b.write_str("n").unwrap();
        }
        b.write_str("ñ").unwrap();
        assert_eq!(b.as_str().len(), 319);
    }

    #[test]
    fn nombres_de_llamadas_al_sistema() {
        assert_eq!(syscall_name(56), "openat");
        assert_eq!(syscall_name(63), "read");
        assert_eq!(syscall_name(93), "exit");
        assert_eq!(syscall_name(222), "mmap");
        assert_eq!(syscall_name(293), "rseq");
        assert_eq!(syscall_name(435), "clone3");
        assert_eq!(syscall_name(462), "mseal");
        assert_eq!(syscall_name(250), "?"); // reservado a la arquitectura
        assert_eq!(syscall_name(1 << 40), "?");
    }
}
