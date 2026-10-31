import json
import unittest

from native_accessibility import announced, menu_state_problems, spoken_count


class AnnouncedTest(unittest.TestCase):
    def test_only_events_carrying_the_text_count(self):
        lines = [
            json.dumps({"text": "Couldn't pin the session", "politeness": 1, "role": "notification"}),
            "not json",
            json.dumps({"text": None, "politeness": 1, "role": "notification"}),
            json.dumps({"text": "Session archived", "politeness": 1, "role": "notification"}),
        ]
        events = announced(lines, "couldn't PIN the session")
        self.assertEqual([event["text"] for event in events], ["Couldn't pin the session"])
        self.assertEqual(announced(lines, "Couldn't unpin"), [])


class SpokenCountTest(unittest.TestCase):
    def test_only_speech_lines_with_the_text_count(self):
        log = "\n".join([
            "12:00 - SPEECH OUTPUT: 'Couldn't pin the session'{'established': 'False'}",
            "12:00 - EVENT MANAGER: object:announcement (1, 0, Couldn't pin the session)",
            "12:01 - SPEECH OUTPUT: 'couldn't pin the session'",
            "12:02 - SPEECH OUTPUT: 'Pin session'",
        ])
        self.assertEqual(spoken_count(log, "Couldn't pin the session"), 2)
        self.assertEqual(spoken_count("", "Couldn't pin the session"), 0)


class MenuStateProblemsTest(unittest.TestCase):
    def test_only_the_named_actions_may_be_enabled(self):
        self.assertEqual(menu_state_problems([("rename", True), ("pin", False), ("delete", False)], {"rename"}), [])
        self.assertEqual(
            menu_state_problems([("rename", False), ("pin", True)], {"rename"}),
            ["rename is disabled", "pin is enabled"],
        )

    def test_a_missing_enabled_action_is_reported(self):
        self.assertEqual(menu_state_problems([("pin", False)], {"rename"}), ["rename is missing"])


if __name__ == "__main__":
    unittest.main()
