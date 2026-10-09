#!/usr/bin/env bash
# Compila las bibliotecas guest (aarch64) y los programas host de prueba en ./build
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p build
# Las pruebas declaran sus DT_NEEDED reales (libc.so, libm.so, libdl.so...), como una biblioteca del NDK: se enlazan
# con --as-needed contra stubs de las bibliotecas del sistema generados desde sigs::EXPORTS (build/ndk). El -L de los
# stubs va el ultimo: una prueba que se enlaza con su propia libc.so de mentira (-Lbuild/stub...) la sigue usando.
python3 tools/mkstubs.py ../src/sigs_gen.rs build/ndk clang --target=aarch64-linux-gnu -fuse-ld=lld
NDK_LIBS="-Lbuild/ndk -Wl,--as-needed $(cd build/ndk && for l in *.so; do printf -- '-l:%s ' "$l"; done) -Wl,--no-as-needed"
# libc, libm y libdl primero (un simbolo que exportan varias se atribuye a la primera, como en el NDK)
NDK_LIBS="-Lbuild/ndk -Wl,--as-needed -l:libc.so -l:libm.so -l:libdl.so ${NDK_LIBS#-Lbuild/ndk -Wl,--as-needed }"
a64() {
  clang --target=aarch64-linux-gnu -O2 -shared -fPIC -fuse-ld=lld -nostdlib -Wno-builtin-requires-header "$@" $NDK_LIBS
}
CC_A64=a64
for t in t1 t3 t4 t6 t7 t9 jni1 bench bench2 tld tvk; do
  $CC_A64 -o build/lib$t.so tests/guest/$t.c
done
# t2 depende de libt1.so (funcion fact)
$CC_A64 -Lbuild -l:libt1.so -o build/libt2.so tests/guest/t2.c
# t5: recarga tras dlclose (libt5ctor -> libt5dep) y TLS importado de otra biblioteca (libt5use -> libt5mid -> libt5tls)
for t in t5 t5dep t5tls; do
  $CC_A64 -o build/lib$t.so tests/guest/$t.c
done
$CC_A64 -Lbuild -l:libt5dep.so -o build/libt5ctor.so tests/guest/t5ctor.c
$CC_A64 -Lbuild -l:libt5tls.so -o build/libt5mid.so tests/guest/t5mid.c
$CC_A64 -Lbuild -l:libt5mid.so -o build/libt5use.so tests/guest/t5use.c
# t8: cargador como bionic (descarga real, direccion nueva, TLS initial-exec en dlopen, ciclo libt8a <-> libt8b)
for t in t8log t8tls t8ieself; do
  $CC_A64 -o build/lib$t.so tests/guest/$t.c
done
$CC_A64 -Lbuild -l:libt8tls.so -o build/libt8ie.so tests/guest/t8ie.c
$CC_A64 -Lbuild -l:libt8log.so -o build/libt8c.so tests/guest/t8c.c
# el ciclo se enlaza en dos pasadas: libt8a provisional, libt8b contra ella y libt8a definitiva contra libt8b
$CC_A64 -Lbuild -l:libt8log.so -o build/libt8a.so tests/guest/t8a.c
$CC_A64 -Lbuild -l:libt8a.so -l:libt8log.so -o build/libt8b.so tests/guest/t8b.c
$CC_A64 -Lbuild -l:libt8b.so -l:libt8log.so -o build/libt8a.so tests/guest/t8a.c
$CC_A64 -Lbuild -l:libt8log.so -o build/libt8.so tests/guest/t8.c
# t10: el cargador como bionic (targetSdkVersion y DT_SONAME, ...). libt10noso.so sin DT_SONAME y libt10so.so con el,
# cada una tambien en build/alt/ (otro inodo); libt10link.so es un enlace simbolico a libt10noso.so
mkdir -p build/alt
for d in build build/alt; do
  $CC_A64 -o $d/libt10noso.so tests/guest/t10noso.c
  $CC_A64 -Wl,-soname,libt10so.so -o $d/libt10so.so tests/guest/t10so.c
done
ln -sf libt10noso.so build/libt10link.so
$CC_A64 -o build/libt10.so tests/guest/t10.c
$CC_A64 -Wl,-soname,libt10tls.so -o build/libt10tls.so tests/guest/t10tls.c
# t10, colocacion: p_align de 2 MiB y seis dependencias nuevas (orden de mapeo barajado)
$CC_A64 -Wl,-z,max-page-size=0x200000 -o build/libt10huge.so tests/guest/t10huge.c
for i in 1 2 3 4 5 6; do $CC_A64 -o build/libt10p_$i.so tests/guest/t10p_$i.c; done
$CC_A64 -Lbuild -l:libt10p_1.so -l:libt10p_2.so -l:libt10p_3.so -l:libt10p_4.so -l:libt10p_5.so -l:libt10p_6.so \
  -o build/libt10p_root.so tests/guest/t10p_root.c
