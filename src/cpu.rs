//! Estado de la CPU guest (AArch64) y flags perezosas.

use crate::monitor::Mon;

/// Tipos de flags perezosas (campo `lf_kind`). El JIT guarda los operandos de la ultima
/// instruccion que fija flags y las calcula solo cuando alguien las lee.
pub const LF_RAW: u64 = 0; // flags ya materializadas en `nzcv`
pub const LF_SUB64: u64 = 1; // a - b
pub const LF_SUB32: u64 = 2;
pub const LF_ADD64: u64 = 3; // a + b
pub const LF_ADD32: u64 = 4;
pub const LF_LOGIC64: u64 = 5; // a = resultado; C = V = 0
pub const LF_LOGIC32: u64 = 6;

pub const NZCV_N: u64 = 1 << 31;
pub const NZCV_Z: u64 = 1 << 30;
pub const NZCV_C: u64 = 1 << 29;
pub const NZCV_V: u64 = 1 << 28;

#[repr(C, align(64))]
pub struct Cpu {
    /// x0..x30; x[31] es SP. XZR no se almacena (se lee como 0 por el decodificador/ejecutor).
    pub x: [u64; 32],
    pub pc: u64,
    pub nzcv: u64,
    pub lf_kind: u64,
    pub lf_a: u64,
    pub lf_b: u64,
    pub tpidr: u64,
    pub fpcr: u64,
    pub fpsr: u64,
    pub mon: Mon,
    /// Registros vectoriales v0..v31 (128 bits como dos u64, little endian).
    pub v: [[u64; 2]; 32],
    /// Reservado para el runtime (codigo de salida, hilo, etc.)
    pub aux: [u64; 8],
    /// Marca "a mitad de store" del hilo (`*const monitor::MonFlag`, ver monitor.rs, via rapida por fence asimetrico):
    /// la del `GuestThread` dueno, compartida por las `Cpu` de sus manejadores de senal. 0 = sin marca (los stores
    /// van por la ruta con bloqueo).
    pub monflag: usize,
    /// RSP del host en el codigo guest en curso de esta Cpu: el del cuerpo del bloque traducido (lo apunta su prologo)
    /// o el de la entrada a `jit::heddle_call_block` (interprete); 0 fuera de el.
    /// Un fallo sincrono en ese codigo puede reanudarse volviendo ahi (ver sig.rs, "Reanudar tras un fallo").
    pub host_sp: u64,
    /// PSTATE.SSBS (FEAT_SSBS) como diferencia con el valor inicial de Linux (`feat::pstate_extra`): 0 = el inicial,
    /// asi una Cpu nueva o reiniciada empieza como un proceso recien creado. Ver `ssbs`/`set_ssbs`.
    pub ssbs_x: u64,
}

impl Cpu {
    pub fn new() -> Box<Cpu> {
        Box::new(Cpu::blank())
    }

    /// Cpu recien creada por valor (sin reservar memoria: el decodificador valida FP/NEON sobre una en la pila).
    pub const fn blank() -> Cpu {
        Cpu {
            x: [0; 32],
            pc: 0,
            nzcv: 0,
            lf_kind: LF_RAW,
            lf_a: 0,
            lf_b: 0,
            tpidr: 0,
            fpcr: 0,
            fpsr: 0,
            mon: Mon::new(),
            v: [[0; 2]; 32],
            aux: [0; 8],
            monflag: 0,
            host_sp: 0,
            ssbs_x: 0,
        }
    }

    /// Deja la Cpu como recien creada sin reservar memoria (Cpu de los manejadores de senal, ya reservadas).
    pub fn reset(&mut self) {
        self.x = [0; 32];
        self.pc = 0;
        self.nzcv = 0;
        self.lf_kind = LF_RAW;
        self.lf_a = 0;
        self.lf_b = 0;
        self.tpidr = 0;
        self.fpcr = 0;
        self.fpsr = 0;
        self.mon = Mon::new();
        self.v = [[0; 2]; 32];
        self.aux = [0; 8];
        self.monflag = 0;
        self.host_sp = 0;
        self.ssbs_x = 0;
    }

    /// PSTATE.SSBS en su posicion del SPSR (bit 12): 0 o `1 << 12`.
    pub fn ssbs(&self) -> u64 {
        (crate::feat::pstate_extra() ^ self.ssbs_x) & (1 << 12)
    }

    /// Escribe PSTATE.SSBS (`v`: bit 12; el resto se ignora).
    pub fn set_ssbs(&mut self, v: u64) {
        self.ssbs_x = (v ^ crate::feat::pstate_extra()) & (1 << 12);
    }

    /// Valor de NZCV (bits 31..28), materializando las flags perezosas.
    pub fn flags(&self) -> u64 {
        let (a, b) = (self.lf_a, self.lf_b);
        match self.lf_kind {
            LF_RAW => self.nzcv,
            LF_SUB64 => add_flags(a, !b, 1, true).1,
            LF_SUB32 => add_flags(a as u32 as u64, !(b as u32) as u64, 1, false).1,
            LF_ADD64 => add_flags(a, b, 0, true).1,
            LF_ADD32 => add_flags(a as u32 as u64, b as u32 as u64, 0, false).1,
            LF_LOGIC64 => logic_flags(a, true),
            LF_LOGIC32 => logic_flags(a, false),
            _ => self.nzcv,
        }
    }

    pub fn set_flags(&mut self, nzcv: u64) {
        self.nzcv = nzcv & 0xF000_0000;
        self.lf_kind = LF_RAW;
    }

    pub fn cond_holds(&self, cond: u8) -> bool {
        cond_holds(cond, self.flags())
    }
}

#[inline]
pub fn logic_flags(res: u64, sf: bool) -> u64 {
    let (n, z) = if sf {
        ((res >> 63) & 1, (res == 0) as u64)
    } else {
        ((res >> 31) & 1, ((res as u32) == 0) as u64)
    };
    (n << 31) | (z << 30)
}

/// AddWithCarry: devuelve (resultado, NZCV en bits 31..28).
#[inline]
pub fn add_flags(a: u64, b: u64, cin: u64, sf: bool) -> (u64, u64) {
    if sf {
        let (s1, c1) = a.overflowing_add(b);
        let (res, c2) = s1.overflowing_add(cin);
        let c = (c1 | c2) as u64;
        let v = ((!(a ^ b) & (a ^ res)) >> 63) & 1;
        let n = res >> 63;
        let z = (res == 0) as u64;
        (res, (n << 31) | (z << 30) | (c << 29) | (v << 28))
    } else {
        let (a, b) = (a as u32, b as u32);
        let (s1, c1) = a.overflowing_add(b);
        let (res, c2) = s1.overflowing_add(cin as u32);
        let c = (c1 | c2) as u64;
        let v = (((!(a ^ b) & (a ^ res)) >> 31) & 1) as u64;
        let n = (res >> 31) as u64;
        let z = (res == 0) as u64;
        (res as u64, (n << 31) | (z << 30) | (c << 29) | (v << 28))
    }
}

#[inline]
pub fn cond_holds(cond: u8, f: u64) -> bool {
    let n = f & NZCV_N != 0;
    let z = f & NZCV_Z != 0;
    let c = f & NZCV_C != 0;
    let v = f & NZCV_V != 0;
    let r = match cond >> 1 {
        0 => z,
        1 => c,
        2 => n,
        3 => v,
        4 => c && !z,
        5 => n == v,
        6 => n == v && !z,
        _ => true,
    };
    if cond & 1 == 1 && cond != 0b1111 {
        !r
    } else {
        r
    }
}
