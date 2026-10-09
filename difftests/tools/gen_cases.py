#!/usr/bin/env python3
"""Genera casos de prueba aleatorios de A64 y su resultado esperado usando Unicorn (QEMU).

Uso: gen_cases.py SALIDA.bin N SEMILLA [clases]
Requiere Unicorn 2.1.4 (`pip install unicorn==2.1.4`, ver scripts/setup-debian.sh).

Memoria: MEM (lectura/escritura, contenido fijo de `splitmix`) entre dos guardas de 64 KiB sin permisos (GUARD_LO y
GUARD_HI; en heddle-difftest, paginas PROT_NONE). Los loads/stores/exclusivos usan como base una direccion de MEM
(o, en los casos de fallo, una de las guardas): nunca una direccion sin mapear al azar.

Fallos: un acceso a una guarda es SIGSEGV (SEGV_ACCERR, si_addr = direccion del acceso) y una falta de alineacion
de un exclusivo/atomico/ordenado es SIGBUS (BUS_ADRALN, si_addr = direccion), como en Linux arm64. Se guardan la
senal, el codigo, la direccion y el estado en el fallo (registros con el pc de la instruccion que falla, memoria
escrita por las anteriores). Los casos que el Arm ARM no fija o en los que Unicorn se aparta de el se excluyen con su
motivo (ver EXCLUSIONES); el recuento se imprime al final.

Formato (little endian): cabecera b"HDT2" y, por caso:
  u32 n; u32 words[n]
  u64 x[31]; u64 sp; u64 nzcv; u8 q[32][16]; u64 fpcr; u64 tpidr
  u32 status   (0 = ejecutado, 1 = Unicorn dice instruccion indefinida, 2 = fallo)
  si status == 2: u32 signo; u32 si_code; u64 si_addr   (y despues el estado en el fallo, como con status 0)
  si status != 1: u64 x[31]; u64 sp; u64 nzcv; u64 pc; u64 fpsr; u64 tpidr; u8 q[32][16];
                  u32 ndiff; (u64 addr, u64 val)*ndiff
"""
import random
import struct
import sys

from unicorn import *
from unicorn.arm64_const import *

CODE_BASE = 0x20000000
CODE_SIZE = 0x20000
CODE = CODE_BASE + 0x10000  # PC de las pruebas
NOP = 0xD503201F
MEM = 0x10000000
MEMSZ = 0x20000
GUARD = 0x10000
GUARD_LO = MEM - GUARD
GUARD_HI = MEM + MEMSZ
M64 = (1 << 64) - 1
MAGIC = b"HDT2"
SIGSEGV, SIGBUS = 11, 7
SEGV_ACCERR, BUS_ADRALN = 2, 1


def splitmix(x):
    x = (x + 0x9E3779B97F4A7C15) & M64
    z = x
    z = ((z ^ (z >> 30)) * 0xBF58476D1CE4E5B9) & M64
    z = ((z ^ (z >> 27)) * 0x94D049BB133111EB) & M64
    return z ^ (z >> 31)


def init_mem():
    out = bytearray()
    for a in range(MEM, MEM + MEMSZ, 8):
        out += struct.pack("<Q", splitmix(a >> 3))
    return bytes(out)


INIT = init_mem()