# t11: llamadas TLSDESC en linea del JIT (libt11 -> libt11b, otro modulo TLS)
$CC_A64 -o build/libt11b.so tests/guest/t11b.c
$CC_A64 -Lbuild -l:libt11b.so -o build/libt11.so tests/guest/t11.c
# microbanco de TLS dinamico (no forma parte de run-all.sh)
$CC_A64 -o build/libtlsbench2.so tests/guest/tlsbench2.c
$CC_A64 -Lbuild -l:libtlsbench2.so -o build/libtlsbench.so tests/guest/tlsbench.c
# microbanco del cargador (no forma parte de run-all.sh): dlopen/dlclose y dlsym
$CC_A64 -w -fno-builtin -o build/libdlbench2.so tests/guest/dlbench2.c
$CC_A64 -o build/libdlbench.so tests/guest/dlbench.c
# t10, orden de busqueda: libt10o_root -> (libt10o_a -> libt10o_c), libt10o_b; -fsemantic-interposition para que las
# llamadas a funciones propias exportadas pasen por la PLT (se puedan interponer, como en GCC)
SI="-fsemantic-interposition"
for t in t10o_c t10o_b t10o_g t10o_user; do
  $CC_A64 $SI -Wl,-soname,lib$t.so -o build/lib$t.so tests/guest/$t.c
done
$CC_A64 $SI -Wl,-z,global -Wl,-soname,libt10o_df1.so -o build/libt10o_df1.so tests/guest/t10o_df1.c
$CC_A64 $SI -Lbuild -l:libt10o_c.so -Wl,-soname,libt10o_a.so -o build/libt10o_a.so tests/guest/t10o_a.c
$CC_A64 $SI -Lbuild -l:libt10o_a.so -l:libt10o_b.so -Wl,-soname,libt10o_root.so -o build/libt10o_root.so tests/guest/t10o_root.c
# t10 en mockart (espacios de nombres): build/ns/libt10ns.so necesita libc.so (enlazada con una de mentira que no
# esta en ninguna ruta) y build/ns/dep/libt10nsdep.so
mkdir -p build/ns/dep build/stub
$CC_A64 -Wl,-soname,libc.so -o build/stub/libc.so tests/guest/t10stubc.c
$CC_A64 -Wl,-soname,libt10nsdep.so -o build/ns/dep/libt10nsdep.so tests/guest/t10nsdep.c
$CC_A64 -Lbuild/stub -Lbuild/ns/dep -l:libc.so -l:libt10nsdep.so -o build/ns/libt10ns.so tests/guest/t10ns.c
$CC_A64 -o build/ns/libt10nsg.so tests/guest/t10nsg.c
# t10, simbolos como bionic: indefinido en todas partes (dlopen falla), exportado por el NDK pero no servido (fallo
# perezoso), versiones propias (dlvsym) y DT_VERNEED contra libc (buena y mala)
$CC_A64 -o build/libt10undef.so tests/guest/t10undef.c
$CC_A64 -o build/libt10lazy.so tests/guest/t10lazy.c
# sin DT_NEEDED de libc.so (sin los stubs): strlen no esta en su lista de busqueda
clang --target=aarch64-linux-gnu -O2 -shared -fPIC -fuse-ld=lld -nostdlib -o build/libt10noneed.so tests/guest/t10noneed.c
$CC_A64 -Wl,--version-script=tests/guest/t10ver.map -o build/libt10ver.so tests/guest/t10ver.c
mkdir -p build/stubv_ok build/stubv_bad
for v in ok bad; do
  $CC_A64 -Wl,-soname,libc.so -Wl,--version-script=tests/guest/t10stubv_$v.map -o build/stubv_$v/libc.so tests/guest/t10stubv.c
