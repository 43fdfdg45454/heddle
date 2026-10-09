//! sigtool: genera la tabla de firmas de la frontera host/guest de heddle a partir de las cabeceras del NDK.
//!
//! Analiza cada cabecera con libclang para arm64 (guest) y x86-64 (host) y clasifica cada funcion exportada por las
//! bibliotecas del NDK: que argumentos son punteros a funcion, JNIEnv*, long double, va_list, estructuras por valor,
//! y si alguna estructura alcanzable por puntero tiene distinta disposicion entre las dos arquitecturas.
//!
//! Uso: sigtool <sysroot-ndk> <salida sigs_gen.rs> <informe.md>
//! El sysroot es el directorio con `include/` y `lib/<triple>/*.so` (lo exporta el flujo de CI `sysroot`) y, para
//! Vulkan, el registro vk.xml de la misma version que las cabeceras: por defecto el extracto versionado
//! `vk_extracto.xml` (registro 1.3.275, generado con `vkextract.py`), o `SIGTOOL_VK_XML`; de el salen `structextends`
//! (que estructura puede ir en la cadena `pNext` de cual) y `len` (cuentas de los arreglos). Sin el, la tabla de
//! Vulkan es conservadora: cualquier estructura en cualquier cadena.
//! Sin dependencias: enlaza con la libclang del sistema (ver build.rs).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{c_char, c_int, c_long, c_longlong, c_uint, c_void, CStr, CString};
use std::fmt::Write as _;

