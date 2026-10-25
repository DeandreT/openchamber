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


class PaintNoiseTest(unittest.TestCase):
    def difference(self, delta, regions, protected=None, text=None):
        left = png.Image(WIDTH, HEIGHT, bytearray((*BACKGROUND, 255)) * (WIDTH * HEIGHT))
        right = png.Image(WIDTH, HEIGHT, left.rgba.copy())
        offset = (5 * WIDTH + 5) * 4
        for channel in range(3):
            right.rgba[offset + channel] += delta
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            png.write(root / "left.png", left)
            png.write(root / "right.png", right)
            return compare.compare_images(root / "left.png", root / "right.png", root / "out",
                text_boxes=text, paint_regions=regions, protected_boxes=protected)

    def test_only_one_or_two_levels_inside_the_region_are_noise(self):
        for delta in (1, 2):
            result = self.difference(delta, [[0, 0, 10, 10]])
            self.assertEqual(result.differing_pixels, 0)
            self.assertEqual(result.paint_noise_pixels, 1)
        result = self.difference(3, [[0, 0, 10, 10]])
        self.assertEqual(result.differing_pixels, 1)
        self.assertEqual(result.paint_noise_pixels, 0)

    def test_pixels_outside_the_region_remain_strict(self):
        result = self.difference(1, [[6, 6, 10, 10]])
        self.assertEqual(result.differing_pixels, 1)
        self.assertEqual(result.paint_noise_pixels, 0)

    def test_text_and_controls_are_excluded(self):
        for protected, text in (([[4, 4, 2, 2]], None), (None, [[4, 4, 2, 2]])):
            result = self.difference(1, [[0, 0, 10, 10]], protected, text)
            baseline = self.difference(1, [], protected, text)
            self.assertEqual(result.differing_pixels, baseline.differing_pixels)
            self.assertEqual(result.glyph_edge_pixels, baseline.glyph_edge_pixels)
            self.assertEqual(result.paint_noise_pixels, 0)


class HiddenGeometryTest(unittest.TestCase):
    def test_hidden_targets_match_absent_native_nodes(self):
        for hidden in (None, [50, 20, 200, 0], [50, 20, 0, 200]):
            self.assertEqual(compare.compare_geometry({"panel": hidden}, {"panel": None}, 1), [])

    def test_visible_missing_targets_still_fail(self):
        visible = [50, 20, 200, 100]
        self.assertEqual(len(compare.compare_geometry({"panel": visible}, {"panel": None}, 1)), 1)
        self.assertEqual(len(compare.compare_geometry({"panel": None}, {"panel": visible}, 1)), 1)


if __name__ == "__main__":
    unittest.main()
