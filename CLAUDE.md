# Lineamientos de heddle: seguridad de punteros y memoria

Este archivo es obligatorio para cualquier cambio en `src/`. Las reglas marcadas **[lint]** las comprueba
`tests/arch_lint.rs`; las marcadas **[tipo]** las impone el compilador. Las demás dependen de quien escribe el código.

`cargo build --release && cargo test --release --lib --bins --test arch_lint --test cli` debe pasar antes de cada commit.
El CI ejecuta esas pruebas y no publica si fallan.

## Modelo

Hay dos mundos en un mismo proceso:

- **guest**: código ARM64 de la app. Se lee y se traduce; **nunca se mapea ejecutable**.
- **host**: código x86-64 (bionic, ART, drivers, el propio puente).

El puente **no es un sandbox**: el guest comparte el espacio de direcciones y puede leer y escribir memoria como
cualquier biblioteca nativa. Lo que sí se garantiza es el control de **qué se ejecuta y con qué ABI**.

## Punteros: invariantes de la frontera

Toda conversión entre los dos mundos vive en `src/boundary.rs`.

1. **El guest nunca ejecuta una dirección del host.** Lo único ejecutable para el guest es código ARM de sus módulos,
   regiones que él mismo pidió ejecutables, slots HLE y el resolutor TLSDESC del puente (código ARM fijo, `tls.rs`). Un salto a código del host es una filtración: termina en
   `boundary::guest_jumped_to_host`, que **no llama a nada** y entrega un fallo con diagnóstico (`FILTRACION: ...`).
   No reintroducir una "llamada nativa perezosa" en ese camino. **[lint]**
2. **El host nunca ejecuta una dirección guest.** Si la llama, el fallo de ejecución se redirige a un trampolín
   (`sig::try_exec_redirect`), y solo si `boundary::is_guest_executable` lo confirma. **[lint]**
3. **Solo se llama al host con un `HostFn`**, y solo se reenvía una función con firma (ver más abajo). El campo es privado; no existe conversión desde un entero. Nace de:
   - `HostFn::symbol(nombre)`: símbolo de una biblioteca **permitida**;
   - `HostFn::from_host(p)`: puntero que el propio host entregó, validado como código del host.
   `hostcall::call` y `Call::new` solo se usan en `boundary.rs`. **[tipo] [lint]**
4. **Búsqueda de símbolos cerrada por defecto.** `dlsym`/`dlopen` solo en `boundary.rs`, y solo contra
   `ALLOWED_LIBS`. Un símbolo que solo exista en otra biblioteca del proceso no se entrega. **[lint]**
5. **El reenvío universal no convierte nada por el aspecto del valor**, ni argumentos ni retorno. Un registro no
   tiene tipo: un entero puede valer exactamente la dirección de una función del host (un guest que lee
   `/proc/self/maps` con `strtoull`) y un puntero a envoltorio puede ser un dato. Los punteros a función devueltos
   por el host se convierten solo donde el tipo se conoce: `RET_FN` (familia `GetProcAddress`) y JNI. Uno que llegue
   por otra vía queda crudo y la regla 1 impide ejecutarlo. **[lint]**
6. **Un `JNIEnv*`/`JavaVM*` guest solo se sustituye por el real donde el tipo es conocido**: las funciones JNI y el
   argumento que la tabla de firmas marca con `env` (`K::Direct { env }` en `src/sigs_gen.rs`, aplicado por
   `boundary::typed_slot`: libandroid, libjnigraphics, libnativehelper…). El reenvío
   universal **no toca los argumentos**: un envoltorio que el guest pasa como dato (`pthread_setspecific`,
   `memcpy`) debe llegar intacto, o el guest recuperará un puntero del host.
7. **No hay conversiones por heurística sobre argumentos.** Un argumento que "parece" código guest puede ser una
   cadena dentro de un segmento ejecutable; convertirlo corrompe datos. Los callbacks se convierten cuando el tipo
   es conocido (`cbs` de la tabla de firmas, `RegisterNatives`) o, si no, en el momento en que el host los ejecuta (regla 2). **[lint]**

### Punteros a función en objetos y estructuras: proxys

Los punteros a función que el host **escribe en memoria** (parámetros de salida, campos de estructuras, tablas de
funciones de objetos) no pasan por un registro en la frontera: nunca se ejecutan (regla 1, `FILTRACION` nombrando el
origen). Las API públicas del NDK que los usan se sirven con **proxys explícitos** que conocen el tipo de cada campo
(regla 7: nada se convierte por el valor):

- **OpenSL ES y OpenMAX AL** (`src/proxy.rs`): el guest recibe objetos proxy (entradas de `proxy::ENTS`) cuya tabla
  son slots HLE, uno por método, con la firma de las cabeceras arm64 (`sigs::ITFS`, generada). Cada slot desenvuelve
  el proxy a la interfaz real, desenvuelve las interfaces que llegan como argumento, envuelve las de salida
  (`CreateAudioPlayer`, `GetInterface`...), convierte los callbacks con un trampolín con firma (la interfaz `caller`
  que pasa el host llega al guest como su proxy: carácter `O` del trampolín) y los `SLDataSource`/`SLDataSink` cuyo
  localizador lleva un objeto (OUTPUTMIX, IODEVICE: copia con el objeto real), reparte AAPCS64 → SysV (coma flotante
  y pila incluidas) y llama al método real con un `HostFn` validado. `Destroy` libera los proxys del objeto.
  `SL_IID_*`/`XA_IID_*` son los datos del host; `GetInterface` elige la tabla comparando el contenido del IID.
- **`ANativeActivity`** (`src/ndkcb.rs`): `ANativeActivity_onCreate` recibe una copia (proxy) con el JavaVM/JNIEnv
  guest y una tabla de callbacks propia que el guest rellena cuando quiera; en la tabla real hay despachadores que, en
  cada evento, llaman al callback actual del guest con su proxy. La estructura del host no se toca. Las funciones
  `ANativeActivity_*` desenvuelven el proxy; `onDestroy` lo libera. Se reconoce por el nombre del símbolo
  `ANativeActivity_onCreate` o, con un `android.app.func_name` propio, como lo pide NativeActivity:
  `loadNativeCode_native` llama a `NativeBridgeGetTrampoline(handle, func_name, NULL, 0)` (firma nula; ART solo pide
  así `JNI_OnLoad`/`JNI_OnUnload`) y el nombre y la biblioteca (`lib<android.app.lib_name>.so`, por defecto `main`)
  coinciden exactamente con los `metaData` de una actividad del paquete, leídos una vez por JNI con
  `PackageManager.getPackageInfo(paquete, GET_ACTIVITIES | GET_META_DATA)` (`ndkcb::is_activity_entry`).
- **Estructuras de callbacks que el host copia** (`sigs::STRUCT_COPIED`, escrita a mano; disposición y firma de cada
  campo en `sigs::STRUCT_CBS`, generada): cámara, `AImageReader`, `utrans_*`. `typed_slot` pasa al host una copia con
  cada puntero convertido (trampolín determinista: la baja de un callback compara bien). A mano:
  `AMediaCodec_setAsyncNotifyCallback` (estructura de 32 bytes por valor: puntero en AAPCS64, pila en SysV) y
  `timer_create` con `SIGEV_THREAD` (la función está en una unión: solo se convierte si `sigev_notify` lo dice).

- **Estructuras que el host guarda por su dirección** (`sigs::STRUCT_INPLACE`, escrita a mano: `z_stream` de zlib,
  que comprueba `strm->state->strm == strm` y lee `zalloc`/`zfree` en cada llamada): durante la llamada sus punteros
  a función se sustituyen en su sitio (`boundary::InPlace`) y al volver se repone lo del guest; si el host escribió
  otro (el `zcalloc` por defecto de `*Init`, la copia de `deflateCopy`), el guest recibe ese puntero convertido (slot
  o su función guest). Sin asignador propio el coste es leer dos campos nulos.
- **Vulkan** (`src/vk.rs`, tablas generadas `sigs::VK_FNS`, `VK_TYPES`, `VK_STYPES`, `VK_ALLOC`): `pAllocator` de toda
  función que lo reciba se pasa como copia con las cinco funciones convertidas (los trampolines son permanentes: el
  cargador guarda la copia y la usa después, desde cualquier hilo; el de `vkDestroy*` es compatible con el de la
  creación). Qué estructuras de entrada pueden llevar callbacks lo decide `tools/sigtool` con `structextends` y `len`
  de `vk.xml` (punto fijo: callbacks propios, punteros const a otras estructuras —anidadas, arreglos con su cuenta,
  arreglos de punteros, anidadas por valor con cadena— y, en la base de una cadena, las estructuras que pueden ir en
  ella). Con el registro 1.3.275 solo `vkCreateInstance` (`VkDebugUtilsMessengerCreateInfoEXT`,
  `VkDebugReportCallbackCreateInfoEXT`, `VkDirectDriverLoadingListLUNARG` → `pDrivers`), `vkCreateDevice`
  (`VkDeviceDeviceMemoryReportCreateInfoEXT`) y la creación de los dos mensajeros tienen `args`; el resto (dibujo,
  comandos, pipelines…) no tiene conversión y su slot es el reenvío directo (0 ns de más). En las que sí, si algo
  lleva un callback, el host recibe copias convertidas hasta el último nodo que lo necesita. Un `sType` desconocido
  (sin tamaño) antes de ese nodo no se copia: se cambia en su sitio su `pNext` para que apunte a la copia y se repone
  al volver (`vk::Keep`, como `InPlace` de zlib; si es memoria de solo lectura, esa cadena queda sin convertir y se
  registra). El registro es el extracto versionado `tools/sigtool/vk_extracto.xml`; sin registro el generador sería
  conservador (cualquier estructura en cualquier cadena: se recorren todas).
  Las extensiones que llegan por `vk*GetProcAddr` usan la misma tabla por su nombre.
- **`glob` con `GLOB_ALTDIRFUNC`** (`ndkcb::glob`): el host recibe una copia de `glob_t` (88 bytes, igual en las dos
  arquitecturas) con `gl_opendir`/`gl_readdir`/`gl_closedir` convertidos y `gl_stat`/`gl_lstat` envueltos (llaman al
  guest con un `struct stat` arm64 y lo convierten); los resultados vuelven al `glob_t` del guest. Sin la bandera, los
  campos no se tocan. `gl_errfunc` va siempre con su firma.
