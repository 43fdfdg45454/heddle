//! JIT x86-64 para A64.
//!
//! Diseno:
//!  * Un bloque = secuencia de instrucciones guest hasta un salto/SVC/BRK/IC (max 64, sin cruzar pagina).
//!  * Funcion generada: `extern "C" fn(cpu: *mut Cpu) -> u64`. RBX = &Cpu durante todo el bloque.
//!    El estado guest vive en memoria (`Cpu`), siempre completo; los registros x86 son temporales
//!    (RAX/RCX/RDX/RSI/RDI/R8-R11) o copias de lectura de registros ARM (R12-R15/RBP, ver "Registros"). Devuelve un estado: 0 = seguir en `cpu.pc`, 1 = SVC, 2 = BRK,
//!    3 = indefinida, 4 = IC (invalidar cache de bloques); el dato asociado queda en `cpu.aux[1]`.
//!  * Flags perezosas: SUBS/ADDS/ANDS guardan (kind, a, b) en el Cpu; los consumidores dentro del
//!    bloque con kind conocido re-ejecutan `cmp/add/test` (o usan las flags x86 aun vivas) y el codigo de
//!    condicion x86 equivalente; sin kind conocido llaman al helper.
//!  * Instrucciones poco frecuentes o complejas (exclusivas, LSE, CRC, sistema...) se ejecutan con un
//!    helper que llama al interprete de referencia (`interp::exec`), por lo que la semantica es
//!    identica por construccion. Todo store de memoria pasa por `monitor::store` (monitor estricto K).
//!  * Dispatcher con cache directa + tabla hash; invalidacion por IC IVAU/IC IALLU.
//!
//! Encadenado directo de bloques (solo fuera del modo contado, que sigue sin encadenar):
//!  * Cada salida con destino constante (B, BL, B.cond, CBZ/CBNZ, TBZ/TBNZ y el fin de bloque por limite) escribe
//!    `cpu.pc = destino` (igual que antes) y luego ejecuta un `jmp rel32` parcheable. Sin parchear, el `jmp` salta
//!    a la instruccion siguiente, que es el epilogo normal (`mov eax,0; pop rbx; ret`): vuelve a
//!    `run_inner` exactamente como sin encadenar. Parcheado, salta al codigo del bloque destino SALTANDO su prologo
//!    (PROLOGUE_LEN bytes, comprobado al compilar): la pila y RBX quedan coherentes y
//!    el `pop rbx; ret` del ultimo bloque de la cadena devuelve a `run_inner`.
//!  * El parche se aplica cuando existen ambos extremos: al registrar un bloque se parchean sus sitios cuyo destino ya
//!    esta en `map` y los sitios pendientes de otros bloques que apuntaban a su pc. Nunca se encadena a una region
//!    HLE, a codigo del host (`is_host_code`) ni al bloque `host_call_block`.
//!  * Equivalencia: el estado observable (`Cpu`, incluido `pc`) es identico al no encadenado en todo punto
//!    observable (salidas a run_inner, helpers, HLE, SVC): cada salida escribe `pc` antes de saltar, de modo que
//!    `pc` es siempre el inicio del bloque en curso (o, en una salida, el destino). Las senales asincronas se
//!    entregan en esas salidas (ver "Senales" abajo).
//!  * Invalidacion: `flush_all` vacia los registros de sitios (se descarta todo el bufer); `invalidate` desparcha
//!    (vuelve al epilogo) todo `jmp` cuyo destino ya no esta en `map` y lo devuelve a pendiente.
//!  * EPOCH: un hilo que gira en un bucle encadenado no volveria a `run_inner` y no veria el cambio de EPOCH de otro
//!    hilo (IC IVAU; la linea invalidada va en el anillo `IC_LOG` y solo se retraducen sus bloques). Para no
//!    debilitar la garantia actual (cada bloque releia EPOCH), los saltos encadenados hacia ATRAS (destino <= pc
//!    del salto; todo ciclo contiene al menos uno) comparan el EPOCH global con la copia que `run_inner` guarda
//!    en `cpu.aux[5]` y, si difiere, salen a `run_inner` (pc ya escrito). Los saltos hacia delante no comprueban:
//!    el ciclo siempre pasa por un salto atras. Coste: ~4 instrucciones por salto atras.
//!  * `HEDDLE_NOCHAIN=1` (o `Jit::chain = false`) desactiva el encadenado (pruebas y microbenchmark).
//!
//! Senales (entrega en frontera de instruccion, ver sig.rs "Senales asincronas"): el manejador del host de una senal
//! que llega con el hilo en codigo guest solo la anota (`GuestThread::sig_pend`) y escribe `SIG_POISON` en
//! `cpu.aux[5]` del Cpu principal. No hay comprobacion nueva en el codigo traducido: el salto hacia atras ya compara
//! EPOCH con aux[5] y sale a `run_inner` con el pc escrito; `run_inner` compara aux[5] con su EPOCH antes de entrar a
//! cada bloque (una lectura del Cpu) y devuelve `Event::Signal`. Latencia: hasta el siguiente salto hacia atras o
//! salida del bloque (un bloque sin ciclos tiene como mucho MAX_BLOCK instrucciones).
//!
//! Registros (cache de registros ARM en registros x86 dentro del bloque, paso 5):
//!  * Variante elegida: cache de LECTURA con escritura inmediata ("write-through"). Cada bloque elige por conteo
//!    estatico (`alloc_regs`: usos por `ld_x`/`st_x`, al menos 2) hasta 5 registros ARM y les da una ranura en
//!    R12, R13, R14, R15, RBP (callee-saved: los helpers y las rutinas del monitor los conservan). RBX sigue siendo
//!    &Cpu. `ld_x` lee la ranura si es valida; si no, carga de memoria y la llena. `st_x` escribe SIEMPRE el Cpu y
//!    ademas la ranura. Asi la secuencia de escrituras en el Cpu es IDENTICA a la del JIT sin cache (solo cambia de
//!    donde salen las lecturas) y el Cpu esta completo en todo punto, igual que antes: salidas, helpers, HLE, fallos
//!    sincronos (fault_host/native_bridge_signal leen el Cpu) y senales asincronas (build_frame). No hay volcado en
//!    las salidas ni antes de las llamadas, ni nada que reconstruir en los manejadores.
//!  * Invariante (conocido al traducir, `Ctx::valid`): una ranura valida contiene el valor de `Cpu.x[r]`. Se mantiene
//!    porque (a) toda escritura de un registro x en codigo generado pasa por `st_x` (emit_ea/post_wb/wb_emit
//!    incluidos), salvo MOVK (`mov` de 16 bits a memoria) que invalida con `forget`; (b) tras una instruccion que
//!    llamo a h_exec (el interprete escribe cualquier registro) se invalidan todas; h_cond, h_store, heddle_fst* y
//!    heddle_rst* no escriben registros x; (c) en jitfp/jitneon (rutas con ramas internas y ruta lenta) `fill = false`: solo se
//!    leen ranuras ya validas y `st_x` invalida en vez de llenar (una validez marcada en una rama no vale en la
//!    otra). Los registros base de sus loads/stores se cargan antes, en la frontera de instruccion (`prefill`).
//!    Fuera de jitfp/jitneon el codigo de una instruccion es lineal (solo saltos hacia delante a salidas), asi que la
//!    validez conocida al traducir vale en todos los caminos.
//!  * Marco de pila: prologo comun `push rbx, r12-r15, rbp, rax(relleno); mov rbx, rdi; mov [rbx+host_sp], rsp`
//!    + nop de relleno (PROLOGUE_LEN = 30; host_sp: punto de reanudacion de un fallo sincrono, ver `heddle_jit_abort`) y
//!    epilogo simetrico; todos los bloques tienen el mismo marco, asi que el encadenado (que entra tras el prologo)
//!    no cambia. RSP queda alineado a 16 en las llamadas.
//!  * Flags x86 vivas (`xf`): si la instruccion anterior fijo flags perezosas con la propia `sub`/`add`/`and` (mismos
//!    operandos y ancho que guardo en lf_a/lf_b, seguida solo de `mov`) y la actual es B.cond/CSEL, se usan esas
//!    flags x86 en lugar de recargar lf_a/lf_b y repetir `cmp`/`test`. Los stores de lf_* se hacen igual.
//!  * Ya no se eliminan stores de flags "muertas" (sobrescritas mas adelante en el bloque): un fallo sincrono o una
//!    senal entre medias veia NZCV de una instruccion anterior (lo detecto la prueba de fallo sincrono).
//!  * Lo que sigue sin ser exacto (previo, documentado): el `pc` del Cpu dentro de un bloque es el de su inicio (o el
//!    de la ultima instruccion que fue al helper); las flags acumuladas de FPSR de las rutas rapidas FP viven en
//!    MXCSR hasta `fold_mxcsr`; un load/store con pre-indice que falla ya escribio la base (igual en el interprete).
//!  * `HEDDLE_NOREGS=1` (o `Jit::regs = false` antes de traducir) desactiva la cache y las flags vivas.
//!  * Pruebas: `regs_tests` (jit_regs_tests.rs): diferencial con el interprete, con el JIT sin cache y con run_n;
//!    fallo sincrono (Cpu visto por el manejador); microbenchmark de bucles del banco.

use crate::cpu::*;
use crate::decode::*;
use crate::interp::{self, Flow, TBI_MASK};
use crate::monitor;
use std::collections::HashMap;
use std::mem::offset_of;

#[path = "jitfp.rs"]
mod jitfp;
#[path = "jitneon.rs"]
mod jitneon;
use crate::profg;

const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
const RBP: u8 = 5;
const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;
/// Registros x86 callee-saved que guardan registros ARM dentro de un bloque (ver cabecera, "Registros").
const HREGS: [u8; 5] = [R12, R13, R14, R15, RBP];
/// Ninguna ranura asignada.
const NO_SLOT: u8 = 0xFF;

const O_PC: i32 = offset_of!(Cpu, pc) as i32;
const O_KIND: i32 = offset_of!(Cpu, lf_kind) as i32;
const O_LFA: i32 = offset_of!(Cpu, lf_a) as i32;
const O_LFB: i32 = offset_of!(Cpu, lf_b) as i32;
const O_TPIDR: i32 = offset_of!(Cpu, tpidr) as i32;
const O_AUX: i32 = offset_of!(Cpu, aux) as i32;
const O_MONFLAG: i32 = offset_of!(Cpu, monflag) as i32;
const O_AUX_COUNT: i32 = O_AUX + 16; // aux[2]: instrucciones ejecutadas (modo contado)
const O_AUX_SCRATCH: i32 = O_AUX + 32; // aux[4]
const O_AUX_EPOCH: i32 = O_AUX + 40; // aux[5]: copia por hilo de EPOCH (la deja run_inner), para los saltos encadenados hacia atras
/// Longitud del prologo de cada bloque (`push rbx; push r12..r15; push rbp; push rax; mov rbx, rdi;
/// mov [rbx+host_sp], rsp`): los saltos encadenados entran despues de el (todos los bloques tienen el mismo marco de
/// pila).
/// pila). Lleva un NOP de 9 bytes de relleno: los bloques empiezan alineados a 16 y el cuerpo (destino de los saltos
/// encadenados) queda en 14 mod 16, donde quedaba con el prologo de 14 bytes; con 21 (o 32) el bucle de lcg iba un 3-4 %
/// mas lento con el mismo numero de instrucciones (alineacion del destino del salto hacia atras).
const PROLOGUE_LEN: usize = 30;
const PROLOGUE_PAD: [u8; 9] = [0x66, 0x0F, 0x1F, 0x84, 0x00, 0x00, 0x00, 0x00, 0x00];
const O_HOST_SP: i32 = offset_of!(Cpu, host_sp) as i32;
/// Cota de sitios de encadenado registrados por Jit (los que no caben quedan sin encadenar).
const SITES_CAP: usize = 1 << 16;

const CC_O: u8 = 0;
const CC_NO: u8 = 1;
const CC_B: u8 = 2;
const CC_AE: u8 = 3;
const CC_E: u8 = 4;
const CC_NE: u8 = 5;
const CC_BE: u8 = 6;
const CC_A: u8 = 7;
const CC_S: u8 = 8;
const CC_NS: u8 = 9;
const CC_L: u8 = 0xC;
const CC_GE: u8 = 0xD;
const CC_LE: u8 = 0xE;
const CC_G: u8 = 0xF;

// ---------------------------------------------------------------------------------------------
// Ensamblador minimo
// ---------------------------------------------------------------------------------------------

struct Asm {
    b: Vec<u8>,
}

impl Asm {
    fn new() -> Asm {
        Asm { b: Vec::with_capacity(4096) }
    }
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn rex(&mut self, w: bool, reg: u8, rm: u8, force: bool) {
        let mut r = 0x40u8;
        if w {
            r |= 8;
        }
        if reg >= 8 {
            r |= 4;
        }
        if rm >= 8 {
            r |= 1;
        }
        if r != 0x40 || force {
            self.u8(r);
        }
    }
    fn modrm_rr(&mut self, reg: u8, rm: u8) {
        self.u8(0xC0 | ((reg & 7) << 3) | (rm & 7));
    }
    fn modrm_mem(&mut self, reg: u8, base: u8, disp: i32) {
        let b7 = base & 7;
        let (md, d8, d32) = if disp == 0 && b7 != 5 {
            (0u8, false, false)
        } else if (-128..128).contains(&disp) {
            (1, true, false)
        } else {
            (2, false, true)
        };
        self.u8((md << 6) | ((reg & 7) << 3) | b7);
        if b7 == 4 {
            self.u8(0x24);
        }
        if d8 {
            self.u8(disp as i8 as u8);
        }
        if d32 {
            self.u32(disp as u32);
        }
    }
    fn op_rr(&mut self, w: bool, opc: &[u8], reg: u8, rm: u8, force: bool) {
        self.rex(w, reg, rm, force);
        for &o in opc {
            self.u8(o);
        }
        self.modrm_rr(reg, rm);
    }
    fn op_rm(&mut self, w: bool, opc: &[u8], reg: u8, base: u8, disp: i32) {
        self.rex(w, reg, base, false);
        for &o in opc {
            self.u8(o);
        }
        self.modrm_mem(reg, base, disp);
    }

    // --- movimientos ---
    fn mov_rm(&mut self, dst: u8, base: u8, disp: i32, w: bool) {
        self.op_rm(w, &[0x8B], dst, base, disp);
    }
    fn mov_mr(&mut self, base: u8, disp: i32, src: u8, w: bool) {
        self.op_rm(w, &[0x89], src, base, disp);
    }
    fn mov_rr(&mut self, dst: u8, src: u8, w: bool) {
        self.op_rr(w, &[0x89], src, dst, false);
    }
    /// mov inmediato; nunca usa XOR (no toca las flags x86).
    fn mov_ri(&mut self, dst: u8, imm: u64) {
        if imm <= u32::MAX as u64 {
            self.rex(false, 0, dst, false);
            self.u8(0xB8 | (dst & 7));
            self.u32(imm as u32);
        } else if (imm as i64) >= i32::MIN as i64 && (imm as i64) < 0 {
            self.rex(true, 0, dst, false);
            self.u8(0xC7);
            self.modrm_rr(0, dst);
            self.u32(imm as u32);
        } else {
            self.rex(true, 0, dst, false);
            self.u8(0xB8 | (dst & 7));
            self.u64(imm);
        }
    }
    /// mov qword [base+disp], imm32 (extendido con signo)
    fn mov_mi64(&mut self, base: u8, disp: i32, imm: i32) {
        self.op_rm(true, &[0xC7], 0, base, disp);
        self.u32(imm as u32);
    }
    fn mov_mi32(&mut self, base: u8, disp: i32, imm: u32) {
        self.op_rm(false, &[0xC7], 0, base, disp);
        self.u32(imm);
    }
    fn mov_mi16(&mut self, base: u8, disp: i32, imm: u16) {
        self.u8(0x66);
        self.op_rm(false, &[0xC7], 0, base, disp);
        self.u16(imm);
    }
    fn lea(&mut self, dst: u8, base: u8, disp: i32, w: bool) {
        self.op_rm(w, &[0x8D], dst, base, disp);
    }
    fn load_ext(&mut self, dst: u8, base: u8, disp: i32, size: u8, sext: u8) {
        match (size, sext) {
            (0, 0) => self.op_rm(false, &[0x0F, 0xB6], dst, base, disp),
            (0, 32) => self.op_rm(false, &[0x0F, 0xBE], dst, base, disp),
            (0, _) => self.op_rm(true, &[0x0F, 0xBE], dst, base, disp),
            (1, 0) => self.op_rm(false, &[0x0F, 0xB7], dst, base, disp),
            (1, 32) => self.op_rm(false, &[0x0F, 0xBF], dst, base, disp),
            (1, _) => self.op_rm(true, &[0x0F, 0xBF], dst, base, disp),
            (2, 64) => self.op_rm(true, &[0x63], dst, base, disp),
            (2, _) => self.op_rm(false, &[0x8B], dst, base, disp),
            _ => self.op_rm(true, &[0x8B], dst, base, disp),
        }
    }
    /// Extiende en sitio segun `Ext` del guest.
    fn ext_rr(&mut self, r: u8, e: Ext) {
        match e {
            Ext::Uxtb => self.op_rr(false, &[0x0F, 0xB6], r, r, true),
            Ext::Uxth => self.op_rr(false, &[0x0F, 0xB7], r, r, false),
            Ext::Uxtw => self.mov_rr(r, r, false),
            Ext::Sxtb => self.op_rr(true, &[0x0F, 0xBE], r, r, true),
            Ext::Sxth => self.op_rr(true, &[0x0F, 0xBF], r, r, false),
            Ext::Sxtw => self.op_rr(true, &[0x63], r, r, false),
            Ext::Uxtx | Ext::Sxtx => {}
        }
    }

