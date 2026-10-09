//! Binarios: opciones comunes (`--help`/`-h`, `--version`/`-V`) y codigos de salida (0 bien, 2 uso incorrecto,
//! 1 error del puente; heddle-run devuelve el estado del guest o muere por su misma senal).
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Command, Output};

const BINS: [&str; 3] = [env!("CARGO_BIN_EXE_heddle-run"), env!("CARGO_BIN_EXE_heddle-difftest"), env!("CARGO_BIN_EXE_heddle-fpone")];

fn run(bin: &str, args: &[&str]) -> Output {
    Command::new(bin).args(args).output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[test]
fn ayuda_y_version() {
    for bin in BINS {
        let name = PathBuf::from(bin).file_stem().unwrap().to_string_lossy().into_owned();
        for h in ["--help", "-h"] {
            let o = run(bin, &[h]);
            assert_eq!(o.status.code(), Some(0), "{} {}", name, h);
            assert!(text(&o.stdout).starts_with(&format!("uso: {} ", name)), "{} {}: {}", name, h, text(&o.stdout));
        }
        for v in ["--version", "-V"] {
            let o = run(bin, &[v]);
            assert_eq!(o.status.code(), Some(0), "{} {}", name, v);
            let s = text(&o.stdout);
            assert!(s.starts_with(&format!("{} ", name)) && s.contains("(build "), "{} {}: {}", name, v, s);
        }
    }
}

#[test]
fn uso_incorrecto_es_2() {
    for bin in BINS {
        for args in [&[][..], &["--opcion-que-no-existe"][..]] {
            let o = run(bin, args);
            assert_eq!(o.status.code(), Some(2), "{} {:?}: {}", bin, args, text(&o.stderr));
            assert!(text(&o.stderr).contains("uso:"), "{} {:?}", bin, args);
        }
    }
    let [hrun, diff, fpone] = BINS;
    assert_eq!(run(hrun, &["lib.so", "f", "no-es-un-numero"]).status.code(), Some(2));
    assert_eq!(run(diff, &["c.bin", "--engine", "qemu"]).status.code(), Some(2));
    assert_eq!(run(fpone, &["zz", "0"]).status.code(), Some(2));
}

#[test]
fn errores_del_puente_son_1() {
    let [hrun, diff, fpone] = BINS;
    let o = run(hrun, &["/no/existe/libnada.so", "f"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    // un archivo que no es un ELF
    let o = run(hrun, &[concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"), "f"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let o = run(diff, &["/no/existe/casos.bin"]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    let t = tmp("truncado.bin");
    std::fs::write(&t, [1u8, 0, 0, 0, 0x1f]).unwrap();
    let o = run(diff, &[t.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(1), "{}", text(&o.stderr));
    assert!(text(&o.stderr).contains("truncado"), "{}", text(&o.stderr));
    // heddle-fpone: ejecutar, aunque la instruccion sea indefinida, es 0 (y nombra la instruccion)
    let o = run(fpone, &["1e604020", "0", "v1=3ff0000000000000"]);
    assert_eq!(o.status.code(), Some(0));
    let o = run(fpone, &["cec08400", "0"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(text(&o.stdout).contains("SM4E"), "{}", text(&o.stdout));
}

/// Estado del guest con una biblioteca arm64 de verdad (difftests/tests/guest/t7.c). Necesita clang con destino
/// aarch64 y lld; si no estan, se omite (scripts/run-all.sh de difftests lo comprueba igualmente).
#[test]
fn heddle_run_devuelve_el_estado_del_guest() {
    let so = tmp("libt7.so");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/difftests/tests/guest/t7.c");
    // t7 llama a exit: declara libc.so en DT_NEEDED (como bionic, el cargador no busca fuera de su lista), enlazada
    // con un stub que solo la exporta (en ejecucion la sirve el puente)
    let dir = tmp("t7stub");
    std::fs::create_dir_all(&dir).unwrap();
    let stub_s = dir.join("libc.s");
    std::fs::write(&stub_s, ".text\n.globl exit\n.type exit,@function\nexit:\nret\n").unwrap();
    let a64 = ["--target=aarch64-linux-gnu", "-O2", "-shared", "-fPIC", "-fuse-ld=lld", "-nostdlib"];
    let ok = |c: std::io::Result<std::process::Output>| matches!(c, Ok(o) if o.status.success());
    let stub = Command::new("clang").args(a64).args(["-Wl,-soname,libc.so", "-o"]).arg(dir.join("libc.so")).arg(&stub_s).output();
    let cc = || Command::new("clang").args(a64).arg("-o").arg(&so).arg(src).arg("-L").arg(&dir).arg("-l:libc.so").output();
    if !ok(stub) || !ok(cc()) {
        eprintln!("omitida: no hay clang para aarch64 con lld");
        return;
    }
    let [hrun, ..] = BINS;
    let so = so.to_str().unwrap();
    assert_eq!(run(hrun, &[so, "ret42"]).status.code(), Some(42));
    assert_eq!(run(hrun, &[so, "ret300"]).status.code(), Some(300 & 0xff));
    assert_eq!(run(hrun, &[so, "exit7"]).status.code(), Some(7));
    assert_eq!(run(hrun, &[so, "no_existe"]).status.code(), Some(1));
    // ID_AA64ISAR0_EL1.TS leido con MRS (lo que anuncia el modelo) * 10 + CFINV aplicado; sin FlagM anunciada (A78),
    // CFINV es SIGILL como en ese procesador
    let o = Command::new(hrun).args([so, "flagm"]).env("HEDDLE_CPU", "max").output().unwrap();
    assert_eq!(o.status.code(), Some(21), "max: {}", text(&o.stderr));
    let o = Command::new(hrun).args([so, "flagm"]).env("HEDDLE_CPU", "cortex-a78").output().unwrap();
    assert_eq!((o.status.code(), o.status.signal()), (None, Some(4)), "cortex-a78: {}", text(&o.stderr));
    assert!(text(&o.stderr).contains("instruccion de FEAT_FlagM (0xd500401f), ausente en el modelo de CPU Cortex-A78"), "{}", text(&o.stderr));
    for (f, what) in [
        ("sm4e", "instruccion no soportada por heddle: SM4E (0xcec08400) [FEAT_SM4; SIMD y FP]"),
        ("udf", "instruccion indefinida en ARMv8/ARMv9: UDF #0x0"),
        ("smstart", "SMSTART (0xd503477f) [FEAT_SME (no soportado);"),
        ("mrs_rndr", "MRS S3_3_C2_C4_0 (0xd53b2400) [FEAT_RNG (RNDR);"),
    ] {
        let o = run(hrun, &[so, f]);
        assert_eq!((o.status.code(), o.status.signal()), (None, Some(4)), "{}: {}", f, text(&o.stderr));
        assert!(text(&o.stderr).contains(what), "{}: {}", f, text(&o.stderr));
    }
}