- **liblog**: `__android_log_set_logger` instala en el host un despachador propio (`ndkcb::heddle_logger`) que llama
  al registrador del guest (trampolín con firma, o la función del host de un slot: `__android_log_logd_logger` llega
  como la del host); los mensajes del propio puente (`bridge::alog`, marcados por hilo) van siempre a
  `__android_log_logd_logger` y nunca al guest. `__android_log_set_aborter` recibe un trampolín con firma. El host los
  llama desde cualquier hilo: el trampolín crea el estado guest del hilo como en los callbacks de audio. Ninguna
  función pública devuelve el registrador o el abortador (no hay puntero del host que llegue al guest).

Sin proxy: los métodos de `sigs::ITFS` con `bad` (`XASnapshotItf::InitiateSnapshot`, estructura por valor con un
callback: responde `FEATURE_UNSUPPORTED`; en ARM funcionaría) y lo que `docs/firmas-ndk.md` lista en "Vulkan:
pAllocator y cadenas pNext" (`VkDirectDriverLoadingInfoLUNARG::pfnGetInstanceProcAddr`: callback que devuelve un
puntero a función; la lista y su arreglo se copian, pero ese campo va tal cual y se registra).

### La frontera es la API del NDK, con firmas generadas

El reparto es: **cómputo puro en el guest** (código ARM traducido), **recursos del sistema en el host** (kernel,
gráficos, audio, vídeo, ventanas, JNI, y el estado global del proceso: hilos, TLS, señales, `malloc`). El puente
depende solo de interfaces estables: la API pública en C del NDK, JNI y las llamadas al sistema de Linux. No depende
de detalles internos de una imagen de Android concreta ni de bibliotecas ARM del sistema.

`resolve()` solo entrega al guest una función del host si:

1. tiene una implementación propia (HLE) en `libc_hle.rs`/`jni.rs`, o
2. su firma está en `src/sigs_gen.rs` y es `K::Direct`.

Todo lo demás no está disponible (queda registrado el motivo). **[lint]**

`src/sigs_gen.rs` lo genera `tools/sigtool` analizando las cabeceras del NDK con el compilador para arm64 y x86-64.
**No se edita a mano.** Para cada función decide por su tipo:

| Lo que ve el generador | Resultado |
|---|---|
| Enteros, punteros a datos, `float`/`double`; estructuras apuntadas con la misma disposición en ambas arquitecturas | `Direct` |
| Argumento puntero a función | `Direct` con `cbs`: el puente crea el trampolín con el retorno declarado |
| Argumento `JNIEnv*`/`JavaVM*` | `Direct` con `env`: se pasa el entorno real del host |
| Argumento `bool`/`char`/`short` | `Direct` con `narrow`: se extiende (el host lo asume extendido; arm64 no lo garantiza) |
| Retorno puntero a función | `Direct` con `ret_fn`: el guest recibe un slot |
| `long double`, complejos, `va_list`, estructura por valor mayor de 16 bytes o con coma flotante | `Unsafe` |
| Estructura apuntada con distinta disposición (también en parámetros de callbacks), objeto con tabla de funciones del host | `Manual` |
| Exportada sin declaración en las cabeceras | `NoSig` (añadir la firma a `tools/sigtool/extra.h`) |
| Estructura `...Itf_` hecha solo de punteros a función (OpenSL ES, OpenMAX AL) | `ITFS`: cada método con sus argumentos (`A::V`, `Itf`, `ItfOut`, `Cb` con la firma del callback, `Data`); `bad` si no se puede reenviar |
| Argumento (puntero o valor) a una estructura con punteros a función | `STRUCT_CBS`: desplazamiento y firma de cada callback; `note` si está en una unión, detrás de otro puntero o difiere |

Para regenerar (requiere libclang y `llvm-readelf`; el sysroot del NDK lo publica el flujo de CI `sysroot`). Las
cabeceras en C no dicen qué estructura puede ir en la cadena `pNext` de cuál: eso sale del registro de Vulkan, del que
se versiona un extracto mínimo (`tools/sigtool/vk_extracto.xml`, registro 1.3.275 —la versión de las cabeceras del
NDK—, generado con `tools/sigtool/vkextract.py`: solo `VK_HEADER_VERSION`, estructuras con `structextends`, campos
con `len` y parámetros de los comandos) que sigtool lee por defecto; `SIGTOOL_VK_XML` lo sustituye por otro (completo
o no), y si las versiones no coinciden el informe lo dice. Al actualizar el NDK, regenerar el extracto con el vk.xml
de su versión:

```
cd tools/sigtool && cargo build --release
SIGTOOL_EXTRA=$PWD/extra.h ./target/release/sigtool <sysroot> ../../src/sigs_gen.rs ../../docs/firmas-ndk.md
```

La misma herramienta genera `sigs::EXPORTS`: cada símbolo exportado por cada biblioteca del NDK con su versión (el
cargador atribuye así cada símbolo del sistema a todas las bibliotecas que lo exportan y compara versiones).
`docs/firmas-ndk.md` es el informe generado: cobertura por biblioteca y la lista de lo que no se reenvía y por qué.
`cargo test --release --lib huecos -- --ignored --nocapture` lista las funciones sin cubrir.

### Biblioteca guest: solo lo inevitable

Lo que no se puede reenviar y es cómputo puro no se aproxima en el puente: va en una biblioteca **ARM64** pequeña
(`guest/`), incrustada en `libheddle.so`, que el guest ejecuta como código traducido, sin frontera.

- Exporta únicamente: las funciones de `libm.so` que la tabla marca `Unsafe` (`long double` de 128 bits y
  complejos); las de `libc.so` que marca `Unsafe("retorno long double")` (`strtold`, `strtold_l`, `wcstold`,
  `wcstold_l`); y los internos `__heddle_*`, que `resolve()` no entrega al guest: `__heddle_snprintf_ld`, el
  `vfprintf` de bionic para **una** conversión `%L[aAeEfFgG]`, y `__heddle_strtold_fast`/`__heddle_strtold_slow`
  (cada ruta de `strtold` por separado, para las pruebas). La lista de exportación se genera desde `src/sigs_gen.rs`; un test
  (`guestlib::tests::solo_lo_inevitable_va_al_guest`) comprueba que coincide exactamente.
- printf/scanf con `long double` (`src/ldio.rs`): el host formatea y lee todo lo demás y solo las conversiones `%L`
  van a la biblioteca guest. printf/wprintf: cada conversión con el `snprintf`/`swprintf` del host, las `%L` con
  `__heddle_snprintf_ld`. scanf/wscanf: el formato se parte; los tramos sin `%L` los lee el scanf del host (con un
  `%lln` final para saber cuánto consumió, los espacios del formato fuera y sus `%n` corregidos) y cada `%L` se lee
  como CT_FLOAT de `vfscanf`/`vfwscanf` de bionic (`parsefloat` y `strtold`/`wcstold` guest), con el `FILE` bloqueado.
  Valor devuelto y `%n` con las reglas de bionic. Sin `%L` el camino es el de antes (variádicas reenviadas al host).
- Rutas rápidas, con los mismos bits que bionic y solo en redondeo al más cercano (gdtoa usa `FLT_ROUNDS`):
  `strtold` (y con ella `wcstold` y scanf `%L`) calcula en el guest un decimal simple de hasta 34 cifras
  significativas por 10^k con |k| <= 48 con **una** multiplicación o división binary128 correctamente redondeada
  (`strtold_fast` en `stdio_ld.cpp`); printf `%L` de un valor que es exactamente un `double` lo formatea el host como
  ese `double` (`ldio::fmt_ld_fast`; no `%a` con precisión ni de un subnormal de `double`, ni la bandera `'`). Lo
  demás va por gdtoa/vfprintf de bionic. `guestlib::tests::rutas_rapidas_de_long_double_como_bionic` compara las dos
  rutas en un barrido aleatorio (`HEDDLE_LD_SWEEP=n` lo agranda) e informa de la fracción que toma la rápida.
- Fuente: la libm y, de la libc, gdtoa, `stdio/vfprintf.cpp`, `stdio/parsefloat.c` y `fvwrite.c` de bionic en el
  commit de `guest/BIONIC_COMMIT`, compiladas con `guest/build.sh` (el CI lo hace antes de `cargo build`; `build.rs`
  incrusta el resultado); `guest/stdio_ld.cpp` replica los envoltorios de bionic que no se compilan tal cual
  (`strtold.cpp`, `stdlib_l.cpp`, `wcstod.cpp`, `vsnprintf`). Sin ella el puente compila igual y usa aproximaciones
  en `double`. Referencia de las pruebas (`difftests/tests/guest/tld.c`): la bionic arm64 del NDK con qemu
  (`difftests/tools/tld_ref.sh`).
- Lo que la propia biblioteca necesita y sí es reenviable (`sqrt`, `exp`… de `double`, `malloc`, `iswspace`…) lo
  importa del host por la vía normal. No se mueve al guest nada que el host pueda servir. Dos excepciones internas,
  porque el host no sirve igual: el mutex de gdtoa (`misc.c` lo guarda en una ranura de 8 bytes; es el mutex normal
  de bionic, palabra de estado y futex, en `guest/include/thread_private.h` y `stdio_ld.cpp`) y
  `nl_langinfo(RADIXCHAR)` de `vfprintf` (`"."` como en bionic; la numeración de los items depende de la libc del
  host).
- Se carga desde memoria (`memfd_create`, por su descriptor con `ANDROID_DLEXT_USE_LIBRARY_FD`: reabrir
  `/proc/self/fd/N` falla en las apps de Android); si el kernel no lo ofrece, desde un archivo en el directorio
  privado de la app.
- En `resolve()` tiene prioridad sobre las implementaciones aproximadas del puente para esos nombres.

### Lo que el tipo no dice: lista escrita a mano

