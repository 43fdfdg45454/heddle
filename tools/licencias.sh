#!/usr/bin/env bash
# Reune en <destino>/LICENSES los avisos que deben acompanar a los binarios publicados (ver NOTICE).
#
# Uso: tools/licencias.sh <destino> [<bionic>]
#   <bionic>  copia de platform/bionic usada para compilar la biblioteca guest (se copian libm/NOTICE y libc/NOTICE).
# Si la biblioteca guest esta compilada (guest/out/libheddle_guest.so) y falta un NOTICE de bionic, falla: el
# binario la incrusta y la clausula 2 de las licencias BSD exige reproducir sus avisos.
set -euo pipefail
DEST="$1"; BIONIC="${2:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
L="$DEST/LICENSES"
mkdir -p "$L/rust-std"
cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$L/"
cp "$ROOT/README.md" "$DEST/"
if [ -n "$BIONIC" ] && [ -f "$BIONIC/libm/NOTICE" ] && [ -f "$BIONIC/libc/NOTICE" ]; then
  cp "$BIONIC/libm/NOTICE" "$L/bionic-libm-NOTICE"
  cp "$BIONIC/libc/NOTICE" "$L/bionic-libc-NOTICE"
elif [ -f "$ROOT/guest/out/libheddle_guest.so" ]; then
  echo "licencias: la biblioteca guest esta compilada pero falta <bionic>/libm/NOTICE o <bionic>/libc/NOTICE" >&2
  exit 1
fi
DOC="$(rustc --print sysroot)/share/doc/rust"
if [ ! -f "$DOC/COPYRIGHT-library.html" ]; then
  echo "licencias: el toolchain no trae $DOC/COPYRIGHT-library.html" >&2
  exit 1
fi
cp "$DOC/COPYRIGHT-library.html" "$L/rust-std/"
cp -r "$DOC/licenses" "$L/rust-std/"