// ------------------------------------------------------------------------------------------------
// libclang (API C estable)
// ------------------------------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy)]
struct CXString {
    data: *const c_void,
    flags: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CXCursor {
    kind: c_int,
    xdata: c_int,
    data: [*const c_void; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CXType {
    kind: c_int,
    data: [*mut c_void; 2],
}

type Visitor = extern "C" fn(CXCursor, CXCursor, *mut c_void) -> c_int;

extern "C" {
    fn clang_createIndex(exclude_pch: c_int, diag: c_int) -> *mut c_void;
    fn clang_parseTranslationUnit(idx: *mut c_void, file: *const c_char, args: *const *const c_char, nargs: c_int, unsaved: *mut c_void, nunsaved: c_uint, opts: c_uint) -> *mut c_void;
    fn clang_disposeTranslationUnit(tu: *mut c_void);
    fn clang_getTranslationUnitCursor(tu: *mut c_void) -> CXCursor;
    fn clang_visitChildren(c: CXCursor, v: Visitor, data: *mut c_void) -> c_uint;
    fn clang_getCursorSpelling(c: CXCursor) -> CXString;
    fn clang_getCString(s: CXString) -> *const c_char;
    fn clang_disposeString(s: CXString);
    fn clang_getCursorType(c: CXCursor) -> CXType;
    fn clang_getCanonicalType(t: CXType) -> CXType;
    fn clang_getResultType(t: CXType) -> CXType;
    fn clang_getNumArgTypes(t: CXType) -> c_int;
    fn clang_getArgType(t: CXType, i: c_uint) -> CXType;
    fn clang_isFunctionTypeVariadic(t: CXType) -> c_uint;
    fn clang_getPointeeType(t: CXType) -> CXType;
    fn clang_getTypeSpelling(t: CXType) -> CXString;
    fn clang_Type_getSizeOf(t: CXType) -> c_longlong;
    fn clang_Type_getAlignOf(t: CXType) -> c_longlong;
    fn clang_Cursor_getOffsetOfField(c: CXCursor) -> c_longlong;
    fn clang_getTypeDeclaration(t: CXType) -> CXCursor;
    fn clang_getElementType(t: CXType) -> CXType;
    fn clang_getNumElements(t: CXType) -> c_longlong;
    fn clang_Cursor_isBitField(c: CXCursor) -> c_uint;
    fn clang_getFieldDeclBitWidth(c: CXCursor) -> c_int;
    fn clang_isCursorDefinition(c: CXCursor) -> c_uint;
    fn clang_getNumDiagnostics(tu: *mut c_void) -> c_uint;
    fn clang_getDiagnostic(tu: *mut c_void, i: c_uint) -> *mut c_void;
    fn clang_getDiagnosticSeverity(d: *mut c_void) -> c_int;
    fn clang_disposeDiagnostic(d: *mut c_void);
    fn clang_Cursor_getNumArguments(c: CXCursor) -> c_int;
    fn clang_Cursor_getArgument(c: CXCursor, i: c_uint) -> CXCursor;
    fn clang_getEnumConstantDeclValue(c: CXCursor) -> c_longlong;
    fn clang_isConstQualifiedType(t: CXType) -> c_uint;
}

const K_STRUCT_DECL: c_int = 2;
const K_UNION_DECL: c_int = 3;
const K_ENUM_DECL: c_int = 5;
const K_ENUM_CONSTANT: c_int = 7;
const K_FIELD: c_int = 6;
const K_FUNCTION: c_int = 8;

const T_VOID: c_int = 2;
const T_BOOL: c_int = 3;
const T_FLOAT: c_int = 21;
const T_DOUBLE: c_int = 22;
const T_LONGDOUBLE: c_int = 23;
const T_FLOAT128: c_int = 30;
const T_COMPLEX: c_int = 100;
const T_POINTER: c_int = 101;
const T_RECORD: c_int = 105;
const T_ENUM: c_int = 106;
const T_FN_NOPROTO: c_int = 110;
const T_FN_PROTO: c_int = 111;
const T_CONST_ARRAY: c_int = 112;
const T_INCOMPLETE_ARRAY: c_int = 114;

fn cxs(s: CXString) -> String {
    unsafe {
        let p = clang_getCString(s);
        let r = if p.is_null() { String::new() } else { CStr::from_ptr(p).to_string_lossy().into_owned() };
        clang_disposeString(s);
        r
    }
}

fn tspell(t: CXType) -> String {
    cxs(unsafe { clang_getTypeSpelling(t) })
}

// ------------------------------------------------------------------------------------------------
// Modelo
// ------------------------------------------------------------------------------------------------

/// Clase de un argumento o retorno para la frontera.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Cls {
    Void,
    /// entero (incluye bool, enum) de `n` bytes; el segundo campo indica si tiene signo
    Int(u8, bool),
    /// puntero a datos (o a tipo incompleto / void)
    Ptr,
    Float,
    Double,
    LongDouble,
    Complex,
    /// puntero a funcion; el caracter es la clase del retorno (V I J F D Z B S)
    FnPtr(char),
    /// JNIEnv* / JavaVM*
    Env,
    /// puntero a un objeto cuya tabla de funciones es del host (interfaz estilo OpenSL)
    HostVtable(String),
    /// estructura por valor: tamano y si contiene coma flotante en algun nivel
    Rec(u32, bool),
    VaList,
    Other(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RecInfo {
    /// tamano, alineacion y campos (nombre@desplazamiento:clase) en una cadena comparable entre arquitecturas
    layout: String,
    has_fnptr: bool,
    all_fnptr: bool,
    nfields: usize,
    /// estructuras alcanzables por punteros en los campos
    ptr_recs: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Func {
    ret: Cls,
    args: Vec<Cls>,
    variadic: bool,
    /// estructuras alcanzables por puntero desde argumentos o retorno
    recs: Vec<String>,
    header: String,
    /// argumentos que son (o apuntan a) estructuras con punteros a funcion
    scb: Vec<Scb>,
}

/// Argumento de un metodo de interfaz (tabla de funciones estilo OpenSL) o de un callback.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MArg {
    /// escalar o puntero a datos: caracter de shorty (V Z B C S I J F D)
    V(char),
    /// interfaz por valor (puntero a puntero a la tabla)
    Itf(String),
    /// puntero a una interfaz que escribe quien recibe la llamada (parametro de salida)
    ItfOut(String),
    /// puntero a funcion: firma del callback (retorno primero; 'O' = interfaz)
    Cb(String),
    /// puntero a SLDataSource/SLDataSink (o XA): su localizador puede llevar un objeto
    Data,
    Bad(String),
}

/// Metodo de una interfaz: nombre, retorno y argumentos (el primero es la propia interfaz), estructuras apuntadas.
#[derive(Clone, Debug)]
struct ItfMethod {
    name: String,
    ret: MArg,
    args: Vec<MArg>,
    recs: Vec<String>,
}

/// Estructura con punteros a funcion recibida por una funcion (por puntero o por valor).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Scb {
    /// nombre de la estructura (p. ej. "struct z_stream_s")
    ty: String,
    arg: usize,
    byval: bool,
    size: i64,
    /// (desplazamiento en bytes, firma del callback o motivo por el que no se puede convertir)
    fields: Vec<(i64, Result<String, String>)>,
    /// hay punteros a funcion alcanzables por otro puntero (no se ven desde aqui)
    deep: bool,
    /// algun puntero a funcion esta en una union (su significado depende de otro campo)
    union: bool,
    recs: Vec<String>,
}

/// Estructura de Vulkan con cabecera `sType`/`pNext` (o `VkAllocationCallbacks`): tamano, disposicion y callbacks.
#[derive(Clone, Debug, Default)]
struct VkS {
    size: i64,
    layout: String,
    sc: Scb,
}

/// Campo de una estructura de Vulkan (las anidadas por valor, aplanadas con su prefijo `a.b`).
#[derive(Clone, Debug, Default)]
struct VkMem {
    name: String,
    /// estructura que declara el campo (la anidada, si se aplano) y nombre ahi
    owner: String,
    local: String,
    off: i64,
    /// entero: tamano (0 si no lo es)
    int_size: i64,
    /// puntero a estructura(s): (nombre, apuntada const, niveles: 1 = arreglo de estructuras, 2 = arreglo de punteros,
    /// 0 = estructura con cabecera sType/pNext anidada por valor)
    ptr: Option<(String, bool, u8)>,
}

/// Cualquier estructura de Vulkan: campos (para seguir punteros a otras estructuras) y callbacks propios.
#[derive(Clone, Debug, Default)]
struct VkRec {
    size: i64,
    chained: bool,
    layout: String,
    mems: Vec<VkMem>,
    sc: Scb,
    /// puntero a una estructura que no es de Vulkan con punteros a funcion
    foreign_cb: bool,
}

/// Argumento de una funcion de Vulkan, lo que importa para convertir `pAllocator` y las cadenas `pNext`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct VkArg {
    fp: bool,
    int: bool,
    pname: String,
    /// puntero a una estructura: (nombre, apuntada const)
    ptr_rec: Option<(String, bool)>,
    /// estructura por valor: tamano
    byval: i64,
}

#[derive(Default)]
struct Arch {
    funcs: BTreeMap<String, Func>,
    recs: HashMap<String, RecInfo>,
    /// tipos escalares apuntados, por nombre (typedef): tamano. Para comparar con una estructura homonima en la otra arquitectura.
    scalars: HashMap<String, i64>,
    parsed: usize,
    failed: Vec<String>,
    /// interfaces (estructuras `...Itf_` hechas solo de punteros a funcion): metodos en orden
    itfs: BTreeMap<String, Vec<ItfMethod>>,
    /// Vulkan: estructuras con sType/pNext (y VkAllocationCallbacks), constantes de VkStructureType (nombre
    /// normalizado -> (nombre, valor)) y argumentos de cada funcion vk*
    vk_structs: BTreeMap<String, VkS>,
    vk_consts: BTreeMap<String, (String, i64)>,
    vk_fns: BTreeMap<String, Vec<VkArg>>,
    /// todas las estructuras Vk* (no solo las de cadena): campos para seguir punteros anidados
    vk_all: BTreeMap<String, VkRec>,
}

struct Ctx<'a> {
    arch: &'a mut Arch,
    header: String,
}

fn ret_char(t: CXType) -> char {
    let c = unsafe { clang_getCanonicalType(t) };
    match c.kind {
        T_VOID => 'V',
        T_BOOL => 'Z',
        T_FLOAT => 'F',
        T_DOUBLE => 'D',
        T_POINTER => 'J',
        _ => match unsafe { clang_Type_getSizeOf(c) } {
            1 => 'B',
            2 => 'S',
            4 => 'I',
            _ => 'J',
        },
    }
}

fn is_fp_kind(k: c_int) -> bool {
    k == T_FLOAT || k == T_DOUBLE
}

/// Registra (si no esta) la informacion de disposicion de una estructura y devuelve su nombre canonico.
fn record_info(arch: &mut Arch, t: CXType) -> String {
    let name = tspell(t).replace("const ", "").replace("volatile ", "");
    let size = unsafe { clang_Type_getSizeOf(t) };
    match arch.recs.get(&name) {
        // ya analizada con su definicion completa (o en curso)
        Some(r) if r.layout != "opaco" => return name,
        // vista antes solo como declaracion adelantada: si ahora esta completa, se analiza de verdad
        Some(_) if size < 0 => return name,
        _ => {}
    }
    if size < 0 {
        // tipo incompleto en esta cabecera: se anota como opaco, pero una definicion posterior lo sustituye
        arch.recs.insert(name.clone(), RecInfo { layout: "opaco".into(), ..Default::default() });
        return name;
    }
    // reservar antes de recorrer (estructuras recursivas por puntero)
    arch.recs.insert(name.clone(), RecInfo { layout: "en curso".into(), ..Default::default() });
    struct F {
        fields: Vec<(String, i64, CXType, bool, i32)>,
    }
    extern "C" fn fv(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
        let f = unsafe { &mut *(d as *mut F) };
        if c.kind == K_FIELD {
            let bit = unsafe { clang_Cursor_isBitField(c) } != 0;
            let w = if bit { unsafe { clang_getFieldDeclBitWidth(c) } } else { 0 };
            f.fields.push((cxs(unsafe { clang_getCursorSpelling(c) }), unsafe { clang_Cursor_getOffsetOfField(c) }, unsafe { clang_getCursorType(c) }, bit, w));
        }
        1
    }
    let mut f = F { fields: Vec::new() };
    unsafe { clang_visitChildren(clang_getTypeDeclaration(t), fv, &mut f as *mut F as *mut c_void) };
    let mut layout = format!("size={} align={}", size, unsafe { clang_Type_getAlignOf(t) });
    let (mut has_fn, mut all_fn, mut ptr_recs) = (false, !f.fields.is_empty(), Vec::new());
    for (fname, off, ft, bit, w) in &f.fields {
        let (desc, isfn) = field_desc(arch, *ft, &mut ptr_recs, 0);
        has_fn |= isfn;
        all_fn &= isfn;
        let _ = write!(layout, " {}@{}:{}{}", fname, off, desc, if *bit { format!(":{}", w) } else { String::new() });
    }
    let info = RecInfo { layout, has_fnptr: has_fn, all_fnptr: all_fn, nfields: f.fields.len(), ptr_recs };
    arch.recs.insert(name.clone(), info);
    name
}

/// Descripcion de un campo para comparar disposiciones; el booleano indica si es (o contiene) un puntero a funcion.
fn field_desc(arch: &mut Arch, t: CXType, ptr_recs: &mut Vec<String>, depth: u32) -> (String, bool) {
    let c = unsafe { clang_getCanonicalType(t) };
    match c.kind {
        T_POINTER => {
            let p = unsafe { clang_getCanonicalType(clang_getPointeeType(c)) };
            if p.kind == T_FN_PROTO || p.kind == T_FN_NOPROTO {
                ("fnptr".into(), true)
            } else {
                // se anota la estructura apuntada (un nivel de punteros intermedio como maximo)
                let mut q = p;
                if q.kind == T_POINTER {
                    q = unsafe { clang_getCanonicalType(clang_getPointeeType(q)) };
                }
                if q.kind == T_RECORD && depth < 8 {
                    let n = record_info(arch, q);
                    if !ptr_recs.contains(&n) {
                        ptr_recs.push(n);
                    }
                }
                ("ptr".into(), false)
            }
        }
        T_RECORD => {
            let n = record_info(arch, c);
            let r = arch.recs.get(&n).cloned().unwrap_or_default();
            for pr in &r.ptr_recs {
                if !ptr_recs.contains(pr) {
                    ptr_recs.push(pr.clone());
                }
            }
            (format!("{{{}}}", r.layout), r.has_fnptr)
        }
        T_CONST_ARRAY => {
            let e = unsafe { clang_getElementType(c) };
            let (d, f) = field_desc(arch, e, ptr_recs, depth + 1);
            (format!("[{}x{}]", unsafe { clang_getNumElements(c) }, d), f)
        }
        T_INCOMPLETE_ARRAY => {
            let e = unsafe { clang_getElementType(c) };
            let (d, f) = field_desc(arch, e, ptr_recs, depth + 1);
            (format!("[]{}", d), f)
        }
        T_FLOAT => ("f32".into(), false),
        T_DOUBLE => ("f64".into(), false),
        T_LONGDOUBLE | T_FLOAT128 => ("longdouble".into(), false),
        T_COMPLEX => ("complex".into(), false),
        _ => (format!("i{}", unsafe { clang_Type_getSizeOf(c) }), false),
    }
}

/// La estructura contiene float/double/long double en algun nivel (campos, estructuras anidadas, arrays)?
/// AAPCS64 y SysV reparten esos miembros en registros de forma distinta, tambien en estructuras pequenas.
fn has_fp(t: CXType, depth: u32) -> bool {
    struct H {
        fp: bool,
        depth: u32,
    }
    extern "C" fn hv(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
        let h = unsafe { &mut *(d as *mut H) };
        if c.kind == K_FIELD {
            let mut ft = unsafe { clang_getCanonicalType(clang_getCursorType(c)) };
            while ft.kind == T_CONST_ARRAY || ft.kind == T_INCOMPLETE_ARRAY {
                ft = unsafe { clang_getCanonicalType(clang_getElementType(ft)) };
            }
            if is_fp_kind(ft.kind) || ft.kind == T_LONGDOUBLE || ft.kind == T_FLOAT128 || ft.kind == T_COMPLEX {
                h.fp = true;
            } else if ft.kind == T_RECORD && h.depth < 8 && has_fp(ft, h.depth + 1) {
                h.fp = true;
            }
        }
        1
    }
    let mut h = H { fp: false, depth };
    unsafe { clang_visitChildren(clang_getTypeDeclaration(t), hv, &mut h as *mut H as *mut c_void) };
    h.fp
}

/// Clasifica un tipo de argumento o retorno. `sugared` conserva los typedef (para reconocer va_list).
fn classify(arch: &mut Arch, sugared: CXType, recs: &mut Vec<String>) -> Cls {
    let sp = tspell(sugared);
    if sp.contains("va_list") {
        return Cls::VaList;
    }
    let c = unsafe { clang_getCanonicalType(sugared) };
    let csp = tspell(c);
    if csp.contains("__va_list") {
        return Cls::VaList;
    }
    match c.kind {
        T_VOID => Cls::Void,
        T_FLOAT => Cls::Float,
        T_DOUBLE => Cls::Double,
        T_LONGDOUBLE | T_FLOAT128 => Cls::LongDouble,
        T_COMPLEX => Cls::Complex,
        T_RECORD => {
            let size = unsafe { clang_Type_getSizeOf(c) }.max(0) as u32;
            let n = record_info(arch, c);
            push_rec(arch, recs, &n);
            Cls::Rec(size, has_fp(c, 0))
        }
        T_POINTER | T_CONST_ARRAY | T_INCOMPLETE_ARRAY => {
            let p = if c.kind == T_POINTER { unsafe { clang_getCanonicalType(clang_getPointeeType(c)) } } else { unsafe { clang_getCanonicalType(clang_getElementType(c)) } };
            if p.kind == T_FN_PROTO || p.kind == T_FN_NOPROTO {
                return fn_ptr(arch, p, recs);
            }
            if csp.contains("JNINativeInterface") || csp.contains("JNIInvokeInterface") || csp.contains("_JNIEnv") || csp.contains("_JavaVM") {
                return Cls::Env;
            }
            // hasta tres niveles de puntero hasta una estructura
            let (mut q, mut levels) = (p, 1);
            while q.kind == T_POINTER && levels < 3 {
                q = unsafe { clang_getCanonicalType(clang_getPointeeType(q)) };
                levels += 1;
            }
            if q.kind == T_FN_PROTO || q.kind == T_FN_NOPROTO {
                // puntero a puntero a funcion: el host escribira o leera un puntero a funcion en memoria
                return Cls::Other("puntero a puntero a funcion".into());
            }
            if q.kind != T_RECORD && c.kind == T_POINTER {
                // puntero a un escalar con nombre (p. ej. sigset_t en arm64): se anota su tamano
                let sug = unsafe { clang_getPointeeType(sugared) };
                let nm = tspell(sug).replace("const ", "").replace("volatile ", "");
                let sz = unsafe { clang_Type_getSizeOf(q) };
                if sz > 0 && !nm.contains(' ') && !nm.contains('*') {
                    arch.scalars.insert(nm.clone(), sz);
                    if !recs.contains(&nm) {
                        recs.push(nm);
                    }
                }
            }
            if q.kind == T_RECORD {
                let n = record_info(arch, q);
                let r = arch.recs.get(&n).cloned().unwrap_or_default();
                // interfaz estilo OpenSL: puntero a puntero a una estructura que es solo punteros a funcion
                if levels >= 2 && r.all_fnptr && r.nfields > 0 {
                    return Cls::HostVtable(n);
                }
                push_rec(arch, recs, &n);
            }
            Cls::Ptr
        }
        T_FN_PROTO | T_FN_NOPROTO => fn_ptr(arch, c, recs),
        T_BOOL => Cls::Int(1, false),
        T_ENUM => Cls::Int(unsafe { clang_Type_getSizeOf(c) }.max(1) as u8, true),
        _ => {
            let s = unsafe { clang_Type_getSizeOf(c) };
            if (1..=8).contains(&s) {
                // con signo: char con signo (13), signed char (14), wchar (15), short (16), int (17), long (18), long long (19)
                Cls::Int(s as u8, (13..=19).contains(&c.kind))
            } else if s == 16 {
                Cls::Other("entero de 128 bits".into())
            } else {
                Cls::Other(csp)
            }
        }
    }
}

/// Puntero a funcion (callback). Ademas de la clase del retorno, se anotan en `recs` las estructuras que el
/// callback recibe o devuelve por puntero (su disposicion tambien debe coincidir entre arquitecturas), y se rechaza
/// el callback cuyos parametros no se pueden pasar con el mapeo de registros.
fn fn_ptr(arch: &mut Arch, f: CXType, recs: &mut Vec<String>) -> Cls {
    let rt = unsafe { clang_getResultType(f) };
    let n = unsafe { clang_getNumArgTypes(f) }.max(0);
    let mut all = vec![classify(arch, rt, recs)];
    for i in 0..n {
        all.push(classify(arch, unsafe { clang_getArgType(f, i as c_uint) }, recs));
    }
    for (i, c) in all.iter().enumerate() {
        let bad = match c {
            Cls::LongDouble | Cls::Complex | Cls::VaList | Cls::HostVtable(_) | Cls::Other(_) => true,
            Cls::Rec(sz, fp) => *sz > 16 || *fp,
            // un callback que recibe un JNIEnv* o un puntero a funcion necesita un trampolin especifico
            Cls::Env | Cls::FnPtr(_) => i > 0,
            _ => false,
        };
        if bad {
            return Cls::Other(format!("callback con {} no reenviable", if i == 0 { "retorno".to_string() } else { format!("parametro {}", i - 1) }));
        }
    }
    // el trampolin host->guest entrega 6 enteros en registros y los siguientes desde la pila del host
    if n > 8 {
        return Cls::Other("callback con mas de 8 parametros".into());
    }
    Cls::FnPtr(ret_char(rt))
}

/// Anade `n` y todo lo alcanzable por punteros desde sus campos.
fn push_rec(arch: &Arch, recs: &mut Vec<String>, n: &str) {
    let mut stack = vec![n.to_string()];
    while let Some(x) = stack.pop() {
        if recs.contains(&x) {
            continue;
        }
        if let Some(r) = arch.recs.get(&x) {
            stack.extend(r.ptr_recs.iter().cloned());
        }
        recs.push(x);
    }
}

fn canon(t: CXType) -> CXType {
    unsafe { clang_getCanonicalType(t) }
}

fn is_fn(t: CXType) -> bool {
    t.kind == T_FN_PROTO || t.kind == T_FN_NOPROTO
}

/// Caracter de shorty de un entero de `n` bytes.
fn int_char(n: u8, signed: bool) -> char {
    match (n, signed) {
        (1, false) => 'Z',
        (1, true) => 'B',
        (2, false) => 'C',
        (2, true) => 'S',
        (4, _) => 'I',
        _ => 'J',
    }
}

/// Clase de un argumento (o retorno) de un metodo de interfaz o de un callback.
fn marg(arch: &mut Arch, t: CXType, recs: &mut Vec<String>) -> MArg {
    let c = canon(t);
    if c.kind == T_POINTER {
        let p = canon(unsafe { clang_getPointeeType(c) });
        if is_fn(p) {
            return match cb_sig(arch, p, recs) {
                Ok(s) => MArg::Cb(s),
                Err(e) => MArg::Bad(e),
            };
        }
        let (mut q, mut levels) = (p, 1);
        while q.kind == T_POINTER && levels < 4 {
            q = canon(unsafe { clang_getPointeeType(q) });
            levels += 1;
        }
        if q.kind == T_RECORD {
            let n = record_info(arch, q);
            let r = arch.recs.get(&n).cloned().unwrap_or_default();
            if r.all_fnptr && r.nfields > 0 && levels >= 2 {
                return match levels {
                    2 => MArg::Itf(n),
                    3 => MArg::ItfOut(n),
                    _ => MArg::Bad(format!("{} niveles de puntero a la interfaz {}", levels, n)),
                };
            }
            let bare = n.trim_start_matches("struct ");
            if levels == 1 && (bare.starts_with("SL") || bare.starts_with("XA")) && (bare.ends_with("DataSource_") || bare.ends_with("DataSink_")) {
                push_rec(arch, recs, &n);
                return MArg::Data;
            }
        }
    }
    match classify(arch, t, recs) {
        Cls::Void => MArg::V('V'),
        Cls::Int(n, s) => MArg::V(int_char(n, s)),
        Cls::Ptr => MArg::V('J'),
        Cls::Float => MArg::V('F'),
        Cls::Double => MArg::V('D'),
        o => MArg::Bad(format!("{:?}", o)),
    }
}

/// Firma de un callback (retorno primero) en el alfabeto de los trampolines con firma: Z B C S I J F D, V (retorno
/// vacio) y O (interfaz que el host pasa y el guest debe ver como su proxy).
fn cb_sig(arch: &mut Arch, f: CXType, recs: &mut Vec<String>) -> Result<String, String> {
    if f.kind == T_FN_NOPROTO || unsafe { clang_isFunctionTypeVariadic(f) } != 0 {
        return Err("callback variadico o sin prototipo".into());
    }
    let n = unsafe { clang_getNumArgTypes(f) }.max(0);
    let mut s = String::new();
    for i in 0..=n {
        let t = if i == 0 { unsafe { clang_getResultType(f) } } else { unsafe { clang_getArgType(f, (i - 1) as c_uint) } };
        match marg(arch, t, recs) {
            MArg::V(c) if i == 0 || c != 'V' => s.push(c),
            MArg::Itf(_) if i > 0 => s.push('O'),
            o => return Err(format!("callback con {} no reenviable ({:?})", if i == 0 { "retorno".to_string() } else { format!("parametro {}", i - 1) }, o)),
        }
    }
    if n > 51 {
        return Err("callback con mas de 51 parametros".into());
    }
    Ok(s)
}

/// Campos puntero a funcion de una estructura (y de las anidadas por valor), con su firma.
fn cb_fields(arch: &mut Arch, t: CXType, base: i64, in_union: bool, sc: &mut Scb, depth: u32) {
    let decl = unsafe { clang_getTypeDeclaration(t) };
    let un = in_union || decl.kind == K_UNION_DECL;
    struct F {
        f: Vec<(i64, CXType)>,
    }
    extern "C" fn fv(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
        let f = unsafe { &mut *(d as *mut F) };
        if c.kind == K_FIELD {
            f.f.push((unsafe { clang_Cursor_getOffsetOfField(c) }, unsafe { clang_getCursorType(c) }));
        }
        1
    }
    let mut f = F { f: Vec::new() };
    unsafe { clang_visitChildren(decl, fv, &mut f as *mut F as *mut c_void) };
    for (bits, ft) in f.f {
        let off = base + bits.max(0) / 8;
        let mut c = canon(ft);
        let mut count = 1i64;
        let mut stride = 0i64;
        if c.kind == T_CONST_ARRAY {
            count = unsafe { clang_getNumElements(c) }.clamp(0, 64);
            c = canon(unsafe { clang_getElementType(c) });
            stride = unsafe { clang_Type_getSizeOf(c) }.max(0);
        }
        for k in 0..count {
            let o = off + k * stride;
            if c.kind == T_POINTER {
                let p = canon(unsafe { clang_getPointeeType(c) });
                if is_fn(p) {
                    let mut r = Vec::new();
                    let sig = cb_sig(arch, p, &mut r);
                    for x in r {
                        if !sc.recs.contains(&x) {
                            sc.recs.push(x);
                        }
                    }
                    sc.union |= un;
                    sc.fields.push((o, sig));
                } else {
                    let mut q = p;
                    while q.kind == T_POINTER {
                        q = canon(unsafe { clang_getPointeeType(q) });
                    }
                    if q.kind == T_RECORD {
                        let n = record_info(arch, q);
                        if arch.recs.get(&n).map_or(false, |r| r.has_fnptr) {
                            sc.deep = true;
                        }
                    }
                }
            } else if c.kind == T_RECORD && depth < 8 {
                cb_fields(arch, c, o, un, sc, depth + 1);
            }
        }
    }
}

/// Estructuras con punteros a funcion que recibe una funcion: por puntero (un nivel) o por valor.
fn struct_cbs(arch: &mut Arch, ft: CXType) -> Vec<Scb> {
    let n = unsafe { clang_getNumArgTypes(ft) }.max(0);
    let mut out = Vec::new();
    for i in 0..n {
        let c = canon(unsafe { clang_getArgType(ft, i as c_uint) });
        let (rec, byval) = if c.kind == T_RECORD {
            (c, true)
        } else if c.kind == T_POINTER && canon(unsafe { clang_getPointeeType(c) }).kind == T_RECORD {
            (canon(unsafe { clang_getPointeeType(c) }), false)
        } else {
            continue;
        };
        let name = record_info(arch, rec);
        let r = arch.recs.get(&name).cloned().unwrap_or_default();
        if !r.has_fnptr {
            continue;
        }
        let mut sc = Scb { ty: name.clone(), arg: i as usize, byval, size: unsafe { clang_Type_getSizeOf(rec) }, ..Default::default() };
        cb_fields(arch, rec, 0, false, &mut sc, 0);
        out.push(sc);
    }
    out
}

/// Interfaz estilo OpenSL ES / OpenMAX AL: estructura `...Itf_` cuyos campos son todos punteros a funcion.
fn collect_itf(arch: &mut Arch, c: CXCursor, name: String) {
    struct F {
        f: Vec<(String, CXType)>,
    }
    extern "C" fn fv(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
        let f = unsafe { &mut *(d as *mut F) };
        if c.kind == K_FIELD {
            f.f.push((cxs(unsafe { clang_getCursorSpelling(c) }), unsafe { clang_getCursorType(c) }));
        }
        1
    }
    let mut f = F { f: Vec::new() };
    unsafe { clang_visitChildren(c, fv, &mut f as *mut F as *mut c_void) };
    let fns: Vec<(String, CXType)> = f
        .f
        .iter()
        .filter_map(|(n, t)| {
            let c = canon(*t);
            if c.kind != T_POINTER {
                return None;
            }
            let p = canon(unsafe { clang_getPointeeType(c) });
            is_fn(p).then(|| (n.clone(), p))
        })
        .collect();
    if fns.is_empty() || fns.len() != f.f.len() {
        return;
    }
    let mut methods = Vec::new();
    for (n, p) in fns {
        let mut recs = Vec::new();
        let ret = marg(arch, unsafe { clang_getResultType(p) }, &mut recs);
        let k = unsafe { clang_getNumArgTypes(p) }.max(0);
        let mut args: Vec<MArg> = (0..k).map(|i| marg(arch, unsafe { clang_getArgType(p, i as c_uint) }, &mut recs)).collect();
        if p.kind == T_FN_NOPROTO || unsafe { clang_isFunctionTypeVariadic(p) } != 0 {
            args.push(MArg::Bad("metodo variadico".into()));
        }
        methods.push(ItfMethod { name: n, ret, args, recs });
    }
    arch.itfs.insert(name, methods);
}

extern "C" fn top_visitor(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
    let ctx = unsafe { &mut *(d as *mut Ctx) };
    if c.kind == K_FUNCTION {
        let name = cxs(unsafe { clang_getCursorSpelling(c) });
        if ctx.arch.funcs.contains_key(&name) {
            return 1;
        }
        let ft = unsafe { clang_getCursorType(c) };
        let mut recs = Vec::new();
        let ret = classify(ctx.arch, unsafe { clang_getResultType(ft) }, &mut recs);
        let n = unsafe { clang_getNumArgTypes(ft) }.max(0);
        let args: Vec<Cls> = (0..n).map(|i| classify(ctx.arch, unsafe { clang_getArgType(ft, i as c_uint) }, &mut recs)).collect();
        let variadic = unsafe { clang_isFunctionTypeVariadic(ft) } != 0 || unsafe { clang_getCanonicalType(ft) }.kind == T_FN_NOPROTO;
        let scb = struct_cbs(ctx.arch, ft);
        if name.starts_with("vk") {
            let va = vk_args(ctx.arch, c, ft);
            ctx.arch.vk_fns.insert(name.clone(), va);
        }
        ctx.arch.funcs.insert(name, Func { ret, args, variadic, recs, header: ctx.header.clone(), scb });
    } else if c.kind == K_STRUCT_DECL && unsafe { clang_isCursorDefinition(c) } != 0 {
        let name = cxs(unsafe { clang_getCursorSpelling(c) });
        if name.ends_with("Itf_") && !ctx.arch.itfs.contains_key(&name) {
            collect_itf(ctx.arch, c, name);
        } else if name.starts_with("Vk") && !ctx.arch.vk_all.contains_key(&name) {
            vk_struct(ctx.arch, c, name);
        }
    } else if c.kind == K_ENUM_DECL && cxs(unsafe { clang_getCursorSpelling(c) }) == "VkStructureType" {
        extern "C" fn ev(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
            let m = unsafe { &mut *(d as *mut BTreeMap<String, (String, i64)>) };
            if c.kind == K_ENUM_CONSTANT {
                let n = cxs(unsafe { clang_getCursorSpelling(c) });
                let v = unsafe { clang_getEnumConstantDeclValue(c) };
                if let Some(k) = n.strip_prefix("VK_STRUCTURE_TYPE_") {
                    m.entry(k.replace('_', "").to_lowercase()).or_insert((n, v));
                }
            }
            1
        }
        unsafe { clang_visitChildren(c, ev, &mut ctx.arch.vk_consts as *mut _ as *mut c_void) };
    }
    1
}

/// Estructura de Vulkan: se anotan sus campos (todas) y, si empieza por `sType`/`pNext` (puede ir en una cadena) o es
/// VkAllocationCallbacks, tambien su disposicion y callbacks en `vk_structs`.
fn vk_struct(arch: &mut Arch, c: CXCursor, name: String) {
    let t = unsafe { clang_getCursorType(c) };
    let rn = record_info(arch, t);
    let layout = arch.recs.get(&rn).map(|r| r.layout.clone()).unwrap_or_default();
    let mut sc = Scb { ty: rn, size: unsafe { clang_Type_getSizeOf(t) }, ..Default::default() };
    cb_fields(arch, t, 0, false, &mut sc, 0);
    let mut rec = VkRec { size: sc.size, layout: layout.clone(), sc: sc.clone(), ..Default::default() };
    vk_members(arch, t, 0, "", &mut rec, 0);
    rec.chained = rec.mems.len() >= 2 && rec.mems[0].name == "sType" && rec.mems[1].name == "pNext" && rec.mems[1].off == 8;
    let chained = rec.chained;
    arch.vk_all.insert(name.clone(), rec);
    if !chained && name != "VkAllocationCallbacks" {
        return;
    }
    arch.vk_structs.insert(name, VkS { size: sc.size, layout, sc });
}

/// Campos de la estructura `t` (desplazados `base`; las anidadas por valor se aplanan con el prefijo `pre`).
fn vk_members(arch: &mut Arch, t: CXType, base: i64, pre: &str, rec: &mut VkRec, depth: u32) {
    let owner = record_info(arch, t).trim_start_matches("struct ").trim_start_matches("union ").to_string();
    struct F {
        f: Vec<(String, i64, CXType)>,
    }
    extern "C" fn fv(c: CXCursor, _p: CXCursor, d: *mut c_void) -> c_int {
        let f = unsafe { &mut *(d as *mut F) };
        if c.kind == K_FIELD {
            f.f.push((cxs(unsafe { clang_getCursorSpelling(c) }), unsafe { clang_Cursor_getOffsetOfField(c) }, unsafe { clang_getCursorType(c) }));
        }
        1
    }
    let mut f = F { f: Vec::new() };
    unsafe { clang_visitChildren(clang_getTypeDeclaration(t), fv, &mut f as *mut F as *mut c_void) };
    let rec_name = |arch: &mut Arch, q: CXType| record_info(arch, q).trim_start_matches("struct ").trim_start_matches("union ").to_string();
    for (fname, bits, ft) in f.f {
        let mut m = VkMem { name: format!("{}{}", pre, fname), owner: owner.clone(), local: fname.clone(), off: base + bits.max(0) / 8, ..Default::default() };
        let c = canon(ft);
        match c.kind {
            T_POINTER => {
                let p = unsafe { clang_getPointeeType(c) };
                let pc = canon(p);
                if pc.kind == T_RECORD {
                    let n = rec_name(arch, pc);
                    let full = record_info(arch, pc);
                    if !n.starts_with("Vk") && arch.recs.get(&full).map_or(false, |r| r.has_fnptr) {
                        rec.foreign_cb = true;
                    }
                    m.ptr = Some((n, unsafe { clang_isConstQualifiedType(p) } != 0, 1));
                } else if pc.kind == T_POINTER {
                    let pp = unsafe { clang_getPointeeType(pc) };
                    let ppc = canon(pp);
                    if ppc.kind == T_RECORD {
                        let n = rec_name(arch, ppc);
                        m.ptr = Some((n, unsafe { clang_isConstQualifiedType(pp) } != 0, 2));
                    }
                }
            }
            T_RECORD if depth < 8 => {
                // anidada por valor: con cabecera de cadena es un nodo propio (`ptr` con 0 niveles: se procesa en su
                // sitio, tambien su cadena); si no, sus campos se aplanan
                let mut sub = VkRec::default();
                vk_members(arch, c, 0, "", &mut sub, depth + 1);
                if sub.mems.len() >= 2 && sub.mems[0].name == "sType" && sub.mems[1].name == "pNext" {
                    m.ptr = Some((rec_name(arch, c), true, 0));
                    rec.mems.push(m);
                } else {
                    let pre = m.name.clone() + ".";
                    vk_members(arch, c, m.off, &pre, rec, depth + 1);
                }
                rec.foreign_cb |= sub.foreign_cb;
                continue;
            }
            T_CONST_ARRAY | T_INCOMPLETE_ARRAY | T_RECORD => {}
            _ => m.int_size = unsafe { clang_Type_getSizeOf(c) },
        }
        rec.mems.push(m);
    }
}

/// Argumentos de una funcion de Vulkan (tipo y nombre del parametro).
fn vk_args(arch: &mut Arch, c: CXCursor, ft: CXType) -> Vec<VkArg> {
    let n = unsafe { clang_getNumArgTypes(ft) }.max(0);
    let named = unsafe { clang_Cursor_getNumArguments(c) } == n;
    let mut out = Vec::new();
    for i in 0..n {
        let t = canon(unsafe { clang_getArgType(ft, i as c_uint) });
        let pname = if named { cxs(unsafe { clang_getCursorSpelling(clang_Cursor_getArgument(c, i as c_uint)) }) } else { String::new() };
        let mut a = VkArg { pname, fp: is_fp_kind(t.kind), ..Default::default() };
        match t.kind {
            T_POINTER => {
                let p = unsafe { clang_getPointeeType(t) };
                let pc = canon(p);
                if pc.kind == T_RECORD {
                    let rn = record_info(arch, pc);
                    a.ptr_rec = Some((rn.trim_start_matches("struct ").to_string(), unsafe { clang_isConstQualifiedType(p) } != 0));
                }
            }
            T_RECORD => a.byval = unsafe { clang_Type_getSizeOf(t) },
            T_FLOAT | T_DOUBLE | T_LONGDOUBLE => {}
            _ => a.int = true,
        }
        out.push(a);
    }
    out
}

// ------------------------------------------------------------------------------------------------
// Registro de Vulkan (vk.xml): que estructura puede ir en la cadena de cual (`structextends`) y cuentas (`len`)
// ------------------------------------------------------------------------------------------------

/// Lo que importa de vk.xml. Las cabeceras en C no dicen que estructuras pueden encadenarse a cuales; sin el registro
/// el generador supone que cualquiera puede ir en cualquier cadena (conservador: nunca deja un callback sin convertir,
/// pero recorre todas las cadenas).
#[derive(Default)]
struct VkXml {
    path: String,
    version: Option<i64>,
    /// estructura -> estructuras en cuya cadena puede ir (`structextends`)
    extends: HashMap<String, Vec<String>>,
    /// estructuras definidas en el registro (con o sin `structextends`)
    structs: BTreeSet<String>,
    /// (estructura, campo) -> `len`
    mem_len: HashMap<(String, String), String>,
    /// funcion -> parametros en orden (nombre, `len`)
    params: HashMap<String, Vec<(String, Option<String>)>>,
    /// alias (estructuras y funciones) -> nombre canonico
    alias: HashMap<String, String>,
}

/// Valor del atributo `a` en la etiqueta `tag` (`<type a="v" ...>`).
fn xml_attr(tag: &str, a: &str) -> Option<String> {
    let k = format!(" {}=\"", a);
    let i = tag.find(&k)? + k.len();
    let j = tag[i..].find('"')?;
    Some(tag[i..i + j].replace("&gt;", ">").replace("&lt;", "<").replace("&amp;", "&"))
}

/// Contenido de `<name>...</name>` en `body`.
fn xml_name(body: &str) -> Option<String> {
    let i = body.find("<name>")? + 6;
    let j = body[i..].find("</name>")?;
    Some(body[i..i + j].trim().to_string())
}

/// El elemento es de la API de Vulkan (vk.xml tambien describe Vulkan SC con `api="vulkansc"`).
fn xml_vulkan(tag: &str) -> bool {
    xml_attr(tag, "api").map_or(true, |a| a.split(',').any(|x| x == "vulkan"))
}

/// Elementos `<el ...>...</el>` (o `<el .../>`) de `s`: (etiqueta de apertura, cuerpo).
fn xml_elems<'a>(s: &'a str, el: &str) -> Vec<(&'a str, &'a str)> {
    let (open, close) = (format!("<{} ", el), format!("</{}>", el));
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = s[at..].find(&open) {
        let i = at + i;
        let Some(te) = s[i..].find('>') else { break };
        let tag = &s[i..i + te + 1];
        if tag.ends_with("/>") {
            out.push((tag, ""));
            at = i + te + 1;
            continue;
        }
        // cierre al mismo nivel (`<member>` lleva `<type>X</type>` dentro de `<type ...>`)
        let (start, mut k, mut depth) = (i + te + 1, i + te + 1, 1);
        let bare = format!("<{}>", el);
        let end = loop {
            let nc = s[k..].find(&close).map(|v| k + v);
            let no = [s[k..].find(&open), s[k..].find(&bare)].into_iter().flatten().min().map(|v| k + v);
            match (nc, no) {
                (None, _) => break None,
                (Some(c), Some(o)) if o < c => {
                    let oe = s[o..].find('>').map_or(o + 1, |v| o + v + 1);
                    if !s[o..oe].ends_with("/>") {
                        depth += 1;
                    }
                    k = oe;
                }
                (Some(c), _) => {
                    depth -= 1;
                    k = c + close.len();
                    if depth == 0 {
                        break Some(c);
                    }
                }
            }
        };
        let Some(ce) = end else { break };
        out.push((tag, &s[start..ce]));
        at = k;
    }
    out
}

