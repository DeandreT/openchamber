"""Control targets keep their selector or exact-button contract through CDP."""

from pathlib import Path
import unittest
from unittest.mock import patch

from electron_adapter import ElectronClient


class ElectronTargetTest(unittest.TestCase):
    def test_control_click_accepts_both_target_kinds(self):
        client = ElectronClient(Path("unused"), {}, 1, 1280, 800)
        for spec in ({"button": "Choose project"}, {"selector": "button[aria-haspopup='dialog']"}):
            with patch.object(client, "_call", return_value={}) as call:
                client.click_target(spec)
                call.assert_called_once_with("click", **spec)

    def test_launch_keeps_output_inside_the_private_profile(self):
        env = {"OPENCHAMBER_DATA_DIR": "/private/run/state/oc-data"}
        client = ElectronClient(Path("unused"), env, 1, 1280, 800)
        with patch("virtual_display.child_environment", return_value=env):
            with patch("electron_adapter.subprocess.Popen"):
                with patch.object(client, "_call", return_value={}) as call:
                    client.launch()
        self.assertEqual(call.call_args.kwargs["logPath"], "/private/run/state/electron.log")


if __name__ == '__main__':
    unittest.main()
