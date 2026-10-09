#!/usr/bin/env python3
"""APK de prueba: un ZIP con bibliotecas almacenadas sin comprimir y alineadas (como zipalign -p), para cargarlas
con la forma `ruta.apk!/lib/arm64-v8a/libx.so`. Uso: mkapk.py salida.apk alineacion entrada=archivo...
Una alineacion 0 deja la entrada sin alinear (a proposito). Una entrada con el prefijo "z:" (z:AndroidManifest.xml=...)
va comprimida con deflate, como el manifiesto de un APK real."""
import sys
import zipfile


def main():
    out, align, entries = sys.argv[1], int(sys.argv[2]), sys.argv[3:]
    with zipfile.ZipFile(out, "w", zipfile.ZIP_STORED) as z:
        for e in entries:
            name, path = e.split("=", 1)
            data = open(path, "rb").read()
            if name.startswith("z:"):
                info = zipfile.ZipInfo(name[2:], date_time=(2020, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                z.writestr(info, data)
                continue
            info = zipfile.ZipInfo(name, date_time=(2020, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_STORED
            # cabecera local: 30 bytes + nombre + extra; el relleno va en el campo extra
            start = z.fp.tell() + 30 + len(name.encode())
            if align:
                pad = (-(start + 4)) % align
                info.extra = b"\xfe\xca" + pad.to_bytes(2, "little") + b"\0" * pad
            else:
                # desalineada: desplazamiento impar
                pad = 1 if start % 2 == 0 else 0
                info.extra = b"\xfe\xca" + pad.to_bytes(2, "little") + b"\0" * pad
            z.writestr(info, data)


if __name__ == "__main__":
    main()
