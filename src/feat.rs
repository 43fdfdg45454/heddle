//! Lo que el guest VE de la CPU: unica fuente para los registros de identificacion que lee `MRS` (con la emulacion y
//! el saneado de Linux arm64 en EL0), `getauxval(AT_HWCAP/AT_HWCAP2)`, `/proc/cpuinfo`, `/proc/self/auxv`,
//! `midr_el1`/`revidr_el1` de sysfs, `CTR_EL0`/`DCZID_EL0` y el bit SSBS del `pstate` de los marcos de senal.
//!
//! **Se ejecuta lo anunciado y implementado** (`need`): una instruccion de una extension que el modelo o el perfil no
//! anuncia es SIGILL, como en un procesador ARM sin ella (UNDEFINED en el Arm ARM), aunque heddle la implemente; una
//! anunciada que heddle no implementa (`EXEC`) tambien es SIGILL (con un aviso en el registro). La interseccion se
//! resuelve una vez al iniciar (`State::exec`); el diagnostico distingue "ausente en el modelo" (`probe_all`) de "no
//! soportada por heddle".
//!
//! De donde sale lo anunciado, una vez al iniciar (sin coste despues):
//! 1. `HEDDLE_CPU` / `debug.heddle.cpu`: un modelo del catalogo (`MODELS`), con los valores reales de su manual (TRM)
//!    limitados campo a campo a lo que heddle implementa entero (`IMPL`); lo que el procesador tiene y heddle no, no
//!    se anuncia (se registra).
//! 2. Si no, un perfil (`HEDDLE_CPU_FILE` / `debug.heddle.cpu_file`; por defecto `/system/etc/heddle/cpu.conf`,
//!    `PROFILE_PATH`): se anuncia **exactamente** lo que dice, aunque heddle no lo implemente (se avisa una vez en
//!    el registro). Formato y precedencia: `docs/perfil-cpu.md` (contrato estable con weft) y `parse_profile`.
//! 3. Si no, Cortex-A78 (`DEFAULT`).
//!
//! Lo que Linux hace en EL0 (arch/arm64/kernel/cpufeature.c, `emulate_mrs`; se sigue Linux 6.1/6.6, los nucleos GKI
//! actuales): un `MRS` del espacio de identificacion (op0=3, op1=0, CRn=0, CRm=0 o 4..7) atrapa y el nucleo lo emula.
//! CRm=0: MIDR_EL1 (el del procesador), MPIDR_EL1 (valor seguro, bit 31) y REVIDR_EL1 (0); el resto de CRm=0 es
//! SIGILL. CRm=4..7: el valor saneado para el usuario (solo los campos `FTR_VISIBLE`; los ocultos con su valor seguro)
//! o 0 si el registro no se sigue. Cualquier otro registro de sistema inaccesible en EL0 da SIGILL. Los HWCAP se
//! derivan de esos registros como en `arm64_elf_hwcaps` (tabla `CAPS`).

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::OnceLock;

/// Valor de un campo de 4 bits en la posicion `shift`.
const fn f(v: u64, shift: u32) -> u64 {
    v << shift
}

/// Campo de 4 bits de `reg` en `shift` (sin signo).
pub const fn field(reg: u64, shift: u32) -> u64 {
    (reg >> shift) & 0xF
}

fn set_field(reg: &mut u64, shift: u32, v: u64) {
    *reg = (*reg & !(0xF << shift)) | ((v & 0xF) << shift);
}

/// Registros de identificacion AArch64 que Linux emula en EL0 (los que tienen campos visibles o fijos).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regs {
    pub pfr0: u64,
    pub pfr1: u64,
    pub zfr0: u64,
    pub dfr0: u64,
    pub isar0: u64,
    pub isar1: u64,
    pub isar2: u64,
    pub mmfr0: u64,
    pub mmfr1: u64,
    pub mmfr2: u64,
}

/// Registro de `Regs` (para las tablas).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum R {
    Pfr0,
    Pfr1,
    Zfr0,
    Dfr0,
    Isar0,
    Isar1,
    Isar2,
    Mmfr0,
    Mmfr1,
    Mmfr2,
}

impl Regs {
    pub fn get(&self, r: R) -> u64 {
        match r {
            R::Pfr0 => self.pfr0,
            R::Pfr1 => self.pfr1,
            R::Zfr0 => self.zfr0,
            R::Dfr0 => self.dfr0,
            R::Isar0 => self.isar0,
            R::Isar1 => self.isar1,
            R::Isar2 => self.isar2,
            R::Mmfr0 => self.mmfr0,
            R::Mmfr1 => self.mmfr1,
            R::Mmfr2 => self.mmfr2,
        }
    }
    pub fn get_mut(&mut self, r: R) -> &mut u64 {
        match r {
            R::Pfr0 => &mut self.pfr0,
            R::Pfr1 => &mut self.pfr1,
            R::Zfr0 => &mut self.zfr0,
            R::Dfr0 => &mut self.dfr0,
            R::Isar0 => &mut self.isar0,
            R::Isar1 => &mut self.isar1,
            R::Isar2 => &mut self.isar2,
            R::Mmfr0 => &mut self.mmfr0,
            R::Mmfr1 => &mut self.mmfr1,
            R::Mmfr2 => &mut self.mmfr2,
        }
    }
}

/// Nombre de cada registro en el perfil (`id_aa64isar0=...`; tambien con el sufijo `_el1`).
pub const REG_KEYS: [(&str, R); 10] = [
    ("id_aa64pfr0", R::Pfr0),
    ("id_aa64pfr1", R::Pfr1),
    ("id_aa64zfr0", R::Zfr0),
    ("id_aa64dfr0", R::Dfr0),
    ("id_aa64isar0", R::Isar0),
    ("id_aa64isar1", R::Isar1),
    ("id_aa64isar2", R::Isar2),
    ("id_aa64mmfr0", R::Mmfr0),
    ("id_aa64mmfr1", R::Mmfr1),
    ("id_aa64mmfr2", R::Mmfr2),
];

/// Un procesador ARM64 tal como lo ve un proceso en Linux.
#[derive(Debug)]
pub struct Model {
    /// nombre para `HEDDLE_CPU` / `debug.heddle.cpu` y `base=` del perfil (en minusculas)
    pub id: &'static str,
    /// nombre legible
    pub name: &'static str,
    /// MIDR_EL1: implementador [31:24], variante [23:20], arquitectura [19:16] (0xF), parte [15:4], revision [3:0]
    pub midr: u64,
    /// CTR_EL0 (lo lee el usuario directamente: no lo emula el nucleo)
    pub ctr: u64,
    /// DCZID_EL0 (DC ZVA: log2 del bloque en palabras)
    pub dczid: u64,
    /// registros de identificacion del manual (solo importan los campos visibles; el saneado los recorta)
    pub hw: Regs,
    /// de donde salen los valores
    pub source: &'static str,
}

/// Registros "crudos" a partir de los campos que importan; el resto, 0.
const fn regs(pfr0: u64, pfr1: u64, isar0: u64, isar1: u64, mmfr2: u64) -> Regs {
    Regs { pfr0, pfr1, zfr0: 0, dfr0: 0, isar0, isar1, isar2: 0, mmfr0: 0, mmfr1: 0, mmfr2 }
}

/// ISAR0 de un nucleo ARMv8.2 con criptografia, RDM, LSE y DotProd (A55, A76, A77, A78, X1).
const ISAR0_V82: u64 = 0x0000_1000_1021_1120;
/// ISAR1 de esos nucleos: DPB (DC CVAP) y LRCPC (LDAPR).
const ISAR1_V82: u64 = 0x0000_0000_0010_0001;
/// PFR0 (campos visibles): FP y AdvSIMD con media precision (1), sin DIT ni SVE.
const PFR0_V82: u64 = f(1, 16) | f(1, 20);
/// PFR1: PSTATE.SSBS (1: FEAT_SSBS, con MSR SSBS inmediato; sin el registro SSBS por MRS/MSR, que es SSBS2).
const PFR1_SSBS: u64 = f(1, 4);

/// Modelos disponibles. El primero que coincide con el nombre pedido; por defecto `DEFAULT`.
pub static MODELS: &[Model] = &[
    Model {
        id: "cortex-a53",
        name: "Cortex-A53",
        midr: 0x410F_D034, // r0p4
        ctr: 0x8444_8004,
        dczid: 4,
        // ARMv8.0 con criptografia y CRC32: sin LSE, FP16, RDM, LRCPC ni DotProd
        hw: regs(0, 0, 0x0001_1120, 0, 0),
        source: "TRM de Cortex-A53 r0p4 (valores de QEMU `cortex-a53`)",
    },
    Model {
        id: "cortex-a55",
        name: "Cortex-A55",
        midr: 0x412F_D050, // r2p0
        ctr: 0x8444_8004,
        dczid: 4,
        hw: regs(PFR0_V82, PFR1_SSBS, ISAR0_V82, ISAR1_V82, 0),
        source: "TRM de Cortex-A55 r2p0 (valores de QEMU `cortex-a55`)",
    },
    Model {
        id: "cortex-a76",
        name: "Cortex-A76",
        midr: 0x414F_D0B1, // r4p1
        ctr: 0x8444_C004,
        dczid: 4,
        hw: regs(PFR0_V82, PFR1_SSBS, ISAR0_V82, ISAR1_V82, 0),
        source: "TRM de Cortex-A76 r4p1 (valores de QEMU `cortex-a76`)",
    },
    Model {
        id: "cortex-a77",
        name: "Cortex-A77",
        midr: 0x411F_D0D0, // r1p0
        ctr: 0x8444_C004,
        dczid: 4,
        hw: regs(PFR0_V82, PFR1_SSBS, ISAR0_V82, ISAR1_V82, 0),
        source: "TRM de Cortex-A77 (ARMv8.2 + LDAPR + DotProd + SSBS, como A76); CTR_EL0 como A76",
    },
    Model {
        id: "cortex-a78",
        name: "Cortex-A78",
        midr: 0x411F_D411, // r1p1
        ctr: 0x9444_C004,
        dczid: 4,
        hw: regs(PFR0_V82, PFR1_SSBS, ISAR0_V82, ISAR1_V82, 0),
        source: "TRM de Cortex-A78 (ARMv8.2 + LDAPR + DotProd + SSBS); CTR_EL0 de Cortex-A78AE (QEMU `cortex-a78ae`)",
    },
    Model {
        id: "cortex-x1",
        name: "Cortex-X1",
        midr: 0x411F_D440, // r1p0
        ctr: 0x9444_C004,
        dczid: 4,
        hw: regs(PFR0_V82, PFR1_SSBS, ISAR0_V82, ISAR1_V82, 0),
        source: "TRM de Cortex-X1 (mismo repertorio que Cortex-A78); CTR_EL0 como Cortex-A78",
    },
    Model {
        id: "max",
        name: "heddle max",
        // implementador 0x00: "reservado para uso de software" (no es un procesador real)
        midr: 0x000F_0000,
        ctr: 0x8444_C004,
        dczid: 4,
        hw: IMPL,
        source: "solo para pruebas: todo lo que heddle implementa (FHM, SHA3, SHA512, FlagM2, JSCVT, FCMA, FRINTTS, SB...)",
    },
];