El generador no ve la **semántica**. Estas clases tienen implementación propia o están en `DENY`, aunque su firma
parezca directa: hilos (`pthread_create`, `pthread_exit`, claves), señales (`sigaction`, `signal`, `sigaltstack`),
saltos no locales (`setjmp`/`longjmp`), procesos (`fork`, `vfork`, `clone`), enlazador (`dl*`), memoria
(`mmap`/`mprotect`/`munmap`: nunca ejecutable), TLS, `getauxval`, propiedades de ABI, `fenv` (estado FP emulado),
`getcontext`/`backtrace`, con páginas de 16 KiB `mincore`, `mlock`, `munlock`, `mlock2`, `brk`, `sbrk` y `fopen`
(`/proc/self/maps`: ver "Lo que no está garantizado"), `__pthread_cleanup_push`/`__pthread_cleanup_pop` (cadena por hilo de estructuras del guest;
`pthread_exit`, también al volver la función del hilo, ejecuta como bionic: destructores de `thread_local`, la
cadena del más reciente al más antiguo desenlazando antes cada uno, y destructores de claves; `pop` con `execute`
la ejecuta como código guest), la llamada al sistema `exit` del guest (`svc`; como el núcleo en ARM, no ejecuta código
guest: ni destructores de `thread_local` —su referencia al DSO queda tomada, como en bionic—, ni la cadena de cleanup,
ni destructores de claves; solo se suelta `GuestThread` con los destructores de TLS del host y `pthread_join` recibe
NULL; en el hilo principal, la llamada al sistema directa tras soltar el estado: `libc_hle::thread_exit_syscall`),
`glob` (proxy), la familia printf (también wprintf, las `_chk` y lo que formatea con ellas: `__android_log_print`,
`syslog`, `err`/`warn`) con `%n`: aborta como bionic (`fmt::printf_n_fatal`: `FORTIFY: %n not allowed on Android` en
stderr, en logcat con prioridad FATAL y etiqueta `libc`, `android_set_abort_message` y `abort()`), también cuando el
formato iría al printf del host (glibc lo escribiría), apertura de los archivos que describen la CPU (`open*`,
`fopen`/`freopen`: `procemu`). Al añadir una función a esta clase, documentarla aquí.

También a mano, en los proxys: `GetInterface` (el tipo de la interfaz de salida lo dice el IID), `Destroy` (libera
los proxys del objeto), `pAuxEffect` de `SLEffectSendItf` (un `const void *` que es una interfaz), qué funciones
copian su estructura de callbacks (`sigs::STRUCT_COPIED`) y cuáles la guardan por su dirección
(`sigs::STRUCT_INPLACE`), `timer_create` (unión), el registrador/abortador de liblog (despachador propio, globales
del proceso) y el punto de entrada de NativeActivity con `android.app.func_name` (manifiesto).

Un parámetro `void*` oculta el tipo: si apunta a una estructura distinta entre arquitecturas, el generador no lo ve.

### Extensiones anunciadas y registros de sistema

`src/feat.rs` es la única fuente de lo que el guest ve de la CPU: los registros de identificación (`ID_AA64*`,
MIDR, MPIDR) que devuelve `MRS` y `AT_HWCAP`/`AT_HWCAP2` de `getauxval` se derivan de los mismos campos (un test
comprueba la coherencia en cada modelo). Hay varios modelos de CPU reales (`feat::MODELS`, `debug.heddle.cpu`/
`HEDDLE_CPU`; por defecto Cortex-A78; `max` para pruebas) con su MIDR, `CTR_EL0`, `DCZID_EL0` y sus registros
`ID_AA64*`; con un modelo del catálogo lo anunciado es el mínimo campo a campo entre el modelo y lo que heddle
implementa (`feat::IMPL`). **Se ejecuta lo anunciado e implementado**: una instrucción de una extensión que el
modelo o el perfil no anuncia es SIGILL, como en un procesador ARM sin ella (UNDEFINED en el Arm ARM), aunque heddle
la implemente; el diagnóstico dice `ausente en el modelo de CPU ...` (`diag::Verdict::AusenteEnModelo`, con
`feat::probe_all`). La intersección (`feat::State::exec`) se resuelve una vez al iniciar; `feat::need(F_*)` en el
decodificador es una carga relajada y una comparación (el decodificador solo corre al traducir), y no se consulta
nada al ejecutar. Un **perfil** (`cpu.conf`, `docs/perfil-cpu.md`, contrato estable con weft;
`feat::parse_profile`) anuncia exactamente lo que dice, aunque heddle no lo implemente: se registra una vez
(`cpu: AVISO: ...`) y eso que no implementa es SIGILL. El código ARM propio (biblioteca guest) solo usa ARMv8.0,
para correr con cualquier modelo. Al implementar una extensión: su campo en `IMPL`, su bit `F_*` en `EXEC` y en
`feat::feats_of`, `need` en el decodificador y su fila en la prueba `decodificacion_por_modelo`. PAuth no está
implementada: sus formas no-pista son siempre SIGILL (nunca un salto simple) y las pistas, NOP.
Registros de sistema como Linux en EL0: `MRS`/`MSR` solo de los accesibles (`feat::readable`/`writable`), el espacio
de identificación emulado como `emulate_mrs` del kernel, `SYS` solo `DC ZVA/CVAC/CVAU/CIVAC` e `IC IVAU`, y `MSR`
(inmediato) solo `CFINV`/`XAFLAG`/`AXFLAG` y `SSBS`. PSTATE.SSBS (FEAT_SSBS; el registro `SSBS` por `MRS`/`MSR` es
SSBS2) vive en `Cpu::ssbs_x` como diferencia con el valor inicial de Linux (1 si el modelo lo tiene), pasa al
`pstate` del marco de una señal y vuelve del `ucontext`. Todo lo demás es SIGILL: nunca un NOP ni un 0 inventado.
`/proc/cpuinfo`, `/proc/self/auxv` y `midr_el1`/`revidr_el1` de sysfs se emulan desde `feat` (`src/procemu.rs`): las
aperturas del guest (`open*` HLE, `fopen`/`freopen`, `svc openat`) reciben un descriptor de solo lectura de un memfd
sellado (o un archivo borrado en el directorio privado). `getauxval` y `/proc/self/auxv` comparten
`libc_hle::guest_auxv_entry`. `CNTFRQ_EL0` es 19,2 MHz (BogoMIPS 38.40).

## Memoria: reglas

1. **Toda reserva del puente tiene dueño.** `mmap` solo a través de `mem::Region`, que libera en `Drop`.
   Excepción: `elf.rs` (reserva y segmentos de módulos; la reserva se desmapea en el `Drop` del módulo, o se deja
   reservada sin acceso si la dio el llamador con `ANDROID_DLEXT_RESERVED_ADDRESS`; las páginas RELRO compartidas
   se mapean dentro de la reserva del módulo). **[lint]**
   Los bloques de TLS dinámico los da el reservador de cada hilo (`tls.rs`, como `BionicAllocator`): sus páginas
   (compartidas por los bloques pequeños de una clase de tamaño) y los bloques grandes son `Region` separadas con
   `Region::detach`, con su cabecera (firma, tipo y, en los grandes, base y longitud); su dueño es el reservador del
   hilo, que las recupera con `Region::reattach` al liberarlas y las devuelve todas al terminar el hilo
   (`tls::free_thread`).
2. **Lo permanente es de tamaño fijo** y se declara con `Region::permanent` (región HLE, arena de trampolines, arena
   de envoltorios JNI, página del resolutor TLSDESC).
3. **Nada crece sin cota.** Cada colección global tiene un tope y, al alcanzarlo, degrada (niega, recicla, deja de
   cachear) sin crecer. Añadir una colección global exige añadir su cota a la tabla de abajo. **[lint]**
4. **`Box::leak`, `mem::forget`, `into_raw` están contados por archivo.** Un uso nuevo exige justificar aquí por qué
   está acotado. **[lint]**
5. **Estado por hilo se libera con el hilo**: `GuestThread` (pila, TLS, pila de señales, JIT) vive en el TLS del hilo
   y se suelta al terminar. La prueba `hilos_guest_liberan_su_memoria` lo comprueba.
6. **Nunca devolver al guest una copia nueva en cada llamada.** Usar almacenamiento estable (estático, por hilo) o
   `intern_cstr`.
7. **Las pilas guest tienen página de guarda.** Un desbordamiento debe fallar, no pisar memoria vecina.

### Colecciones globales y sus cotas

