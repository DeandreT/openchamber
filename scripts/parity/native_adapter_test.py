"""Content-size and row-state checks without launching an application."""

import itertools
import subprocess
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

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
    def test_close_refuses_a_different_display_before_loading_x11(self):
        client = SizedClient()
        client.env = {"DISPLAY": ":other"}
        with patch("native_adapter.virtual_display.verify_owned_display"), \
             patch.dict("native_adapter.os.environ", {"DISPLAY": ":owned"}), \
             patch("native_adapter.ctypes.CDLL") as library:
            with self.assertRaisesRegex(RuntimeError, "verified private display"):
                client.request_window_close()
        library.assert_not_called()


    def test_close_sends_the_delete_protocol_without_destroying_the_window(self):
        client = SizedClient()
        client.env = {"DISPLAY": ":owned"}
        x11 = Mock()
        x11.XOpenDisplay.return_value = 1
        x11.XInternAtom.side_effect = lambda display, name, only: 101 if name == b"WM_PROTOCOLS" else 102
        sent = []
        def send(display, window, propagate, mask, event):
            message = event._obj.message
            sent.append((display, window, propagate, mask, message.type, message.message_type, message.format, message.data.longs[0]))
            return 1
        x11.XSendEvent.side_effect = send
        with patch("native_adapter.virtual_display.verify_owned_display"), \
             patch.dict("native_adapter.os.environ", {"DISPLAY": ":owned"}), \
             patch("native_adapter.ctypes.util.find_library", return_value="X11"), \
             patch("native_adapter.ctypes.CDLL", return_value=x11), \
             patch.object(client, "_window", return_value="123"):
            client.request_window_close()
        self.assertEqual(sent, [(1, 123, 0, 0, 33, 101, 32, 102)])
        x11.XOpenDisplay.assert_called_once_with(b":owned")
        x11.XCloseDisplay.assert_called_once_with(1)
        x11.XDestroyWindow.assert_not_called()


    def test_first_requested_size_does_not_outlive_the_startup_hold(self):
        client = SizedClient()
        client.content = SimpleNamespace(width=900, height=600)
        clock = {'now': 0.0, 'restored': False}
        size_times = []

        def advance(seconds):
            clock['now'] += seconds
            if clock['now'] >= 0.1 and not clock['restored']:
                clock['restored'] = True
                client.content = SimpleNamespace(width=1280, height=800)

        def run(command, **kwargs):
            if command[1] == 'getwindowgeometry':
                return subprocess.CompletedProcess(command, 0,
                    stdout=f'WIDTH={client.content.width + 2}\nHEIGHT={client.content.height + 2}')
            size_times.append(clock['now'])
            client.content = SimpleNamespace(width=int(command[-2]) - 2, height=int(command[-1]) - 2)
            return subprocess.CompletedProcess(command, 0)

        with patch.object(client, 'wait_for_marker', return_value={}), \
             patch.object(client, '_find_app', return_value=object()), \
             patch.object(client, '_read_marks', side_effect=advance), \
             patch('native_adapter.time.monotonic', side_effect=lambda: clock['now']), \
             patch('native_adapter.time.sleep', side_effect=advance), \
             patch('native_adapter.subprocess.run', side_effect=run):
            client.wait_ready(3, connected=True)
        self.assertTrue(clock['restored'], 'the fixture must observe the later startup restoration')
        self.assertGreaterEqual(clock['now'], 2)
        self.assertEqual((client.content.width, client.content.height), (900, 600))
        self.assertTrue(size_times)
        self.assertTrue(all(value >= 2 for value in size_times), 'startup owns its size during the hold')

    def test_screenshot_rejects_wrong_requested_size_at_each_scale(self):
        for scale in (1, 1.25, 1.5):
            with self.subTest(scale=scale):
                client = SizedClient(scale)
                image = SimpleNamespace(width=1280, height=800)
                with patch.object(client, '_activate'), \
                     patch('native_adapter.subprocess.run', return_value=subprocess.CompletedProcess(
                         [], 0, stdout='WIDTH=1280\nHEIGHT=800')), \
                     patch('native_adapter.x11_capture.capture') as capture, \
                     patch('native_adapter.png.read', return_value=image):
                    with self.assertRaisesRegex(RuntimeError, 'wanted'):
                        client.screenshot(Path('unused.png'))
                capture.assert_not_called()

    def test_startup_hold_still_respects_the_existing_readiness_deadline(self):
        client = SizedClient()
        client.content = SimpleNamespace(width=900, height=600)
        client._initial_size_hold_until = 2.0
        ticks = itertools.count(step=0.1)
        with patch.object(client, '_read_marks'), \
             patch('native_adapter.time.monotonic', side_effect=lambda: next(ticks)), \
             patch('native_adapter.subprocess.run') as run:
            with self.assertRaisesRegex(TimeoutError, 'wanted 900x600'):
                client._resize_to_requested(0.5)
        run.assert_not_called()

    def test_screenshot_preserves_the_scaled_content_and_frame_crop(self):
        for scale in (1, 1.25, 1.5):
            with self.subTest(scale=scale):
                client = SizedClient(scale)
                width, height = round(900 * scale), round(600 * scale)
                client.content = SimpleNamespace(width=width, height=height)
                image = SimpleNamespace(width=width + 2, height=height + 2)
                with patch.object(client, '_activate'), \
                     patch('native_adapter.subprocess.run', return_value=subprocess.CompletedProcess(
                         [], 0, stdout=f'WIDTH={width + 2}\nHEIGHT={height + 2}')), \
                     patch('native_adapter.x11_capture.capture') as capture, \
                     patch('native_adapter.png.read', return_value=image), \
                     patch('native_adapter.png.crop', return_value=Mock()), \
                     patch('native_adapter.png.write'):
                    result = client.screenshot(Path('unused.png'))
                self.assertEqual(result['crop'], [1, 1, width, height])
                capture.assert_called_once_with('owned-window', width + 2, height + 2, Path('unused.png'))

    def test_screenshot_rejects_a_size_change_during_capture(self):
        client = SizedClient()
        content = SimpleNamespace(width=900, height=600)
        restored = SimpleNamespace(width=1280, height=800)
        image = SimpleNamespace(width=902, height=602)
        with patch.object(client, '_activate'), \
             patch.object(client, '_content', side_effect=[content, restored]), \
             patch('native_adapter.subprocess.run', return_value=subprocess.CompletedProcess(
                 [], 0, stdout='WIDTH=902\nHEIGHT=602')), \
             patch('native_adapter.x11_capture.capture'), \
             patch('native_adapter.png.read', return_value=image), \
             patch('native_adapter.png.crop', return_value=Mock()), \
             patch('native_adapter.png.write'):
            with self.assertRaisesRegex(RuntimeError, 'wanted'):
                client.screenshot(Path('unused.png'))

    def test_accessibility_root_can_arrive_after_the_application_node(self):
        client = SizedClient()
        client.content = SimpleNamespace(width=900, height=600)
        with patch.object(client, "_content", side_effect=[LookupError("not published"), client.content]), patch.object(client, "_read_marks") as wait:
            client._resize_to_requested(1)
        wait.assert_called_once_with(0.05)

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


