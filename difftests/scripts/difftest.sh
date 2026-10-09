#!/usr/bin/env bash
# Pruebas diferenciales contra Unicorn.
# Uso: [HEDDLE=..] [PYTHON=..] [MOTORES="jit interp"] scripts/difftest.sh [N] [SEMILLA] [clases|todas]
# HEDDLE: raiz del repo heddle con target/release compilado (por defecto la raiz de este repo).
# PYTHON: interprete con el modulo unicorn (por defecto deps/venv/bin/python, ver scripts/setup-debian.sh).
# Sale con 1 si algun motor tiene discrepancias que no estan en la lista de exclusiones.
set -euo pipefail
cd "$(dirname "$0")/.."
AB="${HEDDLE:-..}"
PY="${PYTHON:-deps/venv/bin/python}"
N="${1:-5000}"; SEED="${2:-1}"
CLASSES="${3:-fp_dp1,fp_cmp,fp_dp2,fp_dp3,fp_conv,simd_3same}"
mkdir -p build
if [ "$CLASSES" = todas ]; then
  "$PY" tools/gen_cases.py build/cases.bin "$N" "$SEED"
else
  "$PY" tools/gen_cases.py build/cases.bin "$N" "$SEED" "$CLASSES"
fi
rc=0
for m in ${MOTORES:-jit interp}; do
  "$AB/target/release/heddle-difftest" build/cases.bin --engine "$m" || rc=1
done
exit $rc
