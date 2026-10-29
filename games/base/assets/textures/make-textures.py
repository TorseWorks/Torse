"""Draw the base game's terrain atlas.

Every pixel is generated here from a fixed seed, so the atlas is reproducible
and the repository carries no third-party art. Tiles are 16x16 and sit on a
24-pixel pitch, which leaves an 8-pixel gutter around each sprite for mipmap
extrusion. The atlas is 120x24: stone, glass, plant, missing and white tiles.
"""

from pathlib import Path
from random import Random

from PIL import Image

TILE = 16
PITCH = 24
MARGIN = 4
ATLAS = (PITCH * 5, PITCH)
HERE = Path(__file__).resolve().parent


def stone(rng: Random) -> Image.Image:
    image = Image.new("RGBA", (TILE, TILE))
    for y in range(TILE):
        for x in range(TILE):
            value = 150 + rng.randint(-22, 18)
            image.putpixel((x, y), (value, value - 2, value - 6, 255))
    for _ in range(10):
        x = rng.randrange(TILE)
        y = rng.randrange(TILE)
        image.putpixel((x, y), (104, 100, 94, 255))
    return image


def glass() -> Image.Image:
    # Mask-mode tile: the centre is fully transparent so looking through
    # glass shows the world unblended, and only the frame plus one highlight
    # streak are drawn. No alpha sorting is needed this way.
    image = Image.new("RGBA", (TILE, TILE), (0, 0, 0, 0))
    for i in range(TILE):
        image.putpixel((i, 0), (188, 226, 238, 255))
        image.putpixel((i, TILE - 1), (188, 226, 238, 255))
        image.putpixel((0, i), (188, 226, 238, 255))
        image.putpixel((TILE - 1, i), (188, 226, 238, 255))
    for i in range(3, 11):
        image.putpixel((i, 13 - i), (236, 250, 255, 255))
    return image


def plant() -> Image.Image:
    image = Image.new("RGBA", (TILE, TILE), (0, 0, 0, 0))
    blades = [(3, 15, 8), (6, 15, 14), (9, 15, 11), (12, 15, 9)]
    for base_x, base_y, height in blades:
        for step in range(height):
            y = base_y - step
            x = base_x + (1 if step > height // 2 else 0)
            shade = min(220, 110 + step * 8)
            image.putpixel((x, y), (60, shade, 48, 255))
            if step >= 3 and step % 3 == 0:
                direction = -1 if base_x < 8 else 1
                for length in range(1, 3):
                    leaf_x = max(2, min(13, x + direction * length))
                    image.putpixel((leaf_x, y + length // 2), (54, shade - 8, 43, 255))
    image.putpixel((7, 15), (74, 140, 58, 255))
    return image


def missing() -> Image.Image:
    image = Image.new("RGBA", (TILE, TILE))
    for y in range(TILE):
        for x in range(TILE):
            magenta = ((x // 2) + (y // 2)) % 2 == 0
            image.putpixel((x, y), (255, 0, 255, 255) if magenta else (24, 0, 24, 255))
    return image


def white() -> Image.Image:
    return Image.new("RGBA", (TILE, TILE), (255, 255, 255, 255))


def main() -> None:
    rng = Random(20261025)
    atlas = Image.new("RGBA", ATLAS, (0, 0, 0, 0))
    tiles = [stone(rng), glass(), plant(), missing(), white()]
    for index, tile in enumerate(tiles):
        atlas.paste(tile, (MARGIN + index * PITCH, MARGIN))
    atlas.save(HERE / "atlas.png")
    tiles[2].save(HERE.parent / "icons" / "items" / "plant.png")
    print(f"wrote {HERE / 'atlas.png'} ({ATLAS[0]}x{ATLAS[1]})")


if __name__ == "__main__":
    main()
