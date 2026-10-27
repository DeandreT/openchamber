"""Input preconditions and selection checks for the parity runner.

Run from this directory with `python3 -m unittest runner_test`.
"""

import argparse
import itertools
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import runner


class Client:
    def __init__(self, *, busy=False, selects_target=True):
        self.inflight = busy
        self.selects_target = selects_target
        self.clicked = []
        self.selected = None

    def fingerprint(self):
        return "stable"

    def busy(self):
        return self.inflight

    def rows(self):
        return [
            {"key": "session:target", "label": "target", "selected": self.selected == "session:target"},
            {"key": "session:other", "label": "other", "selected": self.selected == "session:other"},
        ]

    def click(self, key):
        self.clicked.append(key)
        self.selected = key if self.selects_target else "session:other"


class RunnerInputTest(unittest.TestCase):
    def setUp(self):
        ticks = itertools.count(step=0.1)
        self.clock = patch.object(runner.time, "monotonic", side_effect=lambda: next(ticks))
        self.sleep = patch.object(runner.time, "sleep")
        self.clock.start()
        self.sleep.start()
        self.addCleanup(self.clock.stop)
        self.addCleanup(self.sleep.stop)

    def test_busy_client_never_receives_pointer_input(self):
        client = Client(busy=True)
        with self.assertRaisesRegex(TimeoutError, "before row selection"):
            runner.perform(client, "native", [
                {"click": {"zone": "session:", "title": "target"}},
            ], {}, Path("unused"), argparse.Namespace())
        self.assertEqual(client.clicked, [])

    def test_another_selected_session_cannot_satisfy_the_wait(self):
        client = Client(selects_target=False)
        client.click("session:target")
        self.assertFalse(runner.wait_selected(client, "session:target", timeout_s=1))

    def test_failed_row_selection_stops_later_input(self):
        client = Client(selects_target=False)
        client.screenshot = Mock()
        client.type_text = Mock()
        with self.assertRaisesRegex(TimeoutError, "required row selection failed"):
            runner.perform(client, "native", [
                {"click": {"zone": "session:", "title": "target"}},
                {"type": "must not be entered"},
            ], {}, Path("unused"), argparse.Namespace())
        client.screenshot.assert_called_once_with(Path("unused/wait-failed.png"))
        client.type_text.assert_not_called()

    def test_the_requested_selected_session_satisfies_the_wait(self):
        client = Client()
        client.click("session:target")
        self.assertTrue(runner.wait_selected(client, "session:target", timeout_s=1))

    def test_control_clicks_use_the_runtime_target(self):
        client = Client()
        calls = []
        client.click_target = calls.append
        runner.perform(client, "native", [{"clickTarget": "menu"}],
            {"menu": {"native": "menu-control"}}, Path("unused"), argparse.Namespace())
        self.assertEqual(calls, ["menu-control"])

    def test_failed_wait_captures_state_and_stops_later_input(self):
        client = Client()
        captures = []
        inputs = []
        client.wait_for = lambda condition, timeout: False
        client.screenshot = captures.append
        client.type_text = inputs.append
        with self.assertRaisesRegex(TimeoutError, "before further input"):
            runner.perform(client, "native", [
                {"wait": {"native": {"elements": ["project-search"]}}},
                {"type": "must not reach the composer"},
            ], {}, Path("unused"), argparse.Namespace())
        self.assertEqual(captures, [Path("unused/wait-failed.png")])
        self.assertEqual(inputs, [])


