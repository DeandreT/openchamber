"""Pure evidence/guard tests. These do not stand in for real private-window proof."""

from pathlib import Path
import tempfile
import unittest

import native_terminal_appearance as appearance
import png


def image(background, ink=(225, 225, 225), shift=0):
    result = png.Image(180, 140, bytearray([*background, 255]) * (180 * 140))
    # A stationary, text-like region with enough ink to exercise the minimum.
    for y in range(16, 64):
        for x in range(16 + shift, 116 + shift):
            if (x - shift) % 6 < 2 and y % 8 < 5:
                start = (y * result.width + x) * 4
                result.rgba[start:start + 4] = bytes([*ink, 255])
    return result


class GridGeometryTests(unittest.TestCase):
    def test_crop_uses_physical_content_coordinates_and_excludes_chrome(self):
        rect = [280, 520, 1000, 260]
        self.assertEqual(appearance.grid_crop(rect, 1.25, (1600, 1000)), [365, 665, 1205, 280])
        self.assertEqual(appearance.grid_crop(rect, 1.25, (1600, 1000), glyphs=True), [365, 710, 1205, 235])

    def test_rounding_stays_inside_grid(self):
        bounds = appearance.grid_crop([10.2, 20.2, 300.3, 180.3], 1.25, (800, 600))
        self.assertEqual(bounds, [28, 41, 330, 179])

    def test_missing_clipped_nonfinite_or_tiny_grid_is_refused(self):
        for rect, scale in [([0, 0, 0, 0], 1), ([-1, 20, 300, 200], 1),
                            ([10, 20, 300, 100], 1), ([500, 500, 300, 200], 1),
                            ([10, 20, 300, 200], 0), ([10, float('nan'), 300, 200], 1),
                            ([10, 20, 300, 200], float('inf'))]:
            with self.subTest(rect=rect, scale=scale), self.assertRaises(ValueError):
                appearance.grid_crop(rect, scale, (640, 600))

    def test_cropped_grid_ignores_changed_sidebar_and_panel_chrome(self):
        first = png.Image(400, 300, bytearray([20, 20, 20, 255]) * (400 * 300))
        second = png.Image(400, 300, bytearray([240, 240, 240, 255]) * (400 * 300))
        # The whole terminal surface, including its cached grid, remains dark.
        for source in (first, second):
            for y in range(100, 300):
                start = (y * source.width + 100) * 4
                source.rgba[start:start + 300 * 4] = bytes([20, 20, 20, 255]) * 300
        crop = appearance.grid_crop([100, 100, 300, 200], 1, (400, 300))
        self.assertEqual(png.crop(first, *crop).rgba, png.crop(second, *crop).rgba)


class PaintInterpretationTests(unittest.TestCase):
    def test_theme_requires_actual_grid_repaint_and_restore(self):
        dark = appearance.paint_metrics(image((20, 20, 20)))
        light = appearance.paint_metrics(image((240, 240, 240), ink=(30, 30, 30)))
        appearance.verify_theme(dark, light, dark)
        with self.assertRaisesRegex(RuntimeError, 'did not repaint'):
            appearance.verify_theme(dark, dark, dark)
        with self.assertRaisesRegex(RuntimeError, 'did not restore'):
            appearance.verify_theme(dark, light, light)

    def test_empty_grid_or_overlay_cannot_establish_theme_success(self):
        dark = appearance.paint_metrics(image((20, 20, 20)))
        empty = appearance.paint_metrics(png.Image(180, 140, bytearray([240, 240, 240, 255]) * (180 * 140)))
        with self.assertRaisesRegex(RuntimeError, 'visible glyphs'):
            appearance.verify_theme(dark, empty, dark)
        mixed = dict(dark, backgroundShare=0.4)
        with self.assertRaisesRegex(RuntimeError, 'dominant grid background'):
            appearance.verify_theme(mixed, empty, dark)

    def test_font_change_requires_changed_ink_with_unchanged_theme(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            path = Path(root) / 'diff.png'
            before = image((20, 20, 20))
            changed = image((20, 20, 20), shift=2)
            comparison = appearance.compare_paint(before, changed, path)
            appearance.verify_font(comparison)
            self.assertGreater(comparison['inkChangedPixels'], 200)
            self.assertTrue(path.is_file())
            self.assertNotEqual(comparison['before']['inkBounds'], comparison['after']['inkBounds'])
            unchanged = appearance.compare_paint(before, before, path)
            with self.assertRaisesRegex(RuntimeError, 'did not change'):
                appearance.verify_font(unchanged)

    def test_palette_change_alone_cannot_establish_font_success(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            comparison = appearance.compare_paint(image((20, 20, 20)), image((240, 240, 240)), Path(root) / 'diff.png')
            with self.assertRaisesRegex(RuntimeError, 'also changed.*theme'):
                appearance.verify_font(comparison)

    def test_lost_output_cannot_establish_font_success(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            empty = png.Image(180, 140, bytearray([20, 20, 20, 255]) * (180 * 140))
            comparison = appearance.compare_paint(image((20, 20, 20)), empty, Path(root) / 'diff.png')
            with self.assertRaisesRegex(RuntimeError, 'lost.*output'):
                appearance.verify_font(comparison)


class OutputProofTests(unittest.TestCase):
    def test_exact_pty_write_and_native_copy_are_required_together(self):
        appearance.verify_terminal_output(appearance.PAINT.encode(), 'python3 /owned/paint.py /owned', appearance.MARKER)

    def test_marker_in_typed_command_cannot_establish_output_proof(self):
        with self.assertRaisesRegex(RuntimeError, 'could pass through echo'):
            appearance.verify_terminal_output(appearance.PAINT.encode(), 'echo ' + appearance.MARKER, appearance.MARKER)

    def test_short_or_repeated_pty_write_and_inexact_copy_are_refused(self):
        for written, copied in [(b'', appearance.MARKER), (appearance.PAINT[:-1].encode(), appearance.MARKER),
                                (appearance.PAINT.encode() * 2, appearance.MARKER),
                                (appearance.PAINT.encode(), ''), (appearance.PAINT.encode(), appearance.MARKER + '\n')]:
            with self.subTest(written=written, copied=copied), self.assertRaises(RuntimeError):
                appearance.verify_terminal_output(written, 'python3 /owned/paint.py /owned', copied)


class OutputIsolationTests(unittest.TestCase):
    def test_only_new_direct_children_of_approved_roots_are_accepted(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            with self.assertRaises(ValueError):
                appearance.validate_output(Path(root))
            with self.assertRaises(ValueError):
                appearance.validate_output(Path(root) / 'nested')
        appearance.validate_output(Path(root))
        self.assertFalse(Path(root).exists())
        appearance.validate_output(appearance.runner.REPO_ROOT / 'native/target/tmp' / Path(root).name)
        with self.assertRaises(ValueError):
            appearance.validate_output(Path('/home/deandre/terminal-appearance-evidence'))


if __name__ == '__main__':
    unittest.main()
