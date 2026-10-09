//! heddle-fpone: ejecuta una instruccion FP/NEON suelta en el interprete de referencia. Ver `USO`.
//! Codigos de salida: 0 ejecutada (aunque sea indefinida: lo dice la salida), 2 uso incorrecto.
use heddle::cpu::Cpu;

const USO: &str = "\
uso: heddle-fpone [opciones] PALABRA FPCR [REG=VALOR...]

Ejecuta la instruccion PALABRA (hexadecimal) en el interprete con FPCR (hexadecimal) y los registros dados, e
imprime la operacion decodificada, el flujo, FPSR/NZCV y los registros resultantes.
  REG=VALOR   xN=HEX (x0-x30; x31 = sp) o vN=ALTO_BAJO / vN=BAJO (128 bits en hexadecimal)

opciones:
  -h, --help      muestra esta ayuda
  -V, --version   muestra la version

salida: 0 ejecutada (tambien si la instruccion es indefinida: lo dice la salida); 2 uso incorrecto.
";

const EXIT_USO: i32 = 2;

#[derive(Debug, PartialEq)]
enum Cmd {
    Help,
    Version,
    Run { word: u32, fpcr: u64, x: Vec<(usize, u64)>, v: Vec<(usize, [u64; 2])> },
}

fn hex(s: &str) -> Result<u64, String> {
    let t = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(t, 16).map_err(|_| format!("valor hexadecimal no valido: {}", s))
}

fn parse(a: &[String]) -> Result<Cmd, String> {
    match a.first().map(|s| s.as_str()) {
        Some("-h" | "--help") => return Ok(Cmd::Help),
        Some("-V" | "--version") => return Ok(Cmd::Version),
        Some(o) if o.starts_with('-') => return Err(format!("opcion desconocida: {}", o)),
        _ => {}
    }
    if a.len() < 2 {
        return Err("faltan la palabra y FPCR".into());
    }
    let word = hex(&a[0])?;
    if word > u32::MAX as u64 {
        return Err(format!("la palabra no cabe en 32 bits: {}", a[0]));
    }
    let fpcr = hex(&a[1])?;
    let (mut x, mut v) = (Vec::new(), Vec::new());
    for s in &a[2..] {
        let bad = || format!("registro no valido: {} (xN=HEX o vN=ALTO_BAJO)", s);
        let (r, val) = s.split_once('=').ok_or_else(bad)?;
        let (kind, n) = r.split_at(1.min(r.len()));
        let n: usize = n.parse().map_err(|_| bad())?;
        match kind {
            "x" if n < 32 => x.push((n, hex(val)?)),
            "v" if n < 32 => {
                let (hi, lo) = val.split_once('_').unwrap_or(("0", val));
                v.push((n, [hex(lo)?, hex(hi)?]));
            }
            _ => return Err(bad()),
        }
    }
    Ok(Cmd::Run { word: word as u32, fpcr, x, v })
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let (w, fpcr, xs, vs) = match parse(&a) {
        Ok(Cmd::Help) => {
            print!("{}", USO);
            return;
        }
        Ok(Cmd::Version) => {
            println!("{}", heddle::bridge::version_line("heddle-fpone"));
            return;
        }
        Ok(Cmd::Run { word, fpcr, x, v }) => (word, fpcr, x, v),
        Err(e) => {
            eprintln!("heddle-fpone: {}\n\n{}", e, USO);
            std::process::exit(EXIT_USO);
        }
    };
    let mut c = Cpu::new();
    c.fpcr = fpcr;
    for (r, val) in xs {
        c.x[r] = val;
    }
    for (r, val) in vs {
        c.v[r] = val;
    }
    let op = heddle::decode::decode(w);
    println!("{:?}", op);
    let f = heddle::interp::exec(&mut c, &op);
    if let heddle::interp::Flow::Undef(_) = f {
        println!("{}", heddle::diag::classify(w));
    }
    println!("{:?} fpsr={:#x} nzcv={:#x}", f, c.fpsr, c.flags());
    for i in 0..32 {
        if c.v[i] != [0, 0] {
            println!("v{} = {:016x}_{:016x}", i, c.v[i][1], c.v[i][0]);
        }
    }
    println!("x0={:#x} x1={:#x} x2={:#x}", c.x[0], c.x[1], c.x[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Cmd, String> {
        parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn opciones_y_argumentos() {
        assert_eq!(p(&["-h"]), Ok(Cmd::Help));
        assert_eq!(p(&["--version"]), Ok(Cmd::Version));
        assert!(p(&["-x"]).is_err());
        assert!(p(&["1e604020"]).is_err());
        assert!(p(&["zz", "0"]).is_err());
        assert!(p(&["123456789", "0"]).is_err());
        assert_eq!(
            p(&["1e604020", "0", "x2=ff", "v1=1_2", "v3=0x5"]),
            Ok(Cmd::Run { word: 0x1e60_4020, fpcr: 0, x: vec![(2, 0xff)], v: vec![(1, [2, 1]), (3, [5, 0])] })
        );
        assert!(p(&["1e604020", "0", "x32=1"]).is_err());
        assert!(p(&["1e604020", "0", "q1=1"]).is_err());
        assert!(p(&["1e604020", "0", "v1"]).is_err());
    }
}
