"""Normal-close qualification and cleanup preserve the native child contract."""

from contextlib import ExitStack
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, call, patch

import native_knowledge_schedules as driver


class SessionCleanupTests(unittest.TestCase):
    def run_session(self, process):
        client = SimpleNamespace(
            process=process, close=Mock(), launch=Mock(), wait_ready=Mock()
        )
        server = SimpleNamespace(url="http://127.0.0.1:12345", stop=Mock(), wait_ready=Mock())
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="knowledge-cleanup-test-") as root:
            args = SimpleNamespace(output=Path(root), binary=Path(root) / "unused", scale=1)
            with (
                patch.object(driver.runner, "IsolatedServer", return_value=server),
                patch.object(driver.runner, "state_dirs", return_value={}),
                patch.object(driver.runner, "native_env", return_value={}),
                patch.object(driver, "NativeClient", return_value=client),
                patch.object(driver, "request", return_value={
                    "projects": [{"id": "project", "path": "/work/project"}]
                }),
                ExitStack() as cleanup,
            ):
                driver.Session(args, cleanup)
        server.stop.assert_called_once_with()
        return client

    def test_an_unstarted_client_has_nothing_to_stop(self):
        client = self.run_session(None)
        client.close.assert_not_called()

    def test_a_reaped_child_is_not_signalled_again(self):
        client = self.run_session(SimpleNamespace(poll=Mock(return_value=0)))
        client.close.assert_not_called()

    def test_a_live_child_keeps_the_existing_owned_cleanup(self):
        client = self.run_session(SimpleNamespace(poll=Mock(return_value=None)))
        client.close.assert_called_once_with()


class CloseQualificationTests(unittest.TestCase):
    def run_close(self, exit_code):
        client = SimpleNamespace(click=Mock(), process=SimpleNamespace(wait=Mock(return_value=exit_code)))
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="knowledge-close-test-") as root:
            args = SimpleNamespace(output=Path(root) / "run", binary=Path(root) / "unused", scale=1)
            with (
                patch.object(driver.virtual_display, "verify_owned_display"),
                patch.object(driver, "accessibility_enabled", return_value=False),
                patch.object(driver, "set_accessibility"),
                patch.object(driver, "Session", return_value=SimpleNamespace(client=client)),
                patch.object(driver, "steps", return_value=[]),
                patch.object(driver, "binary_info", return_value={"fixture": True}),
            ):
                status = driver.run_checks(args)
            report = json.loads((args.output / "report.json").read_text())
        return status, report["checks"][-1]

    def test_fast_nonzero_exit_fails_qualification(self):
        status, close = self.run_close(1)
        self.assertEqual(status, 1, "a fast native crash cannot pass normal-close qualification")
        self.assertFalse(close["passed"])
        self.assertEqual(close["exitCode"], 1)

    def test_clean_normal_exit_passes_qualification(self):
        status, close = self.run_close(0)
        self.assertEqual(status, 0)
        self.assertTrue(close["passed"])
        self.assertEqual(close["exitCode"], 0)


class NameReadinessTests(unittest.TestCase):
    def create_until_defaults(self, *, focused=True, typed_text=driver.TASK_NAME, lose_focus=False):
        node = Mock()
        state = {"focused": focused, "text": ""}
        node.get_state_set.return_value.contains.side_effect = lambda _: state["focused"]
        client = Mock()
        client._find.return_value = node
        # A different focused entry must not satisfy the task-name assertion.
        client.focused_text.return_value = driver.TASK_NAME
        session = Mock(client=client)

        def type_text(value):
            state.update(text=typed_text, focused=focused and not lose_focus)

        session.type.side_effect = type_text

        def click(target):
            if target == "scheduled-task-use-defaults":
                raise StopIteration("reached defaults")

        client.click.side_effect = click

        def check_once(description, read, **_):
            if not read():
                raise TimeoutError(description)

        create = dict(driver.steps(session))["schedule-created-in-zone"]
        error = None
        with (
            patch.object(driver, "_atspi", create=True),
            patch.object(driver, "editable_text", side_effect=lambda _: state["text"], create=True),
            patch.object(driver, "wait_for", side_effect=check_once),
        ):
            try:
                create()
            except (TimeoutError, StopIteration) as caught:
                error = caught
        return session, node, error

    def assert_halted_before_defaults(self, session, error):
        self.assertIsInstance(error, TimeoutError)
        session.click_node.assert_called_once_with("scheduled-task-name")
        self.assertNotIn(call("scheduled-task-use-defaults"), session.client.click.call_args_list)
        self.assertNotIn(call("scheduled-task-save"), session.client.click.call_args_list)

    def test_wrong_entry_focus_stops_before_typing_or_defaults(self):
        session, node, error = self.create_until_defaults(focused=False)
        self.assert_halted_before_defaults(session, error)
        session.type.assert_not_called()
        node.clear_cache.assert_called()

    def test_wrong_name_text_stops_before_defaults(self):
        session, _, error = self.create_until_defaults(typed_text="Nightly")
        self.assert_halted_before_defaults(session, error)
        session.type.assert_called_once_with(driver.TASK_NAME)

    def test_focus_lost_after_typing_stops_even_with_exact_text(self):
        session, _, error = self.create_until_defaults(lose_focus=True)
        self.assert_halted_before_defaults(session, error)
        session.type.assert_called_once_with(driver.TASK_NAME)

    def test_fresh_exact_name_and_focus_allow_defaults_after_one_click_and_type(self):
        session, node, error = self.create_until_defaults()
        self.assertIsInstance(error, StopIteration)
        session.click_node.assert_called_once_with("scheduled-task-name")
        session.type.assert_called_once_with(driver.TASK_NAME)
        self.assertGreaterEqual(node.clear_cache.call_count, 2)
        session.client._find.assert_called_with("scheduled-task-name")


if __name__ == "__main__":
    unittest.main()
