//! heddle-run: carga una biblioteca AArch64 y llama a una funcion guest. Ver `USO`.
//!
//! Codigos de salida: el estado del guest (el valor que devuelve la funcion, `x0 & 0xff`, como el de `main`, o el de
//! `exit()` si la llama); si el guest muere por una senal, heddle-run muere por esa misma senal (el shell ve 128+N).
//! Errores propios: 2 uso incorrecto, 1 error del puente (biblioteca o simbolo no encontrados, ELF no valido).
use heddle::{elf, rt};

const USO: &str = "\
uso: heddle-run [opciones] BIBLIOTECA SIMBOLO [ARG...]

Carga BIBLIOTECA (ELF AArch64) y llama a la funcion guest SIMBOLO con los argumentos ARG: enteros (decimal, con
signo, o hexadecimal 0x...) en x0-x7 y despues en la pila, y dobles con sufijo 'd' (p. ej. 1.5d) en d0-d7.
Al terminar imprime x0 y d0.

opciones:
  -h, --help      muestra esta ayuda
  -V, --version   muestra la version

salida: el estado del guest (x0 & 0xff, o el de exit()); si el guest muere por una senal, la misma senal.
        2: uso incorrecto; 1: error del puente (biblioteca o simbolo no encontrados, ELF no valido).
";

/// Uso incorrecto (argumentos).
const EXIT_USO: i32 = 2;
/// Error del puente: archivo no encontrado, ELF no valido, simbolo inexistente.
const EXIT_ERROR: i32 = 1;

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Version,
    Run { lib: String, sym: String, ints: Vec<u64>, fps: Vec<u64> },
}

fn parse(a: &[String]) -> Result<Cmd, String> {
    // las opciones van antes de la biblioteca; "--" permite una biblioteca cuyo nombre empieza por '-'
    let pos = match a.first().map(|s| s.as_str()) {
        Some("-h" | "--help") => return Ok(Cmd::Help),
        Some("-V" | "--version") => return Ok(Cmd::Version),
        Some("--") => &a[1..],
        Some(o) if o.starts_with('-') => return Err(format!("opcion desconocida: {}", o)),
        _ => a,
    };
    if pos.len() < 2 {
        return Err("faltan la biblioteca y el simbolo".into());
    }
    let (mut ints, mut fps) = (Vec::new(), Vec::new());
    for s in &pos[2..] {
        if let Some(v) = s.strip_suffix('d').and_then(|d| d.parse::<f64>().ok()) {
            fps.push(v.to_bits());
            continue;
        }
        let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(h) => u64::from_str_radix(h, 16).ok(),
            None => s.parse::<i64>().map(|v| v as u64).ok().or_else(|| s.parse::<u64>().ok()),
        };
        ints.push(v.ok_or_else(|| format!("argumento no valido: {} (entero, 0x... o doble con sufijo 'd')", s))?);
    }
    if fps.len() > 8 {
        return Err("como mucho 8 argumentos dobles (d0-d7)".into());
    }
    Ok(Cmd::Run { lib: pos[0].clone(), sym: pos[1].clone(), ints, fps })
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (lib, sym, ints, fps) = match parse(&a) {
        Ok(Cmd::Help) => {
            print!("{}", USO);
            return;
        }
        Ok(Cmd::Version) => {
            println!("{}", heddle::bridge::version_line("heddle-run"));
            return;
        }
        Ok(Cmd::Run { lib, sym, ints, fps }) => (lib, sym, ints, fps),
        Err(e) => {
            eprintln!("heddle-run: {}\n\n{}", e, USO);
            std::process::exit(EXIT_USO);
        }
    };
    // rutas del espacio por defecto (su LD_LIBRARY_PATH): el directorio de la biblioteca y el actual
    if let Some((d, _)) = lib.rsplit_once('/') {
        elf::add_search_path(if d.is_empty() { "/" } else { d });
    }
    if let Ok(d) = std::env::current_dir() {
        elf::add_search_path(&d.to_string_lossy());
    }
    rt::ensure_thread(rt::DEFAULT_STACK);
    heddle::sig::install_fault_handlers();
    let m = match elf::load_library(&lib) {
        Ok(Some(m)) => m,
        Ok(None) => {
            eprintln!("heddle-run: {}: es una biblioteca del sistema que el puente implementa (HLE), no se carga", lib);
            std::process::exit(EXIT_ERROR);
        }
        Err(e) => {
            eprintln!("heddle-run: {}", e);
            std::process::exit(EXIT_ERROR);
        }
    };
    let f = m.lookup(&sym).unwrap_or_else(|| {
        eprintln!("heddle-run: simbolo no encontrado: {}", sym);
        std::process::exit(EXIT_ERROR)
    });
    if std::env::var("HEDDLE_STATS").is_ok() {
        heddle::jit::HELPER_COUNT_ON.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    // Si el guest llama a exit() o muere por una senal no vuelve aqui: exit() termina con su estado y un fallo sin
    // manejador termina el proceso con la misma senal (sig::guest_fault).
    let (x0, d0) = rt::call_guest(f, &ints, &fps);
    if std::env::var("HEDDLE_STATS").is_ok() {
        eprintln!("[stats] llamadas al interprete desde el JIT: {}", heddle::jit::HELPER_CALLS.load(std::sync::atomic::Ordering::Relaxed));
    }
    println!("x0={} ({:#x})  d0={}", x0 as i64, x0, f64::from_bits(d0));
    std::process::exit((x0 & 0xff) as i32);
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
        assert_eq!(p(&["--help", "x"]), Ok(Cmd::Help));
        assert_eq!(p(&["-V"]), Ok(Cmd::Version));
        assert_eq!(p(&["--version"]), Ok(Cmd::Version));
        assert!(p(&["--nada"]).is_err());
        assert!(p(&[]).is_err());
        assert!(p(&["lib.so"]).is_err());
    }

    #[test]
    fn argumentos() {
        let r = p(&["lib.so", "f", "1", "-2", "0x10", "1.5d", "18446744073709551615"]).unwrap();
        assert_eq!(r, Cmd::Run { lib: "lib.so".into(), sym: "f".into(), ints: vec![1, (-2i64) as u64, 16, u64::MAX], fps: vec![1.5f64.to_bits()] });
        // un numero negativo despues de la biblioteca no es una opcion; "--" separa una biblioteca que empieza por '-'
        assert!(matches!(p(&["--", "-lib.so", "f", "-1"]), Ok(Cmd::Run { .. })));
        assert!(p(&["lib.so", "f", "0xzz"]).is_err());
        assert!(p(&["lib.so", "f", "abc"]).is_err());
        assert!(p(&["lib.so", "f", "1", "2", "3", "4", "5", "6", "7", "8", "9"]).is_ok()); // x0-x7 y pila
        assert!(p(&["lib.so", "f", "1d", "2d", "3d", "4d", "5d", "6d", "7d", "8d", "9d"]).is_err());
    }
}
