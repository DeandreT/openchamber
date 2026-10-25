"""Content-size checks without launching an application or a display."""

import itertools
import subprocess
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from native_adapter import NativeClient


class SizedClient(NativeClient):
    def __init__(self, scale=1):
        self.width, self.height, self.scale = 900, 600, scale
        self.content = SimpleNamespace(width=1280, height=800)

    def _content(self):
        return self.content

    def _window(self):
        return "owned-window"


class NativeSizeTest(unittest.TestCase):
    def test_resize_adds_only_the_frame_and_respects_scale(self):
        for scale in (1, 1.25, 1.5):
            client = SizedClient(scale)
            commands = []

            def run(command, **kwargs):
                commands.append(command)
                if command[1] == "getwindowgeometry":
                    return subprocess.CompletedProcess(command, 0, stdout=f"WIDTH={client.content.width + 2}\nHEIGHT={client.content.height + 2}")
                client.content.width = int(command[-2]) - 2
                client.content.height = int(command[-1]) - 2
                return subprocess.CompletedProcess(command, 0)

            with patch("native_adapter.subprocess.run", side_effect=run), patch("native_adapter.time.sleep"):
                client._resize_to_requested(1)
            self.assertEqual(commands[-1], ["xdotool", "windowsize", "owned-window", str(round(900 * scale) + 2), str(round(600 * scale) + 2)])
            self.assertEqual((client.content.width, client.content.height), (round(900 * scale), round(600 * scale)))

    def test_an_already_correct_window_is_not_changed(self):
        client = SizedClient()
        client.content = SimpleNamespace(width=900, height=600)
        with patch("native_adapter.subprocess.run") as run:
            client._resize_to_requested(1)
        run.assert_not_called()

    def test_a_window_that_refuses_the_size_fails_readiness(self):
        client = SizedClient()
        ticks = itertools.count(step=0.1)
        result = subprocess.CompletedProcess([], 0, stdout="WIDTH=1282\nHEIGHT=802")
        with patch("native_adapter.subprocess.run", return_value=result), patch("native_adapter.time.sleep"), patch("native_adapter.time.monotonic", side_effect=lambda: next(ticks)):
            with self.assertRaisesRegex(TimeoutError, "wanted 900x600"):
                client._resize_to_requested(1)


if __name__ == "__main__":
    unittest.main()
