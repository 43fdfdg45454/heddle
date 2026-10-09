//! heddle-difftest: compara el interprete (o el JIT) contra los resultados de Unicorn guardados por gen_cases.py.
//! Ver `USO`. Codigos de salida: 0 todos los casos coinciden, 1 hay diferencias o error (archivo ilegible o truncado,
//! memoria de prueba no disponible), 2 uso incorrecto.
//!
//! Fallos (SIGSEGV en las guardas, SIGBUS por alineacion): con `--engine jit` el caso corre en el hilo guest del
//! proceso (`rt::ensure_thread`, su Cpu y su Jit) con un manejador guest de SIGSEGV/SIGBUS instalado con
//! `sig::guest_sigaction`: el fallo recorre el camino real (`sig::sync_fault`, `build_frame`) y el manejador (codigo
//! ARM64, `HANDLER`) copia su `siginfo` y su `ucontext` en `DUMP` y reanuda en `END`. Se compara lo que veria una app
//! en su manejador: senal, `si_code`, `si_addr` (y `fault_address`), pc, registros, NZCV, FPSR, vectores y la memoria.
//! Con `--engine interp` el interprete corre dentro de `heddle_call_block` y un manejador del host copia la Cpu
//! (cuyo pc es el de la instruccion en curso) y reanuda con `heddle_block_abort`.
use heddle::cpu::*;
use heddle::decode::{decode, Op};
use heddle::interp::{self, Flow};
use std::io::Read;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn sigaction(sig: i32, act: *const SigAct, old: *mut SigAct) -> i32;
}

#[repr(C)]
struct SigAct {
    handler: usize,
    mask: [u64; 16],
    flags: i32,
    restorer: usize,
}

const CODE_BASE: u64 = 0x2000_0000;
const CODE_SIZE: usize = 0x20000;
const CODE: u64 = CODE_BASE + 0x10000;
const MEM: u64 = 0x1000_0000;
const MEMSZ: usize = 0x20000;
/// Guardas sin permisos a ambos lados de MEM (las de gen_cases.py)
const GUARD: usize = 0x10000;
/// Pagina del manejador guest: codigo en HANDLER, volcado en DUMP, NOPs desde END y pila alternativa en ALT.
const HPAGE: u64 = 0x2200_0000;
const HSIZE: usize = 0x20000;
const HANDLER: u64 = HPAGE;
const DUMP: u64 = HPAGE + 0x1000;
const END: u64 = HPAGE + 0x4000;
const ALT: u64 = HPAGE + 0x10000;
const ALT_SIZE: u64 = 0x10000;
const NOP: u32 = 0xD503_201F;
const MAGIC: &[u8; 4] = b"HDT2";

/// Manejador guest (x0 = senal, x1 = siginfo, x2 = ucontext): copia siginfo (128 bytes) en DUMP y el ucontext hasta
/// el final del fpsimd_context (1008 bytes) en DUMP + 128, pone el pc del ucontext en END y vuelve.
const HANDLER_CODE: [u32; 19] = [
    0xd2820003, // movz x3, #0x1000
    0xf2a44003, // movk x3, #0x2200, lsl #16     (DUMP)
    0xd2800004, // mov  x4, #0
    0xf8646825, // 1: ldr x5, [x1, x4]
    0xf8246865, //    str x5, [x3, x4]
    0x91002084, //    add x4, x4, #8
    0xf102009f, //    cmp x4, #128
    0x54ffff81, //    b.ne 1b
    0x91020063, // add  x3, x3, #128
    0xd2800004, // mov  x4, #0
    0xf8646845, // 2: ldr x5, [x2, x4]
    0xf8246865, //    str x5, [x3, x4]
    0x91002084, //    add x4, x4, #8
    0xf10fc09f, //    cmp x4, #1008
    0x54ffff81, //    b.ne 2b
    0xd2880006, // movz x6, #0x4000
    0xf2a44006, // movk x6, #0x2200, lsl #16     (END)
    0xf900dc46, // str  x6, [x2, #440]           (uc_mcontext.pc)
    0xd65f03c0, // ret
];
// desplazamientos en DUMP: siginfo y despues el ucontext aarch64 (mcontext en +176, fpsimd_context en +464)
const D_UC: u64 = 128;
const D_MC: u64 = D_UC + 176;
const D_FP: u64 = D_UC + 464;