| Dónde | Qué guarda | Cota |
|---|---|---|
| `hle::TABLE` | slots HLE | 65 536; agotada, el último slot es común y falla al llamarse |
| `boundary::SLOTS` | función del host → slot | la de `hle::TABLE`; agotada, deja de insertar |
| `boundary::SYMS` | caché de símbolos | `mem::TABLE_CAP`; llena, deja de cachear |
| `boundary::hostmap::SNAPS` | rangos ejecutables del host | 4 instantáneas fijas de 8 192 rangos (512 KiB estáticos; cada relectura escribe la siguiente y la publica con un índice atómico, se leen sin bloqueos); un mapa con más rangos se recorta a los primeros 8 192 (se registra) |
| `cbthunk::ARENA` (`INFOS`, `KEYS`/`VALS`) | trampolines host→guest | arena fija de 1 MiB (32 768), uno por (función guest, firma, modo); datos en una tabla estática y búsqueda sin bloqueos de 65 536 entradas; agotada, devuelve 0 sin abortar. Los de una biblioteca descargada se retiran (`retire_range`: siguen llamando a su dirección, pero `find_for_guest` no los da) y una petición posterior con la misma clave exacta los reactiva sin gastar arena |
| `jni::ENVS`, `jni::VMS` | entorno del host → envoltorio | arena fija de 1 MiB |
| `libc_hle::intern_cstr` | cadenas devueltas al guest | `mem::TABLE_CAP`; llena, devuelve una cadena fija |
| `libc_hle::DATA` | celdas de datos por nombre de símbolo | símbolos distintos importados |
| `proxy::ENTS` | proxys de interfaces OpenSL ES / OpenMAX AL vivas | 4 096 entradas fijas (128 KiB estáticos); `Destroy` libera las del objeto; llena, la creación responde `MEMORY_FAILURE` y destruye el objeto real |
| `proxy::VT` | tablas de métodos de los proxys | 1 024 slots fijos (un slot HLE por método de `sigs::ITFS`, ~575, creados al usar la interfaz) |
| `ndkcb::ACTS` | proxys de `ANativeActivity` | 8 actividades fijas; `onDestroy` libera; llena, `onCreate` no llega al guest (`FALLO` en logcat) |
| `ndkcb::manifest_entries` | (`lib_name`, `func_name`) de las actividades del manifiesto | las actividades del paquete (como mucho 1 024), leído una vez |
| `hle::TWIN` | función del host equivalente a cada slot (para pasar un slot al host) | 65 536 entradas fijas (512 KiB estáticos), una por slot |
| `boundary::CB_CACHE` (por hilo) | conversiones de punteros a función de estructuras (zlib, Vulkan) | 8 entradas fijas por hilo |
| `libc_hle::ATEXIT`, `KEYS` | destructores registrados por el guest | lo que registre el guest; `KEYS` por el límite de claves del host |
| `hle::HleStats` (en `GuestThread`) | contadores HLE por hilo | tabla fija de 256 entradas (4 KiB) por hilo con sondeo de 8; lo que no cabe cuenta en los globales `COUNTS`/`BYTES` (un contador por slot) |
| `rt::ALL_THREADS`, `STACKS` | hilos guest vivos | se retiran en `Drop` |
| `tls::TABLE` | módulos TLS (segmento PT_TLS de cada biblioteca cargada) | tabla fija de `MAX_TLS_MODULES` (1 024) entradas; los huecos de las descargadas se reutilizan; llena, el `dlopen` de una biblioteca con TLS falla |
| DTV por hilo (`tls.rs`) | bloque TLS de cada módulo que el hilo tocó | `MAX_TLS_MODULES` entradas dentro del área TLS del hilo; cada bloque se libera al ver el hilo una generación nueva sin su módulo y al terminar el hilo |
| reservador TLS por hilo (`tls.rs`, `tls::ALLOC_OFF`) | páginas de bloques pequeños (clases de 16 a 1 024 bytes) y bloques grandes | los bloques vivos del DTV; de las páginas vacías se guarda una por clase (siete como mucho); todo se devuelve al terminar el hilo |
| `monitor::FLAGS` | marca de store (`MonFlag`) de cada hilo guest | hilos guest vivos; se retira en el `Drop` del `GuestThread` |
| `elf::MODULES` | módulos cargados | módulos cargados (y los de una carga en curso); la descarga de su grupo los retira (`free_module`) y su reserva se desmapea al soltar el último `Arc` |
| `jit::IC_LOG` | líneas invalidadas por IC IVAU para los demás hilos | anillo fijo de 4 096 entradas (32 KiB); un hilo que se queda atrás o una entrada perdida vacía toda su caché JIT |
| `mem::GEXEC` | regiones que el guest pidió ejecutables | 4 096 intervalos (los contiguos se fusionan); superado, las nuevas no se registran |
| `namespace::NSS` | espacios de nombres (ART, exportados) con sus rutas y enlaces | `NS_CAP` (1 024); lleno, `createNamespace` falla |
| JIT por hilo | código traducido | 32 MiB por hilo y `mem::JIT_BUDGET` entre todos; superado, se vacía |
| `Jit::pcs`, `Jit::pc_offs` (por hilo) | pc guest de cada instrucción traducida | una entrada por bloque y 4 bytes por instrucción del código traducido; se vacían con él |
| `Jit::fused` (por hilo) | rutas rápidas TLSDESC en línea (código, ruta de fallo) | una entrada por llamada TLSDESC traducida; se vacía con el código |
| `rt::EMERG` | estados guest de emergencia para señales en hilos sin estado guest | `EMERG_N` (4), creados con el primer hilo guest (pila de 1 MiB, TLS, pila de señales y un JIT de 256 KiB sin ocupar cada uno: no traducen); permanentes |
| `rt::EMERG_DEFER` | señales que esperan un estado de emergencia libre | `EMERG_DEFER_N` (8) con su `siginfo`; llena, se pierde y se cuenta (`EMERG_LOST`) |
| `GuestThread::sig_cpus`, `sig_lvl` | `Cpu` y estado de cada nivel de señal | `MAX_SIG_DEPTH`, creadas con el hilo |
| `GuestThread::sig_pend`, `sig_pinfo` | señales asíncronas aplazadas y su `siginfo` | 64 bits + 64 × 128 B (8 KiB) por hilo guest; las estándar iguales se funden |
| `GuestThread::sig_rtq` (`sig::RtQueue`) | instancias extra de señales de tiempo real aplazadas, con su `siginfo` | `RTQ_CAP` (32) × 140 B por hilo guest; llena, la instancia se funde y se cuenta en `lost` |
| `GuestThread::host_alt` | pila alternativa de señales del host | 64 KiB + página de guarda por hilo guest (solo si la suya es menor); se libera con el hilo tras reponer la anterior |

### Vigilante

El hilo `heddle-samp` (Android) registra cada 6 s la memoria del proceso, lo que ocupa el puente, las funciones
HLE más llamadas y la memoria pedida por `malloc`/`mmap`. Si la memoria residente supera el límite
(`debug.heddle.rss_limit_mb`; por defecto el 60 % de la RAM, máximo 3 GiB) vuelca el estado y **termina la app**.

`debug.heddle.samp=0` (o `HEDDLE_SAMP=0`) apaga los registros periódicos del muestreador, no el vigilante: el hilo
sigue midiendo cada 6 s y lo fatal (`VIGILANTE`) se registra siempre. Solo si además `rss_limit_mb=0` no se crea el
hilo.

Es una red de seguridad, no una garantía: mide el proceso de la app. La memoria que el **emulador** reserve en la
máquina anfitriona por orden de la app (gráficos) queda fuera de su alcance.

## Señales

- Acción inicial como en bionic: en `SIGABRT`, `SIGBUS`, `SIGFPE`, `SIGILL`, `SIGSEGV`, `SIGSTKFLT`, `SIGSYS`,
  `SIGTRAP` y la 35, `sigaction` devuelve como anterior (mientras el guest no la cambió) el
  `debuggerd_signal_handler` de bionic (`sig::debuggerd_slot`, SA_RESTART | SA_SIGINFO | SA_ONSTACK), no SIG_DFL.
  Los manejadores de fallos que encadenan al anterior (Unity, Crashlytics, Breakpad) lo llaman al terminar: pone
  SIG_DFL y reenvía la señal (como `resend_signal`), y el proceso muere. Con SIG_DFL, Unity volvía sin más y el
  fallo se repetía para siempre (`difftests` t7 `chain_segv`/`chain_abort`).
- Una señal con manejador guest que llega a un hilo sin estado guest (un hilo del host) se ejecuta con un estado de
  emergencia (`rt::emergency_enter`/`leave`, creados de antemano); agotados, espera en `EMERG_DEFER` y se reenvía al
  hilo, con su `siginfo`, al soltarse uno.
- En un manejador de señal no se reserva memoria, no se toma un `Mutex` y no se crea estado de hilo
  (`async_host_handler`/`sync_fault` salen si el hilo no tiene estado guest; `try_exec_redirect` solo usa trampolines
  ya creados o la arena sin bloqueos y devuelve `false` si no hay; `ACTS` es un seqlock). Lo que el puente usa desde
  ahí es de tamaño fijo y está creado de antemano: una `Cpu` por nivel (`GuestThread::sig_cpus`, `MAX_SIG_DEPTH`) y
  su estado (`sig_lvl`).
- No registrar en logcat ni llamar al host con un bloqueo global tomado.
- Las señales del guest se entregan como lo hace el kernel (`sig::build_frame`): sobre la pila que el hilo estaba
  usando (o la alternativa del guest con `SA_ONSTACK`), con un `ucontext` aarch64 real que lleva los registros
  interrumpidos. Nunca sobre una pila aparte: los recolectores de basura recorren desde el `sp` del manejador hasta
  el final de la pila del hilo y buscan ahí los registros.
- El manejador guest lo ejecuta el **intérprete** sobre la `Cpu` de su nivel (sin traducir: no se reserva ni se toca
  la caché JIT), dentro del manejador del host (fallos síncronos, señales en una HLE) o en contexto normal (señales
  asíncronas aplazadas, abajo). El intérprete no reserva memoria (`neon::Lanes` en lugar de `Vec`). Hasta
  `MAX_SIG_DEPTH` (8) niveles anidados; una señal asíncrona que llega con todos ocupados se aplaza (`sig_pend`) y se
  entrega al volver un manejador; un fallo síncrono más allá termina el proceso (no se puede aplazar).
- **Fallos síncronos** (SEGV, BUS, ILL, FPE de código guest; `sig::sync_fault`, al que llegan `fault_host` y, en modo
  bridge, `nb_signal` desde sigchain): el guest ve el **pc exacto** de la instrucción (tabla código x86 → pc de cada
  bloque, `Jit::fault_pc`; en el intérprete, `cpu.pc`) y un `ucontext` **escribible**. Si el manejador vuelve, su
  `ucontext` (x0-x30, sp, pc, NZCV, FPSR/FPCR, v0-v31, `uc_sigmask`) pasa a la `Cpu`; si sale con `siglongjmp` (el
  `sp` destino queda fuera de la pila del manejador, `sig::note_longjmp`), pasa el contexto del salto. En ambos casos
  el manejador del host vuelve con `rsp = cpu.host_sp` y `rip = heddle_jit_abort` (bloque traducido: `run_inner` lo
  llama directamente y su prólogo apunta en `cpu.host_sp` el RSP del cuerpo; `heddle_jit_abort` ejecuta el epílogo
  del bloque con 0, que repone los callee-saved de `run_inner` y le devuelve; sin coste por despacho) o
  `rip = heddle_block_abort`, `rbx = &Cpu` (intérprete, dentro de `jit::heddle_call_block`, que guarda los
  callee-saved) y el kernel restaura eso y la máscara:
  `nb_signal` devuelve `true`. BRK, instrucción indefinida y salto a memoria sin mapear siguen el mismo camino desde el
  bucle (`sig::guest_fault`).
