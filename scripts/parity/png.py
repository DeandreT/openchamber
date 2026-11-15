"""Minimal PNG reading and writing with the standard library.

Reads 8-bit grayscale, RGB and RGBA images (with or without alpha in the
palette-free colour types) of any filter type, non-interlaced, which covers
what spectacle and Chromium write. Writes 8-bit RGBA. Pixels are a flat
``bytearray`` of RGBA values, row by row.
"""

from __future__ import annotations

import struct
import zlib
from dataclasses import dataclass
from pathlib import Path

SIGNATURE = b"\x89PNG\r\n\x1a\n"
CHANNELS = {0: 1, 2: 3, 4: 2, 6: 4}


@dataclass
class Image:
    width: int
    height: int
    rgba: bytearray

    def pixel(self, x: int, y: int) -> tuple[int, int, int, int]:
        offset = (y * self.width + x) * 4
        return tuple(self.rgba[offset : offset + 4])  # type: ignore[return-value]


def read(path: Path) -> Image:
    data = path.read_bytes()
    if not data.startswith(SIGNATURE):
        raise ValueError(f"{path} is not a PNG")
    position = len(SIGNATURE)
    width = height = depth = colour = interlace = None
    compressed = bytearray()
    while position < len(data):
        (length,) = struct.unpack(">I", data[position : position + 4])
        kind = data[position + 4 : position + 8]
        body = data[position + 8 : position + 8 + length]
        position += 12 + length
        if kind == b"IHDR":
            width, height, depth, colour, _, _, interlace = struct.unpack(">IIBBBBB", body)
        elif kind == b"IDAT":
            compressed += body
        elif kind == b"IEND":
            break
    if width is None or height is None:
        raise ValueError(f"{path} has no header")
    if depth != 8 or colour not in CHANNELS or interlace != 0:
        raise ValueError(f"{path}: unsupported PNG (depth {depth}, colour {colour}, interlace {interlace})")
    channels = CHANNELS[colour]
    stride = width * channels
    raw = zlib.decompress(bytes(compressed))
    rows: list[bytearray] = []
    previous = bytearray(stride)
    offset = 0
    for _ in range(height):
        kind = raw[offset]
        line = bytearray(raw[offset + 1 : offset + 1 + stride])
        offset += 1 + stride
        unfilter(kind, line, previous, channels)
        rows.append(line)
        previous = line
    rgba = bytearray(width * height * 4)
    target = 0
    for line in rows:
        for index in range(0, stride, channels):
            if channels == 4:
                rgba[target : target + 4] = line[index : index + 4]
            elif channels == 3:
                rgba[target : target + 3] = line[index : index + 3]
                rgba[target + 3] = 255
            elif channels == 2:
                rgba[target : target + 3] = bytes((line[index],)) * 3
                rgba[target + 3] = line[index + 1]
            else:
                rgba[target : target + 3] = bytes((line[index],)) * 3
                rgba[target + 3] = 255
            target += 4
    return Image(width, height, rgba)


def unfilter(kind: int, line: bytearray, previous: bytearray, bpp: int) -> None:
    if kind == 0:
        return
    for index in range(len(line)):
        left = line[index - bpp] if index >= bpp else 0
        up = previous[index]
        if kind == 1:
            line[index] = (line[index] + left) & 0xFF
        elif kind == 2:
            line[index] = (line[index] + up) & 0xFF
        elif kind == 3:
            line[index] = (line[index] + ((left + up) >> 1)) & 0xFF
        elif kind == 4:
            upper_left = previous[index - bpp] if index >= bpp else 0
            estimate = left + up - upper_left
            distance_left = abs(estimate - left)
            distance_up = abs(estimate - up)
            distance_upper_left = abs(estimate - upper_left)
            if distance_left <= distance_up and distance_left <= distance_upper_left:
                predictor = left
            elif distance_up <= distance_upper_left:
                predictor = up
            else:
                predictor = upper_left
            line[index] = (line[index] + predictor) & 0xFF
        else:
            raise ValueError(f"unknown PNG filter {kind}")


def crop(image: Image, x: int, y: int, width: int, height: int) -> Image:
    """The ``width`` by ``height`` region whose top-left pixel is ``(x, y)``."""
    if x < 0 or y < 0 or x + width > image.width or y + height > image.height:
        raise ValueError(f"crop {x},{y} {width}x{height} leaves the {image.width}x{image.height} image")
    stride = image.width * 4
    rgba = bytearray()
    for row in range(y, y + height):
        rgba += image.rgba[row * stride + x * 4 : row * stride + (x + width) * 4]
    return Image(width, height, rgba)


def write(path: Path, image: Image) -> None:
    stride = image.width * 4
    raw = bytearray()
    for row in range(image.height):
        raw.append(0)
        raw += image.rgba[row * stride : (row + 1) * stride]

    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    header = struct.pack(">IIBBBBB", image.width, image.height, 8, 6, 0, 0, 0)
    path.write_bytes(SIGNATURE + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(bytes(raw), 6)) + chunk(b"IEND", b""))
