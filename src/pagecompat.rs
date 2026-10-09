//! Modo de compatibilidad de 16 KiB por aplicacion, como lo decide Android 16 en un dispositivo de 16 KiB.
//!
//! En el dispositivo la decision es de PackageManager al instalar (`ScanPackageUtils`): si el manifiesto declara
//! `android:pageSizeCompat` en `<application>` (enabled = 32, disabled = 64), ese valor; si no, la comprobacion de
//! alineacion de las bibliotecas nativas del ABI de la app (`NativeLibraryHelper.checkApkAlignment`: con
//! `extractNativeLibs` las extraidas en `nativeLibraryDir`, sin el las de dentro del APK, que deben ir sin comprimir; una
//! con algun PT_LOAD de `p_align` 0x1000 da `ELF_NOT_ALIGNED`). `PackageSetting.isPageSizeAppCompatEnabled` lo activa
//! con `ELF_NOT_ALIGNED`, `MANIFEST_OVERRIDE_ENABLED` o `SETTINGS_OVERRIDE_ENABLED`, salvo que el manifiesto o los
//! ajustes lo desactiven. Zygote pasa la decision al proceso (`android_set_16kb_appcompat_mode`) y el enlazador la usa
//! para cada biblioteca con `p_align` de 4 KiB (`ElfReader::Read`), igual que la propiedad
//! `bionic.linker.16kb.app_compat.enabled`.
//!
//! Aqui se calcula una vez por proceso, la primera vez que hace falta (una biblioteca de 4 KiB con paginas de 16 KiB):
//! los APK y `nativeLibraryDir` salen de `ApplicationInfo` por JNI (`ndkcb::app_info`), de `HEDDLE_APK` (lista separada
//! por ':', la primera es la base) y `HEDDLE_NATIVE_LIB_DIR` fuera de Android, o de la ruta de la biblioteca
//! (`.../<app>/lib/<isa>/libx.so` o `.../<app>/base.apk!/lib/...`). El manifiesto binario se lee del APK base (deflate
//! propio, `inflate`) y sus atributos se reconocen por el identificador de recurso, como `TypedArray`.

use std::sync::OnceLock;

/// `ApplicationInfo.PAGE_SIZE_APP_COMPAT_FLAG_*`
const UNCOMPRESSED_LIBS_NOT_ALIGNED: i32 = 1 << 1;
const ELF_NOT_ALIGNED: i32 = 1 << 2;
const SETTINGS_OVERRIDE_ENABLED: i32 = 1 << 3;
const SETTINGS_OVERRIDE_DISABLED: i32 = 1 << 4;
const MANIFEST_OVERRIDE_ENABLED: i32 = 1 << 5;
const MANIFEST_OVERRIDE_DISABLED: i32 = 1 << 6;

/// `android.R.attr.pageSizeCompat` (API 36) y `android.R.attr.extractNativeLibs` (API 23).
const ATTR_PAGE_SIZE_COMPAT: u32 = 0x0101_06ab;
const ATTR_EXTRACT_NATIVE_LIBS: u32 = 0x0101_04ea;

/// ABI de las bibliotecas que se comprueban (la app se ejecuta como arm64).
const ABI_DIR: &str = "lib/arm64-v8a/";

/// Archivos de la aplicacion: APK base, divisiones y directorio de las bibliotecas extraidas.
#[derive(Debug, Clone)]
pub struct App {
    pub apks: Vec<String>,
    pub native_dir: Option<String>,
}

/// Decision del proceso (`android_set_16kb_appcompat_mode`). `settings`: Some(true/false) = ajuste del usuario
/// (`SETTINGS_OVERRIDE_*`). `lib_path`: ruta real de la biblioteca que se carga (para localizar la app si no hay
/// otra fuente). Sin aplicacion localizable, false (y se vuelve a intentar con la siguiente biblioteca).
pub fn process_mode(settings: Option<bool>, lib_path: &str) -> bool {
    static MODE: OnceLock<bool> = OnceLock::new();
    if let Some(&m) = MODE.get() {
        return m;
    }
    let Some(app) = app_files(lib_path) else { return false };
    let (flags, why) = flags_of(&app);
    let flags = flags
        | match settings {
            Some(true) => SETTINGS_OVERRIDE_ENABLED,
            Some(false) => SETTINGS_OVERRIDE_DISABLED,
            None => 0,
        };
    let on = enabled(flags);
    crate::bridge::alog(&format!("paginas de 16 KiB: modo de compatibilidad {} ({}; banderas {:#x}; {:?})", if on { "activado" } else { "desactivado" }, why, flags, app.apks));
    *MODE.get_or_init(|| on)
}

