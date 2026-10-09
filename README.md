# heddle

<img src="docs/icono.svg" alt="" width="96" align="right">

Traductor ARM64 (AArch64) → x86-64 escrito en Rust, con cargador ELF, HLE de bionic/libc, JNI y una
implementación de `NativeBridgeItf` (v8) para ejecutar bibliotecas nativas arm64 en Android x86_64.

## Estado (honesto)
- Intérprete de referencia y JIT validados contra Unicorn/QEMU con casos aleatorios (ver `difftests/`).
  Fallo conocido: FZ + FNMADD (difiere de QEMU; intérprete y JIT coinciden entre sí).
- Capa HLE/JNI/bridge probada con bibliotecas aarch64 compiladas con clang y un ART simulado en x86 Linux, y en
  Android x86_64 sobre emulador.
- **No validado en hardware Android/ARM real.**
- La frontera con el host es la API en C del NDK, con firmas generadas por `tools/sigtool` (`docs/firmas-ndk.md`).
- Frontera host/guest y gestión de memoria: ver `CLAUDE.md` (invariantes, límites y lo que no está garantizado).
- OpenSL ES, OpenMAX AL y `ANativeActivity` van por proxys explícitos; las estructuras de callbacks del NDK
  (cámara, `AImageReader`, `AMediaCodec` asíncrono, `timer_create`) se convierten campo a campo (ver `CLAUDE.md`),
  igual que `z_stream` de zlib con `zalloc` propio, `VkAllocationCallbacks` y las cadenas `pNext` de Vulkan (solo en
  las funciones donde `vk.xml` permite callbacks: creación de instancia, dispositivo y mensajeros; el resto, sin coste), `glob`
  con `GLOB_ALTDIRFUNC` y el registrador/abortador de liblog. `pthread_cleanup_push` tiene implementación propia y
  NativeActivity reconoce un `android.app.func_name` propio (lo lee del manifiesto). Sin proxy:
  `XASnapshotItf::InitiateSnapshot`. Vulkan activado por defecto (`debug.heddle.vulkan=0` lo desactiva).
- Modelo de CPU (`src/feat.rs`): decide **lo que el guest ve** — MIDR, MPIDR, `CTR_EL0`, `DCZID_EL0`, registros
  `ID_AA64*` (como los sanea Linux en EL0), `AT_HWCAP`/`AT_HWCAP2`, `/proc/cpuinfo`, `/proc/self/auxv` y
  `/sys/devices/system/cpu/cpuN/regs/identification/{midr,revidr}_el1` —, y con ello qué se ejecuta: heddle sigue la
  especificación Arm ARM y ejecuta lo que el modelo anuncia y él implementa; una instrucción de una extensión no
  anunciada es SIGILL como en ese procesador (logcat: `ausente en el modelo de CPU ...`), y lo que no implementa es
  SIGILL con cualquiera (el diagnóstico lo nombra). Se elige un modelo del catálogo (`debug.heddle.cpu`/
  `HEDDLE_CPU`, limitado a lo que heddle implementa) o un **perfil** libre (`/system/etc/heddle/cpu.conf`, o
  `debug.heddle.cpu_file`/`HEDDLE_CPU_FILE`) que se anuncia tal cual aunque pida extensiones que heddle no tiene (lo
  avisa una vez en logcat). Formato del perfil: [`docs/perfil-cpu.md`](docs/perfil-cpu.md). PAuth no está
  implementada: `RETAA`, `BRAA`, `LDRAA`, `PACGA`... son SIGILL; las pistas (`PACIASP`, `AUTIASP`, `XPACLRI`) son NOP
  como en un procesador sin PAuth.

  | Modelo | MIDR | Arquitectura | Extensiones (`Features`) | Ejemplos |
  |---|---|---|---|---|
  | `cortex-a53` | 0x410FD034 (r0p4) | ARMv8.0 | fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid | Snapdragon 625/450, Exynos 7870 |
  | `cortex-a55` | 0x412FD050 (r2p0) | ARMv8.2 | las de A53 + atomics fphp asimdhp asimdrdm lrcpc dcpop asimddp | núcleos pequeños 2018-2023 |
  | `cortex-a76` | 0x414FD0B1 (r4p1) | ARMv8.2 | como A55 | Snapdragon 855, Kirin 980 |
  | `cortex-a77` | 0x411FD0D0 (r1p0) | ARMv8.2 | como A55 | Snapdragon 865, Dimensity 1000 |
  | `cortex-a78` (por defecto) | 0x411FD411 (r1p1) | ARMv8.2 | como A55 | Snapdragon 888 (Kryo), Dimensity 1200, Tensor |
  | `cortex-x1` | 0x411FD440 (r1p0) | ARMv8.2 | como A55 | Snapdragon 888, Tensor, Exynos 2100 |
  | `max` | 0x000F0000 | — | todo lo que implementa heddle: además sha3 sha512 asimdfhm jscvt fcma flagm flagm2 sb dcpodp frint dgh | solo pruebas (`heddle-difftest` lo usa por defecto) |

  El catálogo no incluye ARMv9 (A710/X2 y posteriores: PAuth, BTI, SVE2, MTE, BF16, I8MM, que heddle no
  implementa); un perfil puede anunciarlos. Todas las CPU en línea muestran el mismo modelo (en el dispositivo, big.LITTLE mezcla varios). Lo no anunciado
  (SVE/SME, PAuth, BTI, MTE, RNG, LRCPC2/3, LSE2…) o no accesible en EL0 es SIGILL.