/// Modelo por defecto (`MODELS[DEFAULT]`): Cortex-A78.
pub const DEFAULT: usize = 4;
/// Modelo de pruebas (`max`): anuncia todo lo que implementa heddle, sin corresponder a ningun procesador. Es el de
/// las pruebas unitarias y el de `heddle-difftest` (oraculo `-cpu max`).
pub const MAX: usize = 6;

/// Lo que heddle implementa entero en interprete y JIT: maximo anunciable de cada campo visible con un modelo del
/// catalogo. Un campo a 0 es una extension que heddle no tiene (SVE, DIT, SM3/SM4, PAuth, BF16, I8MM, LRCPC2, BTI,
/// MTE, SSBS2, RNDR, LSE2...).
pub const IMPL: Regs = Regs {
    pfr0: f(1, 16) | f(1, 20),
    pfr1: f(2, 4),
    zfr0: 0,
    dfr0: 0,
    // AES+PMULL, SHA1, SHA2+SHA512, CRC32, LSE, RDM, SHA3, DotProd, FHM, FlagM2
    isar0: f(2, 4) | f(1, 8) | f(2, 12) | f(1, 16) | f(2, 20) | f(1, 28) | f(1, 32) | f(1, 44) | f(1, 48) | f(2, 52),
    // DPB2 (DC CVAP/CVADP), JSCVT, FCMA, LRCPC, FRINTTS, SB, DGH (pista: NOP)
    isar1: f(2, 0) | f(1, 12) | f(1, 16) | f(1, 20) | f(1, 32) | f(1, 36) | f(1, 48),
    isar2: 0,
    mmfr0: 0,
    mmfr1: 0,
    mmfr2: 0,
};

/// Campos visibles en EL0 (Linux 6.6 `ftr_id_aa64*`, `FTR_VISIBLE` y los `FTR_VISIBLE_IF_IS_ENABLED` de GKI: SVE,
/// SME, MTE, BTI, PTR_AUTH): (registro, desplazamientos).
const VISIBLE: Regs = Regs {
    // FP, AdvSIMD, SVE, DIT
    pfr0: mask(&[16, 20, 32, 48]),
    // BT, SSBS, MTE, SME
    pfr1: mask(&[0, 4, 8, 24]),
    // SVEver, AES, BitPerm, BF16, SHA3, SM4, I8MM, F32MM, F64MM
    zfr0: mask(&[0, 4, 16, 20, 32, 40, 44, 52, 56]),
    dfr0: 0,
    // AES, SHA1, SHA2, CRC32, ATOMIC, RDM, SHA3, SM3, SM4, DP, FHM, TS, RNDR
    isar0: mask(&[4, 8, 12, 16, 20, 28, 32, 36, 40, 44, 48, 52, 60]),
    // DPB, APA, API, JSCVT, FCMA, LRCPC, GPA, GPI, FRINTTS, SB, BF16, DGH, I8MM
    isar1: mask(&[0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 44, 48, 52]),
    // WFxT, RPRES, GPA3, APA3, MOPS, BC, RPRFM, CSSC
    isar2: mask(&[0, 4, 8, 12, 16, 20, 48, 52]),
    // ECV
    mmfr0: mask(&[60]),
    // AFP
    mmfr1: mask(&[44]),
    // AT
    mmfr2: mask(&[32]),
};

/// Campos ocultos con valor seguro distinto de 0 (se leen siempre asi en EL0): PFR0 EL0/EL1 solo AArch64 (1);
/// DFR0 DebugVer (6); MMFR0 TGRAN4_2/TGRAN64_2/TGRAN16_2 (1, desde Linux 5.10) y TGRAN4/TGRAN64 "no implementado"
/// (0xF).
const HIDDEN_SAFE: Regs = Regs {
    pfr0: f(1, 0) | f(1, 4),
    pfr1: 0,
    zfr0: 0,
    dfr0: 6,
    isar0: 0,
    isar1: 0,
    isar2: 0,
    mmfr0: f(1, 32) | f(1, 36) | f(1, 40) | f(0xF, 24) | f(0xF, 28),
    mmfr1: 0,
    mmfr2: 0,
};

const fn mask(shifts: &[u32]) -> u64 {
    let mut m = 0;
    let mut i = 0;
    while i < shifts.len() {
        m |= 0xF << shifts[i];
        i += 1;
    }
    m
}

/// Aplica `op` campo a campo a los diez registros.
fn zip(a: &Regs, b: &Regs, op: impl Fn(u64, u64) -> u64) -> Regs {
    Regs {
        pfr0: op(a.pfr0, b.pfr0),
        pfr1: op(a.pfr1, b.pfr1),
        zfr0: op(a.zfr0, b.zfr0),
        dfr0: op(a.dfr0, b.dfr0),
        isar0: op(a.isar0, b.isar0),
        isar1: op(a.isar1, b.isar1),
        isar2: op(a.isar2, b.isar2),
        mmfr0: op(a.mmfr0, b.mmfr0),
        mmfr1: op(a.mmfr1, b.mmfr1),
        mmfr2: op(a.mmfr2, b.mmfr2),
    }
}

/// Minimo campo a campo (nibbles sin signo) de `a` y `b`.
fn min_fields(a: u64, b: u64) -> u64 {
    (0..16).map(|i| field(a, i * 4).min(field(b, i * 4)) << (i * 4)).fold(0, |x, y| x | y)
}

/// Saneado de Linux en EL0: los campos visibles tal cual, los ocultos con su valor seguro. Idempotente: se puede
/// aplicar a valores del manual o a los leidos con `MRS` en un dispositivo.
pub fn sanitize(r: &Regs) -> Regs {
    let vis = zip(r, &VISIBLE, |h, m| h & m);
    zip(&vis, &HIDDEN_SAFE, |v, s| v | s)
}

impl Model {
    /// Registros tal como los leeria un proceso en Linux sobre este procesador (sin limitar a lo de heddle).
    pub fn user_view(&self) -> Regs {
        sanitize(&self.hw)
    }

    /// Registros que ve el guest con este modelo del catalogo: los del procesador limitados a lo que heddle
    /// implementa.
    pub fn announced(&self) -> Regs {
        zip(&self.user_view(), &sanitize(&IMPL), min_fields)
    }

    /// Nombre (`/proc/cpuinfo`) de las extensiones del procesador que heddle no anuncia (no las implementa entero).
    pub fn missing(&self) -> Vec<&'static str> {
        let (h1, h2) = hwcaps_of(&self.user_view());
        let (a1, a2) = hwcaps_of(&self.announced());
        cap_names(h1 & !a1, h2 & !a2)
    }

    /// Valor de un registro del espacio de identificacion tal como lo emula Linux, o None si el nucleo da SIGILL.
    pub fn id_reg(&self, sysreg: u16) -> Option<u64> {
        id_reg_of(self.midr, &self.announced(), sysreg)
    }

    /// (AT_HWCAP, AT_HWCAP2) que entrega Linux con estos registros.
    pub fn hwcaps(&self) -> (u64, u64) {
        hwcaps_of(&self.announced())
    }

    /// Revision legible: `r1p1`.
    pub fn revision(&self) -> String {
        revision(self.midr)
    }
}

/// `r<variante>p<revision>` de un MIDR.
pub fn revision(midr: u64) -> String {
    format!("r{}p{}", (midr >> 20) & 0xF, midr & 0xF)
}

/// Valor de un registro del espacio de identificacion con MIDR `midr` y los registros saneados `r`.
fn id_reg_of(midr: u64, r: &Regs, sysreg: u16) -> Option<u64> {
    let (op0, op1, crn, crm, op2) = (sysreg >> 14, (sysreg >> 11) & 7, (sysreg >> 7) & 15, (sysreg >> 3) & 15, sysreg & 7);
    if op0 != 3 || op1 != 0 || crn != 0 {
        return None;
    }
    Some(match (crm, op2) {
        (0, 0) => midr,
        (0, 5) => MPIDR_EL1,
        (0, 6) => 0, // REVIDR_EL1: definido por la implementacion, Linux lo emula con 0
        (0, _) => return None,
        (4, 0) => r.pfr0,
        (4, 1) => r.pfr1,
        (4, 4) => r.zfr0,
        (5, 0) => r.dfr0,
        (6, 0) => r.isar0,
        (6, 1) => r.isar1,
        (6, 2) => r.isar2,
        (7, 0) => r.mmfr0,
        (7, 1) => r.mmfr1,
        (7, 2) => r.mmfr2,
        // SMFR0, DFR1, AFR0/1, MMFR3/4, PFR2, ISAR3, FPFR0 y los reservados: 0
        (4..=7, _) => 0,
        _ => return None,
    })
}