/// `PackageSetting.isPageSizeAppCompatEnabled`
pub fn enabled(flags: i32) -> bool {
    if flags & (MANIFEST_OVERRIDE_DISABLED | SETTINGS_OVERRIDE_DISABLED) != 0 {
        return false;
    }
    flags & (ELF_NOT_ALIGNED | MANIFEST_OVERRIDE_ENABLED | SETTINGS_OVERRIDE_ENABLED) != 0
}

/// Banderas de PackageManager para la app (`ScanPackageUtils`): las del manifiesto si declara un valor; si no, las de
/// la comprobacion de alineacion (un error deja UNDEFINED).
pub fn flags_of(app: &App) -> (i32, &'static str) {
    let (psc, extract) = app.apks.first().and_then(|b| manifest_of(b)).unwrap_or((None, true));
    if let Some(v) = psc.filter(|&v| v > 0) {
        return (v, "android:pageSizeCompat del manifiesto");
    }
    match check_alignment(app, extract) {
        Some(m) => (m, "alineacion de las bibliotecas"),
        None => (0, "error al comprobar la alineacion"),
    }
}

/// `NativeLibraryHelper.checkAlignmentForCompatMode` sobre todos los APK (None = PAGE_SIZE_APP_COMPAT_FLAG_ERROR).
fn check_alignment(app: &App, extract: bool) -> Option<i32> {
    let mut mode = 0;
    for apk in &app.apks {
        let mut err = false;
        crate::elf::zip_scan(apk, |name, lho, _size, stored| {
            let Ok(name) = std::str::from_utf8(name) else { return true };
            let Some(file) = name.strip_prefix(ABI_DIR) else { return true };
            if file.contains('/') || !file.ends_with(".so") {
                return true;
            }
            let r = if extract {
                // checkExtractedLibAlignment: la extraida en nativeLibraryDir
                match &app.native_dir {
                    Some(d) => load_segment_flags(&format!("{}/{}", d, file), 0),
                    None => None,
                }
            } else if !stored {
                None // comprimida sin extraer: no se podria abrir desde el APK
            } else {
                crate::elf::zip_data_off(apk, lho).and_then(|off| {
                    let un = if off % 16384 != 0 { UNCOMPRESSED_LIBS_NOT_ALIGNED } else { 0 };
                    load_segment_flags(apk, off).map(|m| m | un)
                })
            };
            match r {
                Some(m) => {
                    mode |= m;
                    true
                }
                None => {
                    err = true;
                    false
                }
            }
        })?;
        if err {
            return None;
        }
    }
    Some(mode)
}

/// `checkLoadSegmentAlignment`: ELF_NOT_ALIGNED si algun PT_LOAD tiene `p_align` 0x1000.
pub fn load_segment_flags(path: &str, off: u64) -> Option<i32> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut eh = [0u8; 64];
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(&mut eh).ok()?;
    if eh[0..4] != *b"\x7fELF" || eh[4] != 2 {
        return None;
    }
    let phoff = u64::from_le_bytes(eh[0x20..0x28].try_into().unwrap());
    let (phentsize, phnum) = (u16::from_le_bytes([eh[0x36], eh[0x37]]) as u64, u16::from_le_bytes([eh[0x38], eh[0x39]]) as u64);
    if phentsize < 56 || phnum > 4096 {
        return None;
    }
    let mut ph = vec![0u8; (phentsize * phnum) as usize];
    f.seek(SeekFrom::Start(off + phoff)).ok()?;
    f.read_exact(&mut ph).ok()?;
    let not_aligned = ph.chunks(phentsize as usize).any(|p| u32::from_le_bytes(p[0..4].try_into().unwrap()) == 1 && u64::from_le_bytes(p[0x30..0x38].try_into().unwrap()) == 0x1000);
    Some(if not_aligned { ELF_NOT_ALIGNED } else { 0 })
}

/// (`android:pageSizeCompat`, `android:extractNativeLibs`) del `<application>` del manifiesto del APK base.
pub fn manifest_of(apk: &str) -> Option<(Option<i32>, bool)> {
    let xml = crate::elf::zip_read(apk, "AndroidManifest.xml", 16 << 20)?;
    Some(manifest_attrs(&xml))
}