fn splitmix(x: u64) -> u64 {
    let x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const USO: &str = "\
uso: heddle-difftest [opciones] CASOS.bin

Ejecuta los casos de CASOS.bin (generados por difftests/tools/gen_cases.py con Unicorn como referencia) y compara
registros, flags, FPSR, TPIDR_EL0 y memoria; en los casos que fallan (SIGSEGV en una guarda, SIGBUS por alineacion),
la senal, si_code, la direccion y el estado que ve el manejador.

opciones:
  --engine interp|jit   motor que se prueba (por defecto interp)
  --show N              cuantos fallos se detallan (por defecto 15)
  -h, --help            muestra esta ayuda
  -V, --version         muestra la version

salida: 0 todos los casos coinciden; 1 hay diferencias o error (archivo ilegible o truncado); 2 uso incorrecto.
";

const EXIT_USO: i32 = 2;
const EXIT_ERROR: i32 = 1;
/// Instrucciones por caso como mucho (el codigo de prueba ocupa 64 KiB tras CODE; tras END hay sitio para tantas).
const MAX_WORDS: usize = 4096;

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Version,
    Run { path: String, jit: bool, show: usize },
}

fn parse(a: &[String]) -> Result<Cmd, String> {
    let (mut path, mut jit, mut show) = (None, false, 15usize);
    let mut it = a.iter();
    while let Some(s) = it.next() {
        match s.as_str() {
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "--engine" => {
                jit = match it.next().map(|s| s.as_str()) {
                    Some("interp") => false,
                    Some("jit") => true,
                    v => return Err(format!("--engine espera interp o jit (recibido {:?})", v.unwrap_or(""))),
                }
            }
            "--show" => {
                let v = it.next().ok_or("--show espera un numero")?;
                show = v.parse().map_err(|_| format!("--show espera un numero (recibido {})", v))?;
            }
            o if o.starts_with('-') => return Err(format!("opcion desconocida: {}", o)),
            _ if path.is_some() => return Err(format!("sobra el argumento {}", s)),
            _ => path = Some(s.clone()),
        }
    }
    let path = path.ok_or("falta el archivo de casos")?;
    Ok(Cmd::Run { path, jit, show })
}

fn fatal(msg: &str) -> ! {
    eprintln!("heddle-difftest: {}", msg);
    std::process::exit(EXIT_ERROR)
}

/// Lector de los casos. Un archivo truncado no aborta: `short` queda marcado y se comprueba tras cada caso.
struct Rd<'a> {
    b: &'a [u8],
    p: usize,
    short: bool,
}
impl<'a> Rd<'a> {
    fn bytes<const N: usize>(&mut self) -> [u8; N] {
        match self.b.get(self.p..self.p + N) {
            Some(s) => {
                self.p += N;
                s.try_into().unwrap()
            }
            None => {
                self.short = true;
                self.p = self.b.len();
                [0; N]
            }
        }
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.bytes())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.bytes())
    }
    fn eof(&self) -> bool {
        self.p >= self.b.len()
    }
}

fn fill_mem() {
    for i in 0..MEMSZ / 8 {
        let a = MEM + (i as u64) * 8;
        unsafe { *(a as *mut u64) = splitmix(a >> 3) };
    }
}

/// Estado de la CPU que se compara (al final del caso o en el manejador de un fallo).
#[derive(Clone, PartialEq, Default)]
struct State {
    x: [u64; 31],
    sp: u64,
    nzcv: u64,
    pc: u64,
    fpsr: u64,
    tpidr: u64,
    v: [[u64; 2]; 32],
}

