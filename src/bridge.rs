//! Native Bridge de Android: exporta `NativeBridgeItf` (struct NativeBridgeCallbacks, version 8).

use crate::cbthunk;
use crate::elf;
use crate::hle;
use crate::jni;
use crate::libc_hle;
use crate::rt;
use crate::sig;
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

pub const VERSION: u32 = 8;

/// Version de heddle de esta compilacion: la etiqueta de la release (`HEDDLE_VERSION`, la fija el CI al compilar)
/// o, en una compilacion local, la de Cargo.toml. No confundir con `VERSION`, la de la interfaz NativeBridge.
pub const HEDDLE_VERSION: &str = match option_env!("HEDDLE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// Commit de esta compilacion (7 caracteres de `GITHUB_SHA` en el CI; "local" fuera de el).
pub fn build_commit() -> &'static str {
    option_env!("GITHUB_SHA").map(|v| &v[..v.len().min(7)]).unwrap_or("local")
}

/// Linea de `--version` de los binarios: `<programa> <version> (build <commit>)`.
pub fn version_line(prog: &str) -> String {
    format!("{} {} (build {})", prog, HEDDLE_VERSION, build_commit())
}

/// `targetSdkVersion` que se supone fuera de Android (sin `HEDDLE_TARGET_SDK`): la API de la tabla de firmas del NDK.
pub const DEFAULT_TARGET_SDK: i32 = 35;

/// `targetSdkVersion` de la app, el que usa el enlazador de bionic (`get_application_target_sdk_version`) para sus
/// compatibilidades: `HEDDLE_TARGET_SDK` si esta definida; si no, en Android el del proceso (ART lo fija con
/// `android_set_application_target_sdk_version` antes de cargar las bibliotecas de la app y la libc del host lo
/// devuelve) y fuera de Android `DEFAULT_TARGET_SDK`.
pub fn target_sdk() -> i32 {
    if let Some(v) = std::env::var("HEDDLE_TARGET_SDK").ok().and_then(|v| v.trim().parse::<i32>().ok()) {
        return v;
    }
    #[cfg(target_os = "android")]
    {
        extern "C" {
            fn android_get_application_target_sdk_version() -> c_int;
        }
        unsafe { android_get_application_target_sdk_version() }
    }
    #[cfg(not(target_os = "android"))]
    DEFAULT_TARGET_SDK
}

type Ns = c_void;
type SigFn = extern "C" fn(c_int, *mut c_void, *mut c_void) -> bool;

#[repr(C)]
pub struct NativeBridgeCallbacks {
    pub version: u32,
    pub initialize: extern "C" fn(*const c_void, *const c_char, *const c_char) -> bool,
    pub load_library: extern "C" fn(*const c_char, c_int) -> *mut c_void,
    pub get_trampoline: extern "C" fn(*mut c_void, *const c_char, *const c_char, u32) -> *mut c_void,
    pub is_supported: extern "C" fn(*const c_char) -> bool,
    pub get_app_env: extern "C" fn(*const c_char) -> *const c_void,
    pub is_compatible_with: extern "C" fn(u32) -> bool,
    pub get_signal_handler: extern "C" fn(c_int) -> Option<SigFn>,
    pub unload_library: extern "C" fn(*mut c_void) -> c_int,
    pub get_error: extern "C" fn() -> *const c_char,
    pub is_path_supported: extern "C" fn(*const c_char) -> bool,
    pub init_anonymous_namespace: extern "C" fn(*const c_char, *const c_char) -> bool,
    pub create_namespace: extern "C" fn(*const c_char, *const c_char, *const c_char, u64, *const c_char, *mut Ns) -> *mut Ns,
    pub link_namespaces: extern "C" fn(*mut Ns, *mut Ns, *const c_char) -> bool,
    pub load_library_ext: extern "C" fn(*const c_char, c_int, *mut Ns) -> *mut c_void,
    pub get_vendor_namespace: extern "C" fn() -> *mut Ns,
    pub get_exported_namespace: extern "C" fn(*const c_char) -> *mut Ns,
    pub pre_zygote_fork: extern "C" fn(),
    pub get_trampoline_with_jni_call_type: extern "C" fn(*mut c_void, *const c_char, *const c_char, u32, c_int) -> *mut c_void,
    pub get_trampoline_for_function_pointer: extern "C" fn(*const c_void, *const c_char, u32, c_int) -> *mut c_void,
    pub is_native_bridge_function_pointer: extern "C" fn(*const c_void) -> bool,
}

