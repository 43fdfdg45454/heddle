# difftests: pruebas de heddle

Bibliotecas guest en C (aarch64, compiladas con clang), un ART simulado y utilidades host, y el generador de casos
diferenciales (Unicorn/QEMU como oráculo). La carpeta se llama `difftests/` y no
`tests/` porque `tests/` es la de cargo (pruebas de integración de Rust, p. ej. `arch_lint.rs`).

```bash
cargo build --release          # en la raíz del repo
cd difftests
scripts/setup-debian.sh        # dependencias + Unicorn 2.1.4 (pip, en deps/venv)
scripts/build-tests.sh         # compila libs guest y programas host en build/
scripts/run-all.sh
scripts/difftest.sh 20000 11 fp_dp2,fp_dp3,simd_3same   # o `todas`
```
`HEDDLE=ruta` apunta a otra raíz compilada (por defecto `..`); `PYTHON` al intérprete con `unicorn`; `MOTORES` a los
motores (por defecto `jit interp`).
Esperado: `0 fallos` en todo, también en difftest (sale con 1 si hay alguna discrepancia no excluida).

## Diferencial contra Unicorn

`tools/gen_cases.py` genera casos (una instrucción o una secuencia corta) con estado aleatorio, los ejecuta en
Unicorn 2.1.4 (QEMU 5, CPU `max`) y escribe el estado esperado (formato `HDT2`, descrito en la cabecera del script);
`heddle-difftest` los ejecuta con el JIT o el intérprete y compara registros x, sp, NZCV, pc, v0-v31, FPSR,
TPIDR_EL0 y la memoria. La referencia es el Arm ARM, no Unicorn: cuando difieren se decide por el pseudocódigo y, si
el que se equivoca es el oráculo o el resultado no está determinado por la arquitectura, el caso se excluye en el
generador con su motivo (se cuenta y se imprime al final de la generación).

- **Memoria y fallos.** Los accesos van a la región de datos o, con probabilidad 0,15, a una de las páginas de
  guarda sin permisos que la rodean. Unicorn da el fallo con el pc y los registros exactos; heddle lo entrega a un
  manejador guest real (`sigaction` + `sigaltstack` del guest, en el JIT por el camino de `sig::sync_fault`; en el
  intérprete, un manejador del host) y se comparan la señal, `si_code`, `si_addr`, el pc y los registros del
  `ucontext`, y la memoria. SIGSEGV `SEGV_ACCERR` en una guarda; SIGBUS `BUS_ADRALN` para exclusivos, atómicos LSE,
  CAS/CASP y LDAPR desalineados (sin FEAT_LSE2, `AArch64.CheckAlignment`).
- **Indefinidas.** Si Unicorn rechaza la instrucción, heddle debe dar SIGILL; si la ejecuta, es un fallo.
- **TPIDR_EL0** aleatorio por caso y comparado (Unicorn lo conservaba entre casos).
- **FPCR** con FZ, FZ16, DN, AHP y redondeos; valores cerca del underflow. La clase `fp_cvth` (FCVT, FCVTL y FCVTN
  con half) usa valores de borde del half IEEE y del alternativo (exponente 31, 65504/65520, 131008/131040, NaN,
  infinitos) y AHP en el 60 % de los casos.

Exclusiones (`EXCLUSIONES` en `tools/gen_cases.py`):

| Nombre | Por qué |
|---|---|
| `parcial` | Instrucción de varios accesos que falla tras completar alguno: el orden de los accesos y el estado de los destinos no están definidos. |
| `a_caballo` | Acceso que cruza de la región de datos a una guarda: Unicorn no detecta la segunda página. |
| `stxr_desalineado` | STXR/STXP desalineado: es falta de alineación antes de mirar el monitor (`AArch64.ExclusiveMonitorsPass`); QEMU 5 solo devuelve 1. |
| `stxr_guarda` | STXR/STXP sin reserva a una guarda: es IMPLEMENTATION DEFINED si se detecta el fallo o falla el monitor primero. |
| `ordenado_desalineado` | LDAR/STLR desalineado: falta de alineación (`AArch64.CheckAlignment`); QEMU 5 no la comprueba. |
| `fpcr_len_stride` | MSR FPCR con Len/Stride: son RES0 sin AArch32 (heddle anuncia EL0 solo AArch64) y heddle los descarta; la CPU `max` de Unicorn tiene AArch32 y los conserva. Las habilitaciones de trampas, RAZ/WI en heddle, coinciden con QEMU y se comprueban (`MSR`+`MRS` de FPCR/FPSR en la clase `sysreg`). |
| `store_intermedio` | LDXR; STR a la misma dirección; STXR con éxito en Unicorn: es IMPLEMENTATION DEFINED si un store del mismo PE borra el monitor local (heddle lo borra siempre; QEMU compara valores). |