# (valor, mascara, peso, nombre): bits fijos = mascara; el resto aleatorio
CLASSES = [
    (0x10000000, 0x1F000000, 3, "adr"),
    (0x11000000, 0x1F000000, 8, "addsub_imm"),
    (0x12000000, 0x1F800000, 6, "logic_imm"),
    (0x12800000, 0x1F800000, 4, "movewide"),
    (0x13000000, 0x1F800000, 8, "bitfield"),
    (0x13800000, 0x7F800000, 3, "extr"),
    (0x0A000000, 0x1F000000, 8, "logic_shift"),
    (0x0B000000, 0x1F200000, 8, "addsub_shift"),
    (0x0B200000, 0x1F200000, 6, "addsub_ext"),
    (0x1A000000, 0x1FE0FC00, 5, "adc"),
    (0x3A400000, 0x3FE00C10, 4, "ccmp_reg"),
    (0x3A400800, 0x3FE00C10, 4, "ccmp_imm"),
    (0x1A800000, 0x3FE00800, 6, "csel"),
    (0x1AC00000, 0x7FE00000, 8, "dp2"),
    (0x5AC00000, 0x5FFF0000, 5, "dp1"),
    (0x1B000000, 0x7F000000, 8, "dp3"),
    (0x14000000, 0x7C000000, 2, "b_bl"),
    (0x34000000, 0x7E000000, 3, "cbz"),
    (0x36000000, 0x7E000000, 3, "tbz"),
    (0x54000000, 0xFF000010, 4, "bcond"),
    (0xD61F0000, 0xFFFFFC1F, 2, "br"),
    (0xD63F0000, 0xFFFFFC1F, 2, "blr"),
    (0xD65F0000, 0xFFFFFC1F, 1, "ret"),
    (0x08000000, 0x3F000000, 8, "excl_family"),
    (0x18000000, 0x3F000000, 2, "ldr_lit"),
    (0x28000000, 0x3C000000, 5, "pair"),
    (0x38000000, 0x3F200000, 10, "ldst_imm9"),
    (0x38200800, 0x3F200C00, 8, "ldst_reg"),
    (0x39000000, 0x3F000000, 8, "ldst_uimm"),
    (0x38200000, 0x3F200C00, 8, "lse_atomic"),
    (0xD503301F, 0xFFFFF01F, 3, "barrier"),
    (0xD503201F, 0xFFFFF01F, 2, "hint"),
    (0xD5087000, 0xFFFFFF00, 1, "sys_ic"),
    (0xD500401F, 0xFFFFFF9F, 2, "flagm"),  # CFINV / XAFLAG / AXFLAG (op2 = 3 se descarta)
    (0xBA000400, 0xFFE07C10, 2, "rmif"),
    (0x3A00080D, 0xFFFFBC1F, 2, "setf"),
    (0x1E204000, 0xFF207C00, 8, "fp_dp1"),
    (0x1E202000, 0xFF203C00, 6, "fp_cmp"),
    (0x1E200400, 0xFF200C00, 4, "fp_ccmp"),
    (0x1E200C00, 0xFF200C00, 4, "fp_csel"),
    (0x1E200800, 0xFF200C00, 8, "fp_dp2"),
    (0x1E201000, 0xFF201C00, 3, "fp_imm"),
    (0x1F000000, 0xFF000000, 8, "fp_dp3"),
    (0x1E200000, 0x7F20FC00, 8, "fp_conv"),
    (0, 0, 3, "fp_cvth"),  # FCVT/FCVTL/FCVTN con half y valores de borde (AHP), ver gen_cvth
    (0x1E000000, 0x7F200000, 6, "fp_fixed"),
    (0x0E200400, 0x9F200400, 20, "simd_3same"),
    (0x0E200000, 0x9F200C00, 8, "simd_3diff"),
    (0x0E200800, 0x9F3E0C00, 10, "simd_2misc"),
    (0x0E300800, 0x9F3E0C00, 5, "simd_across"),
    (0x0E000400, 0x9FE08400, 6, "simd_copy"),
    (0x0E000800, 0xBF208C00, 6, "simd_perm"),
    (0x2E000000, 0xBFE08400, 3, "simd_ext"),
    (0x0E000000, 0xBFE08C00, 4, "simd_tbl"),
    (0x0F000400, 0x9FF80400, 6, "simd_modimm"),
    (0x0F000400, 0x9F800400, 16, "simd_shift"),
    (0x0F000000, 0x9F000400, 14, "simd_indexed"),
    (0x0E008400, 0x9F208400, 6, "simd_3ext"),
    (0x5E200400, 0xDF200400, 8, "simd_s3same"),
    (0x5E200000, 0xDF200C00, 3, "simd_s3diff"),
    (0x5E200800, 0xDF200C00, 8, "simd_s2misc"),
    (0x5F000400, 0xDF800400, 10, "simd_sshift"),
    (0x5F000000, 0xDF000400, 8, "simd_sindexed"),
    (0x5E000400, 0xDFE08400, 3, "simd_scopy"),
    (0x5E008400, 0xDF208400, 2, "simd_s3ext"),
    (0x0E400400, 0x9F60C400, 10, "simd_h3same"),
    (0x0E780800, 0x9F7E0C00, 8, "simd_h2misc"),
    (0x0E20EC00, 0xBF60FC00, 3, "simd_fhm3"),
    (0x2E20CC00, 0xBF60FC00, 3, "simd_fhm3b"),
    (0x5E400400, 0xDF60C400, 4, "simd_sh3same"),
    (0x5E780800, 0xDF7E0C00, 6, "simd_sh2misc"),
    (0x5E300800, 0xDF7E0C00, 3, "simd_shpair"),
    (0x4E284800, 0xFFFFCC00, 8, "crypto_aes"),
    (0x5E280800, 0xFFFFCC00, 5, "crypto_sha2r"),
    (0x5E000000, 0xFFE08C00, 14, "crypto_sha3r"),
    (0xCE000000, 0xFFC08000, 4, "crypto_eor3"),
    (0xCE800000, 0xFFE00000, 3, "crypto_xar"),
    (0xCE608C00, 0xFFE0FC00, 2, "crypto_rax1"),
    (0xCE608000, 0xFFE0F800, 6, "crypto_sha512"),
    (0xCEC08000, 0xFFFFFC00, 2, "crypto_sha512su0"),
    (0x0E20E000, 0xBF20FC00, 8, "simd_pmull"),
    (0x3D000000, 0x3F000000, 6, "simd_ldst_uimm"),
    (0x3C000000, 0x3F200000, 6, "simd_ldst_imm9"),
    (0x3C200800, 0x3F200C00, 5, "simd_ldst_reg"),
    (0x1C000000, 0x3F000000, 2, "simd_ldst_lit"),
    (0x2C000000, 0x3E000000, 6, "simd_ldst_pair"),
    (0x0C000000, 0xBF000000, 14, "simd_ldst_multi"),
    (0x0D000000, 0xBF000000, 14, "simd_ldst_single"),
]

# MRS/MSR permitidos: (op0,op1,crn,crm,op2)
SYSREGS = [(3, 3, 13, 0, 2), (3, 3, 4, 2, 0), (3, 3, 4, 4, 0), (3, 3, 4, 4, 1)]


def gen_cvth(rng):
    """Conversiones de precision con half: FCVT escalar (H<->S/D), FCVTL/FCVTL2 (H->S), FCVTN/FCVTN2 (S->H)."""
    rd, rn = rng.randrange(32), rng.randrange(32)
    k = rng.randrange(6)
    if k < 4:
        ft, opc = [(3, 0), (3, 1), (0, 3), (1, 3)][k]
        return 0x1E224000 | (ft << 22) | (opc << 15) | (rn << 5) | rd
    q = rng.getrandbits(1)
    return (0x0E217800 if k == 4 else 0x0E216800) | (q << 30) | (rn << 5) | rd


