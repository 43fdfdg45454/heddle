# Perfil de CPU (`cpu.conf`)

Contrato estable entre heddle y quien lo instala (weft). El perfil decide **lo que el guest ve** de la CPU:
`MIDR_EL1`, los registros `ID_AA64*` que lee `MRS`, `AT_HWCAP`/`AT_HWCAP2` (`getauxval` y `/proc/self/auxv`),
`/proc/cpuinfo`, `midr_el1`/`revidr_el1` de sysfs, `CTR_EL0` y `DCZID_EL0`, y con ello **qué se ejecuta**: heddle
sigue la especificación Arm ARM y ejecuta solo las extensiones que el perfil anuncia y que implementa. Una
instrucción de una extensión que el perfil no anuncia da SIGILL, como en un procesador sin ella, aunque heddle la
implemente (logcat: `ausente en el modelo de CPU ...`); una anunciada que heddle no implementa también da SIGILL (el
diagnóstico nombra la instrucción y su extensión).

heddle anuncia **exactamente** lo que dice el perfil, aunque no lo implemente. Al iniciar, registra una sola vez en
logcat (etiqueta `heddle`) un aviso con las extensiones anunciadas que no implementa completas:

```
cpu: AVISO: el perfil anuncia extensiones que heddle no implementa (si el guest las usa recibe SIGILL): sve sve2 bf16
```

## Dónde se lee

El perfil se lee una sola vez, al crear el primer hilo guest. Después no tiene coste.

1. Si `HEDDLE_CPU` o `debug.heddle.cpu` nombran un modelo del catálogo, se usa ese modelo y el perfil no se lee.
   Los modelos son `cortex-a53`, `cortex-a55`, `cortex-a76`, `cortex-a77`, `cortex-a78`, `cortex-x1` y `max`.
   Con un modelo del catálogo solo se anuncia lo que heddle implementa.
2. Si no, se lee el archivo indicado en `HEDDLE_CPU_FILE` o `debug.heddle.cpu_file`. Sin esas variables, se lee
   `/system/etc/heddle/cpu.conf`.
3. Si no hay archivo, se usa Cortex-A78, limitado a lo que heddle implementa.

Si el archivo pedido por `HEDDLE_CPU_FILE` o `debug.heddle.cpu_file` no se puede leer, se registra y se aplica el
punto 3. Lo elegido queda en logcat:

```
cpu: perfil <ruta> '<nombre>' (MIDR 0x...); Features: ...
```

## Formato

El archivo es texto UTF-8 con una línea `clave=valor` por entrada:

- Los espacios alrededor de la clave y del valor se ignoran.
- Las claves no distinguen mayúsculas.
- `#` inicia un comentario hasta el final de la línea.
- Si una clave se repite, vale la última.
- Las líneas vacías se ignoran.
- Una clave desconocida, un valor no válido o una línea sin `=` se ignoran con un aviso en logcat:
  `cpu: perfil <ruta>: aviso: ...`.

Los números se escriben en hexadecimal con `0x` o en decimal, y admiten `_` como separador.

| Clave | Valor | Si falta |
|---|---|---|
| `nombre` | Texto libre. Aparece en logcat. `/proc/cpuinfo` de Linux arm64 no tiene una línea de nombre. | El nombre de `base`, o `perfil` |
| `hardware` | Texto libre. Se añade al final de `/proc/cpuinfo` como `Hardware\t: <texto>`, igual que algunos núcleos de fabricante; el Linux original no tiene esta línea. | Sin línea |
| `base` | Id de un modelo del catálogo; se acepta sin `cortex-`. Se parte de sus valores reales, **sin** limitarlos a lo que implementa heddle. | Cortex-A78 |
| `midr` | `MIDR_EL1`, 32 bits. `/proc/cpuinfo` deriva de él `CPU implementer`, `CPU variant`, `CPU part` y `CPU revision`. | El de la base |
| `revidr` | `REVIDR_EL1`, que solo se ve en sysfs (`revidr_el1`). `MRS REVIDR_EL1` devuelve 0, como lo emula Linux. | 0 |
| `ctr` | `CTR_EL0`, que el guest lee directamente. | El de la base |
| `dczid` | `DCZID_EL0`. | El de la base |
| `features` | Lista de nombres de `/proc/cpuinfo` de Linux arm64, separados por comas o espacios (ver abajo). | Las de la base |
| `id_aa64pfr0`, `id_aa64pfr1`, `id_aa64zfr0`, `id_aa64dfr0`, `id_aa64isar0`, `id_aa64isar1`, `id_aa64isar2`, `id_aa64mmfr0`, `id_aa64mmfr1`, `id_aa64mmfr2` | Valor de 64 bits del registro. También se acepta con el sufijo `_el1`. | Los de la base, modificados por `features` |

### `features`

Los nombres son los de la línea `Features` de Linux 6.6:

