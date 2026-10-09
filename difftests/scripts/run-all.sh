#!/usr/bin/env bash
# Ejecuta las pruebas guest y las del puente. Uso: [HEDDLE=ruta/a/heddle] scripts/run-all.sh
# (HEDDLE debe tener target/release compilado; por defecto la raiz de este repo)
set -euo pipefail
cd "$(dirname "$0")/.."
AB="${HEDDLE:-..}"
RUN="$AB/target/release/heddle-run"
LIB="$AB/target/release/libheddle.so"
fail=0
export LD_LIBRARY_PATH="$PWD/build:${LD_LIBRARY_PATH:-}"
# dlopen desde el guest busca en el directorio de las bibliotecas de la app (en Android, el del namespace)
export HEDDLE_LIBPATH="$PWD/build"
# tvk: Vulkan con el ICD por software (lavapipe) si esta instalado, para no depender de la GPU del equipo
lvp=/usr/share/vulkan/icd.d/lvp_icd.x86_64.json
[ -f "$lvp" ] && export VK_DRIVER_FILES="$lvp" VK_ICD_FILENAMES="$lvp"
for t in t1 t2 t3 t5 t6 t8 t11 tld tvk; do
  out=$("$RUN" build/lib$t.so run_all 2>&1 | tail -1 || true)
  echo "$t: $out"
  case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
done
# las mismas con otros modelos de CPU (HEDDLE_CPU): se ejecuta lo que el modelo anuncia; t3 usa LSE (ARMv8.1), que el
# Cortex-A53 (ARMv8.0) no tiene: SIGILL como en ese procesador
for cpu in max cortex-a55 cortex-a53; do
  for t in t1 t2 t3 t5 t6 t8 t11; do
    if [ "$cpu $t" = "cortex-a53 t3" ]; then
      set +e; HEDDLE_CPU=$cpu "$RUN" build/lib$t.so run_all >/dev/null 2>build/a53.err; got=$?; set -e
      if [ "$got" = 132 ] && grep -qF "de FEAT_LSE (" build/a53.err && grep -qF "ausente en el modelo de CPU Cortex-A53" build/a53.err; then
        echo "$cpu $t: SIGILL (FEAT_LSE ausente en el modelo)"
      else
        echo "$cpu $t: salida $got, esperada 132 por FEAT_LSE"; cat build/a53.err; fail=1
      fi
      continue
    fi
    out=$(HEDDLE_CPU=$cpu "$RUN" build/lib$t.so run_all 2>&1 | tail -1 || true)
    echo "$cpu $t: $out"
    case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
  done
done
# t9: proxys del NDK contra el host simulado (sus simbolos entran en el espacio global con LD_PRELOAD)
out=$(LD_PRELOAD="$PWD/build/libmockndk.so" "$RUN" build/libt9.so run_all 2>&1)
echo "$out" | grep -E "^t9 (OpenSL|AAudio)" || true
out=$(echo "$out" | tail -1)
echo "t9: $out"
case "$out" in *"0 fallos"*) ;; *) fail=1 ;; esac
# t10: el cargador como bionic con distintos targetSdkVersion (DT_SONAME, ...)
for sdk in 35 30 22; do
  out=$(HEDDLE_TARGET_SDK=$sdk "$RUN" build/libt10.so run_all 2>&1 | grep "run_all:" | tail -1 || true)
  echo "t10 sdk $sdk: $out"
  case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
done
# t10 con paginas de 16 KiB (getpagesize, mmap y el cargador como un dispositivo de 16 KiB)
out=$(HEDDLE_PAGE_SIZE=16384 "$RUN" build/libt10.so run_all 2>&1 | grep "run_all:" | tail -1 || true)
echo "t10 paginas de 16 KiB: $out"
case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
# t10 con migracion de tamano de pagina (nota NT_ANDROID_TYPE_PAD_SEGMENT: segmentos extendidos sin huecos)
out=$(HEDDLE_PGSIZE_MIGRATION=1 "$RUN" build/libt10.so run_all 2>&1 | grep "run_all:" | tail -1 || true)
echo "t10 pad_segment: $out"
case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
# t10, modo de compatibilidad de 16 KiB automatico como Android 16: "variables|esperado|descripcion"
M=build/compat
while IFS='|' read -r vars want what; do
  out=$(env -u HEDDLE_PAGE_COMPAT -u HEDDLE_APK -u HEDDLE_NATIVE_LIB_DIR HEDDLE_PAGE_SIZE=16384 T10_COMPAT=$want $vars \
    "$RUN" build/libt10.so compat_auto 2>&1 | grep "compat_auto:" | tail -1 || true)
  echo "t10 compat ($what): $out"
  case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
