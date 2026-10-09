# Cambios

Cada push a `main` que pasa el CI publica `vX.Y.Z` (GitVersion, ver README); las entradas se agrupan aquí por la primera versión que las
incluye. La versión instalada aparece en logcat al cargar la biblioteca (`libheddle.so cargada (... heddle=vX.Y.Z,
build=<commit>)`) y en `heddle-run --version`.

## v0.1.0

Primera versión publicada.

- Traductor ARM64 → x86-64: intérprete de referencia y JIT (caché de registros, encadenado de bloques, TLSDESC en
  línea), validados contra Unicorn/QEMU con casos aleatorios. Extensiones según el modelo de CPU elegido
  (`HEDDLE_CPU`, catálogo de modelos reales o perfil `cpu.conf`); lo no anunciado es SIGILL, como en ARM.
- Monitor exclusivo LL/SC exacto con tres modos (`rseq`, `fence`, `bloqueo`) elegidos según el kernel.
- `NativeBridgeItf` v8 para Android x86_64: cargador ELF como el enlazador de bionic (espacios de nombres,
  `android_dlopen_ext`, versiones de símbolos, TLS dinámico, páginas de 16 KiB y su modo de compatibilidad), JNI y
  frontera con firmas generadas desde las cabeceras del NDK (`tools/sigtool`).
- Proxys para OpenSL ES, OpenMAX AL, `ANativeActivity`, estructuras de callbacks, zlib, Vulkan (activado por defecto),
  `glob` y liblog.
- Señales como el kernel: fallos síncronos con pc exacto y `ucontext` escribible, señales asíncronas en frontera de
  instrucción, hasta 8 niveles anidados, señales de tiempo real encoladas, `siglongjmp` desde una señal dentro de una
  llamada al host en un punto seguro y estados de emergencia para hilos sin estado guest.
- Biblioteca guest ARM64 incrustada para `long double` exacto (libm, `strtold`, printf/scanf `%L`).
- Binarios `heddle-run`, `heddle-difftest` y `heddle-fpone`; paquetes para Linux y Android x86_64 con `SHA256SUMS`.