- Solo se abandona código guest: un fallo con el hilo en una HLE o una llamada al sistema (`phase != 0`), preparando
  otra señal (`sig_setup`) o con un bloqueo del monitor tomado (`monitor::in_critical`) **no** llega al manejador
  guest y sigue la cadena (ART: sus comprobaciones implícitas en Java llamado por JNI; debuggerd). Por eso un acceso
  a memoria guest con un bloqueo de granulo tomado (STXR, store con bloqueo, RMW) es una instrucción con entrada en la
  tabla de reparaciones `heddle_lkfix` (`monitor::lk_store`/`lk_load`): si falla, `monitor::fault_in_locked` (lo
  primero de `sync_fault`) lleva el rip a su reparación; quien hizo el acceso suelta el bloqueo y la sección crítica
  y lo repite como sonda sin bloqueo (`lock or` de 0): ese fallo llega al guest y se puede reanudar; después repite la
  operación entera. Todo acceso nuevo a memoria guest bajo un bloqueo del monitor va por `lk_store`/`lk_load` o
  comprueba antes la dirección con `monitor::probe_w` (lo hacen STXP, CASP, el RMW con el granulo caliente en modo
  `fence`, y el STXR o CAS que no escriben, que en ARM también fallan): `probe_w` recuerda 4 páginas por hilo
  (`ThreadMon::wpages`) hasta que el guest cambia mapeos o protecciones (`monitor::prot_changed`).
- **Señales asíncronas en frontera de instrucción** (`sig.rs`, "Senales asincronas"): con el hilo en código guest
  (fase 0 dentro de `rt::run_loop`) el manejador del host solo la anota (`GuestThread::sig_pend`/`sig_pinfo`, sin
  reservas ni bloqueos) y escribe `jit::SIG_POISON` en `cpu.aux[5]` del `Cpu` principal. El código traducido no lleva
  comprobación nueva: el salto hacia atrás ya compara EPOCH con `aux[5]` y sale; `run_inner` hace una sola
  comparación (EPOCH contra `aux[5]`) antes de cada bloque, que cubre las dos cosas, y devuelve `Event::Signal`; el
  intérprete mira `sig_pend` antes de cada instrucción. `sig::deliver_pending` la entrega en contexto normal: pc
  exacto, ninguna instrucción a medias, `ucontext` **escribible** (registros, pc, `uc_sigmask`) y `siglongjmp`. Antes
  de una HLE o de un `svc` se marca la fase y se mira `sig_pend` (lo aplazado se entrega antes, con el pc en la
  función o en el `svc`); al salir de `run_loop` también. Una señal con su bit ya anotado se funde en la primera; si
  al entregarla está bloqueada o sin manejador vuelve al kernel (`rt_tgsigqueueinfo`). Las de tiempo real no se
  funden: cada instancia extra va a `GuestThread::sig_rtq` con su `siginfo` y se entrega en orden de llegada.
- Dentro de una HLE (fase 1) la señal se entrega en el acto, dentro del manejador del host: es código del host que
  puede bloquearse esperándola (`pthread_cond_wait`, el GC de ART). El pc del marco es el del slot de la función. Si el
  host está en un punto seguro (justo tras una instrucción `syscall`: bloqueado en el núcleo o recién vuelto, como un
  `svc` en ARM) y no hay guardas que repongan memoria del guest (`boundary::NO_ABORT`: `InPlace`, `vk::Keep`), el
  `ucontext` es escribible y un `siglongjmp` fuera del manejador se aplica: la llamada al host se abandona
  (`rt::heddle_hle_call`/`heddle_hle_abort`, como `heddle_sc`) y el guest sigue en el contexto nuevo (con el pc en la
  función, la llamada se repite entera con los registros nuevos). Fuera de un punto seguro, el `ucontext` es de solo
  lectura y el `siglongjmp` no se aplica. Costo: ~1 ns por llamada HLE (el trampolín). Las llamadas al sistema del guest
  (`svc`, fase 2) van por `syscall::heddle_sc`: si la señal llega antes del `syscall` o el kernel la rebobinó para
  reiniciarla (SA_RESTART del guest, que se instala igual en el host), el `rip` pasa a `heddle_sc_abort`, el `svc` se
  repite después del manejador (que ve el pc en el `svc`, como en ARM); si terminó (-EINTR sin SA_RESTART), la señal
  se entrega después con ese resultado en x0. `rt_sigsuspend`, `ppoll`, `pselect6` y `epoll_pwait` entregan en el
  acto (el manejador debe correr con la máscara temporal). Un hilo creado por el guest nace con todo bloqueado y pone
  la máscara heredada cuando ya tiene estado guest (como bionic): antes, una señal en esa ventana se perdía.
- **Máscaras** (como libsigchain + bionic en ARM): `pthread_sigmask`/`sigprocmask` del guest
  (`sig::guest_sigprocmask`) quitan SIGSEGV y SIGBUS (`CLAIMED`, las reclama ART) del conjunto de `SIG_BLOCK` y
  `SIG_SETMASK` (no de `SIG_UNBLOCK`) y, en Android, aplican `filter_reserved_signals` de bionic (la 32 siempre
  bloqueada, las reservadas 33..SIGRTMIN-1 nunca; `SIGRTMIN` = `sig::rtmin()`, el de la bionic del host). El `svc`
  `rt_sigprocmask`, la máscara de `sigaction` y `uc_sigmask` al volver de un manejador no se filtran (en el
  dispositivo van al kernel directamente). Toda consulta (conjunto viejo, `uc_sigmask` de los marcos, `sigsetjmp`) es
  la máscara real del kernel; `siglongjmp` la repone por el camino filtrado (bionic llama a `sigprocmask64`, que en el
  dispositivo intercepta libsigchain). Única diferencia interna: el puente nunca bloquea SIGSEGV en el kernel
  (`KEEP_UNBLOCKED`); cuando en el dispositivo quedaría bloqueada se anota en `GuestThread::sig_view` y se suma a las
  consultas, y un fallo de SIGSEGV con ella bloqueada termina el proceso como `force_sig` del kernel (sin llegar al
  manejador guest). Lo mismo con cualquier señal de fallo que detecte el bucle (`guest_fault`: BRK, indefinida).
- Bloqueos del monitor exclusivo (`glock`, `ARM_LOCK`): son bloqueos por giro no reentrantes (TTAS; tras
  `SPIN_BEFORE_YIELD` giros ceden la CPU: con más hilos que CPU el dueño suele estar desalojado) y un manejador
  guest puede dormir (el GC de Boehm deja a los hilos en `sigsuspend` hasta terminar de marcar). Por eso, mientras el hilo tiene
  uno tomado, el manejador host no ejecuta el manejador guest: anota la señal con su `siginfo`
  (`monitor::defer_signal`) y la reenvía al propio hilo al soltar el último bloqueo (`rt_tgsigqueueinfo`). Para el
  guest equivale a que la señal llegó justo después de la instrucción, como en un procesador ARM real. Todo
  bloqueo nuevo del monitor debe tomarse con `glock`/`arm_lock` (que abren la sección crítica) y nunca llamar al
  guest con él tomado. Los fallos síncronos del kernel (SEGV, SIGILL, SIGBUS, SIGFPE) no se aplazan. Las señales
  iguales pendientes se funden en una, salvo las de tiempo real: cada instancia extra se encola con su `siginfo` (cola
  fija del hilo, `sig::RtQueue`) y se reenvía en orden, como las encola el kernel.
- Monitor en modo `fence` (Android: sin rseq). Invariante: **un store normal sobre un granulo FRIO es un `mov` sin
  bloqueo, siempre dentro de la marca `MonFlag::in_store` del hilo** (`heddle_fst*` en `monitor.rs`: marca = 1,
  leer `HOT`, `mov`, marca = 0); armar un granulo es `HOT = ARMANDO`, `membarrier(PRIVATE_EXPEDITED)`, esperar a que
  ninguna marca de `monitor::FLAGS` valga 1 y `HOT = CALIENTE`. Es exacto en x86-TSO (razonamiento completo en la
  cabecera de `monitor.rs`): tras el armado, todo store rapido que vio el granulo frio ya es visible antes de que el
  LDXR lea, y todo store posterior va por el bloqueo y sube la versión. Reglas que lo sostienen:
  * Toda escritura del guest pasa por `monitor::store` o por `heddle_fst*` (el JIT, con `cpu.monflag` en RCX). La
    marca es del hilo (`GuestThread::mon_flag`), compartida por las `Cpu` de sus manejadores de señal; una `Cpu` sin
    marca (`monflag = 0`) usa la ruta con bloqueo.
  * Un manejador guest nunca corre con la marca puesta: una señal asíncrona se aplaza (`defer_signal`) y la salida del
    store la reenvía; un fallo síncrono en el propio `mov` limpia la marca y reinicia el store
    (`monitor::fault_in_store`, llamado en `sync_fault` antes de ejecutar el manejador guest). Si no, el armador esperaría para siempre a un hilo
    dormido en su manejador (el cuelgue del 53 %).
  * Entre poner y quitar la marca no hay llamadas, bloqueos ni esperas (solo esas instrucciones).
  * Las atómicas LSE (`rmw`) no arman. Sobre un granulo FRIO escriben con `heddle_fcas*`: la misma ventana de la
    marca con un `lock cmpxchg` en lugar del `mov` (sin bloqueo ni versión, como el store rápido; un fallo en el
    `cmpxchg` reinicia igual que el `mov`). Caliente o armando: bloqueo del granulo, `lock cmpxchg` del host y versión.
  * Enfriar no necesita barrera (sube `EPOCH` antes de poner FRIO bajo el bloqueo del granulo).
  * `HOT` se indexa con 16 bits de la línea (las separadas por 4 MiB comparten entrada) y `HOT_TAG` guarda qué línea
    la armó (0: no se sabe; `TAG_MULTI`: varias, o tabla saturada). Con la entrada caliente, `heddle_fst*` mira la
    etiqueta y, si es otra línea concreta, hace el `mov` con su marca puesta (como con la entrada fría). La etiqueta se
    escribe antes de A1 y pasa a `TAG_MULTI` con la barrera de un armado cuando otra línea arma la misma entrada, antes
    de que la reserva lea: mismo razonamiento que el armado. Sin ella, con la tabla casi llena, la mayoría de los
    stores de un juego Unity iban con bloqueo (~30 % de su CPU en `slow_store`).
  * Si `membarrier` fallara tras registrarse (solo con un filtro seccomp añadido por la app), el puente termina con
    `FALLO monitor:` en lugar de seguir sin exactitud.
