#!/usr/bin/env python3
"""Bibliotecas stub del NDK (aarch64) para enlazar las pruebas guest con sus DT_NEEDED reales.

Uso: mkstubs.py <src/sigs_gen.rs> <directorio de salida> <compilador...>

Como los stubs del NDK: cada biblioteca de `sigs::EXPORTS` con todos sus simbolos, con su version (DT_VERDEF; las
que no tienen versiones, sin ella) y su DT_SONAME. Los datos (`sigs::DATA`) son objetos; el resto, funciones. En
ejecucion no se usan: las bibliotecas del sistema las sirve el puente.
"""
import os
import re
import subprocess
import sys


def table(src, name):
    i = src.index("pub static %s:" % name)
    j = src.index("\n];", i)
    return src[i:j]


def main():
    src = open(sys.argv[1]).read()
    out = sys.argv[2]
    cc = sys.argv[3:]
    os.makedirs(out, exist_ok=True)
    exports = re.findall(r'\("([^"]+)", "([^"]+)", "([^"]*)"\)', table(src, "EXPORTS"))
    data = set(re.findall(r'\("([^"]+)", "[^"]+"\)', table(src, "DATA")))
    libs = {}
    for sym, lib, ver in exports:
        libs.setdefault(lib, []).append((sym, ver))
    for lib, syms in sorted(libs.items()):
        base = os.path.join(out, lib[:-3])
        with open(base + ".s", "w") as f:
            for sym, _ in syms:
                q = '"%s"' % sym
                if sym in data:
                    f.write('.data\n.globl %s\n.type %s,@object\n.size %s,8\n.balign 8\n%s:\n.quad 0\n' % (q, q, q, q))
                else:
                    f.write('.text\n.globl %s\n.type %s,@function\n%s:\nret\n' % (q, q, q))
        cmd = cc + ["-shared", "-nostdlib", "-Wl,-soname," + lib, "-o", os.path.join(out, lib), base + ".s"]
        vers = sorted({v for _, v in syms if v})
        if vers:
            with open(base + ".map", "w") as f:
                for v in vers:
                    f.write("%s {\n  global:\n" % v)
                    for sym, sv in syms:
                        if sv == v:
                            f.write('    "%s";\n' % sym)
                    f.write("};\n")
            cmd.insert(len(cc), "-Wl,--version-script=" + base + ".map")
        subprocess.check_call(cmd)


if __name__ == "__main__":
    main()