def cvth_lane64(rng):
    """Valores para gen_cvth: half con exponente 31 (numeros con AHP), infinitos, NaN y bordes de 65504 y 131008."""
    def h():
        k = rng.random()
        if k < 0.3:
            return rng.choice([0x7C00, 0xFC00, 0x7FFF, 0xFFFF, 0x7E00, 0x7D00, 0x7C01, 0, 0x8000, 1, 0x3FF, 0x3C00, 0x7BFF])
        if k < 0.6:
            return (31 << 10) | rng.randrange(1024) | rng.choice([0, 0x8000])
        return rng.getrandbits(16)

    def f32():
        k = rng.random()
        if k < 0.2:
            return rng.choice([0x7F800000, 0xFF800000, 0x7FC00000, 0x7F800001, 0xFFA00000, 0, 0x80000000, 1, 0x00800000,
                               0x477FE000, 0x477FF000, 0x47800000, 0x47FFE000, 0x47FFF000, 0x48000000, 0x33800000])
        if k < 0.7:
            v = rng.choice([0x477FE000, 0x47FFE000, 0x47800000, 0x38800000, 0x33800000]) + rng.randrange(-0x3000, 0x3000)
            return v | rng.choice([0, 1 << 31])
        return rng.getrandbits(32)

    def f64():
        k = rng.random()
        if k < 0.2:
            return rng.choice([0x7FF0000000000000, 0xFFF0000000000000, 0x7FF8000000000000, 0x7FF0000000000001, 0, 1,
                               0x40FFFC0000000000, 0x40FFFE0000000000, 0x40FFFD0000000000, 0x4100000000000000,
                               0x40EFFC0000000000])
        if k < 0.7:
            v = rng.choice([0x40EFFC0000000000, 0x40FFFC0000000000, 0x40F0000000000000]) + rng.randrange(-1 << 42, 1 << 42)
            return v | rng.choice([0, 1 << 63])
        return rng.getrandbits(64)

    k = rng.randrange(3)
    if k == 0:
        return sum(h() << (16 * i) for i in range(4))
    if k == 1:
        return f32() | (f32() << 32)
    return f64()


def gen_word(rng, pool=None):
    pool = pool or CLASSES
    tot = sum(c[2] for c in pool)
    r = rng.randrange(tot)
    for val, mask, w, name in pool:
        if r < w:
            break
        r -= w
    if name == "fp_cvth":
        return gen_cvth(rng), name
    word = val | (rng.getrandbits(32) & ~mask & 0xFFFFFFFF)
    # (los filtros devuelven (None, nombre) para descartar el caso)
    if name in ("excl_family", "ldr_lit", "pair", "ldst_imm9", "ldst_reg", "ldst_uimm", "lse_atomic"):
        word &= ~(1 << 26)  # V=0 (enteros)
    if name == "simd_s2misc" and ((word >> 17) & 15) == 0b1100:
        return None, name  # FP16 escalar: Unicorn aborta (assert) con algunas codificaciones
    if (word & 0x9FF80400) == 0x0F000400 and (word >> 11) & 1 and (word >> 29) & 1:
        # MOVI/ORR... (inmediato modificado; tambien desde simd_shift con immh = 0) con o2=1 y op=1: no asignado en el
        # Arm ARM (capitulo C4, "Advanced SIMD modified immediate"); QEMU 5 lo ejecuta (laxo)
        return None, name
    if name == "simd_indexed" and (word >> 29) & 1 and ((word >> 12) & 9) == 1 and ((word >> 22) & 3) == 1 and not (word >> 30) & 1:
        return None, name  # FCMLA H por elemento con Q=0: QEMU lee/computa los lanes altos (bug del oraculo)
    if name == "simd_sshift" and ((word >> 19) & 15) in (2, 3) and ((word >> 11) & 31) == 31:
        return None, name  # FCVTZ* H escalar: QEMU extiende el resultado a 32 bits (el estandar: 16 bits, resto cero)
    if name == "simd_sh2misc":
        ua, a_, opc = (word >> 29) & 1, (word >> 23) & 1, (word >> 12) & 31
        ok = {(0,0,0x1a),(0,0,0x1b),(0,0,0x1c),(0,0,0x1d),(0,1,0x0c),(0,1,0x0d),(0,1,0x0e),(0,1,0x1a),(0,1,0x1b),(0,1,0x1d),(0,1,0x1f),
              (1,0,0x1a),(1,0,0x1b),(1,0,0x1c),(1,0,0x1d),(1,1,0x0c),(1,1,0x0d),(1,1,0x1a),(1,1,0x1b),(1,1,0x1d)}
        if (ua, a_, opc) not in ok:
            return None, name
    if name == "simd_h2misc":
        ua, a_, opc = (word >> 29) & 1, (word >> 23) & 1, (word >> 12) & 31
        ok = {(0,0,0x18),(0,0,0x19),(0,0,0x1a),(0,0,0x1b),(0,0,0x1c),(0,0,0x1d),(0,1,0x0c),(0,1,0x0d),(0,1,0x0e),(0,1,0x0f),(0,1,0x18),(0,1,0x19),(0,1,0x1a),(0,1,0x1b),(0,1,0x1d),
              (1,0,0x18),(1,0,0x19),(1,0,0x1a),(1,0,0x1b),(1,0,0x1c),(1,0,0x1d),(1,1,0x0c),(1,1,0x0d),(1,1,0x0f),(1,1,0x19),(1,1,0x1a),(1,1,0x1b),(1,1,0x1d),(1,1,0x1f)}
        if (ua, a_, opc) not in ok:
            return None, name
    if name in ("simd_h3same", "simd_sh3same"):
        ua, a_, opc = (word >> 29) & 1, (word >> 23) & 1, (word >> 11) & 7
        if name == "simd_h3same":
            ok = {(0,0,0),(0,0,1),(0,0,2),(0,0,3),(0,0,4),(0,0,6),(0,0,7),(0,1,0),(0,1,1),(0,1,2),(0,1,6),(0,1,7),
                  (1,0,0),(1,0,2),(1,0,3),(1,0,4),(1,0,5),(1,0,6),(1,0,7),(1,1,0),(1,1,2),(1,1,4),(1,1,5),(1,1,6)}
        else:
            ok = {(0,0,3),(0,0,4),(0,0,7),(0,1,7),(1,0,4),(1,0,5),(1,1,2),(1,1,4),(1,1,5)}
        if (ua, a_, opc) not in ok:
            return None, name
    if name == "simd_across" and not (word >> 29) & 1 and ((word >> 12) & 31) in (0x0c, 0x0f) and (word >> 22) & 1:
        return None, name  # FP16 across con size<0>=1: reservado; QEMU lo acepta
    if name == "simd_shpair" and ((word >> 12) & 31) not in (0x0c, 0x0d, 0x0f):
        return None, name
    if name == "simd_s2misc" and ((word >> 17) & 15) == 8 and not (word >> 29) & 1 and ((word >> 12) & 31) != 0x1b:
        return None, name  # pareado FP16 con bit22=1: reservado; QEMU lo acepta
    if name == "simd_ldst_multi":
        word &= ~(1 << 21)
    if name in ("simd_ldst_multi", "simd_ldst_single") and not (word >> 23) & 1:
        word &= ~(31 << 16)
    if name == "simd_ldst_pair":
        rt, rt2 = word & 31, (word >> 10) & 31
        if (word >> 22) & 1 and rt == rt2:
            word = (word & ~(31 << 10)) | (((rt2 + 1) % 32) << 10)
    if name in ("b_bl",):
        imm = rng.randrange(-8000, 8000) & 0x3FFFFFF
        word = (word & ~0x3FFFFFF) | imm
    if name in ("cbz", "bcond"):
        imm = rng.randrange(-8000, 8000) & 0x7FFFF
        word = (word & ~(0x7FFFF << 5)) | (imm << 5)
    if name in ("br", "blr", "ret"):
        pass  # el registro se fija en el bucle principal
    if name == "lse_atomic":
        opc = (word >> 12) & 7
        if opc in (4, 5) and (word >> 30) < 3:
            return None, name  # Unicorn (QEMU antiguo) implementa mal SMAX/SMIN de ancho < 64
    if name in ("ldst_imm9", "pair"):
        if name == "ldst_imm9":
            wb = ((word >> 10) & 3) in (1, 3)
        else:
            wb = ((word >> 23) & 3) in (1, 3)
        rt, rn = word & 31, (word >> 5) & 31
        if name == "pair":
            rt2 = (word >> 10) & 31
            if (word >> 22) & 1 and rt == rt2 and rt != 31:
                rt2 = (rt2 + 1) % 31
                word = (word & ~(31 << 10)) | (rt2 << 10)
            if wb and rt2 == rn and rn != 31:
                rt2 = (rt2 + 1) % 31 if (rt2 + 1) % 31 != rt else (rt2 + 2) % 31
                word = (word & ~(31 << 10)) | (rt2 << 10)
        if wb and rt == rn and rn != 31:
            word = (word & ~31) | ((rt + 1) % 31)
    if name == "dp3":
        if (word >> 21) & 7 in (2, 6):
            word = (word & ~(0x1F << 10) & ~(1 << 15)) | (31 << 10)
    if name == "excl_family":
        o2, l, o1 = (word >> 23) & 1, (word >> 22) & 1, (word >> 21) & 1
        def setf(w, lo, v):
            return (w & ~(0x1F << lo)) | (v << lo)
        if (o2, o1) == (0, 0):
            word = setf(word, 10, 31)
            if l:
                word = setf(word, 16, 31)
            else:
                rs_, rt_, rn_ = (word >> 16) & 31, word & 31, (word >> 5) & 31
                if rs_ == rt_ or rs_ == rn_:
                    word = setf(word, 16, (max(rt_, rn_) + 1) % 31 if max(rt_, rn_) != 31 else 0)
        elif (o2, o1) == (0, 1):
            if (word >> 31) & 1:
                if l:
                    word = setf(word, 16, 31)
                    if ((word >> 10) & 31) == (word & 31) and (word & 31) != 31:
                        word = setf(word, 10, ((word >> 10) + 1) % 31)
                else:
                    return None, name  # STXP aleatorio: se prueba en las secuencias
            else:
                word = setf(word, 10, 31)
                word = setf(word, 16, ((word >> 16) & 31) & ~1)
                word = setf(word, 0, (word & 31) & ~1)
        elif (o2, o1) == (1, 0):
            word = setf(setf(word, 10, 31), 16, 31)
        else:
            word = setf(word, 10, 31)
    if name == "flagm" and ((word >> 5) & 3) == 3:
        return None, name  # MSR (inmediato) op2=3 con CRm=0: no asignado
    if name == "sys_ic":
        # IC IALLU (0,7,5,0) es de EL1: en EL0 es SIGILL (Unicorn corre en EL1)
        op1, crn, crm, op2 = rng.choice([(3, 7, 5, 1), (3, 7, 4, 1), (3, 7, 10, 1), (3, 7, 11, 1), (3, 7, 14, 1)])
        word = 0xD5080000 | (op1 << 16) | (crn << 12) | (crm << 8) | (op2 << 5) | rng.randrange(31)
        name = "sys"
    return word, name