/// Atributos de `<application>` de un XML binario de Android (ResXMLTree): pool de cadenas, mapa de recursos (el
/// identificador de cada nombre de atributo) y elementos. Por defecto `extractNativeLibs` es true.
pub fn manifest_attrs(x: &[u8]) -> (Option<i32>, bool) {
    let (mut psc, mut extract) = (None, true);
    let u16at = |o: usize| x.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32at = |o: usize| x.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if u16at(0) != Some(0x0003) {
        return (psc, extract);
    }
    let Some(hs) = u16at(2) else { return (psc, extract) };
    let mut pool: Option<usize> = None;
    let mut resmap: &[u8] = &[];
    let mut o = hs;
    while let (Some(t), Some(chs), Some(sz)) = (u16at(o), u16at(o + 2), u32at(o + 4)) {
        let sz = sz as usize;
        if sz < 8 || o + sz > x.len() {
            break;
        }
        match t {
            0x0001 => pool = Some(o),
            0x0180 => resmap = &x[o + chs..o + sz],
            0x0102 => {
                let b = o + chs;
                let name = u32at(b + 4).and_then(|i| pool.and_then(|p| pool_str(x, p, i)));
                if name.as_deref() == Some("application") {
                    let (Some(start), Some(asz), Some(n)) = (u16at(b + 8), u16at(b + 10), u16at(b + 12)) else { break };
                    for k in 0..n {
                        let a = b + start + k * asz;
                        let (Some(ni), Some(dt), Some(data)) = (u32at(a + 4), x.get(a + 15).copied(), u32at(a + 16)) else { break };
                        let id = resmap.get(ni as usize * 4..ni as usize * 4 + 4).map(|v| u32::from_le_bytes([v[0], v[1], v[2], v[3]]));
                        match id {
                            // TypedArray.getInt: tipos enteros (TYPE_FIRST_INT..TYPE_LAST_INT)
                            Some(ATTR_PAGE_SIZE_COMPAT) if (0x10..=0x1f).contains(&dt) => psc = Some(data as i32),
                            Some(ATTR_EXTRACT_NATIVE_LIBS) if (0x10..=0x1f).contains(&dt) => extract = data != 0,
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        o += sz;
    }
    (psc, extract)
}

/// Cadena `i` del pool (ResStringPool) que empieza en `p`: UTF-8 (bandera 0x100) o UTF-16.
fn pool_str(x: &[u8], p: usize, i: u32) -> Option<String> {
    let u16at = |o: usize| x.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u32at = |o: usize| x.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let hs = u16at(p + 2)?;
    let (count, flags, start) = (u32at(p + 8)?, u32at(p + 16)?, u32at(p + 20)?);
    if i as usize >= count {
        return None;
    }
    let s = p + start + u32at(p + hs + 4 * i as usize)?;
    if flags & 0x100 != 0 {
        // longitud en caracteres y en bytes, cada una de 1 o 2 bytes
        let mut o = s;
        let skip = |o: &mut usize| -> Option<usize> {
            let b = *x.get(*o)? as usize;
            *o += 1;
            if b & 0x80 != 0 {
                let c = *x.get(*o)? as usize;
                *o += 1;
                return Some(((b & 0x7f) << 8) | c);
            }
            Some(b)
        };
        skip(&mut o)?;
        let n = skip(&mut o)?;
        Some(String::from_utf8_lossy(x.get(o..o + n)?).into_owned())
    } else {
        let mut n = u16at(s)?;
        let mut o = s + 2;
        if n & 0x8000 != 0 {
            n = ((n & 0x7fff) << 16) | u16at(o)?;
            o += 2;
        }
        let units: Vec<u16> = x.get(o..o + 2 * n)?.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        Some(String::from_utf16_lossy(&units))
    }
}

/// Archivos de la app (ver la cabecera del modulo).
fn app_files(lib_path: &str) -> Option<App> {
    if let Ok(v) = std::env::var("HEDDLE_APK") {
        let apks: Vec<String> = v.split(':').filter(|s| !s.is_empty()).map(String::from).collect();
        if !apks.is_empty() {
            return Some(App { apks, native_dir: std::env::var("HEDDLE_NATIVE_LIB_DIR").ok() });
        }
    }
    #[cfg(target_os = "android")]
    if let Some((mut apks, splits, native_dir)) = crate::ndkcb::app_info() {
        apks.extend(splits);
        return Some(App { apks, native_dir });
    }
    app_from_lib_path(lib_path)
}

/// Directorio de instalacion a partir de la ruta de una biblioteca suya: `<app>/base.apk!/lib/...`,
/// `<app>/split_x.apk!/lib/...` o `<app>/lib/<isa>/libx.so` (las extraidas). Los APK son `base.apk` y los `split_*.apk`
/// del directorio.
pub fn app_from_lib_path(p: &str) -> Option<App> {
    let (dir, native_dir) = if let Some((apk, _)) = p.split_once("!/") {
        (apk.rsplit_once('/')?.0.to_string(), None)
    } else {
        let libdir = p.rsplit_once('/')?.0;
        let (parent, _isa) = libdir.rsplit_once('/')?;
        let (dir, lib) = parent.rsplit_once('/')?;
        if lib != "lib" {
            return None;
        }
        (dir.to_string(), Some(libdir.to_string()))
    };
    let base = format!("{}/base.apk", dir);
    if !std::path::Path::new(&base).is_file() {
        return None;
    }
    let mut splits: Vec<String> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("split_") && n.ends_with(".apk"))
        .map(|n| format!("{}/{}", dir, n))
        .collect();
    splits.sort();
    let mut apks = vec![base];
    apks.extend(splits);
    Some(App { apks, native_dir })
}

// ---------------------------------------------------------------------------------------------
// inflate (RFC 1951), para el manifiesto comprimido del APK
// ---------------------------------------------------------------------------------------------

struct Bits<'a> {
    s: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl Bits<'_> {
    fn need(&mut self, n: u32) -> Option<u32> {
        while self.cnt < n {
            let b = *self.s.get(self.pos)? as u64;
            self.pos += 1;
            self.buf |= b << self.cnt;
            self.cnt += 8;
        }
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        Some(v)
    }
}

/// Codigo de Huffman canonico: cuantos codigos hay de cada longitud y los simbolos ordenados por codigo.
struct Huff {
    count: [u16; 16],
    sym: Vec<u16>,
}

impl Huff {
    fn new(lens: &[u8]) -> Option<Huff> {
        let mut count = [0u16; 16];
        for &l in lens {
            count[l as usize] += 1;
        }
        // sobresuscrito: invalido (incompleto se admite, como zlib para un solo codigo de distancia)
        let mut left: i32 = 1;
        for &c in &count[1..] {
            left = (left << 1) - c as i32;
            if left < 0 {
                return None;
            }
        }
        let mut offs = [0u16; 16];
        for l in 1..15 {
            offs[l + 1] = offs[l] + count[l];
        }
        let mut sym = vec![0u16; lens.len()];
        for (s, &l) in lens.iter().enumerate() {
            if l != 0 {
                sym[offs[l as usize] as usize] = s as u16;
                offs[l as usize] += 1;
            }
        }
        Some(Huff { count, sym })
    }
    fn decode(&self, b: &mut Bits) -> Option<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.need(1)? as i32;
            let c = self.count[len] as i32;
            if code - c < first {
                return self.sym.get((index + code - first) as usize).copied();
            }
            index += c;
            first += c;
            first <<= 1;
            code <<= 1;
        }
        None
    }
}

const LBASE: [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEXT: [u8; 29] = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DBASE: [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DEXT: [u8; 30] = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

/// Descomprime un flujo deflate crudo (sin cabecera zlib, como en un ZIP). None si es invalido o pasa de `max` bytes.
pub fn inflate(src: &[u8], max: usize) -> Option<Vec<u8>> {
    let mut b = Bits { s: src, pos: 0, buf: 0, cnt: 0 };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = b.need(1)?;
        match b.need(2)? {
            0 => {
                // almacenado: alinear a byte, LEN y NLEN
                b.buf = 0;
                b.cnt = 0;
                let h = src.get(b.pos..b.pos + 4)?;
                let (len, nlen) = (u16::from_le_bytes([h[0], h[1]]), u16::from_le_bytes([h[2], h[3]]));
                if len != !nlen {
                    return None;
                }
                b.pos += 4;
                out.extend_from_slice(src.get(b.pos..b.pos + len as usize)?);
                b.pos += len as usize;
            }
            t @ (1 | 2) => {
                let (lit, dist) = if t == 1 {
                    let mut l = [8u8; 288];
                    l[144..256].fill(9);
                    l[256..280].fill(7);
                    (Huff::new(&l)?, Huff::new(&[5u8; 30])?)
                } else {
                    let (nlen, ndist, ncode) = (b.need(5)? as usize + 257, b.need(5)? as usize + 1, b.need(4)? as usize + 4);
                    if nlen > 286 || ndist > 30 {
                        return None;
                    }
                    const ORD: [usize; 19] = [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];
                    let mut cl = [0u8; 19];
                    for &k in &ORD[..ncode] {
                        cl[k] = b.need(3)? as u8;
                    }
                    let ch = Huff::new(&cl)?;
                    let mut lens = vec![0u8; nlen + ndist];
                    let mut i = 0;
                    while i < nlen + ndist {
                        let s = ch.decode(&mut b)?;
                        let (v, rep) = match s {
                            0..=15 => (s as u8, 1),
                            16 => (*lens.get(i.checked_sub(1)?)?, 3 + b.need(2)? as usize),
                            17 => (0, 3 + b.need(3)? as usize),
                            _ => (0, 11 + b.need(7)? as usize),
                        };
                        if i + rep > lens.len() {
                            return None;
                        }
                        lens[i..i + rep].fill(v);
                        i += rep;
                    }
                    if lens[256] == 0 {
                        return None;
                    }
                    (Huff::new(&lens[..nlen])?, Huff::new(&lens[nlen..])?)
                };
                loop {
                    let s = lit.decode(&mut b)? as usize;
                    if s < 256 {
                        out.push(s as u8);
                    } else if s == 256 {
                        break;
                    } else {
                        let k = s - 257;
                        let len = *LBASE.get(k)? as usize + b.need(*LEXT.get(k)? as u32)? as usize;
                        let d = dist.decode(&mut b)? as usize;
                        let dd = *DBASE.get(d)? as usize + b.need(*DEXT.get(d)? as u32)? as usize;
                        if dd > out.len() {
                            return None;
                        }
                        let from = out.len() - dd;
                        for j in 0..len {
                            let c = out[from + j];
                            out.push(c);
                        }
                    }
                    if out.len() > max {
                        return None;
                    }
                }
            }
            _ => return None,
        }
        if out.len() > max {
            return None;
        }
        if last == 1 {
            return Some(out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_como_package_setting() {
        assert!(!enabled(0));
        assert!(enabled(ELF_NOT_ALIGNED));
        assert!(enabled(ELF_NOT_ALIGNED | UNCOMPRESSED_LIBS_NOT_ALIGNED));
        assert!(!enabled(UNCOMPRESSED_LIBS_NOT_ALIGNED));
        assert!(enabled(MANIFEST_OVERRIDE_ENABLED));
        assert!(!enabled(MANIFEST_OVERRIDE_DISABLED | ELF_NOT_ALIGNED));
        assert!(!enabled(MANIFEST_OVERRIDE_ENABLED | SETTINGS_OVERRIDE_DISABLED));
        assert!(enabled(SETTINGS_OVERRIDE_ENABLED));
    }

    #[test]
    fn inflate_bloques_almacenados_fijos_y_dinamicos() {
        // generados con zlib (compressobj(level, DEFLATED, -15)): almacenado, Huffman fijo y dinamico
        let stored = [0x01u8, 0x05, 0x00, 0xfa, 0xff, b'h', b'o', b'l', b'a', b'!'];
        assert_eq!(inflate(&stored, 100).unwrap(), b"hola!");
        // "abcabcabcabcabcabc" con Huffman fijo (Z_FIXED)
        let fixed = [75u8, 76, 74, 78, 68, 69, 0];
        assert_eq!(inflate(&fixed, 100).unwrap(), b"abcabcabcabcabcabc");
        assert!(inflate(&fixed, 5).is_none());
        assert!(inflate(&[0x07], 100).is_none());
        // bloques dinamicos y almacenados: zlib del host (compress2 = cabecera zlib de 2 bytes + deflate + adler32)
        #[link(name = "z")]
        extern "C" {
            fn compress2(dst: *mut u8, dlen: *mut std::os::raw::c_ulong, src: *const u8, slen: std::os::raw::c_ulong, level: i32) -> i32;
        }
        let mut x: u32 = 12345;
        let mut rnd = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let words = [&b"application "[..], b"pageSizeCompat ", b"android ", b"lib/arm64-v8a/", b"x", b"\0\x01"];
        let text: Vec<u8> = (0..20000).flat_map(|_| words[(rnd() % 6) as usize].iter().copied()).collect();
        let noise: Vec<u8> = (0..70000).map(|_| rnd() as u8).collect();
        for data in [&text, &noise] {
            for level in [0, 1, 6, 9] {
                let mut dst = vec![0u8; data.len() + data.len() / 100 + 64];
                let mut dlen = dst.len() as std::os::raw::c_ulong;
                assert_eq!(unsafe { compress2(dst.as_mut_ptr(), &mut dlen, data.as_ptr(), data.len() as _, level) }, 0);
                let raw = &dst[2..dlen as usize - 4];
                assert_eq!(inflate(raw, data.len()).as_deref(), Some(&data[..]), "nivel {}", level);
                assert!(inflate(&raw[..raw.len() / 2], data.len()).is_none());
            }
        }
    }
}