    // --- aritmetica ---
    /// opc: 0x01 add, 0x09 or, 0x21 and, 0x29 sub, 0x31 xor, 0x39 cmp, 0x85 test
    fn alu_rr(&mut self, w: bool, opc: u8, dst: u8, src: u8) {
        self.op_rr(w, &[opc], src, dst, false);
    }
    /// ext: add 0, or 1, adc 2, sbb 3, and 4, sub 5, xor 6, cmp 7
    fn alu_ri(&mut self, w: bool, ext: u8, r: u8, imm: i32) {
        if (-128..128).contains(&imm) {
            self.op_rr(w, &[0x83], ext, r, false);
            self.u8(imm as i8 as u8);
        } else {
            self.op_rr(w, &[0x81], ext, r, false);
            self.u32(imm as u32);
        }
    }
    /// Operacion con inmediato arbitrario de hasta 64 bits (usa `tmp` si no cabe en imm32).
    fn alu_imm_any(&mut self, w: bool, opc_rr: u8, ext: u8, r: u8, imm: u64, tmp: u8) {
        if w {
            if (imm as i64) >= i32::MIN as i64 && (imm as i64) <= i32::MAX as i64 {
                self.alu_ri(true, ext, r, imm as i64 as i32);
            } else {
                self.mov_ri(tmp, imm);
                self.alu_rr(true, opc_rr, r, tmp);
            }
        } else {
            self.alu_ri(false, ext, r, imm as u32 as i32);
        }
    }
    fn shift_ri(&mut self, w: bool, ext: u8, r: u8, imm: u8) {
        self.op_rr(w, &[0xC1], ext, r, false);
        self.u8(imm);
    }
    fn shift_cl(&mut self, w: bool, ext: u8, r: u8) {
        self.op_rr(w, &[0xD3], ext, r, false);
    }
    fn unary(&mut self, w: bool, ext: u8, r: u8) {
        self.op_rr(w, &[0xF7], ext, r, false);
    }
    fn imul_rr(&mut self, w: bool, dst: u8, src: u8) {
        self.op_rr(w, &[0x0F, 0xAF], dst, src, false);
    }
    fn cmov(&mut self, w: bool, cc: u8, dst: u8, src: u8) {
        self.op_rr(w, &[0x0F, 0x40 | cc], dst, src, false);
    }
    fn bswap(&mut self, w: bool, r: u8) {
        self.rex(w, 0, r, false);
        self.u8(0x0F);
        self.u8(0xC8 | (r & 7));
    }
    fn shrd_ri(&mut self, w: bool, dst: u8, src: u8, imm: u8) {
        self.op_rr(w, &[0x0F, 0xAC], src, dst, false);
        self.u8(imm);
    }
    fn bt_ri(&mut self, r: u8, bit: u8) {
        self.op_rr(true, &[0x0F, 0xBA], 4, r, false);
        self.u8(bit);
    }
    fn mfence(&mut self) {
        self.b.extend_from_slice(&[0x0F, 0xAE, 0xF0]);
    }

