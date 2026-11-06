"""Input preconditions and selection checks for the parity runner.

Run from this directory with `python3 -m unittest runner_test`.
"""

import argparse
import copy
import hashlib
import io
import itertools
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import runner


def declaration_identities():
    return {name: runner.artifact_identity(path) for name, path in (
        ("scenarios", runner.SCENARIOS), ("inventory", runner.INVENTORY),
    )}


class ArtifactIdentityTest(unittest.TestCase):
    def test_binary_digest_is_the_standard_sha256_of_its_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            binary = Path(temporary) / "binary"
            binary.write_bytes(b"synthetic executable")
            self.assertEqual(runner.artifact_identity(binary)["sha256"], hashlib.sha256(binary.read_bytes()).hexdigest())

    def test_tree_digest_uses_case_sensitive_posix_name_order(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            expected = hashlib.sha256()
            for name in ("Z", "a"):
                (root / name).write_bytes(name.encode())
                expected.update(name.encode() + b"\0" + hashlib.sha256(name.encode()).digest())
            identity = runner.artifact_identity(root)
            self.assertEqual(identity["algorithm"], "sha256-tree-v1")
            self.assertEqual(identity["sha256"], expected.hexdigest())

    def test_bytes_and_relative_names_define_the_identity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "first.txt"
            source.write_text("same")
            initial = runner.artifact_identity(root)
            self.assertEqual(initial, runner.artifact_identity(root))
            source.write_text("changed")
            self.assertNotEqual(initial, runner.artifact_identity(root))
            source.write_text("same")
            source.rename(root / "renamed.txt")
            self.assertNotEqual(initial, runner.artifact_identity(root))

    def test_missing_input_cannot_be_an_empty_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            with self.assertRaises(FileNotFoundError):
                runner.artifact_identity(Path(temporary) / "missing")

    def test_existing_run_is_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "seed").mkdir()
            output = root / "previous"
            output.mkdir()
            saved = output / "report.json"
            saved.write_text("previous evidence")
            args = argparse.Namespace(root=root, output=output, scenarios=None)
            self.assertEqual(runner.run(args), 2)
            self.assertEqual(saved.read_text(), "previous evidence")

    def test_unknown_scenario_cannot_succeed_without_running_checks(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "seed").mkdir()
            output = root / "new"
            args = argparse.Namespace(root=root, output=output, scenarios="not-a-scenario")
            self.assertEqual(runner.run(args), 2)
            self.assertFalse(output.exists())

    def test_changed_inputs_block_an_otherwise_passing_run(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "seed").mkdir()
            output = root / "new"
            args = argparse.Namespace(root=root, output=output, scenarios=None)

            def passing(_scenarios, _wanted, _output, report, _args):
                report["scenarios"]["shell.startup"] = {"status": "passed"}
                return 0

            declarations = declaration_identities()
            with patch.object(runner, "input_identities", side_effect=[{**declarations, "input": "before"}, {**declarations, "input": "after"}]):
                with patch.object(runner, "run_scenarios", side_effect=passing):
                    with patch.object(runner.native_adapter, "accessibility_enabled", return_value=False):
                        with patch.object(runner.native_adapter, "set_accessibility"):
                            self.assertEqual(runner.run(args), 1)
            report = json.loads((output / "report.json").read_text())
            self.assertFalse(report["inputsUnchanged"])
            self.assertEqual(report["scenarios"]["shell.startup"]["status"], "blocked")
            self.assertEqual(report["scenarios"]["shell.startup"]["observedStatus"], "passed")

    def test_changes_to_the_launched_electron_package_block_results(self):
        for changed in ("web-dist/index.html", "opencode-cli/opencode"):
            with self.subTest(changed=changed), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                resources = root / "electron/resources"
                files = ("electron/app", "electron/resources/app.asar",
                         "electron/resources/web-dist/index.html", "electron/resources/opencode-cli/opencode",
                         "native", "staged/index.html", "server-opencode", "seed/snapshot")
                for name in files:
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(name)
                args = argparse.Namespace(
                    root=root, output=root / "new", scenarios=None,
                    electron_app=root / "electron/app", native_binary=root / "native",
                    dist=root / "staged", opencode_binary=root / "server-opencode",
                )

                def passing(_scenarios, _wanted, _output, report, _args):
                    report["scenarios"]["shell.startup"] = {"status": "passed"}
                    (resources / changed).write_text("replacement reference")
                    return 0

                with patch.object(runner, "run_scenarios", side_effect=passing):
                    with patch.object(runner.native_adapter, "accessibility_enabled", return_value=False):
                        with patch.object(runner.native_adapter, "set_accessibility"):
                            self.assertEqual(runner.run(args), 1)
                report = json.loads((args.output / "report.json").read_text())
                self.assertFalse(report["inputsUnchanged"])
                self.assertEqual(report["scenarios"]["shell.startup"]["status"], "blocked")


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
    def test_electron_uses_simple_gtk_without_changing_native_ime_setup(self):
        with patch.dict(os.environ, {"GTK_IM_MODULE": "ibus"}, clear=True):
            electron = runner.electron_env(Path("state"), "synthetic-config")
            native = runner.native_env(Path("state"), "http://localhost:1", "openchamber-dark")
        self.assertEqual(electron["GTK_IM_MODULE"], "gtk-im-context-simple")
        self.assertNotIn("GTK_IM_MODULE", native)

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
        features = [{"id": "feature", "scenarios": ["check"], "coverage": "automated",
                     "requiredBehaviors": ["activation"], "states": ["open"]}]
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


class CompleteInventoryGateTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        for name in ("electron/app", "electron/resources/app.asar",
                     "electron/resources/web-dist/index.html", "electron/resources/opencode-cli/opencode",
                     "native", "staged/index.html", "server-opencode", "seed/snapshot"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name)
        self.scenarios_file = self.root / "scenarios.json"
        self.inventory_file = self.root / "inventory.json"
        self.exceptions_file = self.root / "exceptions.json"
        self.exceptions_file.write_text("{}")
        self.features = [
            {"id": "menu", "coverage": "automated", "scenarios": ["menu.open", "menu.result"],
             "requiredBehaviors": ["activation", "outcome"], "states": ["open", "failed"]},
            {"id": "editor", "coverage": "automated", "scenarios": ["editor.save"],
             "requiredBehaviors": ["save"], "states": ["restored"]},
        ]
        self.scenarios = [
            {"id": "menu.open", "feature": "menu", "steps": [{"capture": "open"}],
             "proofs": {"menu": {"behaviors": ["activation"], "states": ["open"]}}},
            {"id": "menu.result", "feature": "menu", "steps": [{"capture": "result"}],
             "proofs": {"menu": {"behaviors": ["outcome"], "states": ["failed"]}}},
            {"id": "editor.save", "feature": "editor", "steps": [{"capture": "saved"}],
             "proofs": {"editor": {"behaviors": ["save"], "states": ["restored"]}}},
        ]
        self.statuses = {}
        self.executed = []
        self.after_run = lambda: None
        self.scenario_executor = self.simulated_scenarios
        self.run_number = 0

    def simulated_scenarios(self, scenarios, wanted, output, report, args):
        for scenario in scenarios:
            if wanted and scenario["id"] not in wanted:
                continue
            self.executed.append(scenario["id"])
            report["scenarios"][scenario["id"]] = {"status": self.statuses.get(scenario["id"], "passed")}
        self.after_run()
        return sum(result["status"] != "passed" for result in report["scenarios"].values())

    def run_fixture(self, *, require_complete=False, selected=None, cli=False):
        self.scenarios_file.write_text(json.dumps({"scenarios": self.scenarios, "targets": {}}))
        self.inventory_file.write_text(json.dumps({"features": self.features}))
        self.run_number += 1
        self.executed = []
        args = argparse.Namespace(
            root=self.root, output=self.root / f"result-{self.run_number}", scenarios=selected,
            require_complete_inventory=require_complete,
            electron_app=self.root / "electron/app", native_binary=self.root / "native",
            dist=self.root / "staged", opencode_binary=self.root / "server-opencode",
            scale=1.0, tolerance=0,
        )
        stdout, stderr = io.StringIO(), io.StringIO()
        with patch.object(runner, "SCENARIOS", self.scenarios_file), \
             patch.object(runner, "INVENTORY", self.inventory_file), \
             patch.object(runner, "EXCEPTIONS", self.exceptions_file), \
             patch.object(runner, "run_scenarios", side_effect=self.scenario_executor), \
             patch.object(runner.native_adapter, "accessibility_enabled", return_value=False), \
             patch.object(runner.native_adapter, "set_accessibility"), \
             patch.object(runner.sys, "stdout", stdout), patch.object(runner.sys, "stderr", stderr):
            if cli:
                argv = ["runner.py", "run"]
                for flag, value in (("--root", args.root), ("--output", args.output),
                                    ("--electron-app", args.electron_app), ("--native-binary", args.native_binary),
                                    ("--dist", args.dist), ("--opencode-binary", args.opencode_binary)):
                    argv.extend((flag, str(value)))
                if require_complete:
                    argv.append("--require-complete-inventory")
                if selected is not None:
                    argv.extend(("--scenarios", selected))
                with patch.object(runner.sys, "argv", argv), patch.object(runner.virtual_display, "inside", return_value=True):
                    code = runner.main()
            else:
                code = runner.run(args)
        report_path = args.output / "report.json"
        report = json.loads(report_path.read_text()) if report_path.exists() else None
        return code, report, stdout.getvalue(), stderr.getvalue()

    def test_complete_feature_proof_unions_pass_the_cli_gate(self):
        code, report, _, stderr = self.run_fixture(require_complete=True, cli=True)
        self.assertEqual(code, 0)
        self.assertEqual(report["inventory"]["byFeature"], {"menu": "passed", "editor": "passed"})
        self.assertEqual(report["inventory"]["passed"], 2)
        self.assertTrue(report["inputsUnchanged"])
        self.assertIn("inventory", report["inputs"])
        self.assertNotIn("complete inventory required", stderr)

    def test_missing_behavior_or_state_fails_only_the_opt_in_cli_gate(self):
        proof = self.scenarios[1]["proofs"]["menu"]
        for field in ("behaviors", "states"):
            with self.subTest(field=field):
                missing = proof[field].pop()
                code, _, _, stderr = self.run_fixture(cli=True)
                self.assertEqual(code, 0, "focused checks retain their existing exit contract")
                self.assertNotIn("complete inventory required", stderr)
                code, report, _, stderr = self.run_fixture(require_complete=True, cli=True)
                self.assertEqual(code, 1)
                self.assertEqual(report["inventory"]["missingRequirements"]["menu"][field], [missing])
                self.assertIn("menu: incomplete", stderr)
                self.assertIn(f"missing {field}: {missing}", stderr)
                proof[field].append(missing)

    def test_focused_selection_does_not_shrink_the_required_inventory(self):
        selected = "menu.open,menu.result"
        self.assertEqual(self.run_fixture(selected=selected)[0], 0)
        code, report, _, stderr = self.run_fixture(require_complete=True, selected=selected)
        self.assertEqual(code, 1)
        self.assertEqual(self.executed, ["menu.open", "menu.result"])
        self.assertEqual(report["inventory"]["byFeature"], {"menu": "passed", "editor": "not run"})
        self.assertEqual(report["inventory"]["missingRequirements"]["editor"], {
            "behaviors": ["save"], "states": ["restored"],
        })
        self.assertIn("editor: not run", stderr)

    def test_nonpassing_results_never_grant_gate_proofs(self):
        for status in ("failed", "blocked", "incomplete", "skipped", "not run"):
            with self.subTest(status=status):
                self.statuses = {"menu.result": status}
                self.assertEqual(self.run_fixture()[0], 1)
                code, report, _, _ = self.run_fixture(require_complete=True)
                self.assertEqual(code, 1)
                self.assertNotEqual(report["inventory"]["byFeature"]["menu"], "passed")
                self.assertEqual(report["inventory"]["missingRequirements"]["menu"], {
                    "behaviors": ["outcome"], "states": ["failed"],
                })

    def test_nonpassing_linked_scenario_blocks_even_when_other_proofs_cover_everything(self):
        self.features[0]["scenarios"].append("menu.extra")
        self.scenarios.append({"id": "menu.extra", "feature": "menu", "steps": [], "proofs": {}})
        for status in ("failed", "blocked", "incomplete", "skipped"):
            with self.subTest(status=status):
                self.statuses = {"menu.extra": status}
                code, report, _, _ = self.run_fixture(require_complete=True)
                self.assertEqual(code, 1)
                self.assertNotEqual(report["inventory"]["byFeature"]["menu"], "passed")
                self.assertEqual(report["inventory"]["missingRequirements"]["menu"], {"behaviors": [], "states": []})

    def test_pointer_substitution_leaves_the_scenario_and_inventory_unqualified(self):
        self.features = [{"id": "menu", "coverage": "automated", "scenarios": ["menu.open"],
                          "requiredBehaviors": ["activation"], "states": ["open"]}]
        self.scenarios = self.scenarios[:1]
        self.scenario_executor = runner.run_scenarios

        def captured_client(kind, scenario, work, args):
            checkpoint = {"image": "unused.png", "rows": [], "targets": {}, "waits": [], "settled": True}
            if kind == "native":
                checkpoint["substitutions"] = [{"target": "menu", "input": "accessibility instead of pointer"}]
            return {"checkpoints": {"open": checkpoint}}

        image = Mock()
        image.as_dict.return_value = {
            "sizeMatches": True, "differingPixels": 0, "differingShare": 0,
            "glyphEdgePixels": 0, "paintNoisePixels": 0, "composerGradientNoisePixels": 0,
        }
        with patch.object(runner, "run_client", side_effect=captured_client), \
             patch.object(runner.compare, "compare_images", return_value=image):
            code, report, _, _ = self.run_fixture(require_complete=True)
        self.assertEqual(code, 1)
        self.assertEqual(report["scenarios"]["menu.open"]["status"], "incomplete")
        self.assertEqual(report["inventory"]["passed"], 0)
        self.assertEqual(report["inventory"]["missingRequirements"]["menu"], {
            "behaviors": ["activation"], "states": ["open"],
        })

    def test_manual_classification_cannot_be_promoted_by_automated_proofs(self):
        self.features[1].update(coverage="manual", manualCheck="Observe the real supported host.")
        self.assertEqual(self.run_fixture()[0], 0)
        code, report, _, stderr = self.run_fixture(require_complete=True)
        self.assertEqual(code, 1)
        self.assertEqual(report["inventory"]["byFeature"]["editor"], "manual")
        self.assertIn("editor: manual", stderr)

    def test_empty_inventory_cannot_qualify_a_replacement(self):
        self.scenarios, self.features = [], []
        self.assertEqual(self.run_fixture()[0], 0)
        code, _, _, stderr = self.run_fixture(require_complete=True)
        self.assertEqual(code, 1)
        self.assertIn("inventory has no features", stderr)

    def test_unknown_selection_and_invalid_proofs_still_fail_before_execution(self):
        code, report, _, stderr = self.run_fixture(require_complete=True, selected="unknown", cli=True)
        self.assertEqual(code, 2)
        self.assertIsNone(report)
        self.assertEqual(self.executed, [])
        self.assertIn("unknown parity scenarios: unknown", stderr)
        self.scenarios[0]["proofs"]["menu"]["behaviors"].append("invented")
        code, report, _, stderr = self.run_fixture(require_complete=True)
        self.assertEqual(code, 2)
        self.assertIsNone(report)
        self.assertEqual(self.executed, [])
        self.assertIn("unknown behaviors menu:invented", stderr)

    def test_declarations_changed_before_initial_fingerprint_cannot_start_a_run(self):
        identify = runner.input_identities
        for changed in (self.inventory_file, self.scenarios_file):
            with self.subTest(changed=changed.name):
                def replace_before_fingerprint(args):
                    changed.write_text("replaced declarations")
                    return identify(args)

                with patch.object(runner, "input_identities", side_effect=replace_before_fingerprint):
                    code, report, _, stderr = self.run_fixture(require_complete=True)
                self.assertEqual(code, 2)
                self.assertIsNone(report)
                self.assertEqual(self.executed, [])
                self.assertIn("declarations changed before the run", stderr)

    def test_changed_inventory_scenarios_or_binary_block_all_previously_passed_proofs(self):
        for changed in (self.inventory_file, self.scenarios_file, self.root / "native"):
            with self.subTest(changed=changed.name):
                self.after_run = lambda: changed.write_text("replaced input")
                code, report, _, _ = self.run_fixture(require_complete=True)
                self.assertEqual(code, 1)
                self.assertFalse(report["inputsUnchanged"])
                self.assertTrue(all(result["status"] == "blocked" for result in report["scenarios"].values()))
                self.assertEqual(report["inventory"]["passed"], 0)
                self.assertEqual(report["inventory"]["missingRequirements"]["menu"], {
                    "behaviors": ["activation", "outcome"], "states": ["open", "failed"],
                })

    def test_final_fingerprint_failure_blocks_complete_proofs(self):
        identify = runner.input_identities
        calls = 0

        def unavailable_at_finish(args):
            nonlocal calls
            calls += 1
            if calls == 2:
                raise OSError("comparison input unavailable")
            return identify(args)

        with patch.object(runner, "input_identities", side_effect=unavailable_at_finish):
            code, report, _, _ = self.run_fixture(require_complete=True)
        self.assertEqual(code, 1)
        self.assertFalse(report["inputsUnchanged"])
        self.assertEqual(report["inventory"]["passed"], 0)

    def test_declarations_replaced_after_final_fingerprint_cannot_rewrite_executed_proofs(self):
        self.scenarios[1]["proofs"]["menu"]["behaviors"] = []
        identify = runner.input_identities
        for changed in ("scenarios", "inventory"):
            with self.subTest(changed=changed):
                calls = 0

                def replace_after_fingerprint(args):
                    nonlocal calls
                    calls += 1
                    identities = identify(args)
                    if calls == 2:
                        if changed == "scenarios":
                            replacement = copy.deepcopy(self.scenarios)
                            replacement[1]["proofs"]["menu"]["behaviors"] = ["outcome"]
                            self.scenarios_file.write_text(json.dumps({"scenarios": replacement, "targets": {}}))
                        else:
                            replacement = copy.deepcopy(self.features)
                            replacement[0]["requiredBehaviors"] = ["activation"]
                            self.inventory_file.write_text(json.dumps({"features": replacement}))
                    return identities

                with patch.object(runner, "input_identities", side_effect=replace_after_fingerprint):
                    code, report, _, _ = self.run_fixture(require_complete=True)
                self.assertEqual(code, 1)
                self.assertTrue(report["inputsUnchanged"])
                self.assertEqual(report["inventory"]["byFeature"]["menu"], "incomplete")
                self.assertEqual(report["inventory"]["missingRequirements"]["menu"]["behaviors"], ["outcome"])

    def test_diagnostics_bound_feature_and_requirement_details(self):
        features = [{"id": f"feature-{index}", "coverage": "automated", "scenarios": [],
                     "requiredBehaviors": [f"behavior-{number}-" + "x" * 200 for number in range(20)],
                     "states": [f"state-{number}" for number in range(20)]} for index in range(20)]
        coverage = runner.inventory_coverage(features, {}, [])
        stderr = io.StringIO()
        with patch.object(runner.sys, "stderr", stderr):
            runner.print_inventory_gaps(coverage)
        text = stderr.getvalue()
        self.assertIn("feature-0: not run", text)
        self.assertIn("feature-7: not run", text)
        self.assertNotIn("feature-8:", text)
        self.assertIn("12 more features", text)
        self.assertIn("+17 more", text)
        self.assertIn("report.json", text)
        self.assertLess(len(text), 4000)


