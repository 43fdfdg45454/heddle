#!/usr/bin/env bash
# Instala dependencias en Debian/Ubuntu e instala Unicorn (oraculo de las pruebas diferenciales) en deps/venv.
# La version de Unicorn esta fijada: las exclusiones del arnes (tools/gen_cases.py) describen su comportamiento.
set -euo pipefail
cd "$(dirname "$0")/.."
UNICORN_VERSION="${UNICORN_VERSION:-2.1.4}"
sudo apt-get update
sudo apt-get install -y build-essential clang lld cmake ninja-build git curl python3 python3-venv python3-pip pkg-config
mkdir -p deps
python3 -m venv deps/venv
deps/venv/bin/pip install "unicorn==$UNICORN_VERSION"
echo "Listo. Unicorn $UNICORN_VERSION en deps/venv"