/// vk.xml: `SIGTOOL_VK_XML` o, por defecto, el extracto versionado junto a la herramienta (`vk_extracto.xml`, de
/// `vkextract.py`: solo lo que se lee aqui); si faltara, el del sysroot (`share/vulkan/registry/vk.xml`, donde lo
/// instala Vulkan-Headers y lo deja el flujo de CI `sysroot`).
fn load_vk_xml(sysroot: &str) -> Option<VkXml> {
    let cands = [
        std::env::var("SIGTOOL_VK_XML").unwrap_or_default(),
        concat!(env!("CARGO_MANIFEST_DIR"), "/vk_extracto.xml").to_string(),
        format!("{}/share/vulkan/registry/vk.xml", sysroot),
        format!("{}/registry/vk.xml", sysroot),
        format!("{}/vk.xml", sysroot),
    ];
    let path = cands.iter().find(|p| !p.is_empty() && std::path::Path::new(p).is_file())?.clone();
    let s = std::fs::read_to_string(&path).ok()?;
    let mut x = VkXml { path, ..Default::default() };
    if let Some(i) = s.find("VK_HEADER_VERSION</name>") {
        x.version = s[i + 24..].trim_start().split(|c: char| !c.is_ascii_digit()).next().and_then(|v| v.parse().ok());
    }
    let types_sec = s.find("<types").map(|i| &s[i..s[i..].find("</types>").map_or(s.len(), |j| i + j)]).unwrap_or("");
    for (tag, body) in xml_elems(types_sec, "type") {
        let cat = xml_attr(tag, "category").unwrap_or_default();
        if (cat != "struct" && cat != "union") || !xml_vulkan(tag) {
            continue;
        }
        let Some(name) = xml_attr(tag, "name") else { continue };
        if let Some(a) = xml_attr(tag, "alias") {
            x.alias.insert(name, a);
            continue;
        }
        if let Some(e) = xml_attr(tag, "structextends") {
            x.extends.insert(name.clone(), e.split(',').map(|v| v.trim().to_string()).collect());
        }
        for (mt, mb) in xml_elems(body, "member") {
            if !xml_vulkan(mt) {
                continue;
            }
            if let (Some(l), Some(mn)) = (xml_attr(mt, "len"), xml_name(mb)) {
                x.mem_len.insert((name.clone(), mn), l);
            }
        }
        x.structs.insert(name);
    }
    let cmds = s.find("<commands").map(|i| &s[i..s[i..].find("</commands>").map_or(s.len(), |j| i + j)]).unwrap_or("");
    for (tag, body) in xml_elems(cmds, "command") {
        if !xml_vulkan(tag) {
            continue;
        }
        if let (Some(n), Some(a)) = (xml_attr(tag, "name"), xml_attr(tag, "alias")) {
            x.alias.insert(n, a);
            continue;
        }
        let Some(pe) = body.find("</proto>") else { continue };
        let Some(name) = xml_name(&body[..pe]) else { continue };
        let mut ps = Vec::new();
        for (pt, pb) in xml_elems(&body[pe..], "param") {
            if !xml_vulkan(pt) {
                continue;
            }
            ps.push((xml_name(pb).unwrap_or_default(), xml_attr(pt, "len")));
        }
        x.params.insert(name, ps);
    }
    Some(x)
}