    // --- control ---
    fn jcc_fwd(&mut self, cc: u8) -> usize {
        self.u8(0x0F);
        self.u8(0x80 | cc);
        self.u32(0);
        self.b.len() - 4
    }
    fn jmp_fwd(&mut self) -> usize {
        self.u8(0xE9);
        self.u32(0);
        self.b.len() - 4
    }
    fn patch_here(&mut self, pos: usize) {
        let rel = (self.b.len() - (pos + 4)) as u32;
        self.b[pos..pos + 4].copy_from_slice(&rel.to_le_bytes());
    }
    fn call_abs(&mut self, f: usize) {
        self.mov_ri(RAX, f as u64);
        self.u8(0xFF);
        self.u8(0xD0);
    }
    /// Prologo comun a todos los bloques: guarda RBX y los registros de la cache (callee-saved del ABI de C), mas
    /// un hueco de 8 bytes para que RSP quede alineado a 16 en las llamadas a helpers (entrada: RSP = 8 mod 16;
    /// 7 push = 56 bytes). RBX = &Cpu. Apunta el RSP del cuerpo en `cpu.host_sp` (punto de reanudacion de un fallo
    /// sincrono, ver `heddle_jit_abort`).
    fn prologue(&mut self) {
        self.u8(0x53); // push rbx
        self.b.extend_from_slice(&[0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57]); // push r12..r15
        self.u8(0x55); // push rbp
        self.u8(0x50); // push rax (relleno de alineacion)
        self.mov_rr(RBX, RDI, true);
        self.b.extend_from_slice(&[0x48, 0x89, 0xA3]); // mov [rbx+disp32], rsp
        self.b.extend_from_slice(&O_HOST_SP.to_le_bytes());
        self.b.extend_from_slice(&PROLOGUE_PAD[..PROLOGUE_LEN - 21]);
    }
    /// Epilogo: deshace `prologue` (RAX = estado de salida; el relleno se descarta en RCX) y vuelve.
    fn ret_epilogue(&mut self) {
        self.u8(0x59); // pop rcx (relleno)
        self.u8(0x5D); // pop rbp
        self.b.extend_from_slice(&[0x41, 0x5F, 0x41, 0x5E, 0x41, 0x5D, 0x41, 0x5C]); // pop r15..r12
        self.u8(0x5B); // pop rbx
        self.u8(0xC3);
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers llamados desde el codigo generado
// ---------------------------------------------------------------------------------------------

/// Numero de instrucciones ejecutadas por el interprete de referencia desde el JIT (diagnostico). Solo cuenta con
/// HELPER_COUNT_ON (pruebas y HEDDLE_STATS de heddle-run; nadie mas lo lee): un contador global que todos los hilos incrementan en cada instruccion
/// interpretada es contencion pura (la linea va y viene entre nucleos y arrastra a las variables vecinas que se leen
/// en cada despacho, como EPOCH). Medido con xbench (8 hilos): LDADD en lineas distintas 119 -> 21 ns por operacion.
pub static HELPER_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Activa HELPER_CALLS (por defecto solo en las pruebas; el microbenchmark xbench lo apaga para medir como en produccion).
pub static HELPER_COUNT_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(cfg!(test));

/// Op interpretado desde el JIT junto con el origen de la llamada (solo para el perfilador, ver profg.rs). `op` va
/// primero y la estructura es repr(C): el helper recibe un `*const Op` que apunta a un `OpR` entero.
#[repr(C)]
pub(crate) struct OpR {
    pub op: Op,
    pub origen: u8,
}

extern "C" fn h_exec(c: *mut Cpu, op: *const Op) -> u64 {
    if HELPER_COUNT_ON.load(std::sync::atomic::Ordering::Relaxed) {
        HELPER_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let origen = unsafe { (*(op as *const OpR)).origen };
    let (c, op) = unsafe { (&mut *c, &*op) };
    let mut fpg = None;
    if PROF_ON.load(std::sync::atomic::Ordering::Relaxed) {
        if let Op::Fp(f) = op {
            fpg = Some((f.0, c.fpcr as u64, c.fpsr as u64));
        }
        let k = interp_class(op);
        PROF_INTERP[k].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if k < 2 && c.fpcr != 0 {
            PROF_FP_FPCR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        if k == 3 {
            PROF_OTRAS[op_idx(op)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let fl = interp::exec(c, op);
    if let Some((w, fpcr, fpsr0)) = fpg {
        profg::cuenta(origen, w, fpcr, fpsr0, c.fpsr as u64, &c.v);
    }
    match fl {
        Flow::Next => 0,
        Flow::Jump(t) => {
            c.pc = t;
            1
        }
        Flow::Svc(i) => {
            c.aux[1] = i as u64;
            c.pc += 4;
            2
        }
        Flow::Brk(i) => {
            c.aux[1] = i as u64;
            3
        }
        Flow::Undef(w) => {
            c.aux[1] = w as u64;
            4
        }
        Flow::ICacheFlush(a) => {
            // el bloque devuelve 5 (este valor menos 1): el 4 es Event::HostCall (host_call_block)
            c.aux[1] = a;
            c.pc += 4;
            6
        }
    }
}

extern "C" fn h_cond(c: *mut Cpu, cond: u64) -> u64 {
    unsafe { (&*c).cond_holds(cond as u8) as u64 }
}

extern "C" fn h_store(addr: u64, size: u64, v: u64) {
    if PROF_ON.load(std::sync::atomic::Ordering::Relaxed) {
        PROF_STORES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    monitor::store(addr as usize, size as u8, v);
}

// ---------------------------------------------------------------------------------------------
// Analisis de flags
// ---------------------------------------------------------------------------------------------

fn sets_flags(op: &Op) -> bool {
    match *op {
        Op::AddSubImm { s, .. } | Op::AddSubShift { s, .. } | Op::AddSubExt { s, .. } => s,
        Op::LogImm { op, .. } | Op::LogShift { op, .. } => op == LogOp::Ands,
        _ => false,
    }
}

fn reads_flags(op: &Op) -> bool {
    match *op {
        Op::BCond { .. } | Op::CondSel { .. } | Op::CondCmp { .. } | Op::AddSubCarry { .. } => true,
        Op::Mrs { sysreg, .. } => sysreg == 0xDA10,
        Op::Msr { .. } | Op::MsrImm { .. } | Op::Rmif { .. } | Op::Setf { .. } => true,
        // FCSEL / FCCMP leen NZCV
        Op::Fp(crate::fp::FpOp(w)) => {
            (w >> 24) & 0x1f == 0b11110 && (w >> 21) & 1 == 1 && matches!((w >> 10) & 3, 1 | 3) && (w >> 10) & 0x3f != 0b001000 && ((w >> 10) & 0xf) != 0b1000
        }
        _ => false,
    }
}

fn is_terminator(op: &Op) -> bool {
    match *op {
        Op::B { .. }
        | Op::Bl { .. }
        | Op::Br { .. }
        | Op::Blr { .. }
        | Op::Ret { .. }
        | Op::BCond { .. }
        | Op::Cbz { .. }
        | Op::Tbz { .. }
        | Op::Svc { .. }
        | Op::Brk { .. }
        | Op::Undef(_) => true,
        Op::Sys { op1, crn, crm, op2, .. } => (op1, crn, crm, op2) == (3, 7, 5, 1) || (op1, crn, crm, op2) == (0, 7, 5, 0),
        _ => false,
    }
}

/// true si las flags que fija ops[i] se sobrescriben dentro del bloque antes de leerse. Ya no se usa para eliminar
/// stores (ver `compile`): se conserva para diagnostico.
#[allow(dead_code)]
fn flags_dead(ops: &[(u64, Op)], i: usize) -> bool {
    for j in i + 1..ops.len() {
        let o = &ops[j].1;
        if reads_flags(o) {
            return false;
        }
        if sets_flags(o) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------
// Compilador de bloques
// ---------------------------------------------------------------------------------------------

struct Ctx<'a> {
    a: Asm,
    counted: bool,
    /// kind de flags perezosas conocido en memoria en este punto del bloque (0 = desconocido)
    known: u64,
    ops_keep: &'a mut Vec<Box<OpR>>,
    /// encadenado permitido en este bloque
    chain: bool,
    /// sitios de encadenado: (posicion del rel32 dentro del bloque, pc destino)
    sites: Vec<(usize, u64)>,
    /// Cache de registros (ver cabecera, "Registros"): ranura de cada registro ARM (indice en HREGS) o NO_SLOT.
    slot: [u8; 32],
    /// ranuras cuyo registro x86 contiene AHORA el valor de `Cpu.x[r]` (bit i = ranura i), conocido al traducir
    valid: u8,
    /// true: `ld_x` puede cargar la ranura y `st_x` actualizarla (codigo lineal de jit.rs); false: rutas de
    /// jitfp/jitneon (pueden tener ramas internas): solo se lee una ranura ya valida y `st_x` la invalida
    fill: bool,
    /// la instruccion en curso llamo a h_exec (el interprete puede escribir cualquier registro): al terminarla
    /// se invalidan todas las ranuras
    helper: bool,
    /// kind de flags perezosas cuyas flags x86 siguen vivas: la ultima instruccion las fijo con la propia operacion
    /// (`sub`/`add`/`and` con los mismos operandos que guardo en lf_a/lf_b) y desde entonces solo hubo `mov`.
    /// `xf_next` lo deja la instruccion que fija flags; `xf` vale solo al empezar B.cond/CSEL (emit_cond es lo primero
    /// que emiten) y se consume una vez.
    xf_next: u64,
    xf: u64,
    /// rutas rapidas TLSDESC en linea del bloque (`tlsdesc_inline`): (inicio, fin, ruta lenta), desplazamientos en el
    /// bloque. Un fallo sincrono en [inicio, fin) sigue en la ruta lenta (`fault_in_fused`).
    fused: Vec<(usize, usize, usize)>,
}

fn xoff(r: u8) -> i32 {
    8 * r as i32
}

impl<'a> Ctx<'a> {
    fn ld_x(&mut self, dst: u8, r: Reg, zr: bool) {
        if r == 31 && zr {
            self.a.mov_ri(dst, 0);
            return;
        }
        let s = self.slot[r as usize];
        if s != NO_SLOT {
            let h = HREGS[s as usize];
            if self.valid & (1 << s) != 0 {
                self.a.mov_rr(dst, h, true);
                return;
            }
            if self.fill {
                self.a.mov_rm(h, RBX, xoff(r), true);
                self.valid |= 1 << s;
                self.a.mov_rr(dst, h, true);
                return;
            }
        }
        self.a.mov_rm(dst, RBX, xoff(r), true);
    }
    /// Escritura de un registro: SIEMPRE en el Cpu (escritura inmediata, "write-through") y, si tiene ranura, tambien
    /// en su registro x86 (o se invalida la ranura fuera del codigo lineal).
    fn st_x(&mut self, r: Reg, src: u8, discard31: bool) {
        if r == 31 && discard31 {
            return;
        }
        self.a.mov_mr(RBX, xoff(r), src, true);
        let s = self.slot[r as usize];
        if s != NO_SLOT {
            if self.fill {
                self.a.mov_rr(HREGS[s as usize], src, true);
                self.valid |= 1 << s;
            } else {
                self.valid &= !(1 << s);
            }
        }
    }
    /// `Cpu.x[r]` se escribio en memoria por otra via: su ranura deja de ser valida.
    fn forget(&mut self, r: Reg) {
        let s = self.slot[r as usize];
        if s != NO_SLOT {
            self.valid &= !(1 << s);
        }
    }
    /// Carga la ranura de `r` (si tiene y no es valida) en una frontera de instruccion (codigo lineal).
    fn prefill(&mut self, r: Reg) {
        let s = self.slot[r as usize];
        if s != NO_SLOT && self.valid & (1 << s) == 0 {
            self.a.mov_rm(HREGS[s as usize], RBX, xoff(r), true);
            self.valid |= 1 << s;
        }
    }
    fn set_pc_const(&mut self, pc: u64) {
        self.a.mov_ri(RAX, pc);
        self.a.mov_mr(RBX, O_PC, RAX, true);
    }
    fn count(&mut self, n: usize) {
        if self.counted {
            self.a.mov_mi64(RBX, O_AUX_COUNT, n as i32);
        }
    }
    /// Salida del bloque con PC constante, estado 0.
    /// `from` = pc de la instruccion que salta (decide si es un salto hacia atras).
    fn exit_const(&mut self, target: u64, n: usize, from: u64) {
        self.count(n);
        self.set_pc_const(target);
        if self.chain && !self.counted && !crate::hle::is_hle(target & TBI_MASK) && !crate::boundary::is_host_code(target & TBI_MASK) {
            let out = if target <= from {
                // salto hacia atras: si cambio EPOCH, volver al despachador (pc ya escrito)
                self.a.mov_ri(RAX, std::ptr::addr_of!(EPOCH) as u64);
                self.a.mov_rm(RAX, RAX, 0, true);
                self.a.op_rm(true, &[0x3B], RAX, RBX, O_AUX_EPOCH); // cmp rax, [rbx+aux5]
                Some(self.a.jcc_fwd(CC_NE))
            } else {
                None
            };
            let site = self.a.jmp_fwd(); // rel32 = 0: cae al epilogo hasta que se parchee
            self.sites.push((site, target));
            if let Some(o) = out {
                self.a.patch_here(o);
            }
        }
        self.a.mov_ri(RAX, 0);
        self.a.ret_epilogue();
    }
    /// Salida con el destino ya en RAX.
    fn exit_rax(&mut self, n: usize) {
        self.count(n);
        self.a.mov_mr(RBX, O_PC, RAX, true);
        self.a.mov_ri(RAX, 0);
        self.a.ret_epilogue();
    }

    fn call_helper_exec(&mut self, pc: u64, op: Op, idx: usize) {
        self.call_helper_exec_o(pc, op, idx, profg::O_GENERAL);
    }
    /// `origen`: de que sitio viene la llamada (solo para el perfilador; no cambia el codigo generado).
    fn call_helper_exec_o(&mut self, pc: u64, op: Op, idx: usize, origen: u8) {
        let b = Box::new(OpR { op, origen });
        let p = &b.op as *const Op as u64;
        self.ops_keep.push(b);
        self.set_pc_const(pc);
        self.a.mov_rr(RDI, RBX, true);
        self.a.mov_ri(RSI, p);
        self.a.call_abs(h_exec as usize);
        self.a.alu_rr(false, 0x85, RAX, RAX);
        let skip = self.a.jcc_fwd(CC_E);
        self.count(idx + 1);
        self.a.alu_ri(false, 5, RAX, 1);
        self.a.ret_epilogue();
        self.a.patch_here(skip);
        // el helper puede haber cambiado las flags: el kind en memoria ya no se conoce
        self.known = 0;
        // y cualquier registro: las ranuras se invalidan al terminar la instruccion
        self.helper = true;
    }

    /// Deja las flags x86 de modo que `cc` (devuelto) sea verdadero sii la condicion ARM se cumple.
    /// None = condicion siempre verdadera (AL/NV). Usa solo R10/R11 (y el helper clobbera todo).
    fn emit_cond(&mut self, cond: u8) -> Option<u8> {
        if cond >= 14 {
            return None;
        }
        let kind = self.known;
        let sub_map = |c: u8| -> Option<u8> {
            Some(match c {
                0 => CC_E,
                1 => CC_NE,
                2 => CC_AE,
                3 => CC_B,
                4 => CC_S,
                5 => CC_NS,
                6 => CC_O,
                7 => CC_NO,
                8 => CC_A,
                9 => CC_BE,
                10 => CC_GE,
                11 => CC_L,
                12 => CC_G,
                13 => CC_LE,
                _ => return None,
            })
        };
        // ADD / LOGIC: C = CF directo; HI/LS no tienen cc equivalente
        let add_map = |c: u8| -> Option<u8> {
            match c {
                8 | 9 => None,
                2 => Some(CC_B),
                3 => Some(CC_AE),
                _ => sub_map(c),
            }
        };
        if kind == jitfp::KNOWN_RAW {
            return self.cond_from_raw(cond);
        }
        let (w, m): (bool, Option<u8>) = match kind {
            LF_SUB64 => (true, sub_map(cond)),
            LF_SUB32 => (false, sub_map(cond)),
            LF_ADD64 => (true, add_map(cond)),
            LF_ADD32 => (false, add_map(cond)),
            LF_LOGIC64 => (true, add_map(cond)),
            LF_LOGIC32 => (false, add_map(cond)),
            _ => (true, None),
        };
        if m.is_some() && kind != 0 && self.xf == kind {
            // las flags x86 de la instruccion anterior (misma operacion, mismos operandos, mismo ancho) son las que
            // daria el cmp/add/test de abajo: se usan tal cual
            self.xf = 0;
            return m;
        }
        self.xf = 0;
        match (kind, m) {
            (LF_SUB64 | LF_SUB32, Some(cc)) => {
                self.a.mov_rm(R10, RBX, O_LFA, true);
                self.a.mov_rm(R11, RBX, O_LFB, true);
                self.a.alu_rr(w, 0x39, R10, R11);
                Some(cc)
            }
            (LF_ADD64 | LF_ADD32, Some(cc)) => {
                self.a.mov_rm(R10, RBX, O_LFA, true);
                self.a.mov_rm(R11, RBX, O_LFB, true);
                self.a.alu_rr(w, 0x01, R10, R11);
                Some(cc)
            }
            (LF_LOGIC64 | LF_LOGIC32, Some(cc)) => {
                self.a.mov_rm(R10, RBX, O_LFA, true);
                self.a.alu_rr(w, 0x85, R10, R10);
                Some(cc)
            }
            _ => {
                self.a.mov_rr(RDI, RBX, true);
                self.a.mov_ri(RSI, cond as u64);
                self.a.call_abs(h_cond as usize);
                self.a.alu_rr(false, 0x85, RAX, RAX);
                Some(CC_NE)
            }
        }
    }

    /// Condicion ARM a partir de NZCV materializado en memoria (kind RAW).
    fn cond_from_raw(&mut self, cond: u8) -> Option<u8> {
        let o_nzcv = offset_of!(Cpu, nzcv) as i32;
        self.a.mov_rm(R10, RBX, o_nzcv, true);
        match cond {
            0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 => {
                let bitn: u8 = match cond >> 1 {
                    0 => 30,
                    1 => 29,
                    2 => 31,
                    _ => 28,
                };
                self.a.bt_ri(R10, bitn);
                // bt deja CF = bit
                Some(if cond & 1 == 0 { CC_B } else { CC_AE })
            }
            8 | 9 => {
                self.a.alu_ri(false, 4, R10, 0x6000_0000);
                self.a.alu_ri(false, 7, R10, 0x2000_0000);
                Some(if cond == 8 { CC_E } else { CC_NE })
            }
            10 | 11 => {
                self.a.mov_rr(R11, R10, false);
                self.a.shift_ri(false, 4, R11, 3); // shl: V (bit 28) -> bit 31
                self.a.alu_rr(false, 0x31, R10, R11); // bit 31 = N ^ V
                self.a.alu_rr(false, 0x85, R10, R10);
                Some(if cond == 10 { CC_NS } else { CC_S })
            }
            12 | 13 => {
                self.a.mov_rr(R11, R10, false);
                self.a.shift_ri(false, 4, R11, 3); // shl (ver arriba)
                self.a.alu_rr(false, 0x31, R11, R10);
                self.a.alu_ri(false, 4, R11, i32::MIN);
                self.a.alu_ri(false, 4, R10, 0x4000_0000);
                self.a.alu_rr(false, 0x09, R11, R10);
                Some(if cond == 12 { CC_E } else { CC_NE })
            }
            _ => None,
        }
    }

    fn store_flags(&mut self, kind: u64, a_reg: u8, b_reg: Option<u8>, b_imm: i32) {
        self.a.mov_mi64(RBX, O_KIND, kind as i32);
        self.a.mov_mr(RBX, O_LFA, a_reg, true);
        match b_reg {
            Some(r) => self.a.mov_mr(RBX, O_LFB, r, true),
            None => self.a.mov_mi64(RBX, O_LFB, b_imm),
        }
        self.known = kind;
    }

    /// Calcula la direccion efectiva en RAX (sin TBI). Aplica el write-back previo (Pre).
    fn emit_ea(&mut self, rn: Reg, addr: Addr) {
        self.ld_x(RAX, rn, false);
        match addr {
            Addr::Off(i) => {
                if i != 0 {
                    self.a.lea(RAX, RAX, i as i32, true);
                }
            }
            // pre-indice: la base se escribe despues del acceso (`post_wb`), como en el pseudocodigo del Arm ARM:
            // si el acceso falla, el manejador ve la base sin cambiar y la instruccion se puede repetir
            Addr::Pre(i) => {
                if i != 0 {
                    self.a.lea(RAX, RAX, i as i32, true);
                }
            }
            Addr::Post(_) => {}
            Addr::Reg { rm, ext, amt } => {
                self.ld_x(RDX, rm, true);
                self.a.ext_rr(RDX, ext);
                if amt > 0 {
                    self.a.shift_ri(true, 4, RDX, amt);
                }
                self.a.alu_rr(true, 0x01, RAX, RDX);
            }
        }
    }
    fn mask_tbi(&mut self, r: u8) {
        self.a.shift_ri(true, 4, r, 8);
        self.a.shift_ri(true, 5, r, 8);
    }
    /// Escritura de la base de un pre o post-indice, tras el acceso (base + desplazamiento en los dos casos).
    fn post_wb(&mut self, rn: Reg, addr: Addr) {
        if let Addr::Post(i) | Addr::Pre(i) = addr {
            self.ld_x(RCX, rn, false);
            self.a.lea(RCX, RCX, i as i32, true);
            self.st_x(rn, RCX, false);
        }
    }
    fn call_store(&mut self, size: u8) {
        // RDI = direccion, RDX = valor ya cargados
        // Store rapido del monitor, llamado directamente: modo fence (`heddle_fst*`, con la marca del hilo en RCX:
        // cpu.monflag; 0 = ruta con bloqueo) o rseq (`heddle_rst*`). Con el perfilador activo, antes se cuenta el
        // store (`lock inc` de PROF_STORES, como hace h_store).
        let (f, flag) = monitor::fast_store_entry(size);
        if f != 0 {
            if PROF_ON.load(std::sync::atomic::Ordering::Relaxed) {
                self.a.mov_ri(RAX, &PROF_STORES as *const _ as u64);
                self.a.b.extend_from_slice(&[0xF0, 0x48, 0xFF, 0x00]); // lock inc qword ptr [rax]
            }
            if flag {
                self.a.mov_rm(RCX, RBX, O_MONFLAG, true);
            }
            self.a.call_abs(f);
            return;
        }
        self.a.mov_ri(RSI, size as u64);
        self.a.call_abs(h_store as usize);
    }

    fn bitfield(&mut self, sf: bool, op: BfOp, rd: Reg, rn: Reg, immr: u8, imms: u8) -> bool {
        let ds = if sf { 64 } else { 32 };
        let (wmask, tmask) = match decode_bit_masks(sf as u32, imms as u32, immr as u32, false, ds) {
            Some(x) => x,
            None => return false,
        };
        let mk = if sf { u64::MAX } else { 0xFFFF_FFFF };
        let m = wmask & tmask & mk;
        self.ld_x(RAX, rn, true);
        match op {
            BfOp::Ubfm => {
                if immr != 0 {
                    self.a.shift_ri(sf, 1, RAX, immr);
                }
                self.a.alu_imm_any(sf, 0x21, 4, RAX, m, R8);
            }
            BfOp::Sbfm => {
                // RCX = difusion del bit `imms` del origen
                self.a.mov_rr(RCX, RAX, true);
                let top = if sf { 63 } else { 31 };
                self.a.shift_ri(sf, 4, RCX, top - imms);
                self.a.shift_ri(sf, 7, RCX, top);
                self.a.alu_imm_any(sf, 0x21, 4, RCX, !tmask & mk, R8);
                if immr != 0 {
                    self.a.shift_ri(sf, 1, RAX, immr);
                }
                self.a.alu_imm_any(sf, 0x21, 4, RAX, m, R8);
                self.a.alu_rr(sf, 0x09, RAX, RCX);
            }
            BfOp::Bfm => {
                if immr != 0 {
                    self.a.shift_ri(sf, 1, RAX, immr);
                }
                self.a.alu_imm_any(sf, 0x21, 4, RAX, m, R8);
                self.ld_x(RDX, rd, true);
                self.a.alu_imm_any(sf, 0x21, 4, RDX, !m & mk, R8);
                self.a.alu_rr(sf, 0x09, RAX, RDX);
            }
        }
        self.st_x(rd, RAX, true);
        true
    }
}

fn shift_ext(s: Shift) -> u8 {
    match s {
        Shift::Lsl => 4,
        Shift::Lsr => 5,
        Shift::Asr => 7,
        Shift::Ror => 1,
    }
}

// ---------------------------------------------------------------------------------------------
// Jit
// ---------------------------------------------------------------------------------------------


pub(crate) type BlockFn = extern "C" fn(*mut Cpu) -> u64;

pub const CODE_CAP: usize = 32 << 20;
const MAX_BLOCK: usize = 64;
const CACHE_BITS: usize = 14;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Event {
    Svc(u16),
    Brk(u16),
    Undef(u32),
    /// el pc entro en la region HLE (llamada a funcion del host)
    Hle,
    /// el guest salto a una direccion de codigo del host (puntero a funcion devuelto por el host, vtable del host...)
    HostCall,
    /// el pc apunta a memoria que no se puede leer (salto a una direccion sin mapear): fallo de instruccion, SIGSEGV
    /// para el guest con el pc en esa direccion
    InstAbort,
    /// hay senales aplazadas para el hilo (`sig::pend`): el `Cpu` esta en una frontera de instruccion
    Signal,
}

/// Valor de `cpu.aux[5]` que pone el manejador de una senal aplazada: ningun EPOCH vale esto, asi que el siguiente
/// salto encadenado hacia atras sale a `run_inner`, que entrega la senal (ver la cabecera, "Senales").
pub const SIG_POISON: u64 = u64::MAX;

/// Senales aplazadas de un `Jit` sin hilo guest (pruebas): siempre ninguna.
static NO_PEND: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Bloque especial: el pc es codigo del host; el bucle de ejecucion hace la llamada nativa.
extern "C" fn host_call_block(_c: *mut Cpu) -> u64 {
    4
}

/// Bloque especial: el pc no se puede leer (no se traduce ni se guarda en la cache).
extern "C" fn inst_abort_block(_c: *mut Cpu) -> u64 {
    7
}

// Entrada al codigo guest con punto de reanudacion.
//
// Bloques traducidos: `run_inner` llama al bloque directamente (`f(cpu)`) y el prologo de cada bloque entrado desde
// fuera apunta en `cpu.host_sp` el RSP de su cuerpo (`mov [rbx+host_sp], rsp`, ver `Asm::prologue`). Los bloques
// encadenados entran tras el prologo y comparten ese marco, asi que `host_sp` vale mientras dure la cadena. Debajo de
// `host_sp` solo hay, si el bloque llamo a un helper, su direccion de retorno (host_sp-8; el cuerpo de un bloque nunca
// mueve RSP); encima, el marco del prologo (relleno, rbp, r15..r12, rbx) y la direccion de retorno a `run_inner`.
// `run_inner` pone `host_sp` a 0 al volver el bloque. Un manejador de fallo sincrono que quiere reanudar el guest con
// otro contexto (sig.rs) escribe ese contexto en la Cpu y devuelve del manejador con rip = `heddle_jit_abort` y rsp =
// `host_sp`: el nucleo restaura ese estado y `heddle_jit_abort` ejecuta el epilogo del bloque con 0 ("seguir en
// cpu.pc"): repone los callee-saved de `run_inner` desde el marco y le devuelve, descartando el bloque a medias. Es un
// longjmp a la salida del bloque, sin coste en el despacho (ningun trampolin de entrada por bloque).
//
// Bucle del interprete de los manejadores de senal: `heddle_call_block(cpu, f)` guarda los registros callee-saved y
// el `host_sp` anterior de la Cpu, apunta en `cpu.host_sp` el RSP actual (antes de la llamada) y llama a `f(cpu)`; la
// reanudacion vuelve con rip = `heddle_block_abort`, rsp = `host_sp` y rbx = &Cpu y sale de `heddle_call_block` con 0.
std::arch::global_asm!(
    ".p2align 6",
    ".globl heddle_call_block",
    ".hidden heddle_call_block",
    ".type heddle_call_block,@function",
    "heddle_call_block:",
    "push rbx",
    "push rbp",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rbx, rdi",
    "push qword ptr [rdi + {hsp}]",
    "mov [rdi + {hsp}], rsp",
    "call rsi",
    ".Lheddle_cb_out:",
    "pop qword ptr [rbx + {hsp}]",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbp",
    "pop rbx",
    "ret",
    ".globl heddle_block_abort",
    ".hidden heddle_block_abort",
    ".type heddle_block_abort,@function",
    "heddle_block_abort:",
    "xor eax, eax",
    "jmp .Lheddle_cb_out",
    // Reanudacion de un bloque traducido: rsp = host_sp (cuerpo del bloque); el epilogo de `Asm::ret_epilogue`.
    ".p2align 4",
    ".globl heddle_jit_abort",
    ".hidden heddle_jit_abort",
    ".type heddle_jit_abort,@function",
    "heddle_jit_abort:",
    "xor eax, eax",
    "pop rcx",
    "pop rbp",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "ret",
    hsp = const std::mem::offset_of!(Cpu, host_sp),
);

// la Cpu solo se usa por puntero (el ensamblador conoce `host_sp` por offset_of)
#[allow(improper_ctypes)]
extern "C" {
    /// Ejecuta `f(c)` con punto de reanudacion (ver arriba; bucle del interprete).
    pub fn heddle_call_block(c: *mut Cpu, f: BlockFn) -> u64;
    /// Destino de la reanudacion tras un fallo sincrono en `heddle_call_block` (solo se llega devolviendo de un
    /// manejador de senal).
    pub fn heddle_block_abort();
    /// Destino de la reanudacion tras un fallo sincrono en un bloque traducido (rsp = `host_sp`; ver arriba).
    pub fn heddle_jit_abort();
}

/// Bytes de la pila del host entre `host_sp` (cuerpo del bloque) y la direccion de retorno de una llamada a un helper
/// desde un bloque.
pub const HELPER_RET_BELOW_HOST_SP: u64 = 8;

/// Publica una invalidacion de codigo (IC IVAU/IALLU, `addr` = 0: todo) para todos los hilos, incluido el propio:
/// sus `Jit` la procesan al volver a `run_inner`. Para el interprete de los manejadores de senal (no toca ningun Jit).
pub fn publish_ic(addr: u64) {
    ic_publish(addr);
}

/// Contador global: sube con cada IC IVAU/IALLU (`ic_publish`). Un hilo que lo ve cambiar invalida las lineas
/// publicadas en `IC_LOG` desde la ultima que proceso (`Jit::ic_catch_up`), no toda su cache.
pub static EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Invalidaciones de codigo (IC IVAU/IALLU) publicadas para los demas hilos: anillo fijo de `IC_RING` entradas
/// (ver la tabla de cotas de CLAUDE.md). La secuencia `s` va en `IC_LOG[s % IC_RING]` como un unico atomico:
/// linea de 64 bytes (0 = todo) | etiqueta de 6 bits `ic_tag(s)`. La etiqueta distingue "aun no escrita" (vuelta
/// anterior) de "pisada por una vuelta posterior". Un hilo que se quedo mas de `IC_RING` entradas atras, o que
/// encuentra una entrada pisada o perdida, vacia toda su cache (lo que antes se hacia con cada IC IVAU).
/// `__clear_cache` emite un IC IVAU por linea: con un vaciado global en cada uno, un JIT del guest (Mono, LuaJIT,
/// V8) tiraba sin parar el codigo traducido de todos los hilos.
///
/// Orden: quien escribe codigo y luego hace IC IVAU lo hace antes de reservar su secuencia (`fetch_add` AcqRel);
/// un hilo que lee `IC_SEQ` (Acquire) y despues traduce ve ya el codigo nuevo de toda secuencia reservada. Por eso
/// un `Jit` nuevo o recien vaciado puede saltarse las entradas anteriores aunque aun no esten escritas. Uno que
/// conserva bloques se detiene en la primera entrada no escrita: su publicador sube EPOCH al terminar.
const IC_RING: usize = 4096;
static IC_LOG: [std::sync::atomic::AtomicU64; IC_RING] = [const { std::sync::atomic::AtomicU64::new(63) }; IC_RING];
/// proxima secuencia a reservar en IC_LOG
static IC_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// sube cuando un publicador encuentra su casilla ya ocupada por una vuelta posterior (estuvo detenido mas de
/// IC_RING publicaciones): los lectores no pueden saber que linea era y vacian todo
static IC_LOST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Etiqueta de la vuelta de la secuencia `s` (valor inicial de las casillas: 63, la vuelta "-1").
fn ic_tag(s: u64) -> u64 {
    (s / IC_RING as u64) & 63
}

/// Cuantas vueltas va la etiqueta `want` por delante de `have` (0 = la misma; 32..63 = `have` es mas nueva).
fn ic_ahead(want: u64, have: u64) -> u64 {
    want.wrapping_sub(have) & 63
}

/// Publica la invalidacion de la linea de `addr` (0 = todo) para los demas hilos y sube EPOCH.
/// Devuelve (secuencia reservada, EPOCH anterior a la subida).
fn ic_publish(addr: u64) -> (u64, u64) {
    use std::sync::atomic::Ordering::*;
    let s = IC_SEQ.fetch_add(1, AcqRel);
    ic_write(s, addr);
    (s, EPOCH.fetch_add(1, Release))
}

/// Escribe la entrada de la secuencia reservada `s`, o la da por perdida si su casilla ya es de otra vuelta.
fn ic_write(s: u64, addr: u64) {
    use std::sync::atomic::Ordering::*;
    let line = (addr & TBI_MASK) & !63; // una linea en [0, 64) se trata como "todo"
    let slot = &IC_LOG[(s % IC_RING as u64) as usize];
    let mut cur = slot.load(Acquire);
    loop {
        let d = ic_ahead(ic_tag(s), cur & 63);
        if d == 0 || d >= 32 {
            // la casilla ya es de esta vuelta o de una posterior: nunca se pisa una entrada mas nueva
            IC_LOST.fetch_add(1, Release);
            break;
        }
        match slot.compare_exchange_weak(cur, line | ic_tag(s), Release, Acquire) {
            Ok(_) => break,
            Err(v) => cur = v,
        }
    }
}

/// Contadores adicionales del perfilador, solo activos con PROF_ON (ver rt::start_profiler): distorsionan algo la
/// velocidad (atomicos compartidos) pero dan proporciones fiables.
pub static PROF_ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// llamadas a h_exec (instruccion interpretada) por clase: 0 FP escalar, 1 SIMD, 2 bitfield, 3 otras
pub static PROF_INTERP: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];
/// FP/SIMD interpretadas con FPCR != 0 (el JIT solo tiene ruta rapida con FPCR = 0)
pub static PROF_FP_FPCR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PROF_STORES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// estaticos, al traducir: instrucciones traducidas y tipo de salida del bloque (0 directa, 1 indirecta, 2 secuencia)
pub static PROF_OPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PROF_EXITS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Nombres de las variantes de `Op` (indice = posicion en la macro de abajo) para contar las "otras" por variante.
macro_rules! op_names {
    ($($v:ident),* $(,)?) => {
        pub const OP_NAMES: &[&str] = &[$(stringify!($v)),*, "?"];
        pub fn op_idx(op: &Op) -> usize {
            let mut i = 0usize;
            $( if matches!(op, Op::$v { .. }) { return i; } i += 1; )*
            i
        }
    };
}
op_names!(
    Undef, Nop, Adr, AddSubImm, LogImm, MovWide, Bitfield, Extr, AddSubShift, AddSubExt, LogShift, AddSubCarry, CondCmp,
    CondSel, Madd, MaddL, MulH, Div, ShiftV, Dp1, Crc32, B, Bl, Br, Blr, Ret, BCond, Cbz, Tbz, Load, Store, LoadLit,
    LoadPair, StorePair, Prefetch, Ldx, Stx, Ldxp, Stxp, Ldar, Stlr, Atomic, Cas, Casp, Barrier, Svc, Brk, Mrs, Msr,
    MsrImm, Sys, Fp, Rmif, Setf
);
/// "otras" (h_exec fuera de FP/SIMD/bitfield) por variante de Op; arreglo fijo (cota: numero de variantes).
pub static PROF_OTRAS: [std::sync::atomic::AtomicU64; 64] = [const { std::sync::atomic::AtomicU64::new(0) }; 64];

fn interp_class(op: &Op) -> usize {
    match op {
        Op::Fp(f) => {
            let g = (f.0 >> 24) & 0x1F;
            if g == 0x0E || g == 0x0F {
                1
            } else {
                0
            }
        }
        Op::Bitfield { .. } => 2,
        _ => 3,
    }
}

/// Contadores del perfilador (ver rt::start_profiler): bloques DESPACHADOS por run_inner (con encadenado cuenta menos:
/// los saltos encadenados no pasan por el despachador), bloques traducidos y tiempo traduciendo.
pub static PROF_BLOCKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PROF_COMPILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static PROF_COMPILE_NS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub struct Jit {
    /// bufer de codigo generado: Region (se libera al soltar el Jit, es decir, al terminar el hilo)
    region: crate::mem::Region,
    buf: *mut u8,
    used: usize,
    /// tamano del bufer (CODE_CAP, o menos en los estados de emergencia de las senales, que no traducen)
    cap: usize,
    ops_keep: Vec<Box<OpR>>,
    /// cache directa pc -> codigo (tamano fijo: el indice va enmascarado y no necesita comprobar limites)
    cache: Box<[(u64, usize); 1 << CACHE_BITS]>,
    map: HashMap<u64, (usize, u64)>, // pc -> (codigo, fin del bloque guest)
    pub compiled: u64,
    pub flushes: u64,
    epoch: u64,
    /// proxima secuencia de IC_LOG por procesar y ultimo IC_LOST visto (ver `ic_catch_up`)
    ic_seq: u64,
    ic_lost: u64,
    /// encadenado de bloques activo (ver cabecera)
    pub chain: bool,
    /// sitios de encadenado de los bloques de este Jit (acotado por SITES_CAP; se vacia en flush_all)
    sites: Vec<Site>,
    /// sitios no parcheados por destino (indices en `sites`)
    pending: HashMap<u64, Vec<u32>>,
    /// sitios del bloque recien compilado (los recoge `lookup`)
    new_sites: Vec<(usize, u64)>,
    /// cuantos saltos se han parcheado (pruebas y diagnostico)
    pub patched: u64,
    /// bloques despachados por run_inner (solo se cuenta en pruebas; no poner atributos de prueba ni su texto
    /// literal en el codigo de produccion: arch_lint corta cada archivo en la primera aparicion)
    pub dispatched: u64,
    /// cache de registros ARM en registros x86 dentro del bloque (ver cabecera, "Registros"); afecta a los bloques
    /// que se traduzcan despues de cambiarlo
    pub regs: bool,
    /// Tabla codigo x86 -> pc guest (fallos sincronos con pc exacto): un elemento por bloque, en orden de direccion
    /// (el bufer crece hacia arriba), con el desplazamiento del codigo de cada instruccion guest en `pc_offs`. Se
    /// vacia con el bufer (`flush_all`); su tamano es proporcional al codigo traducido (cota: CODE_CAP).
    pcs: Vec<BlockPcs>,
    pc_offs: Vec<u32>,
    /// Rutas rapidas TLSDESC en linea (`Ctx::tlsdesc_inline`): (inicio, fin, ruta lenta) del codigo, en orden de
    /// direccion. Se vacia con el bufer; una entrada por llamada TLSDESC traducida (cota: CODE_CAP).
    fused: Vec<(usize, usize, usize)>,
    /// senales aplazadas del hilo dueno (`GuestThread::sig_pend`; `NO_PEND` sin hilo)
    sig_pend: *const std::sync::atomic::AtomicU64,
}

/// Bloque traducido para `pc_at`: codigo [code, end), pc guest de su primera instruccion e indice de los
/// desplazamientos de sus `n` instrucciones en `Jit::pc_offs`.
struct BlockPcs {
    code: usize,
    end: usize,
    pc: u64,
    first: u32,
    n: u32,
}

/// Un `jmp rel32` parcheable de un bloque traducido.
struct Site {
    /// posicion del rel32 dentro del bufer de codigo
    off: usize,
    /// bloque al que pertenece (pc y codigo): si ya no esta en `map`, el sitio esta muerto
    src: u64,
    src_code: usize,
    dest: u64,
    patched: bool,
}

fn regs_default() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("HEDDLE_NOREGS").is_none())
}

fn chain_default() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var_os("HEDDLE_NOCHAIN").is_none())
}

unsafe impl Send for Jit {}

impl Drop for Jit {
    fn drop(&mut self) {
        // la Region se desmapea sola; aqui solo se descuenta el codigo en uso
        crate::mem::JIT_USED.fetch_sub(self.used as u64, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Jit {
    pub fn new() -> Jit {
        Jit::with_capacity(CODE_CAP)
    }

    /// Jit con un bufer de `cap` bytes (reservado sin ocupar: NORESERVE). Los estados guest de emergencia de las senales
    /// (rt::EMERG) solo ejecutan manejadores con el interprete: no necesitan los 32 MiB de un hilo.
    pub fn with_capacity(cap: usize) -> Jit {
        let region = match crate::mem::Region::new(cap, 7 /*RWX*/, crate::mem::Kind::Jit) {
            Some(r) => r,
            None => {
                crate::bridge::alog_fatal(&format!("sin memoria para el JIT\n{}", crate::mem::status_line()));
                unsafe { crate::sys::abort() }
            }
        };
        let buf = region.base() as *mut u8;
        Jit {
            region,
            buf,
            used: 0,
            cap,
            ops_keep: Vec::new(),
            cache: vec![(u64::MAX, 0); 1 << CACHE_BITS].into_boxed_slice().try_into().unwrap(),
            map: HashMap::new(),
            compiled: 0,
            flushes: 0,
            epoch: EPOCH.load(std::sync::atomic::Ordering::Acquire),
            ic_lost: IC_LOST.load(std::sync::atomic::Ordering::Acquire),
            ic_seq: IC_SEQ.load(std::sync::atomic::Ordering::Acquire),
            chain: chain_default(),
            sites: Vec::new(),
            pending: HashMap::new(),
            new_sites: Vec::new(),
            patched: 0,
            dispatched: 0,
            regs: regs_default(),
            pcs: Vec::new(),
            pc_offs: Vec::new(),
            fused: Vec::new(),
            sig_pend: &NO_PEND,
        }
    }

    /// Senales aplazadas del hilo que usa este Jit (las comprueba `run_inner`; ver `Event::Signal`).
    ///
    /// # Safety
    /// `p` vive mas que el Jit (los dos son del mismo `GuestThread`).
    pub unsafe fn set_sig_pend(&mut self, p: *const std::sync::atomic::AtomicU64) {
        self.sig_pend = p;
    }

    #[inline(always)]
    fn sig_pending(&self) -> bool {
        unsafe { (*self.sig_pend).load(std::sync::atomic::Ordering::Relaxed) != 0 }
    }

    /// `a` esta en el codigo traducido de este Jit?
    pub fn code_contains(&self, a: u64) -> bool {
        let b = self.buf as u64;
        a >= b && a < b + self.used as u64
    }

    /// pc guest de la instruccion cuyo codigo traducido contiene `a` (None si `a` no es codigo de un bloque). Sin
    /// reservas ni bloqueos: se usa en el manejador de un fallo sincrono, con el hilo detenido dentro de un bloque.
    pub fn pc_at(&self, a: u64) -> Option<u64> {
        let a = a as usize;
        let i = self.pcs.partition_point(|b| b.code <= a);
        let b = self.pcs.get(i.checked_sub(1)?)?;
        if a >= b.end || b.n == 0 {
            return None;
        }
        let offs = &self.pc_offs[b.first as usize..(b.first + b.n) as usize];
        let k = offs.partition_point(|&o| b.code + o as usize <= a).max(1) - 1;
        Some(b.pc + 4 * k as u64)
    }

    /// pc guest exacto de un fallo sincrono ocurrido con la Cpu dentro de un bloque de este Jit (`host_sp` de la
    /// Cpu): en el propio codigo traducido (`rip`) o en un helper que el bloque llamo (su direccion de retorno).
    ///
    /// # Safety
    /// `host_sp` es el de una Cpu que esta ejecutando un bloque de este Jit en este hilo (la pila es legible).
    pub unsafe fn fault_pc(&self, host_sp: u64, rip: u64) -> Option<u64> {
        if self.code_contains(rip) {
            return self.pc_at(rip);
        }
        if host_sp == 0 {
            return None;
        }
        let ret = unsafe { *((host_sp - HELPER_RET_BELOW_HOST_SP) as *const u64) };
        if self.code_contains(ret) {
            return self.pc_at(ret - 1);
        }
        None
    }

    fn patch_site(&mut self, off: usize, dest_code: usize) {
        let from = self.buf as usize + off + 4;
        let rel = (dest_code + PROLOGUE_LEN) as i64 - from as i64;
        let rel = i32::try_from(rel).expect("salto encadenado fuera de alcance rel32");
        unsafe { std::ptr::write_unaligned(self.buf.add(off) as *mut i32, rel) };
    }

    /// Registra un bloque recien compilado: sus sitios y los pendientes que apuntaban a el.
    fn register_block(&mut self, start: u64, code: usize) {
        let new = std::mem::take(&mut self.new_sites);
        for (pos, dest) in new {
            if self.sites.len() >= SITES_CAP {
                break; // sin sitio: el salto queda sin encadenar (vuelve al despachador)
            }
            let off = pos + (code - self.buf as usize);
            let idx = self.sites.len() as u32;
            let hit = match self.map.get(&dest) {
                Some(&(c, _)) if c != host_call_block as usize => Some(c),
                _ => None,
            };
            let mut st = Site { off, src: start, src_code: code, dest, patched: false };
            if let Some(c) = hit {
                self.patch_site(off, c);
                st.patched = true;
                self.patched += 1;
            }
            self.sites.push(st);
            if !self.sites[idx as usize].patched {
                self.pending.entry(dest).or_default().push(idx);
            }
        }
        if let Some(list) = self.pending.remove(&start) {
            for idx in list {
                let off = self.sites[idx as usize].off;
                self.patch_site(off, code);
                self.sites[idx as usize].patched = true;
                self.patched += 1;
            }
        }
    }

    /// Tras quitar bloques de `map`: desparcha los saltos hacia bloques ya inexistentes (vuelven al epilogo y quedan
    /// pendientes), descarta los sitios de bloques muertos y reconstruye `pending`.
    fn fix_sites_after_invalidate(&mut self) {
        let old = std::mem::take(&mut self.sites);
        self.pending = HashMap::new();
        for mut st in old {
            let alive = matches!(self.map.get(&st.src), Some(&(c, _)) if c == st.src_code);
            if !alive {
                // Bloque invalidado: su codigo sigue en el bufer (no se reutiliza hasta flush_all) y aun puede estar en
                // ejecucion suspendida en este hilo (un manejador de senal que vuelve a entrar en este Jit via
                // call_guest e invalida). Se desparcha igual: si se reanuda, vuelve al despachador en vez de saltar a
                // un destino que despues podria invalidarse sin que este sitio, ya no registrado, se entere.
                if st.patched {
                    unsafe { std::ptr::write_unaligned(self.buf.add(st.off) as *mut i32, 0) };
                }
                continue;
            }
            let dest_alive = matches!(self.map.get(&st.dest), Some(&(c, _)) if c != host_call_block as usize);
            if st.patched && !dest_alive {
                unsafe { std::ptr::write_unaligned(self.buf.add(st.off) as *mut i32, 0) };
                st.patched = false;
            }
            let idx = self.sites.len() as u32;
            if !st.patched {
                self.pending.entry(st.dest).or_default().push(idx);
            }
            self.sites.push(st);
        }
    }

    pub fn flush_all(&mut self) {
        // devolver al sistema las paginas del codigo descartado (si no, la memoria residente nunca baja)
        self.region.discard(self.used);
        crate::mem::JIT_USED.fetch_sub(self.used as u64, std::sync::atomic::Ordering::Relaxed);
        self.used = 0;
        // soltar tambien la capacidad acumulada (no solo vaciar)
        self.ops_keep = Vec::new();
        self.map = HashMap::new();
        // todo el bufer se descarta: ningun salto parcheado puede sobrevivir
        self.sites = Vec::new();
        self.pending = HashMap::new();
        self.pcs = Vec::new();
        self.pc_offs = Vec::new();
        self.fused = Vec::new();
        for e in self.cache.iter_mut() {
            *e = (u64::MAX, 0);
        }
        self.flushes += 1;
    }

    /// Invalida bloques que cubren `[addr & !63, +64)`; addr = 0 invalida todo.
    pub fn invalidate(&mut self, addr: u64) {
        if addr == 0 {
            self.flush_all();
            return;
        }
        self.invalidate_lines(&mut vec![(addr & TBI_MASK) & !63]);
    }

    /// Invalida los bloques que solapan alguna de las lineas de 64 bytes `lines` (se ordenan aqui).
    fn invalidate_lines(&mut self, lines: &mut Vec<u64>) {
        lines.sort_unstable();
        lines.dedup();
        let before = self.map.len();
        // bloque [s, e) solapa la linea [l, l + 64) si l < e && s < l + 64: la primera linea con l + 64 > s
        self.map.retain(|&s, &mut (_, e)| {
            let i = lines.partition_point(|&l| l + 64 <= s);
            !(i < lines.len() && lines[i] < e)
        });
        if self.map.len() != before {
            // solo si se quito algun bloque: si no, ningun sitio cambia de estado (evita O(sites) por cada IC IVAU)
            self.fix_sites_after_invalidate();
        }
        for e in self.cache.iter_mut() {
            *e = (u64::MAX, 0);
        }
    }

    /// Compila un bloque que empieza en `start`. Devuelve (puntero al codigo, fin guest); (0, start) si la primera
    /// instruccion no se puede leer.
    fn compile(&mut self, start: u64, limit: usize, counted: bool) -> (usize, u64) {
        // un puntero del host que se filtro al guest: el guest no puede ejecutar el trampolin x86 del host
        if crate::cbthunk::is_thunk(start & TBI_MASK) {
            crate::bridge::alog_fatal(&format!("el guest intenta ejecutar un trampolin x86 del host en {:#x} (puntero del host filtrado)", start));
            unsafe { crate::sys::abort() }
        }
        // 1. predecodificar
        let mut ops: Vec<(u64, Op)> = Vec::new();
        let mut pc = start;
        for _ in 0..limit.min(MAX_BLOCK) {
            // lectura protegida: si la pagina desaparecio despues de comprobarla, el bloque termina antes y el
            // proximo despacho a esa instruccion es un fallo de instruccion para el guest (no un fallo del traductor)
            let Some(w) = crate::monitor::read_code(pc & TBI_MASK) else {
                if ops.is_empty() {
                    return (0, start); // ni una instruccion legible: el llamador da el fallo de instruccion
                }
                break;
            };
            let op = decode(w);
            let term = is_terminator(&op);
            ops.push((pc, op));
            if term {
                break;
            }
            pc += 4;
            if pc & 0xFFF == 0 {
                break;
            }
        }
        let end_pc = ops.last().map(|x| x.0 + 4).unwrap_or(start);
        let mut keep = std::mem::take(&mut self.ops_keep);
        let slot = if self.regs { alloc_regs(&ops) } else { [NO_SLOT; 32] };
        let mut cx = Ctx {
            a: Asm::new(),
            counted,
            known: 0,
            ops_keep: &mut keep,
            chain: self.chain && !counted,
            sites: Vec::new(),
            slot,
            valid: 0,
            fill: true,
            helper: false,
            xf_next: 0,
            xf: 0,
            fused: Vec::new(),
        };
        cx.a.prologue();
        assert_eq!(cx.a.b.len(), PROLOGUE_LEN);
        let n = ops.len();
        if !counted && PROF_ON.load(std::sync::atomic::Ordering::Relaxed) {
            PROF_OPS.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            let kind = match ops.last().map(|x| &x.1) {
                Some(Op::B { .. }) | Some(Op::Bl { .. }) | Some(Op::BCond { .. }) | Some(Op::Cbz { .. }) | Some(Op::Tbz { .. }) => 0,
                Some(Op::Br { .. }) | Some(Op::Blr { .. }) | Some(Op::Ret { .. }) => 1,
                _ => 2,
            };
            PROF_EXITS[kind].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let mut terminated = false;
        let first = self.pc_offs.len() as u32;
        for i in 0..n {
            let (pc, op) = ops[i];
            // inicio del codigo de esta instruccion (pc exacto en un fallo sincrono: `pc_at`)
            self.pc_offs.push(cx.a.b.len() as u32);
            // Los stores de flags NO se eliminan aunque otra instruccion del bloque las sobrescriba antes de leerlas
            // (`flags_dead`): un fallo sincrono o una senal entre medias veria NZCV de una instruccion anterior
            // (detectado por regs_tests::fallo_sincrono_ve_el_cpu_completo; paso 5).
            let dead = false;
            cx.xf = if self.regs && matches!(op, Op::BCond { .. } | Op::CondSel { .. }) { cx.xf_next } else { 0 };
            cx.xf_next = 0;
            if let Op::Fp(crate::fp::FpOp(w)) = op {
                // FP/SIMD (jitfp/jitneon): ranuras solo de lectura; los registros base de un load/store se cargan
                // antes, en la frontera de la instruccion (codigo lineal)
                if let Some((rn, rm)) = fp_ls_regs(w) {
                    cx.prefill(rn);
                    if let Some(rm) = rm {
                        cx.prefill(rm);
                    }
                }
                cx.fill = false;
            }
            terminated = emit_op(&mut cx, &ops, i, pc, op, dead);
            cx.fill = true;
            if cx.helper {
                // h_exec (interprete) pudo escribir cualquier registro en el Cpu
                cx.valid = 0;
                cx.helper = false;
            }
            if terminated {
                break;
            }
        }
        if !terminated {
            cx.exit_const(end_pc, n, end_pc.wrapping_sub(4));
        }
        let code = std::mem::take(&mut cx.a.b);
        self.new_sites = std::mem::take(&mut cx.sites);
        let fused = std::mem::take(&mut cx.fused);
        drop(cx);
        self.ops_keep = keep;
        assert!(self.used + code.len() < self.cap);
        let dst = unsafe { self.buf.add(self.used) };
        unsafe { std::ptr::copy_nonoverlapping(code.as_ptr(), dst, code.len()) };
        for (a, b, c) in fused {
            let d = dst as usize;
            self.fused.push((d + a, d + b, d + c));
        }
        let add = (code.len() + 15) & !15;
        let emitted = self.pc_offs.len() as u32 - first;
        self.pcs.push(BlockPcs { code: dst as usize, end: dst as usize + code.len(), pc: start, first, n: emitted });
        self.used += add;
        crate::mem::JIT_USED.fetch_add(add as u64, std::sync::atomic::Ordering::Relaxed);
        self.compiled += 1;
        (dst as usize, end_pc)
    }

    #[inline(always)]
    pub(crate) fn lookup(&mut self, pc: u64) -> BlockFn {
        let idx = ((pc >> 2) as usize) & ((1 << CACHE_BITS) - 1);
        let e = self.cache[idx];
        if e.0 == pc {
            return unsafe { std::mem::transmute::<usize, BlockFn>(e.1) };
        }
        self.lookup_miss(pc, idx)
    }

    /// Fuera de la cache directa: tabla, traduccion. Aparte (y fria) para que el bucle de despacho no reserve
    /// registros ni pila para ella.
    #[cold]
    #[inline(never)]
    fn lookup_miss(&mut self, pc: u64, idx: usize) -> BlockFn {
        let code = if let Some(&(c, _)) = self.map.get(&pc) {
            c
        } else {
            // tope por hilo, y presupuesto global: si entre todos los hilos se supera, este hilo recicla su cache
            if self.used > self.cap - (self.cap >> 3)
                || (self.used > (1 << 20) && crate::mem::JIT_USED.load(std::sync::atomic::Ordering::Relaxed) > crate::mem::JIT_BUDGET)
            {
                self.flush_all();
            }
            if crate::boundary::is_host_code(pc & TBI_MASK) {
                // no se traduce: se invoca de forma nativa (mapeo universal de argumentos)
                let c = host_call_block as usize;
                self.map.insert(pc, (c, pc));
                c
            } else if !crate::rt::addr_readable(pc & TBI_MASK) {
                // salto a memoria sin mapear (o sin lectura): fallo de instruccion para el guest, no un SIGSEGV del
                // traductor al leer el codigo. No se guarda: si el guest mapea la pagina, la proxima vez se traduce.
                return inst_abort_block;
            } else {
                let t0 = std::time::Instant::now();
                let (c, end) = self.compile(pc, MAX_BLOCK, false);
                if c == 0 {
                    return inst_abort_block; // la pagina se fue al leerla: fallo de instruccion, sin guardar
                }
                PROF_COMPILE_NS.fetch_add(t0.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
                PROF_COMPILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.map.insert(pc, (c, end));
                self.register_block(pc, c);
                c
            }
        };
        self.cache[idx] = (pc, code);
        unsafe { std::mem::transmute::<usize, BlockFn>(code) }
    }

    /// Ejecuta codigo guest desde `cpu.pc` hasta el proximo SVC/BRK/instruccion indefinida.
    pub fn run(&mut self, cpu: &mut Cpu) -> Event {
        clear_mxcsr();
        let r = self.run_inner(cpu);
        fold_mxcsr(cpu);
        r
    }

    fn run_inner(&mut self, cpu: &mut Cpu) -> Event {
        // copia de EPOCH que comprueban los saltos encadenados hacia atras (ver cabecera); una senal aplazada la
        // envenena (SIG_POISON). Primero la copia y despues la comprobacion: una senal entre medias se ve aqui.
        cpu.aux[5] = self.epoch;
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
        if self.sig_pending() {
            return Event::Signal;
        }
        let mut blocks = 0u64;
        loop {
            blocks += 1;
            if cfg!(test) {
                self.dispatched += 1;
            }
            if blocks & 0x7FF == 0 {
                PROF_BLOCKS.fetch_add(blocks, std::sync::atomic::Ordering::Relaxed);
                blocks = 0;
            }
            if crate::hle::is_hle(cpu.pc & TBI_MASK) {
                PROF_BLOCKS.fetch_add(blocks, std::sync::atomic::Ordering::Relaxed);
                return Event::Hle;
            }
            // Una sola comparacion para las dos cosas: aux[5] es la copia de EPOCH (igual a self.epoch) salvo si
            // EPOCH cambio o si una senal aplazada la enveneno (cabecera, "Senales"). Va justo antes de entrar al
            // bloque, despues de toda escritura de aux[5] en esta vuelta: una senal que llegue despues de leerla deja
            // aux[5] envenenada y sale en el siguiente salto hacia atras o vuelve aqui.
            let ep = EPOCH.load(std::sync::atomic::Ordering::Acquire);
            if ep != unsafe { std::ptr::read_volatile(&cpu.aux[5]) } {
                if ep != self.epoch {
                    // primero la copia: una publicacion que termine despues vuelve a cambiar EPOCH y se procesa
                    self.epoch = ep;
                    cpu.aux[5] = ep;
                    self.ic_catch_up();
                } else {
                    cpu.aux[5] = ep;
                }
                std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
                if self.sig_pending() {
                    return Event::Signal;
                }
            }
            let f = self.lookup(cpu.pc);
            // el prologo del bloque apunta su marco en cpu.host_sp: un fallo sincrono gestionado por el guest vuelve
            // aqui con 0 (heddle_jit_abort, sig.rs); fuera del bloque, host_sp = 0
            let st = f(cpu);
            cpu.host_sp = 0;
            if st == 0 {
                continue; // lo habitual (salto indirecto, cadena cortada): sin pasar por la tabla de saltos del match
            }
            match st {
                0 => {}
                1 => return Event::Svc(cpu.aux[1] as u16),
                2 => return Event::Brk(cpu.aux[1] as u16),
                3 => return Event::Undef(cpu.aux[1] as u32),
                4 => return Event::HostCall,
                7 => return Event::InstAbort,
                // 5: IC IVAU / IC IALLU (h_exec)
                _ => {
                    self.invalidate(cpu.aux[1]);
                    // los demas hilos deben descartar sus copias traducidas de esa linea
                    let (s, prev) = ic_publish(cpu.aux[1]);
                    // si nadie publico entre medias, este hilo ya esta al dia; si no, ic_catch_up procesara tambien
                    // lo de los demas (adoptar el EPOCH nuevo sin mas perderia una subida concurrente)
                    if s == self.ic_seq && prev == self.epoch {
                        self.ic_seq = s + 1;
                        self.epoch = prev + 1;
                        cpu.aux[5] = self.epoch;
                    }
                }
            }
        }
    }

    /// Procesa las invalidaciones de IC_LOG publicadas por otros hilos desde la ultima vez (ver IC_RING).
    #[cold]
    #[inline(never)]
    fn ic_catch_up(&mut self) {
        use std::sync::atomic::Ordering::Acquire;
        let lost = IC_LOST.load(Acquire);
        let end = IC_SEQ.load(Acquire);
        if lost != self.ic_lost || end.wrapping_sub(self.ic_seq) > IC_RING as u64 {
            self.ic_lost = lost;
            self.ic_seq = end;
            self.flush_all();
            return;
        }
        let mut lines = Vec::new();
        let mut s = self.ic_seq;
        while s != end {
            let v = IC_LOG[(s % IC_RING as u64) as usize].load(Acquire);
            let d = ic_ahead(ic_tag(s), v & 63);
            if d != 0 && d < 32 {
                break; // aun no escrita: su publicador subira EPOCH al terminar y se retoma aqui
            }
            if d != 0 || v & !63 == 0 {
                // pisada por una vuelta posterior, o IC IALLU: todo (y lo anterior a `end` ya se ve al traducir)
                self.ic_seq = end;
                self.flush_all();
                return;
            }
            lines.push(v & !63);
            s += 1;
        }
        self.ic_seq = s;
        if !lines.is_empty() {
            self.invalidate_lines(&mut lines);
        }
    }

    /// Ejecuta exactamente `n` instrucciones guest (para pruebas diferenciales). Bloques sin cache.
    pub fn run_n(&mut self, cpu: &mut Cpu, n: usize) -> Result<(), String> {
        clear_mxcsr();
        let r = self.run_n_inner(cpu, n);
        fold_mxcsr(cpu);
        r
    }

    fn run_n_inner(&mut self, cpu: &mut Cpu, n: usize) -> Result<(), String> {
        self.flush_all();
        let mut left = n;
        while left > 0 {
            let (code, _) = self.compile(cpu.pc, left, true);
            if code == 0 {
                return Err(format!("instruccion ilegible en {:#x}", cpu.pc));
            }
            cpu.aux[2] = 0;
            let f: BlockFn = unsafe { std::mem::transmute::<usize, BlockFn>(code) };
            let st = f(cpu);
            cpu.host_sp = 0;
            let done = (cpu.aux[2] as usize).max(1).min(left);
            left -= done;
            match st {
                0 | 5 => {}
                1 => {}
                s => return Err(format!("flujo inesperado {} (aux {:#x})", s, cpu.aux[1])),
            }
            if self.used > self.cap / 2 {
                self.flush_all();
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Asignacion de registros (ver cabecera, "Registros")
// ---------------------------------------------------------------------------------------------

/// Registros base (rn) e indice (rm, forma con registro) de un load/store FP/SIMD; None si `w` no es de esa clase.
/// Solo decide que precargar: un registro de mas o de menos no cambia el resultado, solo el coste.
fn fp_ls_regs(w: u32) -> Option<(Reg, Option<Reg>)> {
    if (w >> 25) & 0b101 != 0b100 || (w >> 26) & 1 == 0 {
        return None; // no es load/store, o no es SIMD/FP (bit V)
    }
    let rn = ((w >> 5) & 31) as Reg;
    // forma con registro de desplazamiento (LDR/STR q, [xn, xm]): bits 29..28 = 11, 24 = 0, 21 = 1, 11..10 = 10
    let rm = if (w >> 28) & 3 == 3 && (w >> 24) & 1 == 0 && (w >> 21) & 1 == 1 && (w >> 10) & 3 == 2 { Some(((w >> 16) & 31) as Reg) } else { None };
    Some((rn, rm))
}

/// Registros ARM que usa cada instruccion por los accesos `ld_x`/`st_x` (para el conteo estatico).
fn op_regs(op: &Op, f: &mut dyn FnMut(Reg)) {
    let addr_rm = |a: &Addr, f: &mut dyn FnMut(Reg)| {
        if let Addr::Reg { rm, .. } = a {
            f(*rm)
        }
    };
    match *op {
        Op::Adr { rd, .. } => f(rd),
        Op::AddSubImm { rd, rn, .. } | Op::LogImm { rd, rn, .. } | Op::Bitfield { rd, rn, .. } | Op::Dp1 { rd, rn, .. } => {
            f(rd);
            f(rn)
        }
        Op::MovWide { kind, rd, .. } if kind != 3 => f(rd),
        Op::Extr { rd, rn, rm, .. }
        | Op::AddSubShift { rd, rn, rm, .. }
        | Op::AddSubExt { rd, rn, rm, .. }
        | Op::LogShift { rd, rn, rm, .. }
        | Op::CondSel { rd, rn, rm, .. }
        | Op::MulH { rd, rn, rm, .. }
        | Op::ShiftV { rd, rn, rm, .. } => {
            f(rd);
            f(rn);
            f(rm)
        }
        Op::Madd { rd, rn, rm, ra, .. } | Op::MaddL { rd, rn, rm, ra, .. } => {
            f(rd);
            f(rn);
            f(rm);
            f(ra)
        }
        Op::Bl { .. } => f(30),
        Op::Br { rn } | Op::Ret { rn } => f(rn),
        Op::Blr { rn } => {
            f(rn);
            f(30)
        }
        Op::Cbz { rt, .. } | Op::Tbz { rt, .. } | Op::LoadLit { rt, .. } | Op::Mrs { rt, .. } | Op::Msr { rt, .. } => f(rt),
        Op::Load { rt, rn, ref addr, .. } | Op::Store { rt, rn, ref addr, .. } => {
            f(rt);
            f(rn);
            addr_rm(addr, f)
        }
        Op::LoadPair { rt, rt2, rn, ref addr, .. } | Op::StorePair { rt, rt2, rn, ref addr, .. } => {
            f(rt);
            f(rt2);
            f(rn);
            addr_rm(addr, f)
        }
        Op::Ldar { rt, rn, .. } => {
            f(rt);
            f(rn)
        }
        Op::Fp(crate::fp::FpOp(w)) => {
            if let Some((rn, rm)) = fp_ls_regs(w) {
                f(rn);
                if let Some(rm) = rm {
                    f(rm)
                }
            }
        }
        _ => {}
    }
}

/// Elige hasta `HREGS.len()` registros ARM para el bloque: los de mas usos estaticos (al menos 2: con uno no hay
/// nada que ahorrar), desempate por numero de registro. XZR/SP comparten el indice 31: `ld_x`/`st_x` filtran XZR
/// antes de mirar la ranura, asi que la ranura 31 siempre es SP.
fn alloc_regs(ops: &[(u64, Op)]) -> [u8; 32] {
    let mut cnt = [0u32; 32];
    for (_, op) in ops {
        op_regs(op, &mut |r| cnt[r as usize & 31] += 1);
    }
    let mut order: Vec<usize> = (0..32).filter(|&r| cnt[r] >= 2).collect();
    order.sort_by(|&a, &b| cnt[b].cmp(&cnt[a]).then(a.cmp(&b)));
    let mut slot = [NO_SLOT; 32];
    for (i, &r) in order.iter().take(HREGS.len()).enumerate() {
        slot[r] = i as u8;
    }
    slot
}

// ---------------------------------------------------------------------------------------------
// Traduccion por instruccion. Devuelve true si el bloque termino (ya emitio la salida).
// ---------------------------------------------------------------------------------------------

fn emit_op(cx: &mut Ctx, ops: &[(u64, Op)], i: usize, pc: u64, op: Op, dead: bool) -> bool {
    let _ = ops;
    let n = i + 1;
    match op {
        // MsrImm (CFINV/XAFLAG/AXFLAG), RMIF y SETF van por el helper (`other`): reescriben NZCV
        Op::Nop | Op::Prefetch | Op::Barrier(Barrier::Isb) => {}
        Op::Barrier(Barrier::Dmb) | Op::Barrier(Barrier::Dsb) => cx.a.mfence(),
        Op::Adr { rd, imm, page } => {
            let base = if page { pc & !0xFFF } else { pc };
            cx.a.mov_ri(RAX, base.wrapping_add(imm as u64));
            cx.st_x(rd, RAX, true);
        }
        Op::AddSubImm { sf, sub, s, rd, rn, imm } => {
            cx.ld_x(RAX, rn, false);
            if s && !dead {
                let k = match (sub, sf) {
                    (true, true) => LF_SUB64,
                    (true, false) => LF_SUB32,
                    (false, true) => LF_ADD64,
                    (false, false) => LF_ADD32,
                };
                cx.store_flags(k, RAX, None, imm as i32);
            }
            cx.a.alu_ri(sf, if sub { 5 } else { 0 }, RAX, imm as i32);
            if s && !dead {
                cx.xf_next = cx.known;
            }
            cx.st_x(rd, RAX, s);
        }
        Op::AddSubShift { sf, sub, s, rd, rn, rm, shift, amt } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RDX, rm, true);
            if amt > 0 {
                cx.a.shift_ri(sf, shift_ext(shift), RDX, amt);
            }
            if s && !dead {
                let k = match (sub, sf) {
                    (true, true) => LF_SUB64,
                    (true, false) => LF_SUB32,
                    (false, true) => LF_ADD64,
                    (false, false) => LF_ADD32,
                };
                cx.store_flags(k, RAX, Some(RDX), 0);
            }
            cx.a.alu_rr(sf, if sub { 0x29 } else { 0x01 }, RAX, RDX);
            if s && !dead {
                cx.xf_next = cx.known;
            }
            cx.st_x(rd, RAX, true);
        }
        Op::AddSubExt { sf, sub, s, rd, rn, rm, ext, amt } => {
            cx.ld_x(RAX, rn, false);
            cx.ld_x(RDX, rm, true);
            cx.a.ext_rr(RDX, ext);
            if amt > 0 {
                cx.a.shift_ri(true, 4, RDX, amt);
            }
            if s && !dead {
                let k = match (sub, sf) {
                    (true, true) => LF_SUB64,
                    (true, false) => LF_SUB32,
                    (false, true) => LF_ADD64,
                    (false, false) => LF_ADD32,
                };
                cx.store_flags(k, RAX, Some(RDX), 0);
            }
            cx.a.alu_rr(sf, if sub { 0x29 } else { 0x01 }, RAX, RDX);
            if s && !dead {
                cx.xf_next = cx.known;
            }
            cx.st_x(rd, RAX, s);
        }
        Op::LogImm { sf, op, rd, rn, imm } => {
            cx.ld_x(RAX, rn, true);
            let (opc, ext) = match op {
                LogOp::And | LogOp::Ands => (0x21, 4),
                LogOp::Orr => (0x09, 1),
                LogOp::Eor => (0x31, 6),
            };
            cx.a.alu_imm_any(sf, opc, ext, RAX, imm, R8);
            if op == LogOp::Ands {
                if !dead {
                    cx.store_flags(if sf { LF_LOGIC64 } else { LF_LOGIC32 }, RAX, None, 0);
                    cx.xf_next = cx.known; // `and` fija SF/ZF del resultado y CF = OF = 0, como `test`
                }
                cx.st_x(rd, RAX, true);
            } else {
                cx.st_x(rd, RAX, false);
            }
        }
        Op::LogShift { sf, op, inv, rd, rn, rm, shift, amt } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RDX, rm, true);
            if amt > 0 {
                cx.a.shift_ri(sf, shift_ext(shift), RDX, amt);
            }
            if inv {
                cx.a.unary(sf, 2, RDX);
            }
            let opc = match op {
                LogOp::And | LogOp::Ands => 0x21,
                LogOp::Orr => 0x09,
                LogOp::Eor => 0x31,
            };
            cx.a.alu_rr(sf, opc, RAX, RDX);
            if op == LogOp::Ands && !dead {
                cx.store_flags(if sf { LF_LOGIC64 } else { LF_LOGIC32 }, RAX, None, 0);
                cx.xf_next = cx.known;
            }
            cx.st_x(rd, RAX, true);
        }
        Op::MovWide { sf, kind, rd, imm, hw } => {
            if rd != 31 {
                let sh = hw as u32 * 16;
                let v = (imm as u64) << sh;
                let mk = if sf { u64::MAX } else { 0xFFFF_FFFF };
                match kind {
                    0 | 2 => {
                        let r = if kind == 0 { !v & mk } else { v };
                        cx.a.mov_ri(RAX, r);
                        cx.st_x(rd, RAX, true);
                    }
                    _ => {
                        cx.a.mov_mi16(RBX, xoff(rd) + 2 * hw as i32, imm);
                        if !sf {
                            cx.a.mov_mi32(RBX, xoff(rd) + 4, 0);
                        }
                        cx.forget(rd);
                    }
                }
            }
        }
        Op::Bitfield { sf, op, rd, rn, immr, imms } => {
            if !cx.bitfield(sf, op, rd, rn, immr, imms) {
                cx.call_helper_exec(pc, op_bitfield(sf, op, rd, rn, immr, imms), i);
            }
        }
        Op::Extr { sf, rd, rn, rm, lsb } => {
            cx.ld_x(RAX, rm, true);
            cx.ld_x(RDX, rn, true);
            cx.a.shrd_ri(sf, RAX, RDX, lsb);
            cx.st_x(rd, RAX, true);
        }
        Op::CondSel { sf, op, rd, rn, rm, cond } => {
            let cc = cx.emit_cond(cond);
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RDX, rm, true);
            match op {
                CselOp::Csel => {}
                CselOp::Csinc => cx.a.lea(RDX, RDX, 1, sf),
                CselOp::Csinv => cx.a.unary(sf, 2, RDX),
                CselOp::Csneg => {
                    cx.a.unary(sf, 2, RDX);
                    cx.a.lea(RDX, RDX, 1, sf);
                }
            }
            match cc {
                Some(cc) => {
                    cx.a.cmov(sf, cc, RDX, RAX);
                    cx.st_x(rd, RDX, true);
                }
                None => {
                    if !sf {
                        cx.a.mov_rr(RAX, RAX, false);
                    }
                    cx.st_x(rd, RAX, true);
                }
            }
        }
        Op::Madd { sf, sub, rd, rn, rm, ra } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RDX, rm, true);
            cx.a.imul_rr(sf, RAX, RDX);
            cx.ld_x(RCX, ra, true);
            if sub {
                cx.a.alu_rr(sf, 0x29, RCX, RAX);
            } else {
                cx.a.alu_rr(sf, 0x01, RCX, RAX);
            }
            cx.st_x(rd, RCX, true);
        }
        Op::MaddL { signed, sub, rd, rn, rm, ra } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RDX, rm, true);
            let e = if signed { Ext::Sxtw } else { Ext::Uxtw };
            cx.a.ext_rr(RAX, e);
            cx.a.ext_rr(RDX, e);
            cx.a.imul_rr(true, RAX, RDX);
            cx.ld_x(RCX, ra, true);
            if sub {
                cx.a.alu_rr(true, 0x29, RCX, RAX);
            } else {
                cx.a.alu_rr(true, 0x01, RCX, RAX);
            }
            cx.st_x(rd, RCX, true);
        }
        Op::MulH { signed, rd, rn, rm } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RCX, rm, true);
            cx.a.unary(true, if signed { 5 } else { 4 }, RCX);
            cx.st_x(rd, RDX, true);
        }
        Op::ShiftV { sf, shift, rd, rn, rm } => {
            cx.ld_x(RAX, rn, true);
            cx.ld_x(RCX, rm, true);
            cx.a.shift_cl(sf, shift_ext(shift), RAX);
            cx.st_x(rd, RAX, true);
        }
        Op::Dp1 { sf, op: Dp1Op::Rev, rd, rn } => {
            cx.ld_x(RAX, rn, true);
            cx.a.bswap(sf, RAX);
            cx.st_x(rd, RAX, true);
        }
        Op::B { imm } => {
            cx.exit_const(pc.wrapping_add(imm as u64), n, pc);
            return true;
        }
        Op::Bl { imm } => {
            cx.a.mov_ri(RAX, pc + 4);
            cx.st_x(30, RAX, false);
            cx.exit_const(pc.wrapping_add(imm as u64), n, pc);
            return true;
        }
        Op::Br { rn } | Op::Ret { rn } => {
            cx.ld_x(RAX, rn, true);
            cx.exit_rax(n);
            return true;
        }
        Op::Blr { rn } => {
            cx.ld_x(RAX, rn, true);
            if !cx.counted && tlsdesc_call(ops, i, rn) {
                let r = crate::tls::resolver_at();
                if r != 0 {
                    cx.tlsdesc_inline(pc, n, r);
                }
            }
            cx.a.mov_ri(RCX, pc + 4);
            cx.st_x(30, RCX, false);
            cx.exit_rax(n);
            return true;
        }
        Op::BCond { cond, imm } => {
            match cx.emit_cond(cond) {
                None => cx.exit_const(pc.wrapping_add(imm as u64), n, pc),
                Some(cc) => {
                    let taken = cx.a.jcc_fwd(cc);
                    cx.exit_const(pc + 4, n, pc);
                    cx.a.patch_here(taken);
                    cx.exit_const(pc.wrapping_add(imm as u64), n, pc);
                }
            }
            return true;
        }
        Op::Cbz { sf, nz, rt, imm } => {
            cx.ld_x(RAX, rt, true);
            cx.a.alu_rr(sf, 0x85, RAX, RAX);
            let taken = cx.a.jcc_fwd(if nz { CC_NE } else { CC_E });
            cx.exit_const(pc + 4, n, pc);
            cx.a.patch_here(taken);
            cx.exit_const(pc.wrapping_add(imm as u64), n, pc);
            return true;
        }
        Op::Tbz { nz, rt, bit, imm } => {
            cx.ld_x(RAX, rt, true);
            cx.a.bt_ri(RAX, bit);
            let taken = cx.a.jcc_fwd(if nz { CC_B } else { CC_AE });
            cx.exit_const(pc + 4, n, pc);
            cx.a.patch_here(taken);
            cx.exit_const(pc.wrapping_add(imm as u64), n, pc);
            return true;
        }
        Op::Load { size, sext, rt, rn, addr } => {
            cx.emit_ea(rn, addr);
            cx.mask_tbi(RAX);
            cx.a.load_ext(RDX, RAX, 0, size, sext);
            cx.post_wb(rn, addr);
            cx.st_x(rt, RDX, true);
        }
        Op::Store { size, rt, rn, addr } => {
            cx.emit_ea(rn, addr);
            cx.mask_tbi(RAX);
            cx.ld_x(RDX, rt, true);
            cx.a.mov_rr(RDI, RAX, true);
            cx.call_store(size);
            cx.post_wb(rn, addr);
        }
        Op::LoadLit { size, sext, rt, imm } => {
            cx.a.mov_ri(RAX, pc.wrapping_add(imm as u64) & TBI_MASK);
            cx.a.load_ext(RDX, RAX, 0, size, if sext != 0 { 64 } else { 0 });
            cx.st_x(rt, RDX, true);
        }
        Op::LoadPair { size, sext, rt, rt2, rn, addr } => {
            cx.emit_ea(rn, addr);
            cx.mask_tbi(RAX);
            let step = 1i32 << size;
            let sx = if sext { 64 } else { 0 };
            cx.a.load_ext(RDX, RAX, 0, size, sx);
            cx.a.load_ext(RCX, RAX, step, size, sx);
            cx.post_wb_keep(rn, addr);
            cx.st_x(rt, RDX, true);
            cx.st_x(rt2, RCX, true);
        }
        Op::StorePair { size, rt, rt2, rn, addr } => {
            cx.emit_ea(rn, addr);
            cx.mask_tbi(RAX);
            cx.a.mov_mr(RBX, O_AUX_SCRATCH, RAX, true);
            cx.ld_x(RDX, rt, true);
            cx.a.mov_rr(RDI, RAX, true);
            cx.call_store(size);
            cx.a.mov_rm(RDI, RBX, O_AUX_SCRATCH, true);
            cx.a.lea(RDI, RDI, 1 << size, true);
            cx.ld_x(RDX, rt2, true);
            cx.call_store(size);
            cx.post_wb(rn, addr);
        }
        Op::Ldar { size, rt, rn } => {
            cx.ld_x(RAX, rn, false);
            cx.mask_tbi(RAX);
            if size > 0 {
                // desalineado: SIGBUS BUS_ADRALN en esta instruccion (interp::check_align, sig::align_fault); el pc
                // exacto sale de la direccion de retorno de la llamada (Jit::fault_pc)
                cx.a.mov_rr(RCX, RAX, true);
                cx.a.alu_ri(true, 4, RCX, (1 << size) - 1);
                let ok = cx.a.jcc_fwd(CC_E);
                cx.a.mov_rr(RDI, RAX, true);
                cx.a.call_abs(h_align_fault as usize);
                cx.a.patch_here(ok);
            }
            cx.a.load_ext(RDX, RAX, 0, size, 0);
            cx.st_x(rt, RDX, true);
        }
        Op::Mrs { rt, sysreg: 0xDE82 } => {
            cx.a.mov_rm(RAX, RBX, O_TPIDR, true);
            cx.st_x(rt, RAX, true);
        }
        Op::Msr { rt, sysreg: 0xDE82 } => {
            cx.ld_x(RAX, rt, true);
            cx.a.mov_mr(RBX, O_TPIDR, RAX, true);
        }
        Op::Fp(crate::fp::FpOp(w)) if jitfp::try_emit(cx, w, pc, i) => {}
        // Todo lo demas: helper con el interprete de referencia (mismo codigo, misma semantica)
        other => {
            cx.call_helper_exec(pc, other, i);
            return false;
        }
    }
    false
}

fn op_bitfield(sf: bool, op: BfOp, rd: Reg, rn: Reg, immr: u8, imms: u8) -> Op {
    Op::Bitfield { sf, op, rd, rn, immr, imms }
}

impl<'a> Ctx<'a> {
    /// Igual que `post_wb` pero sin tocar RDX/RCX (usados por LDP).
    fn post_wb_keep(&mut self, rn: Reg, addr: Addr) {
        if let Addr::Post(i) | Addr::Pre(i) = addr {
            self.ld_x(R9, rn, false);
            self.a.lea(R9, R9, i as i32, true);
            self.st_x(rn, R9, false);
        }
    }
}

/// LDAR/LDAPR desalineado en codigo traducido (no vuelve: ver `sig::align_fault`).
extern "C" fn h_align_fault(a: u64) -> u64 {
    crate::sig::align_fault(a)
}

/// MXCSR por defecto (RN, todo enmascarado, flags limpias) al entrar en codigo traducido.
// ---------------------------------------------------------------------------------------------
// TLSDESC en linea
// ---------------------------------------------------------------------------------------------
//
// Una llamada TLSDESC (`adrp x0; ldr xN, [x0, #k]; add x0, x0, #k; blr xN`, la secuencia fija del ABI) salta al
// resolutor del descriptor. Si es el resolutor dinamico del puente (`tls::resolver`, codigo ARM64 que copia el de
// bionic), su ruta rapida se emite en linea en el sitio del `blr`, sin pasar por el despachador ni a la ida ni a la
// vuelta. En el sitio:
//  * `xN != tls::resolver_at()` (otro resolutor, o descriptor sin resolver): el `blr` de siempre.
//  * Si coincide: x30 = pc + 4 (lo que escribe el `blr`) y la ruta rapida del resolutor con la misma semantica: sp
//    (los 32 bytes que el resolutor guarda bajo sp deben ser accesibles), tp = tpidr_el0, dtv = [tp], generacion del
//    DTV [dtv] frente a la del argumento [[x0 + 8]], bloque [dtv + 8 * id] no nulo; x0 = bloque + offset - tp y
//    NZCV = `cmp` de las dos generaciones (como `cmp x21, x22`). x19-x22, x16, x30 y sp quedan como al volver del
//    resolutor; despues, salto encadenado a pc + 4. Lo unico que no se reproduce es el CONTENIDO de esos 32 bytes bajo
//    sp (el resolutor guarda ahi x19-x22 y los repone): memoria ya liberada de la pila, que AAPCS64 no conserva.
//  * Generacion vieja o bloque sin reservar: sigue en el resolutor justo donde ARM llama a la ruta lenta
//    (`tls::RESOLVER_SLOW_OFF`, tras reponer x19-x22) con su mismo estado: x0 = argumento, NZCV del `cmp`. Desde ahi,
//    el codigo ARM traducido de siempre (`__heddle_tlsdesc_slow`, que preserva todo salvo x0).
//  * Fallo sincrono en cualquier acceso de la ruta rapida (descriptor o tp invalidos, pila sin mapear, puntero con
//    etiqueta TBI, que en x86 no es canonico): no se escribio nada guest todavia (salvo x30, que el `blr` tambien
//    escribe), asi que `fault_in_fused` (al principio de `sig::sync_fault`, antes de mirar si el guest tiene
//    manejador) mueve el rip a un `cpu.pc = resolver` y salida al despachador: el resolutor traducido desde su primera
//    instruccion repite el acceso (con TBI, funciona como en ARM) y, si falla, el guest ve el pc y los registros
//    exactos de ARM.
//  * Senales asincronas: la ruta en linea no tiene salidas; el guest ve la senal antes del `blr` o tras volver del
//    resolutor, nunca dentro (como si llegara en esas fronteras).
// No se usa en el modo contado (`run_n`): alli cada instruccion del resolutor cuenta.

/// `ops[i]` (`blr rn`) cierra la secuencia de llamada TLSDESC?
fn tlsdesc_call(ops: &[(u64, Op)], i: usize, rn: Reg) -> bool {
    if i < 2 || rn == 0 {
        return false;
    }
    let Op::AddSubImm { sf: true, sub: false, s: false, rd: 0, rn: 0, imm } = ops[i - 1].1 else { return false };
    matches!(ops[i - 2].1, Op::Load { size: 3, sext: 0, rt, rn: 0, addr: Addr::Off(o) } if rt == rn && o as u64 == imm)
}

impl<'a> Ctx<'a> {
    /// Ruta rapida TLSDESC en linea (ver arriba). RAX = destino del `blr`; si no es `resolver`, sigue el codigo del
    /// `blr` que se emite despues.
    fn tlsdesc_inline(&mut self, pc: u64, n: usize, resolver: u64) {
        let (valid, known) = (self.valid, self.known);
        self.a.mov_ri(RCX, resolver);
        self.a.alu_rr(true, 0x39, RAX, RCX);
        let normal = self.a.jcc_fwd(CC_NE);
        self.a.mov_ri(RCX, pc + 4);
        self.st_x(30, RCX, false);
        let f0 = self.a.b.len();
        // la pila que el resolutor usaria: [sp - 32, sp)
        self.a.mov_rm(RDI, RBX, xoff(31), true);
        self.a.load_ext(RCX, RDI, -32, 0, 0);
        self.a.load_ext(RCX, RDI, -1, 0, 0);
        self.a.mov_rm(RSI, RBX, O_TPIDR, true); // x19 = tp
        self.a.mov_rm(RDX, RSI, 0, true); // x20 = [tp] (TLS_SLOT_DTV: &dtv->generation)
        self.a.mov_rm(R8, RBX, xoff(0), true);
        self.a.mov_rm(R8, R8, 8, true); // x0 = TlsDynamicResolverArg*
        self.a.mov_rm(RAX, RDX, 0, true); // x21 = dtv->generation
        self.a.mov_rm(RCX, R8, 0, true); // x22 = arg->generation
        self.a.alu_rr(true, 0x39, RAX, RCX);
        let old = self.a.jcc_fwd(CC_B);
        self.a.mov_rm(R9, R8, 8, true); // module_id
        self.a.shift_ri(true, 4, R9, 3);
        self.a.alu_rr(true, 0x01, R9, RDX);
        self.a.mov_rm(R9, R9, 0, true); // dtv->modules[module_id - 1]
        self.a.alu_rr(true, 0x85, R9, R9);
        let none = self.a.jcc_fwd(CC_E);
        self.a.op_rm(true, &[0x03], R9, R8, 16); // + arg->offset
        let f1 = self.a.b.len();
        let v1 = self.valid;
        self.a.alu_rr(true, 0x29, R9, RSI);
        self.store_flags(LF_SUB64, RAX, Some(RCX), 0);
        self.st_x(0, R9, false);
        self.exit_const(pc + 4, n, pc);
        // ruta lenta: el estado de ARM en `lento:` tras reponer x19-x22 (x0 = argumento, NZCV del `cmp`)
        self.valid = v1;
        self.a.patch_here(old);
        self.a.patch_here(none);
        self.store_flags(LF_SUB64, RAX, Some(RCX), 0);
        self.st_x(0, R8, false);
        self.exit_const(resolver + crate::tls::RESOLVER_SLOW_OFF, n, pc);
        // fallo en un acceso: el resolutor traducido desde el principio
        let slow = self.a.b.len();
        self.a.mov_ri(RAX, resolver);
        self.exit_rax(n);
        self.fused.push((f0, f1, slow));
        self.a.patch_here(normal);
        self.valid = valid;
        self.known = known;
    }
}

impl Jit {
    /// Ruta lenta de la ruta rapida TLSDESC en linea cuyo codigo contiene `a` (ver "TLSDESC en linea").
    fn fused_slow(&self, a: usize) -> Option<usize> {
        let i = self.fused.partition_point(|f| f.0 <= a);
        let f = self.fused.get(i.checked_sub(1)?)?;
        (a < f.1).then_some(f.2)
    }
}

/// Llamar al principio del manejador de un fallo sincrono (`sig::sync_fault`, tras `fault_in_locked`): si el fallo
/// es un acceso de una ruta rapida TLSDESC en linea del Jit de este hilo, el rip pasa a su ruta lenta (el resolutor
/// traducido repite el acceso con la semantica exacta) y devuelve true. Sin reservas ni bloqueos.
///
/// # Safety
/// `uc` es nulo o el ucontext que el nucleo entrego al manejador.
pub unsafe fn fault_in_fused(uc: *mut std::ffi::c_void) -> bool {
    if uc.is_null() {
        return false;
    }
    let Some(t) = crate::rt::cur_opt() else { return false };
    if !t.sig_cpu.is_null() || t.phase != 0 || t.sig_setup {
        return false;
    }
    let hsp = t.cpu.host_sp;
    let g = unsafe { (uc as *mut u8).add(40) as *mut u64 };
    let (rsp, rip) = unsafe { (*g.add(15), *g.add(16)) };
    // la ruta en linea no llama a nada: el fallo es del propio bloque, con RSP en su cuerpo
    if hsp == 0 || rsp != hsp || !t.jit.code_contains(rip) {
        return false;
    }
    match t.jit.fused_slow(rip as usize) {
        Some(slow) => {
            unsafe { *g.add(16) = slow as u64 };
            true
        }
        None => false,
    }
}

pub fn clear_mxcsr() {
    unsafe { core::arch::x86_64::_mm_setcsr(0x1F80) };
}

/// Pasa las flags acumuladas de MXCSR (ZE->DZC, OE->OFC, PE->IXC) a FPSR y las limpia.
/// IE/UE/DE no se pasan: las rutas rapidas nunca aceptan resultados con esas condiciones.
pub fn fold_mxcsr(cpu: &mut Cpu) {
    let m = unsafe { core::arch::x86_64::_mm_getcsr() };
    if m & 0x3F != 0 {
        cpu.fpsr |= ((m & 0x2c) >> 1) as u64;
        unsafe { core::arch::x86_64::_mm_setcsr(m & !0x3F) };
    }
}

/// Microbenchmark del coste unitario de las instrucciones que caen al interprete:
/// `cargo test --release --lib jit::bench -- --ignored --nocapture --test-threads=1`
#[cfg(test)]
mod bench {
    use super::*;
    use std::time::Instant;
    use std::sync::atomic::Ordering::Relaxed;

    #[repr(align(4096))]
    struct Page([u32; 1024]);

    const K: usize = 32;
    const REPS_DIRECT: u64 = 20_000_000;
    const REPS_BLOCK: u64 = 400_000;

    static mut BUF: [u64; 8] = [0; 8];

    fn cpu(fpcr: u64) -> Box<Cpu> {
        let mut c = Cpu::new();
        c.x[1] = 0x1234_5678_9abc_def0;
        c.x[2] = 0x0fed_cba9_8765_4321;
        c.x[3] = std::ptr::addr_of_mut!(BUF) as u64; // memoria valida para LDXR/STXR/LDADD/STLR
        c.fpcr = fpcr;
        for r in 0..8 {
            c.v[r] = [(1.5f64 + r as f64).to_bits(), ((2.5f32 + r as f32).to_bits() as u64) << 32 | (1.25f32).to_bits() as u64];
        }
        c
    }

    /// Bloque: `n` copias de `w` seguidas de B al inicio. Devuelve ns por ejecucion del bloque entero y las llamadas a h_exec por ejecucion.
    fn block_ns(w: u32, n: usize, fpcr: u64) -> (f64, u64) {
        let mut page = Box::new(Page([0; 1024]));
        for i in 0..n {
            page.0[i] = w;
        }
        page.0[n] = 0x1400_0000 | ((-(n as i64)) as u32 & 0x03FF_FFFF);
        let pc = page.0.as_ptr() as u64;
        let mut jit = Jit::new();
        jit.chain = false; // el bloque termina en un B a si mismo: encadenado seria un bucle infinito
        let mut c = cpu(fpcr);
        c.pc = pc;
        let f = jit.lookup(pc);
        for _ in 0..1000 {
            f(&mut *c);
        }
        let h0 = HELPER_CALLS.load(Relaxed);
        let t0 = Instant::now();
        for _ in 0..REPS_BLOCK {
            f(&mut *c);
        }
        let ns = t0.elapsed().as_nanos() as f64 / REPS_BLOCK as f64;
        (ns, (HELPER_CALLS.load(Relaxed) - h0) / REPS_BLOCK)
    }

    fn direct_ns(w: u32, fpcr: u64) -> f64 {
        let op = decode(w);
        let mut c = cpu(fpcr);
        c.pc = 0x1000;
        for _ in 0..10000 {
            std::hint::black_box(interp::exec(&mut c, std::hint::black_box(&op)));
        }
        let t0 = Instant::now();
        for _ in 0..REPS_DIRECT {
            std::hint::black_box(interp::exec(&mut c, std::hint::black_box(&op)));
        }
        t0.elapsed().as_nanos() as f64 / REPS_DIRECT as f64
    }

    #[test]
    #[ignore]
    fn bench_interp_vs_helper() {
        let (base, _) = block_ns(0xD503201F, 0, 0); // solo el B (bloque vacio): coste fijo de la llamada al bloque
        println!("BENCH base: bloque de solo B = {:.2} ns por ejecucion", base);
        const FZ: u64 = 1 << 24; // FPCR.FZ distinto de 0: el JIT descarta su ruta rapida de FP/SIMD
        let casos: &[(&str, u32, u64)] = &[
            ("ADD x0,x1,x2 (traducida)", 0x8B020020, 0),
            ("FADD d0,d1,d2 (fpcr=0, ruta rapida)", 0x1E622820, 0),
            ("FADD d0,d1,d2 (fpcr=FZ, ruta lenta)", 0x1E622820, FZ),
            ("FMLA v0.4s (fpcr=0, ruta rapida)", 0x4E22CC20, 0),
            ("FMLA v0.4s,v1.4s,v2.4s (fpcr=FZ)", 0x4E22CC20, FZ),
            ("ADD v0.4s (fpcr=0)", 0x4EA28420, 0),
            ("ADD v0.4s,v1.4s,v2.4s (fpcr=FZ)", 0x4EA28420, FZ),
            ("FRINTM d0,d1 (fpcr=0)", 0x1E654020, 0),
            ("FCVT d0,s1 (fpcr=0)", 0x1E22C020, 0),
            ("CCMP x1,x2,#0,eq", 0xFA420020, 0),
            ("ADC x0,x1,x2", 0x9A020020, 0),
            ("UDIV x0,x1,x2", 0x9AC20820, 0),
            ("SDIV x0,x1,x2", 0x9AC20C20, 0),
            ("CLZ x0,x1", 0xDAC01020, 0),
            ("RBIT x0,x1", 0xDAC00020, 0),
            ("MRS x0,NZCV", 0xD53B4200, 0),
            ("MRS x0,FPCR", 0xD53B4400, 0),
            ("LDXR x0,[x3]", 0xC85F7C60, 0),
            ("STXR w4,x0,[x3]", 0xC8047C60, 0),
            ("STLR x0,[x3]", 0xC89FFC60, 0),
            ("LDADD x1,x0,[x3]", 0xF8210060, 0),
            ("EXTR x0,x1,x2,#5 (control)", 0x93C21420, 0),
            ("CSEL x0,x1,x2,eq (control)", 0x9A820020, 0),
            ("MUL x0,x1,x2 (control)", 0x9B027C20, 0),
            // paso 3a: formas que mas caen al interprete en Subway Surfers
            ("FMUL v0.4s,v1.4s,v2.s[0] (por elemento)", 0x4F82_9020, 0),
            ("FMLA v0.4s,v1.4s,v2.s[0] (por elemento)", 0x4F82_1020, 0),
            ("FMUL d0,d10,d11 (resultado cero)", 0x1E6B_0940, 0),
            ("EXT v0.16b,v1.16b,v2.16b,#4", 0x6E02_2020, 0),
            ("REV64 v0.4s,v1.4s", 0x4EA0_0820, 0),
            ("LD1 {v0.16b,v1.16b},[x3]", 0x4C40_A060, 0),
            ("INS v0.s[1],w1", 0x4E0C_1C20, 0),
            ("FMIN d0,d1,d2 (fpcr=0)", 0x1E62_5820, 0),
            ("SMAXP v0.4s,v1.4s,v2.4s", 0x4EA2_A420, 0),
            ("FCCMP s1,s2,#0,eq", 0x1E22_0420, 0),
            ("LD1 {v0.s}[1],[x3]", 0x0D40_9060, 0),
            ("FMUL d0,d1,d2 (fpcr=0, ruta rapida)", 0x1E62_0820, 0),
            ("LD1 {v0.16b},[x3]", 0x4C40_7060, 0),
            ("ST1 {v0.16b},[x3]", 0x4C00_7060, 0),
            ("DUP v0.4s,v1.s[0]", 0x4E04_0420, 0),
            // paso 3b-1: por elemento y FMUL escalar con cero, antes (fpcr=FZ: helper) y despues (traducida)
            ("FMUL v0.4s,v1.4s,v2.s[0] (FZ, helper)", 0x4F82_9020, FZ),
            ("FMLA v0.4s,v1.4s,v2.s[0] (FZ, helper)", 0x4F82_1020, FZ),
            ("FMUL v0.2s,v1.2s,v2.s[1]", 0x0FA2_9020, 0),
            ("FMUL v0.2s,v1.2s,v2.s[1] (FZ, helper)", 0x0FA2_9020, FZ),
            ("FMUL v0.2d,v1.2d,v2.d[1]", 0x4FC2_9820, 0),
            ("FMUL v0.2d,v1.2d,v2.d[1] (FZ, helper)", 0x4FC2_9820, FZ),
            ("FMLS v0.4s,v1.4s,v2.s[0]", 0x4F82_5020, 0),
            ("FMLS v0.4s,v1.4s,v2.s[0] (FZ, helper)", 0x4F82_5020, FZ),
            ("FMULX v0.4s,v1.4s,v2.s[0]", 0x6F82_9020, 0),
            ("FMULX v0.4s,v1.4s,v2.s[0] (FZ, helper)", 0x6F82_9020, FZ),
            ("FMUL s0,s1,v2.s[0] (escalar)", 0x5F82_9020, 0),
            ("FMLA d0,d1,v2.d[1] (escalar)", 0x5FC2_1820, 0),
            ("FMUL d0,d10,d11 (cero, FZ, helper)", 0x1E6B_0940, FZ),
            ("FSUB d0,d1,d1 (cero)", 0x1E61_3820, 0),
            // paso 3b-2: LD1/ST1 multiples, permutaciones, INS/DUP, MOVI, pares, FMIN iguales, FCCMP
            ("LD1 {v0.16b,v1.16b},[x3]", 0x4C40_A060, 0),
            ("LD1 {v0.16b-v3.16b},[x3]", 0x4C40_2060, 0),
            ("LD1 {v0.8b},[x3]", 0x0C40_7060, 0),
            ("ST1 {v0.16b,v1.16b},[x3]", 0x4C00_A060, 0),
            ("ST1 {v0.16b-v3.16b},[x3]", 0x4C00_2060, 0),
            ("EXT v0.16b,v1.16b,v2.16b,#4", 0x6E02_2020, 0),
            ("REV64 v0.4s,v1.4s", 0x4EA0_0820, 0),
            ("ZIP1 v0.4s,v1.4s,v2.4s", 0x4E82_3820, 0),
            ("UZP1 v0.4s,v1.4s,v2.4s", 0x4E82_1820, 0),
            ("TRN1 v0.4s,v1.4s,v2.4s", 0x4E82_2820, 0),
            ("INS v0.s[1],w1", 0x4E0C_1C20, 0),
            ("INS v0.s[1],v1.s[2]", 0x6E0C_4420, 0),
            ("DUP v0.4s,w1", 0x4E04_0C20, 0),
            ("DUP v0.4s,v1.s[0]", 0x4E04_0420, 0),
            ("MOVI v0.4s,#0x12", 0x4F00_0640, 0),
            ("ADDP v0.4s,v1.4s,v2.4s", 0x4EA2_BC20, 0),
            ("SMAXP v0.4s,v1.4s,v2.4s", 0x4EA2_A420, 0),
            ("FMIN d0,d1,d2 (distintos)", 0x1E62_5820, 0),
            ("FMIN d0,d1,d1 (iguales)", 0x1E61_5820, 0),
            ("FCCMP s1,s2,#0,eq", 0x1E22_0420, 0),
            // paso 6: FMAXNM/FMINNM/FMAX/FMIN vectoriales, pares y across (FZ = interprete/helper)
            ("FMAXNM v0.4s,v1.4s,v2.4s", 0x4E22_C420, 0),
            ("FMAXNM v0.4s (FZ, helper)", 0x4E22_C420, FZ),
            ("FMINNM v0.4s,v1.4s,v2.4s", 0x4EA2_C420, 0),
            ("FMINNM v0.4s (FZ, helper)", 0x4EA2_C420, FZ),
            ("FMAXNM v0.2d,v1.2d,v2.2d", 0x4E62_C420, 0),
            ("FMAXNM v0.2s,v1.2s,v2.2s", 0x0E22_C420, 0),
            ("FMAX v0.4s,v1.4s,v2.4s", 0x4E22_F420, 0),
            ("FMIN v0.4s,v1.4s,v2.4s", 0x4EA2_F420, 0),
            ("FMAXNMP v0.4s,v1.4s,v2.4s", 0x6E22_C420, 0),
            ("FMAXP v0.4s,v1.4s,v2.4s", 0x6E22_F420, 0),
            ("FMAXNMV s0,v1.4s", 0x6E30_C820, 0),
            ("FMAXNMV s0,v1.4s (FZ, helper)", 0x6E30_C820, FZ),
            ("FMAXP d0,v1.2d (escalar)", 0x7E70_F820, 0),
        ];
        println!("BENCH {:<36} {:>11} {:>13} {:>10}", "instruccion", "directo ns", "bloque ns/ins", "h_exec/blq");
        for &(name, w, fpcr) in casos {
            let d = direct_ns(w, fpcr);
            let (t, h) = block_ns(w, K, fpcr);
            let per = (t - base) / K as f64;
            println!("BENCH {:<36} {:>11.2} {:>13.2} {:>10}", name, d, per, h);
        }
    }
}

/// Pruebas del encadenado de bloques. Cuentan bloques y sitios de un Jit y algunas publican invalidaciones globales
/// (EPOCH, IC_LOG) que alcanzan a los Jit de las demas: cada una toma `serie()` y corren de una en una.
#[cfg(test)]
mod chain_tests {
    use super::*;
    use std::sync::atomic::Ordering::Relaxed;

    #[repr(align(4096))]
    struct Page([u32; 1024]);

    fn serie() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        M.lock().unwrap_or_else(|e| e.into_inner())
    }

    const SVC: u32 = 0xD400_0001;
    const MOVZ_X0_1000: u32 = 0xD280_7D00;
    const ADD_X1_X1_3: u32 = 0x9100_0C21;
    const EOR_X2_X2_X1: u32 = 0xCA01_0042;
    const SUBS_X0_X0_1: u32 = 0xF100_0400;
    const BNE_BACK3: u32 = 0x54FF_FFA1; // b.ne -3 instrucciones

    fn page(words: &[(usize, u32)]) -> Box<Page> {
        let mut p = Box::new(Page([0; 1024]));
        for &(i, w) in words {
            p.0[i] = w;
        }
        p
    }
    fn addr(p: &Page, i: usize) -> u64 {
        p.0.as_ptr() as u64 + 4 * i as u64
    }
    fn loop_page() -> Box<Page> {
        // x0 = 1000; do { x1 += 3; x2 ^= x1; x0 -= 1 } while (x0 != 0); svc
        page(&[(0, MOVZ_X0_1000), (1, ADD_X1_X1_3), (2, EOR_X2_X2_X1), (3, SUBS_X0_X0_1), (4, BNE_BACK3), (5, SVC)])
    }

    #[test]
    fn bucle_encadenado_igual_que_run_n() {
        let _g = serie();
        let p = loop_page();
        let mut a = Cpu::new();
        a.pc = addr(&p, 0);
        let mut j = Jit::new();
        j.chain = true;
        assert!(matches!(j.run(&mut a), Event::Svc(0)));
        assert!(j.patched >= 1, "el bucle debio encadenarse");
        // 1 + 4*1000 instrucciones hasta el SVC (que run_n no ejecuta)
        let mut b = Cpu::new();
        b.pc = addr(&p, 0);
        let mut j2 = Jit::new();
        j2.run_n(&mut b, 1 + 4 * 1000).unwrap();
        assert_eq!(a.x, b.x);
        assert_eq!(a.pc, b.pc + 4); // el SVC deja pc en la instruccion siguiente; run_n se detiene antes de ejecutarlo
        assert_eq!(a.flags(), b.flags());
        assert_eq!(a.x[1], 3000);
        // mucho menos despachos que los ~1000 sin encadenar
        assert!(j.dispatched < 10, "despachos: {}", j.dispatched);
        // sin encadenar: mismo estado, ~1000 despachos
        let mut c = Cpu::new();
        c.pc = addr(&p, 0);
        let mut j3 = Jit::new();
        j3.chain = false;
        assert!(matches!(j3.run(&mut c), Event::Svc(0)));
        assert_eq!(a.x, c.x);
        assert_eq!(a.pc, c.pc);
        assert_eq!(a.flags(), c.flags());
        assert!(j3.dispatched >= 1000);
    }

    #[test]
    fn invalidar_destino_desparcha_y_retraduce() {
        let _g = serie();
        // i0: movz x0,#5 ; i1: b i16 ; i16: add x1,x1,#1 ; i17: svc   (D en otra linea de 64 bytes)
        let mut p = page(&[(0, 0xD280_00A0), (1, 0x1400_000F), (16, 0x9100_0421), (17, SVC)]);
        let mut cpu = Cpu::new();
        let mut j = Jit::new();
        j.chain = true;
        let run = |j: &mut Jit, cpu: &mut Cpu, p: &Page| {
            cpu.pc = addr(p, 0);
            let d0 = j.dispatched;
            assert!(matches!(j.run(cpu), Event::Svc(0)));
            j.dispatched - d0
        };
        assert_eq!(run(&mut j, &mut cpu, &p), 2); // A y luego D (compilado despues: A aun sin parchear)
        assert_eq!(cpu.x[1], 1);
        assert_eq!(run(&mut j, &mut cpu, &p), 1); // A -> D encadenado
        assert_eq!(cpu.x[1], 2);
        let c0 = j.compiled;
        // el guest reescribe D (add #2) e invalida su linea
        p.0[16] = 0x9100_0821;
        j.invalidate(addr(&p, 16));
        assert!(j.sites.iter().all(|s| !s.patched || s.dest != addr(&p, 16)), "salto hacia bloque invalidado sigue parcheado");
        assert_eq!(run(&mut j, &mut cpu, &p), 2); // vuelve al despachador y re-traduce D
        assert_eq!(j.compiled, c0 + 1);
        assert_eq!(cpu.x[1], 2 + 2);
        assert_eq!(run(&mut j, &mut cpu, &p), 1); // y se vuelve a encadenar
        assert_eq!(cpu.x[1], 4 + 2);
        // flush_all descarta todos los registros
        j.flush_all();
        assert!(j.sites.is_empty() && j.pending.is_empty());
        assert_eq!(run(&mut j, &mut cpu, &p), 2);
        assert_eq!(cpu.x[1], 6 + 2);
    }

    #[test]
    fn cambio_de_epoch_saca_de_un_bucle_encadenado() {
        let _g = serie();
        // i0: add x1,x1,#1 ; i1: ldr x2,[x3] ; i2: cbz x2,i0 ; i3: svc   (x3 apunta a una bandera)
        let p = page(&[(0, 0x9100_0421), (1, 0xF940_0062), (2, 0xB4FF_FFC2), (3, SVC)]);
        static FLAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        FLAG.store(0, Relaxed);
        let mut cpu = Cpu::new();
        cpu.pc = addr(&p, 0);
        cpu.x[3] = std::ptr::addr_of!(FLAG) as u64;
        let mut j = Jit::new();
        j.chain = true;
        let c0 = j.compiled;
        let line = addr(&p, 0);
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            ic_publish(line); // lo que hace un IC IVAU de otro hilo sobre la linea del bucle
            std::thread::sleep(std::time::Duration::from_millis(50));
            FLAG.store(1, Relaxed);
        });
        assert!(matches!(j.run(&mut cpu), Event::Svc(0)));
        t.join().unwrap();
        assert!(j.patched >= 1);
        // el bucle (encadenado, nunca vuelve solo al despachador) vio el EPOCH ~50 ms antes de terminar y volvio a
        // traducir sus bloques (3 la primera vez: i0, i1-i2 tras el salto... al menos uno mas tras la invalidacion)
        assert!(j.compiled > c0 + 2, "el cambio de EPOCH no saco al hilo del bucle encadenado ({} bloques)", j.compiled - c0);
        assert!(cpu.x[1] > 1000);
    }

    /// IC IVAU de otro hilo: solo se vuelven a traducir los bloques de esa linea; el resto sigue en la cache.
    #[test]
    fn ic_ivau_de_otro_hilo_invalida_solo_su_linea() {
        let _g = serie();
        // i0: movz x0,#5 ; i1: b i16 ; i16: add x1,x1,#1 ; i17: svc   (D en otra linea de 64 bytes)
        let mut p = page(&[(0, 0xD280_00A0), (1, 0x1400_000F), (16, 0x9100_0421), (17, SVC)]);
        let mut cpu = Cpu::new();
        let mut j = Jit::new();
        j.chain = true;
        let run = |j: &mut Jit, cpu: &mut Cpu, p: &Page| {
            cpu.pc = addr(p, 0);
            assert!(matches!(j.run(cpu), Event::Svc(0)));
        };
        run(&mut j, &mut cpu, &p);
        run(&mut j, &mut cpu, &p);
        assert_eq!(cpu.x[1], 2);
        let (c0, f0) = (j.compiled, j.flushes);
        // otro hilo (su propio Jit) reescribe D (add #2) y ejecuta `ic ivau, x3` sobre su linea
        p.0[16] = 0x9100_0821;
        let d = addr(&p, 16);
        std::thread::spawn(move || {
            let q = page(&[(0, 0xD50B_7523), (1, SVC)]); // ic ivau, x3 ; svc
            let mut c = Cpu::new();
            c.pc = addr(&q, 0);
            c.x[3] = d;
            let mut j2 = Jit::new();
            let e = j2.run(&mut c);
            assert!(matches!(e, Event::Svc(0)), "{:?}", e);
        })
        .join()
        .unwrap();
        run(&mut j, &mut cpu, &p);
        assert_eq!(cpu.x[1], 4, "se ejecuto la traduccion vieja de la linea invalidada");
        assert_eq!(j.compiled, c0 + 1, "solo D se vuelve a traducir");
        assert_eq!(j.flushes, f0, "un IC IVAU de otro hilo no vacia toda la cache");
        // varias lineas en una sola puesta al dia, una de ellas repetida y otra ajena
        p.0[16] = 0x9100_0C21;
        p.0[0] = 0xD280_00C0;
        for a in [addr(&p, 0), addr(&p, 16), addr(&p, 17), addr(&p, 512)] {
            std::thread::spawn(move || ic_publish(a)).join().unwrap();
        }
        run(&mut j, &mut cpu, &p);
        assert_eq!((cpu.x[0], cpu.x[1]), (6, 7));
        assert_eq!(j.compiled, c0 + 3);
        // IC IALLU: todo
        std::thread::spawn(|| ic_publish(0)).join().unwrap();
        run(&mut j, &mut cpu, &p);
        assert!(j.flushes > f0);
    }

    /// Un hilo que se quedo mas de IC_RING publicaciones atras vacia todo; una entrada reservada pero aun no escrita
    /// detiene la puesta al dia sin perder nada; un publicador cuya casilla ya es de una vuelta posterior la da por
    /// perdida (todos vacian) sin pisarla.
    #[test]
    fn anillo_de_invalidaciones_desbordado_o_incompleto() {
        let _g = serie();
        use std::sync::atomic::Ordering::{AcqRel, Release};
        let p = page(&[(0, 0x9100_0421), (1, SVC)]);
        let mut cpu = Cpu::new();
        let mut j = Jit::new();
        let run = |j: &mut Jit, cpu: &mut Cpu| {
            cpu.pc = addr(&p, 0);
            assert!(matches!(j.run(cpu), Event::Svc(0)));
        };
        run(&mut j, &mut cpu);
        // desbordado: lineas ajenas, pero mas de las que caben
        let f0 = j.flushes;
        for i in 0..IC_RING as u64 + 1 {
            ic_publish(0x7000_0000_0000 + 64 * i);
        }
        run(&mut j, &mut cpu);
        assert!(j.flushes > f0);
        run(&mut j, &mut cpu);
        // entrada reservada y aun sin escribir (publicador detenido): la puesta al dia se detiene en ella
        let (f1, c1) = (j.flushes, j.compiled);
        let s = IC_SEQ.fetch_add(1, AcqRel);
        EPOCH.fetch_add(1, Release); // otro publicador posterior ya termino
        run(&mut j, &mut cpu);
        assert!(j.ic_seq <= s);
        assert_eq!((j.flushes, j.compiled), (f1, c1), "nada que invalidar todavia");
        // el publicador termina: su linea (la del bloque) se invalida
        ic_write(s, addr(&p, 0));
        EPOCH.fetch_add(1, Release);
        run(&mut j, &mut cpu);
        assert!(j.ic_seq > s);
        assert_eq!((j.flushes, j.compiled), (f1, c1 + 1));
        // casilla ya ocupada por una vuelta posterior: no se pisa y se cuenta como perdida
        let l0 = IC_LOST.load(Relaxed);
        let viejo = s.wrapping_sub(IC_RING as u64); // misma casilla, vuelta anterior
        ic_write(viejo, 0x7100_0000_0000);
        assert!(IC_LOST.load(Relaxed) > l0);
        assert_eq!(IC_LOG[(s % IC_RING as u64) as usize].load(Relaxed) & 63, ic_tag(s));
        EPOCH.fetch_add(1, Release);
        run(&mut j, &mut cpu);
        assert!(j.flushes > f1);
        // etiquetas
        assert_eq!(ic_ahead(ic_tag(5), ic_tag(5)), 0);
        assert_eq!(ic_ahead(ic_tag(IC_RING as u64), 0), 1);
        assert!(ic_ahead(0, ic_tag(IC_RING as u64)) >= 32);
        assert_eq!(ic_ahead(0, 63), 1, "valor inicial de las casillas: vuelta anterior a la 0");
    }

    #[test]
    fn nunca_se_encadena_a_hle_ni_a_codigo_del_host() {
        let _g = serie();
        crate::hle::init();
        let slot = crate::hle::register("jit_chain_test_hle", Box::new(|_c| crate::hle::Ret::Return));
        assert!(crate::hle::is_hle(slot));
        let mut keep = Vec::new();
        let mut cx = mk_ctx(&mut keep);
        cx.exit_const(slot, 1, 0x1000);
        assert!(cx.sites.is_empty(), "salto a HLE encadenado");
        cx.exit_const(chain_tests_host_fn as usize as u64, 1, 0x1000);
        assert!(cx.sites.is_empty(), "salto a codigo del host encadenado");
        let mut cx = mk_ctx(&mut keep);
        cx.exit_const(0x4000_0000, 1, 0x1000);
        assert_eq!(cx.sites.len(), 1); // destino guest normal: si
        let mut cx = mk_ctx(&mut keep);
        cx.counted = true;
        cx.exit_const(0x4000_0000, 1, 0x1000);
        assert!(cx.sites.is_empty(), "el modo contado no encadena");
    }
    /// B.cond con ambos destinos encadenados (cada uno en otra linea de 64 bytes); se invalida solo uno: el otro
    /// sigue encadenado. Despues se invalida el bloque origen: sus saltos parcheados vuelven al epilogo.
    #[test]
    fn bcond_ambos_destinos_invalidar_uno() {
        let _g = serie();
        // i14: cmp x0,#0 ; i15: b.eq i32 ; i16: add x1,x1,#1 ; i17: svc ; i32: add x1,x1,#16 ; i33: svc
        let mut p = page(&[(14, 0xF100_001F), (15, 0x5400_0220), (16, 0x9100_0421), (17, SVC), (32, 0x9100_4021), (33, SVC)]);
        let mut cpu = Cpu::new();
        let mut j = Jit::new();
        j.chain = true;
        let run = |j: &mut Jit, cpu: &mut Cpu, p: &Page, x0: u64| {
            cpu.pc = addr(p, 14);
            cpu.x[0] = x0;
            let d0 = j.dispatched;
            assert!(matches!(j.run(cpu), Event::Svc(0)));
            j.dispatched - d0
        };
        assert_eq!(run(&mut j, &mut cpu, &p, 1), 2); // no tomado: compila i16 (parchea el sitio pendiente)
        assert_eq!(run(&mut j, &mut cpu, &p, 0), 2); // tomado: compila i32
        assert_eq!(run(&mut j, &mut cpu, &p, 1), 1);
        assert_eq!(run(&mut j, &mut cpu, &p, 0), 1);
        assert_eq!(cpu.x[1], 1 + 16 + 1 + 16);
        let a = addr(&p, 14);
        assert_eq!(j.sites.iter().filter(|s| s.src == a && s.patched).count(), 2);
        // reescribir i16 (add #2) e invalidar solo su linea
        p.0[16] = 0x9100_0821;
        j.invalidate(addr(&p, 16));
        assert!(j.sites.iter().any(|s| s.src == a && s.dest == addr(&p, 32) && s.patched), "el otro destino debe seguir encadenado");
        assert!(j.sites.iter().all(|s| !(s.dest == addr(&p, 16) && s.patched)));
        assert_eq!(run(&mut j, &mut cpu, &p, 0), 1); // i32 sigue encadenado
        assert_eq!(run(&mut j, &mut cpu, &p, 1), 2); // i16 re-traducido
        assert_eq!(run(&mut j, &mut cpu, &p, 1), 1); // y re-encadenado
        assert_eq!(cpu.x[1], 34 + 16 + 2 + 2);
        // invalidar el bloque origen: sus saltos parcheados se desparchan aunque el bloque ya no este registrado
        let offs: Vec<usize> = j.sites.iter().filter(|s| s.src == a && s.patched).map(|s| s.off).collect();
        assert_eq!(offs.len(), 2);
        j.invalidate(a);
        assert!(j.sites.iter().all(|s| s.src != a));
        for off in offs {
            assert_eq!(unsafe { std::ptr::read_unaligned(j.buf.add(off) as *const i32) }, 0, "salto de bloque muerto sigue parcheado");
        }
        assert_eq!(run(&mut j, &mut cpu, &p, 1), 1); // i14 re-traducido y encadenado a i16 ya existente
        assert_eq!(cpu.x[1], 54 + 2);
    }

    /// Bucle cuyo cuerpo cruza un limite de pagina (el primer bloque termina por fin de pagina, sin salto).
    #[test]
    fn cadena_que_cruza_pagina() {
        let _g = serie();
        #[repr(align(4096))]
        struct Pages([u32; 2048]);
        let mut p = Box::new(Pages([0; 2048]));
        // i1021: add x1,x1,#3 ; i1022: subs x0,x0,#1 ; i1023: nop | pagina | i1024: b.ne i1021 ; i1025: svc
        p.0[1021] = ADD_X1_X1_3;
        p.0[1022] = SUBS_X0_X0_1;
        p.0[1023] = 0xD503_201F;
        p.0[1024] = BNE_BACK3;
        p.0[1025] = SVC;
        let start = p.0.as_ptr() as u64 + 4 * 1021;
        let mk = || {
            let mut c = Cpu::new();
            c.pc = start;
            c.x[0] = 500;
            c
        };
        let mut a = mk();
        let mut j = Jit::new();
        j.chain = true;
        assert!(matches!(j.run(&mut a), Event::Svc(0)));
        assert!(j.patched >= 2, "parcheados: {}", j.patched);
        assert!(j.dispatched < 10, "despachos: {}", j.dispatched);
        let mut b = mk();
        let mut j2 = Jit::new();
        j2.run_n(&mut b, 4 * 500).unwrap();
        assert_eq!(a.x, b.x);
        assert_eq!(a.pc, b.pc + 4);
        assert_eq!(a.flags(), b.flags());
        let mut c = mk();
        let mut j3 = Jit::new();
        j3.chain = false;
        assert!(matches!(j3.run(&mut c), Event::Svc(0)));
        assert_eq!((a.x, a.pc, a.flags()), (c.x, c.pc, c.flags()));
        assert_eq!(a.x[1], 1500);
    }

    /// Al llegar a SITES_CAP los saltos nuevos quedan sin encadenar (vuelven al despachador) y todo sigue correcto.
    #[test]
    fn tope_de_sitios_degrada_sin_encadenar() {
        let _g = serie();
        let extra = 100usize;
        let n = SITES_CAP + extra;
        // n instrucciones `b .+4` (un bloque y un sitio cada una) y un svc
        let mut code = vec![0x1400_0001u32; n + 1];
        code[n] = SVC;
        let start = code.as_ptr() as u64;
        let mut j = Jit::new();
        j.chain = true;
        let mut cpu = Cpu::new();
        cpu.pc = start;
        assert!(matches!(j.run(&mut cpu), Event::Svc(0)));
        assert_eq!(cpu.pc, start + 4 * (n as u64 + 1));
        assert_eq!(j.sites.len(), SITES_CAP);
        assert!(j.pending.values().map(|v| v.len()).sum::<usize>() <= SITES_CAP);
        let d0 = j.dispatched;
        cpu.pc = start;
        assert!(matches!(j.run(&mut cpu), Event::Svc(0)));
        assert_eq!(cpu.pc, start + 4 * (n as u64 + 1));
        // la parte con sitio va encadenada de una vez; los bloques sin sitio pasan cada uno por el despachador
        let d = j.dispatched - d0;
        assert!(d >= extra as u64 && d <= extra as u64 + 3, "despachos: {}", d);
    }

    extern "C" fn chain_tests_host_fn() {}
    fn mk_ctx(keep: &mut Vec<Box<OpR>>) -> Ctx<'_> {
        Ctx {
            a: Asm::new(),
            counted: false,
            known: 0,
            ops_keep: keep,
            chain: true,
            sites: Vec::new(),
            slot: [NO_SLOT; 32],
            valid: 0,
            fill: true,
            helper: false,
            xf_next: 0,
            xf: 0,
            fused: Vec::new(),
        }
    }

    /// ns por iteracion (4 instrucciones) del bucle de 1000 iteraciones, sin y con encadenado:
    /// `cargo test --release --lib jit::chain_tests::bench_encadenado -- --ignored --nocapture --test-threads=1`
    #[test]
    #[ignore]
    fn bench_encadenado() {
        let p = loop_page();
        for chain in [false, true, false, true] {
            let mut j = Jit::new();
            j.chain = chain;
            let mut cpu = Cpu::new();
            for _ in 0..200 {
                cpu.pc = addr(&p, 0);
                j.run(&mut cpu);
            }
            let reps = 20_000u64;
            let t0 = std::time::Instant::now();
            for _ in 0..reps {
                cpu.pc = addr(&p, 0);
                j.run(&mut cpu);
            }
            let ns = t0.elapsed().as_nanos() as f64 / (reps * 1000) as f64;
            println!("BENCH encadenado={} : {:.2} ns por iteracion (4 instrucciones)", chain, ns);
        }
    }
}

