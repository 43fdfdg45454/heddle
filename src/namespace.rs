//! Espacios de nombres del enlazador, como `android_namespace_t` de bionic (linker_namespaces.cpp, `create_namespace`
//! y `link_namespaces` de linker.cpp).
//!
//! Cada biblioteca guest pertenece a un espacio primario (donde se cargo) y quiza a otros secundarios (heredada al
//! crear un espacio `SHARED`, o del grupo compartido del padre). Un espacio tiene rutas de busqueda (`ld_paths`, luego
//! DT_RUNPATH, luego `default_paths`), rutas permitidas si es `isolated`, y enlaces hacia otros espacios con la lista de
//! sonames que se pueden tomar de ellos. ART los crea por NativeBridge (createNamespace/linkNamespaces/
//! getExportedNamespace/initAnonymousNamespace) uno por classloader; fuera de ART solo existe el espacio "default".
//!
//! Las bibliotecas del sistema que sirve el puente (HLE: libc, libm, liblog...) viven en los espacios "de sistema": el
//! espacio por defecto y los exportados que pide ART ("system", "sphal"...). Un espacio de una app las alcanza por sus
//! enlaces, con la lista de bibliotecas publicas que da ART, como en un dispositivo.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::RwLock;

/// Tipos de `android_create_namespace` (android/dlext.h).
pub const TYPE_ISOLATED: u64 = 1;
pub const TYPE_SHARED: u64 = 2;
pub const TYPE_ALSO_USED_AS_ANONYMOUS: u64 = 0x1000_0000;
/// ART lo pide con targetSdkVersion < 24: las bibliotecas de la lista de excepciones de bionic se buscan tambien en
/// el espacio por defecto (`is_exempt_lib`).
pub const TYPE_EXEMPT_LIST_ENABLED: u64 = 0x0800_0000;

/// Cota de espacios de nombres (ART crea uno por classloader con bibliotecas nativas y unos pocos exportados); llena,
/// `createNamespace` falla.
pub const NS_CAP: usize = 1024;
/// El espacio por defecto (indice 0): el de las cargas fuera de ART y el que exporta ART como "default".
pub const DEFAULT: usize = 0;

#[derive(Clone)]
pub struct Link {
    pub to: usize,
    pub sonames: Vec<String>,
    pub all: bool,
}

impl Link {
    /// `android_namespace_link_t::is_accessible`
    pub fn accessible(&self, soname: &str) -> bool {
        self.all || self.sonames.iter().any(|s| s == soname)
    }
}

#[derive(Clone)]
pub struct Namespace {
    pub name: String,
    pub isolated: bool,
    pub ld_paths: Vec<String>,
    pub default_paths: Vec<String>,
    pub permitted: Vec<String>,
    pub links: Vec<Link>,
    /// contiene las bibliotecas del sistema (HLE)
    pub system: bool,
    /// TYPE_EXEMPT_LIST_ENABLED
    pub exempt_list: bool,
    /// lo devolvio getExportedNamespace con este nombre (se reutiliza)
    exported: bool,
}

/// Todos los espacios (el indice es su identificador; bionic tampoco los borra nunca). Cota: `NS_CAP`.
static NSS: RwLock<Vec<Namespace>> = RwLock::new(Vec::new());
/// Espacio anonimo: el de un `dlopen` cuyo llamador no es una biblioteca guest (bionic `g_anonymous_namespace`).
static ANON: AtomicUsize = AtomicUsize::new(DEFAULT);
static ANON_SET: AtomicBool = AtomicBool::new(false);

fn ensure(v: &mut Vec<Namespace>) {
    if v.is_empty() {
        v.push(Namespace {
            name: "default".into(),
            isolated: false,
            ld_paths: Vec::new(),
            default_paths: Vec::new(),
            permitted: Vec::new(),
            links: Vec::new(),
            system: true,
            exempt_list: false,
            exported: true,
        });
    }
}

fn parse_path(p: &str) -> Vec<String> {
    p.split(':').filter(|s| !s.is_empty()).map(|s| s.trim_end_matches('/').to_string()).filter(|s| !s.is_empty()).collect()
}

/// Copia del espacio `id` (None si no existe).
pub fn get(id: usize) -> Option<Namespace> {
    let mut g = crate::monitor::write(&NSS);
    ensure(&mut g);
    g.get(id).cloned()
}

/// El espacio `id` contiene las bibliotecas del sistema? (sin copiar el espacio: se consulta en cada dlsym)
pub fn is_system(id: usize) -> bool {
    {
        let g = crate::monitor::read(&NSS);
        if let Some(n) = g.get(id) {
            return n.system;
        }
        if !g.is_empty() {
            return false;
        }
    }
    id == DEFAULT
}

