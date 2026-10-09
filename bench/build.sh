#!/usr/bin/env bash
# Compila los benchmarks Rust (x86-64) en ./out
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p out
for f in arm_xlat_suite arm_xlat_bench arm_xlat_strict ldxr_bench monitor_strict; do
  rustc --edition 2021 -C opt-level=3 src/$f.rs -o out/$f
done
echo "Binarios en out/. La version nativa ARM (native/arm_native.c) se compila en un equipo AArch64:"
echo "  clang -O2 -pthread -Inative native/arm_native.c -o arm_native"
