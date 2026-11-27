"""Panel declarations and outcome checks. These tests grant no GUI parity proof."""

import argparse
import base64
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

import runner
import steps


KNOWLEDGE = "panels.knowledge-plans-todos"
SCHEDULES = "panels.schedules"
CASES = {
    "panels.knowledge.note-edit-search": KNOWLEDGE,
    "panels.knowledge.todo-complete-reopen": KNOWLEDGE,
    "panels.knowledge.note-create-retry": KNOWLEDGE,
    "panels.knowledge.note-delete-undo": KNOWLEDGE,
    "panels.schedules.invalid-preview": SCHEDULES,
    "panels.schedules.list-read-retry": SCHEDULES,
}


class PanelScenariosTest(unittest.TestCase):
    def setUp(self):
        self.document = json.loads(runner.SCENARIOS.read_text(encoding="utf-8"))
        self.features = json.loads(runner.INVENTORY.read_text(encoding="utf-8"))["features"]
        self.scenarios = {case["id"]: case for case in self.document["scenarios"]}

    def test_implemented_panels_have_linked_paired_cases(self):
        features = {feature["id"]: feature for feature in self.features}
        for case_id, feature_id in CASES.items():
            with self.subTest(case=case_id):
                self.assertIsNotNone(self.scenarios.get(case_id), f"missing paired scenario {case_id}")
                self.assertIn(case_id, features[feature_id]["scenarios"])
                self.assertEqual(self.scenarios[case_id]["feature"], feature_id)
        self.assertEqual(runner.check_contracts(
            self.document["scenarios"], self.features, self.document["targets"],
        ), [])

    def test_declarations_and_native_only_receipts_never_grant_paired_credit(self):
        coverage = runner.inventory_coverage(self.features, {}, self.document["scenarios"])
        for feature in (KNOWLEDGE, SCHEDULES):
            self.assertEqual(coverage["byFeature"][feature], "not run")
        for status in ("failed", "blocked", "incomplete", "not run"):
            results = {case: {"status": status} for case in CASES}
            coverage = runner.inventory_coverage(self.features, results, self.document["scenarios"])
            for feature in (KNOWLEDGE, SCHEDULES):
                self.assertEqual(
                    coverage["missingRequirements"][feature]["behaviors"],
                    next(item["requiredBehaviors"] for item in self.features if item["id"] == feature),
                )

    def test_partial_cases_keep_unexercised_requirements_open_even_if_they_pass(self):
        results = {case: {"status": "passed"} for case in CASES}
        coverage = runner.inventory_coverage(self.features, results, self.document["scenarios"])
        self.assertEqual(coverage["byFeature"][KNOWLEDGE], "incomplete")
        self.assertEqual(coverage["byFeature"][SCHEDULES], "incomplete")
        self.assertIn("plan linking and dispatch", coverage["missingRequirements"][KNOWLEDGE]["behaviors"])
        self.assertIn("failure retains edits and owner", coverage["missingRequirements"][KNOWLEDGE]["behaviors"])
        self.assertIn("todo completion and dependencies", coverage["missingRequirements"][KNOWLEDGE]["behaviors"])
        self.assertIn("timezone and target selection", coverage["missingRequirements"][SCHEDULES]["behaviors"])
        self.assertIn("failed operation recovery", coverage["missingRequirements"][SCHEDULES]["behaviors"])

    def test_all_inputs_wait_for_their_exact_focused_text(self):
        for case_id in CASES:
            scenario = self.scenarios[case_id]
            for index, step in enumerate(scenario["steps"]):
                if "type" not in step:
                    continue
                with self.subTest(case=case_id, step=index):
                    condition = scenario["steps"][index + 1]["wait"]
                    self.assertEqual(set(condition), {"electron", "native"})
                    self.assertEqual(condition["electron"], condition["native"])
                    self.assertIn("focusedText", condition["native"])

    def test_panel_fixture_registers_fresh_project_without_repository_setup(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for client in ("electron", "native"):
                settings = root / client / "oc-data" / "settings.json"
                settings.parent.mkdir(parents=True)
                settings.write_text('{"projects": [{"id": "seed", "path": "/seed"}]}')
                with patch.object(runner.subprocess, "run", side_effect=AssertionError("started a process")):
                    project = runner.prepare_fixture(root, "project", settings.parents[1])
                self.assertEqual(list(project.iterdir()), [])
                identifier = "path_" + base64.urlsafe_b64encode(str(project).encode()).decode().rstrip("=")
                config = json.loads(settings.read_text())
                self.assertEqual(config["activeProjectId"], identifier)
                self.assertEqual([(item["id"], item["path"]) for item in config["projects"]],
                                 [("seed", "/seed"), (identifier, str(project))])
                (project / "previous-client").touch()
        self.assertEqual({self.scenarios[case]["fixture"] for case in CASES}, {"project"})

    def test_persistence_checks_reject_wrong_text_duplicates_and_failed_reads(self):
        expectations = [
            (step["expect"], scenario["id"])
            for scenario in self.scenarios.values() if scenario["id"] in CASES
            for step in scenario["steps"]
            if "expect" in step and "api" in step["expect"]
        ]
        self.assertTrue(expectations)
        context = runner.StepContext()
        client = Mock()
        for spec, case_id in expectations:
            with self.subTest(case=case_id, spec=spec):
                # A failed response with an empty-looking body cannot establish
                # either an empty initial list or a successful mutation.
                client.api_get.return_value = {"status": 503, "body": {"notes": [], "todos": [], "tasks": []}}
                self.assertFalse(runner.observe(client, spec, context)[0])
        stored = next(spec for spec, _ in expectations if spec.get("contains") == {"body": "Release checklist for 2.7"})
        for body in (
            {"notes": []},
            {"notes": [{"body": "Release checklist"}]},
            {"notes": [{"body": "Release checklist for 2.7"}, {"body": "unexpected duplicate"}]},
            {"notes": [{"body": "Release checklist for 2.7"}, {}]},
        ):
            client.api_get.return_value = {"status": 200, "body": body}
            self.assertFalse(runner.observe(client, stored, context)[0])
        client.api_get.return_value = {"status": 200, "body": {"notes": [{"body": "Release checklist for 2.7"}]}}
        self.assertTrue(runner.observe(client, stored, context)[0])

    def test_bad_server_outcome_stops_before_the_next_input_or_capture(self):
        scenario = self.scenarios["panels.knowledge.todo-complete-reopen"]
        completed = next(step for step in scenario["steps"]
                         if step.get("expect", {}).get("contains") == {"text": "Write release notes", "completed": True})
        client = Mock()
        client.api_get.return_value = {
            "status": 200, "body": {"todos": [{"text": "Write release notes", "completed": False}]},
        }
        failed = copy.deepcopy(completed)
        failed["expect"]["timeout"] = 0
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            with self.assertRaisesRegex(TimeoutError, "expectation not met"):
                runner.perform(client, "native", [
                    failed, {"type": "must not run"}, {"capture": "must-not-exist"},
                ], {}, output, argparse.Namespace())
            client.screenshot.assert_called_once_with(output / "wait-failed.png")
        client.type_text.assert_not_called()
        client.targets.assert_not_called()

    def test_invalid_preview_has_no_save_run_or_create_proof(self):
        scenario = self.scenarios["panels.schedules.invalid-preview"]
        self.assertEqual(scenario["proofs"][SCHEDULES], {
            "behaviors": ["appearance", "invalid cron clears the preview without saving"], "states": ["empty", "editing", "invalid"],
        })
        self.assertFalse(any(step.get("clickTarget") in ("schedule-save", "schedule-run") for step in scenario["steps"]))
        self.assertTrue(any(step.get("expect", {}).get("fault") == "unexpected-schedule-create"
                            and step["expect"]["hits"] == 0 for step in scenario["steps"]))
        spec = next(step["fault"] for step in scenario["steps"] if "fault" in step)
        for client in ("electron", "native"):
            rule = steps.fault_rule(spec, client)
            rule["path"] = rule["path"].replace("{fixtureId}", "path_fixture")
            self.assertTrue(steps.fault_matches(rule, "PUT", "/api/projects/path_fixture/scheduled-tasks"))
            self.assertTrue(steps.fault_matches(rule, "PUT", "/api/projects/other/scheduled-tasks"))
            self.assertFalse(steps.fault_matches(rule, "GET", "/api/projects/path_fixture/scheduled-tasks"))

    def test_checked_wait_rejects_unobservable_or_malformed_state_requests(self):
        for native in (
            {"named": {"name": "todo"}, "checked": "false"},
            {"elements": ["todo"], "checked": False},
            {"named": {"name": "todo"}, "checked": False, "present": False},
            {"named": {"name": "todo", "role": "entry"}, "checked": False},
            {"named": {"role": "check box"}, "checked": False},
            {"named": {"name": "todo", "role": "check box"}, "elements": [], "checked": True},
        ):
            self.assertTrue(steps.check_step({"wait": {
                "electron": {"expression": "false"}, "native": native,
            }}, {}))

    def test_schedule_navigation_dispatches_the_actual_path_for_each_desktop(self):
        case = self.scenarios["panels.schedules.invalid-preview"]
        start = next(i for i, step in enumerate(case["steps"]) if step.get("clickTarget") == "fixture-project-option") + 1
        end = next(i for i, step in enumerate(case["steps"]) if step.get("clickTarget") == "schedule-scope")
        targets = self.document["targets"]
        for kind, path in (("electron", [{"selector": "[data-sidebar-row-key='page:scheduled'] button"}]),
                           ("native", ["sidebar-more", "scheduled-tasks"])):
            client = Mock()
            with self.subTest(client=kind), patch.object(runner, "settle", return_value=True):
                runner.perform(client, kind, case["steps"][start:end], targets, Path("unused"), argparse.Namespace())
                self.assertEqual([args[0] for args, _ in client.click_target.call_args_list], path)
        step = case["steps"][start]
        for broken in ({"native": ["schedule-open"]}, {"native": [], "electron": ["schedule-open"]},
                       {"native": ["sidebar-more", "missing-target"], "electron": ["schedule-open"]}):
            self.assertTrue(steps.check_step({"clickTarget": broken}, targets))
        with patch.object(runner, "settle", side_effect=[True, False]):
            client = Mock()
            with self.assertRaises(TimeoutError):
                runner.perform(client, "native", [step], targets, Path("unused"), argparse.Namespace())
            client.click_target.assert_called_once_with("sidebar-more")

    def test_schedule_preview_requires_fixed_timezone_and_real_preview_before_absence(self):
        case = self.scenarios["panels.schedules.invalid-preview"]
        native_waits = [step["wait"]["native"] for step in case["steps"] if "wait" in step]
        self.assertIn({"named": {"id": "scheduled-task-timezone", "name": "Timezone: UTC"}}, native_waits)
        before = native_waits.index({"named": {"id": "scheduled-task-next-runs", "name": ", ".join(["Jan 10, 11:17 AM"] * 4)}})
        after = next(index for index, condition in enumerate(native_waits)
                     if condition.get("named", {}).get("id") == "scheduled-task-next-runs" and condition.get("present") is False)
        self.assertLess(before, after)
        self.assertFalse(native_waits[after]["present"])


if __name__ == "__main__":
    unittest.main()