unsafe impl Sync for NativeBridgeCallbacks {}

#[no_mangle]
pub static NativeBridgeItf: NativeBridgeCallbacks = NativeBridgeCallbacks {
    version: VERSION,
    initialize: nb_initialize,
    load_library: nb_load_library,
    get_trampoline: nb_get_trampoline,
    is_supported: nb_is_supported,
    get_app_env: nb_get_app_env,
    is_compatible_with: nb_is_compatible_with,
    get_signal_handler: nb_get_signal_handler,
    unload_library: nb_unload_library,
    get_error: nb_get_error,
    is_path_supported: nb_is_path_supported,
    init_anonymous_namespace: nb_init_anonymous_namespace,
    create_namespace: nb_create_namespace,
    link_namespaces: nb_link_namespaces,
    load_library_ext: nb_load_library_ext,
    get_vendor_namespace: nb_get_vendor_namespace,
    get_exported_namespace: nb_get_exported_namespace,
    pre_zygote_fork: nb_pre_zygote_fork,
    get_trampoline_with_jni_call_type: nb_get_trampoline_jni,
    get_trampoline_for_function_pointer: nb_get_trampoline_fp,
    is_native_bridge_function_pointer: nb_is_nb_fp,
};


// ---------------------------------------------------------------------------------------------
// Registro: en Android los procesos de apps no envian stderr a logcat, asi que se usa liblog
// (resuelta con dlsym para no depender de ella en tiempo de enlace). En Linux cae a stderr.
// ---------------------------------------------------------------------------------------------

extern "C" {
    fn dlsym(h: *mut c_void, name: *const c_char) -> *mut c_void;
    fn pipe(fds: *mut c_int) -> c_int;
    fn dup2(a: c_int, b: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, n: usize) -> isize;
    fn getpid() -> c_int;
}

type AndroidLogWrite = unsafe extern "C" fn(c_int, *const c_char, *const c_char) -> c_int;

fn android_log_fn() -> Option<AndroidLogWrite> {
    static F: std::sync::OnceLock<Option<AndroidLogWrite>> = std::sync::OnceLock::new();
    *F.get_or_init(|| unsafe {
        let p = dlsym(std::ptr::null_mut(), b"__android_log_write\0".as_ptr() as *const c_char);
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute::<*mut c_void, AndroidLogWrite>(p))
        }
    })
}

/// Diagnostico de fallos en codigo del host: pc, sp y quien llamo (direccion de retorno y palabras de la pila),
/// cada direccion con el mapa de /proc/self/maps que la contiene. Layout x86_64 de ucontext_t en bionic.
fn diag_fault(sig: c_int, info: *const u8, uc: *const u64) {
    if uc.is_null() || info.is_null() {
        return;
    }
    let addr = unsafe { *(info.add(16) as *const u64) };
    // uc_mcontext empieza en el byte 40 (5 u64); gregs: R8..R15, RDI, RSI, RBP, RBX, RDX, RAX, RCX, RSP, RIP
    let (rsp, rip) = unsafe { (*uc.add(5 + 15), *uc.add(5 + 16)) };
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let find = |a: u64| -> String {
        for l in maps.lines() {
            let mut it = l.split_whitespace();
            let (Some(r), Some(_), Some(_), Some(_), Some(_)) = (it.next(), it.next(), it.next(), it.next(), it.next()) else { continue };
            let name = it.next().unwrap_or("");
            if let Some((lo, hi)) = r.split_once('-') {
                if let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) {
                    if a >= lo && a < hi {
                        return format!("{}+{:#x}", name, a - lo);
                    }
                }
            }
        }
        "?".to_string()
    };
    alog_fatal(&format!("FALLO senal {} addr={:#x} rip={:#x} [{}] rsp={:#x}", sig, addr, rip, find(rip), rsp));
    alog(&format!("ultimas llamadas HLE de este hilo:\n{}", crate::hle::recent_calls()));
    if let Some(t) = crate::rt::cur_opt() {
        let c = &t.cpu;
        alog(&format!("estado guest:\n{}", crate::rt::dump_backtrace(c)));
        // registros guest completos
        let mut r = String::new();
        for i in 0..31 {
            r += &format!("x{}={:#x} ", i, c.x[i]);
            if i % 6 == 5 {
                r += "\n";
            }
        }
        alog(&format!("registros guest: sp={:#x} pc={:#x}\n{}", c.x[31], c.pc, r));
        // palabras de codigo ARM desde el pc (decodificables con llvm-mc -triple=aarch64 --disassemble)
        let pc = c.pc & 0x00FF_FFFF_FFFF_FFFF;
        if crate::rt::addr_readable(pc) && crate::rt::addr_readable(pc + 63) {
            let mut w = String::new();
            for i in 0..16u64 {
                w += &format!("{:08x} ", unsafe { *((pc + 4 * i) as *const u32) });
            }
            alog(&format!("codigo guest en pc: {}", w));
        }
        // modulos guest cargados
        for m in crate::elf::modules() {
            alog(&format!("modulo {} {:#x}-{:#x}", m.name, m.lo, m.hi));
        }
        // mapas del proceso cercanos a la direccion del fallo y al sp
        for target in [addr, c.x[31]] {
            for l in maps.lines() {
                let rng = l.split_whitespace().next().unwrap_or("");
                if let Some((lo, hi)) = rng.split_once('-') {
                    if let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) {
                        if target + 0x400_0000 >= lo && target <= hi + 0x400_0000 {
                            let short: Vec<&str> = l.split_whitespace().take(2).chain(l.split_whitespace().skip(5).take(1)).collect();
                            alog(&format!("mapa cerca de {:#x}: {}", target, short.join(" ")));
                        }
                    }
                }
            }
        }
    }
    if rsp != 0 {
        for i in 0..24u64 {
            let w = unsafe { *((rsp + 8 * i) as *const u64) };
            let m = find(w);
            if m != "?" && !m.starts_with("[stack") {
                alog(&format!("  pila[{}] = {:#x} {}", i, w, m));
            }
        }
    }
}

