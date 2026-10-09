#!/usr/bin/env bash
# Compila la biblioteca guest de heddle: codigo ARM64 con lo que NO se puede reenviar al host: las funciones de la
# libm con long double de 128 bits y numeros complejos, y la conversion texto <-> long double de la libc (strtold,
# wcstold y las conversiones %L de printf/scanf). El resto de la libm y de la libc sigue siendo la del host.
#
# Fuente: la libm y la libc de bionic (misma implementacion que un dispositivo arm64 real). Las funciones exportadas
# son exactamente las que la tabla de firmas (src/sigs_gen.rs) marca como ABI incompatible en libm.so, las de
# libc.so que devuelven long double (strtold, strtold_l, wcstold, wcstold_l) y el formateador interno
# internos que resolve() no entrega al guest: __heddle_snprintf_ld (vfprintf de bionic para una conversion %L) y
# __heddle_strtold_fast/__heddle_strtold_slow (cada ruta de strtold por separado, para las pruebas).
#
# Uso: guest/build.sh <bionic> <salida.so>
#   <bionic>  copia de platform/bionic (se usan libm/ y libc/: include, private, platform, async_safe, stdio,
#             upstream-openbsd)  -> ver guest/BIONIC_COMMIT
# Entorno (uno de los dos):
#   NDK=<ruta al NDK>                       usa su clang, sus cabeceras y su biblioteca auxiliar
#   SYSROOT=<sysroot exportado por el CI>   include/, lib/aarch64-linux-android/, rt/ ; usa el clang del sistema
set -euo pipefail
BIONIC="$1"; OUT="$2"
HERE="$(cd "$(dirname "$0")" && pwd)"
M="$BIONIC/libm"; F="$M/upstream-freebsd/lib/msun"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT

if [ -n "${NDK:-}" ]; then
  T="$NDK/toolchains/llvm/prebuilt/linux-x86_64"
  CC="$T/bin/clang"
  INC=()
  RT="$(find "$T/lib/clang" -name 'libclang_rt.builtins-aarch64-android.a' | head -n1)"
  LIBS="$T/sysroot/usr/lib/aarch64-linux-android/35"
else
  CC="${CC:-clang}"
  INC=(-nostdinc -isystem "$SYSROOT/include/aarch64-linux-android" -isystem "$SYSROOT/include" -isystem "$("$CC" -print-resource-dir)/include")
  RT="$SYSROOT/rt/libclang_rt.builtins-aarch64-android.a"
  LIBS="$SYSROOT/lib/aarch64-linux-android"
fi

