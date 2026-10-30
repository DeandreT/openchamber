"""Guard permission shield proof against label-only, stale-revision and gated-Safety successes."""

import unittest
from types import SimpleNamespace
from unittest.mock import Mock

from native_permission_modes import (LABELS, ROUTING_TIMEOUT, Modes, expected_cycle, safety_offered,
                                     server_mode, verify_steps)


def steps(cycle, start=1):
    return [{"expected": mode, "label": LABELS[mode], "serverMode": mode,
             "revisionBefore": start + index - 1, "revisionAfter": start + index}
            for index, mode in enumerate(cycle)]


class RoutingGate(unittest.TestCase):
    def test_safety_needs_routing_and_jev(self):
        self.assertTrue(safety_offered({"available": True}))
        self.assertTrue(safety_offered({"available": True, "jevAvailable": True}))
        for routing in ({}, {"available": False}, {"available": "yes"},
                        {"available": True, "jevAvailable": False}):
            with self.subTest(routing=routing):
                self.assertFalse(safety_offered(routing))

    def test_cycle_skips_safety_when_gated(self):
        self.assertEqual(expected_cycle(False), ["auto", "ask", "auto"])
        self.assertEqual(expected_cycle(True), ["safety", "auto", "ask", "safety", "auto"])

    def test_routing_read_uses_the_long_timeout(self):
        probe = Modes.__new__(Modes)
        probe.fixture = SimpleNamespace(get=Mock(side_effect=RuntimeError("stop")))
        with self.assertRaisesRegex(RuntimeError, "stop"):
            probe.run()
        probe.fixture.get.assert_called_once_with("/api/routing", timeout=ROUTING_TIMEOUT)


class ServerMode(unittest.TestCase):
    def test_modes_win_over_legacy_booleans(self):
        policy = {"modes": {"ses_a": "safety"}, "sessions": {"ses_a": False}}
        self.assertEqual(server_mode(policy, "ses_a"), "safety")

    def test_legacy_booleans_and_missing_session(self):
        policy = {"sessions": {"ses_a": True, "ses_b": False}}
        self.assertEqual(server_mode(policy, "ses_a"), "auto")
        self.assertEqual(server_mode(policy, "ses_b"), "ask")
        self.assertEqual(server_mode(policy, "ses_c"), "ask")
        self.assertEqual(server_mode({"modes": {"ses_a": "bogus"}}, "ses_a"), "ask")


class StepProof(unittest.TestCase):
    def test_matching_cycle_passes(self):
        cycle = expected_cycle(True)
        verify_steps(steps(cycle), cycle)

    def test_label_only_change_fails(self):
        cycle = expected_cycle(False)
        recorded = steps(cycle)
        recorded[1]["serverMode"] = "auto"
        with self.assertRaisesRegex(ValueError, "disagree"):
            verify_steps(recorded, cycle)
        recorded = steps(cycle)
        recorded[0]["label"] = LABELS["ask"]
        with self.assertRaisesRegex(ValueError, "disagree"):
            verify_steps(recorded, cycle)

    def test_stale_or_missing_revision_fails(self):
        cycle = expected_cycle(False)
        recorded = steps(cycle)
        recorded[2]["revisionAfter"] = recorded[1]["revisionAfter"]
        with self.assertRaisesRegex(ValueError, "did not advance"):
            verify_steps(recorded, cycle)
        for value in (None, True, "3"):
            recorded = steps(cycle)
            recorded[0]["revisionAfter"] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "no revision"):
                verify_steps(recorded, cycle)

    def test_skipped_or_offered_safety_fails(self):
        with self.assertRaisesRegex(ValueError, "cycle"):
            verify_steps(steps(expected_cycle(False)), expected_cycle(True))
        with self.assertRaisesRegex(ValueError, "cycle"):
            verify_steps(steps(expected_cycle(True)), expected_cycle(False))


if __name__ == "__main__":
    unittest.main()
