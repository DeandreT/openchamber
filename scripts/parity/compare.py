"""Screenshot and geometry comparison for parity checkpoints.

Raw pixel equality is a detector, not the verdict: a reported region still
needs review, and tolerances apply only to verified rasterization noise
(see docs/rust-desktop/PARITY.md). Nothing here blurs, rescales or masks.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

import png

# Mismatches are grouped into cells of this many pixels for reporting.
CELL = 16


@dataclass
class ImageDifference:
    """The result of comparing a reference and a candidate image."""

    width: int
    height: int
    size_matches: bool
    differing_pixels: int = 0
    regions: list[dict] = field(default_factory=list)

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
            "regions": self.regions,
        }


def compare_images(reference: Path, candidate: Path, output: Path, tolerance: int = 0) -> ImageDifference:
    """Writes ``<output>-diff.png`` and ``<output>-overlay.png`` and reports
    the pixels whose largest channel difference exceeds ``tolerance``."""
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
        if delta > tolerance:
            differing += 1
            diff[offset : offset + 4] = b"\xff\x00\x00\xff"
            pixel = offset // 4
            cell = (pixel % width // CELL, pixel // width // CELL)
            cells[cell] = cells.get(cell, 0) + 1
        else:
            gray = (a[offset] + a[offset + 1] + a[offset + 2]) // 3
            faded = 255 - ((255 - gray) >> 2)
            diff[offset : offset + 4] = bytes((faded, faded, faded, 255))
    png.write(output.with_name(output.name + "-diff.png"), png.Image(width, height, diff))
    png.write(output.with_name(output.name + "-overlay.png"), png.Image(width, height, overlay))
    return ImageDifference(width, height, True, differing, merge_cells(cells))


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

    A target present in only one client is reported too: a missing control is
    a difference, never a skip.
    """
    allowance = 1.0 / scale
    differences = []
    for target in sorted(set(reference) | set(candidate)):
        left, right = reference.get(target), candidate.get(target)
        if left is None or right is None:
            differences.append({"target": target, "reference": left, "candidate": right, "reason": "missing"})
            continue
        if any(abs(l - r) > allowance for l, r in zip(left, right)):
            differences.append({"target": target, "reference": left, "candidate": right, "reason": "moved"})
    return differences
