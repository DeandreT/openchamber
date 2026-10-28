"""A separate X11 clipboard owner proves that probes wait for real copy output."""

from pathlib import Path
import subprocess
import sys
import unittest

import virtual_display


class ClipboardProbeTest(unittest.TestCase):
    def test_paste_serves_exact_unicode_and_requires_a_new_result(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        root = Path(__file__).resolve().parents[2]
        script = r'''
import subprocess, sys
sys.path.insert(0, 'scripts/parity')
from clipboard_probe import ClipboardProbe
probe = ClipboardProbe()
text = '  é e\u0301 你好 👩\u200d💻\n'
try:
    probe.paste(text, lambda: None, lambda: True)
except ValueError:
    pass
else:
    raise AssertionError('existing text is not proof of a paste')
reader = """
import sys, gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
from gi.repository import Gtk, Gdk, GLib
Gtk.init()
loop = GLib.MainLoop()
def got(clipboard, result):
    sys.stdout.write(clipboard.read_text_finish(result))
    loop.quit()
Gdk.Display.get_default().get_clipboard().read_text_async(None, got)
loop.run()
"""
children = []
output = []
def dispatch():
    children.append(subprocess.Popen([sys.executable, '-c', reader], stdout=subprocess.PIPE, text=True))
def observed():
    if not children or children[0].poll() is None:
        return False
    if not output:
        output.append(children[0].communicate()[0])
    return output[0] == text
try:
    probe.paste(text, dispatch, observed)
    assert output == [text], repr(output)
finally:
    for child in children:
        if child.poll() is None:
            child.terminate()
        child.wait(timeout=5)
'''
        result = subprocess.run(
            [sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
            cwd=root, capture_output=True, text=True, timeout=45,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_waits_for_a_new_owner_and_preserves_exact_unicode(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        root = Path(__file__).resolve().parents[2]
        script = r'''
import subprocess, sys
sys.path.insert(0, 'scripts/parity')
from clipboard_probe import ClipboardProbe
probe = ClipboardProbe()
try:
    probe.copy(lambda: None, timeout_ms=50)
except TimeoutError:
    pass
else:
    raise AssertionError('an old clipboard value must never count as a copy')
text = '  é e\u0301 你好 👩\u200d💻\n'
producer = r"""
import sys, gi
gi.require_version('Gtk', '4.0')
gi.require_version('Gdk', '4.0')
from gi.repository import Gtk, Gdk, GLib
Gtk.init()
Gdk.Display.get_default().get_clipboard().set(sys.argv[1])
GLib.MainLoop().run()
"""
children = []
def dispatch():
    children.append(subprocess.Popen([sys.executable, '-c', producer, text]))
try:
    assert probe.copy(dispatch) == text
finally:
    for child in children:
        child.terminate()
        child.wait(timeout=5)
'''
        result = subprocess.run(
            [sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
            cwd=root, capture_output=True, text=True, timeout=45,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main()
