"""Generate original compact UI skins using only the standard library."""
from pathlib import Path
import struct
import zlib

ROOT = Path(__file__).resolve().parent
PARTS = [
    ("window", (24, 26, 30, 244), (86, 92, 102, 255)),
    ("group", (29, 31, 36, 255), (58, 62, 70, 255)),
    ("button", (44, 47, 53, 255), (66, 71, 80, 255)),
    ("button_hover", (56, 60, 68, 255), (104, 111, 122, 255)),
    ("button_pressed", (38, 41, 46, 255), (96, 148, 210, 255)),
    ("field", (20, 21, 24, 255), (86, 92, 102, 255)),
    ("slot", (30, 32, 36, 255), (58, 62, 70, 255)),
    ("slot_selected", (30, 32, 36, 255), (96, 148, 210, 255)),
    ("track", (36, 39, 44, 255), (58, 62, 70, 255)),
    ("thumb", (66, 71, 80, 255), (104, 111, 122, 255)),
    ("thumb_hover", (96, 148, 210, 255), (174, 203, 238, 255)),
    ("chip", (22, 24, 28, 208), (70, 75, 84, 190)),
    ("progress", (96, 148, 210, 255), (96, 148, 210, 255)),
    ("check", (20, 21, 24, 255), (86, 92, 102, 255)),
    ("check_active", (96, 148, 210, 255), (174, 203, 238, 255)),
    ("close", (0, 0, 0, 0), (148, 155, 163, 255)),
    ("close_hover", (0, 0, 0, 0), (242, 244, 246, 255)),
    ("crosshair", (0, 0, 0, 0), (235, 237, 240, 220)),
]
WIDTH, HEIGHT = 160, 32
def chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

def build(stem, parts):
    pixels = bytearray(WIDTH * HEIGHT * 4)
    manifest = [f"width = {WIDTH}", f"height = {HEIGHT}", ""]
    for index, (name, fill, edge) in enumerate(parts):
        ox, oy = (index % 10) * 16, (index // 10) * 16
        glyph = name in ("close", "close_hover", "crosshair")
        for y in range(16):
            for x in range(16):
                color = edge if x in (0, 15) or y in (0, 15) else fill
                if glyph:
                    ink = (x == y or x == 15 - y) and 4 <= x <= 11
                    if name == "crosshair":
                        ink = (x == 7 and 3 <= y <= 11) or (y == 7 and 3 <= x <= 11)
                    color = edge if ink else fill
                offset = ((oy + y) * WIDTH + ox + x) * 4
                pixels[offset:offset + 4] = bytes(color)
        manifest.extend(["[[regions]]", f'part = "{name}"', f"x = {ox}", f"y = {oy}",
                         "width = 16", "height = 16", f"border = {0 if glyph else 1}", ""])
    raw = b"".join(b"\0" + pixels[y * WIDTH * 4:(y + 1) * WIDTH * 4] for y in range(HEIGHT))
    png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", WIDTH, HEIGHT, 8, 6, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")
    (ROOT / f"{stem}.png").write_bytes(png)
    (ROOT / f"{stem}.toml").write_text("\n".join(manifest), encoding="utf-8")

BLUE = {
    "window": ((18, 19, 21, 255), (62, 66, 73, 255)),
    "group": ((25, 26, 28, 255), (54, 58, 65, 255)),
    "button": ((34, 65, 103, 255), (42, 75, 116, 255)),
    "button_hover": ((43, 86, 139, 255), (65, 119, 182, 255)),
    "button_pressed": ((48, 98, 160, 255), (85, 141, 205, 255)),
    "field": ((29, 47, 72, 255), (43, 63, 90, 255)),
    "slot": ((24, 32, 44, 255), (57, 66, 81, 255)),
    "slot_selected": ((30, 56, 87, 255), (77, 137, 207, 255)),
    "track": ((29, 33, 40, 255), (54, 62, 74, 255)),
    "thumb": ((49, 97, 157, 255), (77, 129, 191, 255)),
    "thumb_hover": ((64, 121, 190, 255), (125, 170, 220, 255)),
    "chip": ((19, 23, 29, 226), (51, 70, 94, 230)),
    "progress": ((43, 87, 142, 255), (61, 113, 176, 255)),
    "check": ((28, 44, 65, 255), (53, 76, 107, 255)),
    "check_active": ((52, 116, 194, 255), (111, 165, 225, 255)),
}
build("default", PARTS)
build("blue", [(name, *BLUE.get(name, (fill, edge))) for name, fill, edge in PARTS]
      + [("title", (38, 74, 119, 255), (52, 91, 141, 255))])