# 1) fuentes: la lista lib64 de libm/Android.bp (long double) + complejos + auxiliares
mapfile -t SRCS < <(
  sed -n '/lib64: {/,/local_include_dirs/p' "$M/Android.bp" | grep -o '"upstream-freebsd[^"]*\.c"' | tr -d '"' | sed "s|^|$M/|"
  ls "$F"/src/catrig*.c "$F"/src/s_c{arg,cosh,exp,imag,log,onj,pow,proj,real,sinh,sqrt,tanh}*.c "$F"/src/w_cabs*.c "$F"/src/k_exp*.c 2>/dev/null
  ls "$M"/upstream-netbsd/lib/libm/complex/*.c "$M"/significandl.c 2>/dev/null
  # variantes long double generadas por macro en archivos comunes, y auxiliares internos que el host no exporta
  ls "$F"/src/s_fdim.c "$F"/src/s_nearbyint.c "$F"/src/s_scalbln.c "$F"/src/k_rem_pio2.c "$F"/src/s_nan.c 2>/dev/null || true
  echo "$HERE/extra.c"
)
mapfile -t SRCS < <(printf '%s\n' "${SRCS[@]}" | sort -u)

CFLAGS=(--target=aarch64-linux-android35 -O2 -fPIC -fvisibility=default -ffunction-sections -fno-builtin -fno-math-errno
  -include "$M/freebsd-compat.h" -I "$M/upstream-freebsd/android/include" -I "$F/src" -I "$F/ld128" -I "$M" -I "$BIONIC/libc"
  -Wno-everything "${INC[@]}")
OBJS=()
for s in "${SRCS[@]}"; do
  o="$W/$(basename "${s%.c}").o"
  "$CC" "${CFLAGS[@]}" -c "$s" -o "$o"
  OBJS+=("$o")
done

# 1b) libc: texto <-> long double. gdtoa (la lista de libc_gdtoa en libc/Android.bp, con strtorQ de lib64), el
# vfprintf de bionic (para las conversiones %L), parsefloat (wcstold) y fvwrite (salida a cadena); guest/stdio_ld.cpp
# replica los envoltorios de bionic (strtold.cpp, stdlib_l.cpp, wcstod.cpp, vsnprintf) y define como internos los
# caminos de stdio que no se recorren; guest/include/thread_private.h da a los bloqueos de gdtoa un mutex completo. Todo con visibilidad oculta salvo lo exportado en el paso 2 (strtod, strtof...
# de gdtoa quedan internas y --gc-sections las descarta: el host las sirve).
C="$BIONIC/libc"; O="$C/upstream-openbsd"; G="$O/lib/libc/gdtoa"
LFLAGS=(--target=aarch64-linux-android35 -O2 -fPIC -fvisibility=hidden -ffunction-sections -fno-builtin
  -include "$O/android/include/openbsd-compat.h" -I "$HERE/include" -I "$C/private" -I "$O/android/include" -I "$O/lib/libc/include"
  -I "$C/stdio" -I "$G" -I "$O/lib/libc/stdio" -I "$C" -I "$C/platform" -I "$C/async_safe/include" -Wno-everything "${INC[@]}")
LSRCS=("$G"/{dmisc,dtoa,gdtoa,gethex,gmisc,hd_init,hdtoa,hexnan,ldtoa,misc,smisc,strtod,strtodg,strtof,strtord,sum,ulp,strtorQ}.c
  "$O/lib/libc/stdio/fvwrite.c" "$C/stdio/vfprintf.cpp" "$C/stdio/parsefloat.c" "$HERE/stdio_ld.cpp")
for s in "${LSRCS[@]}"; do
  b="$(basename "$s")"; o="$W/libc_${b%.*}.o"
  case "$s" in
    # nl_langinfo(RADIXCHAR) de bionic es siempre "." (sin locales con otra coma); se toma de stdio_ld.cpp para no
    # depender de la numeracion de los items de la libc del host (glibc en las pruebas en Linux)
    */vfprintf.cpp) "$CC" -x c++ -std=gnu++20 -fno-exceptions -fno-rtti "${LFLAGS[@]}" -Dnl_langinfo=__heddle_nl_langinfo -c "$s" -o "$o" ;;
    # el mutex de gdtoa con exclusivos de ARMv8.0 (LDAXR/STLXR) en linea: LSE solo se ejecuta si el modelo de CPU la
    # anuncia (el Cortex-A53 no), y sin la llamada a __aarch64_cas4_*
    */stdio_ld.cpp) "$CC" -x c++ -std=gnu++20 -fno-exceptions -fno-rtti -march=armv8-a -mno-outline-atomics "${LFLAGS[@]}" -c "$s" -o "$o" ;;
    *.cpp) "$CC" -x c++ -std=gnu++20 -fno-exceptions -fno-rtti "${LFLAGS[@]}" -c "$s" -o "$o" ;;
    # el FILE de mentira de wcstold: su __srefill y su ungetc son los internos de stdio_ld.cpp
    */parsefloat.c) "$CC" "${LFLAGS[@]}" -D__srefill=__heddle_srefill -Dungetc=__heddle_ungetc -c "$s" -o "$o" ;;
    *) "$CC" "${LFLAGS[@]}" -c "$s" -o "$o" ;;
  esac
  OBJS+=("$o")
done

# DT_NEEDED de libm.so y libc.so, como cualquier biblioteca del NDK: el cargador resuelve los simbolos del sistema
# (signgam, fe*, malloc...) solo en la lista de busqueda de la biblioteca (grupo local de bionic).
# 2) exportar SOLO lo inevitable: funciones de libm.so que la tabla marca como ABI incompatible, las de libc.so que
# devuelven long double y los internos __heddle_* (src/guestlib.rs comprueba que coincide)
{
  echo "HEDDLE_GUEST {"; echo "  global:"
  { grep 'lib: "libm.so", k: K::Unsafe' "$HERE/../src/sigs_gen.rs"
    grep 'lib: "libc.so", k: K::Unsafe("retorno long double")' "$HERE/../src/sigs_gen.rs"
  } | grep -o 'name: "[^"]*"' | cut -d'"' -f2 | sed 's/^/    /;s/$/;/'
  echo "    __heddle_snprintf_ld;"
  echo "    __heddle_strtold_fast;"
  echo "    __heddle_strtold_slow;"
  echo "  local: *;"; echo "};"
} > "$W/exports.map"

"$CC" --target=aarch64-linux-android35 -shared -nostdlib -fuse-ld=lld -Wl,--gc-sections -Wl,-Bsymbolic -Wl,-soname,libheddle_guest.so \
  -Wl,--version-script="$W/exports.map" -Wl,--undefined-version -Wl,--hash-style=both -Wl,-z,max-page-size=16384 \
  -o "$OUT" "${OBJS[@]}" "$RT" -L"$LIBS" -lm -lc
echo "biblioteca guest: $OUT ($(stat -c %s "$OUT") bytes, $((${#SRCS[@]} + ${#LSRCS[@]})) fuentes)"