/// Pruebas de la cache de registros (paso 5): diferencial, fallo sincrono y microbenchmark.
#[cfg(test)]
#[path = "jit_regs_tests.rs"]
mod regs_tests;

#[cfg(test)]
mod read_code_tests {
    use super::*;

    /// El traductor lee el codigo guest con una lectura protegida. Una pagina sin lectura (desmapeada despues de
    /// comprobarla, o PROT_NONE) da None y `compile` devuelve el bloque de fallo de instruccion, sin que falle el
    /// traductor; una legible se lee y se traduce.
    #[test]
    fn traductor_con_lectura_protegida() {
        use crate::sys::*;
        use std::os::raw::c_void;
        crate::sig::install_fault_handlers();
        let p = unsafe { mmap(std::ptr::null_mut(), 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) } as u64;
        assert_ne!(p as isize, -1);
        unsafe {
            *(p as *mut u32) = 0xd280_0540; // mov x0, #42
            *((p + 4) as *mut u32) = 0xd65f_03c0; // ret
            mprotect((p + 4096) as *mut c_void, 4096, PROT_NONE);
        }
        assert_eq!(crate::monitor::read_code(p), Some(0xd280_0540));
        assert_eq!(crate::monitor::read_code(p + 4096), None, "lectura de una pagina PROT_NONE: None, no SIGSEGV");
        let mut j = Jit::new();
        let (c, _) = j.compile(p + 4096, MAX_BLOCK, false);
        assert_eq!(c, 0, "primera instruccion ilegible: sin bloque");
        let (c, end) = j.compile(p, MAX_BLOCK, false);
        assert_ne!(c, 0);
        assert_eq!(end, p + 8);
        unsafe { munmap(p as *mut c_void, 8192) };
    }
}

#[cfg(test)]
mod cap_tests {
    use super::*;

    /// Un Jit pequeno (estados de emergencia de las senales, 256 KiB) se vacia al llenarse en lugar de escribir fuera
    /// de su bufer: los limites miran su capacidad, no CODE_CAP.
    #[test]
    fn jit_pequeno_se_vacia_al_llenarse() {
        use crate::sys::*;
        let len = 1usize << 20;
        let p = unsafe { mmap(std::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) } as u64;
        // bloques de 8 instrucciones que terminan en ret: muchos bloques distintos
        for i in 0..(len / 4) as u64 {
            let w: u32 = if i % 8 == 7 { 0xd65f_03c0 } else { 0x9100_0400 }; // ret / add x0, x0, #1
            unsafe { *((p + 4 * i) as *mut u32) = w };
        }
        let mut j = Jit::with_capacity(256 << 10);
        for b in 0..(len / 32) as u64 {
            j.lookup_miss(p + 32 * b, 0);
            assert!(j.used < j.cap);
        }
        assert!(j.flushes > 0, "con 256 KiB tiene que haberse vaciado");
        unsafe { munmap(p as *mut std::os::raw::c_void, len) };
    }
}