def gen_sysreg_word(rng):
    op0, op1, crn, crm, op2 = rng.choice(SYSREGS)
    l = rng.getrandbits(1)
    return 0xD5000000 | (l << 21) | (op0 << 19) | (op1 << 16) | (crn << 12) | (crm << 8) | (op2 << 5) | rng.randrange(31)


def gen_fpcr_pair(rng):
    """MSR FPCR/FPSR, Xt; MRS Xu, FPCR/FPSR: que bits se conservan (las trampas de FPCR son RAZ/WI)."""
    rt, ru = rng.sample(range(31), 2)
    enc = rng.choice([(3, 3, 4, 4, 0), (3, 3, 4, 4, 1)])  # FPCR, FPSR
    op0, op1, crn, crm, op2 = enc
    base = 0xD5000000 | (op0 << 19) | (op1 << 16) | (crn << 12) | (crm << 8) | (op2 << 5)
    return [base | rt, base | (1 << 21) | ru], rt


def gen_excl_seq(rng):
    """Secuencias LDXR/STXR (y pares) con interferencias tipicas."""
    rn = rng.randrange(0, 28)
    rt, rt2, rs = rng.sample([r for r in range(0, 28) if r != rn], 3)
    size = rng.randrange(4)
    pair = rng.random() < 0.25
    acq = rng.getrandbits(1)
    rel = rng.getrandbits(1)
    words = []
    if pair:
        sz = rng.getrandbits(1)  # 0: 32 bit, 1: 64 bit
        ldxp = 0x88600000 | (sz << 30) | (acq << 15) | (31 << 10) | (rn << 5) | rt
        ldxp = (ldxp & ~(0x1F << 10)) | (rt2 << 10) | (31 << 16)
        words.append(ldxp)
    else:
        ldx = 0x08400000 | (size << 30) | (31 << 16) | (acq << 15) | (31 << 10) | (rn << 5) | rt
        words.append(ldx)
    mid = rng.choice(["none", "none", "clrex", "other_ldx", "store"])
    other = None
    if mid == "clrex":
        words.append(0xD5033F5F)
    elif mid == "other_ldx":
        other = (rn + 1) % 28 if (rn + 1) % 28 not in (rt, rt2, rs) else (rn + 2) % 28
        words.append(0x08400000 | (3 << 30) | (31 << 16) | (31 << 10) | (other << 5) | rt)
    elif mid == "store":
        words.append(0xF9000000 | (rn << 5) | rt2)  # str xt2,[xn]
    if pair:
        stxp = 0x88200000 | (sz << 30) | (rel << 15) | (rt2 << 10) | (rn << 5) | rt | (rs << 16)
        words.append(stxp)
    else:
        stx = 0x08000000 | (size << 30) | (rs << 16) | (rel << 15) | (31 << 10) | (rn << 5) | rt2
        words.append(stx)
    return words, ("excl_pair" if pair else "excl_seq"), (rn, size, pair, other, mid == "store")