class RowNode:
    def __init__(self, key, states=(), children=()):
        self.key, self.states, self.children = key, set(states), children
        self.cache_clears = 0

    def get_accessible_id(self):
        return self.key

    def get_child_count(self):
        return len(self.children)

    def get_child_at_index(self, index):
        return self.children[index]

    def get_name(self):
        return self.key

    def get_state_set(self):
        return SimpleNamespace(contains=lambda value: value in self.states)

    def clear_cache(self):
        self.cache_clears += 1


class RowClient(NativeClient):
    def __init__(self, root):
        self.root = root

    def _find(self, key):
        assert key == "sidebar"
        return self.root

    def _content(self):
        return None

    def _rect(self, node, content=None):
        return [0, 0, 100, 30]


class NativeExpansionTest(unittest.TestCase):
    def test_row_states_distinguish_absent_collapsed_and_expanded(self):
        row = RowNode("row")
        client = RowClient(RowNode("sidebar", children=[row]))
        states = SimpleNamespace(SELECTED="selected", EXPANDABLE="expandable", EXPANDED="expanded")
        with patch("native_adapter._atspi", return_value=SimpleNamespace(StateType=states)):
            self.assertIsNone(client.rows()[0]["expanded"])
            row.states = {"expandable"}
            self.assertIs(client.rows()[0]["expanded"], False)
            row.states = {"expandable", "expanded"}
            self.assertIs(client.rows()[0]["expanded"], True)
        self.assertEqual(row.cache_clears, 3)