/// El espacio `id` tiene la lista de excepciones (TYPE_EXEMPT_LIST_ENABLED)?
pub fn exempt_list_enabled(id: usize) -> bool {
    crate::monitor::read(&NSS).get(id).map_or(false, |n| n.exempt_list)
}

pub fn name(id: usize) -> String {
    get(id).map(|n| n.name).unwrap_or_else(|| "(null)".into())
}

/// Rutas de busqueda de `id` como `open_library`: (LD_LIBRARY_PATH del espacio, rutas por defecto). Las del espacio
/// por defecto llevan delante los directorios de `HEDDLE_LIBPATH` (su LD_LIBRARY_PATH fuera de Android).
pub fn search_paths(id: usize) -> (Vec<String>, Vec<String>) {
    let Some(n) = get(id) else { return (Vec::new(), Vec::new()) };
    let mut ld = Vec::new();
    if id == DEFAULT {
        if let Ok(p) = std::env::var("HEDDLE_LIBPATH") {
            ld.extend(parse_path(&p));
        }
    }
    ld.extend(n.ld_paths.iter().cloned());
    (ld, n.default_paths)
}

/// Anade un directorio de busqueda al espacio por defecto (`heddle-run`, `HEDDLE_LIBPATH` en Android).
pub fn add_default_path(dir: &str) {
    let mut g = crate::monitor::write(&NSS);
    ensure(&mut g);
    let d = dir.trim_end_matches('/').to_string();
    if !d.is_empty() && !g[DEFAULT].ld_paths.contains(&d) {
        g[DEFAULT].ld_paths.push(d);
    }
}

fn file_is_in_dir(file: &str, dir: &str) -> bool {
    file.strip_prefix(dir).and_then(|r| r.strip_prefix('/')).map_or(false, |r| !r.is_empty() && !r.contains('/'))
}

fn file_is_under_dir(file: &str, dir: &str) -> bool {
    file.strip_prefix(dir).and_then(|r| r.strip_prefix('/')).map_or(false, |r| !r.is_empty())
}

/// `android_namespace_t::is_accessible(const std::string& file)`: se puede cargar `realpath` en el espacio?
pub fn path_accessible(id: usize, realpath: &str) -> bool {
    let Some(n) = get(id) else { return false };
    if !n.isolated {
        return true;
    }
    n.ld_paths.iter().chain(n.default_paths.iter()).any(|d| file_is_in_dir(realpath, d)) || n.permitted.iter().any(|d| file_is_under_dir(realpath, d))
}

/// Espacio anonimo actual.
pub fn anonymous() -> usize {
    ANON.load(SeqCst)
}

/// `create_namespace` de bionic. `parent` None: el anonimo (el llamador no es una biblioteca guest). Devuelve el
/// identificador, o el texto de error de bionic.
pub fn create(name: &str, ld: &str, default: &str, typ: u64, permitted: &str, parent: Option<usize>) -> Result<usize, String> {
    let parent = parent.unwrap_or_else(anonymous);
    let (id, parent_ns) = {
        let mut g = crate::monitor::write(&NSS);
        ensure(&mut g);
        if g.len() >= NS_CAP {
            return Err(format!("too many namespaces ({}): cannot create namespace \"{}\"", NS_CAP, name));
        }
        let Some(p) = g.get(parent).cloned() else {
            return Err(format!("invalid parent namespace for \"{}\"", name));
        };
        let shared = typ & TYPE_SHARED != 0;
        let mut ns = Namespace {
            name: name.to_string(),
            isolated: typ & TYPE_ISOLATED != 0,
            ld_paths: parse_path(ld),
            default_paths: parse_path(default),
            permitted: parse_path(permitted),
            links: Vec::new(),
            system: shared && p.system,
            exempt_list: typ & TYPE_EXEMPT_LIST_ENABLED != 0,
            exported: false,
        };
        if shared {
            // se le anaden las rutas del padre y sus enlaces
            ns.ld_paths.extend(p.ld_paths.iter().cloned());
            ns.default_paths.extend(p.default_paths.iter().cloned());
            ns.permitted.extend(p.permitted.iter().cloned());
            ns.links.extend(p.links.iter().cloned());
        }
        g.push(ns);
        (g.len() - 1, parent)
    };
    // miembros heredados: con SHARED, todas las bibliotecas del padre; si no, su grupo compartido
    crate::elf::inherit_namespace(parent_ns, id, typ & TYPE_SHARED != 0);
    if typ & TYPE_ALSO_USED_AS_ANONYMOUS != 0 && !set_anonymous(id) {
        let n = get(id).unwrap();
        return Err(format!(
            "failed to set namespace: [name=\"{}\", ld_library_path=\"{}\", default_library_paths=\"{}\" permitted_paths=\"{}\"] as the anonymous namespace",
            n.name,
            n.ld_paths.join(":"),
            n.default_paths.join(":"),
            n.permitted.join(":")
        ));
    }
    Ok(id)
}

