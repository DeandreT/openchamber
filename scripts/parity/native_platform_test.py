"""The platform runner starts with its own application and input-method state."""

import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import native_platform


class PlatformEnvironmentTest(unittest.TestCase):
    def test_editable_text_uses_the_text_interface_method(self):
        node = Mock()
        interface = node.get_text_iface.return_value
        api = Mock()
        api.Text.get_text.return_value = "你好\n🙂"
        with patch.object(native_platform, "_atspi", return_value=api):
            self.assertEqual(native_platform.editable_text(node), "你好\n🙂")
        api.Text.get_text.assert_called_once_with(interface, 0, -1)
        interface.get_text.assert_not_called()

    def test_missing_text_interface_is_a_reportable_failure(self):
        node = Mock()
        node.get_text_iface.return_value = None
        with self.assertRaisesRegex(RuntimeError, "no accessible text interface"):
            native_platform.editable_text(node)

    def test_wait_allows_delayed_accessibility_publication(self):
        attempts = []
        def visible():
            attempts.append(True)
            if len(attempts) == 1:
                raise LookupError("the accessibility node is not published yet")
            return True
        with patch.object(native_platform.time, "sleep"):
            native_platform.wait_for("content never appeared", visible)
        self.assertEqual(len(attempts), 2)

    def test_profile_does_not_inherit_application_or_input_method_state(self):
        inherited = {
            "HOME": "/personal/home",
            "OPENCHAMBER_NATIVE_PASSWORD": "test-only-password",
            "OPENCODE_CONFIG_CONTENT": "test-only-config",
            "OPENAI_API_KEY": "test-only-key",
            "IBUS_ADDRESS": "unix:path=/personal/input-bus",
            "XMODIFIERS": "@im=personal",
            "PATH": "/test/bin",
        }
        scratch = "/tmp/opencode" if Path("/tmp/opencode").is_dir() else None
        with tempfile.TemporaryDirectory(dir=scratch) as temporary:
            profile = Path(temporary)
            with patch.dict(os.environ, inherited, clear=True), patch.object(native_platform.virtual_display, "child_environment", side_effect=lambda env: env):
                env = native_platform.isolated_environment(profile)
            self.assertEqual(env["HOME"], str(profile))
            self.assertEqual(env["XDG_CONFIG_HOME"], str(profile / "config"))
            self.assertTrue((profile / "config").is_dir())
            self.assertEqual(env["OPENCHAMBER_NATIVE_WORKLOAD"], "small")
            for key in ("OPENCHAMBER_NATIVE_PASSWORD", "OPENCODE_CONFIG_CONTENT", "OPENAI_API_KEY", "IBUS_ADDRESS", "XMODIFIERS"):
                self.assertNotIn(key, env)


if __name__ == '__main__':
    unittest.main()