/// Registro informativo (prioridad INFO de logcat).
pub fn alog(msg: &str) {
    alog_prio(4, msg);
}

/// Mensaje fatal o de fallo (`FALLO`, `FILTRACION`, `VIGILANTE` y lo que precede a un abort/raise): prioridad
/// ERROR (visible con `logcat *:E`) y escrito de forma sincrona con `__android_log_write`, que no pasa por la
/// tuberia de stderr ni por su hilo lector: el mensaje ya esta en logd cuando el proceso termina. Sin logcat,
/// directo a stderr (sin bufer).
pub fn alog_fatal(msg: &str) {
    alog_prio(6, msg);
}

/// `async_safe_write_log(ANDROID_LOG_FATAL, "libc", msg)` de bionic: lo que escribe la libc del guest al abortar
/// (`__fortify_fatal`). Sin logcat no hace nada (bionic ya lo escribio en stderr).
pub fn alog_libc_fatal(msg: &str) {
    if let Some(f) = android_log_fn() {
        let m = CString::new(msg.replace('\0', "")).unwrap_or_default();
        crate::ndkcb::bridge_logging(|| unsafe { f(7, b"libc\0".as_ptr() as *const c_char, m.as_ptr()) });
    }
}

fn alog_prio(prio: c_int, msg: &str) {
    match android_log_fn() {
        Some(f) => {
            let m = CString::new(msg.replace('\0', "")).unwrap_or_default();
            // mensaje del puente: no pasa por un registrador que haya instalado el guest (ver ndkcb::heddle_logger)
            crate::ndkcb::bridge_logging(|| unsafe { f(prio, b"heddle\0".as_ptr() as *const c_char, m.as_ptr()) });
        }
        None => eprintln!("[heddle] {}", msg),
    }
}

macro_rules! blog {
    ($($a:tt)*) => { $crate::bridge::alog(&format!($($a)*)) };
}

/// Redirige stderr (fd 2) a logcat mediante una tuberia y un hilo. Solo en Android y solo despues del fork
/// del zygote (en `initialize`), porque el zygote debe ser monohilo al hacer fork.
fn redirect_stderr_to_logcat() {
    if android_log_fn().is_none() {
        return;
    }
    unsafe {
        let mut fds = [0 as c_int; 2];
        if pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        dup2(fds[1], 2);
        let r = fds[0];
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut acc: Vec<u8> = Vec::new();
            loop {
                let n = read(r, buf.as_mut_ptr() as *mut c_void, buf.len());
                if n <= 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n as usize]);
                if acc.len() > (64 << 10) && !acc.contains(&b'\n') {
                    acc.push(b'\n'); // linea sin fin: se corta para no acumular
                }
                while let Some(i) = acc.iter().position(|&b| b == b'\n') {
                    let line: Vec<u8> = acc.drain(..=i).collect();
                    alog(&String::from_utf8_lossy(&line[..line.len() - 1]));
                }
            }
        });
    }
}