- Sin implementar: SM3/SM4, I8MM/BF16, `clone` con CLONE_THREAD,
  `long double`/complejos de libm, `strtold`/`wcstold` y las conversiones `%L` de printf/wprintf/scanf/wscanf van en
  una biblioteca ARM incrustada (`guest/`, libm y gdtoa/vfprintf de bionic: resultados bit a bit los de bionic arm64);
  el resto de libm y de stdio es la del host.

## Compilar
```bash
cargo build --release          # heddle-run, heddle-difftest, heddle-fpone, libheddle.so (x86-64 Linux)
./target/release/heddle-run lib.so simbolo [args]   # ver "Binarios"
```
Android: `cargo build --release --target x86_64-linux-android --lib` con el NDK como linker (ver
`.github/workflows/build.yml`: `CARGO_TARGET_X86_64_LINUX_ANDROID_LINKER=<NDK>/toolchains/llvm/prebuilt/linux-x86_64/bin/x86_64-linux-android26-clang`).
La biblioteca guest (`long double`/complejos exactos, también en `strtold` y printf/scanf con `%L`) se compila antes
con `guest/build.sh` (ver el CI).

## Instalación en Android x86_64
Requiere una imagen en la que se pueda escribir `/system` y `/vendor` (emulador o máquina virtual con root):
1. Copiar `libheddle.so` (del paquete `heddle-android-x86_64.tar.gz`) a `/system/lib64/libheddle.so`, con los
   permisos y la etiqueta SELinux del resto de `/system/lib64`.
2. Añadir a `/vendor/build.prop`:
   ```
   ro.dalvik.vm.native.bridge=libheddle.so
   ro.dalvik.vm.isa.arm64=x86_64
   ro.product.cpu.abilist=x86_64,arm64-v8a
   ro.product.cpu.abilist64=x86_64,arm64-v8a
   ```
3. Reiniciar. El zygote carga el puente al arrancar: `grep libheddle /proc/$(pidof zygote64)/maps` lo confirma, y
   al abrir una app arm64 aparece en logcat `libheddle.so cargada (pid=..., version=8, heddle=vX.Y.Z, build=...)`
   (`version=8` es la de la interfaz NativeBridge; `heddle=` la de la release).