/// MPIDR_EL1 tal como lo emula Linux (`SYS_MPIDR_SAFE_VAL`: solo el bit 31, RES1).
pub const MPIDR_EL1: u64 = 1 << 31;

// --- extensiones del decodificador ----------------------------------------------------------------------------------

pub const F_FP16: u64 = 1 << 0; // FPHP: aritmetica escalar de media precision
pub const F_ASIMDHP: u64 = 1 << 1; // ASIMDHP: aritmetica vectorial de media precision
pub const F_AES: u64 = 1 << 2;
pub const F_PMULL: u64 = 1 << 3; // PMULL/PMULL2 de 64 bits
pub const F_SHA1: u64 = 1 << 4;
pub const F_SHA256: u64 = 1 << 5;
pub const F_SHA512: u64 = 1 << 6;
pub const F_SHA3: u64 = 1 << 7;
pub const F_CRC32: u64 = 1 << 8;
pub const F_LSE: u64 = 1 << 9; // atomicas LSE, CAS, CASP
pub const F_RDM: u64 = 1 << 10; // SQRDMLAH/SQRDMLSH
pub const F_DOTPROD: u64 = 1 << 11;
pub const F_FHM: u64 = 1 << 12; // FMLAL/FMLSL
pub const F_FLAGM: u64 = 1 << 13;
pub const F_FLAGM2: u64 = 1 << 14;
pub const F_DPB: u64 = 1 << 15; // DC CVAP
pub const F_DPB2: u64 = 1 << 16; // DC CVADP
pub const F_JSCVT: u64 = 1 << 17;
pub const F_FCMA: u64 = 1 << 18;
pub const F_LRCPC: u64 = 1 << 19; // LDAPR
pub const F_FRINTTS: u64 = 1 << 20;
pub const F_SB: u64 = 1 << 21;
pub const F_PAUTH: u64 = 1 << 22; // formas no-pista de PAuth (BRAA, RETAA, LDRAA, PACGA...): no implementadas
pub const F_LOR: u64 = 1 << 23; // LDLAR/STLLR
pub const F_SSBS: u64 = 1 << 24; // PSTATE.SSBS y MSR SSBS, #imm
pub const F_SSBS2: u64 = 1 << 25; // MRS/MSR del registro SSBS

/// Extensiones que heddle implementa. Se ejecutan las que ademas anuncia el modelo o el perfil (`need`).
pub const EXEC: u64 = F_FP16
    | F_ASIMDHP
    | F_AES
    | F_PMULL
    | F_SHA1
    | F_SHA256
    | F_SHA512
    | F_SHA3
    | F_CRC32
    | F_LSE
    | F_RDM
    | F_DOTPROD
    | F_FHM
    | F_FLAGM
    | F_FLAGM2
    | F_DPB
    | F_DPB2
    | F_JSCVT
    | F_FCMA
    | F_LRCPC
    | F_FRINTTS
    | F_SB
    | F_LOR
    | F_SSBS
    | F_SSBS2;

/// Extensiones (bits `F_*`) que dicen los registros de identificacion `r` (saneados). FEAT_LOR no es visible en EL0
/// (ID_AA64MMFR1_EL1.LO no es `FTR_VISIBLE`): es obligatoria desde ARMv8.1, y FEAT_LSE tambien, asi que se deduce de
/// esta. Las formas no-pista de PAuth no estan en `EXEC`: nunca se ejecutan.
pub fn feats_of(r: &Regs) -> u64 {
    let c = |cond: bool, bit: u64| if cond { bit } else { 0 };
    let (p0, i0, i1) = (r.pfr0, r.isar0, r.isar1);
    c(field(p0, 16) == 1, F_FP16)
        | c(field(p0, 20) == 1, F_ASIMDHP)
        | c(field(i0, 4) >= 1, F_AES)
        | c(field(i0, 4) >= 2, F_PMULL)
        | c(field(i0, 8) >= 1, F_SHA1)
        | c(field(i0, 12) >= 1, F_SHA256)
        | c(field(i0, 12) >= 2, F_SHA512)
        | c(field(i0, 32) >= 1, F_SHA3)
        | c(field(i0, 16) >= 1, F_CRC32)
        | c(field(i0, 20) >= 2, F_LSE | F_LOR)
        | c(field(i0, 28) >= 1, F_RDM)
        | c(field(i0, 44) >= 1, F_DOTPROD)
        | c(field(i0, 48) >= 1, F_FHM)
        | c(field(i0, 52) >= 1, F_FLAGM)
        | c(field(i0, 52) >= 2, F_FLAGM2)
        | c(field(i1, 0) >= 1, F_DPB)
        | c(field(i1, 0) >= 2, F_DPB2)
        | c(field(i1, 12) >= 1, F_JSCVT)
        | c(field(i1, 16) >= 1, F_FCMA)
        | c(field(i1, 20) >= 1, F_LRCPC)
        | c(field(i1, 32) >= 1, F_FRINTTS)
        | c(field(i1, 36) >= 1, F_SB)
        | c(field(i1, 4) | field(i1, 8) | field(i1, 24) | field(i1, 28) != 0, F_PAUTH)
        | c(field(r.pfr1, 4) >= 1, F_SSBS)
        | c(field(r.pfr1, 4) >= 2, F_SSBS2)
}

/// Nombre de la arquitectura de una extension `F_*` (diagnostico).
pub fn feat_name(bit: u64) -> &'static str {
    match bit {
        F_FP16 | F_ASIMDHP => "FEAT_FP16",
        F_AES => "FEAT_AES",
        F_PMULL => "FEAT_PMULL",
        F_SHA1 => "FEAT_SHA1",
        F_SHA256 => "FEAT_SHA256",
        F_SHA512 => "FEAT_SHA512",
        F_SHA3 => "FEAT_SHA3",
        F_CRC32 => "FEAT_CRC32",
        F_LSE => "FEAT_LSE",
        F_RDM => "FEAT_RDM",
        F_DOTPROD => "FEAT_DotProd",
        F_FHM => "FEAT_FHM",
        F_FLAGM => "FEAT_FlagM",
        F_FLAGM2 => "FEAT_FlagM2",
        F_DPB => "FEAT_DPB",
        F_DPB2 => "FEAT_DPB2",
        F_JSCVT => "FEAT_JSCVT",
        F_FCMA => "FEAT_FCMA",
        F_LRCPC => "FEAT_LRCPC",
        F_FRINTTS => "FEAT_FRINTTS",
        F_SB => "FEAT_SB",
        F_PAUTH => "FEAT_PAuth",
        F_LOR => "FEAT_LOR",
        F_SSBS => "FEAT_SSBS",
        F_SSBS2 => "FEAT_SSBS2",
        _ => "",
    }
}

/// Extensiones que se ejecutan (`State::exec`), publicadas por `init`; `UNSET` hasta entonces.
static EXEC_NOW: AtomicU64 = AtomicU64::new(UNSET);
const UNSET: u64 = u64::MAX;

thread_local! {
    /// Decodificacion para el diagnostico (`probe_all`): todo lo implementado se da por presente.
    static PROBE_ALL: Cell<bool> = const { Cell::new(false) };
    /// Primera extension (indice de bit + 1) que falto en la ultima decodificacion de este hilo dentro de `probe_all`.
    static LAST_MISS: Cell<u8> = const { Cell::new(0) };
}

#[cold]
fn exec_slow() -> u64 {
    init().exec
}

/// Para el decodificador: se ejecutan las extensiones `bits` (`F_*`; anunciadas e implementadas)? Una carga relajada y
/// una comparacion; el decodificador solo corre al traducir.
#[inline]
pub fn need(bits: u64) -> bool {
    let e = match EXEC_NOW.load(Relaxed) {
        UNSET => exec_slow(),
        e => e,
    };
    bits & !e == 0 || miss(bits & !e)
}

/// Fuera de `probe_all`, falta. Dentro, la anota y se da por presente si heddle la implementa.
#[cold]
fn miss(m: u64) -> bool {
    PROBE_ALL.with(|p| p.get()) && {
        LAST_MISS.with(|l| {
            if l.get() == 0 {
                l.set(m.trailing_zeros() as u8 + 1)
            }
        });
        m & !EXEC == 0
    }
}

/// Ejecuta `f` (una decodificacion) como si se anunciara todo lo que heddle implementa, y devuelve tambien la primera
/// extension que no esta en el modelo (`FEAT_*`), si alguna: asi el diagnostico distingue una instruccion valida de
/// una extension ausente de una codificacion que no existe. Solo afecta a este hilo.
pub fn probe_all<R>(f: impl FnOnce() -> R) -> (R, Option<&'static str>) {
    LAST_MISS.with(|l| l.set(0));
    PROBE_ALL.with(|p| p.set(true));
    let r = f();
    PROBE_ALL.with(|p| p.set(false));
    let m = match LAST_MISS.with(|l| l.get()) {
        0 => None,
        n => Some(feat_name(1u64 << (n - 1))).filter(|s| !s.is_empty()),
    };
    (r, m)
}

// --- HWCAP ---------------------------------------------------------------------------------------------------------