impl VkXml {
    fn canon<'a>(&'a self, n: &'a str) -> &'a str {
        self.alias.get(n).map(|s| s.as_str()).unwrap_or(n)
    }
}

fn headers(inc: &str) -> Vec<String> {
    // Cabeceras de la API publica en C. Fuera: C++ (c++/), cabeceras del kernel (no declaran funciones de biblioteca) y
    // las de otras arquitecturas.
    const SKIP_DIRS: &[&str] = &[
        "c++", "linux", "asm-generic", "drm", "mtd", "rdma", "scsi", "sound", "video", "xen", "misc", "aarch64-linux-android", "x86_64-linux-android",
        "arm-linux-androideabi", "i686-linux-android", "riscv64-linux-android", "vk_video", "bits",
    ];
    let mut out = Vec::new();
    let mut stack = vec![String::new()];
    while let Some(rel) = stack.pop() {
        let dir = if rel.is_empty() { inc.to_string() } else { format!("{}/{}", inc, rel) };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            let r = if rel.is_empty() { n.clone() } else { format!("{}/{}", rel, n) };
            if e.path().is_dir() {
                if rel.is_empty() && SKIP_DIRS.contains(&n.as_str()) {
                    continue;
                }
                stack.push(r);
            } else if n.ends_with(".h") {
                // de Vulkan basta la cabecera principal (las demas son de plataformas ajenas o C++)
                if r.starts_with("vulkan/") && r != "vulkan/vulkan.h" {
                    continue;
                }
                out.push(r);
            }
        }
    }
    out.sort();
    out
}

