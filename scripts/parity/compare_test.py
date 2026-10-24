"""Checks that the glyph-edge exception forgives only edge noise.

Run with `python3 -m unittest scripts/parity/compare_test.py`.
"""

import tempfile
import unittest
from pathlib import Path

import compare
import png

WIDTH, HEIGHT = 40, 20
BACKGROUND = (20, 20, 20)
INK = (230, 230, 230)


def glyphs(edge: int, shift: int = 0, ink: tuple[int, int, int] = INK, missing: bool = False) -> png.Image:
    """Two solid 4x8 strokes in `ink` with a one-pixel anti-aliased edge of
    brightness `edge` on their left, moved right by `shift`; the second is
    left out when `missing`."""
    rgba = bytearray()
    for y in range(HEIGHT):
        for x in range(WIDTH):
            colour = BACKGROUND
            for left in (10, 24):
                if missing and left == 24:
                    continue
                start = left + shift
                if 6 <= y < 14 and start <= x < start + 4:
                    colour = ink
                elif 6 <= y < 14 and x == start - 1:
                    colour = (edge, edge, edge)
            rgba += bytes((*colour, 255))
    return png.Image(WIDTH, HEIGHT, rgba)


TEXT = [[8.0, 5.0, 24.0, 10.0]]


class GlyphEdgeTest(unittest.TestCase):
    def compare(self, reference: png.Image, candidate: png.Image, boxes=TEXT) -> compare.ImageDifference:
        with tempfile.TemporaryDirectory() as directory:
            left, right = Path(directory) / "left.png", Path(directory) / "right.png"
            png.write(left, reference)
            png.write(right, candidate)
            return compare.compare_images(left, right, Path(directory) / "out", text_boxes=boxes)

    def test_a_different_edge_phase_inside_text_is_noise(self) -> None:
        result = self.compare(glyphs(edge=90), glyphs(edge=140))
        self.assertEqual(result.differing_pixels, 0)
        self.assertGreater(result.glyph_edge_pixels, 0)

    def test_the_same_difference_outside_text_still_counts(self) -> None:
        result = self.compare(glyphs(edge=90), glyphs(edge=140), boxes=[])
        self.assertGreater(result.differing_pixels, 0)
        self.assertEqual(result.glyph_edge_pixels, 0)

    def test_a_glyph_a_pixel_away_is_noise(self) -> None:
        # Geometry allows one physical pixel; a glyph's phase is no stricter.
        result = self.compare(glyphs(edge=90), glyphs(edge=140, shift=1))
        self.assertEqual(result.differing_pixels, 0)
        self.assertGreater(result.glyph_edge_pixels, 0)

    def test_a_glyph_moved_by_two_pixels_counts(self) -> None:
        self.assertGreater(self.compare(glyphs(edge=90), glyphs(edge=90, shift=2)).differing_pixels, 0)

    def test_a_missing_glyph_counts(self) -> None:
        self.assertGreater(self.compare(glyphs(edge=90), glyphs(edge=90, missing=True)).differing_pixels, 0)

    def test_a_recoloured_glyph_counts(self) -> None:
        recoloured = glyphs(edge=90, ink=(200, 120, 60))
        self.assertGreater(self.compare(glyphs(edge=90), recoloured).differing_pixels, 0)

    def test_a_dimmer_glyph_counts(self) -> None:
        dimmer = glyphs(edge=90, ink=(200, 200, 200))
        self.assertGreater(self.compare(glyphs(edge=90), dimmer).differing_pixels, 0)


if __name__ == "__main__":
    unittest.main()