/// Nombres de los bits de AT_HWCAP (0..63) y AT_HWCAP2 (64..) en el orden de `/proc/cpuinfo` (Linux 6.6,
/// `hwcap_str`): el indice es el numero de bit.
pub const HWCAP_NAMES: [&str; 64 + 45] = [
    "fp", "asimd", "evtstrm", "aes", "pmull", "sha1", "sha2", "crc32", "atomics", "fphp", "asimdhp", "cpuid", "asimdrdm",
    "jscvt", "fcma", "lrcpc", "dcpop", "sha3", "sm3", "sm4", "asimddp", "sha512", "sve", "asimdfhm", "dit", "uscat",
    "ilrcpc", "flagm", "ssbs", "sb", "paca", "pacg", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    "", "", "", "", "", "", "", "", "", "", "", "", "", "", "",
    // HWCAP2
    "dcpodp", "sve2", "sveaes", "svepmull", "svebitperm", "svesha3", "svesm4", "flagm2", "frint", "svei8mm", "svef32mm",
    "svef64mm", "svebf16", "i8mm", "bf16", "dgh", "rng", "bti", "mte", "ecv", "afp", "rpres", "mte3", "sme",
    "smei16i64", "smef64f64", "smei8i32", "smef16f32", "smeb16f32", "smef32f32", "smefa64", "wfxt", "ebf16", "sveebf16",
    "cssc", "rprfm", "sve2p1", "sme2", "sme2p1", "smei16i32", "smebi32i32", "smeb16b16", "smef16f16", "mops", "hbc",
];

/// Condicion de un HWCAP sobre su campo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cond {
    /// campo >= minimo
    Ge,
    /// FP/AdvSIMD (con signo): implementado salvo 0xF
    Fp,
    /// FP/AdvSIMD con media precision: exactamente 1
    Half,
    /// de SVE (ZFR0): campo >= minimo y SVE implementado
    Sve,
}

/// Un HWCAP de Linux (`arm64_elf_hwcaps`): bit (0..63 HWCAP, 64.. HWCAP2), registro, campo y valor minimo.
struct Cap {
    bit: u32,
    reg: R,
    shift: u32,
    min: u64,
    cond: Cond,
}

const fn cap(bit: u32, reg: R, shift: u32, min: u64) -> Cap {
    Cap { bit, reg, shift, min, cond: Cond::Ge }
}
const fn sve(bit: u32, shift: u32, min: u64) -> Cap {
    Cap { bit, reg: R::Zfr0, shift, min, cond: Cond::Sve }
}

/// HWCAP derivados de un solo campo (Linux 6.6). Aparte: EVTSTRM y CPUID (siempre), PACA y PACG (varios campos) y
/// los de SME salvo `sme` (dependen de ID_AA64SMFR0_EL1, que heddle no modela: nunca se anuncian).
const CAPS: &[Cap] = &[
    Cap { bit: 0, reg: R::Pfr0, shift: 16, min: 0, cond: Cond::Fp },
    Cap { bit: 1, reg: R::Pfr0, shift: 20, min: 0, cond: Cond::Fp },
    cap(3, R::Isar0, 4, 1),   // aes
    cap(4, R::Isar0, 4, 2),   // pmull
    cap(5, R::Isar0, 8, 1),   // sha1
    cap(6, R::Isar0, 12, 1),  // sha2
    cap(7, R::Isar0, 16, 1),  // crc32
    cap(8, R::Isar0, 20, 2),  // atomics
    Cap { bit: 9, reg: R::Pfr0, shift: 16, min: 1, cond: Cond::Half },
    Cap { bit: 10, reg: R::Pfr0, shift: 20, min: 1, cond: Cond::Half },
    cap(12, R::Isar0, 28, 1), // asimdrdm
    cap(13, R::Isar1, 12, 1), // jscvt
    cap(14, R::Isar1, 16, 1), // fcma
    cap(15, R::Isar1, 20, 1), // lrcpc
    cap(16, R::Isar1, 0, 1),  // dcpop
    cap(17, R::Isar0, 32, 1), // sha3
    cap(18, R::Isar0, 36, 1), // sm3
    cap(19, R::Isar0, 40, 1), // sm4
    cap(20, R::Isar0, 44, 1), // asimddp
    cap(21, R::Isar0, 12, 2), // sha512
    cap(22, R::Pfr0, 32, 1),  // sve
    cap(23, R::Isar0, 48, 1), // asimdfhm
    cap(24, R::Pfr0, 48, 1),  // dit
    cap(25, R::Mmfr2, 32, 1), // uscat
    cap(26, R::Isar1, 20, 2), // ilrcpc
    cap(27, R::Isar0, 52, 1), // flagm
    cap(28, R::Pfr1, 4, 2),   // ssbs (SSBS2)
    cap(29, R::Isar1, 36, 1), // sb
    cap(64, R::Isar1, 0, 2),  // dcpodp
    sve(65, 0, 1),            // sve2
    sve(66, 4, 1),            // sveaes
    sve(67, 4, 2),            // svepmull
    sve(68, 16, 1),           // svebitperm
    sve(69, 32, 1),           // svesha3
    sve(70, 40, 1),           // svesm4
    cap(71, R::Isar0, 52, 2), // flagm2
    cap(72, R::Isar1, 32, 1), // frint
    sve(73, 44, 1),           // svei8mm
    sve(74, 52, 1),           // svef32mm
    sve(75, 56, 1),           // svef64mm
    sve(76, 20, 1),           // svebf16
    cap(77, R::Isar1, 52, 1), // i8mm
    cap(78, R::Isar1, 44, 1), // bf16
    cap(79, R::Isar1, 48, 1), // dgh
    cap(80, R::Isar0, 60, 1), // rng
    cap(81, R::Pfr1, 0, 1),   // bti
    cap(82, R::Pfr1, 8, 2),   // mte
    cap(83, R::Mmfr0, 60, 1), // ecv
    cap(84, R::Mmfr1, 44, 1), // afp
    cap(85, R::Isar2, 4, 1),  // rpres
    cap(86, R::Pfr1, 8, 3),   // mte3
    cap(87, R::Pfr1, 24, 1),  // sme
    cap(95, R::Isar2, 0, 2),  // wfxt
    cap(96, R::Isar1, 44, 2), // ebf16
    sve(97, 20, 2),           // sveebf16
    cap(98, R::Isar2, 52, 1), // cssc
    cap(99, R::Isar2, 48, 1), // rprfm
    sve(100, 0, 2),           // sve2p1
    cap(107, R::Isar2, 16, 1), // mops
    cap(108, R::Isar2, 20, 1), // hbc
];

/// Campos de PACA (APA, API, APA3) y PACG (GPA, GPI, GPA3).
const PACA: [(R, u32); 3] = [(R::Isar1, 4), (R::Isar1, 8), (R::Isar2, 12)];
const PACG: [(R, u32); 3] = [(R::Isar1, 24), (R::Isar1, 28), (R::Isar2, 8)];

fn cap_present(c: &Cap, r: &Regs) -> bool {
    let v = field(r.get(c.reg), c.shift);
    match c.cond {
        Cond::Ge => v >= c.min,
        Cond::Fp => v != 0xF,
        Cond::Half => v == 1,
        Cond::Sve => v >= c.min && field(r.pfr0, 32) >= 1,
    }
}

/// AT_HWCAP y AT_HWCAP2 derivados de los registros saneados como en Linux (`arm64_elf_hwcaps`), mas CPUID (MRS de
/// identificacion emulado) y EVTSTRM (flujo de eventos del temporizador: lo activan todos los nucleos de Android).
pub fn hwcaps_of(r: &Regs) -> (u64, u64) {
    let mut h = [(1u64 << 2) | (1 << 11), 0u64];
    let mut put = |bit: u32| h[bit as usize / 64] |= 1 << (bit % 64);
    for c in CAPS {
        if cap_present(c, r) {
            put(c.bit);
        }
    }
    if PACA.iter().any(|&(g, s)| field(r.get(g), s) >= 1) {
        put(30);
    }
    if PACG.iter().any(|&(g, s)| field(r.get(g), s) >= 1) {
        put(31);
    }
    (h[0], h[1])
}

/// Nombre de un bit de HWCAP2 (`bit` 0..).
pub fn hwcap2_name(bit: u32) -> &'static str {
    HWCAP_NAMES.get(64 + bit as usize).copied().unwrap_or("")
}

/// Nombres de los bits presentes, en el orden de `/proc/cpuinfo`.
pub fn cap_names(h1: u64, h2: u64) -> Vec<&'static str> {
    let mut out: Vec<&str> = (0..64).filter(|i| h1 >> i & 1 != 0).map(|i| HWCAP_NAMES[i as usize]).collect();
    out.extend((0..64u32).filter(|i| h2 >> i & 1 != 0).map(hwcap2_name));
    out
}

/// Linea `Features` de `/proc/cpuinfo` (sin el prefijo): los nombres de los bits presentes, en orden.
pub fn features_line(h1: u64, h2: u64) -> String {
    cap_names(h1, h2).join(" ")
}

/// HWCAP de lo que heddle implementa (para el aviso de un perfil que anuncia mas).
fn implemented_caps() -> (u64, u64) {
    hwcaps_of(&sanitize(&IMPL))
}

