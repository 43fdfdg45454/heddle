#!/usr/bin/env bash
# Genera tests/guest/tld_ref.h: los resultados de tld.c con la bionic real de arm64 (libc.a del NDK, enlazado
# estatico) ejecutada con qemu-aarch64. Uso: NDK=<ruta al NDK> tools/tld_ref.sh
set -euo pipefail
cd "$(dirname "$0")/.."
: "${NDK:?NDK=<ruta al NDK>}"
CC="$NDK/toolchains/llvm/prebuilt/linux-x86_64/bin/clang"
QEMU="${QEMU:-$(command -v qemu-aarch64-static || command -v qemu-aarch64)}"
W="$(mktemp -d)"; trap 'rm -rf "$W"' EXIT
"$CC" --target=aarch64-linux-android35 -static -O1 -DTLD_REF -Wno-builtin-requires-header -Wno-incompatible-library-redeclaration \
  -o "$W/tld_ref" tests/guest/tld.c
"$QEMU" "$W/tld_ref" > tests/guest/tld_ref.h
echo "tests/guest/tld_ref.h: $(grep -c '^    "' tests/guest/tld_ref.h) lineas"