fn parse_arch(sysroot: &str, triple: &str, api: &str) -> Arch {
    let inc = format!("{}/include", sysroot);
    let mut arch = Arch::default();
    let idx = unsafe { clang_createIndex(0, 0) };
    let target = format!("{}{}", triple, api);
    let args: Vec<CString> = [
        "-target", &target, "-x", "c", "-std=gnu17", "-D_GNU_SOURCE", "-DGL_GLEXT_PROTOTYPES", "-DEGL_EGLEXT_PROTOTYPES", "-D__ANDROID_UNAVAILABLE_SYMBOLS_ARE_WEAK__", "-DVK_USE_PLATFORM_ANDROID_KHR", "-nostdinc",
        "-isystem", &format!("{}/{}", inc, triple), "-isystem", &inc, "-isystem", &resource_include(), "-w",
    ]
    .iter()
    .map(|s| CString::new(*s).unwrap())
    .collect();
    let mut list: Vec<(String, String)> = headers(&inc).into_iter().map(|h| (format!("{}/{}", inc, h), h)).collect();
    // firmas suplementarias: funciones exportadas que las cabeceras publicas no declaran (FORTIFY, __cxa_*...)
    if let Ok(x) = std::env::var("SIGTOOL_EXTRA") {
        list.push((x, "extra.h".into()));
    }
    for (full, h) in list {
        let path = CString::new(full).unwrap();
        // las cabeceras de extensiones necesitan la principal de su API incluida antes
        let prelude = if h.starts_with("EGL/") && h != "EGL/egl.h" && h != "EGL/eglplatform.h" {
            Some("EGL/egl.h")
        } else if h == "GLES/glext.h" {
            Some("GLES/gl.h")
        } else if h == "GLES2/gl2ext.h" {
            Some("GLES2/gl2.h")
        } else {
            None
        };
        let mut args = args.clone();
        if let Some(p) = prelude {
            args.push(CString::new("-include").unwrap());
            args.push(CString::new(p).unwrap());
        }
        let argv: Vec<*const c_char> = args.iter().map(|s| s.as_ptr()).collect();
        // 0x200 = SkipFunctionBodies (las funciones inline de las cabeceras no hace falta analizarlas)
        let tu = unsafe { clang_parseTranslationUnit(idx, path.as_ptr(), argv.as_ptr(), argv.len() as c_int, std::ptr::null_mut(), 0, 0x40) };
        if tu.is_null() {
            arch.failed.push(h);
            continue;
        }
        let mut errors = 0;
        for i in 0..unsafe { clang_getNumDiagnostics(tu) } {
            let d = unsafe { clang_getDiagnostic(tu, i) };
            if unsafe { clang_getDiagnosticSeverity(d) } >= 3 {
                errors += 1;
            }
            unsafe { clang_disposeDiagnostic(d) };
        }
        if errors > 0 {
            // una cabecera que no se analiza limpia no aporta firmas (las declaraciones recuperadas no son fiables)
            arch.failed.push(h.clone());
            unsafe { clang_disposeTranslationUnit(tu) };
            continue;
        }
        arch.parsed += 1;
        let mut ctx = Ctx { arch: &mut arch, header: h };
        unsafe { clang_visitChildren(clang_getTranslationUnitCursor(tu), top_visitor, &mut ctx as *mut Ctx as *mut c_void) };
        unsafe { clang_disposeTranslationUnit(tu) };
    }
    arch
}

/// Directorio de cabeceras propias del compilador (stddef.h, stdarg.h...).
fn resource_include() -> String {
    if let Ok(p) = std::env::var("SIGTOOL_RESOURCE_INCLUDE") {
        return p;
    }
    let out = std::process::Command::new("clang").arg("-print-resource-dir").output().expect("clang no encontrado");
    format!("{}/include", String::from_utf8_lossy(&out.stdout).trim())
}

/// Simbolos exportados (funciones y datos) de una biblioteca-stub del NDK, y todos (tambien los de C++) con su version
/// (vacia si no tiene).
fn exports(path: &str) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
    let out = std::process::Command::new("llvm-readelf").args(["--dyn-syms", "-W", path]).output().expect("llvm-readelf no encontrado");
    let (mut f, mut d, mut all) = (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() < 8 || c[6] == "UND" {
            continue;
        }
        let mut it = c[7].splitn(2, '@');
        let name = it.next().unwrap_or("").to_string();
        let ver = it.next().unwrap_or("").trim_start_matches('@').to_string();
        match c[3] {
            "FUNC" | "IFUNC" => drop(f.insert(name.clone())),
            "OBJECT" | "TLS" => drop(d.insert(name.clone())),
            _ => continue,
        }
        all.insert((name, ver));
    }
    (f.into_iter().collect(), d.into_iter().collect(), all.into_iter().collect())
}

// ------------------------------------------------------------------------------------------------
// Veredicto
// ------------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    /// reenvio directo (con las conversiones tipadas indicadas)
    Direct { env: Vec<u8>, cbs: Vec<(u8, char)>, narrow: Vec<(u8, u8)>, ret_fn: bool, variadic: bool, struct_cb: bool },
    /// ABI incompatible con el mapeo de registros
    Unsafe(String),
    /// necesita implementacion a mano o proxy
    Manual(String),
    /// exportada pero sin declaracion en las cabeceras analizadas
    NoSig,
}

/// Las estructuras alcanzables por puntero (`recs`) tienen la misma disposicion en las dos arquitecturas? Ok(true)
/// si alguna contiene punteros a funcion.
fn recs_check(recs: &[String], ga: &Arch, ha: &Arch) -> Result<bool, String> {
    let mut struct_cb = false;
    for r in recs {
        let (gi, hi) = (ga.recs.get(r), ha.recs.get(r));
        // el mismo nombre es un escalar en una arquitectura y una estructura (o escalar) en la otra: basta el tamano
        let size_of = |a: &Arch| -> Option<i64> {
            a.scalars.get(r).copied().or_else(|| a.recs.get(r).and_then(|i| i.layout.strip_prefix("size=").and_then(|x| x.split(' ').next()).and_then(|x| x.parse().ok())))
        };
        if ga.scalars.contains_key(r) || ha.scalars.contains_key(r) {
            let plain = |a: &Arch| a.recs.get(r).map_or(true, |i| !i.has_fnptr && !i.layout.contains("longdouble") && !i.layout.contains("ptr"));
            if size_of(ga).is_some() && size_of(ga) == size_of(ha) && plain(ga) && plain(ha) {
                continue;
            }
            return Err(format!("el tipo {} difiere entre arm64 y x86-64", r));
        }
        match (gi, hi) {
            (Some(a), Some(b)) => {
                if (a.layout == "opaco") != (b.layout == "opaco") || a.layout == "en curso" || b.layout == "en curso" {
                    return Err(format!("la estructura {} no se pudo comparar entre arquitecturas", r));
                }
                if a.layout != b.layout {
                    return Err(format!("la estructura {} difiere entre arm64 y x86-64", r));
                }
                struct_cb |= a.has_fnptr;
            }
            _ => return Err(format!("la estructura {} no existe en las dos arquitecturas", r)),
        }
    }
    Ok(struct_cb)
}