class NamedElementWaitTest(unittest.TestCase):
    def test_name_wait_checks_role_and_identity(self):
        node = SimpleNamespace(
            get_name=lambda: "Search projects...",
            get_role_name=lambda: "entry",
            get_accessible_id=lambda: "project-search",
        )
        client = NativeClient.__new__(NativeClient)
        for wanted, expected in (
            ({"name": "Search projects...", "role": "entry"}, True),
            ({"name": "Search projects...", "id": "project-search"}, True),
            ({"name": "Search projects...", "role": "button"}, False),
            ({"name": "Search projects...", "id": "other-search"}, False),
            ({"name": "A different field"}, False),
            ({"name": "Search proj", "prefix": True}, True),
            ({"name": "Search proj"}, False),
        ):
            ticks = itertools.count(step=0.1)
            with patch.object(client, "_walk", return_value=[node]), patch.object(client, "_read_marks"), patch("native_adapter.time.monotonic", side_effect=lambda: next(ticks)):
                self.assertEqual(client.wait_for({"named": wanted}, 0.5), expected)
        with patch.object(client, "_walk", return_value=[]), patch.object(client, "_read_marks"):
            self.assertTrue(client.wait_for({"named": {"name": "Search projects..."}, "present": False}, 0.5))

    def test_focused_text_wait_needs_the_exact_text(self):
        client = NativeClient.__new__(NativeClient)
        for text, expected in (("draft one", True), ("draft one\n", False), (None, False)):
            ticks = itertools.count(step=0.1)
            with patch.object(client, "focused_text", return_value=text), patch.object(client, "_read_marks"), \
                    patch("native_adapter.time.monotonic", side_effect=lambda: next(ticks)):
                self.assertEqual(client.wait_for({"focusedText": "draft one"}, 0.5), expected)


class ComposerCoverageTest(unittest.TestCase):
    def client(self, children):
        root = RowNode("composer", children=children)
        client = NativeClient.__new__(NativeClient)
        client._find = lambda key: root
        client._content = lambda: None
        client._rect = lambda node, content=None: [0, 0, 200, 100] if node is root else node.rect
        return client

    def node(self, role, rect):
        node = RowNode(role)
        node.rect = rect
        node.get_role_name = lambda: role
        return node

    def test_whole_editor_and_candidate_only_controls_stay_protected(self):
        editor = self.node("entry", [5, 16, 190, 21])
        extra = self.node("push button", [70, 4, 20, 10])
        result = self.client([editor, extra]).paint_regions()
        self.assertTrue(result["composerGradientProtectionComplete"])
        self.assertEqual(result["composerGradientRegions"], [[50, 2, 100, 48]])
        self.assertEqual(result["composerGradientProtected"], [extra.rect, editor.rect])

    def test_missing_editor_or_unmeasured_content_disables_the_allowance(self):
        self.assertFalse(self.client([]).paint_regions()["composerGradientProtectionComplete"])
        editor = self.node("entry", [5, 16, 190, 21])
        hidden = self.node("push button", [0, 0, 0, 0])
        self.assertFalse(self.client([editor, hidden]).paint_regions()["composerGradientProtectionComplete"])

    def test_unnamed_and_unknown_leaf_controls_are_protected(self):
        for role in ("slider", "check box", "unknown", "filler"):
            editor = self.node("entry", [5, 16, 190, 21])
            extra = self.node(role, [70, 4, 20, 10])
            extra.get_name = lambda: ""
            result = self.client([editor, extra]).paint_regions()
            self.assertTrue(result["composerGradientProtectionComplete"])
            self.assertIn(extra.rect, result["composerGradientProtected"])


