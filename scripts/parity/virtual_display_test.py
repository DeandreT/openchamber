"""Real private-display ownership and cleanup checks."""

import json
import os
import select
import signal
from pathlib import Path
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

import virtual_display


class DisplayOwnershipTest(unittest.TestCase):
    def test_sigterm_cleans_the_owned_display_and_runtime(self):
        if virtual_display.available():
            self.skipTest(virtual_display.available())
        root = Path(__file__).resolve().parents[2]
        script = """import json, os, time
print('OWNED ' + json.dumps({key: os.environ[key] for key in ('DISPLAY', 'XDG_RUNTIME_DIR', 'OPENCHAMBER_PARITY_XVFB_PID')}), flush=True)
time.sleep(120)
"""
        process = subprocess.Popen([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
                                   cwd=root, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
        try:
            output = b""
            deadline = time.monotonic() + 30
            while b'OWNED ' not in output or not output.split(b'OWNED ', 1)[1].endswith(b'\n'):
                remaining = max(0, deadline - time.monotonic())
                self.assertTrue(select.select([process.stdout], [], [], remaining)[0], "private session did not start")
                chunk = os.read(process.stdout.fileno(), 4096)
                self.assertTrue(chunk, output.decode())
                output += chunk
            line = next(line for line in output.decode().splitlines() if line.startswith('OWNED '))
            record = json.loads(line[6:])
            process.send_signal(signal.SIGTERM)
            self.assertEqual(process.wait(timeout=40), 128 + signal.SIGTERM)
            self.assertFalse(Path(record['XDG_RUNTIME_DIR']).exists())
            self.assertFalse(Path('/proc', record['OPENCHAMBER_PARITY_XVFB_PID']).exists())
            self.assertFalse(Path('/tmp/.X11-unix', 'X' + record['DISPLAY'][1:]).exists())
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=40)
            process.stdout.close()

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
assert not any(key in os.environ for key in virtual_display.INPUT_SESSION_VARIABLES)
print('OWNED ' + json.dumps({key: os.environ[key] for key in ('DISPLAY', 'XDG_RUNTIME_DIR', 'OPENCHAMBER_PARITY_XVFB_PID')}), flush=True)
sys.exit(7)
"""
        root = Path(__file__).resolve().parents[2]
        env = dict(os.environ, IBUS_ADDRESS="unix:path=/not-the-private-bus", XMODIFIERS="@im=personal")
        result = subprocess.run([sys.executable, str(root / 'scripts/parity/virtual_display.py'), '--', sys.executable, '-c', script],
                                cwd=root, env=env, capture_output=True, text=True, timeout=45)
        self.assertEqual(result.returncode, 7, result.stderr)
        record = json.loads(next(line[6:] for line in result.stdout.splitlines() if line.startswith('OWNED ')))
        self.assertFalse(Path(record['XDG_RUNTIME_DIR']).exists())
        self.assertFalse(Path('/proc', record['OPENCHAMBER_PARITY_XVFB_PID']).exists())
        self.assertFalse(Path('/tmp/.X11-unix', 'X' + record['DISPLAY'][1:]).exists())


if __name__ == '__main__':
    unittest.main()