fn set_anonymous(id: usize) -> bool {
    if ANON_SET.swap(true, SeqCst) {
        return false;
    }
    ANON.store(id, SeqCst);
    true
}

/// `link_namespaces`: `from` toma de `to` (None: el por defecto) las bibliotecas de la lista (separada por ':').
pub fn link(from: usize, to: Option<usize>, sonames: &str) -> Result<(), String> {
    let to = to.unwrap_or(DEFAULT);
    let mut g = crate::monitor::write(&NSS);
    ensure(&mut g);
    if from >= g.len() || to >= g.len() {
        return Err("error linking namespaces: namespace_from is null.".into());
    }
    if sonames.is_empty() {
        return Err(format!("error linking namespaces \"{}\"->\"{}\": the list of shared libraries is empty.", g[from].name, g[to].name));
    }
    let list = sonames.split(':').map(|s| s.to_string()).collect();
    g[from].links.push(Link { to, sonames: list, all: false });
    Ok(())
}

/// `link_namespaces_all_libs`.
pub fn link_all(from: usize, to: usize) -> Result<(), String> {
    let mut g = crate::monitor::write(&NSS);
    ensure(&mut g);
    if from >= g.len() || to >= g.len() {
        return Err("error linking namespaces: namespace_from is null.".into());
    }
    g[from].links.push(Link { to, sonames: Vec::new(), all: true });
    Ok(())
}

/// `init_anonymous_namespace`: "(anonymous)" aislado con `search_path`, enlazado al por defecto con `sonames`.
pub fn init_anonymous(sonames: &str, search_path: &str) -> Result<(), String> {
    ANON_SET.store(false, SeqCst);
    let id = create("(anonymous)", "", search_path, TYPE_ISOLATED | TYPE_ALSO_USED_AS_ANONYMOUS, "", Some(DEFAULT))?;
    link(id, Some(DEFAULT), sonames)
}

/// Espacio exportado por nombre (`android_get_exported_namespace`): "default" es el por defecto; cualquier otro
/// ("system", "sphal", "vndk"...) es un espacio de sistema con ese nombre, el mismo en cada llamada.
pub fn exported(name: &str) -> Option<usize> {
    let mut g = crate::monitor::write(&NSS);
    ensure(&mut g);
    if let Some(i) = g.iter().position(|n| n.exported && n.name == name) {
        return Some(i);
    }
    if g.len() >= NS_CAP {
        return None;
    }
    g.push(Namespace {
        name: name.to_string(),
        isolated: false,
        ld_paths: Vec::new(),
        default_paths: Vec::new(),
        permitted: Vec::new(),
        links: Vec::new(),
        system: true,
        exempt_list: false,
        exported: true,
    });
    Some(g.len() - 1)
}

const HANDLE_TAG: u64 = 0x6e73_0000_0000;

/// Valor opaco que recibe ART por cada espacio (`native_bridge_namespace_t*`).
pub fn handle_of(id: usize) -> u64 {
    HANDLE_TAG | ((id as u64 + 1) << 4)
}

pub fn from_handle(h: u64) -> Option<usize> {
    if h & !0xffff_fff0 != HANDLE_TAG || h & 0xf != 0 {
        return None;
    }
    let id = ((h & 0xffff_fff0) >> 4) as usize;
    let n = crate::monitor::read(&NSS).len();
    if id == 0 || id > n.max(1) {
        return None;
    }
    Some(id - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rutas_y_accesibilidad_como_bionic() {
        assert!(file_is_in_dir("/a/b/libx.so", "/a/b"));
        assert!(!file_is_in_dir("/a/b/c/libx.so", "/a/b"));
        assert!(!file_is_in_dir("/a/bc/libx.so", "/a/b"));
        assert!(file_is_under_dir("/a/b/c/libx.so", "/a/b"));
        assert!(!file_is_under_dir("/a/bc/libx.so", "/a/b"));
        let id = create("prueba-aislado", "", "/x/lib", TYPE_ISOLATED, "/x/perm", Some(DEFAULT)).unwrap();
        assert!(path_accessible(id, "/x/lib/liba.so"));
        assert!(!path_accessible(id, "/x/lib/sub/liba.so"));
        assert!(path_accessible(id, "/x/perm/sub/liba.so"));
        assert!(!path_accessible(id, "/y/liba.so"));
        assert_eq!(from_handle(handle_of(id)), Some(id));
        assert_eq!(from_handle(0x1234), None);
        assert!(link(id, None, "").unwrap_err().contains("the list of shared libraries is empty"));
    }
}
