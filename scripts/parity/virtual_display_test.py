"""Real private-display ownership and cleanup checks."""

import json
import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

import virtual_display


class DisplayOwnershipTest(unittest.TestCase):
    def test_unowned_display_is_refused(self):
        with patch.dict(os.environ, {virtual_display.XVFB_PID: "0"}):
            with self.assertRaisesRegex(RuntimeError, "does not own"):
                virtual_display.verify_owned_display()

    def test_failure_still_cleans_the_owned_display_and_runtime(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        script = """import json, os, sys
sys.path.insert(0, 'scripts/parity')
import virtual_display
virtual_display.verify_owned_display()
print('OWNED ' + json.dumps({key: os.environ[key] for key in ('DISPLAY', 'XDG_RUNTIME_DIR', 'OPENCHAMBER_PARITY_XVFB_PID')}), flush=True)
sys.exit(7)
"""
        root = Path(__file__).resolve().parents[2]
        result = subprocess.run([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
                                cwd=root, capture_output=True, text=True, timeout=45)
        self.assertEqual(result.returncode, 7, result.stderr)
        record = json.loads(next(line[6:] for line in result.stdout.splitlines() if line.startswith('OWNED ')))
        self.assertFalse(Path(record['XDG_RUNTIME_DIR']).exists())
        self.assertFalse(Path('/proc', record['OPENCHAMBER_PARITY_XVFB_PID']).exists())
        self.assertFalse(Path('/tmp/.X11-unix', 'X' + record['DISPLAY'][1:]).exists())


if __name__ == '__main__':
    unittest.main()
