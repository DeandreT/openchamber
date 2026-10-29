"""Guard selection proof against marker-only, stale-step and cleanup successes."""

import copy
from contextlib import nullcontext
from pathlib import Path
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

from native_selections import AGENT, MODEL, PROMPT, SelectionFixture, Selections, require_clean_result, verify_turn
from native_interactions import Fixture


def turn(variant="high"):
    model = {"providerID": "perf", "id": MODEL, "variant": "default" if variant is None else variant}
    return [{"type": "user", "id": "msg_user", "text": PROMPT},
            {"type": "assistant", "id": "msg_assistant", "agent": AGENT, "model": model,
             "time": {"completed": 123}, "finish": "stop",
             "content": [{"type": "text", "text": "Fixture response"}]}]


class TurnProof(unittest.TestCase):
    def test_exact_unicode_and_completed_step(self):
        self.assertEqual(verify_turn(turn(), PROMPT, MODEL, AGENT, "high")["assistantIDs"], ["msg_assistant"])

    def test_default_requires_canonical_engine_variant(self):
        verify_turn(turn(None), PROMPT, MODEL, AGENT, None)
        for variant in ("high", "", "arbitrary"):
            with self.subTest(variant=variant), self.assertRaisesRegex(ValueError, "different"):
                verify_turn(turn(variant), PROMPT, MODEL, AGENT, None)
        missing = turn(None)
        del missing[1]["model"]["variant"]
        with self.assertRaisesRegex(ValueError, "different"):
            verify_turn(missing, PROMPT, MODEL, AGENT, None)
        with self.assertRaisesRegex(ValueError, "different"):
            verify_turn(turn(None), PROMPT, MODEL, AGENT, "high")

    def test_newest_first_api_history_is_reversed_before_turn_checks(self):
        probe = Selections.__new__(Selections)
        probe.session_id = "ses_fixture"
        previous = [{"type": "user", "id": "msg_old", "text": "old"},
                    {**turn()[1], "id": "msg_old_assistant"}]
        chronological = previous + turn(None)
        probe.fixture = SimpleNamespace(data=Mock(return_value=list(reversed(chronological))))
        self.assertEqual(probe.messages(), chronological)
        verify_turn(probe.messages(), PROMPT, MODEL, AGENT, None)

    def test_prompt_marker_in_assistant_is_not_user_input(self):
        messages = turn()[1:]
        messages[0]["content"][0]["text"] = PROMPT
        with self.assertRaisesRegex(ValueError, "exactly once"):
            verify_turn(messages, PROMPT, MODEL, AGENT, "high")

    def test_unicode_loss_and_duplicate_send_fail(self):
        messages = turn()
        messages[0]["text"] = PROMPT.encode("ascii", errors="ignore").decode()
        for candidate in (messages, turn() + turn()):
            with self.subTest(candidate=candidate), self.assertRaisesRegex(ValueError, "exactly once"):
                verify_turn(candidate, PROMPT, MODEL, AGENT, "high")

    def test_wrong_provider_model_agent_or_variant_fails(self):
        for field, value in (("providerID", "external"), ("id", "stream-20000cps"), ("variant", "low")):
            messages = turn()
            messages[1]["model"][field] = value
            with self.subTest(field=field), self.assertRaisesRegex(ValueError, "different"):
                verify_turn(messages, PROMPT, MODEL, AGENT, "high")
        messages = turn()
        messages[1]["agent"] = "fixture-alpha"
        with self.assertRaisesRegex(ValueError, "different"):
            verify_turn(messages, PROMPT, MODEL, AGENT, "high")

    def test_previous_or_next_turn_step_cannot_satisfy_send(self):
        messages = turn()
        for candidate in ([messages[1], messages[0]],
                          [messages[0], {"type": "user", "text": "other"}, messages[1]]):
            with self.subTest(candidate=candidate), self.assertRaisesRegex(ValueError, "no assistant"):
                verify_turn(candidate, PROMPT, MODEL, AGENT, "high")

    def test_empty_error_incomplete_and_tool_steps_fail(self):
        for patch in ({"content": []}, {"error": {"message": "failed"}}, {"time": {}},
                      {"finish": "length"}, {"content": [{"type": "tool", "text": PROMPT}]}):
            messages = turn()
            messages[1].update(patch)
            with self.subTest(patch=patch), self.assertRaises(ValueError):
                verify_turn(messages, PROMPT, MODEL, AGENT, "high")

    def test_later_wrong_step_is_not_hidden_by_one_matching_step(self):
        messages = turn()
        wrong = copy.deepcopy(messages[1])
        wrong["agent"] = "build"
        messages.append(wrong)
        with self.assertRaisesRegex(ValueError, "different"):
            verify_turn(messages, PROMPT, MODEL, AGENT, "high")


