// Incrusta la biblioteca guest (guest/build.sh) en libheddle.so. Si no se ha compilado, se incrusta un bloque
// vacio y el puente sigue funcionando con las aproximaciones en double de esas funciones.
//   HEDDLE_GUEST_LIB = ruta al .so (por defecto guest/out/libheddle_guest.so)
fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out = std::env::var("OUT_DIR").unwrap();
    let cand = std::env::var("HEDDLE_GUEST_LIB").unwrap_or_else(|_| format!("{}/guest/out/libheddle_guest.so", dir));
    let blob = if std::path::Path::new(&cand).is_file() {
        cand.clone()
    } else {
        let empty = format!("{}/guest_vacia.bin", out);
        std::fs::write(&empty, b"").unwrap();
        println!("cargo:warning=biblioteca guest no compilada ({}): long double y complejos usaran la aproximacion en double", cand);
        empty
    };
    println!("cargo:rustc-env=HEDDLE_GUEST_BLOB={}", blob);
    if std::path::Path::new(&cand).is_file() {
        println!("cargo:rerun-if-changed={}", cand);
    } else {
        // vigilar un archivo inexistente hace que cargo vuelva a ejecutar build.rs y recompile el crate en cada orden
        // (y que dos cargo a la vez reenlacen el binario de pruebas que el otro ejecuta): se vigila el directorio
        // existente mas cercano dentro del repositorio, donde aparecera la biblioteca al compilarla
        let mut p = std::path::Path::new(&cand).parent();
        while let Some(d) = p {
            if d.is_dir() {
                if d.starts_with(&dir) {
                    println!("cargo:rerun-if-changed={}", d.display());
                }
                break;
            }
            p = d.parent();
        }
    }
    println!("cargo:rerun-if-env-changed=HEDDLE_GUEST_LIB");
    println!("cargo:rerun-if-changed=build.rs");
}
