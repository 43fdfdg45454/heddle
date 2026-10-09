#!/usr/bin/env python3
"""AndroidManifest.xml binario (ResXMLTree, como lo compila aapt2) para las pruebas del modo de compatibilidad de
16 KiB: <manifest package="..."><application .../></manifest> con los atributos android: pedidos.

Uso: mkaxml.py salida [--utf8] [--sin-mapa] [atributo=valor...]
  atributo: pageSizeCompat (entero: 32 enabled, 64 disabled) o extractNativeLibs (true/false).
  --utf8: pool de cadenas UTF-8 (por defecto UTF-16, como aapt2).
  --sin-mapa: sin mapa de recursos (los atributos no tienen identificador: Android no los reconoce).
"""
import struct
import sys

ANDROID_NS = "http://schemas.android.com/apk/res/android"
IDS = {"pageSizeCompat": 0x010106AB, "extractNativeLibs": 0x010104EA}
TYPE_INT_DEC, TYPE_INT_BOOLEAN = 0x10, 0x12


def string_pool(strings, utf8):
    data = b""
    offs = []
    for s in strings:
        offs.append(len(data))
        if utf8:
            b = s.encode()
            assert len(s) < 128 and len(b) < 128
            data += bytes([len(s), len(b)]) + b + b"\0"
        else:
            data += struct.pack("<H", len(s)) + s.encode("utf-16-le") + b"\0\0"
    data += b"\0" * (-len(data) % 4)
    hdr_size = 28
    start = hdr_size + 4 * len(strings)
    body = b"".join(struct.pack("<I", o) for o in offs) + data
    flags = 0x100 if utf8 else 0
    return struct.pack("<HHIIIIII", 0x0001, hdr_size, hdr_size + len(body), len(strings), 0, flags, start, 0) + body


def node(typ, ext):
    return struct.pack("<HHIII", typ, 16, 16 + len(ext), 1, 0xFFFFFFFF) + ext


def main():
    args = sys.argv[1:]
    out = args.pop(0)
    utf8 = "--utf8" in args
    resmap = "--sin-mapa" not in args
    attrs = [a.split("=", 1) for a in args if not a.startswith("--")]
    # los nombres de atributos con identificador van primero (el mapa de recursos indexa el pool)
    strings = [n for n, _ in attrs] + ["package", "android", ANDROID_NS, "manifest", "application", "com.example.t10"]
    idx = {s: i for i, s in enumerate(strings)}
    chunks = string_pool(strings, utf8)
    if resmap:
        ids = b"".join(struct.pack("<I", IDS[n]) for n, _ in attrs)
        chunks += struct.pack("<HHI", 0x0180, 8, 8 + len(ids)) + ids
    chunks += node(0x0100, struct.pack("<II", idx["android"], idx[ANDROID_NS]))
    # <manifest package="com.example.t10">
    pkg = struct.pack("<IIIHBBI", 0xFFFFFFFF, idx["package"], idx["com.example.t10"], 8, 0, 0x03, idx["com.example.t10"])
    chunks += node(0x0102, struct.pack("<IIHHHHHH", 0xFFFFFFFF, idx["manifest"], 20, 20, 1, 0, 0, 0) + pkg)
    al = b""
    for n, v in attrs:
        if v in ("true", "false"):
            t, d = TYPE_INT_BOOLEAN, 0xFFFFFFFF if v == "true" else 0
        else:
            t, d = TYPE_INT_DEC, int(v, 0)
        al += struct.pack("<IIIHBBI", idx[ANDROID_NS], idx[n], 0xFFFFFFFF, 8, 0, t, d)
    chunks += node(0x0102, struct.pack("<IIHHHHHH", 0xFFFFFFFF, idx["application"], 20, 20, len(attrs), 0, 0, 0) + al)
    chunks += node(0x0103, struct.pack("<II", 0xFFFFFFFF, idx["application"]))
    chunks += node(0x0103, struct.pack("<II", 0xFFFFFFFF, idx["manifest"]))
    chunks += node(0x0101, struct.pack("<II", idx["android"], idx[ANDROID_NS]))
    with open(out, "wb") as f:
        f.write(struct.pack("<HHI", 0x0003, 8, 8 + len(chunks)) + chunks)


if __name__ == "__main__":
    main()
