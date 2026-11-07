"""Content-size and row-state checks without launching an application."""

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