class FixtureModeProof(unittest.TestCase):
    def test_both_modes_use_the_same_owned_startup_and_exact_cli_flag(self):
        self.assertIs(SelectionFixture.__enter__, Fixture.__enter__)
        self.assertIs(SelectionFixture.close, Fixture.close)
        for fixture_type, flag in ((Fixture, "--interactions"), (SelectionFixture, "--selections")):
            with self.subTest(flag=flag), tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
                base = Path(directory)
                args = SimpleNamespace(dist=base / "dist", opencode_binary=base / "engine")
                fixture = fixture_type(base / "root", base / "evidence", args,
                                       SimpleNamespace(defer=nullcontext))
                with patch("native_interactions.subprocess.Popen", side_effect=OSError("launch sentinel")) as launch:
                    with self.assertRaisesRegex(OSError, "launch sentinel"):
                        fixture.__enter__()
                argv = launch.call_args.args[0]
                self.assertEqual(argv[2:], [flag, "--root", str(fixture.root), "--dist", str(args.dist),
                                           "--opencode-binary", str(args.opencode_binary)])
                self.assertTrue(fixture.log.closed)


class SelectionWorkflowProof(unittest.TestCase):
    def test_agent_default_step_fails_despite_reopened_chosen_toolbar(self):
        # Simulate a broken model picker that changes and persists its label,
        # while the real send continues to use beta's configured default.
        probe = Selections.__new__(Selections)
        probe.args = SimpleNamespace(timeout=1)
        probe.env = {}
        probe.prompts = 0
        probe.fixture = SimpleNamespace(create_session=Mock(return_value={"id": "ses_fixture"}),
                                        evidence=Path("unused"))
        labels = {"model": "stream-20000cps", "agent": "Build", "variant": "Default"}
        draft = [""]
        messages = []
        choices = []
        node = Mock()
        node.get_state_set.return_value.contains.return_value = True
        probe.client = Mock()
        probe.client._find.return_value = node
        probe.start = Mock()
        probe.reopen = Mock()
        probe.click = Mock()
        probe.pointer = Mock()
        probe.named = Mock(return_value=Mock())
        probe.composer = Mock(return_value=node)
        probe.labels = lambda: dict(labels)
        probe.messages = lambda: messages
        probe.settled = lambda: True
        probe.paste = lambda target, text: draft.__setitem__(0, text) if target is node else None

        def choose(kind, label, search=None):
            choices.append((kind, label))
            labels[kind] = label

        def key(name):
            if name == "Return":
                messages.extend(turn())
                messages[1]["model"]["id"] = "stream-20000cps"

        probe.choose = choose
        probe.client.key.side_effect = key
        api = SimpleNamespace(StateType=SimpleNamespace(FOCUSED="focused", ENABLED="enabled"))
        with patch("native_selections.editable_text", side_effect=lambda _node: draft[0]), \
                patch("native_selections._atspi", return_value=api), \
                patch("native_selections.command"), \
                self.assertRaisesRegex(ValueError, "assistant used a different model"):
            probe.run()

        self.assertEqual(MODEL, "stream-1200cps")
        self.assertLess(choices.index(("agent", "fixture-beta")), choices.index(("model", MODEL)))
        self.assertEqual(labels, {"model": MODEL, "agent": "fixture-beta", "variant": "high"})
        probe.reopen.assert_called_once_with("unsent")
        self.assertEqual(messages[1]["agent"], AGENT)
        self.assertEqual(messages[1]["model"]["variant"], "high")


class CleanupProof(unittest.TestCase):
    def test_substituted_action_cannot_pass(self):
        client = SimpleNamespace(substitutions=[{"replacedBy": "accessibility action"}], process=None)
        with self.assertRaisesRegex(RuntimeError, "substitution"):
            require_clean_result([client])

    def test_surviving_process_cannot_pass(self):
        client = SimpleNamespace(substitutions=[], process=SimpleNamespace(poll=lambda: None))
        with self.assertRaisesRegex(RuntimeError, "remained"):
            require_clean_result([client])

    def test_stopped_process_passes(self):
        require_clean_result([SimpleNamespace(substitutions=[], process=SimpleNamespace(poll=lambda: -15))])

    def test_crash_and_forced_kill_cannot_pass(self):
        for code in (1, -9, -11):
            with self.subTest(code=code), self.assertRaisesRegex(RuntimeError, "crashed"):
                require_clean_result([SimpleNamespace(substitutions=[], process=SimpleNamespace(poll=lambda: code))])


if __name__ == "__main__":
    unittest.main()