- Rutas calientes del monitor alineadas: el JIT llama directamente en cada store a `heddle_rst*` (rseq, sección
  crítica rseq en ensamblador) o `heddle_fst*` (fence), y `rmw` en fence a `heddle_fcas*`. Cada entrada empieza con
  `.p2align 6` y su ruta caliente cabe en esa línea de 64 B sin saltos que crucen o terminen en un límite de 32 B
  (errata JCC): sin eso, el mismo código daba stores rseq un 15-20 % y fence un ~10 % más lentos según dónde lo
  dejara el enlazador. Al tocarlas, mantenerlo (objdump; cabecera de `monitor.rs`, "Alineacion de las rutas
  calientes"). **[lint]** (`.p2align 6` delante de cada entrada) y prueba `monitor::align_tests` (direcciones). El modo
  bloqueo (`slow_store`, Rust) no depende de la posición en lo medido y no se alinea.
- Secciones críticas del resto del puente: todo bloqueo global que un manejador guest pueda volver a pedir en el mismo
  hilo (`SYMS`, `SLOTS`, `hle::TABLE`, `ENVS`/`VMS`, `DATA`, `KEYS`, `ATEXIT`, `MODULES`, `GEXEC`, `proxy::ALLOC`,
  `ndkcb::ACTS_LOCK`, listas de hilos...)
  se toma con `monitor::lock`/`read`/`write`, y una sección sin bloqueo pero no reentrante (creación de un trampolín)
  con `monitor::NoSignals`. Mientras duran, las señales con manejador guest se aplazan con el mismo mecanismo que los
  bloqueos del monitor (`defer_signal`, reenvío al salir): equivale a tenerlas bloqueadas en la sección, sin dos
  llamadas al sistema por entrada. Nunca ejecutar código guest dentro de una de ellas. **Un bloqueo global nuevo del
  puente se toma así.**
- Pila del host de los manejadores: el de un fallo síncrono corre sobre la pila alternativa del host (los manejadores
  del puente para SEGV/BUS/ILL/FPE llevan siempre `SA_ONSTACK`, también el que instala `sigaction` del guest; en ART,
  el de sigchain). Al crear el estado guest de un hilo se instala una de 64 KiB con página de guarda
  (`rt::HOST_ALT_STACK`) si la que tiene es menor (bionic/ART: 16 KiB) y al terminar se repone la anterior; el guest
  sigue viendo su `sigaltstack` (emulada). No se cambia de pila dentro de un manejador (una señal `SA_ONSTACK` anidada
  pisaría los marcos vivos). Tres fallos anidados con 12 callbacks anidados por `bsearch` usan ~30 KiB
  (`rt::tests::pila_alternativa_del_host_grande`); `siglongjmp` o `sigaction` dentro del manejador, ~5 KiB. Una
  HLE llamada desde el manejador puede reservar memoria o tomar un bloqueo (como `malloc` en un manejador ARM real: es
  decisión del guest). Lo mismo el primer acceso de un hilo al TLS dinámico de un módulo dentro de un manejador: la
  ruta lenta (`tls::get_addr`) reserva el bloque ahí, como bionic (que bloquea las señales y usa su asignador); la
  tabla de módulos se toma con `monitor::lock`, así que una señal guest que llegue durante la ruta lenta se aplaza. El mapa de código del host se lee sin bloqueos ni reservas (`is_host_code_sig`, última
  instantánea publicada): un salto del manejador a código del host siempre se detecta (`FILTRACION`).


## JIT: dónde vive cada registro guest

El `Cpu` del hilo (`x0`-`x30`, `sp`, flags perezosas, `v0`-`v31`, FPSR/FPCR) es la **única copia autoritativa** y está
**completo en todo punto** del código traducido: salidas a `run_inner`, llamadas a helpers/HLE, fallos síncronos
(`fault_host`, `native_bridge_signal`, `guest_fault` lo leen) y señales asíncronas (`build_frame`).

- Cache de registros (`src/jit.rs`, cabecera "Registros"): hasta 5 registros ARM por bloque se copian en R12-R15/RBP
  (callee-saved). Es una cache de **lectura con escritura inmediata**: toda escritura de un registro `x` en código
  generado pasa por `Ctx::st_x`, que escribe el `Cpu` y la copia. Una copia solo se usa mientras es válida; se
  invalida tras cualquier instrucción que llame a `h_exec`, con `forget` cuando algo escribe el `Cpu` por otra vía
  (MOVK) y dentro de `jitfp`/`jitneon` (`fill = false`: allí `st_x` invalida). **No escribir `Cpu.x` desde código
  generado sin `st_x` o sin `forget`.** Un helper nuevo que escriba registros `x` debe ir por `call_helper_exec_o`
  (marca `helper`) o invalidar.
- Todos los bloques comparten el marco de pila (`Asm::prologue`/`ret_epilogue`, `PROLOGUE_LEN`): el encadenado entra
  tras el prólogo. El prólogo apunta `cpu.host_sp` (punto de reanudación) y `run_inner` lo pone a 0 al volver el
  bloque; `heddle_jit_abort` repite el epílogo: si cambia el marco, cambiar ambos. Una rutina llamada desde el JIT debe respetar el ABI de C (R12-R15, RBP, RBX).
- Los stores de flags perezosas no se eliminan aunque estén muertas: entre medias puede haber un fallo o una señal.
- Lo que no es exacto (previo): el `pc` del `Cpu` dentro de un bloque es el de su inicio (o el de la última
  instrucción que fue al helper); el marco de una señal lleva el exacto (tabla `Jit::pcs`/`pc_offs`, `Jit::fault_pc`:
  el `rip` del fallo o, en un helper, su dirección de retorno, a `HELPER_RET_BELOW_HOST_SP` bajo `cpu.host_sp`); FPSR acumulado de las rutas rápidas FP está en MXCSR hasta `fold_mxcsr`. Las
  señales asíncronas no ven nada de esto: se entregan en una frontera de instrucción (ver Señales).
- Un load/store con pre-índice que falla no ha escrito la base (la escritura va tras el acceso, como en el
  pseudocódigo). Exclusivos, atómicos LSE, CAS/CASP y LDAR/STLR/LDAPR desalineados (no se anuncia FEAT_LSE2) son
  SIGBUS `BUS_ADRALN` en el pc exacto y sin cambios (`sig::align_fault`, en el intérprete y en los helpers o en línea
  en el JIT), como `AArch64.CheckAlignment`. Pruebas: `desalineado_es_sigbus_en_el_pc_exacto`, el diferencial
  contra Unicorn (`difftests/README.md`).
- FPCR.AHP: FCVT, FCVTL y FCVTN con half usan el formato alternativo (`softfp::convert_ahp`, como FPConvert,
  FPUnpackCV y FPRoundBase: exponente 31 numérico hasta 131008; NaN → cero e IOC; infinito o desbordamiento →
  máximo e IOC). Las instrucciones de datos de FEAT_FP16 (y FMLAL) son siempre IEEE. `MSR FPCR`, `fesetenv` y el
  `ucontext` al volver de una señal guardan solo AHP, DN, FZ, RMode y FZ16 (`interp::FPCR_RW`): las habilitaciones
  de trampas son RAZ/WI (heddle no implementa trampas de coma flotante) y Len/Stride RES0 (sin AArch32).
- `HEDDLE_NOREGS=1` desactiva la cache. Pruebas: `jit::regs_tests` (diferencial, fallo síncrono, banco).
- **TLSDESC en línea** (`jit.rs`, "TLSDESC en linea"): en la secuencia de llamada del ABI (`ldr xN, [x0, #k]; add x0,
  x0, #k; blr xN`), si `xN` es el resolutor dinámico del puente (`tls::resolver_at()`), el JIT emite su ruta rápida en
  x86 en el sitio del `blr` y encadena a pc + 4 (ni ida ni vuelta por el despachador). Mismo resultado que el código
  ARM del resolutor: x0, NZCV (`cmp` de generaciones), x30 = pc + 4, el resto de registros y sp intactos; la pila
  `[sp-32, sp)` debe ser accesible (se sondea). Generación vieja o bloque sin reservar: sigue en
  `tls::RESOLVER_SLOW_OFF` con el estado de ARM en ese punto. Un fallo en cualquiera de sus accesos (descriptor o tp
  inválidos, TBI) lo desvía `jit::fault_in_fused` (lo primero de `sig::sync_fault`, tras `fault_in_locked`) al
  resolutor traducido desde su inicio, que repite el acceso con la semántica exacta. Única diferencia: no se escribe
  el contenido de `[sp-32, sp)` (el resolutor guarda ahí x19-x22 y los repone; memoria por debajo de sp, que AAPCS64
  no conserva). No se usa en el modo contado. Prueba: `difftests` t11 (frente a la misma llamada con un `nop` que la
  hace irreconocible).

## Kernels personalizados

El puente debe funcionar con cualquier kernel de Linux razonable, incluidos los personalizados:

- Solo llamadas al sistema estándar. Nada de módulos del kernel ni de `binfmt_misc`.
- Toda función opcional del kernel tiene un camino alternativo que se activa solo. Hoy: el monitor exclusivo usa
  `rseq` + `membarrier` (glibc), si no `membarrier(PRIVATE_EXPEDITED)` sola (modo `fence`, Android) y si tampoco, un
  modo con bloqueo (`monitor::FALLBACK`); se elige una vez al inicio y queda en logcat (`monitor: modo=...`);
  `HEDDLE_MONITOR=rseq|fence|bloqueo` (o `debug.heddle.monitor`) lo fuerza;
  `memfd_create` (biblioteca guest) cae a un archivo en el directorio privado y, para `/proc/self/maps` con páginas
  de 16 KiB, al del host sin reescribir; `process_vm_readv` (`rt::addr_readable`) cae a `mincore`;
  `process_vm_writev` (`mincore` con páginas de 16 KiB) a una copia directa.
- Una llamada al sistema que devuelva `ENOSYS` se propaga al guest tal cual; el puente no aborta por ello.

## Lo que no está garantizado

- El mapa de código del host se revalida como mucho cada ~50 ms: un puntero a función de una biblioteca cargada en
  esa ventana puede llegar crudo (termina en `FILTRACION`, no en ejecución). Antes de declarar una `FILTRACION` se
  relee el mapa (fuera de un manejador de señal): si la dirección ya no es del host (biblioteca descargada y rango
  reutilizado), sigue la ejecución si es código guest o es un SIGSEGV corriente.
- Un callback guest al que el host llega solo por redirección (regla 2) recibe los argumentos tal cual: si uno es
  un `JNIEnv*` del host, el guest no puede usarlo. Si ya existe un trampolín con firma para esa función (native
  registrado), se usa ese; si no, el reparto es el universal (sin coma flotante en pila).
- Los trampolines con firma (natives JNI de `getTrampoline*` y `RegisterNatives`) reparten los argumentos por el
  shorty con los dos ABI reales (SysV x86-64 → AAPCS64 de Linux: 8 enteros, 8 flotantes, el resto en pila en orden y
  en palabras de 8 bytes) hasta `cbthunk::MAX_SIG_ARGS` argumentos; una firma más larga usa el reparto universal.

- Cargador (`elf.rs`): sigue `find_libraries`/`soinfo_unload` del enlazador de bionic (Android 10 o posterior):
  carga en anchura con ciclos de DT_NEEDED, enlazado del grupo local desde la raíz, constructores en profundidad (una
  vez por módulo), cuenta de referencias en la raíz del grupo (y una por arista entre grupos y por destructor de
  `thread_local` pendiente), descarga real (destructores en el orden del recorrido, reserva desmapeada: un puntero
  viejo da SIGSEGV), cada carga en una dirección nueva (`ReserveWithAlignmentPadding`, con inicio alineado a 2 MiB si hay páginas enormes
  transparentes, `targetSdkVersion` >= 31 y el mayor `p_align` es 2 MiB; `MADV_HUGEPAGE` en sus segmentos
  ejecutables), las bibliotecas nuevas de una carga leídas en anchura y mapeadas en orden aleatorio, `RTLD_NODELETE`/`RTLD_GLOBAL`
  solo en cargas nuevas y el grupo nunca se descarga, y TLS initial-exec hacia cualquier biblioteca guest rechazado
  con el texto de bionic (todas se abren con `dlopen`). Una biblioteca ya cargada se reconoce por su DT_SONAME (sin
  él, con `targetSdkVersion` < 23, por el nombre de su archivo) o por su archivo (dispositivo, inodo y desplazamiento
  dentro del APK), como `find_loaded_library_by_soname`/`_by_inode`. `dladdr` y `dl_iterate_phdr` dan la ruta real
  (`realpath`; dentro de un APK, `ruta/base.apk!/lib/arm64-v8a/libx.so`), como `soinfo::get_realpath`. El `targetSdkVersion` sale de
  `bridge::target_sdk`: `HEDDLE_TARGET_SDK` si está definida; si no, en Android el del proceso (lo fija ART y lo
  devuelve `android_get_application_target_sdk_version` del host) y fuera de Android 35. El guest ve el mismo valor.
  Diferencias que quedan:
  * Espacios de nombres (`namespace.rs`): los que crea ART por NativeBridge (`createNamespace`, `linkNamespaces`,
    `getExportedNamespace`, `initAnonymousNamespace`, `loadLibraryExt`) con rutas, rutas permitidas, `isolated`,
    `shared` y enlaces con lista de sonames, como `android_namespace_t`; un `dlopen` del guest se hace en el espacio
    de quien llama. Las bibliotecas del sistema (HLE) están "cargadas" en los espacios de sistema (el por defecto y
    los exportados) y una app las alcanza por sus enlaces. Lista de excepciones de bionic (`is_exempt_lib`, lista
    exacta de AOSP) con `targetSdkVersion` < 24: en un espacio con `ANDROID_NAMESPACE_TYPE_EXEMPT_LIST_ENABLED` se
    reintenta desde el espacio por defecto, y una inaccesible de la lista se carga con aviso. Diferencias: el espacio
    por defecto no tiene rutas de búsqueda (en el dispositivo, `/system/lib64`, `/apex/.../lib64`...: aquí las
    bibliotecas del sistema son HLE y no hay archivos arm64 que buscar), así que un `dlopen` por nombre de algo que
    solo exista como archivo arm64 del sistema falla con `library "x" not found`, `is_system_library` no reconoce
    ninguna guest y un `dlopen` por ruta (`/system/lib64/libc.so`) de una del sistema se resuelve por su nombre a la
    HLE; un `library_namespace` que no es un espacio se rechaza (bionic usaría el puntero).
  * `android_dlopen_ext` con todas las opciones de `android_dlextinfo` y los textos de bionic: banderas no válidas,
    `USE_LIBRARY_FD(_OFFSET)` (ruta real del descriptor, desplazamiento alineado a página, `fstat`), `FORCE_LOAD`,
    `RESERVED_ADDRESS`/`_HINT`/`_RECURSIVE` (la región del llamador se consume en orden de carga, sin barajar con
    `RECURSIVE`, y al descargar se deja reservada sin acceso en lugar de desmapearla), `WRITE_RELRO`/`USE_RELRO`
    (RELRO escrito y mapeado desde el archivo, o sustituidas las páginas iguales) y `USE_NAMESPACE`; el extinfo
    solo se aplica a la biblioteca pedida (a todas con `RECURSIVE`). La lectura sigue `ElfReader::Read` con `pread`
    (cabecera, program headers, section headers, `.dynamic` y su `.dynstr`, con sus textos de error y los
    `DL_ERROR_AFTER(26, ...)`: una biblioteca sin section headers no se carga) y los segmentos se mapean desde el
    archivo (`LoadSegments`) con su protección final (`PFLAGS_TO_PROT`, nunca ejecutable en el host: el código guest
    se traduce leyéndolo); solo el RELRO es escribible durante la relocación (`_extend_gnu_relro_prot_end`) y una
    biblioteca con DT_TEXTREL o DF_TEXTREL se rechaza (`"x" has text relocations`, como bionic en LP64). La nota
    `NT_ANDROID_TYPE_PAD_SEGMENT` se lee como `ReadPadSegmentNote` solo si el kernel admite la migración de tamaño de
    página (`/sys/kernel/mm/pgsize_migration/enabled`; `HEDDLE_PGSIZE_MIGRATION=1/0` lo fuerza) y los segmentos se
    extienden hasta el siguiente (`_extend_load_segment_vma`); sin ella, el relleno se descarta (`DropPaddingPages`).
    `.bss` con nombre (`[anon:.bss]`). Una entrada de APK comprimida o no alineada a página no se abre
    (`open_library_in_zipfile`, con `normalize_path` y el límite de 512 caracteres de la ruta).
  * Orden de búsqueda de símbolos como `SymbolLookupList` (relocación: el propio módulo si es DT_SYMBOLIC, el grupo
    global, que es el ejecutable con lo que exporta app_process64 y las DF_1_GLOBAL, y el grupo local en anchura con
    las bibliotecas del sistema en su sitio de DT_NEEDED), `dlsym_handle_lookup` y `dlsym_linear_lookup`
    (RTLD_DEFAULT/RTLD_NEXT: las bibliotecas del espacio del llamador en orden de carga, solo las RTLD_GLOBAL y, con
    `targetSdkVersion` < 23, todas; luego el grupo del llamador). Cada símbolo del sistema se atribuye a todas las
    bibliotecas del NDK que lo exportan, con su versión (`sigs::EXPORTS`, generada por `tools/sigtool` desde los stubs
    del NDK; lo que solo implementa el puente, a libc sin versión). Las del sistema están cargadas en los espacios de
    sistema antes que cualquier guest (como las que carga app_process64 al arrancar) y no en los de las apps (allí se
    llega a ellas por el grupo local). Versiones como bionic (`check_symbol_version`, `VersionTracker`): DT_VERSYM,
    DT_VERDEF y DT_VERNEED de las guest, `dlvsym`, y las referencias con versión de una guest al sistema. Las del
    sistema tienen sus DT_NEEDED (`sigs::SYS_NEEDED`, escrita a mano desde los `Android.bp` de AOSP: libc → libdl,
    ld-android; libm → libc; el resto → liblog y sus dependencias públicas del NDK, libc, libm, libdl): `dlsym` con el
    handle de una del sistema recorre su árbol en anchura (libm → libc → libdl) y en el grupo local de una guest sus
    hijos cuentan si son accesibles desde el espacio (`is_accessible`: alguna biblioteca del espacio los declara). Un
    símbolo que no está en la lista de búsqueda hace fallar el `dlopen` (`cannot locate symbol "x" referenced by
    "..."...`), también uno del sistema que la biblioteca no declara en DT_NEEDED (`difftests` enlaza sus pruebas con
    stubs del NDK generados desde `sigs::EXPORTS`, `difftests/tools/mkstubs.py`, para tener los DT_NEEDED reales).
    `dladdr` sobre una función del sistema (un slot HLE) da la ruta de su biblioteca en el dispositivo (en Android la
    del host, que es la misma ruta; fuera, `elf::sys_realpath`: `/apex/com.android.runtime/lib64/bionic/libc.so`,
    `/system/lib64/liblog.so`...), la base de la biblioteca del host (legible) y el símbolo con la dirección que da
    `dlsym`. Diferencias: lo que exporta una biblioteca del sistema pero el puente no sirve no hace fallar el `dlopen`
    (fallo perezoso al llamarlo, con registro); solo se conocen los símbolos públicos del NDK (no los privados de la
    plataforma) y las dependencias públicas (`SYS_NEEDED` no tiene libbase, libutils...: tampoco son accesibles desde
    una app); `dl_iterate_phdr` no lista las bibliotecas del sistema ni el ejecutable (bionic sí: aquí no hay imagen
    ELF arm64 que mostrar y dar las cabeceras x86-64 del host confundiría a quien busca `PT_GNU_EH_FRAME` o el
    build-id); `dladdr` de una dirección interna del puente (trampolines, proxys) da 0.
  * TLS dinámico como bionic (`tls.rs`): identificador de módulo, generación, DTV por hilo (ranura 0 =
    `TLS_SLOT_DTV`), bloque reservado en el primer acceso de cada hilo, resolutor TLSDESC dinámico de bionic en ARM64
    con su ruta lenta como HLE, `__tls_get_addr`, `dlsym` de una variable TLS (la copia del hilo) y `dl_iterate_phdr`
    con `dlpi_tls_modid`/`dlpi_tls_data`. Diferencias: el DTV tiene capacidad fija (`MAX_TLS_MODULES`; bionic lo
    agranda); los bloques salen de un reservador como `BionicAllocator` pero por hilo (bionic tiene uno global,
    protegido por el bloqueo del enlazador con las señales bloqueadas): el primer acceso de un hilo no toma ningún
    bloqueo global para reservar (solo la tabla de módulos, con `monitor::lock`) y todo se devuelve al terminar el
    hilo (`tls::free_thread`), sin listas compartidas entre hilos. Clases de 16 a 1 024 bytes en páginas de 4 KiB
    del host (bionic usa su página: con páginas de 16 KiB del guest, páginas de 16 KiB) y los bloques grandes aparte;
    se guarda una página vacía por clase. Lo observable es solo la dirección de cada bloque (qué bloques comparten
    página y la distancia entre los de hilos distintos), que bionic tampoco garantiza.
  * Descargar una biblioteca mientras otro hilo ejecuta su código (indefinido en bionic): ese hilo falla con SIGSEGV
    en su siguiente acceso o despacho; si justo estaba traduciendo un bloque de ella, la lectura del código es
    protegida (`monitor::read_code`, tabla `heddle_lkfix`): el bloque termina antes y el despacho a esa instrucción es
    un SIGSEGV del guest en ese pc (también en el intérprete). Si un hilo del puente tiene un instante el módulo (muestreo, diagnóstico), el desmapeo espera a que lo
    suelte.
- Páginas de 16 KiB (`HEDDLE_PAGE_SIZE=16384`, `mem::guest_page`): el guest ve 16384 en `getpagesize`, `sysconf` y
  `AT_PAGESZ`; `mmap`, `munmap`, `mprotect`, `mremap`, `madvise` y `msync` (HLE y `svc`) exigen y redondean como un
  kernel de 16 KiB (`syscall.rs`, mapeos nuevos en direcciones alineadas a 16 KiB) y el cargador alinea a 16 KiB, con
  el error de bionic para `p_align` menor y su modo de compatibilidad (`IsEligibleFor16KiBAppCompat`, segmentos
  leídos en una reserva RW con el nombre `"<ruta> (compat loaded)"`, la región RELRO de compatibilidad protegida
  tras relocar y toda ella código guest para el JIT). El modo de compatibilidad se decide como Android 16
  (`pagecompat.rs`, una vez por proceso, solo al cargar una biblioteca de 4 KiB): `android:pageSizeCompat` del
  manifiesto binario del APK base (`enabled`/`disabled`, reconocido por su identificador de recurso 0x010106ab como
  `TypedArray`) o, sin él, la alineación de las bibliotecas `lib/arm64-v8a/*.so` de la app
  (`checkLoadSegmentAlignment`: algún PT_LOAD con `p_align` 0x1000; con `extractNativeLibs` las extraídas en
  `nativeLibraryDir`; una comprimida sin extraer es un error y deja el modo apagado); la app sale de
  `ApplicationInfo` por JNI, de `HEDDLE_APK`/`HEDDLE_NATIVE_LIB_DIR` o de la ruta de la biblioteca
  (`<app>/lib/<isa>/` o `<app>/*.apk!/`). `bionic.linker.16kb.app_compat.enabled` y `HEDDLE_PAGE_COMPAT=1` (o
  `debug.heddle.page_compat`) lo fuerzan; `HEDDLE_PAGE_COMPAT=0` es el ajuste del usuario que lo desactiva. Lo que
  el host hace a 4 KiB se ve a 16 KiB: un mapeo de archivo que acaba a media página tiene ceros hasta el final de la
  de 16 KiB (páginas anónimas encima, `file_tail_zeros`); `/proc/self/maps` y `smaps` (también
  `/proc/<pid>`, `thread-self`, `task/<tid>`) por `open`/`openat`/`fopen`/`svc` se dan reescritos en un memfd (VMA
  redondeados a 16 KiB sin solaparse, `Size`, `KernelPageSize` y `MMUPageSize`); `mincore` (un byte por página de
  16 KiB), `mlock`/`munlock`/`mlock2` (páginas enteras) y `brk`/`sbrk` (inicio alineado, accesible hasta el final de
  su página) como un kernel de 16 KiB. Con páginas de 4 KiB nada de esto se registra ni se comprueba (salvo una
  comparación en `open`). Lo que no puede ser exacto con el host de 4 KiB: la memoria que reserva el host (`malloc`,
  pilas y TLS de los hilos, bibliotecas del sistema) va a 4 KiB; la cola de ceros de un mapeo de archivo es privada
  (con `MAP_SHARED` lo que se escriba ahí no lo ven otros mapeos del archivo, y si el archivo crece después esa cola
  sigue en ceros); `maps`/`smaps` se reescriben solo al abrirlos por su ruta (no por un `openat` relativo a un
  descriptor de `/proc/self`, ni los de otro proceso), sin memfd (kernel sin `memfd_create`) se ve el del host, y en
  `smaps` el resto de campos (`Rss`, `Pss`...) son los del host y una VMA del host que queda vacía al redondear
  desaparece sin sumarse a la anterior; `mincore` da residente una página de 16 KiB si lo es alguna de sus páginas
  del host; con un host glibc (pruebas) su `malloc` también mueve `brk` y no sabe del redondeo (en Android el
  asignador del host no usa `brk`); en un segmento de solo lectura del cargador, la cola de la última página de
  16 KiB más allá del final del archivo es anónima (ceros) y no del archivo.
- Nada se ha validado en Android real ni en hardware ARM.
- Las reservas que haga el propio guest (su `malloc`) son suyas: una app que pida memoria sin parar lo hará igual
  que en un dispositivo real; el vigilante la corta.
- Vulkan está activado por defecto (`debug.heddle.vulkan=0` o `HEDDLE_VULKAN=0` lo desactivan: el guest no ve
  `libvulkan.so` y la app cae a GLES). Validado con Subway Surfers (Unity) en Cuttlefish y, en el CI, con `difftests`
  tvk sobre lavapipe (instancia y dispositivo con `pAllocator` guest y un mensajero de `VK_EXT_debug_utils` en la
  cadena `pNext`; fuera de Android el puente abre `libvulkan.so.1` del host si existe). Límites:
  * Un `sType` desconocido delante de una estructura con callbacks se enlaza cambiando **en su sitio** su `pNext`
    durante la llamada (en ARM la estructura no se modifica). Otro hilo del guest que leyera esa estructura mientras
    dura la llamada (o un manejador de señal del propio hilo) vería el `pNext` apuntando a la copia del puente (mismo
    contenido, convertido) en lugar del suyo; una escritura suya en ese campo durante la llamada se pierde al
    reponerlo. Leerla o escribirla así es un uso que la especificación ya prohíbe (sincronización externa de los
    parámetros de entrada). En memoria de solo lectura no se cambia y esa cadena queda sin convertir (se registra).
  * Una cadena con más de 256 nodos o anidamientos de más de 16 niveles (ciclos: uso inválido) se cortan.
- `glob` en un host glibc (pruebas, `heddle-run`): `gl_matchc` es aproximado; en Android se reenvía a la de bionic.
- `long double` (libm, `strtold`/`wcstold`, las conversiones `%L` de printf/wprintf/scanf/wscanf) es exacto solo si
  la biblioteca guest está incrustada (el CI la incluye); sin ella se aproxima en `double`. Un scanf con `%L` hace
  varias llamadas al scanf del host sobre el mismo `FILE` (bloqueado con `flockfile`). Los argumentos posicionales
  (`%1$Lf`) no se admiten en el formateador de printf (tampoco antes, para ninguna conversión).

## Diagnóstico

Registro en logcat con la etiqueta `heddle`. Añadir los registros necesarios **de una vez**, no uno por iteración.
Mensajes clave: `FILTRACION`, `FALLO`, `simbolo '...' no disponible` (frontera), `memoria:`, `llamadas HLE en 6 s:`,
`VIGILANTE:`, `instruccion no soportada por heddle` / `instruccion indefinida en ARMv8/ARMv9` / `instruccion no
reconocida` (SIGILL al guest).
Una instrucción que heddle no ejecuta se nombra con `diag::classify` (mnemónico y extensión `FEAT_*`; tabla obtenida
del desensamblador de LLVM, ver la cabecera de `src/diag.rs`) y se formatea en `diag::Buf`, sin reservar memoria
(puede ocurrir en un manejador de señal guest). El guest recibe el mismo SIGILL que en un ARM sin esa extensión: el
diagnóstico no cambia lo que observa. Los números de llamada al sistema arm64 se registran con su nombre
(`diag::syscall_name`).
Lo fatal (`FALLO`, `FILTRACION`, `VIGILANTE` y todo mensaje que precede a un `abort`/`raise`) va por
`bridge::alog_fatal`: prioridad ERROR y escrito de forma síncrona en logcat, nunca por `eprintln` (la tubería de
stderr la lee otro hilo y el mensaje se pierde si el proceso muere antes).

## Repositorio público

No incluir datos personales, rutas locales ni nombres de máquinas en el código, los mensajes de commit o la
documentación.

## Estructura y nombres

Monorepo (ver `docs/ESTRUCTURA.md`): `src/` (núcleo), `tests/` (solo pruebas de cargo, `arch_lint.rs`, `cli.rs`), `difftests/`
(pruebas guest y diferenciales), `bench/` (microbenchmarks), `guest/`,
`tools/sigtool/`. Nombres: crate `heddle`, biblioteca `libheddle.so` (símbolo `NativeBridgeItf`), etiqueta de logcat
`heddle`, propiedades `debug.heddle.*`, variables `HEDDLE_*`, binarios `heddle-run`, `heddle-difftest`, `heddle-fpone`.
Versiones publicadas por el CI: `vX.Y.Z`, calculadas por GitVersion (`GitVersion.yml`).
Los binarios aceptan `-h`/`--help` y `-V`/`--version` y salen con 0 (bien), 2 (uso incorrecto) o 1 (error);
`heddle-run` devuelve el estado del guest o muere por su misma señal (README, "Binarios").