class InventoryCoverageTest(unittest.TestCase):
    def setUp(self):
        self.feature = {
            "id": "menu", "coverage": "automated", "scenarios": ["startup", "activate"],
            "requiredBehaviors": ["appearance", "activation", "outcome"],
            "states": ["closed", "open", "failed"],
        }
        self.scenarios = [
            {"id": "startup", "feature": "menu", "steps": [{"capture": "startup"}],
             "proofs": {"menu": {"behaviors": ["appearance"], "states": ["closed"]}}},
            {"id": "activate", "feature": "menu", "steps": [],
             "proofs": {"menu": {"behaviors": ["activation", "outcome"], "states": ["open", "failed"]}}},
        ]

    def coverage(self, results):
        return runner.inventory_coverage([self.feature], results, self.scenarios)

    def test_startup_only_cannot_complete_an_interaction_feature(self):
        self.feature["scenarios"] = ["startup"]
        report = self.coverage({"startup": {"status": "passed"}})
        self.assertEqual(report["byFeature"]["menu"], "incomplete")
        self.assertEqual(report["missingRequirements"]["menu"], {
            "behaviors": ["activation", "outcome"], "states": ["open", "failed"],
        })

    def test_run_uses_executed_proofs_if_the_disk_contract_changes_after_fingerprinting(self):
        self.feature["scenarios"] = ["startup"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "seed").mkdir()
            scenarios_file, inventory_file = root / "scenarios.json", root / "inventory.json"
            scenarios_file.write_text(json.dumps({"scenarios": self.scenarios[:1], "targets": {}}))
            inventory_file.write_text(json.dumps({"features": [self.feature]}))
            args = argparse.Namespace(root=root, output=root / "result", scenarios=None)

            def passing(_scenarios, _wanted, _output, report, _args):
                report["scenarios"]["startup"] = {"status": "passed"}
                return 0

            def replace_after_read():
                identities = declaration_identities()
                replacement = json.loads(scenarios_file.read_text())
                replacement["scenarios"][0]["proofs"]["menu"] = {
                    "behaviors": self.feature["requiredBehaviors"], "states": self.feature["states"],
                }
                scenarios_file.write_text(json.dumps(replacement))
                return identities

            reads = iter((False, True))

            def read_inputs(_args):
                return replace_after_read() if next(reads) else declaration_identities()

            with patch.object(runner, "SCENARIOS", scenarios_file), patch.object(runner, "INVENTORY", inventory_file):
                with patch.object(runner, "input_identities", side_effect=read_inputs):
                    with patch.object(runner, "run_scenarios", side_effect=passing):
                        with patch.object(runner.native_adapter, "accessibility_enabled", return_value=False):
                            with patch.object(runner.native_adapter, "set_accessibility"):
                                self.assertEqual(runner.run(args), 0)
            report = json.loads((args.output / "report.json").read_text())
            self.assertTrue(report["inputsUnchanged"])
            self.assertEqual(report["inventory"]["byFeature"]["menu"], "incomplete")
            self.assertIn("activation", report["inventory"]["missingRequirements"]["menu"]["behaviors"])

    def test_known_but_unproved_state_or_behavior_prevents_passing(self):
        for field in ("states", "behaviors"):
            with self.subTest(field=field):
                proof = self.scenarios[1]["proofs"]["menu"]
                value = proof[field].pop()
                report = self.coverage({name: {"status": "passed"} for name in ("startup", "activate")})
                self.assertEqual(report["byFeature"]["menu"], "incomplete")
                self.assertEqual(report["missingRequirements"]["menu"][field], [value])
                proof[field].append(value)

    def test_completed_proof_union_qualifies(self):
        report = self.coverage({name: {"status": "passed"} for name in ("startup", "activate")})
        self.assertEqual(report["byFeature"]["menu"], "passed")
        self.assertEqual(report["missingRequirements"]["menu"], {"behaviors": [], "states": []})
        self.assertEqual(report["passed"], 1)

    def test_incomplete_failed_blocked_and_skipped_runs_grant_no_proof(self):
        for status, expected in (("incomplete", "incomplete"), ("failed", "failed"),
                                 ("blocked", "failed"), ("not run", "incomplete")):
            with self.subTest(status=status):
                report = self.coverage({"startup": {"status": "passed"}, "activate": {"status": status}})
                self.assertEqual(report["byFeature"]["menu"], expected)
                self.assertIn("outcome", report["missingRequirements"]["menu"]["behaviors"])

    def test_no_scenario_is_not_run_not_manual(self):
        self.feature["scenarios"] = []
        report = self.coverage({})
        self.assertEqual(report["byFeature"]["menu"], "not run")
        self.assertEqual(report["notRun"], 1)
        self.assertEqual(report["manual"], 0)
        self.assertEqual(report["missingRequirements"]["menu"]["states"], self.feature["states"])

    def test_truly_manual_checks_remain_manual(self):
        self.feature.update(coverage="manual", manualCheck="Observe the screen reader on the supported host.")
        report = self.coverage({name: {"status": "passed"} for name in ("startup", "activate")})
        self.assertEqual(report["byFeature"]["menu"], "manual")
        self.assertEqual(report["passed"], 0)
        self.feature["scenarios"] = []
        report = self.coverage({})
        self.assertEqual(report["byFeature"]["menu"], "manual")
        self.assertEqual(report["missingRequirements"]["menu"]["behaviors"], self.feature["requiredBehaviors"])

    def test_missing_proof_declarations_never_grant_implicit_credit(self):
        self.scenarios[1].pop("proofs")
        report = self.coverage({name: {"status": "passed"} for name in ("startup", "activate")})
        self.assertEqual(report["byFeature"]["menu"], "incomplete")
        self.assertIn("outcome", report["missingRequirements"]["menu"]["behaviors"])

    def test_unknown_or_unlinked_result_cannot_prove_requirements(self):
        self.feature["scenarios"] = ["startup"]
        report = self.coverage({name: {"status": "passed"} for name in ("startup", "activate", "unknown")})
        self.assertEqual(report["byFeature"]["menu"], "incomplete")
        self.assertIn("outcome", report["missingRequirements"]["menu"]["behaviors"])

    def test_contract_rejects_unknown_proof_and_missing_requirements(self):
        self.scenarios[0]["proofs"]["menu"]["states"].append("typo")
        self.scenarios[0]["proofs"]["menu"]["behaviors"].append("invented")
        problems = runner.check_contracts(self.scenarios, [self.feature], {})
        self.assertIn("scenario startup proves unknown states menu:typo", problems)
        self.assertIn("scenario startup proves unknown behaviors menu:invented", problems)
        self.feature.pop("requiredBehaviors")
        self.assertIn("feature menu needs nonempty requiredBehaviors",
                      runner.check_contracts(self.scenarios, [self.feature], {}))

    def test_contract_rejects_unlinked_proof_and_unexplained_manual_classification(self):
        self.feature.update(scenarios=[], coverage="manual")
        problems = runner.check_contracts(self.scenarios, [self.feature], {})
        self.assertIn("scenario startup proof is not linked by feature menu", problems)
        self.assertIn("feature menu needs a manualCheck procedure", problems)

    def test_repository_contract_is_valid_and_startup_leaves_workflows_uncovered(self):
        features = json.loads(runner.INVENTORY.read_text())["features"]
        definitions = json.loads(runner.SCENARIOS.read_text())
        self.assertEqual(runner.check_contracts(definitions["scenarios"], features, definitions["targets"]), [])
        report = runner.inventory_coverage(features, {"shell.startup": {"status": "passed"}}, definitions["scenarios"])
        for feature in ("shell.header", "shell.sidebar-toolbar", "shell.sidebar-footer", "shell.right-rail", "sessions.ordering"):
            self.assertEqual(report["byFeature"][feature], "incomplete")
            self.assertTrue(report["missingRequirements"][feature]["behaviors"])
        self.assertEqual(report["byFeature"]["runtime.connection-states"], "not run")
        self.assertEqual(report["byFeature"]["browser-terminal.terminal"], "not run")
        self.assertEqual(report["byFeature"]["runtime.platform-manual"], "manual")
        self.assertEqual(report["passed"], 0)


if __name__ == "__main__":
    unittest.main()
