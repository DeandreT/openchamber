"""Contract checks for native interaction qualification, without opening windows."""

import copy
import json
from pathlib import Path
import signal
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import native_interactions as interactions


def shell_message(status="completed", text=interactions.SHELL_OUTPUT, exit_code=0):
    return {"type": "assistant", "content": [{"type": "tool", "name": "shell", "state": {
        "status": status, "input": {"command": interactions.SHELL_COMMAND},
        "metadata": {"exit": exit_code}, "content": [{"type": "text", "text": text}],
    }}]}


class Observations(unittest.TestCase):
    def test_superseding_input_targets_only_the_composer_subtree(self):
        class Node:
            def __init__(self, role, children=()):
                self.role, self.children = role, children

            def get_role_name(self):
                return self.role

            def get_child_count(self):
                return len(self.children)

            def get_child_at_index(self, index):
                return self.children[index]

        composer = Node("entry")
        other = Node("entry")
        root = Node("group", [Node("group", [composer])])
        interaction = object.__new__(interactions.Interaction)
        interaction.client = SimpleNamespace(_find=lambda name: root, _walk=lambda: [other, composer])
        self.assertIs(interaction.composer(), composer)
        root.children.append(Node("entry"))
        with self.assertRaisesRegex(RuntimeError, "exactly one entry"):
            interaction.composer()

    def test_command_arguments_or_prose_never_count_as_execution(self):
        messages = [shell_message("running", ""), shell_message("error"),
                    {"type": "user", "text": interactions.SHELL_OUTPUT},
                    {"type": "assistant", "content": [{"type": "text", "text": interactions.SHELL_OUTPUT}]}]
        self.assertEqual(interactions.successful_shell_outputs(messages), [])
        self.assertEqual(interactions.successful_shell_outputs([shell_message(text="")]), [])
        self.assertEqual(len(interactions.successful_shell_outputs(messages + [shell_message()])), 1)

    def test_nonzero_exit_timeout_and_wrong_command_cannot_pass(self):
        for message in (shell_message(exit_code=1), shell_message(text="native-permission-ok")):
            self.assertEqual(interactions.successful_shell_outputs([message]), [])
        for field, value in (("timeout", True), ("signal", "SIGTERM")):
            message = shell_message()
            message["content"][0]["state"]["metadata"][field] = value
            self.assertEqual(interactions.successful_shell_outputs([message]), [])
        message = shell_message()
        message["content"][0]["state"]["input"]["command"] = "echo native-permission-ok"
        self.assertEqual(interactions.successful_shell_outputs([message]), [])

    def test_only_exact_owned_permission_can_be_clicked(self):
        request = {"id": "per_fixture", "sessionID": "ses_fixture", "action": "shell", "resources": [interactions.SHELL_COMMAND]}
        self.assertEqual(interactions.verify_permission(request, "ses_fixture"), "per_fixture")
        for key, value in (("sessionID", "ses_other"), ("action", "read"),
                           ("resources", [interactions.SHELL_COMMAND, "rm -rf /"]), ("resources", ["printf other"]), ("id", None)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                interactions.verify_permission({**request, key: value}, "ses_fixture")

    def test_question_shape_and_owner_are_checked(self):
        form = {"id": "frm_fixture", "sessionID": "ses_fixture", "metadata": {"kind": "question"}, "fields": [
            {"key": "q0", "type": "string", "custom": True, "options": [{"value": "Alpha"}, {"value": "Beta"}]},
            {"key": "q1", "type": "multiselect", "custom": True, "options": [{"value": "One"}, {"value": "Two"}]},
            {"key": "q2", "type": "string", "custom": True, "options": []},
        ]}
        self.assertEqual(interactions.verify_form(form, "ses_fixture"), "frm_fixture")
        with self.assertRaises(ValueError):
            interactions.verify_form(form, "ses_other")
        wrong = copy.deepcopy(form)
        wrong["fields"][1]["type"] = "string"
        with self.assertRaises(ValueError):
            interactions.verify_form(wrong, "ses_fixture")

    def test_malformed_reads_do_not_become_empty_success(self):
        fixture = interactions.Fixture(Path("/tmp/opencode/test"), Path("unused"), None, None)
        for response in ({}, {"data": None}, {"data": {}}, []):
            with patch.object(fixture, "get", return_value=response), self.assertRaises(ValueError):
                fixture.data("/api/session/ses_fixture/permission", list)
        with patch.object(fixture, "get", return_value={"data": []}):
            self.assertEqual(fixture.data("/api/session/ses_fixture/permission", list), [])

    def test_readiness_rejects_remote_origin_and_directory_escape(self):
        root = Path("/tmp/opencode/fixture-unit-test")
        ready = {"root": str(root), "project": str(root / "project"), "state": str(root / "state"),
                 "url": "http://127.0.0.1:4321", "pid": 100, "fixturePid": 101, "fixturePort": 4322}
        self.assertEqual(interactions.verify_readiness(ready, root), ready)
        for key, value in (("url", "http://example.com:4321"), ("url", "http://127.0.0.1:4321/path"),
                           ("url", "http://@127.0.0.1:4321"), ("url", "http://:secret@127.0.0.1:4321"),
                           ("project", "/home/deandre"), ("fixturePid", True), ("pid", None)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                interactions.verify_readiness({**ready, key: value}, root)


class Lifecycle(unittest.TestCase):
    def test_fixture_launcher_owns_port_allocation(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="interaction-ports-test-") as temporary:
            args = SimpleNamespace(dist=Path("built-ui"), opencode_binary=Path("engine"))
            with interactions.Interrupts() as interrupts, \
                    patch.object(interactions.runner, "free_port") as reserve, \
                    patch.object(interactions.subprocess, "Popen", side_effect=RuntimeError("stop before launch")) as spawn:
                fixture = interactions.Fixture(Path(temporary) / "fixture", Path(temporary) / "evidence", args, interrupts)
                with self.assertRaisesRegex(RuntimeError, "stop before launch"):
                    fixture.__enter__()
            argv = spawn.call_args.args[0]
            self.assertIn("--interactions", argv)
            self.assertNotIn("--port", argv)
            self.assertNotIn("--fixture-port", argv)
            reserve.assert_not_called()

    def test_report_survives_private_bus_shutdown_during_interruption(self):
        class InterruptedFixture(interactions.Fixture):
            def __enter__(self):
                self.evidence.mkdir()
                raise interactions.RunInterrupted("startup interrupted")

        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="interaction-report-test-") as temporary:
            args = SimpleNamespace(output=Path(temporary) / "report", binary=Path("native"), opencode_binary=Path("engine"),
                                   checks=["reject"], scale=1.25)
            with patch.object(interactions.virtual_display, "verify_owned_display"), \
                    patch.object(interactions, "binary_info", return_value={"sha256": "frozen"}), \
                    patch.object(interactions, "accessibility_enabled", return_value=False), \
                    patch.object(interactions, "set_accessibility", side_effect=[None, subprocess.CalledProcessError(1, "busctl")]), \
                    patch.object(interactions, "Fixture", InterruptedFixture):
                self.assertEqual(interactions.qualify(args), 1)
            report = json.loads((args.output / "report.json").read_text())
            self.assertTrue(report["checks"][0]["interrupted"])
            self.assertIn("busctl", report["accessibilityRestoreError"])

    def test_interrupt_defers_until_child_cleanup_is_registered_and_restores_handlers(self):
        previous = {sig: signal.getsignal(sig) for sig in (signal.SIGINT, signal.SIGTERM)}
        registered = []
        with interactions.Interrupts() as interrupts:
            with self.assertRaises(interactions.RunInterrupted):
                with interrupts.defer():
                    interrupts.receive(signal.SIGTERM, None)
                    registered.append("owned process")
            self.assertEqual(registered, ["owned process"])
            # Repeated signals must not interrupt the cleanup already in progress.
            interrupts.receive(signal.SIGINT, None)
        self.assertEqual({sig: signal.getsignal(sig) for sig in previous}, previous)

    def test_process_identity_uses_start_time_even_with_spaces_in_comm(self):
        fields = ["S"] + ["0"] * 18 + ["123456"] + ["0"] * 5
        with patch.object(Path, "read_text", return_value="42 (test (fixture) process) " + " ".join(fields)):
            self.assertEqual(interactions.process_identity(42), "123456")

    def test_process_disappearing_during_proc_read_is_already_stopped(self):
        with patch.object(Path, "read_text", side_effect=ProcessLookupError(3, "No such process")):
            self.assertIsNone(interactions.process_identity(42))

    def test_recorded_pid_reuse_is_not_adopted_for_cleanup(self):
        owned = {42: "original"}
        with patch.object(interactions, "process_identity", return_value="reused"), patch.object(Path, "read_text") as read:
            interactions.record_descendants(42, owned)
        self.assertEqual(owned, {42: "original"})
        read.assert_not_called()


class QualificationResult(unittest.TestCase):
    def exercise(self, *, native_error=None, fixture_error=None, substitutions=(), restore_error=None):
        events = []

        class Fixture:
            def __init__(self, root, evidence, args, interrupts):
                self.evidence = evidence

            def __enter__(self):
                self.evidence.mkdir()
                return self

            def __exit__(self, *_):
                events.append("fixture cleanup")
                if fixture_error:
                    raise fixture_error

        class Interaction:
            def __init__(self, fixture, args, identity, cleanup):
                self.client = None
                self.clients = [SimpleNamespace(process=None, substitutions=list(substitutions))]

                def close_native():
                    events.append("native cleanup")
                    if native_error:
                        raise native_error
                cleanup.callback(close_native)

            def run(self, check):
                events.append("successful interaction")
                return {"sessionID": "ses_fixture", "prompts": 1}

        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="interaction-result-test-") as temporary:
            args = SimpleNamespace(output=Path(temporary) / "report", binary=Path("native"), opencode_binary=Path("engine"),
                                   checks=["reject"], scale=1.25)
            with patch.object(interactions.virtual_display, "verify_owned_display"), \
                    patch.object(interactions, "binary_info", return_value={"sha256": "frozen"}), \
                    patch.object(interactions, "accessibility_enabled", return_value=False), \
                    patch.object(interactions, "set_accessibility", side_effect=[None, restore_error]), \
                    patch.object(interactions, "Fixture", Fixture), patch.object(interactions, "Interaction", Interaction):
                code = interactions.qualify(args)
            report = json.loads((args.output / "report.json").read_text())
        self.assertEqual(events, ["successful interaction", "native cleanup", "fixture cleanup"])
        return code, report["checks"][0]

    def test_success_requires_both_cleanup_contexts_to_finish(self):
        code, result = self.exercise()
        self.assertEqual(code, 0)
        self.assertTrue(result["passed"])
        self.assertEqual(result["status"], "passed")

    def test_successful_interaction_then_cleanup_failure_is_failed(self):
        for owner in ("native", "fixture"):
            with self.subTest(owner=owner):
                code, result = self.exercise(**{owner + "_error": RuntimeError(f"{owner} cleanup failed")})
                self.assertEqual(code, 1)
                self.assertFalse(result["passed"])
                self.assertEqual(result["status"], "failed")
                self.assertIn("cleanup failed", result["error"])

    def test_successful_interaction_then_cleanup_interrupt_is_failed(self):
        for owner in ("native", "fixture"):
            with self.subTest(owner=owner):
                code, result = self.exercise(**{owner + "_error": interactions.RunInterrupted("interrupted during cleanup")})
                self.assertEqual(code, 1)
                self.assertFalse(result["passed"])
                self.assertTrue(result["interrupted"])

    def test_success_with_pointer_substitution_is_incomplete(self):
        substitution = {"kind": "pointer", "key": "permission-per_fixture-reject", "replacedBy": "accessibility action"}
        code, result = self.exercise(substitutions=[substitution])
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(result["inputSubstitutions"], [substitution])

    def test_accessibility_cleanup_failure_invalidates_success(self):
        code, result = self.exercise(restore_error=subprocess.CalledProcessError(1, "busctl"))
        self.assertEqual(code, 1)
        self.assertFalse(result["passed"])
        self.assertEqual(result["status"], "failed")


if __name__ == "__main__":
    unittest.main()