/// Anuncia (`on`) o retira una extension por su nombre de `/proc/cpuinfo`, ajustando los campos de identificacion
/// como haria el procesador (lo inverso de `hwcaps_of`). Err con el motivo si el nombre no se puede expresar.
pub fn set_feature(r: &mut Regs, name: &str, on: bool) -> Result<(), &'static str> {
    match name {
        "evtstrm" | "cpuid" => return if on { Ok(()) } else { Err("siempre presente en Linux/Android") },
        "paca" | "pacg" => {
            let fields = if name == "paca" { PACA } else { PACG };
            if on {
                if !fields.iter().any(|&(g, s)| field(r.get(g), s) >= 1) {
                    // API/GPI = 1: algoritmo definido por la implementacion
                    let (g, s) = fields[1];
                    set_field(r.get_mut(g), s, 1);
                }
            } else {
                for (g, s) in fields {
                    set_field(r.get_mut(g), s, 0);
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let bit = HWCAP_NAMES.iter().position(|n| *n == name).ok_or("caracteristica desconocida")? as u32;
    let c = CAPS.iter().find(|c| c.bit == bit).ok_or("depende de ID_AA64SMFR0_EL1, que heddle no modela")?;
    let reg = r.get_mut(c.reg);
    let v = field(*reg, c.shift);
    match (c.cond, on) {
        (Cond::Fp, true) if v == 0xF => set_field(reg, c.shift, 0),
        (Cond::Fp, false) => set_field(reg, c.shift, 0xF),
        (Cond::Half, true) => set_field(reg, c.shift, 1),
        (Cond::Half, false) if v == 1 => set_field(reg, c.shift, 0),
        (Cond::Ge | Cond::Sve, true) if v < c.min => set_field(reg, c.shift, c.min),
        (Cond::Ge | Cond::Sve, false) if v >= c.min => set_field(reg, c.shift, c.min - 1),
        _ => {}
    }
    if c.cond == Cond::Sve && on && field(r.pfr0, 32) == 0 {
        set_field(&mut r.pfr0, 32, 1);
    }
    Ok(())
}

// --- perfil ---------------------------------------------------------------------------------------------------------

/// Ruta por defecto del perfil (la instala weft junto a `libheddle.so`).
pub const PROFILE_PATH: &str = "/system/etc/heddle/cpu.conf";

/// Lo que ve el guest (se calcula una vez).
#[derive(Clone, Debug)]
pub struct State {
    /// nombre legible (registro, diagnostico)
    pub name: String,
    /// de donde sale (modelo del catalogo, perfil y su ruta)
    pub origin: String,
    pub midr: u64,
    /// REVIDR_EL1 (solo en sysfs: `MRS` lo lee como 0, como lo emula Linux)
    pub revidr: u64,
    pub ctr: u64,
    pub dczid: u64,
    /// registros saneados que lee `MRS`
    pub regs: Regs,
    pub hwcap: u64,
    pub hwcap2: u64,
    /// linea `Hardware` de `/proc/cpuinfo` (la imprimen algunos nucleos de fabricante; Linux no)
    pub hardware: Option<String>,
    /// extensiones anunciadas que heddle no implementa (solo con un perfil)
    pub unimplemented: Vec<&'static str>,
    /// extensiones que se ejecutan (`F_*`): las anunciadas que heddle implementa
    pub exec: u64,
}

impl State {
    fn of_model(m: &Model, origin: String) -> State {
        let regs = m.announced();
        let (hwcap, hwcap2) = hwcaps_of(&regs);
        State {
            name: m.name.to_string(),
            origin,
            midr: m.midr,
            revidr: 0,
            ctr: m.ctr,
            dczid: m.dczid,
            regs,
            hwcap,
            hwcap2,
            hardware: None,
            unimplemented: Vec::new(),
            exec: feats_of(&regs) & EXEC,
        }
    }
}

fn parse_u64(v: &str) -> Option<u64> {
    let v = v.trim().replace('_', "");
    match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

/// Interpreta un perfil (formato en `docs/perfil-cpu.md`). Devuelve lo que ve el guest y los avisos (claves o
/// valores que no se entienden: se ignoran). Precedencia, sin importar el orden de las lineas:
/// `base` -> `features` (lista absoluta desde cero, o `+x`/`-x` sobre la base) -> `id_aa64*` (sustituyen el registro
/// entero) -> saneado de Linux -> HWCAP derivados de los registros finales. `midr`, `revidr`, `ctr`, `dczid`, si no
/// estan, salen de la base (o de Cortex-A78).
pub fn parse_profile(text: &str, origin: &str) -> (State, Vec<String>) {
    let mut warn = Vec::new();
    let mut kv: Vec<(String, String)> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match line.split_once('=') {
            Some((k, v)) => kv.push((k.trim().to_ascii_lowercase(), v.trim().to_string())),
            None => warn.push(format!("linea {}: sin '=' ({})", n + 1, line)),
        }
    }
    let get = |k: &str| kv.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let base = match get("base") {
        Some(b) => find(b).or_else(|| {
            warn.push(format!("base '{}' desconocida (validos: {}); se parte de {}", b, model_ids(), MODELS[DEFAULT].id));
            None
        }),
        None => None,
    };
    let start = base.unwrap_or(&MODELS[DEFAULT]);
    let mut regs = start.user_view();
    if let Some(list) = get("features") {
        let items: Vec<&str> = list.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).collect();
        if items.iter().any(|i| !i.starts_with('+') && !i.starts_with('-')) {
            // lista absoluta: desde un procesador sin nada (ni FP ni AdvSIMD)
            regs = sanitize(&Regs { pfr0: f(0xF, 16) | f(0xF, 20), ..regs_zero() });
        }
        for it in items {
            let (on, name) = match it.as_bytes()[0] {
                b'+' => (true, &it[1..]),
                b'-' => (false, &it[1..]),
                _ => (true, it),
            };
            if let Err(e) = set_feature(&mut regs, &name.to_ascii_lowercase(), on) {
                warn.push(format!("features: '{}': {} (se ignora)", it, e));
            }
        }
    }
    for (k, r) in REG_KEYS {
        let v = get(k).or_else(|| get(&format!("{}_el1", k)));
        if let Some(v) = v {
            match parse_u64(v) {
                Some(x) => *regs.get_mut(r) = x,
                None => warn.push(format!("{}: valor no valido '{}' (se ignora)", k, v)),
            }
        }
    }
    let regs = sanitize(&regs);
    let num = |k: &str, dflt: u64, warn: &mut Vec<String>| match get(k).map(|v| (v, parse_u64(v))) {
        None => dflt,
        Some((_, Some(x))) => x,
        Some((v, None)) => {
            warn.push(format!("{}: valor no valido '{}' (se ignora)", k, v));
            dflt
        }
    };
    let midr = num("midr", start.midr, &mut warn) & 0xFFFF_FFFF;
    let revidr = num("revidr", 0, &mut warn);
    let ctr = num("ctr", start.ctr, &mut warn);
    let dczid = num("dczid", start.dczid, &mut warn);
    const KNOWN: [&str; 8] = ["nombre", "hardware", "base", "midr", "revidr", "ctr", "dczid", "features"];
    for (k, _) in &kv {
        let reg = REG_KEYS.iter().any(|(n, _)| k == n || k.strip_suffix("_el1") == Some(n));
        if !reg && !KNOWN.contains(&k.as_str()) {
            warn.push(format!("clave desconocida '{}' (se ignora)", k));
        }
    }
    let (hwcap, hwcap2) = hwcaps_of(&regs);
    let (i1, i2) = implemented_caps();
    let name = get("nombre").map(str::to_string).unwrap_or_else(|| base.map_or("perfil".to_string(), |m| m.name.to_string()));
    let state = State {
        name,
        origin: format!("perfil {}", origin),
        midr,
        revidr,
        ctr,
        dczid,
        regs,
        hwcap,
        hwcap2,
        hardware: get("hardware").filter(|h| !h.is_empty()).map(str::to_string),
        unimplemented: cap_names(hwcap & !i1, hwcap2 & !i2),
        exec: feats_of(&regs) & EXEC,
    };
    (state, warn)
}

const fn regs_zero() -> Regs {
    regs(0, 0, 0, 0, 0)
}

// --- seleccion -------------------------------------------------------------------------------------------------------

static STATE: OnceLock<State> = OnceLock::new();

/// Busca un modelo por nombre (`cortex-a78`, `a78`, `Cortex-A78`, `x1`...).
pub fn find(name: &str) -> Option<&'static Model> {
    let n = name.trim().to_ascii_lowercase();
    let n = n.strip_prefix("cortex-").unwrap_or(&n);
    MODELS.iter().find(|m| m.id.strip_prefix("cortex-").unwrap_or(m.id) == n)
}

/// Nombres validos para `HEDDLE_CPU` (para los mensajes).
pub fn model_ids() -> String {
    MODELS.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
}

/// Modelo cuando no se pide ninguno (`set_default`); si no se fijo, `DEFAULT` (`MAX` en las pruebas unitarias).
static FALLBACK: OnceLock<&'static Model> = OnceLock::new();

/// Fija el modelo por defecto de este proceso (antes de la primera consulta): `heddle-difftest` usa `max`. Con un
/// modelo fijado asi, el perfil solo se lee si se pide expresamente (`HEDDLE_CPU_FILE`).
pub fn set_default(id: &str) {
    if let Some(m) = find(id) {
        let _ = FALLBACK.set(m);
    }
}

fn env_or_prop(env: &str, prop: &str) -> Option<String> {
    std::env::var(env).ok().filter(|v| !v.is_empty()).or_else(|| crate::boundary::prop(prop).filter(|v| !v.is_empty()))
}

fn select() -> State {
    let log = crate::bridge::alog;
    let fallback = FALLBACK.get().copied();
    let dflt: &'static Model = fallback.unwrap_or(if cfg!(test) { &MODELS[MAX] } else { &MODELS[DEFAULT] });
    let mut note = String::new();
    // 1) modelo del catalogo pedido
    if let Some(w) = env_or_prop("HEDDLE_CPU", "debug.heddle.cpu") {
        match find(&w) {
            Some(m) => return log_model(State::of_model(m, "pedido".into()), m),
            None => note = format!("; '{}' desconocido (validos: {})", w, model_ids()),
        }
    }
    // 2) perfil
    let explicit = env_or_prop("HEDDLE_CPU_FILE", "debug.heddle.cpu_file");
    let path = explicit.clone().or_else(|| (fallback.is_none() && !cfg!(test)).then(|| PROFILE_PATH.to_string()));
    if let Some(p) = path {
        match std::fs::read_to_string(&p) {
            Ok(text) => {
                let (s, warn) = parse_profile(&text, &p);
                log(&format!(
                    "cpu: {} '{}' (MIDR {:#010x}); Features: {}",
                    s.origin,
                    s.name,
                    s.midr,
                    features_line(s.hwcap, s.hwcap2)
                ));
                for w in warn {
                    log(&format!("cpu: perfil {}: aviso: {}", p, w));
                }
                if !s.unimplemented.is_empty() {
                    log(&format!(
                        "cpu: AVISO: el perfil anuncia extensiones que heddle no implementa (si el guest las usa recibe \
                         SIGILL): {}",
                        s.unimplemented.join(" ")
                    ));
                }
                return s;
            }
            Err(e) if explicit.is_some() => note += &format!("; perfil {} no legible: {}", p, e),
            Err(_) => {}
        }
    }
    // 3) por defecto
    log_model(State::of_model(dflt, format!("por defecto{}", note)), dflt)
}

fn log_model(s: State, m: &Model) -> State {
    let missing = m.missing();
    crate::bridge::alog(&format!(
        "cpu: modelo={} {} (MIDR {:#010x}, {}); sin anunciar (heddle no las implementa): {}",
        m.name,
        m.revision(),
        m.midr,
        s.origin,
        if missing.is_empty() { "ninguna".to_string() } else { missing.join(" ") }
    ));
    s
}

/// Elige lo que ve el guest (una vez). Se llama al crear el primer hilo guest; las consultas lo hacen si hace falta.
pub fn init() -> &'static State {
    let s = STATE.get_or_init(select);
    EXEC_NOW.store(s.exec, Relaxed);
    s
}