done <<EOF2
HEDDLE_APK=$M/c4k.apk|1|biblioteca de 4 KiB en el APK
HEDDLE_APK=$M/c4kext.apk HEDDLE_NATIVE_LIB_DIR=build|1|extraida de 4 KiB
HEDDLE_APK=$M/cdis.apk|0|manifiesto disabled
HEDDLE_APK=$M/cen.apk|1|manifiesto enabled con bibliotecas de 16 KiB
HEDDLE_APK=$M/c16k.apk|0|bibliotecas de 16 KiB
HEDDLE_APK=$M/ccomp.apk|0|comprimida sin extraer: error
HEDDLE_APK=$M/c4k.apk HEDDLE_PAGE_COMPAT=0|0|desactivado en los ajustes
HEDDLE_APK=$M/cnomap.apk HEDDLE_NATIVE_LIB_DIR=build|1|atributos sin identificador de recurso
T10_COMPAT_LIB=$M/app/lib/arm64/libt10p4k.so|1|app instalada, extraida
T10_COMPAT_LIB=$PWD/$M/app2/split_config.arm64_v8a.apk!/lib/arm64-v8a/libt10p4k.so|1|app instalada, en el APK dividido
|0|sin app
EOF2
# t7: codigos de salida de heddle-run (estado del guest, o su misma senal) y nombre de la instruccion no soportada
# Cada caso: "[CPU=modelo|CPU=perfil] funcion salida[:texto del diagnostico]" (modelo: HEDDLE_CPU, por defecto el de
# heddle-run, Cortex-A78; perfil: HEDDLE_CPU_FILE con build/perfil.conf, que anuncia extensiones que heddle no tiene)
printf '# perfil de prueba\nnombre=Perfil t7\nmidr=0x41000001\nfeatures=fp,asimd,flagm,flagm2,sm4,sve\n' > build/perfil.conf
st() {
  set +e
  if [ "$1" = perfil ]; then HEDDLE_CPU_FILE="$PWD/build/perfil.conf" "$RUN" build/libt7.so "$2" >/dev/null 2>build/t7.err
  else HEDDLE_CPU="$1" "$RUN" build/libt7.so "$2" >/dev/null 2>build/t7.err; fi
  echo $?; set -e
}
for c in "ret42 42" "ret300 44" "exit7 7" "sm4e 132:SM4E (0xcec08400)" "udf 132:UDF #0x0" "smstart 132:SMSTART (0xd503477f)" \
  "mrs_rndr 132:MRS S3_3_C2_C4_0 (0xd53b2400)" "flagm 132:FEAT_FlagM (0xd500401f), ausente en el modelo de CPU Cortex-A78" \
  "CPU=cortex-a53 flagm 132:FEAT_FlagM (0xd500401f), ausente en el modelo de CPU Cortex-A53" "CPU=max flagm 21" \
  "retaa 132:RETAA (0xd65f0bff)" "CPU=max retaa 132:RETAA (0xd65f0bff)" \
  "cpu_files 0" "CPU=cortex-a53 cpu_files 0" "CPU=cortex-x1 cpu_files 0" "CPU=max cpu_files 0" \
  "CPU=perfil flagm 21" "CPU=perfil sm4e 132:SM4E (0xcec08400)" "CPU=perfil cpu_files 0" \
  "CPU=perfil ret42 42:AVISO: el perfil anuncia extensiones que heddle no implementa (si el guest las usa recibe SIGILL): sm4 sve" \
  "chain_segv 139" "chain_abort 134" "CPU=max ssbs_signal 0" \
  "ssbs_signal 132:FEAT_SSBS2 (0xd53b42" "no_existe 1"; do
  cpu=""; case "$c" in CPU=*) cpu=${c%% *}; cpu=${cpu#CPU=}; c=${c#* } ;; esac
  f=${c%% *}; want=${c#* }; code=${want%%:*}; msg=${want#*:}
  got=$(st "$cpu" "$f")
  if [ "$got" = "$code" ] && { [ "$msg" = "$want" ] || grep -qF "$msg" build/t7.err; }; then
    echo "t7 ${cpu:+$cpu }$f: salida $got"
  else
    echo "t7 ${cpu:+$cpu }$f: salida $got, esperada $want"; cat build/t7.err; fail=1
  fi
done
# %n en la familia printf: como bionic, SIGABRT (134) con "FORTIFY: %n not allowed on Android" en stderr
for f in n_printf n_snprintf n_snprintf_ld n_vsnprintf n_snprintf_chk n_swprintf n_swprintf_ld n_wprintf; do
  set +e; "$RUN" build/libtld.so $f >/dev/null 2>build/tldn.err; got=$?; set -e
  if [ "$got" = 134 ] && grep -qxF "FORTIFY: %n not allowed on Android" build/tldn.err; then
    echo "tld $f: SIGABRT"
  else
    echo "tld $f: salida $got, esperada 134 con el mensaje de bionic"; cat build/tldn.err; fail=1
  fi
done
set +e; "$RUN" build/libtld.so n_ninguno >/dev/null 2>&1; got=$?; set -e
[ "$got" = 5 ] && echo "tld n_ninguno: salida 5" || { echo "tld n_ninguno: salida $got, esperada 5"; fail=1; }
for cmd in "build/nbtest2 $LIB build/libt4.so" "build/mockart $LIB build/libjni1.so"; do
  out=$($cmd 2>&1 | grep -E ": [0-9]+ fallos" | tail -1)
  echo "$cmd -> $out"
  case "$out" in *": 0 fallos"*) ;; *) fail=1 ;; esac
done
exit $fail