- **HWCAP:** `fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp sha512 sve asimdfhm dit uscat ilrcpc flagm ssbs sb paca pacg`.
- **HWCAP2:** `dcpodp sve2 sveaes svepmull svebitperm svesha3 svesm4 flagm2 frint svei8mm svef32mm svef64mm svebf16 i8mm bf16 dgh rng bti mte ecv afp rpres mte3 sme wfxt ebf16 sveebf16 cssc rprfm sve2p1 mops hbc`.

La lista se interpreta así:

- **Lista absoluta.** Si algún elemento no lleva `+` ni `-`, la lista se aplica desde un procesador vacío, sin FP ni
  AdvSIMD; hay que nombrar también `fp` y `asimd`. Los elementos con `+` o `-` de esa misma lista se aplican
  después.
- **Lista relativa.** Si todos llevan `+x` o `-x`, se añaden o quitan sobre `base`.
- `evtstrm` y `cpuid` siempre están presentes, porque Linux siempre los da en Android. Quitarlos se ignora con un
  aviso.
- No se pueden anunciar las características de SME que dependen de `ID_AA64SMFR0_EL1`, porque heddle no modela ese
  registro: `smei16i64`, `smef64f64`, `smei8i32`, `smef16f32`, `smeb16f32`, `smef32f32`, `smefa64`, `sme2`,
  `sme2p1`, `smei16i32`, `smebi32i32`, `smeb16b16` y `smef16f16`. Se ignoran con un aviso. `sme` sí se puede anunciar.

Cada nombre ajusta el campo de identificación correspondiente, haciendo a la inversa lo que hace el kernel, de modo
que `MRS`, HWCAP y `/proc/cpuinfo` quedan coherentes:

- Anunciar sube el campo al valor mínimo que hace falta. Por ejemplo, `pmull` deja `ISAR0.AES = 2` y `sha512` deja
  `ISAR0.SHA2 = 2`.
- Quitar lo baja justo por debajo de ese mínimo. Por ejemplo, `-pmull` deja `AES = 1`.
- `fp` y `asimd` se quitan con el valor 0xF, que significa «no implementado». `fphp` y `asimdhp` valen 1.
- Las características de SVE (`sve2`, `sveaes`...) activan también `sve`.
- `paca` pone `ISAR1.API = 1` y `pacg` pone `ISAR1.GPI = 1`. Quitarlos borra APA/API/APA3 y GPA/GPI/GPA3.

## Precedencia

El orden de las líneas en el archivo no importa. Los valores se aplican siempre en este orden:

1. `base`, o Cortex-A78 si falta.
2. `features`, de forma absoluta o relativa sobre la base.
3. Las claves `id_aa64*`, que sustituyen el registro **entero**, incluido lo que `features` haya cambiado en él.
4. El saneado de Linux en EL0: solo quedan los campos `FTR_VISIBLE`, y los ocultos toman su valor seguro (por
   ejemplo `PFR0.EL0 = 1` o `DFR0 = 6`). El saneado no cambia el resultado si se aplica dos veces, así que se pueden
   copiar valores del manual o los leídos con `MRS` en un dispositivo.
5. `AT_HWCAP` y `AT_HWCAP2` se calculan desde los registros finales, como hace `arm64_elf_hwcaps` en Linux 6.6.
   Siempre incluyen `evtstrm` y `cpuid`.

`midr`, `revidr`, `ctr` y `dczid` son independientes de los pasos anteriores.

## Qué implementa heddle (`cpu-features`)

Cada release lleva, junto a `libheddle.so` (en los dos paquetes), un archivo de texto `cpu-features` con una sola
línea: las extensiones que heddle implementa, con los nombres de la línea `Features` de `/proc/cpuinfo` de Linux
(los mismos que acepta `features`). Es lo que anuncia el modelo `max`; una prueba (`feat::tests::cpu_features_publicado`)
comprueba que coincide con `src/feat.rs`. Un perfil que anuncie algo fuera de esa lista recibirá SIGILL al usarlo.
Quien instala el traductor puede copiarlo para avisar al crear un perfil; no es obligatorio.

## Ejemplo

```
# Cortex-A710 (2022). Lo que heddle no implementa (sve, sve2, bf16, paca...) se anuncia igual y el aviso lo lista
nombre=Cortex-A710
midr=0x412FD471
base=cortex-a78
features=+sve,+sve2,+svebitperm,+bf16,+i8mm,+paca,+pacg,+bti,+ssbs,+flagm,+flagm2,+frint,+jscvt,+fcma,+ilrcpc,+asimdfhm,+sha3,+sha512,+sb,+dit,+uscat,+dcpodp
```

## Lo que no se puede expresar

- Hay un solo `MIDR` para todas las CPUs. En un procesador big.LITTLE real, cada núcleo muestra el suyo en
  `/proc/cpuinfo` y en sysfs.
- `ID_AA64SMFR0_EL1`, y con él las características de SME de grano fino, se lee siempre como 0.
- `MPIDR_EL1` y `REVIDR_EL1` se leen con `MRS` como los emula Linux: el bit 31 y 0.