class InputClient(NativeClient):
    def __init__(self, nodes=()):
        self.nodes = list(nodes)
        self.marks, self._input_mark, self.substitutions = [], 0, []
        self.clicked = []

    def _walk(self):
        return iter(self.nodes)

    def _activate(self):
        pass

    def _click_node(self, node, label):
        self.clicked.append((node, label))


class NativeStepInputTest(unittest.TestCase):
    def test_bare_keys_keep_atspi_and_chords_use_xtest(self):
        client = InputClient()
        atspi = SimpleNamespace(generate_keyboard_event=lambda *arguments: atspi.sent.append(arguments),
                                KeySynthType=SimpleNamespace(SYM="sym"), sent=[])
        with patch("native_adapter._atspi", return_value=atspi), patch.object(NativeClient, "_xdotool") as xdotool:
            client.key("Return")
            client.key("ctrl+shift+z")
            client.key("ctrl+Return")
        self.assertEqual(len(atspi.sent), 1)
        self.assertEqual(atspi.sent[0][1:], (None, "sym"))
        self.assertEqual([entry.args for entry in xdotool.call_args_list], [("key", "ctrl+shift+z"), ("key", "ctrl+Return")])

    def test_named_clicks_need_one_exact_name_and_role(self):
        def node(name, role):
            return SimpleNamespace(get_name=lambda: name, get_role_name=lambda: role)

        send, other_send, label = node("Send", "push button"), node("Send", "menu item"), node("Send later", "push button")
        client = InputClient([send, other_send, label])
        client.click_named({"name": "Send", "roles": ["push button"]})
        self.assertEqual(client.clicked, [(send, "named:Send")])
        with self.assertRaises(LookupError):
            client.click_named({"name": "Send"})
        with self.assertRaises(LookupError):
            client.click_named({"name": "Missing", "roles": ["push button"]})

    def test_row_scoped_targets_need_one_row_and_one_control_in_it(self):
        def node(name, role, children=()):
            return SimpleNamespace(get_name=lambda: name, get_role_name=lambda: role,
                                   get_child_count=lambda: len(children), get_child_at_index=lambda index: children[index])

        alpha_send, beta_send = node("send", "button"), node("send", "button")
        alpha, beta = node("Queued alpha", "panel", [alpha_send]), node("Queued beta", "panel", [beta_send])
        client = InputClient([alpha, alpha_send, beta, beta_send])
        client.click_target({"within": {"name": "Queued beta", "role": "panel"}, "name": "send"})
        self.assertEqual(client.clicked, [(beta_send, "Queued beta/send")])
        with self.assertRaises(LookupError):
            client.click_target({"within": {"name": "Queued gamma"}, "name": "send"})
        with self.assertRaises(LookupError):
            client.click_target({"within": {"name": "Queued beta"}, "name": "edit"})
        with self.assertRaises(LookupError):
            client.click_target({"name": "send", "role": "button"})
        client.click_target({"name": "Queued alpha", "role": "panel"})
        self.assertEqual(client.clicked[-1], (alpha, "Queued alpha"))
        client.click_target({"name": "Queued b", "prefix": True})
        self.assertEqual(client.clicked[-1], (beta, "Queued b"))

    def test_restart_signals_waits_and_clears_the_previous_process_state(self):
        client = InputClient()
        process = SimpleNamespace(pid=4242, returncode=0, stdout=SimpleNamespace(close=lambda: None), wait=lambda timeout: 0)
        client.process = process
        client.marks, client._buffer, client._input_mark, client._app = [{"marker": "old"}], "partial", 1, object()
        client._selector = SimpleNamespace(unregister=lambda _stream: None)
        with patch("native_adapter.os.kill") as kill, patch.object(NativeClient, "close", return_value={}), \
                patch.object(NativeClient, "launch", return_value={"pid": 4343}):
            record = client.restart("signal", 5)
        kill.assert_called_once()
        self.assertEqual(record, {"closed": {"exitCode": 0}, "launch": {"pid": 4343}})
        self.assertEqual((client.marks, client._buffer, client._input_mark, client._app), ([], "", 0, None))


if __name__ == "__main__":
    unittest.main()
