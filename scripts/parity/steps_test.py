"""Step parsing, key events, fault rules and expectation predicates.

Run from this directory with `python3 -m unittest steps_test`.
"""

import unittest

import steps

TARGETS = {"composer": {"electron": {"selector": ".composer"}, "native": "composer"}}


class KeyTest(unittest.TestCase):
    def test_chords_parse_in_press_order(self):
        self.assertEqual(steps.parse_key("shift+ctrl+z"), steps.Chord(("ctrl", "shift"), "z"))
        self.assertEqual(steps.parse_key("Control+Return").xdotool(), "ctrl+Return")
        self.assertEqual(steps.parse_key("F2"), steps.Chord((), "F2"))

    def test_unknown_repeated_and_layout_dependent_keys_are_rejected(self):
        for spec in ("ctrl+", "hyper+a", "ctrl+ctrl+a", "Enter", "shift+1", "shift+slash", " a", ""):
            with self.subTest(spec=spec), self.assertRaises(ValueError):
                steps.parse_key(spec)

    def test_named_keys_keep_the_runners_textless_events(self):
        events = steps.dom_key_events(steps.parse_key("Return"))
        self.assertEqual(events, [
            {"type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0},
            {"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "modifiers": 0},
        ])

    def test_modifiers_wrap_the_key_and_command_chords_type_nothing(self):
        events = steps.dom_key_events(steps.parse_key("ctrl+shift+z"))
        self.assertEqual([(event["type"], event["key"], event["modifiers"]) for event in events], [
            ("rawKeyDown", "Control", 2), ("rawKeyDown", "Shift", 10), ("rawKeyDown", "Z", 10),
            ("keyUp", "Z", 10), ("keyUp", "Shift", 2), ("keyUp", "Control", 0),
        ])
        self.assertTrue(all("text" not in event for event in events))

    def test_bare_and_shifted_letters_type_their_character(self):
        bare = steps.dom_key_events(steps.parse_key("a"))
        self.assertEqual(bare[0], {"type": "keyDown", "key": "a", "code": "KeyA", "windowsVirtualKeyCode": 65,
                                   "modifiers": 0, "text": "a", "unmodifiedText": "a"})
        self.assertNotIn("text", bare[1])
        shifted = steps.dom_key_events(steps.parse_key("shift+a"))
        self.assertEqual((shifted[1]["key"], shifted[1]["text"], shifted[1]["modifiers"]), ("A", "A", 8))
        space = steps.dom_key_events(steps.parse_key("space"))
        self.assertEqual((space[0]["key"], space[0]["code"], space[0]["text"]), (" ", "Space", " "))


class NamedControlTest(unittest.TestCase):
    def test_aria_roles_map_to_atspi_roles_for_native(self):
        query = {"name": "Send message", "role": "button"}
        self.assertEqual(steps.named_control(query, "electron"), query)
        self.assertEqual(steps.named_control(query, "native"), {"name": "Send message", "roles": ["push button", "button", "toggle button"]})

    def test_per_client_overrides_and_unmapped_roles(self):
        spec = {"electron": {"name": "Attach", "role": "menuitem"}, "native": {"name": "Attach file", "roles": ["menu item"]}}
        self.assertEqual(steps.named_control(spec, "native"), {"name": "Attach file", "roles": ["menu item"]})
        with self.assertRaises(ValueError):
            steps.named_control({"name": "Grid", "role": "grid"}, "native")
        with self.assertRaises(ValueError):
            steps.named_control({"role": "button"}, "electron")


class FaultRuleTest(unittest.TestCase):
    RULE = {"id": "lose", "method": "post", "path": "/api/session/*/message", "when": "after", "action": "drop"}

    def test_defaults_and_per_client_paths(self):
        rule = steps.fault_rule(self.RULE, "native")
        self.assertEqual((rule["method"], rule["status"], rule["times"]), ("POST", 500, 1))
        split = steps.fault_rule({**self.RULE, "path": {"electron": "/api/a", "native": "/api/b"}}, "electron")
        self.assertEqual(split["path"], "/api/a")

    def test_invalid_rules_are_rejected(self):
        for change in ({"when": "during"}, {"action": "slow"}, {"status": 200}, {"times": 0},
                       {"path": "api"}, {"path": "/api/[ab]"}, {"id": ""}, {"method": ""}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                steps.fault_rule({**self.RULE, **change}, "native")

    def test_glob_crosses_segments_and_ignores_queries(self):
        rule = steps.fault_rule(self.RULE, "native")
        self.assertTrue(steps.fault_matches(rule, "POST", "/api/session/ses_1/message?limit=5"))
        self.assertTrue(steps.fault_matches(rule, "post", "http://127.0.0.1:9/api/session/a/b/message"))
        self.assertFalse(steps.fault_matches(rule, "GET", "/api/session/ses_1/message"))
        self.assertFalse(steps.fault_matches(rule, "POST", "/api/session/ses_1/messages"))


class ExpectationTest(unittest.TestCase):
    MESSAGES = {"data": [{"info": {"role": "user", "id": "a"}}, {"info": {"role": "assistant"}}, {"info": {"role": "user", "id": "b"}}]}

    def test_select_where_and_length(self):
        value = steps.select(self.MESSAGES, ["data"])
        self.assertTrue(steps.holds({"where": {"info": {"role": "user"}}, "length": 2}, value))
        self.assertFalse(steps.holds({"where": {"info": {"role": "user"}}, "length": 1}, value))
        self.assertIsNone(steps.select(self.MESSAGES, ["data", 9, "info"]))
        self.assertEqual(steps.select(self.MESSAGES, ["data", -1, "info", "id"]), "b")

    def test_select_wildcard_maps_over_lists_and_drops_misses(self):
        nested = {"data": [{"parts": [{"answers": [["A"]]}, {"text": "x"}]}, {"parts": []}, {"other": 1}]}
        self.assertEqual(steps.select(nested, ["data", "*", "parts", "*", "answers"]), [[["A"]]])
        self.assertEqual(steps.select(nested, ["data", "*", "parts"]), [[{"answers": [["A"]]}, {"text": "x"}], []])
        self.assertEqual(steps.select(self.MESSAGES, ["data", "*", "info", "id"]), ["a", "b"])
        self.assertIsNone(steps.select({"data": {}}, ["data", "*"]))

    def test_contains_equals_and_exists(self):
        self.assertTrue(steps.holds({"contains": "M  src/app.txt"}, "M  src/app.txt\n"))
        self.assertTrue(steps.holds({"contains": {"info": {"id": "b"}}}, self.MESSAGES["data"]))
        self.assertFalse(steps.holds({"contains": {"info": {"id": "c"}}}, self.MESSAGES["data"]))
        self.assertTrue(steps.holds({"equals": "x\r\n"}, "x\r\n"))
        self.assertFalse(steps.holds({"equals": "x\n"}, "x\r\n"))
        self.assertTrue(steps.holds({"exists": False}, None))
        self.assertFalse(steps.holds({"exists": False}, ""))

    def test_where_on_a_non_list_never_holds(self):
        self.assertFalse(steps.holds({"where": {}, "length": 0}, None))

    def test_placeholders(self):
        self.assertEqual(steps.substitute("{fixture}/src/app.txt", {"fixture": "/f"}), "/f/src/app.txt")
        self.assertEqual(steps.SESSION_PLACEHOLDER.findall("/api/session/{session:parity: long}/message"), ["parity: long"])


class StepValidationTest(unittest.TestCase):
    def check(self, step):
        return steps.check_step(step, TARGETS)

    def test_valid_steps(self):
        for step in (
            {"key": "ctrl+Return"}, {"hover": "composer"}, {"clickNamed": {"name": "Send", "role": "button"}},
            {"paste": "你好"}, {"paste": {"text": "x", "until": {"electron": {"expression": "1"}, "native": {"elements": ["a"]}}}},
            {"restart": {}}, {"restart": {"after": "exit", "timeout": 20}},
            {"expect": {"api": "/api/session/{session:a}/message", "select": ["data"], "length": 1}},
            {"expect": {"file": "{fixture}/src/app.txt", "contains": "line"}},
            {"expect": {"git": ["status", "--porcelain=v1"], "equals": ""}},
            {"expect": {"git": ["branch", "--show-current"], "equals": "main\n"}},
            {"expect": {"fault": "lose", "hits": 1}},
            {"fault": {"id": "lose", "method": "POST", "path": "/api/x", "when": "before", "action": "hold"}}, {"release": "lose"},
            {"drop": {"target": "composer", "files": [{"name": "a.txt", "text": "a"}]}},
            {"pick": {"files": [{"name": "a.txt", "text": "a"}]}},
            {"ime": {"compose": "nihao"}}, {"ime": {"compose": "nihao", "preedit": "你好"}}, {"ime": {"commit": "candidate", "text": "你好"}}, {"ime": {"commit": "raw"}},
            {"capture": "c", "targets": ["composer"]}, {"settle": 0.5}, {"type": "x"},
            {"gate": "hold"}, {"gate": "release"}, {"expect": {"gate": "waiting", "equals": 1}},
            {"drag": {"from": "composer", "to": "composer"}},
        ):
            with self.subTest(step=step):
                self.assertEqual(self.check(step), [])

    def test_invalid_steps(self):
        for step in (
            {"key": "ctrl+Enter"}, {"hover": "missing"}, {"clickNamed": {"name": "X", "role": "grid"}},
            {"paste": ""}, {"restart": {"after": "crash"}}, {"wait": {"electron": {}}},
            {"expect": {"api": "/health", "equals": 1}}, {"expect": {"file": "/etc/passwd", "exists": True}},
            {"expect": {"git": ["push"], "equals": ""}}, {"expect": {"git": ["branch", "-D", "main"], "equals": ""}},
            {"expect": {"git": ["status"]}}, {"expect": {"api": "/api/x", "equals": 1, "extra": 1}},
            {"drop": {"target": "composer", "files": [{"name": "../a", "text": "a"}]}},
            {"drop": {"target": "composer", "files": [{"name": "a", "text": "a"}, {"name": "a", "text": "b"}]}},
            {"pick": {"files": [{"name": "a", "text": "a"}, {"name": "b", "text": "b"}]}}, {"pick": {"cancel": True}},
            {"ime": {"compose": "你"}}, {"ime": {"compose": "ni", "preedit": ""}}, {"ime": {"compose": "ni", "text": "x"}}, {"ime": {"commit": "candidate"}}, {"ime": {"commit": "raw", "text": "x"}},
            {"key": "a", "type": "b"}, {"unknown": 1},
            {"gate": "open"}, {"expect": {"gate": "turns", "equals": 1}}, {"expect": {"gate": "waiting"}},
            {"drag": {"from": "composer"}}, {"drag": {"from": "composer", "to": "missing"}},
        ):
            with self.subTest(step=step):
                self.assertTrue(self.check(step))

    def test_scenario_options(self):
        fault = {"fault": {"id": "f", "method": "GET", "path": "/api/x", "when": "after", "action": "fail"}}
        self.assertEqual(steps.check_scenario({"fixture": "git", "model": "perf/think-30s", "steps": [
            fault, {"release": "f"}, {"expect": {"fault": "f", "hits": 1}}, {"expect": {"file": "{fixture}/a", "exists": True}}]}), [])
        self.assertTrue(steps.check_scenario({"fixture": "svn", "steps": []}))
        self.assertTrue(steps.check_scenario({"model": "think", "steps": []}))
        self.assertEqual(steps.check_scenario({"provider": "interactions", "steps": []}), [])
        self.assertTrue(steps.check_scenario({"provider": "selections", "steps": []}))
        self.assertTrue(steps.check_scenario({"steps": [{"release": "f"}, fault]}))
        self.assertTrue(steps.check_scenario({"steps": [{"expect": {"file": "{fixture}/a", "exists": True}}]}))


if __name__ == "__main__":
    unittest.main()