fn verdict(name: &str, g: Option<&Func>, h: Option<&Func>, ga: &Arch, ha: &Arch) -> Verdict {
    let (Some(g), Some(h)) = (g, h) else { return Verdict::NoSig };
    let _ = name;
    // el signo de un entero no cambia donde viaja (wchar_t y char tienen distinto signo en cada arquitectura)
    let norm = |c: &Cls| match c {
        Cls::Int(n, _) => Cls::Int(*n, false),
        o => o.clone(),
    };
    let same = |a: &[Cls], b: &[Cls]| a.len() == b.len() && a.iter().zip(b).all(|(x, y)| norm(x) == norm(y));
    if !same(&g.args, &h.args) || norm(&g.ret) != norm(&h.ret) || g.variadic != h.variadic {
        return Verdict::Manual("la firma difiere entre arm64 y x86-64".into());
    }
    let all = || g.args.iter().chain(std::iter::once(&g.ret));
    for (i, c) in g.args.iter().enumerate() {
        match c {
            Cls::LongDouble => return Verdict::Unsafe("argumento long double".into()),
            Cls::Complex => return Verdict::Unsafe("argumento complejo".into()),
            Cls::VaList => return Verdict::Unsafe("argumento va_list".into()),
            Cls::Rec(sz, fp) if *sz > 16 || *fp => return Verdict::Unsafe(format!("estructura por valor en el argumento {} ({} bytes{})", i, sz, if *fp { ", con coma flotante" } else { "" })),
            Cls::Other(s) => return Verdict::Unsafe(format!("argumento {}: {}", i, s)),
            _ => {}
        }
    }
    match &g.ret {
        Cls::LongDouble => return Verdict::Unsafe("retorno long double".into()),
        Cls::Complex => return Verdict::Unsafe("retorno complejo".into()),
        Cls::VaList => return Verdict::Unsafe("retorno va_list".into()),
        Cls::Rec(sz, fp) if *sz > 16 || *fp => return Verdict::Unsafe(format!("retorno de estructura ({} bytes{})", sz, if *fp { ", con coma flotante" } else { "" })),
        Cls::Other(s) => return Verdict::Unsafe(format!("retorno: {}", s)),
        _ => {}
    }
    for c in all() {
        if let Cls::HostVtable(n) = c {
            return Verdict::Manual(format!("objeto con tabla de funciones del host ({})", n));
        }
    }
    if g.args.len() > 8 && g.args.iter().filter(|c| matches!(c, Cls::Float | Cls::Double)).count() > 0 && g.args.iter().filter(|c| !matches!(c, Cls::Float | Cls::Double)).count() > 6 {
        // enteros en pila mezclados con coma flotante: el orden en pila puede diferir
        return Verdict::Manual("mas de 6 enteros y argumentos de coma flotante".into());
    }
    if g.args.iter().filter(|c| matches!(c, Cls::Float | Cls::Double)).count() > 8 {
        return Verdict::Manual("mas de 8 argumentos de coma flotante".into());
    }
    // Registros enteros: x86-64 solo tiene 6 (arm64, 8). Una estructura por valor de 9..16 bytes ocupa dos y debe
    // caber entera en los 6 primeros; un argumento estrecho (bool/char/short) debe estar en registro o en x6/x7
    // para poder extenderlo.
    let mut slot = 0usize;
    let mut narrow: Vec<(u8, u8)> = Vec::new();
    // la extension usa el signo que espera el HOST (quien recibe el valor)
    for c in &h.args {
        match c {
            Cls::Float | Cls::Double => {}
            Cls::Rec(sz, _) => {
                let n = ((*sz as usize) + 7) / 8;
                if slot < 6 && slot + n > 6 || (slot >= 6 && n > 1) {
                    return Verdict::Unsafe("estructura por valor repartida entre registros y pila".into());
                }
                slot += n.max(1);
            }
            Cls::Int(n, signed) if *n < 4 => {
                if slot > 7 {
                    return Verdict::Manual("argumento estrecho (bool/char/short) en la pila".into());
                }
                // 1 = u8, 2 = i8, 3 = u16, 4 = i16
                narrow.push((slot as u8, match (*n, *signed) { (1, false) => 1, (1, true) => 2, (_, false) => 3, _ => 4 }));
                slot += 1;
            }
            _ => slot += 1,
        }
    }
    // estructuras alcanzables por puntero: la disposicion debe coincidir
    let struct_cb = match recs_check(&g.recs, ga, ha) {
        Ok(s) => s,
        Err(e) => return Verdict::Manual(e),
    };
    // indices en registros ENTEROS (los argumentos de coma flotante no consumen x0..x7)
    let mut slot = 0usize;
    let (mut env, mut cbs): (Vec<u8>, Vec<(u8, char)>) = (Vec::new(), Vec::new());
    for c in &g.args {
        match c {
            Cls::Float | Cls::Double => continue,
            Cls::Env => env.push(slot.min(255) as u8),
            Cls::FnPtr(r) => cbs.push((slot.min(255) as u8, *r)),
            Cls::Rec(sz, _) => slot += (((*sz as usize) + 7) / 8).max(1) - 1,
            _ => {}
        }
        slot += 1;
    }
    if env.iter().any(|i| *i > 5) || cbs.iter().any(|(i, _)| *i > 7) {
        return Verdict::Manual("puntero a funcion o JNIEnv en un argumento de pila".into());
    }
    if matches!(g.ret, Cls::Env) {
        return Verdict::Manual("devuelve un JNIEnv/JavaVM".into());
    }
    Verdict::Direct { env, cbs, narrow, ret_fn: matches!(g.ret, Cls::FnPtr(_)), variadic: g.variadic, struct_cb }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 4 {
        eprintln!("uso: sigtool <sysroot-ndk> <salida sigs_gen.rs> <informe.md>");
        std::process::exit(2);
    }
    let (sysroot, out_rs, out_md) = (&a[1], &a[2], &a[3]);
    let api = std::fs::read_to_string(format!("{}/lib/aarch64-linux-android/API", sysroot)).unwrap_or_default().trim().to_string();
    let ndk = std::fs::read_to_string(format!("{}/source.properties", sysroot))
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("Pkg.Revision"))
        .map(|l| l.split('=').nth(1).unwrap_or("").trim().to_string())
        .unwrap_or_default();
    eprintln!("NDK {} API {}", ndk, api);
    let guest = parse_arch(sysroot, "aarch64-linux-android", &api);
    let host = parse_arch(sysroot, "x86_64-linux-android", &api);
    eprintln!("arm64: {} cabeceras, {} con errores, {} funciones, {} estructuras", guest.parsed, guest.failed.len(), guest.funcs.len(), guest.recs.len());
    eprintln!("x86-64: {} cabeceras, {} con errores, {} funciones, {} estructuras", host.parsed, host.failed.len(), host.funcs.len(), host.recs.len());

    // bibliotecas del NDK (C++ fuera)
    let libdir = format!("{}/lib/aarch64-linux-android", sysroot);
    let mut libs: Vec<String> = std::fs::read_dir(&libdir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".so") && n != "libc++.so").collect();
    libs.sort();
    // (nombre, biblioteca, veredicto); una funcion exportada por varias bibliotecas se atribuye a la primera
    let mut rows: BTreeMap<String, (String, Verdict)> = BTreeMap::new();
    let mut data: BTreeMap<String, String> = BTreeMap::new();
    // (nombre, biblioteca) -> version: todas las bibliotecas que exportan cada simbolo (el enlazador busca en cada una)
    let mut all_exports: BTreeMap<(String, String), String> = BTreeMap::new();
    for lib in &libs {
        let (fs, ds, all) = exports(&format!("{}/{}", libdir, lib));
        for (n, v) in all {
            all_exports.insert((n, lib.clone()), v);
        }
        for f in fs {
            if f.starts_with("_Z") {
                continue; // C++: fuera de la frontera
            }
            let v = verdict(&f, guest.funcs.get(&f), host.funcs.get(&f), &guest, &host);
            rows.entry(f).or_insert((lib.clone(), v));
        }
        for d in ds {
            data.entry(d).or_insert(lib.clone());
        }
    }

    // ---- sigs_gen.rs ----
    let mut rs = String::new();
    let _ = writeln!(rs, "// @generated por tools/sigtool a partir del NDK {} (API {}). NO EDITAR: regenerar con la herramienta.", ndk, api);
    let _ = writeln!(rs, "// Cada funcion exportada por las bibliotecas en C del NDK, con su clase para la frontera host/guest.");
    let _ = writeln!(rs, "use crate::sigs::{{CbField, Itf, Method, Sig, StructCb, VkArg, VkFn, VkPtr, VkSType, VkType, A, K, VK_NONE}};\n");
    let _ = writeln!(rs, "pub const NDK_REVISION: &str = \"{}\";\npub const NDK_API: &str = \"{}\";\n", ndk, api);
    let _ = writeln!(rs, "/// Ordenada por nombre (busqueda binaria).\npub static SIGS: &[Sig] = &[");
    for (name, (lib, v)) in &rows {
        let k = match v {
            Verdict::Direct { env, cbs, narrow, ret_fn, variadic, struct_cb } => {
                let envm: u8 = env.iter().fold(0, |m, i| m | (1 << i));
                let cb = cbs.iter().map(|(i, r)| format!("({}, b'{}')", i, r)).collect::<Vec<_>>().join(", ");
                let nw = narrow.iter().map(|(i, k)| format!("({}, {})", i, k)).collect::<Vec<_>>().join(", ");
                format!("K::Direct {{ env: {}, cbs: &[{}], narrow: &[{}], ret_fn: {}, variadic: {}, struct_cb: {} }}", envm, cb, nw, ret_fn, variadic, struct_cb)
            }
            Verdict::Unsafe(r) => format!("K::Unsafe({:?})", r),
            Verdict::Manual(r) => format!("K::Manual({:?})", r),
            Verdict::NoSig => "K::NoSig".to_string(),
        };
        let _ = writeln!(rs, "    Sig {{ name: {:?}, lib: {:?}, k: {} }},", name, lib, k);
    }
    let _ = writeln!(rs, "];\n\n/// Simbolos de datos exportados (el guest los lee directamente). Ordenada por nombre.\npub static DATA: &[(&str, &str)] = &[");
    for (n, l) in &data {
        let _ = writeln!(rs, "    ({:?}, {:?}),", n, l);
    }
    let _ = writeln!(rs, "];");
    let _ = writeln!(
        rs,
        "\n/// Cada simbolo (funcion o dato) exportado por cada biblioteca del NDK, con su version (vacia: la\n/// biblioteca no tiene versiones). Un simbolo exportado por varias aparece una vez por biblioteca. Ordenada por\n/// nombre y biblioteca.\npub static EXPORTS: &[(&str, &str, &str)] = &["
    );
    for ((n, l), v) in &all_exports {
        let _ = writeln!(rs, "    ({:?}, {:?}, {:?}),", n, l, v);
    }
    let _ = writeln!(rs, "];");

    // ---- interfaces (tablas de funciones del host: proxy) ----
    let itf_names: Vec<String> = guest.itfs.keys().filter(|k| host.itfs.contains_key(*k)).cloned().collect();
    let itf_idx = |n: &str| itf_names.iter().position(|x| x == n.trim_start_matches("struct "));
    let normc = |c: char| match c {
        'B' => 'Z',
        'S' => 'C',
        x => x,
    };
    let norm = |a: &MArg| match a {
        MArg::V(c) => MArg::V(normc(*c)),
        MArg::Cb(s) => MArg::Cb(s.chars().map(normc).collect()),
        o => o.clone(),
    };
    let mut itf_report: Vec<(String, usize, Vec<String>)> = Vec::new();
    let _ = writeln!(rs, "\n/// Interfaces estilo OpenSL ES / OpenMAX AL (tablas de punteros a funcion del host): metodos en el orden de la\n/// estructura. El primer argumento es la propia interfaz. Ordenada por nombre.\npub static ITFS: &[Itf] = &[");
    for name in &itf_names {
        let (gm, hm) = (&guest.itfs[name], &host.itfs[name]);
        let prefix = &name[..2];
        let iid = format!("{}_IID_{}", prefix, name[2..name.len() - 4].to_uppercase());
        let iid = if data.contains_key(&iid) { iid } else { String::new() };
        let _ = writeln!(rs, "    Itf {{ name: {:?}, iid: {:?}, methods: &[", name, iid);
        let mut bad_list = Vec::new();
        for (k, g) in gm.iter().enumerate() {
            let h = hm.get(k);
            let mut bad = String::new();
            match h {
                Some(h) if h.name == g.name && h.args.len() == g.args.len() && norm(&h.ret) == norm(&g.ret) && h.args.iter().zip(&g.args).all(|(a, b)| norm(a) == norm(b)) => {}
                _ => bad = "la firma difiere entre arm64 y x86-64".into(),
            }
            if bad.is_empty() {
                for a in g.args.iter().chain(std::iter::once(&g.ret)) {
                    if let MArg::Bad(e) = a {
                        bad = e.clone();
                        break;
                    }
                }
            }
            if bad.is_empty() {
                if let Err(e) = recs_check(&g.recs, &guest, &host) {
                    bad = e;
                }
            }
            let ret = match &g.ret {
                MArg::V(c) => *c,
                _ => {
                    if bad.is_empty() {
                        bad = "retorno no escalar".into();
                    }
                    'J'
                }
            };
            let h = h.unwrap_or(g);
            let mut args = Vec::new();
            for (i, a) in h.args.iter().enumerate() {
                args.push(match a {
                    MArg::V(c) => format!("A::V(b'{}')", c),
                    MArg::Itf(n) | MArg::ItfOut(n) => match itf_idx(n) {
                        Some(x) => format!("A::{}({})", if matches!(a, MArg::Itf(_)) { "Itf" } else { "ItfOut" }, x),
                        None => {
                            if bad.is_empty() {
                                bad = format!("interfaz desconocida {}", n);
                            }
                            "A::V(b'J')".into()
                        }
                    },
                    MArg::Cb(_) => match &g.args[i] {
                        MArg::Cb(gs) => format!("A::Cb(b\"{}\")", gs),
                        _ => "A::V(b'J')".into(),
                    },
                    MArg::Data => "A::Data".into(),
                    MArg::Bad(_) => "A::V(b'J')".into(),
                });
            }
            if !bad.is_empty() {
                bad_list.push(format!("{} ({})", g.name, bad));
            }
            let _ = writeln!(rs, "        Method {{ name: {:?}, ret: b'{}', args: &[{}], bad: {:?} }},", g.name, ret, args.join(", "), bad);
        }
        let _ = writeln!(rs, "    ] }},");
        itf_report.push((name.clone(), gm.len(), bad_list));
    }
    let _ = writeln!(rs, "];");

    // ---- estructuras con punteros a funcion recibidas por las funciones ----
    let mut scb_report: Vec<(String, String)> = Vec::new();
    let _ = writeln!(rs, "\n/// Estructuras con punteros a funcion que recibe una funcion (por puntero o por valor): desplazamiento y firma de\n/// cada callback. `note` vacia = se puede convertir campo a campo. Ordenada por funcion.\npub static STRUCT_CBS: &[StructCb] = &[");
    for name in rows.keys() {
        let (Some(g), Some(h)) = (guest.funcs.get(name), host.funcs.get(name)) else { continue };
        for (k, sc) in g.scb.iter().enumerate() {
            let offs = |x: &Scb| (x.size, x.byval, x.fields.iter().map(|f| (f.0, f.1.is_ok())).collect::<Vec<_>>());
            let mut note = String::new();
            if h.scb.get(k).map(offs) != Some(offs(sc)) {
                note = "la estructura difiere entre arm64 y x86-64".into();
            } else if sc.union {
                note = "puntero a funcion dentro de una union".into();
            } else if sc.deep {
                note = "punteros a funcion alcanzables por otro puntero".into();
            } else if let Some(e) = sc.fields.iter().find_map(|f| f.1.as_ref().err()) {
                note = e.clone();
            } else if let Err(e) = recs_check(&sc.recs, &guest, &host) {
                note = e;
            }
            // registro entero (AAPCS64) del argumento
            let mut reg = 0usize;
            for c in &g.args[..sc.arg] {
                reg += match c {
                    Cls::Float | Cls::Double => 0,
                    Cls::Rec(sz, _) if *sz > 16 => 1,
                    Cls::Rec(sz, _) => ((*sz as usize + 7) / 8).max(1),
                    _ => 1,
                };
            }
            let fields: Vec<String> = sc.fields.iter().map(|(o, r)| format!("CbField {{ off: {}, sig: b\"{}\" }}", o, r.as_deref().unwrap_or(""))).collect();
            let _ = writeln!(
                rs,
                "    StructCb {{ func: {:?}, ty: {:?}, arg: {}, reg: {}, byval: {}, size: {}, fields: &[{}], note: {:?} }},",
                name,
                sc.ty,
                sc.arg,
                reg,
                sc.byval,
                sc.size,
                fields.join(", "),
                note
            );
            scb_report.push((name.clone(), if note.is_empty() { format!("argumento {}: {} callbacks", sc.arg, sc.fields.len()) } else { format!("argumento {}: {}", sc.arg, note) }));
        }
    }
    let _ = writeln!(rs, "];");

    // ---- Vulkan: VkAllocationCallbacks, estructuras de las cadenas pNext y argumentos que las llevan ----
    let mut vk_report: Vec<String> = Vec::new();
    let fields_of = |sc: &Scb| sc.fields.iter().map(|(o, r)| format!("CbField {{ off: {}, sig: b\"{}\" }}", o, r.as_deref().unwrap_or(""))).collect::<Vec<_>>().join(", ");
    let vk_note = |n: &str| -> String {
        let (Some(g), Some(h)) = (guest.vk_structs.get(n), host.vk_structs.get(n)) else { return "la estructura no existe en las dos arquitecturas".into() };
        let offs = |x: &Scb| x.fields.iter().map(|f| (f.0, f.1.clone())).collect::<Vec<_>>();
        if g.layout != h.layout || g.size != h.size || offs(&g.sc) != offs(&h.sc) {
            return "la estructura difiere entre arm64 y x86-64".into();
        }
        if g.sc.union {
            return "puntero a funcion dentro de una union".into();
        }
        if g.sc.deep {
            return "punteros a funcion alcanzables por otro puntero".into();
        }
        if let Some(e) = g.sc.fields.iter().find_map(|f| f.1.as_ref().err()) {
            return e.clone();
        }
        if let Err(e) = recs_check(&g.sc.recs, &guest, &host) {
            return e;
        }
        String::new()
    };
    let alloc = guest.vk_structs.get("VkAllocationCallbacks").cloned().unwrap_or_default();
    let an = vk_note("VkAllocationCallbacks");
    assert!(an.is_empty() && alloc.sc.fields.len() == 5, "VkAllocationCallbacks: {}", an);
    let _ = writeln!(rs, "\n/// Vulkan: campos de VkAllocationCallbacks ({} bytes) con la firma de cada callback.\npub const VK_ALLOC_SIZE: u32 = {};\npub static VK_ALLOC: &[CbField] = &[{}];", alloc.size, alloc.size, fields_of(&alloc.sc));
    // ---- que estructuras pueden llevar callbacks (directa o transitivamente) ----
    // Un argumento de entrada X "lleva callbacks" si: X tiene punteros a funcion; o algun campo puntero const de X
    // apunta a estructuras que los llevan (anidadas, arreglos); o X tiene cadena pNext y alguna estructura que puede ir
    // en ella (vk.xml: `structextends` X) los lleva como nodo (sus campos o sus punteros; no su propia cadena: lo que
    // sigue a un nodo en la cadena de X tambien tiene que extender X). Punto fijo minimo (las estructuras pueden
    // apuntarse en ciclo).
    let xml = load_vk_xml(sysroot);
    let header_ver = std::fs::read_to_string(format!("{}/include/vulkan/vulkan_core.h", sysroot))
        .ok()
        .and_then(|h| h.lines().find_map(|l| l.strip_prefix("#define VK_HEADER_VERSION ").and_then(|v| v.trim().parse::<i64>().ok())));
    let vk_mode = match &xml {
        Some(x) if x.version == header_ver => format!("`structextends` y `len` de vk.xml (version {})", x.version.unwrap_or(0)),
        Some(x) => format!(
            "vk.xml de otra version ({:?}, cabeceras {:?}): `structextends` del registro; las estructuras que no esten en el se suponen encadenables a cualquiera",
            x.version, header_ver
        ),
        None => "SIN vk.xml: las cabeceras en C no dicen que estructura puede ir en la cadena de cual; se supone que cualquiera en cualquiera (conservador: se recorren todas las cadenas)".to_string(),
    };
    eprintln!("vulkan: {}", vk_mode);
    let all = &guest.vk_all;
    let is_cbs = |n: &str| -> bool {
        let r = &all[n];
        n != "VkAllocationCallbacks" && (!r.sc.fields.is_empty() || r.foreign_cb)
    };
    // bases(Y): estructuras en cuya cadena puede ir Y (None: cualquiera)
    let bases = |y: &str| -> Option<Vec<String>> {
        let x = xml.as_ref()?;
        if !x.structs.contains(y) {
            return None;
        }
        Some(x.extends.get(y).map(|v| v.iter().map(|b| x.canon(b).to_string()).collect()).unwrap_or_default())
    };
    let chained: Vec<&String> = all.iter().filter(|(_, r)| r.chained).map(|(n, _)| n).collect();
    let extenders: BTreeMap<&String, Vec<&String>> = all
        .iter()
        .filter(|(_, r)| r.chained)
        .map(|(x, _)| (x, chained.iter().copied().filter(|y| *y != x && bases(y).map_or(true, |b| b.contains(x))).collect()))
        .collect();
    let ptr_targets = |n: &str| -> Vec<(usize, String)> {
        all[n].mems.iter().enumerate().filter_map(|(i, m)| m.ptr.as_ref().filter(|p| p.1 && all.contains_key(&p.0)).map(|p| (i, p.0.clone()))).collect()
    };
    let mut reach_n: BTreeMap<&String, bool> = all.keys().map(|k| (k, is_cbs(k))).collect();
    let mut reach_s = reach_n.clone();
    loop {
        let mut changed = false;
        for n in all.keys() {
            let rn = reach_n[n] || ptr_targets(n).iter().any(|(_, z)| reach_s[z]);
            let r_s = rn || extenders.get(n).map_or(false, |ys| ys.iter().any(|y| reach_n[*y]));
            if rn != reach_n[n] || r_s != reach_s[n] {
                changed = true;
            }
            reach_n.insert(n, rn);
            reach_s.insert(n, r_s);
        }
        if !changed {
            break;
        }
    }
    let types: Vec<&String> = all.keys().filter(|k| reach_s[*k]).collect();
    let tyidx: BTreeMap<&String, usize> = types.iter().enumerate().map(|(i, n)| (*n, i)).collect();
    assert!(types.len() < 0xffff, "demasiadas estructuras de Vulkan con callbacks");
    // motivo por el que los callbacks propios de una estructura no se convierten (vacio: se convierten)
    let vk_note2 = |n: &str| -> String {
        let (Some(g), Some(h)) = (guest.vk_all.get(n), host.vk_all.get(n)) else { return "la estructura no existe en las dos arquitecturas".into() };
        let offs = |x: &Scb| x.fields.iter().map(|f| (f.0, f.1.clone())).collect::<Vec<_>>();
        if g.layout != h.layout || g.size != h.size || offs(&g.sc) != offs(&h.sc) {
            return "la estructura difiere entre arm64 y x86-64".into();
        }
        if g.sc.union {
            return "puntero a funcion dentro de una union".into();
        }
        if g.foreign_cb {
            return "punteros a funcion alcanzables por un puntero a una estructura ajena a Vulkan".into();
        }
        if let Some(e) = g.sc.fields.iter().find_map(|f| f.1.as_ref().err()) {
            return e.clone();
        }
        if let Err(e) = recs_check(&g.sc.recs, &guest, &host) {
            return e;
        }
        String::new()
    };
    let _ = writeln!(rs, "\n/// Vulkan: estructuras que pueden llevar callbacks, directa o transitivamente ({}). `chain`: su cadena\n/// `pNext` puede llevar estructuras con callbacks; `fields`: sus punteros a funcion (`note` no vacia: no se convierten);\n/// `ptrs`: campos que apuntan a otras de esta tabla. Ordenada por nombre; `ty` de las demas tablas es el indice.\npub static VK_TYPES: &[VkType] = &[", vk_mode.replace('`', ""));
    for n in &types {
        let r = &all[*n];
        let note = if is_cbs(n) { vk_note2(n) } else { String::new() };
        let layout_ok = host.vk_all.get(*n).map_or(false, |h| h.layout == r.layout && h.size == r.size);
        let chain = r.chained && extenders.get(*n).map_or(false, |ys| ys.iter().any(|y| reach_n[*y]));
        let mut ptrs = Vec::new();
        for (i, z) in ptr_targets(n) {
            if !reach_s[&z] {
                continue;
            }
            let m = &r.mems[i];
            let deref = m.ptr.as_ref().unwrap().2;
            if !layout_ok {
                vk_report.push(format!("estructura {}: {} no se recorre (la estructura difiere entre arm64 y x86-64)", n, m.name));
                continue;
            }
            if deref == 0 {
                // anidada por valor con cadena: se procesa en su sitio dentro de la copia
                ptrs.push(format!("VkPtr {{ off: {}, count_off: {}, count_size: 0, deref: 0, ty: {} }}", m.off, u32::MAX, tyidx[&z]));
                continue;
            }
            // cuenta: `len` del registro (un campo hermano entero); sin registro, el campo anterior `...Count`
            // (en una anidada aplanada, el `len` es de la anidada y nombra un campo suyo)
            let len = match &xml {
                Some(x) if x.structs.contains(x.canon(&m.owner)) => x.mem_len.get(&(x.canon(&m.owner).to_string(), m.local.clone())).cloned(),
                _ => (i > 0 && r.mems[i - 1].int_size > 0 && r.mems[i - 1].owner == m.owner && r.mems[i - 1].local.ends_with("Count")).then(|| r.mems[i - 1].local.clone()),
            };
            let prefix = &m.name[..m.name.len() - m.local.len()];
            let (coff, csize) = match &len {
                None => (u32::MAX, 0),
                Some(l) => {
                    let first = format!("{}{}", prefix, l.split(',').next().unwrap_or(""));
                    match r.mems.iter().find(|c| c.name == first && (c.int_size == 4 || c.int_size == 8)) {
                        Some(c) => (c.off as u32, c.int_size),
                        None => {
                            vk_report.push(format!("estructura {}: {} con cuenta '{}' no reconocida: no se recorre", n, m.name, l));
                            continue;
                        }
                    }
                }
            };
            ptrs.push(format!("VkPtr {{ off: {}, count_off: {}, count_size: {}, deref: {}, ty: {} }}", m.off, coff, csize, deref, tyidx[&z]));
        }
        let f = if is_cbs(n) { fields_of(&r.sc) } else { String::new() };
        if is_cbs(n) {
            vk_report.push(format!("estructura {}: {}", n, if note.is_empty() { format!("{} callbacks", r.sc.fields.len()) } else { note.clone() }));
        }
        let _ = writeln!(rs, "    VkType {{ name: {:?}, size: {}, chain: {}, fields: &[{}], note: {:?}, ptrs: &[{}] }},", n, r.size, chain, f, note, ptrs.join(", "));
    }
    let _ = writeln!(rs, "];");
    // sType de cada estructura: la constante VK_STRUCTURE_TYPE_* cuyo nombre normalizado coincide con el de la estructura
    let mut st: BTreeMap<i64, (String, i64)> = BTreeMap::new();
    let mut sin_stype = Vec::new();
    for (n, v) in &guest.vk_structs {
        if n == "VkAllocationCallbacks" {
            continue;
        }
        let key = n.trim_start_matches("Vk").to_lowercase();
        match guest.vk_consts.get(&key) {
            Some((_, val)) if !st.contains_key(val) => {
                st.insert(*val, (n.clone(), v.size));
            }
            Some(_) => sin_stype.push(format!("{} (sType repetido)", n)),
            None => sin_stype.push(n.clone()),
        }
    }
    let _ = writeln!(rs, "\n/// Vulkan: estructuras que pueden ir en una cadena `pNext` (cabecera sType/pNext), por sType: tamano (para copiarlas)\n/// y, si llevan callbacks como nodo de una cadena, su indice en `VK_TYPES` (`VK_NONE` si no). Ordenada por sType.\npub static VK_STYPES: &[VkSType] = &[");
    let mut cb_stypes = Vec::new();
    for (val, (n, size)) in &st {
        let ty = if reach_n[n] {
            cb_stypes.push(*val);
            tyidx[n].to_string()
        } else {
            "VK_NONE".to_string()
        };
        let _ = writeln!(rs, "    VkSType {{ stype: {}, size: {}, name: {:?}, ty: {} }},", val, size, n, ty);
    }
    let _ = writeln!(rs, "];\n\n/// sType de las estructuras que llevan callbacks como nodo de una cadena (solo se copia si la cadena lleva alguna).\npub static VK_CB_STYPES: &[u32] = &[{}];", cb_stypes.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", "));
    // argumentos de cada funcion: VkAllocationCallbacks y punteros const a estructuras que pueden llevar callbacks (con
    // su cuenta si son un arreglo: `len` del registro o, sin el, el parametro anterior entero `...Count`)
    let _ = writeln!(rs, "\n/// Vulkan: funciones (tambien las que solo da vkGet*ProcAddr) con `pAllocator` o con estructuras de entrada que\n/// pueden llevar callbacks: registro AAPCS64 de cada una (255 = ninguno). Ordenada por nombre.\npub static VK_FNS: &[VkFn] = &[");
    let mut nfns = 0;
    let mut nchain = 0;
    for (name, ga) in &guest.vk_fns {
        if host.vk_fns.get(name) != Some(ga) {
            vk_report.push(format!("funcion {}: los argumentos difieren entre arm64 y x86-64", name));
            continue;
        }
        let mut reg = 0usize;
        let mut regs = Vec::new();
        for a in ga {
            regs.push(reg);
            if a.fp {
                continue;
            }
            reg += if a.byval > 16 { 1 } else if a.byval > 0 { ((a.byval as usize) + 7) / 8 } else { 1 };
        }
        let xparams = xml.as_ref().and_then(|x| x.params.get(x.canon(name)));
        let mut alloc_reg = 255usize;
        let mut args = Vec::new();
        for (i, a) in ga.iter().enumerate() {
            let Some((rn, is_const)) = &a.ptr_rec else { continue };
            if rn == "VkAllocationCallbacks" {
                if regs[i] > 7 {
                    vk_report.push(format!("funcion {}: pAllocator en la pila", name));
                } else {
                    alloc_reg = regs[i];
                }
                continue;
            }
            if !*is_const || !reach_s.get(rn).copied().unwrap_or(false) {
                continue;
            }
            let count = match xparams {
                Some(ps) => match ps.iter().find(|p| p.0 == a.pname).and_then(|p| p.1.clone()) {
                    None => Some(255),
                    Some(l) => ga.iter().position(|b| b.pname == l && b.int).map(|k| regs[k]),
                },
                None => Some(if i > 0 && ga[i - 1].int && ga[i - 1].pname.ends_with("Count") { regs[i - 1] } else { 255 }),
            };
            let Some(count) = count else {
                vk_report.push(format!("funcion {}: {} ({}) con una cuenta no reconocida: no se recorre", name, a.pname, rn));
                continue;
            };
            if regs[i] > 7 || (count != 255 && count > 7) {
                vk_report.push(format!("funcion {}: {} ({}) en la pila: no se recorre", name, a.pname, rn));
                continue;
            }
            vk_report.push(format!("funcion {}: {} ({}) puede llevar callbacks", name, a.pname, rn));
            args.push(format!("VkArg {{ reg: {}, count: {}, ty: {} }}", regs[i], count, tyidx[rn]));
        }
        if alloc_reg == 255 && args.is_empty() {
            continue;
        }
        nfns += 1;
        nchain += !args.is_empty() as usize;
        let _ = writeln!(rs, "    VkFn {{ name: {:?}, alloc: {}, args: &[{}] }},", name, alloc_reg, args.join(", "));
    }
    let _ = writeln!(rs, "];");
    eprintln!("vulkan: {} estructuras con sType ({} sin constante), {} que pueden llevar callbacks ({} como nodo de cadena), {} funciones con pAllocator, {} con estructuras que pueden llevarlos", st.len(), sin_stype.len(), types.len(), cb_stypes.len(), nfns, nchain);
    std::fs::write(out_rs, rs).unwrap();

    // ---- informe ----
    let mut md = String::new();
    let _ = writeln!(md, "# Firmas de la frontera (generado)\n\nGenerado por `tools/sigtool` a partir del NDK {} (API {}). No editar a mano.\n", ndk, api);
    let _ = writeln!(md, "Cabeceras analizadas: {} (arm64) / {} (x86-64). Con errores de analisis: {} / {}.\n", guest.parsed, host.parsed, guest.failed.len(), host.failed.len());
    let _ = writeln!(md, "| Biblioteca | Funciones | Directas | de ellas con callback | con JNIEnv | ABI incompatible | A mano / proxy | Sin firma |\n|---|---|---|---|---|---|---|---|");
    let mut tot = [0usize; 7];
    for lib in &libs {
        let mut c = [0usize; 7];
        for (_, (l, v)) in rows.iter() {
            if l != lib {
                continue;
            }
            c[0] += 1;
            match v {
                Verdict::Direct { env, cbs, .. } => {
                    c[1] += 1;
                    c[2] += (!cbs.is_empty()) as usize;
                    c[3] += (!env.is_empty()) as usize;
                }
                Verdict::Unsafe(_) => c[4] += 1,
                Verdict::Manual(_) => c[5] += 1,
                Verdict::NoSig => c[6] += 1,
            }
        }
        for i in 0..7 {
            tot[i] += c[i];
        }
        let _ = writeln!(md, "| {} | {} | {} | {} | {} | {} | {} | {} |", lib, c[0], c[1], c[2], c[3], c[4], c[5], c[6]);
    }
    let _ = writeln!(md, "| **Total** | {} | {} | {} | {} | {} | {} | {} |\n", tot[0], tot[1], tot[2], tot[3], tot[4], tot[5], tot[6]);
    for (title, pick) in [("ABI incompatible (no se reenvian)", 0), ("A mano o proxy", 1), ("Exportadas sin declaracion en las cabeceras", 2)] {
        let _ = writeln!(md, "## {}\n", title);
        let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (n, (l, v)) in &rows {
            match (pick, v) {
                (0, Verdict::Unsafe(r)) | (1, Verdict::Manual(r)) => by.entry(format!("{} — {}", l, r)).or_default().push(n.clone()),
                (2, Verdict::NoSig) => by.entry(l.clone()).or_default().push(n.clone()),
                _ => {}
            }
        }
        for (k, v) in by {
            let _ = writeln!(md, "- **{}** ({}): {}", k, v.len(), v.join(", "));
        }
        let _ = writeln!(md);
    }
    let _ = writeln!(md, "## Interfaces con tabla de funciones (proxy)\n\nEl guest recibe objetos proxy cuyas tablas son slots con estas firmas (`src/proxy.rs`). Metodos sin proxy: responden \"no soportado\".\n");
    let _ = writeln!(md, "| Interfaz | Metodos | Sin proxy |\n|---|---|---|");
    for (n, k, bad) in &itf_report {
        let _ = writeln!(md, "| {} | {} | {} |", n, k, if bad.is_empty() { "-".to_string() } else { bad.join("; ") });
    }
    let _ = writeln!(md, "\n## Estructuras con punteros a funcion\n");
    for (f, r) in &scb_report {
        let _ = writeln!(md, "- **{}**: {}", f, r);
    }
    let _ = writeln!(md);
    let _ = writeln!(md, "## Vulkan: pAllocator y cadenas pNext\n\nOrigen de las relaciones entre estructuras: {}.\n\n{} estructuras con sType; {} pueden llevar callbacks directa o transitivamente (`VK_TYPES`); {} funciones en `VK_FNS`, {} de ellas con estructuras de entrada que pueden llevarlos (las demas solo `pAllocator`). `VkAllocationCallbacks`: {} callbacks.\n", vk_mode, st.len(), types.len(), nfns, nchain, alloc.sc.fields.len());
    for r in &vk_report {
        let _ = writeln!(md, "- {}", r);
    }
    if !sin_stype.is_empty() {
        let _ = writeln!(md, "\nEstructuras con cabecera sType/pNext sin constante `VK_STRUCTURE_TYPE_*` reconocida (no se copian en una cadena): {}", sin_stype.join(", "));
    }
    let _ = writeln!(md);
    let _ = writeln!(md, "## Cabeceras con errores de analisis\n\narm64: {}\n\nx86-64: {}", guest.failed.join(", "), host.failed.join(", "));
    std::fs::write(out_md, md).unwrap();
    eprintln!("funciones: {} directas: {} abi: {} manual: {} sin firma: {}", tot[0], tot[1], tot[4], tot[5], tot[6]);
    let _ = (c_long::default(), T_BOOL);
}