MIXSET = {"addsub_imm", "addsub_shift", "addsub_ext", "logic_imm", "logic_shift", "adc", "ccmp_reg",
          "ccmp_imm", "csel", "bcond", "cbz", "tbz", "movewide", "bitfield", "dp3", "dp2", "dp1",
          "extr", "adr"}
FLAGSET = {"addsub_imm", "addsub_shift", "addsub_ext", "logic_imm", "logic_shift"}
CONS = {"adc", "ccmp_reg", "ccmp_imm", "csel", "bcond"}


def gen_mix(rng):
    """Secuencia corta con flags: setters, consumidores y otras instrucciones mezcladas."""
    k = rng.randint(2, 10)
    words = []
    tries = 0
    while len(words) < k and tries < 200:
        tries += 1
        w, name = gen_word(rng)
        if w is None or name not in MIXSET:
            continue
        # sesgo: mas setters/consumidores
        if name not in FLAGSET and name not in CONS and rng.random() < 0.5:
            continue
        if name in ("bcond", "cbz", "tbz"):
            if len(words) == k - 1:
                words.append(w)  # el salto solo puede ser la ultima palabra
            continue
        words.append(w)
    if len(words) == k - 1 and rng.random() < 0.5:
        return None
    return words if len(words) == k else None


FPNAMES = ("fp", "neon", "simd", "crypto")

SPECIAL32 = [0x00000000, 0x80000000, 0x3F800000, 0xBF800000, 0x40000000, 0x7F800000, 0xFF800000, 0x7FC00000,
             0x7F800001, 0xFFC00000, 0x00000001, 0x007FFFFF, 0x00800000, 0x7F7FFFFF, 0x3F000000, 0x40490FDB,
             0xC2F60000, 0x4F000000, 0xCF000000, 0x7FA00000]
SPECIAL64 = [0x0000000000000000, 0x8000000000000000, 0x3FF0000000000000, 0xBFF0000000000000, 0x4000000000000000,
             0x7FF0000000000000, 0xFFF0000000000000, 0x7FF8000000000000, 0x7FF0000000000001, 0x0000000000000001,
             0x000FFFFFFFFFFFFF, 0x0010000000000000, 0x7FEFFFFFFFFFFFFF, 0x3FE0000000000000, 0x400921FB54442D18,
             0x43E0000000000000, 0xC3E0000000000000, 0x7FF4000000000000]
SPECIAL16 = [0x0000, 0x8000, 0x3C00, 0xBC00, 0x4000, 0x7C00, 0xFC00, 0x7E00, 0x7C01, 0x0001, 0x03FF, 0x0400,
             0x7BFF, 0x3800, 0x4248]


def rand_lane64(rng):
    k = rng.random()
    if k < 0.25:
        return rng.choice(SPECIAL64)
    if k < 0.45:
        return (rng.choice(SPECIAL32) << 32) | rng.choice(SPECIAL32)
    if k < 0.55:
        return sum(rng.choice(SPECIAL16) << (16 * i) for i in range(4))
    if k < 0.70:
        return rng.getrandbits(64) & 0x0101010101010101 * rng.choice([0xFF, 0x7F, 0x0F, 0x03, 0xFF])
    if k < 0.76:
        # cerca del underflow: productos y cocientes que caen junto al minimo normal (tininess antes/despues de
        # redondear, FZ)
        import struct as st
        if rng.random() < 0.5:
            e = rng.choice([-63, -64, -62, -126, -125, -127, 0, 63, 15, 16])
            a = st.unpack("<I", st.pack("<f", rng.uniform(1, 2) * rng.choice([1, -1]) * 2.0 ** e))[0]
            if rng.random() < 0.15:
                # borde del half (IEEE 65504/65520, alternativo con AHP 131008/131040)
                a = rng.choice([0x477FE000, 0x47FFE000]) + rng.randrange(-0x1000, 0x1800) | rng.choice([0, 1 << 31])
            b = st.unpack("<I", st.pack("<f", rng.uniform(1, 2) * rng.choice([1, -1]) * 2.0 ** rng.choice([-63, -64, -62, 0])))[0]
            return (b << 32) | a
        e = rng.choice([-511, -512, -510, -1022, -1021, -1023, 0, 511, 15, 16])
        return st.unpack("<Q", st.pack("<d", rng.uniform(1, 2) * rng.choice([1, -1]) * 2.0 ** e))[0]
    if k < 0.80:
        # floats normales de magnitud moderada
        import struct as st
        a = st.unpack("<I", st.pack("<f", rng.uniform(-100, 100)))[0]
        b = st.unpack("<I", st.pack("<f", rng.uniform(-100, 100)))[0]
        return (b << 32) | a
    if k < 0.88:
        import struct as st
        return st.unpack("<Q", st.pack("<d", rng.uniform(-1e3, 1e3)))[0]
    return rng.getrandbits(64)


