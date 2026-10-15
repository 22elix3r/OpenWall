#!/usr/bin/env python3
"""Generate original CC0 fixtures. No Wallpaper Engine assets are included."""
import json
import math
from pathlib import Path
import struct
import zlib

ROOT = Path(__file__).resolve().parents[1] / "samples"


def png(width, height, rgba):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    rows = b"".join(b"\0" + rgba[y * width * 4:(y + 1) * width * 4] for y in range(height))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))


def texture(width, height, rgba):
    # TEXV0005 / TEXI0001 / TEXB0001, raw ARGB8888 byte layout (RGBA).
    return (b"TEXV0005\0TEXI0001\0" + struct.pack("<7I", 0, 0, width, height, width, height, 0)
            + b"TEXB0001\0" + struct.pack("<4I", 1, 1, width, height)
            + struct.pack("<I", len(rgba)) + rgba)


def pixels(width, height, layer):
    result = bytearray()
    for y in range(height):
        for x in range(width):
            u, v = x / width, y / height
            if layer == 0:
                glow = math.exp(-((u - .72) ** 2 * 8 + (v - .35) ** 2 * 10))
                aurora = math.exp(-((v - .34 - .09 * math.sin(u * 7)) ** 2 * 200))
                r, g, b, a = int(13 + glow * 30), int(19 + glow * 35 + aurora * 30), int(40 + glow * 55 + aurora * 18), 255
            elif layer == 1:
                radius = math.hypot((u - .74) * width, (v - .31) * height)
                a = max(0, min(255, int((43 - radius) * 255)))
                r, g, b = 226, 232, 219
            else:
                ridge = .63 - .12 * math.sin(u * 8) - .045 * math.sin(u * 21)
                if v > ridge:
                    r, g, b, a = (17, 33, 49, 255) if v < .78 else (10, 25 + int(8 * math.sin(v * 120)), 40, 255)
                else:
                    r, g, b, a = 0, 0, 0, 0
            result.extend((r, g, b, a))
    return bytes(result)


def main():
    entries = {}
    for name, index in [("sky", 0), ("moon", 1), ("ridge", 2)]:
        rgba = pixels(1280, 720, index)
        entries[f"materials/{name}.png"] = png(1280, 720, rgba)
        entries[f"models/{name}.json"] = json.dumps({"material": f"materials/{name}.json", "width": 1280, "height": 720}).encode()
        entries[f"materials/{name}.json"] = json.dumps({"shader": "genericimage", "textures": [f"materials/{name}.png"]}).encode()
    scene = {"general": {"clearcolor": "0.025 0.035 0.06", "orthogonalprojection": {"width": 1280, "height": 720}},
             "objects": [{"id": i + 1, "name": name, "image": f"models/{name}.json", "origin": "640 360 0", "scale": "1 1 1", "angles": "0 0 0"} for i, name in enumerate(["sky", "moon", "ridge"])]}
    entries["scene.json"] = json.dumps(scene, indent=2).encode()
    root = ROOT / "aurora"
    project = {"title": "Aurora Harbor", "type": "scene", "file": "scene.json", "description": "An original OpenWall scene. CC0. No engine assets required.", "preview": "preview.png"}
    root.mkdir(parents=True, exist_ok=True)
    (root / "project.json").write_text(json.dumps(project, indent=2) + "\n")
    composed = bytearray(pixels(1280, 720, 0))
    for layer in [1, 2]:
        data = pixels(1280, 720, layer)
        for i in range(0, len(data), 4):
            a = data[i + 3] / 255
            for j in range(3):
                composed[i + j] = round(composed[i + j] * (1 - a) + data[i + j] * a)
    (root / "preview.png").write_bytes(png(1280, 720, bytes(composed)))
    for name, data in entries.items():
        file = root / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_bytes(data)
    # A tiny independent raw TEX fixture and a package exercising the real reader.
    tex = texture(2, 2, bytes([255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]))
    (ROOT / "checker.tex").write_bytes(tex)
    packed = ROOT / "aurora-packed"
    packed.mkdir(parents=True, exist_ok=True)
    (packed / "project.json").write_text(json.dumps(project, indent=2) + "\n")
    header = bytearray(struct.pack("<I", 8) + b"PKGV0001" + struct.pack("<I", len(entries)))
    payload = bytearray()
    for name, data in entries.items():
        encoded = name.encode()
        header += struct.pack("<I", len(encoded)) + encoded + struct.pack("<II", len(payload), len(data))
        payload += data
    (packed / "scene.pkg").write_bytes(header + payload)


if __name__ == "__main__":
    main()