#[used]
#[link_section = ".init_array"]
static HEDDLE_CTOR: extern "C" fn() = heddle_ctor;

extern "C" fn heddle_ctor() {
    // Solo un mensaje: se ejecuta al hacer dlopen (en el zygote, antes del fork): sin hilos.
    blog!("libheddle.so cargada (pid={}, version={}, heddle={}, build={})", unsafe { getpid() }, VERSION, HEDDLE_VERSION, build_commit());
}

static INITIALIZED: AtomicBool = AtomicBool::new(false);
/// Si es true, los fallos sincronos (SEGV...) llegan por getSignalHandler y no se instalan manejadores propios.
pub static BRIDGE_MODE: AtomicBool = AtomicBool::new(false);
static LAST_ERROR: Mutex<Option<CString>> = Mutex::new(None);

fn set_error(e: &str) {
    blog!("error: {}", e);
    *crate::monitor::lock(&LAST_ERROR) = CString::new(e.replace('\0', "")).ok();
}

fn s(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(p).to_string_lossy().into_owned() }
    }
}

extern "C" fn nb_initialize(rt_cbs: *const c_void, private_dir: *const c_char, isa: *const c_char) -> bool {
    if INITIALIZED.swap(true, Relaxed) {
        return true;
    }
    redirect_stderr_to_logcat();
    let isa = s(isa);
    blog!("initialize(isa='{}', private_dir='{}') pid={} build={}", isa, s(private_dir), unsafe { getpid() }, build_commit());
    if !(isa.is_empty() || isa == "arm64" || isa == "arm64-v8a" || isa == "aarch64") {
        blog!("ISA no soportada: {}", isa);
        INITIALIZED.store(false, Relaxed);
        return false;
    }
    BRIDGE_MODE.store(true, Relaxed);
    jni::RUNTIME_CB.store(rt_cbs as u64, Relaxed);
    hle::init();
    rt::ret_slot();
    jni::ensure_tables();
    sig::init_own_range();
    crate::guestlib::set_private_dir(&s(private_dir));
    blog!("initialize OK");
    true
}

/// dlopen guest pedido por ART en el espacio `ns` (sin llamador guest).
fn do_load(path: &str, flags: c_int, ns: usize) -> *mut c_void {
    let flags = flags as u64 & (elf::RTLD_NOW | elf::RTLD_LAZY | elf::RTLD_GLOBAL | elf::RTLD_NODELETE | elf::RTLD_NOLOAD);
    match elf::open_lib(path, flags, ns, None, None) {
        Ok(elf::Dep::M(m)) => m.handle as *mut c_void,
        Ok(elf::Dep::Sys(l)) => elf::sys_handle(l) as *mut c_void,
        Ok(elf::Dep::Exe) => 1 as *mut c_void,
        Err(e) => {
            alog(&format!("carga fallida: {}", e));
            set_error(&format!("dlopen failed: {}", e));
            std::ptr::null_mut()
        }
    }
}

extern "C" fn nb_load_library(path: *const c_char, flags: c_int) -> *mut c_void {
    blog!("loadLibrary('{}')", s(path));
    do_load(&s(path), flags, crate::namespace::anonymous())
}

fn lookup_handle(h: *mut c_void, name: &str) -> Option<u64> {
    let hv = h as u64;
    if hv == 1 || hv == 0 || elf::is_sys_handle(hv) {
        return libc_hle::resolve(name);
    }
    elf::module_by_handle(hv).and_then(|m| m.lookup(name))
}

