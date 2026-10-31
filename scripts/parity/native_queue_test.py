"""Guard the queue qualification's server readings against look-alike successes."""

import unittest

from native_queue import queued_contents, queued_ids, starter_names, user_texts, verify_starter_prompt


SNAPSHOT = {"revision": 4, "sessions": [
    {"sessionId": "ses_other", "directory": "/p", "items": [{"id": "x", "content": "other"}], "sendingId": None},
    {"sessionId": "ses_a", "directory": "/p", "items": [{"id": "q2", "content": "beta"}, {"id": "q1", "content": "alpha"}],
     "sendingId": None},
]}


class QueueReadings(unittest.TestCase):
    def test_reads_only_the_owned_session_in_server_order(self):
        self.assertEqual(queued_contents(SNAPSHOT, "ses_a"), ["beta", "alpha"])
        self.assertEqual(queued_ids(SNAPSHOT, "ses_a"), ["q2", "q1"])

    def test_a_missing_session_is_an_empty_queue(self):
        self.assertEqual(queued_contents(SNAPSHOT, "ses_none"), [])
        self.assertEqual(queued_contents({"sessions": []}, "ses_a"), [])

    def test_user_texts_skip_assistant_and_synthetic_records(self):
        messages = [{"type": "user", "text": "one"}, {"type": "assistant", "text": "reply"},
                    {"type": "synthetic", "text": "context"}, {"type": "user", "text": "two"}]
        self.assertEqual(user_texts(messages), ["one", "two"])


class StarterReadings(unittest.TestCase):
    def test_defaults_are_not_a_saved_list(self):
        self.assertIsNone(starter_names({}))
        self.assertIsNone(starter_names({"draftStarters": "explore"}))
        self.assertEqual(starter_names({"draftStarters": [{"type": "command", "name": "debug"}, "junk"]}), ["debug"])

    def test_the_rendered_prompt_must_carry_the_draft_as_its_idea(self):
        verify_starter_prompt("Help me craft a goal.\n\nHere is my initial idea:\nidea", "idea")
        for text in ("/craft-goal idea", "Help me craft a goal.", None,
                     "/craft-goal\n\nHere is my initial idea:\nidea"):
            with self.subTest(text=text), self.assertRaises(ValueError):
                verify_starter_prompt(text, "idea")


if __name__ == "__main__":
    unittest.main()
