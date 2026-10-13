#!/usr/bin/env python3
"""Generate Discord PNGs from the dashboard's canonical provider SVG symbols.

Development tool only. Requires rsvg-convert; proxy builds need no renderer.
"""
import copy
import pathlib
import subprocess
import xml.etree.ElementTree as ET


def main():
    root = pathlib.Path(__file__).resolve().parents[1]
    assets = root / "src/notifications/assets"
    source = (root / "ui/logos.svg").read_text()
    attribution = source[:source.index("-->") + 3]
    namespace = "http://www.w3.org/2000/svg"
    ET.register_namespace("", namespace)
    symbols = ET.fromstring(source)
    assets.mkdir(parents=True, exist_ok=True)
    for provider, title, color in [
        ("claude", "Claude", "#D97757"),
        ("codex", "ChatGPT / Codex", "#10A37F"),
    ]:
        symbol = next(x for x in symbols.iter() if x.attrib.get("id") == "logo-" + provider)
        svg = ET.Element("{" + namespace + "}svg", {
            "viewBox": "0 0 32 32", "width": "128", "height": "128",
            "role": "img", "aria-label": title,
        })
        group = ET.SubElement(svg, "{" + namespace + "}g", {
            "transform": "translate(4 4)", "color": color,
        })
        for child in symbol:
            group.append(copy.deepcopy(child))
        path = assets / (provider + ".svg")
        path.write_text(attribution + "\n" + ET.tostring(svg, encoding="unicode") + "\n")
        subprocess.run([
            "rsvg-convert", "-w", "128", "-h", "128", "-o",
            str(assets / (provider + ".png")), str(path),
        ], check=True)


if __name__ == "__main__":
    main()