class ScenarioSettingsTest(unittest.TestCase):
    def test_both_clients_drop_inherited_credentials_and_configuration(self):
        inherited = {"PATH": "/test/bin", "DISPLAY": ":1001", "OPENAI_API_KEY": "private",
                     "OPENCODE_CONFIG_CONTENT": "private", "OPENCHAMBER_NATIVE_PASSWORD": "private"}
        with patch.dict(os.environ, inherited, clear=True):
            native = runner.native_env(Path("state"), "http://localhost:1", "openchamber-dark")
            electron = runner.electron_env(Path("state"), "synthetic-config")
        for env in (native, electron):
            self.assertNotIn("OPENAI_API_KEY", env)
            self.assertNotIn("OPENCHAMBER_NATIVE_PASSWORD", env)
            self.assertEqual(env["DISPLAY"], ":1001")
            self.assertEqual(env["LANG"], "en_US.UTF-8")
        self.assertNotIn("OPENCODE_CONFIG_CONTENT", native)
        self.assertEqual(electron["OPENCODE_CONFIG_CONTENT"], "synthetic-config")

    def test_composer_noise_needs_both_clients_and_protects_candidate_content(self):
        reference = {"composerGradientProtectionComplete": True, "composerGradientRegions": [[0, 0, 100, 50]], "composerGradientProtected": [[0, 0, 1, 1]], "textBoxes": [[5, 5, 10, 10]]}
        candidate = {"composerGradientProtectionComplete": True, "composerGradientRegions": [[10, 10, 100, 50]], "composerGradientProtected": [[25, 20, 10, 5]]}
        regions, protected = runner.composer_gradient_masks(reference, candidate, 1.25)
        self.assertEqual(regions, [[12.5, 12.5, 112.5, 50.0]])
        self.assertIn([31.25, 25.0, 12.5, 6.25], protected)
        self.assertIn([6.25, 6.25, 12.5, 12.5], protected)
        self.assertEqual(runner.composer_gradient_masks(reference, {}, 1), ([], []))
        missing_protection = {key: value for key, value in candidate.items() if key != "composerGradientProtected"}
        self.assertEqual(runner.composer_gradient_masks(reference, missing_protection, 1), ([], []))
        candidate["composerGradientProtectionComplete"] = False
        self.assertEqual(runner.composer_gradient_masks(reference, candidate, 1), ([], []))

    def test_composer_exception_cannot_raise_its_approved_limit(self):
        entry = {"scenario": "shell.startup", "checkpoint": "startup", "region": "blank gradient", "reason": "dither", "decision": "approved", "maxChannelDelta": 2}
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "exceptions.json"
            path.write_text(json.dumps({"composerGradientNoise": [entry]}))
            with patch.object(runner, "EXCEPTIONS", path):
                with self.assertRaisesRegex(ValueError, "one RGB level"):
                    runner.approved_exceptions("composerGradientNoise")
                entry["maxChannelDelta"] = 1
                path.write_text(json.dumps({"composerGradientNoise": [entry]}))
                self.assertEqual(runner.approved_exceptions("composerGradientNoise"), {("shell.startup", "startup")})

    def test_launch_failure_closes_client_and_services(self):
        for kind in ("native", "electron"):
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temporary:
                client = Mock()
                client.launch.side_effect = RuntimeError("launch failed")
                service = Mock(url="http://localhost", opencode_config={})
                args = argparse.Namespace(root=Path(temporary), theme="test", timeout=1,
                    native_binary=Path("native"), electron_app=Path("electron"), scale=1, width=1280, height=800)
                with patch.object(runner, "copy_seed", return_value=Path(temporary)), \
                     patch.object(runner, "native_env", return_value={}), \
                     patch.object(runner, "electron_env", return_value={}), \
                     patch.object(runner, "NativeClient", return_value=client), \
                     patch.object(runner, "ElectronClient", return_value=client), \
                     patch.object(runner, "IsolatedServer", return_value=service), \
                     patch.object(runner, "FixtureProvider", return_value=service):
                    result = runner.run_client(kind, {"steps": []}, Path(temporary), args)
                self.assertEqual(result, {"error": "launch failed"})
                client.close.assert_called_once_with()
                service.stop.assert_called_once_with()

    def test_unknown_click_targets_are_rejected_before_launch(self):
        scenarios = [{"id": "check", "feature": "feature", "steps": [{"clickTarget": "missing"}]}]
        features = [{"id": "feature", "scenarios": ["check"]}]
        self.assertEqual(runner.check_contracts(scenarios, features, {}), ["scenario check references undefined target missing"])

    def test_only_client_generated_draft_ids_are_normalized(self):
        reference = [{"key": "project:root:draft:random-id", "label": "draft", "selected": False, "expanded": None}]
        candidate = [{**reference[0], "key": "project:root:draft:new-session"}]
        self.assertEqual(runner.compare_rows(reference, candidate), [])
        self.assertTrue(runner.compare_rows(reference, []))
        self.assertTrue(runner.compare_rows(reference, [{**candidate[0], "label": "different"}]))
        two_drafts = reference + [{**reference[0], "key": "project:root:draft:another-id"}]
        self.assertEqual(runner.comparable_rows(two_drafts)[1]["key"], "project:root:draft:1")
        session = [{**reference[0], "key": "project:root:session:one"}]
        self.assertTrue(runner.compare_rows(session, [{**session[0], "key": "project:root:session:two"}]))
    def test_settings_are_applied_to_independent_copies_only(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            preferences = root / "seed" / "oc-data" / "preferences.json"
            preferences.parent.mkdir(parents=True)
            original = {"version": 1, "fields": {"sidebarGrouping": {"value": "project", "updatedAt": 1}}}
            preferences.write_text(json.dumps(original))
            first = runner.copy_seed(root, root / "first", "openchamber-dark", {"sidebarGrouping": "time"})
            second = runner.copy_seed(root, root / "second", "openchamber-dark")
            self.assertEqual(json.loads((first / "oc-data" / "preferences.json").read_text())["fields"]["sidebarGrouping"]["value"], "time")
            self.assertEqual(json.loads((second / "oc-data" / "preferences.json").read_text())["fields"]["sidebarGrouping"]["value"], "project")
            self.assertEqual(json.loads(preferences.read_text()), original)


if __name__ == "__main__":
    unittest.main()
