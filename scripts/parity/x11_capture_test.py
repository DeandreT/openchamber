"""Qualify the X11 capture with a known-color synthetic window."""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import png
import virtual_display


class X11CaptureTest(unittest.TestCase):
    def test_known_color_window_is_captured_without_desktop_services(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        root = Path(__file__).resolve().parents[2]
        code = """import ctypes, ctypes.util, os, sys
from pathlib import Path
sys.path.insert(0, 'scripts/parity')
import x11_capture
x = ctypes.CDLL(ctypes.util.find_library('X11'))
x.XOpenDisplay.argtypes = [ctypes.c_char_p]
x.XOpenDisplay.restype = ctypes.c_void_p
x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
x.XDefaultRootWindow.restype = ctypes.c_ulong
x.XCreateSimpleWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.c_int, ctypes.c_int, ctypes.c_uint, ctypes.c_uint, ctypes.c_uint, ctypes.c_ulong, ctypes.c_ulong]
x.XCreateSimpleWindow.restype = ctypes.c_ulong
x.XMapWindow.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
x.XCloseDisplay.argtypes = [ctypes.c_void_p]
d = x.XOpenDisplay(os.environ['DISPLAY'].encode())
w = x.XCreateSimpleWindow(d, x.XDefaultRootWindow(d), 0, 0, 16, 12, 0, 0, 0x123456)
x.XMapWindow(d, w)
x.XSync(d, 0)
x11_capture.capture(str(w), 16, 12, Path(sys.argv[1]))
x.XCloseDisplay(d)
"""
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / 'window.png'
            result = subprocess.run([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', code, str(target)],
                                    cwd=root, capture_output=True, text=True, timeout=45)
            self.assertEqual(result.returncode, 0, result.stderr)
            image = png.read(target)
            self.assertEqual((image.width, image.height), (16, 12))
            self.assertEqual(image.rgba, bytearray((0x12, 0x34, 0x56, 255)) * (16 * 12))


if __name__ == '__main__':
    unittest.main()