/// Nombre del modelo o perfil elegido.
pub fn name() -> &'static str {
    &init().name
}

/// Valor de un registro del espacio de identificacion (modelo elegido), o None si Linux da SIGILL.
pub fn id_reg(sysreg: u16) -> Option<u64> {
    let s = init();
    id_reg_of(s.midr, &s.regs, sysreg)
}

/// AT_HWCAP del modelo elegido.
pub fn hwcap() -> u64 {
    init().hwcap
}

/// AT_HWCAP2 del modelo elegido.
pub fn hwcap2() -> u64 {
    init().hwcap2
}

/// MIDR_EL1 del modelo elegido.
pub fn midr() -> u64 {
    init().midr
}

/// REVIDR_EL1 para sysfs (`MRS` lee 0).
pub fn revidr() -> u64 {
    init().revidr
}

/// CTR_EL0 del modelo elegido.
pub fn ctr() -> u64 {
    init().ctr
}

/// DCZID_EL0 del modelo elegido.
pub fn dczid() -> u64 {
    init().dczid
}

/// Linea `Hardware` de `/proc/cpuinfo`, si el perfil la da.
pub fn hardware() -> Option<&'static str> {
    init().hardware.as_deref()
}

/// Bits de PSTATE, aparte de NZCV, que el nucleo deja en el `pstate` del marco de una senal: SSBS (bit 12), que
/// Linux pone a 1 en los procesos sin mitigacion pedida (`spectre_v4_enable_task_mitigation`) si el procesador lo
/// implementa (ID_AA64PFR1_EL1.SSBS anunciado).
#[inline]
pub fn pstate_extra() -> u64 {
    if field(init().regs.pfr1, 4) >= 1 {
        1 << 12
    } else {
        0
    }
}

// --- registros de sistema accesibles ------------------------------------------------------------------------------

