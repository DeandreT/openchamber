"""Display isolation for the native desktop benchmark."""

import argparse
import os
import unittest
from unittest.mock import patch

import native_bench


class NativeBenchmarkEnvironmentTest(unittest.TestCase):
    def test_other_display_backends_are_rejected(self):
        with self.assertRaisesRegex(ValueError, "owned X11 display"):
            native_bench.apply_backend({}, "wayland")

    def test_client_environment_uses_verified_display_handles(self):
        source = {"HOME": "/isolated/home", "DISPLAY": ":wrong"}
        verified = {"HOME": "/isolated/home", "DISPLAY": ":1234"}
        with patch.object(native_bench.virtual_display, "child_environment", return_value=verified) as check:
            self.assertEqual(native_bench.apply_backend(source, "x11"), verified)
        check.assert_called_once_with(source)

    def test_native_keeps_verified_environment_and_removes_inherited_server(self):
        args = argparse.Namespace(backend="x11", workload="small", theme="openchamber-dark", server=None)
        with patch.dict(os.environ, {"OPENCHAMBER_NATIVE_SERVER": "https://not-the-fixture"}):
            with patch.object(native_bench.virtual_display, "child_environment", side_effect=lambda env: {**env, "DISPLAY": ":1234"}):
                env = native_bench.native_env(args)
        self.assertEqual(env["DISPLAY"], ":1234")
        self.assertEqual(env["OPENCHAMBER_NATIVE_WORKLOAD"], "small")
        self.assertNotIn("OPENCHAMBER_NATIVE_SERVER", env)


if __name__ == "__main__":
    unittest.main()