fn trampoline(h: *mut c_void, name: *const c_char, shorty: *const c_char, critical: bool) -> *mut c_void {
    let n = s(name);
    blog!("getTrampoline('{}', shorty='{}')", n, s(shorty));
    let Some(f) = lookup_handle(h, &n) else {
        set_error(&format!("simbolo no encontrado: {}", n));
        return std::ptr::null_mut();
    };
    // Punto de entrada de NativeActivity: el nombre por defecto, o el que declara el manifiesto
    // (android.app.func_name), que loadNativeCode_native pide con firma nula; ART solo pide sin firma JNI_OnLoad y
    // JNI_OnUnload (los natives llevan siempre su shorty).
    let activity = n == "ANativeActivity_onCreate"
        || (shorty.is_null() && n != "JNI_OnLoad" && n != "JNI_OnUnload" && elf::module_by_handle(h as u64).map_or(false, |m| crate::ndkcb::is_activity_entry(&m.path, &n)));
    let t = match n.as_str() {
        "JNI_OnLoad" | "JNI_OnUnload" => cbthunk::make(f, if n == "JNI_OnLoad" { b'I' } else { b'V' }, 2),
        _ if activity => cbthunk::make_ex(f, b'V', 0, 1),
        // con firma: los argumentos se reparten por el shorty (SysV -> AAPCS64), no de forma universal
        _ if !shorty.is_null() => cbthunk::make_sig(f, unsafe { std::ffi::CStr::from_ptr(shorty) }.to_bytes(), if critical { 0 } else { 1 }),
        _ => cbthunk::make(f, b'J', if critical { 0 } else { 1 }),
    };
    t as *mut c_void
}

extern "C" fn nb_get_trampoline(h: *mut c_void, name: *const c_char, shorty: *const c_char, _len: u32) -> *mut c_void {
    trampoline(h, name, shorty, false)
}

extern "C" fn nb_get_trampoline_jni(h: *mut c_void, name: *const c_char, shorty: *const c_char, _len: u32, t: c_int) -> *mut c_void {
    trampoline(h, name, shorty, t == 2)
}

extern "C" fn nb_get_trampoline_fp(fp: *const c_void, shorty: *const c_char, _len: u32, t: c_int) -> *mut c_void {
    let ret = if shorty.is_null() { b'J' } else { unsafe { *shorty as u8 } };
    let f = fp as u64;
    // RegisterNatives ya envolvio la funcion guest en un trampolin (codigo x86 del host): to_host_callable lo deja
    // igual en vez de envolverlo otra vez, y nunca entrega al host un slot HLE.
    let jni = if t == 2 { 0 } else { 1 };
    let r = if shorty.is_null() { crate::boundary::to_host_callable(f, ret, jni) } else { crate::boundary::to_host_callable_sig(f, unsafe { std::ffi::CStr::from_ptr(shorty) }.to_bytes(), jni) };
    blog!("getTrampolineForFunctionPointer({}, shorty='{}') -> {}", elf::describe_addr(f), s(shorty), if r == f { "sin cambios (ya llamable por el host)" } else { "nuevo trampolin" });
    r as *mut c_void
}

extern "C" fn nb_is_supported(path: *const c_char) -> bool {
    let p = s(path);
    let r = nb_is_supported_inner(&p);
    blog!("isSupported('{}') -> {}", p, r);
    r
}

fn nb_is_supported_inner(p: &str) -> bool {
    let Ok(mut f) = std::fs::File::open(p) else { return false };
    use std::io::Read;
    let mut h = [0u8; 20];
    if f.read_exact(&mut h).is_err() {
        return false;
    }
    &h[0..4] == b"\x7fELF" && h[4] == 2 && h[5] == 1 && u16::from_le_bytes([h[18], h[19]]) == 183
}

extern "C" fn nb_get_app_env(_isa: *const c_char) -> *const c_void {
    blog!("getAppEnv('{}')", s(_isa));
    std::ptr::null() // sin valores de entorno adicionales
}

extern "C" fn nb_is_compatible_with(v: u32) -> bool {
    let r = v >= 1 && v <= VERSION;
    blog!("isCompatibleWith({}) -> {}", v, r);
    r
}

extern "C" fn nb_get_signal_handler(sig: c_int) -> Option<SigFn> {
    blog!("getSignalHandler({})", sig);
    match sig {
        4 | 7 | 8 | 11 => Some(nb_signal),
        _ => None,
    }
}

