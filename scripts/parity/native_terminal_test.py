"""Guard the terminal qualification's readings against look-alike successes."""

import unittest

from native_terminal import KEYS, backoff_within_contract, expected_keys, listed


class KeyExpectations(unittest.TestCase):
    def test_application_cursor_changes_only_cursor_keys(self):
        normal, application = expected_keys(False), expected_keys(True)
        self.assertTrue(normal.startswith(b"\x1b[A\x1b[B") and normal.endswith(b"q"))
        self.assertTrue(application.startswith(b"\x1bOA\x1bOB"))
        changed = [key for key, plain, app in KEYS if plain != app]
        self.assertEqual(changed, ["Up", "Down", "Right", "Left", "Home", "End"])

    def test_no_expected_key_contains_the_stop_byte(self):
        for _, plain, app in KEYS:
            self.assertNotIn(b"q", plain + app)


class Pacing(unittest.TestCase):
    def test_doubling_waits_pass(self):
        self.assertTrue(backoff_within_contract([0.0, 0.5, 1.5, 3.5, 7.5]))

    def test_a_tight_retry_loop_fails(self):
        self.assertFalse(backoff_within_contract([0.0, 0.1, 0.2, 0.3]))

    def test_a_single_attempt_proves_nothing(self):
        self.assertFalse(backoff_within_contract([0.0]))

    def test_waits_past_the_cap_fail(self):
        self.assertFalse(backoff_within_contract([0.0, 0.5, 20.0]))


class Lists(unittest.TestCase):
    def test_filters_by_status(self):
        sessions = [{"sessionId": "b", "status": "running"}, {"sessionId": "a", "status": "exited"}]
        self.assertEqual(listed(sessions), ["a", "b"])
        self.assertEqual(listed(sessions, "running"), ["b"])


if __name__ == "__main__":
    unittest.main()
