// Enlaza con la libclang del sistema. SIGTOOL_LIBCLANG = ruta completa a libclang*.so (por defecto, la de LLVM 18).
fn main() {
    let lib = std::env::var("SIGTOOL_LIBCLANG").unwrap_or_else(|_| {
        for c in ["/usr/lib/llvm-18/lib/libclang-18.so.18", "/usr/lib/llvm-19/lib/libclang-19.so.19", "/usr/lib/llvm-17/lib/libclang-17.so.17", "/usr/lib/x86_64-linux-gnu/libclang-18.so.18"] {
            if std::path::Path::new(c).exists() {
                return c.to_string();
            }
        }
        panic!("no se encontro libclang; define SIGTOOL_LIBCLANG")
    });
    println!("cargo:rustc-link-arg={}", lib);
    println!("cargo:rerun-if-env-changed=SIGTOOL_LIBCLANG");
}