Además el generador no emite codificaciones en las que QEMU es laxo o tiene errores conocidos (comentadas en
`gen_word`): inmediato modificado con o2=1 y op=1 (no asignado), FCMLA H por elemento con Q=0, FCVTZ* H escalar,
FP16 escalar que aborta Unicorn.

El CI (trabajo `unicorn` de `build.yml`, en paralelo a `difftests`) ejecuta `difftest.sh 20000 1 todas` y 10 000
casos de FP (con `fp_cvth`) con semilla 2, con los dos motores (unos 4 minutos con la instalación).
No hay pruebas en Android real. t1–t10 y jni1 usan solo clang + mocks (t5: recarga tras `dlclose` y TLS importado
de otra biblioteca; `run-all.sh` exporta `HEDDLE_LIBPATH=build` para el `dlopen` del guest; t6: máscaras y señales asíncronas; t7: códigos de salida de
`heddle-run` y el mensaje de una instrucción no soportada; t8: el cargador como bionic: puntero viejo tras `dlclose`
con SIGSEGV, dirección nueva al recargar, TLS initial-exec rechazado en `dlopen`, ciclo de DT_NEEDED y
`__cxa_thread_atexit_impl`; t10: más casos del enlazador de bionic, con `HEDDLE_TARGET_SDK` 35, 30 y 22:
DT_SONAME e inodo, orden de búsqueda de símbolos, interposición, `RTLD_NEXT`, `RTLD_GLOBAL` y DF_1_GLOBAL, símbolos
como bionic (indefinido en todas partes, versiones, `dlvsym`, DT_VERNEED, handle de una biblioteca del sistema), TLS
dinámico, alineación de 2 MiB y orden de mapeo aleatorio, rutas reales y `android_dlopen_ext` (descriptor,
desplazamiento, `FORCE_LOAD`, región reservada, RELRO compartido); t11: llamadas TLSDESC con la ruta rápida en línea
del JIT frente al resolutor traducido (rápida, generación vieja, bloque sin reservar, TBI, SIGSEGV dentro del
resolutor); mockart: espacios de nombres de ART, aislados, compartidos y enlazados, RTLD_DEFAULT en el espacio de una
app y la lista de excepciones de `targetSdkVersion` < 24).
tvk: Vulkan por el puente con el ICD por software (lavapipe, `mesa-vulkan-drivers`; `run-all.sh` lo elige con
`VK_DRIVER_FILES`): instancia y dispositivo con un asignador guest (`pAllocator`, todo liberado al destruir) y un
mensajero de `VK_EXT_debug_utils` en la cadena `pNext` de `vkCreateInstance`. Sin Vulkan en el equipo se omite,
salvo con `HEDDLE_REQUIRE_VULKAN=1` (el CI).
tld: `long double` en stdio y stdlib (`strtold`, `wcstold`, `%L` en printf/wprintf/scanf/wscanf, también con
`va_list`): subnormales, redondeos al medio, NaN con carga, infinitos, hexadecimales, anchuras y `%n`, comparados con
`tests/guest/tld_ref.h`, la salida de la bionic arm64 real (`tools/tld_ref.sh`: el mismo `tld.c` enlazado estático con
el `libc.a` del NDK y ejecutado con qemu-aarch64). Sin biblioteca guest incrustada se omite, salvo con
`HEDDLE_REQUIRE_GUEST=1` (el CI). `run-all.sh` ejecuta además sus funciones `n_*`: printf, snprintf, `_chk`,
vsnprintf, swprintf y wprintf con `%n` deben morir con SIGABRT y el mensaje de bionic en stderr.
`run-all.sh` también ejecuta t10 con `HEDDLE_PAGE_SIZE=16384` (tamaño de página, `mmap` y cargador de 16 KiB, modo
de compatibilidad para `p_align` de 4 KiB); todo `run-all.sh` se puede ejecutar así.

Microbanco del cargador (no está en `run-all.sh`): `../target/release/heddle-run build/libdlbench.so run_bench` (con
`HEDDLE_LIBPATH=build`): `dlopen`+`dlclose` de siete bibliotecas nuevas y de una con ~150 importaciones, y `dlsym`.

Microbanco de TLS dinámico (no está en `run-all.sh`): `../target/release/heddle-run build/libtlsbench.so run_bench`
(con `HEDDLE_LIBPATH=build`). Mide el acceso TLSDESC en la ruta rápida (por vuelta de un bucle que llama a una
función que devuelve la dirección de una variable TLS, y la diferencia con la misma función sobre una global) y el
coste del primer acceso de un hilo a un módulo (ruta lenta: reserva del bloque). En x86-64 con 4 CPU cargadas, con la
ruta rápida en línea del JIT (diferencia con la global, que es sobre todo el marco de pila de la función, dos stores
guest): `rseq` 12 ns, `fence` 7 ns, `bloqueo` 28 ns (con el resolutor traducido, sin la ruta en línea: 41, 29 y 86 ns);
primer acceso ~0,6 µs. El bucle usa el resultado de cada llamada: si no, clang la saca del bucle y solo se mide el bucle.