Si el arranque falla, quitar las cuatro líneas y reiniciar. [weft](https://github.com/43fdfdg45454/weft-new) automatiza
estos pasos (`weft bridge install`). Las sumas de cada release están en `SHA256SUMS`.

## Propiedades y variables
En Android se leen las propiedades `debug.heddle.*` (`adb shell setprop ...` y reiniciar la app, o el dispositivo si no surte efecto); fuera de Android,
o para forzarlas, las variables `HEDDLE_*` equivalentes, que tienen prioridad.

| Propiedad | Variable | Efecto |
|---|---|---|
| `debug.heddle.rss_limit_mb` | `HEDDLE_RSS_LIMIT_MB` | Límite de memoria residente del vigilante (MiB). Por defecto el 60 % de la RAM, máximo 3 GiB; `0` lo desactiva. |
| `debug.heddle.samp` | `HEDDLE_SAMP` | `0`: apaga los registros periódicos (cada 6 s) del muestreador `heddle-samp` (hilos sin avanzar, `monitor:`, `memoria:`, llamadas HLE). El vigilante de memoria sigue activo: con `0` solo se registra lo fatal (`VIGILANTE`). |
| `debug.heddle.prof` | `HEDDLE_PROF` | `1`: perfilador (cada 30 s, en logcat, dónde se va la CPU: código guest, HLE, syscalls, traducción). |
| `debug.heddle.vulkan` | `HEDDLE_VULKAN` | `0`: desactiva Vulkan a través del puente (el guest no ve `libvulkan.so`; la app usa GLES). Activado por defecto. |
| `debug.heddle.cpu` | `HEDDLE_CPU` | Modelo de CPU que ve el guest: `cortex-a53`, `cortex-a55`, `cortex-a76`, `cortex-a77`, `cortex-a78` (por defecto), `cortex-x1` o `max` (todo lo que implementa heddle; para pruebas). Se acepta sin el prefijo (`a55`). Tiene prioridad sobre el perfil. Queda en logcat (`cpu: modelo=...`). |
| `debug.heddle.cpu_file` | `HEDDLE_CPU_FILE` | Perfil de CPU a leer en lugar de `/system/etc/heddle/cpu.conf` (formato: `docs/perfil-cpu.md`). |
| `debug.heddle.monitor` | `HEDDLE_MONITOR` | Fuerza el modo del monitor LDXR/STXR: `rseq`, `fence` o `bloqueo` (por defecto se elige solo). |
| `debug.heddle.page_size` | `HEDDLE_PAGE_SIZE` | Tamaño de página que ve el guest: `4096` (por defecto) o `16384`, como un dispositivo arm64 con páginas de 16 KiB (`getpagesize`, `sysconf(_SC_PAGESIZE)`, `getauxval(AT_PAGESZ)`, alineación exigida por `mmap`/`munmap`/`mprotect`/`mremap`/`madvise`/`msync` y cargador alineado a 16 KiB). |
| `debug.heddle.page_compat` | `HEDDLE_PAGE_COMPAT` | Con páginas de 16 KiB, modo de compatibilidad de bionic (Android 15+) para bibliotecas con `p_align` de 4 KiB. Por defecto se decide como Android 16: `android:pageSizeCompat` del manifiesto o, sin él, si alguna biblioteca de la app está alineada a 4 KiB. `1` lo fuerza (como la propiedad de bionic `bionic.linker.16kb.app_compat.enabled`); `0` lo desactiva (el ajuste del usuario). Sin el modo, esas bibliotecas no se cargan, como en el dispositivo. |
| — | `HEDDLE_APK`, `HEDDLE_NATIVE_LIB_DIR` | Fuera de Android, la app para esa decisión: sus APK (separados por `:`, el primero el base) y el directorio de sus bibliotecas extraídas. En Android sale de `ApplicationInfo`; sin nada, de la ruta de la biblioteca. |
| — | `HEDDLE_PGSIZE_MIGRATION` | `1`/`0`: el kernel admite (o no) la migración de tamaño de página (por defecto, `/sys/kernel/mm/pgsize_migration/enabled`); con ella el cargador lee la nota `NT_ANDROID_TYPE_PAD_SEGMENT`. |
| — | `HEDDLE_TARGET_SDK` | `targetSdkVersion` que usa el cargador para las compatibilidades de bionic (bibliotecas sin DT_SONAME, ...). En Android, por defecto el de la app (lo fija ART); fuera de Android, 35. El guest lo ve en `android_get_application_target_sdk_version`. |
| — | `HEDDLE_LIBPATH` | Directorios adicionales (separados por `:`) donde buscar bibliotecas guest en el espacio de nombres por defecto (su `LD_LIBRARY_PATH`). |
| — | `HEDDLE_SAMPLER` | Fuera de Android, activa el muestreador `heddle-samp` (en Android siempre está activo). |
| — | `HEDDLE_NOCHAIN`, `HEDDLE_NOREGS` | Desactivan el encadenado de bloques y la caché de registros del JIT (diagnóstico). |
| — | `HEDDLE_STATS` | `heddle-run`: al terminar, cuántas instrucciones del JIT pasaron por el intérprete. |

Al compilar: `HEDDLE_GUEST_LIB` (ruta de la biblioteca guest a incrustar; por defecto `guest/out/libheddle_guest.so`)
y `HEDDLE_VERSION` (versión que se incrusta; la fija el CI).

## Depuración
- Todo va a logcat con la etiqueta `heddle`: `adb logcat -s heddle`. Los fallos (`FALLO`, `FILTRACION`, `VIGILANTE`,
  abortos) van con prioridad ERROR: `adb logcat heddle:E *:S` muestra solo eso.
- `FILTRACION`: el código ARM intentó ejecutar una dirección del host (un puntero a función que el puente no pudo
  convertir); el mensaje nombra su origen. `simbolo '...' no disponible`: la app importa una función que el puente
  no reenvía (ver `docs/firmas-ndk.md`).
- `fallo guest`/`FALLO senal`: volcado con registros guest, pila de llamadas, módulo y desplazamiento, y las palabras
  de código en `pc` (decodificables con `llvm-mc -triple=aarch64 --disassemble`).
- `VIGILANTE`: la app superó `debug.heddle.rss_limit_mb` y se terminó. Cada 6 s el muestreador anota la memoria
  (`memoria:`) y las funciones HLE más llamadas.
- Instrucción que heddle no ejecuta: el guest recibe `SIGILL` (`ILL_ILLOPC`), como en un procesador ARM sin esa
  extensión, y el mensaje la nombra: `instruccion no soportada por heddle: SM4E (0xcec08400) [FEAT_SM4; SIMD y FP]`
  (SVE y SME se nombran solo como grupo); `instruccion indefinida en ARMv8/ARMv9` para `UDF` y los grupos no
  asignados; `instruccion no reconocida por heddle` si no está en la tabla (`src/diag.rs`). Si el guest tiene manejador
  de `SIGILL` (sondeo de extensiones), las 16 primeras quedan con prioridad INFO; si no, es un `fallo guest`.
- Las llamadas al sistema del guest aparecen con su nombre arm64 junto al número (`syscall arm64 436 (close_range)
  sin traduccion (ENOSYS)`, `ultimo_sc(arm64)=98 (futex)` en el muestreador, `perfil svc:`).
- En Linux, `heddle-run lib.so simbolo [args]` ejecuta una función de una biblioteca arm64 directamente (ver
  [Binarios](#binarios)).

## Binarios
Los tres aceptan `-h`/`--help` y `-V`/`--version` (`<binario> vX.Y.Z (build <commit>)`, la misma versión que
`libheddle.so`). Códigos de salida comunes: `0` bien, `2` uso incorrecto (opción o argumento no válidos), `1` error
(archivo no encontrado, ELF no válido, símbolo inexistente, casos truncados).

| Binario | Uso | Salida |
|---|---|---|
| `heddle-run` | `heddle-run BIBLIOTECA SIMBOLO [ARG...]`: carga una biblioteca arm64 y llama a la función (enteros decimales o `0x...` en x0-x7 y la pila; dobles con sufijo `d`, p. ej. `1.5d`, en d0-d7). Imprime x0 y d0. | El estado del guest: `x0 & 0xff` (como el retorno de `main`) o el valor de `exit()`. Si el guest muere por una señal, heddle-run muere por esa misma señal (no la convierte en un código): el shell ve `128+N` (`132` para `SIGILL`) y quien lo lance ve la señal real. `2`/`1` para los errores propios, que pueden coincidir con un estado del guest: el mensaje en stderr (`heddle-run: ...`) los distingue. |
| `heddle-difftest` | `heddle-difftest [--engine interp\|jit] [--show N] CASOS.bin`: compara el intérprete o el JIT con los casos de `difftests/tools/gen_cases.py` (Unicorn). | `0` todos coinciden, `1` hay diferencias o error. |
| `heddle-fpone` | `heddle-fpone PALABRA FPCR [xN=HEX\|vN=ALTO_BAJO...]`: ejecuta una instrucción FP/NEON suelta en el intérprete. | `0` aunque la instrucción sea indefinida (lo dice la salida, con su nombre). |

## Releases
Cada push a `main` que pasa el CI publica `vX.Y.Z` con `heddle-android-x86_64.tar.gz`, `heddle-linux-x86_64.tar.gz`
y `SHA256SUMS`. Los cambios están en `CHANGELOG.md`.

La versión la calcula [GitVersion](https://gitversion.net) (`GitVersion.yml`, job `semver`): en `main` es la última etiqueta
`vX.Y.Z` más un parche; un commit con `+semver: minor` o `+semver: major` en el mensaje sube la versión menor o la mayor
(`+semver: none` no la sube). En otras ramas y en los PR sale `X.Y.Z-rama.N`, que solo se incrusta en el binario.

## Estructura
`src/` (núcleo), `difftests/` (pruebas guest y diferenciales), `bench/` (microbenchmarks), `guest/`, `tools/sigtool/`.
Ver `docs/ESTRUCTURA.md`.

## Licencia
MIT, `Copyright (c) 2026 43fdfdg45454` (ver `LICENSE`). El código de terceros que se incluye o se incrusta en los
binarios (el `qsort` traducido de FreeBSD, la libm de bionic de la biblioteca guest, la biblioteca estándar de Rust)
está en `NOTICE`; cada paquete de release trae los textos completos en `LICENSES/`.