done
$CC_A64 -Lbuild/stubv_ok -l:libc.so -o build/libt10vneed.so tests/guest/t10vneed.c
$CC_A64 -Lbuild/stubv_bad -l:libc.so -o build/libt10vbad.so tests/guest/t10vneed.c
# t10, rutas reales: biblioteca dentro de un APK sin comprimir (alineada a 16 KiB, como zipalign -P 16) y otra sin
# alinear (bionic no la abre)
$CC_A64 -o build/libt10apk.so tests/guest/t10apk.c
python3 tools/mkapk.py build/t10.apk 16384 lib/arm64-v8a/libt10apk.so=build/libt10apk.so
python3 tools/mkapk.py build/t10bad.apk 0 lib/arm64-v8a/libt10apk.so=build/libt10apk.so
# t10, android_dlopen_ext: por descriptor y en una region reservada; RELRO compartido
$CC_A64 -Wl,-soname,libt10ext.so -o build/libt10ext.so tests/guest/t10ext.c
$CC_A64 -o build/libt10relro.so tests/guest/t10relro.c
# t10 con paginas de 16 KiB: biblioteca alineada a 4 KiB (modo de compatibilidad de bionic)
$CC_A64 -Wl,-z,max-page-size=4096 -o build/libt10p4k.so tests/guest/t10p4k.c
# t10: protecciones de segmentos, nota PAD_SEGMENT (p_align de 16 KiB) y relocaciones de texto (bionic las rechaza)
$CC_A64 -o build/libt10seg.so tests/guest/t10seg.c
$CC_A64 -DPAD -Wl,-z,max-page-size=16384 -o build/libt10pad.so tests/guest/t10seg.c
$CC_A64 -Wl,-z,notext -o build/libt10textrel.so tests/guest/t10textrel.c
# t10, modo de compatibilidad de 16 KiB automatico: APK con manifiesto binario (android:pageSizeCompat,
# extractNativeLibs) y bibliotecas de 4 o 16 KiB, sin comprimir o comprimidas; app instalada (base.apk y lib/arm64/)
mkdir -p build/compat/app/lib/arm64 build/compat/app2
M=build/compat
python3 tools/mkaxml.py $M/m_noext.xml extractNativeLibs=false
python3 tools/mkaxml.py $M/m_def.xml --utf8
python3 tools/mkaxml.py $M/m_dis.xml pageSizeCompat=64 extractNativeLibs=false
python3 tools/mkaxml.py $M/m_en.xml --utf8 pageSizeCompat=32 extractNativeLibs=false
python3 tools/mkaxml.py $M/m_nomap.xml --sin-mapa pageSizeCompat=64 extractNativeLibs=false
L4=lib/arm64-v8a/libt10p4k.so
python3 tools/mkapk.py $M/c4k.apk 16384 z:AndroidManifest.xml=$M/m_noext.xml $L4=build/libt10p4k.so
python3 tools/mkapk.py $M/c4kext.apk 16384 z:AndroidManifest.xml=$M/m_def.xml z:$L4=build/libt10p4k.so
python3 tools/mkapk.py $M/cdis.apk 16384 z:AndroidManifest.xml=$M/m_dis.xml $L4=build/libt10p4k.so
python3 tools/mkapk.py $M/cen.apk 16384 z:AndroidManifest.xml=$M/m_en.xml lib/arm64-v8a/libt10seg.so=build/libt10seg.so
python3 tools/mkapk.py $M/c16k.apk 16384 z:AndroidManifest.xml=$M/m_noext.xml lib/arm64-v8a/libt10seg.so=build/libt10seg.so
python3 tools/mkapk.py $M/ccomp.apk 16384 z:AndroidManifest.xml=$M/m_noext.xml z:$L4=build/libt10p4k.so
python3 tools/mkapk.py $M/cnomap.apk 16384 z:AndroidManifest.xml=$M/m_nomap.xml $L4=build/libt10p4k.so
python3 tools/mkapk.py $M/app/base.apk 16384 z:AndroidManifest.xml=$M/m_def.xml z:$L4=build/libt10p4k.so
cp build/libt10p4k.so $M/app/lib/arm64/
python3 tools/mkapk.py $M/app2/base.apk 16384 z:AndroidManifest.xml=$M/m_noext.xml
python3 tools/mkapk.py $M/app2/split_config.arm64_v8a.apk 16384 $L4=build/libt10p4k.so
# t10 en mockart, lista de excepciones de bionic (targetSdkVersion < 24): libssl.so solo en el espacio por defecto
mkdir -p build/exempt
$CC_A64 -Wl,-soname,libssl.so -o build/exempt/libssl.so tests/guest/t10exempt.c
$CC_A64 -Wl,-soname,libt10noexempt.so -o build/exempt/libt10noexempt.so tests/guest/t10exempt.c
cc -O2 -shared -fPIC -o build/libmockandroid.so tests/host/mock_android.c
# t9: host simulado de OpenSL ES, OpenMAX AL, AAudio, AMediaCodec, camara... (se carga con LD_PRELOAD)
cc -O2 -shared -fPIC -o build/libmockndk.so tests/host/mock_ndk.c -lpthread
cc -O2 -o build/nbtest2 tests/host/nbtest2.c -ldl
cc -O2 -o build/mockart tests/host/mockart.c -ldl -lpthread
echo "Compilado en build/"
