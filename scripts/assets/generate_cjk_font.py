#!/usr/bin/env python3
"""Generate the embedded GB2312 CJK bitmap fonts for the firmware.

Produces three binary blobs into `--output-dir`:
  hzk16.bin    - 16x16 1bpp glyphs, HZK16 layout (94 rows x 94 cols, 32
                 bytes/cell = 16 rows x 2 bytes, MSB first, bit 1 = ink).
                 Cell (q, w) for GB2312 bytes 0xA1+q / 0xA1+w sits at
                 offset ((q*94)+w)*32.
  hzk12.bin    - 12x12 1bpp glyphs, same 94x94 grid, 24 bytes/cell.
  cjk_index.bin - Sorted (u16 codepoint, u16 cell_index) little-endian
                 pairs for every encodable GB2312 character, for binary
                 search at render time (Unicode char -> font cell).

The ASCII range keeps using the existing proportional 8x16 / 5x7 fonts;
this covers GB2312-encodable CJK + punctuation only.
"""

import argparse
import struct
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

GB_HIGH_START = 0xA1
GRID = 94  # GB2312 uses a 94x94 cell grid


def gb2312_cells():
    """Yield (q, w, char) for every defined GB2312 cell."""
    cells = []
    for q in range(GRID):
        for w in range(GRID):
            hi = GB_HIGH_START + q
            lo = GB_HIGH_START + w
            if hi == 0xFF or lo == 0xFF:
                continue
            try:
                char = bytes([hi, lo]).decode("gb2312")
            except UnicodeDecodeError:
                continue
            cells.append((q, w, char))
    return cells


def rasterize(font, char, cell: int) -> bytes:
    """Render `char` into a `cell`x`cell` 1bpp bitmap, centered.

    Renders at a slightly larger canvas, crops the ink bounding box and
    centers it in the cell so glyphs sit on a consistent visual grid.
    """
    pad = 4
    canvas = cell + pad * 2
    image = Image.new("1", (canvas, canvas), 0)
    draw = ImageDraw.Draw(image)
    draw.text((pad, pad), char, font=font, fill=1)
    bbox = image.getbbox()
    if bbox is None:
        return bytes(cell * ((cell + 7) // 8))
    ink = image.crop(bbox)
    target = cell - 2  # leave a 1px breathing border inside the cell
    if ink.width > target or ink.height > target:
        ratio = target / max(ink.width, ink.height)
        ink = ink.resize(
            (max(1, round(ink.width * ratio)), max(1, round(ink.height * ratio))),
            Image.LANCZOS,
        )
    out = Image.new("1", (cell, cell), 0)
    out.paste(ink, ((cell - ink.width) // 2, (cell - ink.height) // 2))
    rows = []
    row_bytes = (cell + 7) // 8
    for y in range(cell):
        value = 0
        for x in range(cell):
            value = (value << 1) | int(out.getpixel((x, y)) != 0)
        rows.append(value.to_bytes(row_bytes, "big"))
    return b"".join(rows)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--font", required=True, help="CJK TTF/OTF (e.g. Noto Sans SC)")
    parser.add_argument("--output-dir", required=True)
    args = parser.parse_args()

    font = ImageFont.truetype(args.font, 16)
    cells = gb2312_cells()
    print(f"{len(cells)} GB2312 characters")

    font16 = bytearray(GRID * GRID * 32)
    font12 = bytearray(GRID * GRID * 24)
    index = []
    for q, w, char in cells:
        cell_index = q * GRID + w
        font16[cell_index * 32 : cell_index * 32 + 32] = rasterize(font, char, 16)
        font12[cell_index * 24 : cell_index * 24 + 24] = rasterize(font, char, 12)
        index.append((ord(char), cell_index))
    index.sort()

    out = Path(args.output_dir)
    out.mkdir(parents=True, exist_ok=True)
    (out / "hzk16.bin").write_bytes(bytes(font16))
    (out / "hzk12.bin").write_bytes(bytes(font12))
    (out / "cjk_index.bin").write_bytes(
        b"".join(struct.pack("<HH", cp, cell) for cp, cell in index)
    )
    print(
        f"wrote hzk16.bin ({len(font16)} B), hzk12.bin ({len(font12)} B), "
        f"cjk_index.bin ({len(index) * 4} B) to {out}"
    )


if __name__ == "__main__":
    main()