/// Senal sincrona que recibio el guest: (signo, si_code, si_addr) y el estado en su manejador.
#[derive(Clone, PartialEq)]
struct Fault {
    signo: u32,
    code: u32,
    addr: u64,
    st: State,
}

fn state_of(c: &Cpu) -> State {
    let mut x = [0; 31];
    x.copy_from_slice(&c.x[..31]);
    State { x, sp: c.x[31], nzcv: c.flags(), pc: c.pc, fpsr: c.fpsr, tpidr: c.tpidr, v: c.v }
}

fn read_state(rd: &mut Rd) -> State {
    let mut s = State::default();
    for r in s.x.iter_mut() {
        *r = rd.u64();
    }
    s.sp = rd.u64();
    s.nzcv = rd.u64();
    s.pc = rd.u64();
    s.fpsr = rd.u64();
    s.tpidr = rd.u64();
    for r in s.v.iter_mut() {
        r[0] = rd.u64();
        r[1] = rd.u64();
    }
    s
}

/// Primera diferencia entre lo esperado y lo obtenido (None si son iguales).
fn diff_state(got: &State, want: &State, fpcr: u64) -> Option<String> {
    for i in 0..31 {
        if got.x[i] != want.x[i] {
            return Some(format!("x{} = {:#x}, esperado {:#x}", i, got.x[i], want.x[i]));
        }
    }
    if got.sp != want.sp {
        return Some(format!("sp = {:#x}, esperado {:#x}", got.sp, want.sp));
    }
    if got.nzcv != want.nzcv {
        return Some(format!("nzcv = {:#x}, esperado {:#x}", got.nzcv, want.nzcv));
    }
    if got.fpsr != want.fpsr {
        return Some(format!("fpsr = {:#x}, esperado {:#x} (fpcr {:#x})", got.fpsr, want.fpsr, fpcr));
    }
    for i in 0..32 {
        if got.v[i] != want.v[i] {
            return Some(format!(
                "v{} = {:016x}_{:016x}, esperado {:016x}_{:016x} (fpcr {:#x})",
                i, got.v[i][1], got.v[i][0], want.v[i][1], want.v[i][0], fpcr
            ));
        }
    }
    if got.pc != want.pc {
        return Some(format!("pc = {:#x}, esperado {:#x}", got.pc, want.pc));
    }
    if got.tpidr != want.tpidr {
        return Some(format!("tpidr_el0 = {:#x}, esperado {:#x}", got.tpidr, want.tpidr));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Fallos con el JIT: manejador guest real
// ---------------------------------------------------------------------------------------------

fn d64(off: u64) -> u64 {
    unsafe { std::ptr::read_volatile((DUMP + off) as *const u64) }
}

/// Lee el volcado del manejador guest (None si no hubo senal). `tpidr`: el de la Cpu (no esta en el ucontext).
fn fault_from_dump(tpidr: u64) -> Option<Fault> {
    let signo = d64(0) as u32;
    if signo == 0 {
        return None;
    }
    let code = (d64(8) & 0xffff_ffff) as u32;
    let addr = d64(16);
    let mut st = State { tpidr, ..Default::default() };
    for i in 0..31 {
        st.x[i] = d64(D_MC + 8 + 8 * i as u64);
    }
    st.sp = d64(D_MC + 256);
    st.pc = d64(D_MC + 264);
    st.nzcv = d64(D_MC + 272) & 0xF000_0000;
    st.fpsr = d64(D_FP + 8) & 0xffff_ffff;
    for i in 0..32 {
        st.v[i] = [d64(D_FP + 16 + 16 * i as u64), d64(D_FP + 24 + 16 * i as u64)];
    }
    if d64(D_MC) != addr {
        // fault_address del ucontext y si_addr deben coincidir (Linux pone los dos)
        // (la diferencia se senala con un si_addr imposible)
        return Some(Fault { signo, code, addr: u64::MAX, st });
    }
    Some(Fault { signo, code, addr, st })
}

/// Prepara el hilo guest: manejador de SIGSEGV y SIGBUS en HANDLER (SA_SIGINFO | SA_ONSTACK, pila alternativa del
/// guest en ALT, para que el marco no ensucie MEM).
fn setup_guest_handler() -> *mut heddle::rt::GuestThread {
    let t = heddle::rt::ensure_thread(256 << 10);
    unsafe {
        let w = HANDLER as *mut u32;
        for (i, x) in HANDLER_CODE.iter().enumerate() {
            *w.add(i) = *x;
        }
        let e = END as *mut u32;
        for i in 0..MAX_WORDS + 16 {
            *e.add(i) = NOP;
        }
    }
    let Some(ss) = heddle::libc_hle::resolve("sigaltstack") else { fatal("sin sigaltstack HLE") };
    let stk: [u64; 3] = [ALT, 0, ALT_SIZE];
    let mut c = Cpu::new();
    c.x[0] = stk.as_ptr() as u64;
    c.x[1] = 0;
    c.pc = ss;
    heddle::hle::dispatch(&mut c);
    if c.x[0] != 0 {
        fatal("sigaltstack del guest fallo");
    }
    for sig in [11, 7] {
        let act = heddle::sig::GuestAct { handler: HANDLER, flags: heddle::sig::SA_SIGINFO | 0x0800_0000, mask: 0 };
        if heddle::sig::guest_sigaction(sig, Some(act)).is_err() {
            fatal("no se pudo instalar el manejador guest");
        }
    }
    t
}

// ---------------------------------------------------------------------------------------------
// Fallos con el interprete: manejador del host
// ---------------------------------------------------------------------------------------------

static mut ICPU: *mut Cpu = std::ptr::null_mut();
static mut IFAULT: Option<Fault> = None;
static mut ISTEPS: usize = 0;
static mut IFLOW: Option<String> = None;

extern "C" fn interp_fault(sig: i32, info: *mut u8, uc: *mut std::ffi::c_void) {
    unsafe {
        // acceso del monitor con un bloqueo tomado: sigue en su continuacion (como sig::sync_fault)
        if heddle::monitor::fault_in_locked(uc) {
            return;
        }
        heddle::monitor::fault_in_store(uc);
        let c = &mut *ICPU;
        if c.host_sp == 0 {
            eprintln!("heddle-difftest: senal {} fuera del interprete", sig);
            std::process::abort();
        }
        let code = *(info.add(8) as *const i32) as u32;
        let addr = *(info.add(16) as *const u64);
        *std::ptr::addr_of_mut!(IFAULT) = Some(Fault { signo: sig as u32, code, addr, st: state_of(c) });
        // ucontext de x86-64: gregs en +40 (RBX = 11, RSP = 15, RIP = 16)
        let g = (uc as *mut u8).add(40) as *mut u64;
        *g.add(16) = heddle::jit::heddle_block_abort as *const () as usize as u64;
        *g.add(15) = c.host_sp;
        *g.add(11) = c as *mut Cpu as u64;
    }
}

extern "C" fn interp_entry(cp: *mut Cpu) -> u64 {
    let c = unsafe { &mut *cp };
    for _ in 0..unsafe { ISTEPS } {
        match interp::step(c) {
            Flow::Next | Flow::ICacheFlush(_) => {}
            f => {
                unsafe { *std::ptr::addr_of_mut!(IFLOW) = Some(format!("flujo inesperado {:?}", f)) };
                break;
            }
        }
    }
    1
}

fn setup_interp_handler() {
    for sig in [11, 7] {
        // SA_SIGINFO | SA_NODEFER: el manejador no vuelve por sigreturn (reanuda en heddle_block_abort)
        let act = SigAct { handler: interp_fault as *const () as usize, mask: [0; 16], flags: 4 | 0x4000_0000, restorer: 0 };
        if unsafe { sigaction(sig, &act, std::ptr::null_mut()) } != 0 {
            fatal("sigaction del host fallo");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, engine, show) = match parse(&args) {
        Ok(Cmd::Help) => {
            print!("{}", USO);
            return;
        }
        Ok(Cmd::Version) => {
            println!("{}", heddle::bridge::version_line("heddle-difftest"));
            return;
        }
        Ok(Cmd::Run { path, jit, show }) => (path, if jit { "jit" } else { "interp" }, show),
        Err(e) => {
            eprintln!("heddle-difftest: {}\n\n{}", e, USO);
            std::process::exit(EXIT_USO);
        }
    };
    let mut buf = Vec::new();
    if let Err(e) = std::fs::File::open(&path).and_then(|mut f| f.read_to_end(&mut buf)) {
        fatal(&format!("{}: {}", path, e));
    }
    if buf.get(..4) != Some(&MAGIC[..]) {
        fatal(&format!("{}: no es un archivo de casos HDT2 (regeneralo con gen_cases.py)", path));
    }
    // oraculo: Unicorn con `-cpu max`; heddle con todo lo que implementa salvo que se pida otro modelo (HEDDLE_CPU)
    heddle::feat::set_default("max");
    heddle::monitor::init();
    unsafe {
        // MAP_PRIVATE|MAP_ANONYMOUS|MAP_FIXED_NOREPLACE
        let fixed = |a: u64, len: usize, prot: i32| mmap(a as *mut u8, len, prot, 0x22 | 0x100000, -1, 0) as u64 == a;
        if !fixed(MEM, MEMSZ, 3) || !fixed(MEM - GUARD as u64, GUARD, 0) || !fixed(MEM + MEMSZ as u64, GUARD, 0) {
            fatal("no se pudo mapear la memoria de prueba");
        }
        // codigo guest: solo se lee y se traduce (no hace falta que sea ejecutable para el host)
        if !fixed(CODE_BASE, CODE_SIZE, 3) || !fixed(HPAGE, HSIZE, 3) {
            fatal("no se pudo mapear el codigo de prueba");
        }
        let w = CODE_BASE as *mut u32;
        for i in 0..CODE_SIZE / 4 {
            *w.add(i) = NOP;
        }
    }
    fill_mem();
    let mut rd = Rd { b: &buf, p: 4, short: false };
    let jit_mode = engine == "jit";
    let t = if jit_mode { setup_guest_handler() } else { std::ptr::null_mut() };
    let mut own = Cpu::new();
    if !jit_mode {
        setup_interp_handler();
    }
    let (mut total, mut ok, mut bad, mut undef_total, mut undef_bad, mut faults) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut shown = 0;
    let mut cats: std::collections::BTreeMap<String, u64> = Default::default();
    while !rd.eof() {
        let n = rd.u32() as usize;
        if n == 0 || n > MAX_WORDS {
            fatal(&format!("{}: caso con {} instrucciones (archivo danado)", path, n));
        }
        let words: Vec<u32> = (0..n).map(|_| rd.u32()).collect();
        let mut x = [0u64; 31];
        for r in x.iter_mut() {
            *r = rd.u64();
        }
        let sp = rd.u64();
        let nzcv = rd.u64();
        let mut q = [[0u64; 2]; 32];
        for r in q.iter_mut() {
            r[0] = rd.u64();
            r[1] = rd.u64();
        }
        let fpcr = rd.u64();
        let tpidr = rd.u64();
        let status = rd.u32();
        let mut want_sig = None;
        if status == 2 {
            let (s, c, a) = (rd.u32(), rd.u32(), rd.u64());
            want_sig = Some((s, c, a));
        }
        let mut want = State::default();
        let mut diffs: Vec<(u64, u64)> = vec![];
        if status != 1 {
            want = read_state(&mut rd);
            let nd = rd.u32();
            for _ in 0..nd {
                let a = rd.u64();
                let v = rd.u64();
                diffs.push((a, v));
            }
        }
        if rd.short {
            fatal(&format!("{}: archivo de casos truncado", path));
        }
        total += 1;
        if status == 1 && n > 1 {
            continue; // (gen_cases.py solo marca indefinidas las instrucciones sueltas)
        }
        if n > 1 && words.iter().any(|w| matches!(decode(*w), Op::Undef(_))) {
            continue; // secuencia con palabra indefinida: la referencia (Unicorn) no es fiable aqui
        }
        if want_sig.is_some() {
            faults += 1;
        }
        // preparar
        for (i, w) in words.iter().enumerate() {
            unsafe { *((CODE + 4 * i as u64) as *mut u32) = *w };
        }
        fill_mem();
        let cpu: &mut Cpu = if jit_mode { unsafe { &mut *(*t).cpu } } else { &mut own };
        cpu.x[..31].copy_from_slice(&x);
        cpu.x[31] = sp;
        cpu.set_flags(nzcv);
        cpu.pc = CODE;
        cpu.mon = heddle::monitor::Mon::new();
        cpu.tpidr = tpidr;
        cpu.fpcr = fpcr;
        cpu.fpsr = 0;
        cpu.v = q;
        let mut fail: Option<String> = None;
        let mut undef = false;
        let got_sig: Option<Fault>;
        if jit_mode {
            unsafe { std::ptr::write_volatile(DUMP as *mut u64, 0) };
            let j = unsafe { &mut (*t).jit };
            match j.run_n(cpu, n) {
                Ok(()) => {}
                // st 3: el bloque salio por una instruccion indefinida (Event::Undef en Jit::run)
                Err(e) if e.starts_with("flujo inesperado 3 ") => undef = true,
                Err(e) => fail = Some(format!("JIT: {}", e)),
            }
            got_sig = fault_from_dump(cpu.tpidr);
        } else {
            unsafe {
                ICPU = cpu as *mut Cpu;
                ISTEPS = n;
                *std::ptr::addr_of_mut!(IFAULT) = None;
                *std::ptr::addr_of_mut!(IFLOW) = None;
                heddle::jit::heddle_call_block(cpu, interp_entry);
                if let Some(f) = (*std::ptr::addr_of!(IFLOW)).clone() {
                    if f.starts_with("flujo inesperado Undef") {
                        undef = true;
                    } else {
                        fail = Some(f);
                    }
                }
                got_sig = (*std::ptr::addr_of!(IFAULT)).clone();
            }
        }
        if status == 1 {
            // Unicorn: indefinida. heddle debe dar SIGILL (Undef) sin ejecutar nada
            undef_total += 1;
            if !undef && fail.is_none() {
                undef_bad += 1;
                if shown < show {
                    shown += 1;
                    println!("EJECUTA UNA INSTRUCCION QUE UNICORN RECHAZA: {:08x} -> {:?}", words[0], decode(words[0]));
                }
            }
            continue;
        }
        if undef && fail.is_none() {
            fail = Some("heddle: instruccion indefinida (SIGILL); Unicorn la ejecuta".into());
        }
        if fail.is_none() {
            let got = match &got_sig {
                Some(f) => f.st.clone(),
                None => state_of(cpu),
            };
            match (&got_sig, want_sig) {
                (None, None) => {}
                (Some(g), None) => {
                    fail = Some(format!("senal {} (si_code {}, si_addr {:#x}) en pc {:#x}; Unicorn no falla", g.signo, g.code, g.addr, g.st.pc))
                }
                (None, Some((s, c, a))) => fail = Some(format!("sin senal; Unicorn: senal {} (si_code {}, si_addr {:#x}) en pc {:#x}", s, c, a, want.pc)),
                (Some(g), Some((s, c, a))) => {
                    if (g.signo, g.code, g.addr) != (s, c, a) {
                        fail = Some(format!("senal {} si_code {} si_addr {:#x}; esperado {} {} {:#x}", g.signo, g.code, g.addr, s, c, a));
                    }
                }
            }
            if fail.is_none() {
                fail = diff_state(&got, &want, fpcr).map(|m| if got_sig.is_some() { format!("en el manejador: {}", m) } else { m });
            }
            if fail.is_none() {
                // memoria: comparar contra lo esperado
                let mut got: Vec<(u64, u64)> = vec![];
                for i in 0..MEMSZ / 8 {
                    let a = MEM + (i as u64) * 8;
                    let v = unsafe { *(a as *const u64) };
                    if v != splitmix(a >> 3) {
                        got.push((a, v));
                    }
                }
                if got != diffs {
                    fail = Some(format!("memoria distinta: obtenido {:x?}, esperado {:x?}", &got[..got.len().min(4)], &diffs[..diffs.len().min(4)]));
                }
            }
        }
        match fail {
            None => ok += 1,
            Some(msg) => {
                bad += 1;
                {
                    let name = format!("{:?}", decode(words[words.len() - 1]));
                    let k = name.split(|c: char| c == ' ' || c == '(').next().unwrap().to_string();
                    *cats.entry(k).or_default() += 1;
                }
                if shown < show {
                    shown += 1;
                    let ws: Vec<String> = words.iter().map(|w| format!("{:08x}", w)).collect();
                    println!("FALLA [{}] {:?}: {}", ws.join(" "), words.iter().map(|w| decode(*w)).collect::<Vec<_>>(), msg);
                    if let Op::Fp(f) = decode(words[0]) {
                        let w0 = f.0;
                        let (rn, rm, ra) = (((w0 >> 5) & 31) as usize, ((w0 >> 16) & 31) as usize, ((w0 >> 10) & 31) as usize);
                        println!("    vn={:016x}_{:016x} vm={:016x}_{:016x} va={:016x}_{:016x} xn={:#x}", q[rn][1], q[rn][0], q[rm][1], q[rm][0], q[ra][1], q[ra][0], x[rn.min(30)]);
                    }
                    let rn = ((words[0] >> 5) & 31) as usize;
                    println!("    xn={:#x} sp={:#x}", if rn == 31 { sp } else { x[rn] }, sp);
                }
            }
        }
    }
    for (k, v) in &cats {
        println!("  fallos en {}: {}", k, v);
    }
    println!(
        "motor={} casos={} ok={} fallos={} | con senal esperada: {} | indefinidas (Unicorn rechaza): {} de las cuales heddle ejecuta: {}",
        engine, total, ok, bad, faults, undef_total, undef_bad
    );
    if bad > 0 || undef_bad > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Cmd, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn opciones() {
        assert_eq!(p(&["-h"]), Ok(Cmd::Help));
        assert_eq!(p(&["c.bin", "--version"]), Ok(Cmd::Version));
        assert_eq!(p(&["c.bin"]), Ok(Cmd::Run { path: "c.bin".into(), jit: false, show: 15 }));
        assert_eq!(p(&["--engine", "jit", "c.bin", "--show", "3"]), Ok(Cmd::Run { path: "c.bin".into(), jit: true, show: 3 }));
        assert!(p(&[]).is_err());
        assert!(p(&["c.bin", "--engine"]).is_err());
        assert!(p(&["c.bin", "--engine", "qemu"]).is_err());
        assert!(p(&["c.bin", "--show", "x"]).is_err());
        assert!(p(&["c.bin", "d.bin"]).is_err());
        assert!(p(&["c.bin", "--rapido"]).is_err());
    }

    #[test]
    fn lector_truncado() {
        let b = [1u8, 0, 0];
        let mut rd = Rd { b: &b, p: 0, short: false };
        assert_eq!(rd.u32(), 0);
        assert!(rd.short && rd.eof());
    }
}