/// Devuelve true si la senal se consumio: el host llamo a una direccion guest (redireccion a un trampolin) o el fallo
/// era de codigo guest y su manejador lo gestiono (volvio, con el contexto cambiado o no, o salio con siglongjmp): la
/// ejecucion sigue con el contexto que dejo el guest (ver sig.rs, "Reanudar tras un fallo sincrono"). Si no, se
/// registra el diagnostico y ART sigue con la cadena (sus comprobaciones implicitas, debuggerd).
extern "C" fn nb_signal(sig: c_int, info: *mut c_void, uc: *mut c_void) -> bool {
    if unsafe { sig::native_bridge_signal(sig, info as *mut u8, uc) } {
        return true;
    }
    // el diagnostico solo si el fallo es del puente o del codigo traducido: los de ART (comprobaciones implicitas en
    // un hilo que tambien ejecuta codigo guest) siguen su camino sin ruido
    if unsafe { sig::fault_is_ours(uc) } {
        diag_fault(sig, info as *const u8, uc as *const u64);
    }
    false
}

extern "C" fn nb_unload_library(h: *mut c_void) -> c_int {
    let hv = h as u64;
    if let Some(m) = elf::module_by_handle(hv) {
        elf::unload(&m);
    }
    0
}

extern "C" fn nb_get_error() -> *const c_char {
    let g = crate::monitor::lock(&LAST_ERROR);
    match g.as_ref() {
        Some(c) => c.as_ptr(),
        None => std::ptr::null(),
    }
}

extern "C" fn nb_is_path_supported(p: *const c_char) -> bool {
    let p = s(p);
    let r = p.contains("arm64") || p.contains("aarch64") || p.contains("/lib64/arm64");
    blog!("isPathSupported('{}') -> {}", p, r);
    r
}

extern "C" fn nb_init_anonymous_namespace(public_libs: *const c_char, search_path: *const c_char) -> bool {
    let (libs, path) = (s(public_libs), s(search_path));
    blog!("initAnonymousNamespace('{}', '{}')", libs, path);
    match crate::namespace::init_anonymous(&libs, &path) {
        Ok(()) => true,
        Err(e) => {
            set_error(&e);
            false
        }
    }
}

fn ns_of(p: *mut Ns) -> Option<usize> {
    crate::namespace::from_handle(p as u64)
}

extern "C" fn nb_create_namespace(name: *const c_char, ld_path: *const c_char, default_path: *const c_char, typ: u64, permitted: *const c_char, parent: *mut Ns) -> *mut Ns {
    let (n, ld, def, perm) = (s(name), s(ld_path), s(default_path), s(permitted));
    blog!("createNamespace('{}', ld='{}', default='{}', type={:#x}, permitted='{}')", n, ld, def, typ, perm);
    match crate::namespace::create(&n, &ld, &def, typ, &perm, ns_of(parent)) {
        Ok(id) => crate::namespace::handle_of(id) as *mut Ns,
        Err(e) => {
            set_error(&e);
            std::ptr::null_mut()
        }
    }
}

extern "C" fn nb_link_namespaces(from: *mut Ns, to: *mut Ns, sonames: *const c_char) -> bool {
    let libs = s(sonames);
    blog!("linkNamespaces({:?} -> {:?}, '{}')", ns_of(from).map(crate::namespace::name), ns_of(to).map(crate::namespace::name), libs);
    let Some(f) = ns_of(from) else {
        set_error("error linking namespaces: namespace_from is null.");
        return false;
    };
    match crate::namespace::link(f, ns_of(to), &libs) {
        Ok(()) => true,
        Err(e) => {
            set_error(&e);
            false
        }
    }
}

extern "C" fn nb_load_library_ext(path: *const c_char, flags: c_int, ns: *mut Ns) -> *mut c_void {
    blog!("loadLibraryExt('{}', ns={:?})", s(path), ns_of(ns).map(crate::namespace::name));
    do_load(&s(path), flags, ns_of(ns).unwrap_or_else(crate::namespace::anonymous))
}

fn exported_ns(name: &str) -> *mut Ns {
    match crate::namespace::exported(name) {
        Some(id) => crate::namespace::handle_of(id) as *mut Ns,
        None => std::ptr::null_mut(),
    }
}

extern "C" fn nb_get_vendor_namespace() -> *mut Ns {
    exported_ns("sphal")
}

extern "C" fn nb_get_exported_namespace(name: *const c_char) -> *mut Ns {
    exported_ns(&s(name))
}

extern "C" fn nb_pre_zygote_fork() {
    blog!("preZygoteFork");
}

extern "C" fn nb_is_nb_fp(p: *const c_void) -> bool {
    let a = p as u64;
    hle::is_hle(a) || elf::module_for_addr(a).is_some()
}

#[allow(dead_code)]
static _KEEP: AtomicU64 = AtomicU64::new(0);
