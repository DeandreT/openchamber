"""Pure fixture-preparation contracts; no engine, Git process or display starts."""

import hashlib
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import runner
import steps


class PairedGitStimulusTest(unittest.TestCase):
    def prepare(self, root, scenario_id, baseline, *, tracked=True, kind="git"):
        state = root / "state"
        settings = state / "oc-data/settings.json"
        settings.parent.mkdir(parents=True)
        settings.write_text(json.dumps({"projects": []}))
        project = runner.fixture_project(root, kind)
        calls = []

        def command(arguments, **kwargs):
            calls.append(arguments)
            if arguments[0] == "node":
                (project / "src").mkdir()
                (project / "src/app.txt").write_bytes(baseline)
                (project / "README.md").write_bytes(b"unrelated fixture file\n")
            elif arguments[:2] == ["git", "ls-files"]:
                self.assertEqual(arguments, ["git", "ls-files", "--error-unmatch", "--", "src/app.txt"])
                self.assertEqual(kwargs["cwd"], project)
                self.assertTrue(kwargs["check"])
                self.assertEqual(kwargs["env"]["GIT_CONFIG_NOSYSTEM"], "1")
                self.assertEqual(kwargs["env"]["GIT_CONFIG_GLOBAL"], "/dev/null")
                if not tracked:
                    raise subprocess.CalledProcessError(1, arguments)
            elif arguments == ["git", "remote", "set-url", "origin", "https://github.com/acme/widgets.git"]:
                self.assertEqual(kind, "github")
            else:
                self.fail(f"unexpected process request: {arguments}")
            return subprocess.CompletedProcess(arguments, 0, stdout=b"", stderr=b"")

        with patch.object(runner.subprocess, "run", side_effect=command):
            result = runner.prepare_fixture(root, kind, state, scenario_id=scenario_id)
        self.assertEqual(result, project)
        self.assertEqual((project / "README.md").read_bytes(), b"unrelated fixture file\n")
        return (project / "src/app.txt").read_bytes(), calls

    def test_both_private_copies_receive_identical_stimulus_without_changing_seed(self):
        baseline = b"line 1 changed\nline 2\nline 3\n"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            seed = root / "seed/src/app.txt"
            seed.parent.mkdir(parents=True)
            seed.write_bytes(baseline)
            seed_before = hashlib.sha256(seed.read_bytes()).hexdigest()
            copies = []
            for client in ("electron", "native"):
                output, calls = self.prepare(root / client, "files.diff-git", baseline)
                copies.append(output)
                self.assertEqual(len([call for call in calls if call[:2] == ["git", "ls-files"]]), 1)
            self.assertEqual(copies, [b"line 1 paired edit\nline 2\nline 3\n"] * 2)
            self.assertEqual(hashlib.sha256(seed.read_bytes()).hexdigest(), seed_before)

    def test_unrelated_scenarios_leave_the_committed_baseline_unchanged(self):
        baseline = b"line 1 changed\nline 2\n"
        for scenario_id, kind in ((None, "git"), ("files.navigation-editor", "git"),
                                  ("files.issues-pull-requests", "github"), ("shell.startup", "git")):
            with self.subTest(scenario_id=scenario_id), tempfile.TemporaryDirectory() as temporary:
                output, calls = self.prepare(Path(temporary), scenario_id, baseline, kind=kind)
                self.assertEqual(output, baseline)
                self.assertFalse(any(call[:2] == ["git", "ls-files"] for call in calls))

    def test_only_the_first_line_changes_and_existing_line_endings_are_preserved(self):
        for baseline, expected in (
            (b"line 1 changed\r\nline 2\r\nlast", b"line 1 paired edit\r\nline 2\r\nlast"),
            (b"line 1 changed\nline 2\r\nlast", b"line 1 paired edit\nline 2\r\nlast"),
        ):
            with self.subTest(baseline=baseline), tempfile.TemporaryDirectory() as temporary:
                output, _ = self.prepare(Path(temporary), "files.diff-git", baseline)
                self.assertEqual(output, expected)

    def test_untracked_or_wrong_baseline_is_refused_without_editing_the_file(self):
        for baseline, tracked, error in (
            (b"line 1 changed\nline 2\n", False, subprocess.CalledProcessError),
            (b"different first line\nline 2\n", True, RuntimeError),
        ):
            with self.subTest(tracked=tracked), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                with self.assertRaises(error):
                    self.prepare(root, "files.diff-git", baseline, tracked=tracked)
                self.assertEqual((runner.fixture_project(root, "git") / "src/app.txt").read_bytes(), baseline)

    def test_missing_original_path_is_refused_without_creating_a_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary)
            with patch.object(runner.subprocess, "run") as command:
                with self.assertRaisesRegex(RuntimeError, "original tracked src/app.txt"):
                    runner.prepare_paired_git_stimulus(project)
            command.assert_not_called()
            self.assertFalse((project / "src/app.txt").exists())


class FilesScenarioContractTest(unittest.TestCase):
    def test_github_is_allowed_but_an_unknown_fixture_stays_rejected(self):
        self.assertEqual(steps.check_scenario({"fixture": "github", "steps": []}), [])
        self.assertTrue(steps.check_scenario({"fixture": "svn", "steps": []}))

    def test_each_files_scenario_is_linked_without_adding_requirements(self):
        document = json.loads(runner.SCENARIOS.read_text())
        features = json.loads(runner.INVENTORY.read_text())["features"]
        wanted = {"files.navigation-editor", "files.diff-git", "files.issues-pull-requests"}
        scenarios = [scenario for scenario in document["scenarios"] if scenario["id"] in wanted]
        self.assertEqual({scenario["id"] for scenario in scenarios}, wanted)
        by_id = {feature["id"]: feature for feature in features}
        for scenario in scenarios:
            self.assertIn(scenario["id"], by_id[scenario["feature"]]["scenarios"])
        self.assertEqual(runner.check_contracts(
            scenarios, features, document["targets"],
            linked={scenario["id"] for scenario in document["scenarios"]},
        ), [])


if __name__ == "__main__":
    unittest.main()