/// Codificacion de registro de sistema de `decode::Op::Mrs/Msr`: op0<<14 | op1<<11 | CRn<<7 | CRm<<3 | op2.
pub const fn sysreg(op0: u16, op1: u16, crn: u16, crm: u16, op2: u16) -> u16 {
    (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
}

pub const NZCV: u16 = sysreg(3, 3, 4, 2, 0);
pub const FPCR: u16 = sysreg(3, 3, 4, 4, 0);
pub const FPSR: u16 = sysreg(3, 3, 4, 4, 1);
pub const TPIDR_EL0: u16 = sysreg(3, 3, 13, 0, 2);
pub const TPIDRRO_EL0: u16 = sysreg(3, 3, 13, 0, 3);
pub const CTR_EL0: u16 = sysreg(3, 3, 0, 0, 1);
pub const DCZID_EL0: u16 = sysreg(3, 3, 0, 0, 7);
pub const CNTFRQ_EL0: u16 = sysreg(3, 3, 14, 0, 0);
pub const CNTVCT_EL0: u16 = sysreg(3, 3, 14, 0, 2);
/// Registro SSBS (FEAT_SSBS2): PSTATE.SSBS en el bit 12.
pub const SSBS: u16 = sysreg(3, 3, 4, 2, 6);

/// El guest puede leer (`MRS`) este registro en EL0 de Linux con las extensiones anunciadas.
pub fn readable(sysreg: u16) -> bool {
    matches!(sysreg, NZCV | FPCR | FPSR | TPIDR_EL0 | TPIDRRO_EL0 | CTR_EL0 | DCZID_EL0 | CNTFRQ_EL0 | CNTVCT_EL0)
        || (sysreg == SSBS && need(F_SSBS2))
        || id_reg_of(0, &IMPL, sysreg).is_some()
}

/// El guest puede escribir (`MSR`) este registro en EL0.
pub fn writable(sysreg: u16) -> bool {
    matches!(sysreg, NZCV | FPCR | FPSR | TPIDR_EL0) || (sysreg == SSBS && need(F_SSBS2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str) -> &'static Model {
        find(id).unwrap()
    }

    /// Bits de HWCAP por nombre.
    fn caps(names: &str) -> (u64, u64) {
        let (mut a, mut b) = (0u64, 0u64);
        for n in names.split_whitespace() {
            if let Some(i) = HWCAP_NAMES[..64].iter().position(|x| *x == n) {
                a |= 1 << i;
            } else if let Some(i) = (0..23).find(|&i| hwcap2_name(i) == n) {
                b |= 1 << i;
            } else {
                panic!("{}", n);
            }
        }
        (a, b)
    }

    /// Linea `Features` de cada modelo: la de un nucleo Linux 6.x sobre ese procesador (las de los dispositivos con
    /// A55/A76/A77/A78/X1 son iguales: el nucleo publica la interseccion y todos tienen el mismo repertorio).
    #[test]
    fn hwcap_por_modelo() {
        let v82 = "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp";
        let want = [("cortex-a53", "fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid"), ("cortex-a55", v82), ("cortex-a76", v82),
            ("cortex-a77", v82), ("cortex-a78", v82), ("cortex-x1", v82)];
        assert_eq!(want.len() + 1, MODELS.len());
        for (id, line) in want {
            let md = m(id);
            assert_eq!(md.hwcaps(), caps(line), "{}", id);
            let (a, b) = md.hwcaps();
            assert_eq!(features_line(a, b), line, "{}", id);
            // sin diferencias: heddle implementa todo lo de estos procesadores
            assert!(md.missing().is_empty(), "{}: {:?}", id, md.missing());
        }
        assert_eq!(MODELS[DEFAULT].id, "cortex-a78");
        // max: todo lo de heddle (lo que se anunciaba antes de los modelos)
        assert_eq!(MODELS[MAX].id, "max");
        assert_eq!(
            features_line(MODELS[MAX].hwcaps().0, MODELS[MAX].hwcaps().1),
            "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 asimddp \
             sha512 asimdfhm flagm ssbs sb dcpodp flagm2 frint dgh"
        );
    }

    /// MRS de los registros de identificacion coherente con AT_HWCAP en cada modelo (comprobacion independiente de
    /// `hwcaps_of`: cada bit se recalcula desde el registro que lee el guest con `MRS`).
    #[test]
    fn mrs_coherente_con_hwcap_en_cada_modelo() {
        for md in MODELS {
            let rd = |crm: u16, op2: u16| md.id_reg(sysreg(3, 0, 0, crm, op2)).unwrap();
            let (pfr0, pfr1, isar0, isar1, mmfr2) = (rd(4, 0), rd(4, 1), rd(6, 0), rd(6, 1), rd(7, 2));
            let (h, h2) = md.hwcaps();
            let has = |n: &str| {
                let (a, b) = caps(n);
                (h & a) == a && (h2 & b) == b
            };
            let campo = |r: u64, s: u32| (r >> s) & 0xF;
            let checks: &[(&str, bool)] = &[
                ("fp", campo(pfr0, 16) != 0xF),
                ("asimd", campo(pfr0, 20) != 0xF),
                ("fphp", campo(pfr0, 16) == 1),
                ("asimdhp", campo(pfr0, 20) == 1),
                ("aes", campo(isar0, 4) >= 1),
                ("pmull", campo(isar0, 4) >= 2),
                ("sha1", campo(isar0, 8) >= 1),
                ("sha2", campo(isar0, 12) >= 1),
                ("sha512", campo(isar0, 12) >= 2),
                ("crc32", campo(isar0, 16) >= 1),
                ("atomics", campo(isar0, 20) >= 2),
                ("asimdrdm", campo(isar0, 28) >= 1),
                ("sha3", campo(isar0, 32) >= 1),
                ("asimddp", campo(isar0, 44) >= 1),
                ("asimdfhm", campo(isar0, 48) >= 1),
                ("flagm", campo(isar0, 52) >= 1),
                ("flagm2", campo(isar0, 52) >= 2),
                ("dcpop", campo(isar1, 0) >= 1),
                ("dcpodp", campo(isar1, 0) >= 2),
                ("jscvt", campo(isar1, 12) >= 1),
                ("fcma", campo(isar1, 16) >= 1),
                ("lrcpc", campo(isar1, 20) >= 1),
                ("ilrcpc", campo(isar1, 20) >= 2),
                ("frint", campo(isar1, 32) >= 1),
                ("sb", campo(isar1, 36) >= 1),
                ("paca", campo(isar1, 4) | campo(isar1, 8) != 0),
                ("pacg", campo(isar1, 24) | campo(isar1, 28) != 0),
                ("ssbs", campo(pfr1, 4) >= 2),
                ("bti", campo(pfr1, 0) >= 1),
                ("uscat", campo(mmfr2, 32) >= 1),
                ("sve", campo(pfr0, 32) >= 1),
                ("dit", campo(pfr0, 48) >= 1),
            ];
            for (n, want) in checks {
                assert_eq!(has(n), *want, "{}: {}", md.id, n);
            }
            assert!(has("cpuid evtstrm"));
            // MIDR y MPIDR como los emula Linux
            assert_eq!(md.id_reg(sysreg(3, 0, 0, 0, 0)), Some(md.midr));
            assert_eq!(md.id_reg(sysreg(3, 0, 0, 0, 5)), Some(1 << 31));
            assert_eq!(md.midr >> 24, if md.id == "max" { 0 } else { 0x41 }, "{}: implementador", md.id);
            assert_eq!((md.midr >> 16) & 0xF, 0xF, "{}: arquitectura por registros de identificacion", md.id);
            // CTR_EL0: lineas de 64 B (IminLine/DminLine = 4), RES1 en el bit 31; DC ZVA de 64 B
            assert_eq!((md.ctr & 0xF, (md.ctr >> 16) & 0xF, md.ctr >> 31 & 1), (4, 4, 1), "{}", md.id);
            assert_eq!(md.dczid, 4);
            // lo que se anuncia, heddle lo implementa
            let vis = zip(&md.announced(), &VISIBLE, |a, v| a & v);
            assert_eq!(zip(&vis, &IMPL, min_fields), vis, "{}", md.id);
        }
    }

    #[test]
    fn registros_de_identificacion_como_linux() {
        let a78 = m("a78");
        let id = |r: u16| a78.id_reg(r);
        assert_eq!(id(sysreg(3, 0, 0, 0, 0)), Some(0x411F_D411));
        assert_eq!(id(sysreg(3, 0, 0, 0, 6)), Some(0));
        assert_eq!(id(sysreg(3, 0, 0, 0, 1)), None); // CRm=0 sin emular: SIGILL
        assert_eq!(id(sysreg(3, 0, 0, 1, 0)), None); // registros AArch32: SIGILL
        assert_eq!(id(sysreg(3, 0, 0, 4, 0)), Some(0x0011_0011)); // PFR0: FP/AdvSIMD con FP16, EL0/EL1 AArch64
        assert_eq!(id(sysreg(3, 0, 0, 4, 1)), Some(0x10)); // PFR1: SSBS
        assert_eq!(id(sysreg(3, 0, 0, 4, 4)), Some(0)); // ZFR0 sin SVE
        assert_eq!(id(sysreg(3, 0, 0, 5, 0)), Some(6)); // DFR0: DebugVer
        assert_eq!(id(sysreg(3, 0, 0, 6, 0)), Some(0x0000_1000_1021_1120));
        assert_eq!(id(sysreg(3, 0, 0, 6, 1)), Some(0x0010_0001));
        assert_eq!(id(sysreg(3, 0, 0, 7, 0)), Some(0x0000_0111_FF00_0000)); // TGRAN*_2 = 1, TGRAN4/64 = 0xF
        assert_eq!(id(sysreg(3, 0, 0, 7, 7)), Some(0)); // reservado: RAZ
        assert_eq!(id(sysreg(3, 0, 0, 8, 0)), None);
        assert_eq!(id(sysreg(3, 1, 0, 0, 0)), None); // CCSIDR_EL1
        assert_eq!(id(TPIDR_EL0), None);
        // A53: ARMv8.0, sin FP16 (FP/AdvSIMD = 0)
        assert_eq!(m("cortex-a53").id_reg(sysreg(3, 0, 0, 4, 0)), Some(0x11));
        assert_eq!(m("cortex-a53").id_reg(sysreg(3, 0, 0, 4, 1)), Some(0));
    }

    #[test]
    fn nombres_de_modelo() {
        assert_eq!(find("Cortex-A55").unwrap().id, "cortex-a55");
        assert_eq!(find(" x1 ").unwrap().id, "cortex-x1");
        assert!(find("a710").is_none());
        assert!(find("").is_none());
        assert_eq!(m("a76").revision(), "r4p1");
        assert!(model_ids().contains("cortex-a53"));
    }

    #[test]
    fn max_ejecuta_todo_lo_implementado() {
        // con el modelo max (el de las pruebas unitarias) se ejecuta todo lo que anuncia, que es todo lo implementado,
        // mas LDLAR/STLLR (LOR, oculto en EL0, deducido de LSE)
        let r = sanitize(&IMPL);
        let c = |n: &str| CAPS.iter().any(|x| HWCAP_NAMES[x.bit as usize] == n && cap_present(x, &r));
        for (n, bit) in [("fphp", F_FP16), ("asimdhp", F_ASIMDHP), ("aes", F_AES), ("pmull", F_PMULL), ("sha1", F_SHA1),
            ("sha2", F_SHA256), ("sha512", F_SHA512), ("sha3", F_SHA3), ("crc32", F_CRC32), ("atomics", F_LSE),
            ("asimdrdm", F_RDM), ("asimddp", F_DOTPROD), ("asimdfhm", F_FHM), ("flagm", F_FLAGM), ("flagm2", F_FLAGM2),
            ("dcpop", F_DPB), ("dcpodp", F_DPB2), ("jscvt", F_JSCVT), ("fcma", F_FCMA), ("lrcpc", F_LRCPC),
            ("frint", F_FRINTTS), ("sb", F_SB), ("paca", F_PAUTH)] {
            assert_eq!(need(bit), c(n) || (n == "paca" && false), "{}", n);
        }
        assert!(need(F_LOR) && !need(F_PAUTH));
    }

    /// Codificaciones (ensambladas con llvm-mc) y la extension que exigen: 0 = base, `NUNCA` = PAuth (heddle no la
    /// implementa: SIGILL en todos los modelos), `PISTA` = espacio de pistas (NOP en todos).
    const NUNCA: u64 = u64::MAX;
    const PISTA: u64 = 0;
    const CASOS: &[(&str, u32, u64)] = &[
        ("ldadd", 0xb8200020, F_LSE),
        ("cas", 0x88a07c41, F_LSE),
        ("casp", 0x08207c82, F_LSE),
        ("ldapr", 0xb8bfc020, F_LRCPC),
        ("ldlar", 0x88df7c20, F_LOR),
        ("stllr", 0x889f7c20, F_LOR),
        ("crc32b", 0x1ac24020, F_CRC32),
        ("fadd h", 0x1ee22820, F_FP16),
        ("fmov h,w", 0x1ee70020, F_FP16),
        ("fcvt s,h", 0x1ee24020, 0),
        ("fadd 4h", 0x0e421420, F_ASIMDHP),
        ("fcvtl", 0x0e217820, 0),
        ("sqrdmlah", 0x2e428420, F_RDM),
        ("sdot", 0x0e829420, F_DOTPROD),
        ("fmlal", 0x0e22ec20, F_FHM),
        ("fcmla", 0x6e82c420, F_FCMA),
        ("fjcvtzs", 0x1e7e0020, F_JSCVT),
        ("frint32z", 0x1e284020, F_FRINTTS),
        ("eor3", 0xce020c20, F_SHA3),
        ("sha512h", 0xce628020, F_SHA512),
        ("aese", 0x4e284820, F_AES),
        ("pmull 1q", 0x0ee2e020, F_PMULL),
        ("sha1h", 0x5e280820, F_SHA1),
        ("sha256h", 0x5e024020, F_SHA256),
        ("rmif", 0xba000400, F_FLAGM),
        ("setf8", 0x3a00080d, F_FLAGM),
        ("cfinv", 0xd500401f, F_FLAGM),
        ("xaflag", 0xd500403f, F_FLAGM2),
        ("sb", 0xd50330ff, F_SB),
        ("dc cvap", 0xd50b7c20, F_DPB),
        ("dc cvadp", 0xd50b7d20, F_DPB2),
        ("msr ssbs, #1", 0xd503413f, F_SSBS),
        ("mrs x0, ssbs", 0xd53b42c0, F_SSBS2),
        ("msr ssbs, x0", 0xd51b42c0, F_SSBS2),
        ("retaa", 0xd65f0bff, NUNCA),
        ("braa", 0xd71f0801, NUNCA),
        ("ldraa", 0xf8200420, NUNCA),
        ("pacia", 0xdac10020, NUNCA),
        ("pacga", 0x9ac23020, NUNCA),
        ("paciasp", 0xd503233f, PISTA),
        ("autiasp", 0xd50323bf, PISTA),
        ("xpaclri", 0xd50320ff, PISTA),
    ];

    /// En un proceso con `HEDDLE_CPU=<modelo>`: decodificador, MRS, diagnostico y PSTATE segun ese modelo.
    fn comprobar_modelo(id: &str) {
        let want = find(id).unwrap();
        assert_eq!(name(), want.name, "HEDDLE_CPU no eligio el modelo");
        // se ejecuta lo anunciado e implementado; lo no anunciado es SIGILL ("ausente en el modelo"); PAuth nunca
        let exec = feats_of(&want.announced()) & EXEC;
        assert_eq!(init().exec, exec);
        for &(n, w, req) in CASOS {
            let valida = !matches!(crate::decode::decode(w), crate::decode::Op::Undef(_));
            assert_eq!(valida, req != NUNCA && req & !exec == 0, "{} {:#010x} en {}", n, w, id);
            if req != NUNCA && req & !exec != 0 {
                let d = crate::diag::classify(w).to_string();
                assert!(d.contains("ausente en el modelo") && d.contains(feat_name(req)), "{}: {}", n, d);
            }
        }
        let d = crate::diag::classify(0xd65f0bff).to_string();
        assert!(d.contains("RETAA"), "{}", d);
        // MRS por el interprete: los mismos valores que `id_reg`, y SIGILL donde Linux la da
        let mut c = crate::cpu::Cpu::new();
        for crm in 0..8u16 {
            for op2 in 0..8u16 {
                let r = sysreg(3, 0, 0, crm, op2);
                let w = 0xd5380000 | (u32::from(crm) << 8) | (u32::from(op2) << 5) | 1; // mrs x1, <reg>
                let flow = crate::interp::exec(&mut c, &crate::decode::decode(w));
                match id_reg(r) {
                    Some(v) => assert_eq!(c.x[1], v, "mrs {:#x} en {} ({:?})", r, id, flow),
                    None => assert!(matches!(flow, crate::interp::Flow::Undef(_)), "mrs {:#x} en {}: {:?}", r, id, flow),
                }
            }
        }
        assert_eq!(id_reg(sysreg(3, 0, 0, 0, 0)), Some(want.midr));
        assert_eq!(hwcaps_of(&want.announced()), (hwcap(), hwcap2()));
        assert_eq!(id == "cortex-a53", pstate_extra() == 0);
    }

    #[test]
    fn decodificacion_por_modelo() {
        if let Ok(id) = std::env::var("HEDDLE_FEAT_HIJO") {
            return comprobar_modelo(&id);
        }
        for m in MODELS {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["feat::tests::decodificacion_por_modelo", "--exact", "--test-threads=1", "--nocapture"])
                .env("HEDDLE_CPU", m.id)
                .env("HEDDLE_FEAT_HIJO", m.id)
                .output()
                .unwrap();
            let (so, se) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && so.contains("1 passed"), "modelo {}:\n{}\n{}", m.id, so, se);
        }
    }

    #[test]
    fn perfil_lista_absoluta() {
        let (s, w) = parse_profile(
            "# perfil de prueba\nnombre = Mi SoC\nmidr=0x411FD4D0 # A710\nrevidr=0x1\n\
             features=fp,asimd,evtstrm,aes,pmull,sha1,sha2,crc32,atomics,fphp,asimdhp,cpuid,asimdrdm,lrcpc,dcpop,asimddp,\
             sve,sve2,bf16,i8mm,paca,pacg,bti,ssbs,flagm,jscvt\n",
            "prueba",
        );
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!(s.name, "Mi SoC");
        assert_eq!((s.midr, s.revidr, s.ctr), (0x411F_D4D0, 1, MODELS[DEFAULT].ctr));
        assert_eq!(
            features_line(s.hwcap, s.hwcap2),
            "fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt lrcpc dcpop asimddp sve \
             flagm ssbs paca pacg sve2 i8mm bf16 bti"
        );
        // los campos de identificacion, coherentes (como los leeria MRS)
        assert_eq!(field(s.regs.pfr0, 32), 1);
        assert_eq!(field(s.regs.zfr0, 0), 1);
        assert_eq!(field(s.regs.isar1, 44), 1);
        assert_eq!(field(s.regs.pfr1, 4), 2);
        assert_eq!(hwcaps_of(&s.regs), (s.hwcap, s.hwcap2));
        // lo que heddle no implementa, para el aviso
        assert_eq!(s.unimplemented, ["sve", "paca", "pacg", "sve2", "i8mm", "bf16", "bti"]);
    }

    #[test]
    fn perfil_relativo_y_registros() {
        // +/- sobre la base; sin FP16 se pierde tambien su campo; los registros explicitos mandan
        let (s, w) = parse_profile("base=cortex-a55\nfeatures=+sha3,-asimddp,-fphp\nhardware=Prueba\n", "p");
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!((s.name.as_str(), s.midr, s.ctr), ("Cortex-A55", 0x412F_D050, 0x8444_8004));
        assert_eq!(s.hardware.as_deref(), Some("Prueba"));
        let line = features_line(s.hwcap, s.hwcap2);
        assert!(line.contains("sha3") && !line.contains("asimddp") && !line.contains("fphp") && line.contains("asimdhp"), "{}", line);
        assert!(s.unimplemented.is_empty());
        let (s, w) = parse_profile("base=a78\nfeatures=+sve\nid_aa64isar0_el1=0x0000000000011120\nid_aa64pfr0=0x0001000000110011\n", "p");
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!(features_line(s.hwcap, s.hwcap2), "fp asimd evtstrm aes pmull sha1 sha2 crc32 fphp asimdhp cpuid lrcpc dcpop dit");
        // sin base ni features: Cortex-A78 tal cual (sin limitar a heddle)
        let (s, _) = parse_profile("", "p");
        assert_eq!((s.midr, s.regs), (MODELS[DEFAULT].midr, MODELS[DEFAULT].user_view()));
        // avisos: clave desconocida, valor no valido, caracteristica desconocida, base desconocida, linea sin '='
        let (s, w) = parse_profile("color=azul\nmidr=zz\nfeatures=+nada,-cpuid,+smefa64\nbase=a999\nbasura\n", "p");
        assert_eq!(w.len(), 7, "{:?}", w);
        assert_eq!(s.midr, MODELS[DEFAULT].midr);
    }

    /// Con un perfil (`HEDDLE_CPU_FILE`): MRS, getauxval y el aviso muestran lo anunciado aunque heddle no lo
    /// implemente; la ejecucion no cambia (SM4E sigue siendo SIGILL, CFINV se ejecuta).
    #[test]
    fn perfil_desde_archivo() {
        if std::env::var("HEDDLE_FEAT_PERFIL").is_ok() {
            assert_eq!(name(), "SoC de prueba");
            assert_eq!(id_reg(sysreg(3, 0, 0, 0, 0)), Some(0x4100_0001));
            assert_eq!(field(id_reg(sysreg(3, 0, 0, 6, 0)).unwrap(), 40), 1, "SM4 anunciada");
            let (h1, _) = (hwcap(), hwcap2());
            assert!(h1 & (1 << 19) != 0 && h1 & (1 << 22) != 0, "sm4 y sve en AT_HWCAP");
            assert!(matches!(crate::decode::decode(0xcec08400), crate::decode::Op::Undef(_)), "SM4E: SIGILL");
            assert!(crate::diag::classify(0xcec08400).to_string().contains("SM4E"));
            assert!(matches!(crate::decode::decode(0xd500401f), crate::decode::Op::Undef(_)), "CFINV sin flagm: SIGILL");
            let d = crate::diag::classify(0xd500401f).to_string();
            assert!(d.contains("FEAT_FlagM") && d.contains("ausente en el modelo"), "{}", d);
            assert!(pstate_extra() == 0, "sin SSBS anunciado");
            return;
        }
        let dir = std::env::temp_dir().join(format!("heddle-perfil-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("cpu.conf");
        std::fs::write(&p, "nombre=SoC de prueba\nmidr=0x41000001\nfeatures=fp,asimd,sm4,sve\n").unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["feat::tests::perfil_desde_archivo", "--exact", "--test-threads=1", "--nocapture"])
            .env_remove("HEDDLE_CPU")
            .env("HEDDLE_CPU_FILE", &p)
            .env("HEDDLE_FEAT_PERFIL", "1")
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let (so, se) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(out.status.success() && so.contains("1 passed"), "{}\n{}", so, se);
        let all = format!("{}{}", so, se);
        assert!(all.contains("AVISO: el perfil anuncia extensiones que heddle no implementa") && all.contains(": sm4 sve"), "{}", all);
    }

    /// `cpu-features` (lo publica el release junto a libheddle.so; weft lo copia al instalar el traductor): la linea
    /// `Features` de `/proc/cpuinfo` con todo lo que heddle implementa (modelo `max`). Si cambia, regenerarlo con
    /// `HEDDLE_CPU_FEATURES=escribir cargo test --release --lib cpu_features_publicado`.
    #[test]
    fn cpu_features_publicado() {
        let (a, b) = MODELS[MAX].hwcaps();
        let want = format!("{}\n", features_line(a, b));
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/cpu-features");
        if std::env::var("HEDDLE_CPU_FEATURES").as_deref() == Ok("escribir") {
            std::fs::write(path, &want).unwrap();
        }
        assert_eq!(std::fs::read_to_string(path).unwrap_or_default(), want, "cpu-features desactualizado");
    }

    /// El ejemplo de `docs/perfil-cpu.md` (contrato con weft) se entiende sin avisos.
    #[test]
    fn ejemplo_del_contrato() {
        let doc = include_str!("../docs/perfil-cpu.md");
        let ej = doc.split("## Ejemplo\n\n```\n").nth(1).unwrap().split("```").next().unwrap();
        let (s, w) = parse_profile(ej, "doc");
        assert!(w.is_empty(), "{:?}", w);
        assert_eq!((s.name.as_str(), s.midr), ("Cortex-A710", 0x412F_D471));
        assert_eq!(s.unimplemented, ["sve", "dit", "uscat", "ilrcpc", "paca", "pacg", "sve2", "svebitperm", "i8mm", "bf16", "bti"]);
    }
}
