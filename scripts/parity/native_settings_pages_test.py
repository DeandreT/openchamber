"""Detector and cleanup ordering checks; these mocks provide no runtime proof."""

from contextlib import ExitStack, nullcontext
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch

import native_settings_pages as driver


class SettingsDriverTests(unittest.TestCase):
    def qualify(self, *, offscreen=False, stuck=False, scale=1.25, fail_search=False,
                capture_fails=False, cleanup_fails=False, fail_relaunch=False,
                workspace_height=800, wheel_distance=100, fail_launch=None,
                fail_constructor=False):
        events = []
        saved = dict(driver.INITIAL)
        checked = {"chat-prompt-navigator": True, "sessions-work-enabled": True,
                   "sessions-show-chats": True, "sessions-deletion-dialog": True}
        top = [900 if offscreen else 450]
        proxy = SimpleNamespace(url="http://fixture", stop=Mock(), settings_requests=[])
        fixture = SimpleNamespace(url="http://fixture", project=Path("/fixture"),
                                  create_session=Mock(), get=lambda _: saved)
        clients = []

        def rect(x, y, width, height):
            return SimpleNamespace(x=round(x * scale), y=round(y * scale),
                                   width=round(width * scale), height=round(height * scale))

        def bounds(target):
            if target == "workspace":
                return rect(0, 0, 1000, workspace_height)
            if target == "settings-dialog":
                return rect(40, 40, 800, 500)
            if target == "sessions-deletion-dialog":
                return rect(400, top[0], 300, 30)
            return rect(400, 150, 300, 30)

        def find(target):
            node = Mock()
            node.get_accessible_id.return_value = target
            node.get_name.return_value = target
            node.get_role_name.return_value = "check box"
            node.get_extents.side_effect = lambda _: bounds(target)
            # SHOWING deliberately remains true for the offscreen control.
            node.get_state_set.return_value.contains.side_effect = lambda state: state in ("SHOWING", "FOCUSED")
            return node

        def make_client(*_, **__):
            index = len(clients)
            if fail_constructor and index == 1:
                raise RuntimeError("original constructor failure")
            process = SimpleNamespace(pid=100 + index, poll=Mock(return_value=None))
            client = Mock(process=None, substitutions=[])
            client._find.side_effect = find

            def launch():
                if index == 1 and fail_launch == "before-process":
                    raise RuntimeError("original launch failure")
                client.process = process
                if index == 1 and fail_launch == "live-process":
                    raise RuntimeError("original launch failure")

            client.launch.side_effect = launch

            def walk():
                if client.process is None:
                    raise RuntimeError("client not started")
                return iter([find("sessions-deletion-dialog")])

            client._walk.side_effect = walk

            def point(node):
                box = bounds(node.get_accessible_id())
                return 100 + box.x + box.width // 2, 200 + box.y + box.height // 2

            client._screen_point.side_effect = point

            def close():
                events.append(("client-close", index))
                events.append("close")
                if client.process is not None and not cleanup_fails:
                    process.poll.return_value = 0

            client.close.side_effect = close

            def screenshot(path):
                if path.name == "failed.png":
                    events.append(("evidence-client", index))
                    if client.process is None:
                        raise RuntimeError("no process to capture")
                    events.append("failure-capture" if process.poll() is None else "capture-after-close")
                    if capture_fails or process.poll() is not None:
                        raise RuntimeError("screenshot unavailable")

            client.screenshot.side_effect = screenshot

            def save(key, value):
                saved[key] = value
                proxy.settings_requests.append({"method": "PUT", "body": {key: value}})

            def click(target):
                events.append(("click", target))
                if target == "sessions-deletion-dialog":
                    box, viewport = bounds(target), bounds("settings-dialog")
                    bottom = min(viewport.y + viewport.height, bounds("workspace").height)
                    if box.y < viewport.y or box.y + box.height > bottom:
                        raise RuntimeError("deletion clicked outside the visible content")
                    save("showDeletionDialog", False)
                    checked[target] = False
                elif target == "chat-prompt-navigator":
                    checked[target] = False
                    save("promptNavigatorEnabled", False)
                elif target == "chat.follow-up-behavior-steer":
                    save("followUpBehavior", "steer")
                elif target == "sessions.permission-default-auto":
                    save("permissionDefaultMode", "auto")
                elif target == "sessions-work-enabled":
                    checked[target] = False
                elif target == "sessions-show-chats":
                    checked[target] = False
                    save("sidebarShowChatsSection", False)

            client.click.side_effect = click
            client.activate.side_effect = lambda target: save("sessionWorkEnabled", False) if target == "sessions-retry" else None
            clients.append(client)
            return client

        def has_state(_session, target, state):
            if state in ("FOCUSED", "PRESSED"):
                return True
            return checked.get(target, True)

        def wait_id(_session, target, **_):
            if fail_search and target == "settings-result-sessions-permission-default":
                raise RuntimeError("original search failure")
            if fail_relaunch and len(clients) == 2 and target == "sessions-work-enabled":
                raise RuntimeError("original relaunch failure")

        def check(description, read, **_):
            for _ in range(3):
                if read():
                    return
            raise TimeoutError(description)

        def command(_env, *arguments):
            events.append(("command", arguments))
            if arguments == ("xdotool", "click", "5") and not stuck:
                top[0] -= wheel_distance

        api = SimpleNamespace(CoordType=SimpleNamespace(WINDOW="WINDOW"),
                              StateType=SimpleNamespace(FOCUSED="FOCUSED", SHOWING="SHOWING", CHECKED="CHECKED"))
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="settings-driver-test-") as temporary:
            args = SimpleNamespace(output=Path(temporary) / "run", binary=Path(temporary) / "unused", scale=scale)
            patches = (
                patch.object(driver.virtual_display, "verify_owned_display"),
                patch.object(driver, "binary_info", return_value={"candidate": "unchanged"}),
                patch.object(driver, "accessibility_enabled", return_value=False),
                patch.object(driver, "set_accessibility"),
                patch.object(driver.traceback, "print_exc"),
                patch.object(driver, "Interrupts", return_value=nullcontext()),
                patch.object(driver, "HostFixture", return_value=nullcontext(fixture)),
                patch.object(driver, "put"),
                patch.object(driver, "SettingsFaults", return_value=proxy),
                patch.object(driver.runner, "native_env", return_value={}),
                patch.object(driver.runner, "state_dirs", return_value={}),
                patch.object(driver, "NativeClient", side_effect=make_client),
                patch.object(driver.Session, "has_state", has_state),
                patch.object(driver.Session, "wait_id", wait_id),
                patch.object(driver.Session, "wait_gone"),
                patch.object(driver.Session, "wait_label"),
                patch.object(driver.Session, "type"),
                patch.object(driver.Session, "keys"),
                patch.object(driver, "until", side_effect=check),
                patch.object(driver, "wait_for", side_effect=check, create=True),
                patch.object(driver, "command", side_effect=command, create=True),
                patch.object(driver, "_atspi", return_value=api, create=True),
            )
            with ExitStack() as cleanup:
                for replacement in patches:
                    cleanup.enter_context(replacement)
                status = driver.qualify(args)
            report = json.loads((args.output / "report.json").read_text())
        return status, report, events, saved

    def test_offscreen_deletion_scrolls_content_before_its_single_click(self):
        for scale in (1, 1.25, 2):
            with self.subTest(scale=scale):
                status, report, events, saved = self.qualify(offscreen=True, scale=scale)
                self.assertEqual(status, 0, report)
                wheels = [event for event in events if event == ("command", ("xdotool", "click", "5"))]
                self.assertEqual(len(wheels), 4)
                self.assertEqual(events.count(("click", "sessions-deletion-dialog")), 1)
                self.assertLess(events.index(wheels[-1]), events.index(("click", "sessions-deletion-dialog")))
                moves = [event[1] for event in events if event[0] == "command" and event[1][1] == "mousemove"]
                self.assertTrue(moves)
                self.assertEqual(moves[0][2:], (
                    str(100 + round(400 * scale) + round(300 * scale) // 2),
                    str(200 + round(40 * scale) + round(500 * scale) // 2)))
                self.assertIs(saved["showDeletionDialog"], False)

    def test_visible_deletion_needs_no_scroll_and_clicks_once(self):
        status, report, events, _ = self.qualify()
        self.assertEqual(status, 0, report)
        self.assertFalse(any(event[0] == "command" for event in events))
        self.assertEqual(events.count(("click", "sessions-deletion-dialog")), 1)

    def test_stuck_scroll_fails_without_clicking_the_offscreen_control(self):
        status, report, events, _ = self.qualify(offscreen=True, stuck=True)
        self.assertEqual(status, 1)
        self.assertIn("scroll did not move", report["error"])
        self.assertNotIn(("click", "sessions-deletion-dialog"), events)
        self.assertEqual(events.count(("command", ("xdotool", "click", "5"))), 1)

    def test_moving_scroll_is_still_bounded_without_a_click(self):
        status, report, events, _ = self.qualify(offscreen=True, wheel_distance=1)
        self.assertEqual(status, 1)
        self.assertIn("did not become fully visible", report["error"])
        self.assertNotIn(("click", "sessions-deletion-dialog"), events)
        self.assertEqual(events.count(("command", ("xdotool", "click", "5"))), 20)

    def test_dialog_visibility_also_requires_the_workspace_viewport(self):
        status, report, events, _ = self.qualify(workspace_height=470)
        self.assertEqual(status, 0, report)
        self.assertEqual(events.count(("command", ("xdotool", "click", "5"))), 1)
        self.assertEqual(events.count(("click", "sessions-deletion-dialog")), 1)

    def test_failure_evidence_precedes_close_and_retains_control_state(self):
        status, report, events, _ = self.qualify(fail_search=True)
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original search failure")
        self.assertIn("failure-capture", events)
        self.assertLess(events.index("failure-capture"), events.index("close"))
        evidence = report["failureEvidence"]
        self.assertIsNone(evidence["process"]["exitCode"])
        self.assertEqual(evidence["controls"][0]["id"], "sessions-deletion-dialog")
        self.assertTrue(evidence["controls"][0]["focused"])

    def test_capture_failure_preserves_original_error_and_cleanup(self):
        status, report, events, _ = self.qualify(fail_search=True, capture_fails=True)
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original search failure")
        self.assertIn("close", events)
        self.assertIn("screenshot unavailable", str(report["failureEvidence"]["captureErrors"]))

    def test_relaunch_failure_captures_only_the_newest_client(self):
        status, report, events, _ = self.qualify(fail_relaunch=True)
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original relaunch failure")
        self.assertEqual(report["failureEvidence"]["process"]["pid"], 101)
        self.assertEqual(events.count("failure-capture"), 1)
        self.assertNotIn("capture-after-close", events)

    def test_relaunch_launch_failure_captures_partial_client_before_its_close(self):
        for capture_fails in (False, True):
            with self.subTest(capture_fails=capture_fails):
                status, report, events, _ = self.qualify(
                    fail_launch="live-process", capture_fails=capture_fails)
                self.assertEqual(status, 1)
                self.assertEqual(report["error"], "original launch failure")
                evidence = report["failureEvidence"]
                self.assertEqual(evidence["process"], {"pid": 101, "exitCode": None})
                self.assertTrue(evidence["controls"][0]["focused"])
                self.assertNotIn(("evidence-client", 0), events)
                self.assertLess(events.index(("evidence-client", 1)), events.index(("client-close", 1)))
                self.assertEqual(events.count(("client-close", 0)), 1)
                self.assertEqual(events.count(("client-close", 1)), 1)
                if capture_fails:
                    self.assertIn("screenshot unavailable", str(evidence["captureErrors"]))

    def test_relaunch_launch_failure_before_process_creation_keeps_current_scope(self):
        status, report, events, _ = self.qualify(fail_launch="before-process")
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original launch failure")
        evidence = report["failureEvidence"]
        self.assertIsNone(evidence["process"])
        self.assertIn("no process to capture", str(evidence["captureErrors"]))
        self.assertNotIn(("evidence-client", 0), events)
        self.assertLess(events.index(("evidence-client", 1)), events.index(("client-close", 1)))
        self.assertEqual(events.count(("client-close", 0)), 1)
        self.assertEqual(events.count(("client-close", 1)), 1)

    def test_relaunch_constructor_failure_does_not_revive_first_client_evidence(self):
        status, report, events, _ = self.qualify(fail_constructor=True)
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original constructor failure")
        self.assertNotIn("failureEvidence", report)
        self.assertFalse(any(event[0] == "evidence-client" for event in events))
        self.assertEqual(events.count(("client-close", 0)), 1)

    def test_cleanup_failure_does_not_replace_the_original_error(self):
        status, report, events, _ = self.qualify(fail_search=True, cleanup_fails=True)
        self.assertEqual(status, 1)
        self.assertEqual(report["error"], "original search failure")
        self.assertIn("close", events)
        self.assertIn("survived cleanup", report["cleanupError"])


if __name__ == "__main__":
    unittest.main()