def rand_vec_value(rng, name):
    if name == "fp_cvth":
        return cvth_lane64(rng) | (cvth_lane64(rng) << 64)
    if name.split("_")[0] in FPNAMES or name == "mixseq":
        return rand_lane64(rng) | (rand_lane64(rng) << 64)
    return 0


def rand_fpcr(rng, name):
    if name.split("_")[0] not in FPNAMES:
        return 0
    k = rng.random()
    if k < 0.6 and name != "fp_cvth":
        return 0
    v = rng.randrange(4) << 22  # RMode
    if name == "fp_cvth" and rng.random() < 0.6:
        v |= 1 << 26  # AHP
    if rng.random() < 0.3:
        v |= 1 << 24  # FZ
    if rng.random() < 0.3:
        v |= 1 << 25  # DN
    if rng.random() < 0.3:
        v |= 1 << 19  # FZ16 (half precision)
    if rng.random() < 0.3:
        v |= 1 << 26  # AHP (half alternativo en FCVT/FCVTL/FCVTN)
    return v


def rand_reg_value(rng, ptr_ok=True):
    k = rng.random()
    if k < 0.20 and ptr_ok:
        return MEM + 0x8000 + rng.randrange(0, 0x800) * (1 if rng.random() < 0.3 else 8)
    if k < 0.45:
        return rng.randrange(0, 256)
    if k < 0.60:
        return rng.choice([0, 1, M64, 0x7FFFFFFFFFFFFFFF, 0x8000000000000000, 0xFFFFFFFF, 0x80000000, 0x7FFFFFFF, 0x100000000])
    if k < 0.70:
        return rng.getrandbits(32)
    return rng.getrandbits(64)


# Clases con acceso a memoria por registro base (bits 9..5). ldr_lit/simd_ldst_lit son relativos al pc.
MEMCLS = {"excl_family", "pair", "ldst_imm9", "ldst_reg", "ldst_uimm", "lse_atomic", "simd_ldst_uimm",
          "simd_ldst_imm9", "simd_ldst_reg", "simd_ldst_pair", "simd_ldst_multi", "simd_ldst_single"}
# Probabilidad de que la base apunte a una guarda (caso de fallo esperado)
P_FAULT = 0.15


def in_guard(a, size=1):
    return any(lo < a + size and a < lo + GUARD for lo in (GUARD_LO, GUARD_HI))


def rand_addr(rng, fault):
    """Direccion base de un acceso: en MEM, o en una guarda si `fault`."""
    gran = rng.choice([1, 1, 4, 8, 8, 16])
    if fault:
        if rng.random() < 0.5:
            return GUARD_HI + rng.randrange(0, 0x4000) * gran % (GUARD - 0x100)
        return GUARD_LO + 0x100 + rng.randrange(0, 0x4000) * gran % (GUARD - 0x200)
    return MEM + 0x8000 + rng.randrange(0, 0x800) * gran


def is_excl_store(w):
    """STXR/STLXR/STXP/STLXP (o2 = 0, L = 0 en la familia de exclusivos)."""
    return (w >> 24) & 0x3F == 0x08 and not (w >> 23) & 1 and not (w >> 22) & 1


def is_ordered(w):
    """LDAR/STLR (o2 = 1, o1 = 0 en la familia de exclusivos). LDAPR no: Unicorn si comprueba su alineacion."""
    return (w >> 24) & 0x3F == 0x08 and (w >> 23) & 1 and not (w >> 21) & 1


def excl_access_size(w):
    """Tamano (alineacion exigida) de un exclusivo/ordenado de la familia 0x08: los pares, el doble."""
    size = 1 << (w >> 30)
    if not (w >> 23) & 1 and (w >> 21) & 1:
        size *= 2
    return size


# Exclusiones: (nombre, motivo). Cada caso excluido se cuenta y se imprime al final. Ver difftests/README.md.
EXCLUSIONES = {
    "parcial": "una instruccion con varios accesos falla tras completar alguno: el Arm ARM no fija el orden de los "
               "accesos de LDP/STP/LD1-4/ST1-4 y deja UNKNOWN la memoria y los registros destino que tocaba una "
               "instruccion que falla, asi que el estado parcial no esta determinado",
    "a_caballo": "acceso que empieza en MEM y entra en una guarda: Unicorn no detecta la falta de permisos en la "
                 "segunda pagina (lee o escribe) y el resultado no es el de ARM",
    "stxr_desalineado": "STXR/STXP desalineado: el Arm ARM exige la falta de alineacion antes de mirar el monitor "
                        "(AArch64.ExclusiveMonitorsPass llama a CheckAlignment primero); Unicorn (QEMU 5) solo "
                        "devuelve 1 en Ws",
    "stxr_guarda": "STXR/STXP sin reserva a una direccion sin permisos: el Arm ARM deja a la implementacion si se "
                   "detecta el fallo antes o despues del monitor (comentario de AArch64.ExclusiveMonitorsPass)",
    "fpcr_len_stride": "MSR FPCR con Len/Stride (bits 21:20 y 18:16): son RES0 sin AArch32 (heddle anuncia EL0 solo "
                       "AArch64 en ID_AA64PFR0_EL1, asi que los descarta); la CPU max de Unicorn tiene AArch32 y los "
                       "conserva para el FPSCR",
    "store_intermedio": "LDXR; STR a la misma direccion; STXR: el Arm ARM deja a la implementacion si un store normal "
                        "del mismo PE a la direccion marcada borra el monitor local. heddle siempre lo borra (STXR "
                        "devuelve 1); Unicorn (QEMU) compara valores y da 0 si el valor coincide por azar",
    "ordenado_desalineado": "LDAR/STLR desalineado: sin FEAT_LSE2 es falta de alineacion "
                            "(AArch64.CheckAlignment, accesos ordenados); Unicorn (QEMU 5) no la comprueba",
}


def base_value(xs, sp, w):
    rn = (w >> 5) & 31
    return sp if rn == 31 else xs[rn]


