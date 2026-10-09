# Estructura del repositorio

heddle es un monorepo: el traductor, sus pruebas y sus benchmarks viven juntos.

| Carpeta | Lenguaje | Contenido |
|---|---|---|
| `src/` | Rust | Núcleo, sin dependencias externas: cpu, decode, interp, softfp, fp, neon*, crypto (intérprete de referencia); jit, jitfp (traductor x86-64); monitor (LL/SC y LSE); elf, rt, sig, syscall, sys, fmt, ldio (long double en printf/scanf), mem; boundary, hostcall, cbthunk, hle, libc_hle, dl (dl* del guest), jni, sigs, sigs_gen, guestlib; bridge (`NativeBridgeItf` v8, `libheddle.so`); diag (nombres de instrucciones y llamadas al sistema para el diagnóstico); feat (extensiones anunciadas: registros de identificación y HWCAP). Binarios: `heddle-run`, `heddle-difftest`, `heddle-fpone`. |
| `tests/` | Rust | Pruebas de integración de cargo (`arch_lint.rs`: reglas de arquitectura; `cli.rs`: opciones y códigos de salida de los binarios). |
| `difftests/` | C + Python + shell | Pruebas guest (`tests/guest`: C compilado con clang a aarch64), ART simulado y utilidades host (`tests/host`), generador de casos diferenciales con Unicorn/QEMU como oráculo (`tools/gen_cases.py`, `uc_one.py`) y scripts. |
| `bench/` | Rust + C | Microbenchmarks de la traducción (`arm_xlat_suite`, `arm_xlat_bench`, `arm_xlat_strict`, `ldxr_bench`, `monitor_strict`) y la versión nativa AArch64 (`native/`). |
| `guest/` | C/C++ + shell | Biblioteca guest ARM64 (libm de bionic y, de su libc, gdtoa y vfprintf para `long double`) que se incrusta en `libheddle.so`. |
| `tools/sigtool/` | Rust | Generador de `src/sigs_gen.rs` y `docs/firmas-ndk.md` desde las cabeceras del NDK. |
| `examples/` | Rust | Ejemplos sueltos. |

`difftests/` y no `tests/` porque `tests/` es la carpeta que cargo reserva para pruebas de integración.

## CI (`.github/workflows/`)

- `build.yml`: `linux` (guest, build, pruebas con `--test-threads=1`), `android` (NDK, símbolos contra bionic,
  `NativeBridgeItf`, tabla de firmas), `difftests` (pruebas guest sobre los binarios de `linux`), `release` (solo si
  todo pasó) y `registro` (registros de todas las tareas en el release prerelease `ci`).
- `sysroot.yml`: manual, exporta las cabeceras del NDK para `tools/sigtool`.

## Releases

- Artefactos: `heddle-android-x86_64.tar.gz` (`libheddle.so` para Android) y `heddle-linux-x86_64.tar.gz`
  (`heddle-run`, `heddle-difftest`, `libheddle.so`).
- Versión: `vX.Y.Z`, calculada por GitVersion (`GitVersion.yml`): cada push a main que pase publica la última etiqueta más
  un parche; `+semver: minor|major` en un commit sube la menor o la mayor. Una etiqueta `v*` empujada a mano se respeta.
