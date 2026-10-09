#!/usr/bin/env python3
"""Extracto minimo del registro de Vulkan (vk.xml) para sigtool: solo lo que lee `load_vk_xml`.

Uso: vkextract.py <vk.xml completo> <salida>

Se conserva, de la API "vulkan" (no Vulkan SC):
  * VK_HEADER_VERSION;
  * cada <type category="struct|union">: name, alias, structextends y, de sus <member> con `len`, el nombre y `len`;
  * cada <command>: name, alias y, de sus <param>, el nombre y `len`.
El resultado (vk_extracto.xml) se versiona junto a sigtool, que lo lee por defecto.
"""
import sys
import xml.etree.ElementTree as ET
from xml.sax.saxutils import escape


def quoteattr(v):
    return '"' + escape(v, {'"': "&quot;"}) + '"'


def vulkan(el):
    api = el.get("api")
    return api is None or "vulkan" in api.split(",")


def main():
    src, out = sys.argv[1], sys.argv[2]
    tree = ET.parse(src)
    root = tree.getroot()
    lines = []
    copyright_text = root.findtext("comment") or ""
    lines.append('<?xml version="1.0" encoding="UTF-8"?>')
    lines.append("<registry>")
    lines.append("<comment>")
    lines.append(escape(copyright_text.strip()))
    lines.append("")
    lines.append("Extracto generado por tools/sigtool/vkextract.py a partir de vk.xml (API vulkan): VK_HEADER_VERSION,")
    lines.append("las estructuras y uniones (name, alias, structextends y los campos con len) y los comandos (name,")
    lines.append("alias y sus parametros con su len).")
    lines.append("</comment>")
    lines.append("<types>")
    version = None
    for t in root.iter("type"):
        if t.get("category") == "define" and t.findtext("name") == "VK_HEADER_VERSION" and vulkan(t):
            tail = (t.find("name").tail or "").strip()
            version = tail.split()[0]
    if version is None:
        sys.exit("vk.xml sin VK_HEADER_VERSION")
    lines.append('<type category="define">#define <name>VK_HEADER_VERSION</name> %s</type>' % version)
    types = root.find("types")
    for t in types.findall("type"):
        cat = t.get("category")
        if cat not in ("struct", "union") or not vulkan(t):
            continue
        attrs = " ".join("%s=%s" % (k, quoteattr(t.get(k))) for k in ("category", "name", "alias", "structextends") if t.get(k) is not None)
        if t.get("alias") is not None:
            lines.append("<type %s/>" % attrs)
            continue
        lines.append("<type %s>" % attrs)
        for m in t.findall("member"):
            if not vulkan(m):
                continue
            if m.get("len") is None:
                continue  # sigtool solo usa los campos con `len`
            n = m.findtext("name")
            ma = " ".join("%s=%s" % (k, quoteattr(m.get(k))) for k in ("len",) if m.get(k) is not None)
            lines.append("  <member%s><name>%s</name></member>" % ((" " + ma) if ma else "", escape(n or "")))
        lines.append("</type>")
    lines.append("</types>")
    lines.append("<commands>")
    for c in root.find("commands").findall("command"):
        if not vulkan(c):
            continue
        if c.get("alias") is not None:
            lines.append("<command name=%s alias=%s/>" % (quoteattr(c.get("name")), quoteattr(c.get("alias"))))
            continue
        proto = c.find("proto")
        if proto is None:
            continue
        lines.append("<command>")
        lines.append("  <proto><name>%s</name></proto>" % escape(proto.findtext("name")))
        for p in c.findall("param"):
            if not vulkan(p):
                continue
            pa = " ".join("%s=%s" % (k, quoteattr(p.get(k))) for k in ("len",) if p.get(k) is not None)
            lines.append("  <param%s><name>%s</name></param>" % ((" " + pa) if pa else "", escape(p.findtext("name") or "")))
        lines.append("</command>")
    lines.append("</commands>")
    lines.append("</registry>")
    with open(out, "w") as f:
        f.write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