def main():
    out_path, n, seed = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    only = sys.argv[4].split(",") if len(sys.argv) > 4 else None
    rng = random.Random(seed)
    # estado de la instruccion en curso (ganchos de Unicorn)
    st = {"acc": [], "bad": None, "intr": None}

    def hook_code(uc, addr, size, ud):
        st["acc"] = []

    def hook_mem(uc, access, addr, size, value, ud):
        st["acc"].append((addr, size))

    def hook_bad(uc, access, addr, size, value, ud):
        if st["bad"] is None:
            st["bad"] = (addr, size, list(st["acc"]))
        return False

    def hook_intr(uc, intno, ud):
        if st["intr"] is None:
            st["intr"] = (intno, uc.reg_read(UC_ARM64_REG_PC))
        uc.emu_stop()

    def make_uc():
        uc = Uc(UC_ARCH_ARM64, UC_MODE_ARM)
        uc.ctl_set_cpu_model(UC_CPU_ARM64_MAX)
        uc.reg_write(UC_ARM64_REG_CPACR_EL1, 0x300000)  # habilita FP/NEON
        uc.mem_map(CODE_BASE, CODE_SIZE)
        uc.mem_write(CODE_BASE, struct.pack('<I', NOP) * (CODE_SIZE // 4))
        uc.mem_map(MEM, MEMSZ)
        uc.mem_map(GUARD_LO, GUARD, UC_PROT_NONE)
        uc.mem_map(GUARD_HI, GUARD, UC_PROT_NONE)
        uc.hook_add(UC_HOOK_CODE, hook_code)
        uc.hook_add(UC_HOOK_MEM_READ | UC_HOOK_MEM_WRITE, hook_mem)
        uc.hook_add(UC_HOOK_MEM_INVALID, hook_bad)
        uc.hook_add(UC_HOOK_INTR, hook_intr)
        return uc
    uc = make_uc()
    regs = [UC_ARM64_REG_X0 + i for i in range(29)] + [UC_ARM64_REG_X29, UC_ARM64_REG_X30]
    stats = {}
    excl = {}
    emitted = 0
    f = open(out_path, "wb")
    f.write(MAGIC)
    # con clases elegidas, solo se generan esas (pesos relativos: secuencias de exclusivos, MRS/MSR, secuencias con
    # flags e instrucciones sueltas)
    pool = [c for c in CLASSES if not only or c[3] in only or (c[3] == "sys_ic" and "sys" in only)]
    gens = [0.06, 0.02, 0.32, 0.60]
    if only:
        gens = [0.06 * bool({"excl_seq", "excl_pair"} & set(only)), 0.02 * ("sysreg" in only),
                0.32 * ("mixseq" in only), 0.60 * bool(pool)]
    gtot = sum(gens)
    if gtot == 0:
        sys.exit("ninguna clase conocida en: " + ",".join(only))
    attempts = 0
    while emitted < n and attempts < n * 20:
        attempts += 1
        r = rng.random() * gtot
        meta = None
        fpcr_rt = None
        if r < gens[0]:
            words, name, meta = gen_excl_seq(rng)
        elif r < gens[0] + gens[1]:
            words, name = [gen_sysreg_word(rng)], "sysreg"
            if rng.random() < 0.3:
                words, fpcr_rt = gen_fpcr_pair(rng)
        elif r < gens[0] + gens[1] + gens[2]:
            words, name = gen_mix(rng), "mixseq"
            if words is None:
                continue
        else:
            w, name = gen_word(rng, pool)
            if w is None:
                continue
            words = [w]
        if only and name not in only:
            continue
        xs = [rand_reg_value(rng) for _ in range(31)]
        if fpcr_rt is not None:
            xs[fpcr_rt] = rng.getrandbits(64) if rng.random() < 0.7 else rng.getrandbits(64) & ~0x00370000
        sp = MEM + 0x8000 + rng.randrange(0, 0x80) * 16
        nzcv = rng.getrandbits(4) << 28
        tpidr = rng.choice([0, rng.getrandbits(64), MEM + 0x100])
        if name in ("br", "blr", "ret"):
            rr = (words[0] >> 5) & 31
            if rr != 31:
                xs[rr] = CODE_BASE + 4 * rng.randrange(0, CODE_SIZE // 4 - 8)
        if name in MEMCLS:
            w = words[0]
            if name in ("ldst_reg", "simd_ldst_reg"):
                rm = (w >> 16) & 31
                if rm != 31:  # indice pequeno (tambien negativo)
                    xs[rm] = rng.randrange(0, 0x400) if rng.random() < 0.7 else (-rng.randrange(1, 0x400)) & M64
            rn = (w >> 5) & 31
            fault = rng.random() < P_FAULT and not (name == "excl_family" and is_excl_store(w))
            if rn != 31:
                xs[rn] = rand_addr(rng, fault)
        if name == "sys" and (words[0] & 31) != 31:
            xs[words[0] & 31] = rand_addr(rng, False)  # DC ZVA/CVAC/... e IC IVAU: siempre sobre MEM
        if meta:
            rn, size, pair, other, _ = meta
            if other is not None:
                xs[other] = MEM + 0x9000 + rng.randrange(0, 0x100) * 8  # base del LDXR intermedio
            xs[rn] = MEM + 0x8000 + rng.randrange(0, 0x100) * 16
            if rng.random() < P_FAULT / 2:
                xs[rn] = rand_addr(rng, True) & ~15  # LDXR/LDXP a una guarda (falla antes del STXR)
        # mem restore
        uc.mem_write(MEM, INIT)
        for i, v in enumerate(xs):
            uc.reg_write(regs[i], v)
        uc.reg_write(UC_ARM64_REG_SP, sp)
        uc.reg_write(UC_ARM64_REG_NZCV, nzcv)
        uc.reg_write(UC_ARM64_REG_TPIDR_EL0, tpidr)
        qs = [rand_vec_value(rng, name) for _ in range(32)]
        fpcr = rand_fpcr(rng, name)
        for i, qv in enumerate(qs):
            uc.reg_write(UC_ARM64_REG_Q0 + i, qv)
        uc.reg_write(UC_ARM64_REG_FPCR, fpcr)
        uc.reg_write(UC_ARM64_REG_FPSR, 0)
        uc.mem_write(CODE, b"".join(struct.pack("<I", w) for w in words) + struct.pack('<I', NOP) * 4)
        uc.reg_write(UC_ARM64_REG_PC, CODE)
        st["acc"], st["bad"], st["intr"] = [], None, None
        status = 0
        sig = None  # (signo, si_code, si_addr)
        err = None
        try:
            uc.emu_start(CODE, CODE + 4 * len(words), count=len(words))
        except UcError as e:
            err = e
        pc_now = uc.reg_read(UC_ARM64_REG_PC)
        idx = (pc_now - CODE) // 4
        cur = words[idx] if CODE <= pc_now < CODE + 4 * len(words) else None
        why = None  # motivo de exclusion
        if fpcr_rt is not None and not (words[0] >> 5) & 1 and xs[fpcr_rt] & 0x00370000 & ~(1 << 19):
            why = "fpcr_len_stride"
        if len(words) == 1 and (words[0] >> 24) & 0x3F == 0x08:
            # (antes de mirar lo que hizo Unicorn: con la base en una guarda da SIGSEGV en lugar de SIGBUS)
            w = words[0]
            a = base_value(xs, sp, w)  # el exclusivo es la unica instruccion: la base no cambio
            if (w >> 24) & 0x3F == 0x08 and is_excl_store(w):
                if a % excl_access_size(w):
                    why = "stxr_desalineado"
                elif in_guard(a, excl_access_size(w)):
                    why = "stxr_guarda"
            elif is_ordered(w) and a % excl_access_size(w):
                why = "ordenado_desalineado"
        if why is not None:
            pass
        elif st["bad"] is not None:
            addr, size, prev = st["bad"]
            if not in_guard(addr) or cur is None:
                why = "-"  # direccion sin mapear al azar (literal relativo al pc, etc.): no se usa
            elif any(a != addr for a, _ in prev):
                why = "parcial"
            else:
                sig = (SIGSEGV, SEGV_ACCERR, addr)
        elif st["intr"] is not None:
            intno, ipc = st["intr"]
            if intno == 1 and len(words) == 1:
                status = 1  # EXCP_UDEF: instruccion indefinida
            elif intno == 4 and cur is not None and ((cur >> 24) & 0x3F == 0x08 or (cur & 0x3B200C00) == 0x38200000):
                # EXCP_DATA_ABORT sin acceso a una guarda: falta de alineacion de un exclusivo/atomico. Unicorn no
                # rellena FAR: estas instrucciones no tienen desplazamiento, la direccion es la base
                rn = (cur >> 5) & 31
                base = uc.reg_read(UC_ARM64_REG_SP) if rn == 31 else uc.reg_read(regs[rn])
                sig = (SIGBUS, BUS_ADRALN, base)
                if not GUARD_LO <= base < GUARD_HI + GUARD:
                    why = "-"  # direccion al azar (sin mapear, quiza con etiqueta): no se usa
            else:
                why = "-"  # otra excepcion (alineacion del SP, BRK...): no se usa
        elif err is not None:
            if err.errno == UC_ERR_INSN_INVALID and len(words) == 1:
                status = 1
            else:
                why = "-"
        if why is None and sig is None and meta and meta[4] and uc.reg_read(regs[(words[-1] >> 16) & 31]) == 0:
            why = "store_intermedio"
        if why is None:
            # accesos a caballo entre MEM y una guarda que Unicorn dejo pasar
            if any(in_guard(a, s) for a, s in st["acc"]) and sig is None:
                why = "a_caballo"
        if why is not None:
            if why != "-":
                excl[why] = excl.get(why, 0) + 1
            if err is not None or st["intr"] is not None:
                uc = make_uc()  # una excepcion deja el estado de la CPU inservible
            continue
        if sig is not None:
            status = 2
        stats.setdefault(name, [0, 0, 0])[status] += 1
        rec = struct.pack("<I", len(words)) + b"".join(struct.pack("<I", w) for w in words)
        rec += struct.pack("<31Q", *xs) + struct.pack("<QQ", sp, nzcv)
        rec += b"".join(qv.to_bytes(16, "little") for qv in qs) + struct.pack("<QQ", fpcr, tpidr)
        rec += struct.pack("<I", status)
        if status == 2:
            rec += struct.pack("<IIQ", *sig)
        if status != 1:
            nx = [uc.reg_read(r_) for r_ in regs]
            nsp = uc.reg_read(UC_ARM64_REG_SP)
            nnz = uc.reg_read(UC_ARM64_REG_NZCV) & 0xF0000000
            npc = uc.reg_read(UC_ARM64_REG_PC)
            ntp = uc.reg_read(UC_ARM64_REG_TPIDR_EL0)
            after = bytes(uc.mem_read(MEM, MEMSZ))
            diffs = []
            if after != INIT:
                for page in range(0, MEMSZ, 4096):
                    if after[page:page + 4096] != INIT[page:page + 4096]:
                        for o in range(page, page + 4096, 8):
                            if after[o:o + 8] != INIT[o:o + 8]:
                                diffs.append((MEM + o, struct.unpack("<Q", after[o:o + 8])[0]))
            nfpsr = uc.reg_read(UC_ARM64_REG_FPSR)
            nq = [uc.reg_read(UC_ARM64_REG_Q0 + i) for i in range(32)]
            rec += struct.pack("<31Q", *nx) + struct.pack("<QQQQQ", nsp, nnz, npc, nfpsr, ntp)
            rec += b"".join(qv.to_bytes(16, "little") for qv in nq) + struct.pack("<I", len(diffs))
            for a, v in diffs:
                rec += struct.pack("<QQ", a, v)
        if err is not None or st["intr"] is not None:
            uc = make_uc()
        f.write(rec)
        emitted += 1
    f.close()
    print("casos:", emitted)
    for k in sorted(stats):
        print("  %-16s ejecutados=%-6d indefinidos=%-5d fallos=%d" % (k, stats[k][0], stats[k][1], stats[k][2]))
    for k in sorted(excl):
        print("  excluidos %-20s %-6d %s" % (k, excl[k], EXCLUSIONES[k]))


main()
