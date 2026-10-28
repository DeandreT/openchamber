"""Display isolation for the native desktop benchmark."""

import argparse
import io
import os
import sys
import time
import types
from pathlib import Path
import unittest
from unittest.mock import Mock, patch

import native_bench


class NativeBenchmarkWorkflowTest(unittest.TestCase):
    def test_connected_startup_does_not_wait_for_unselected_history(self):
        args = argparse.Namespace(server="http://fixture", timeout=1, settle=0,
                                  server_pid=None, idle_seconds=1)
        run = Mock(marks=[])
        run.wait_for.side_effect = lambda name, timeout: {
            "composer-usable": {"arrivedMs": 1, "sinceStartMs": 1},
            "sessions-shown": {"arrivedMs": 2, "count": 3},
        }[name]
        with patch.object(native_bench, "launch_native", return_value=run), \
                patch.object(native_bench, "tree_memory", return_value={}), \
                patch.object(native_bench, "tree_cpu_ticks", return_value=0), \
                patch.object(native_bench, "stop_tree", return_value={}), \
                patch.object(native_bench.time, "sleep"):
            sample = native_bench.native_startup_once(args)
        self.assertNotIn("error", sample)
        self.assertEqual(sample["selectionState"], "unselected")
        self.assertNotIn("historyShownSinceSpawnMs", sample)

    def test_only_session_rows_enter_plan_and_duplicates_visit_once(self):
        rows = [self.node("project:p"), self.node("p:session:a"),
                self.node("worktree:w"), self.node("p:session:b"),
                self.node("pinned:session:a")]
        root = self.node("sidebar", rows, role="tree")
        self.assertEqual(list(native_bench.session_rows(root)), ["a", "b"])

    @staticmethod
    def node(key, children=(), role="tree item"):
        node = Mock()
        node.get_accessible_id.return_value = key
        node.get_role_name.return_value = role
        node.get_child_count.return_value = len(children)
        node.get_child_at_index.side_effect = lambda i: children[i]
        return node

    def test_failed_sample_invalidates_group_without_dropping_raw_samples(self):
        samples = [{"warmup": False, "value": 3}, {"warmup": False, "error": "timeout"}]
        self.assertEqual(native_bench.complete_samples(samples), [])
        self.assertEqual(len(samples), 2)

    def test_failed_warmup_invalidates_group(self):
        self.assertEqual(native_bench.complete_samples([
            {"warmup": True, "error": "timeout"}, {"warmup": False, "value": 3}]), [])

    def test_explicit_plan_keeps_requested_order_and_rejects_missing_rows(self):
        self.assertEqual(native_bench.switch_plan({"a": 1, "b": 2}, 1, ["b", "a"]), ["b", "a"])
        for requested in (["a", "a"], ["a", "missing"], ["a"]):
            with self.assertRaises(RuntimeError):
                native_bench.switch_plan({"a": 1, "b": 2}, 1, requested)

    def test_connected_switch_times_dispatch_after_discovery_and_waits_for_cold_history(self):
        args = argparse.Namespace(server="http://fixture", timeout=1, switches=1, sessions=["a", "b"])
        rows = {key: self.node(f"p:session:{key}") for key in ("a", "b")}
        selected = [None]
        clicks = []
        clock_ns = [0]
        def discover_action(index):
            clock_ns[0] += 20_000_000
            return "click"
        for key, row in rows.items():
            row.get_state_set.return_value.contains.side_effect = lambda state, key=key: selected[0] == key
            action = row.get_action_iface.return_value
            action.get_n_actions.return_value = 1
            action.get_action_name.side_effect = discover_action
            def click(index, key=key):
                clock_ns[0] += 3_000_000
                selected[0] = key
                clicks.append(key)
                return True
            action.do_action.side_effect = click
        app = self.node("app", list(rows.values()), role="application")
        app.get_process_id.return_value = 123
        desktop = self.node("desktop", [app])
        atspi = Mock()
        atspi.get_desktop.return_value = desktop
        modules = {"gi": Mock(), "gi.repository": types.SimpleNamespace(Atspi=atspi)}
        run = Mock(marks=[], spawned_ns=0)
        run.process.pid = 123
        boundaries = []
        def mark(name, timeout, after=0):
            boundaries.append((selected[0], name))
            value = {"marker": name, "arrivedMs": clock_ns[0] / 1e6,
                     "durationMs": 0.1}
            run.marks.append(value)
            return value
        run.wait_for.side_effect = mark
        with patch.dict(sys.modules, modules), \
                patch.object(native_bench.time, "monotonic_ns", side_effect=lambda: clock_ns[0]), \
                patch.object(native_bench.time, "monotonic", side_effect=lambda: clock_ns[0] / 1e9), \
                patch.object(native_bench, "launch_native", return_value=run), \
                patch.object(native_bench, "tree_memory", return_value={}), \
                patch.object(native_bench, "stop_tree", return_value={}):
            sample = native_bench.native_switch_once(args)
        self.assertNotIn("error", sample)
        self.assertEqual(clicks, ["a", "b", "a"])
        self.assertEqual([session for session, name in boundaries if name == "history-shown"], ["a", "b"])
        self.assertEqual(sample["setup"]["sessionId"], "a")
        self.assertEqual([entry["sessionId"] for entry in sample["forward"]], ["b"])
        self.assertEqual([entry["sessionId"] for entry in sample["reverse"]], ["a"])
        for visit in [sample["setup"], *sample["forward"], *sample["reverse"]]:
            self.assertEqual(visit["clickToMarkMs"], 3, "action discovery must precede the input timestamp")

    def test_connected_switch_requires_sessions_with_whitespace_in_scenarios(self):
        for scenarios in ("startup,switch", "startup, switch", " startup , switch , "):
            with self.subTest(scenarios=scenarios), \
                    patch.object(sys, "argv", ["native_bench.py", "--server", "http://fixture", "--scenarios", scenarios]), \
                    patch.object(sys, "stderr", new_callable=io.StringIO) as stderr, \
                    patch.object(native_bench.virtual_display, "inside", side_effect=AssertionError("validation must precede runtime setup")):
                with self.assertRaises(SystemExit) as raised:
                    native_bench.main()
                self.assertEqual(raised.exception.code, 2)
                self.assertIn("connected switches require --sessions", stderr.getvalue())

    def test_nonblocking_drain_collects_pending_mark_before_input_boundary(self):
        read_fd, write_fd = os.pipe()
        run = native_bench.NativeRun(process=Mock(), spawned_ns=time.monotonic_ns())
        try:
            run.selector.register(read_fd, 1)
            os.write(write_fd, b'OPENCHAMBER_NATIVE_MARK {"marker":"switch-completed"}\n')
            run.read_marks(0)
            self.assertEqual([mark["marker"] for mark in run.marks], ["switch-completed"])
        finally:
            run.selector.close()
            os.close(read_fd)
            os.close(write_fd)


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
        args = argparse.Namespace(backend="x11", workload="small", theme="openchamber-dark", server=None, native_home=Path("/private/native-profile"))
        with patch.dict(os.environ, {"OPENCHAMBER_NATIVE_SERVER": "https://not-the-fixture"}, clear=True):
            with patch.object(native_bench.virtual_display, "child_environment", side_effect=lambda env: {**env, "DISPLAY": ":1234"}):
                env = native_bench.native_env(args)
        self.assertEqual(env["DISPLAY"], ":1234")
        self.assertEqual(env["OPENCHAMBER_NATIVE_WORKLOAD"], "small")
        self.assertNotIn("OPENCHAMBER_NATIVE_SERVER", env)
        self.assertEqual(env["HOME"], "/private/native-profile")
        self.assertEqual(env["XDG_CONFIG_HOME"], "/private/native-profile/.config")


if __name__ == "__main__":
    unittest.main()
