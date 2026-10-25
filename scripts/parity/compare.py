"""Screenshot and geometry comparison for parity checkpoints.

Raw pixel equality is a detector, not the verdict: a reported region still
needs review, and tolerances apply only to verified rasterization noise
(see docs/rust-desktop/PARITY.md). Nothing here blurs, rescales or masks.

Glyph-edge noise is the one approved kind (`exceptions.json`): Chromium puts
glyphs on whole pixels and GPUI at sub-pixel phases, and their anti-aliasing
differs, so the same text differs at its edges. A differing pixel counts as
that noise only inside a text box of the reference and only when each image's
colour lies, channel by channel, within the range the other image holds in
the pixel's 3x3 neighbourhood (give or take `GLYPH_SLACK`): the same ink
within a pixel, as the one-physical-pixel geometry rule allows. A missing,
recoloured or further moved glyph leaves colours the other image has nowhere
near, which still count.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import png

# Mismatches are grouped into cells of this many pixels for reporting.
CELL = 16
# Channel levels a glyph-edge colour may sit outside the neighbourhood's range.
GLYPH_SLACK = 8


@dataclass
class ImageDifference:
    """The result of comparing a reference and a candidate image."""

    width: int
    height: int
    size_matches: bool
    differing_pixels: int = 0
    regions: list[dict] = field(default_factory=list)
    # Differences classified as approved glyph-edge noise; never counted in
    # `differing_pixels`, always reported.
    glyph_edge_pixels: int = 0

    @property
    def identical(self) -> bool:
        return self.size_matches and self.differing_pixels == 0

    def as_dict(self) -> dict:
        return {
            "width": self.width,
            "height": self.height,
            "sizeMatches": self.size_matches,
            "differingPixels": self.differing_pixels,
            "differingShare": round(self.differing_pixels / max(1, self.width * self.height), 6),
            "glyphEdgePixels": self.glyph_edge_pixels,
            "regions": self.regions,
        }


def compare_images(
    reference: Path,
    candidate: Path,
    output: Path,
    tolerance: int = 0,
    text_boxes: list[list[float]] | None = None,
) -> ImageDifference:
    """Writes ``<output>-diff.png`` and ``<output>-overlay.png`` and reports
    the pixels whose largest channel difference exceeds ``tolerance``.

    With ``text_boxes`` (the reference's text, ``[x, y, width, height]`` in
    image pixels), differences that are glyph-edge noise inside them are
    counted apart and drawn yellow in the diff image."""
    left = png.read(reference)
    right = png.read(candidate)
    if (left.width, left.height) != (right.width, right.height):
        return ImageDifference(
            width=right.width,
            height=right.height,
            size_matches=False,
            regions=[{"reason": f"reference {left.width}x{left.height}, candidate {right.width}x{right.height}"}],
        )
    width, height = left.width, left.height
    in_text = text_mask(text_boxes or [], width, height)
    noise = 0
    diff = bytearray(width * height * 4)
    overlay = bytearray(width * height * 4)
    cells: dict[tuple[int, int], int] = {}
    differing = 0
    a, b = left.rgba, right.rgba
    for offset in range(0, width * height * 4, 4):
        delta = max(abs(a[offset] - b[offset]), abs(a[offset + 1] - b[offset + 1]), abs(a[offset + 2] - b[offset + 2]))
        for channel in range(3):
            overlay[offset + channel] = (a[offset + channel] + b[offset + channel]) >> 1
        overlay[offset + 3] = 255
        pixel = offset // 4
        if delta > tolerance and in_text[pixel] and (
            within_neighbourhood(left.rgba, right.rgba, pixel % width, pixel // width, width, height)
            and within_neighbourhood(right.rgba, left.rgba, pixel % width, pixel // width, width, height)
        ):
            noise += 1
            diff[offset : offset + 4] = b"\xff\xd0\x00\xff"
        elif delta > tolerance:
            differing += 1
            diff[offset : offset + 4] = b"\xff\x00\x00\xff"
            cell = (pixel % width // CELL, pixel // width // CELL)
            cells[cell] = cells.get(cell, 0) + 1
        else:
            gray = (a[offset] + a[offset + 1] + a[offset + 2]) // 3
            faded = 255 - ((255 - gray) >> 2)
            diff[offset : offset + 4] = bytes((faded, faded, faded, 255))
    png.write(output.with_name(output.name + "-diff.png"), png.Image(width, height, diff))
    png.write(output.with_name(output.name + "-overlay.png"), png.Image(width, height, overlay))
    return ImageDifference(width, height, True, differing, merge_cells(cells), noise)


def text_mask(boxes: list[list[float]], width: int, height: int) -> bytearray:
    """One byte per pixel, set inside a text box widened by a pixel: an
    anti-aliased edge can reach just past the box the layout reports."""
    mask = bytearray(width * height)
    for x, y, box_width, box_height in boxes:
        left = max(0, int(x) - 1)
        top = max(0, int(y) - 1)
        right = min(width, int(x + box_width + 0.999) + 1)
        bottom = min(height, int(y + box_height + 0.999) + 1)
        for row in range(top, bottom):
            start = row * width
            mask[start + left : start + right] = b"\x01" * max(0, right - left)
    return mask


def within_neighbourhood(image: bytearray, other: bytearray, x: int, y: int, width: int, height: int) -> bool:
    """Whether every channel of ``image`` at the pixel lies within the range
    ``other`` holds in the pixel's 3x3 neighbourhood, give or take
    ``GLYPH_SLACK``."""
    offset = (y * width + x) * 4
    neighbours = [
        (ny * width + nx) * 4
        for ny in range(max(0, y - 1), min(height, y + 2))
        for nx in range(max(0, x - 1), min(width, x + 2))
    ]
    for channel in range(3):
        values = [other[neighbour + channel] for neighbour in neighbours]
        if not min(values) - GLYPH_SLACK <= image[offset + channel] <= max(values) + GLYPH_SLACK:
            return False
    return True

def merge_cells(cells: dict[tuple[int, int], int]) -> list[dict]:
    """Joins touching mismatched cells into rectangles, largest first."""
    remaining = set(cells)
    regions = []
    while remaining:
        start = remaining.pop()
        stack, members = [start], [start]
        while stack:
            x, y = stack.pop()
            for neighbour in ((x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)):
                if neighbour in remaining:
                    remaining.remove(neighbour)
                    stack.append(neighbour)
                    members.append(neighbour)
        xs = [x for x, _ in members]
        ys = [y for _, y in members]
        regions.append({
            "x": min(xs) * CELL,
            "y": min(ys) * CELL,
            "width": (max(xs) - min(xs) + 1) * CELL,
            "height": (max(ys) - min(ys) + 1) * CELL,
            "pixels": sum(cells[member] for member in members),
        })
    regions.sort(key=lambda region: region["pixels"], reverse=True)
    return regions


def compare_geometry(reference: dict[str, list[float]], candidate: dict[str, list[float]], scale: float) -> list[dict]:
    """Targets whose logical rectangles differ by more than one physical pixel.

    A visible target present in only one client is reported too. A zero-sized
    DOM node and an absent native node both describe a hidden target.
    """
    allowance = 1.0 / scale
    differences = []
    for target in sorted(set(reference) | set(candidate)):
        left, right = reference.get(target), candidate.get(target)
        left = left if left is not None and left[2] > 0 and left[3] > 0 else None
        right = right if right is not None and right[2] > 0 and right[3] > 0 else None
        if left is None and right is None:
            continue
        if left is None or right is None:
            differences.append({"target": target, "reference": left, "candidate": right, "reason": "missing"})
            continue
        if any(abs(l - r) > allowance for l, r in zip(left, right)):
            differences.append({"target": target, "reference": left, "candidate": right, "reason": "moved"})
    return differences
