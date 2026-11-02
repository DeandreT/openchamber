"""Focused tests for evidence interpretation and output isolation, not UI proof."""

from pathlib import Path
import tempfile
import unittest

import native_appearance
import png


class AppearanceEvidenceTests(unittest.TestCase):
    def test_font_trigger_below_modal_must_scroll_despite_workspace_containment(self):
        frame = [64.8, 60.8, 1150.4, 678.4]
        rect = [784, 750.4, 173.6, 32]
        self.assertLess(rect[1] + rect[3], 800)
        self.assertEqual(native_appearance.settings_scroll_direction(rect, frame), 1)

    def test_control_above_modal_scrolls_up_even_when_inside_workspace(self):
        self.assertEqual(native_appearance.settings_scroll_direction([370, 30, 170, 32], [64, 60, 1152, 680]), -1)

    def test_modal_containment_accepts_a_fully_exposed_control(self):
        self.assertEqual(native_appearance.settings_scroll_direction([784, 650, 174, 32], [64, 60, 1152, 680]), 0)

    def test_horizontal_overflow_or_invalid_settings_geometry_is_refused(self):
        with self.assertRaises(RuntimeError):
            native_appearance.settings_scroll_direction([1200, 650, 174, 32], [64, 60, 1152, 680])
        with self.assertRaises(ValueError):
            native_appearance.settings_scroll_direction([784, 650, 0, 32], [64, 60, 1152, 680])

    def test_page_scroll_preserves_fixed_chrome_with_rounding(self):
        before = {'settings-dialog': [64, 60.8, 1152, 680], 'settings-navigation': [64, 60.8, 256, 680]}
        after = {'settings-dialog': [64, 61, 1152, 680], 'settings-navigation': [64, 61, 256, 680]}
        native_appearance.verify_scroll_frame(before, after)

    def test_page_scroll_rejects_a_moved_dialog_or_navigation(self):
        before = {'settings-dialog': [64, 60.8, 1152, 680], 'settings-navigation': [64, 60.8, 256, 680]}
        for key in before:
            after = {anchor: list(bounds) for anchor, bounds in before.items()}
            after[key][1] -= 156
            with self.subTest(anchor=key), self.assertRaisesRegex(RuntimeError, 'Page scrolling moved'):
                native_appearance.verify_scroll_frame(before, after)

    def test_text_growth_accepts_actual_one_and_half_line_height(self):
        native_appearance.verify_text_growth(24, 36)

    def test_text_growth_rejects_saved_flag_without_geometry_growth(self):
        with self.assertRaises(RuntimeError):
            native_appearance.verify_text_growth(24, 24)

    def test_text_growth_rejects_missing_or_clipped_heading(self):
        for before, after in [(0, 36), (24, 0), (24, 12)]:
            with self.subTest(before=before, after=after), self.assertRaises(RuntimeError):
                native_appearance.verify_text_growth(before, after)

    def test_text_restore_accepts_rounding_to_one_logical_pixel(self):
        native_appearance.verify_text_restore(24, 24.8)

    def test_text_restore_rejects_unrestored_or_missing_heading(self):
        for before, restored in [(24, 36), (24, 0), (0, 0)]:
            with self.subTest(before=before, restored=restored), self.assertRaises(RuntimeError):
                native_appearance.verify_text_restore(before, restored)

    def test_startup_and_restart_environment_has_no_development_appearance_pins(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            for _ in range(2):
                env = native_appearance.native_environment(Path(root), 'http://127.0.0.1:1/')
                self.assertNotIn('OPENCHAMBER_NATIVE_THEME', env)
                self.assertNotIn('OPENCHAMBER_NATIVE_LOCALE', env)
                self.assertEqual(env['OPENCHAMBER_NATIVE_SERVER'], 'http://127.0.0.1:1/')

    def test_dominant_painted_color_ignores_small_foreground_region(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            path = Path(root) / 'paint.png'
            image = png.Image(64, 64, bytearray([240, 240, 240, 255]) * 4096)
            image.rgba[:256] = bytearray([10, 10, 10, 255]) * 64
            png.write(path, image)
            self.assertAlmostEqual(native_appearance.background_luminance(path), 240)
            image.rgba[:] = bytearray([20, 20, 20, 255]) * 4096
            png.write(path, image)
            self.assertAlmostEqual(native_appearance.background_luminance(path), 20)

    def test_existing_and_unowned_outputs_are_refused(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            with self.assertRaises(ValueError):
                native_appearance.validate_output(Path(root))
            with self.assertRaises(ValueError):
                native_appearance.validate_output(Path(root) / 'nested')
        with self.assertRaises(ValueError):
            native_appearance.validate_output(Path('/home/deandre/appearance-evidence'))

    def test_new_owned_output_is_accepted_without_creating_it(self):
        with tempfile.TemporaryDirectory(dir='/tmp/opencode') as root:
            target = Path(root)
        native_appearance.validate_output(target)
        self.assertFalse(target.exists())


if __name__ == '__main__':
    unittest.main()
