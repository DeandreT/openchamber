import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import tempfile
import threading
from types import SimpleNamespace
import urllib.error
import urllib.request
from unittest.mock import Mock, patch

import native_general_settings
from native_general_settings import SettingsFaults, expected_shell_executable, settings_fields


class SettingsFieldsTest(unittest.TestCase):
    def test_missing_and_malformed_fields_are_unknown(self):
        self.assertEqual(settings_fields({'autoSaveEnabled': 1, 'terminalCloseOnExit': 'false',
                                          'terminalShell': None}), {})
        self.assertEqual(settings_fields({'autoSaveEnabled': False}), {'autoSaveEnabled': False})
        with self.assertRaises(ValueError):
            settings_fields([])

    def test_valid_independent_fields_survive(self):
        self.assertEqual(settings_fields({'autoSaveEnabled': False, 'terminalCloseOnExit': True,
                                          'terminalShell': 'bash', 'irrelevant': 1}),
                          {'autoSaveEnabled': False, 'terminalCloseOnExit': True, 'terminalShell': 'bash'})


class ShellExecutableTest(unittest.TestCase):
    def test_first_configured_executable_wins_and_symlinks_are_canonical(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            binary = root / 'real-bash'
            binary.write_text('fixture executable\n')
            binary.chmod(0o700)
            first = root / 'first/bash'
            first.parent.mkdir()
            first.symlink_to(binary)
            nonexecutable = root / 'nonexecutable/bash'
            nonexecutable.parent.mkdir()
            nonexecutable.write_text('not executable\n')
            nonexecutable.chmod(0o600)
            later = root / 'later/bash'
            later.parent.mkdir()
            later.write_text('later executable\n')
            later.chmod(0o700)
            shells = root / 'shells'
            shells.write_text(f'# {later}\nbash\n{root}/missing/bash\n{nonexecutable}\n{first}\n{later}\n')
            self.assertEqual(expected_shell_executable('bash', shells), binary)

    def test_missing_configured_shell_cannot_fall_back_to_ambient_path_or_shell(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            shell = root / 'bash'
            shell.write_text('fixture executable\n')
            shell.chmod(0o700)
            shells = root / 'shells'
            shells.write_text('/bin/sh\n')
            with patch.dict(os.environ, {'PATH': str(root), 'SHELL': str(shell)}):
                with self.assertRaisesRegex(RuntimeError, 'requires an executable bash entry'):
                    expected_shell_executable('bash', shells)

    def test_auto_cannot_supply_an_explicit_shell_expectation(self):
        with self.assertRaisesRegex(ValueError, 'explicit Bash or Zsh'):
            expected_shell_executable('auto')


class KeyboardActivationTest(unittest.TestCase):
    def test_shell_picker_uses_its_semantic_label_and_waits_for_expansion(self):
        session = Mock()
        target = session.node_named.return_value
        session.client._pointer.return_value = (100, 200)
        session.client._screen_point.return_value = (100, 200)
        target.get_state_set.return_value.contains.return_value = True
        api = SimpleNamespace(StateType=SimpleNamespace(EXPANDED='expanded'))
        with patch.object(native_general_settings, '_atspi', return_value=api):
            native_general_settings.open_shell_picker(session)
        session.node_named.assert_called_with('Select terminal shell', roles=('combo box',))
        session.click_node.assert_called_once_with(target)
        target.clear_cache.assert_called_once_with()
        target.get_state_set.return_value.contains.assert_called_once_with('expanded')
        session.client.click.assert_not_called()

    def test_shell_picker_refuses_a_pointer_failure(self):
        session = Mock()
        session.client._pointer.return_value = (0, 0)
        session.client._screen_point.return_value = (100, 200)
        with self.assertRaisesRegex(RuntimeError, 'private pointer'):
            native_general_settings.open_shell_picker(session)
        session.client.activate.assert_not_called()

    def test_space_waits_for_fresh_focused_state(self):
        session = Mock()
        node = session.client._find.return_value
        node.get_component_iface.return_value.grab_focus.return_value = True
        focused = node.get_state_set.return_value.contains
        focused.side_effect = [False, False, True]
        session.keys.side_effect = lambda *_: self.assertEqual(focused.call_count, 3)
        api = SimpleNamespace(StateType=SimpleNamespace(FOCUSED='focused'))
        with patch.object(native_general_settings, '_atspi', return_value=api):
            native_general_settings.keyboard_activate(session, 'general-auto-save')
        session.client._activate.assert_called_once_with()
        self.assertEqual(node.clear_cache.call_count, 3)
        focused.assert_called_with('focused')
        session.keys.assert_called_once_with('space')
        session.client.activate.assert_not_called()

    def test_refused_focus_sends_no_key_or_accessibility_substitute(self):
        session = Mock()
        session.client._find.return_value.get_component_iface.return_value.grab_focus.return_value = False
        with self.assertRaisesRegex(RuntimeError, 'refused keyboard focus'):
            native_general_settings.keyboard_activate(session, 'general-auto-save')
        session.keys.assert_not_called()
        session.client.activate.assert_not_called()

    def test_unacknowledged_focus_fails_within_bounded_wait_without_space(self):
        session = Mock()
        session.client._find.return_value.get_component_iface.return_value.grab_focus.return_value = True
        with patch.object(native_general_settings, 'until', side_effect=TimeoutError('focus absent')) as wait:
            with self.assertRaisesRegex(TimeoutError, 'focus absent'):
                native_general_settings.keyboard_activate(session, 'general-auto-save')
        self.assertEqual(wait.call_args.args[2], 5)
        session.keys.assert_not_called()
        session.client.activate.assert_not_called()

    def test_editor_wait_retries_mounting_lookup_without_dropping_text_assertion(self):
        ready = Mock(side_effect=[LookupError('editor not mounted'), False, True])
        native_general_settings.wait_for('owned file did not load', ready, timeout=1)
        self.assertEqual(ready.call_count, 3)


class SettingsFaultTest(unittest.TestCase):
    def setUp(self):
        self.proxy = SettingsFaults('http://127.0.0.1:1')
        self.addCleanup(self.proxy.stop)

    def test_field_failure_is_one_shot_and_independent(self):
        self.proxy.fail_field = 'autoSaveEnabled'
        self.assertIsNone(self.proxy.decision('PUT', {'terminalCloseOnExit': False}))
        self.assertEqual(self.proxy.decision('PUT', {'autoSaveEnabled': False}), 500)
        self.assertIsNone(self.proxy.decision('PUT', {'autoSaveEnabled': False}))
        self.assertEqual(len(self.proxy.settings_requests), 3)

    def test_read_failure_does_not_block_writes(self):
        self.proxy.read_failure.set()
        self.assertEqual(self.proxy.decision('GET', {}), 503)
        self.assertIsNone(self.proxy.decision('PUT', {'terminalShell': 'bash'}))
        self.proxy.read_failure.clear()
        self.assertIsNone(self.proxy.decision('GET', {}))


class SettingsProxyHTTPTest(unittest.TestCase):
    def test_failed_request_never_reaches_upstream_and_retry_preserves_body(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_PUT(self):
                body = self.rfile.read(int(self.headers['Content-Length']))
                received.append(json.loads(body))
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            proxy = SettingsFaults(f'http://127.0.0.1:{server.server_port}')
            try:
                proxy.fail_field = 'autoSaveEnabled'

                def write(value):
                    request = urllib.request.Request(proxy.url + '/api/config/settings?surface=native',
                        data=json.dumps(value).encode(), headers={'Content-Type': 'application/json'}, method='PUT')
                    with urllib.request.urlopen(request, timeout=5) as response:
                        return json.load(response)

                with self.assertRaises(urllib.error.HTTPError) as error:
                    write({'autoSaveEnabled': False})
                self.assertEqual(error.exception.code, 500)
                error.exception.close()
                self.assertEqual(received, [])
                self.assertEqual(write({'terminalCloseOnExit': False}), {'terminalCloseOnExit': False})
                self.assertEqual(write({'autoSaveEnabled': False}), {'autoSaveEnabled': False})
                self.assertEqual(received, [{'terminalCloseOnExit': False}, {'autoSaveEnabled': False}])
            finally:
                proxy.stop()
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == '__main__':
    unittest.main()
