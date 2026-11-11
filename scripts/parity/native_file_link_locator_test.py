"""Pure proposed locator controls. No GI initialization, bus, display or HTTP.

The geometry fixtures exercise supplied rectangles only. Real shaping and
source hit-test correctness remain a separate qualification gate.
"""

import copy
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest import mock

import native_adapter
import native_files


API = SimpleNamespace(
    CoordType=SimpleNamespace(WINDOW="window"),
    StateType=SimpleNamespace(**{name: name for name in
        ("DEFUNCT", "VISIBLE", "SHOWING", "READ_ONLY", "EDITABLE", "FOCUSABLE", "FOCUSED", "ENABLED", "SENSITIVE")}),
)
URI = "file:///fixture/outside-proof.txt"
PROMPT = f"files-link-proof: {URI}"
RESPONSE = "message:msg_exact_assistant"


class Node:
    """Only installed Atspi Accessible/Component methods used by the locator."""

    def __init__(self, identity="", role="label", name="", children=(), bounds=(10, 20, 80, 18),
                 interfaces=("Accessible", "Component"), states=("VISIBLE", "SHOWING", "ENABLED", "SENSITIVE")):
        self.identity, self.role, self.name = identity, role, name
        self.children = list(children)
        self.bounds, self.interfaces, self.states = bounds, set(interfaces), set(states)
        self.hyperlink = None
        self.action = None
        self.on_clear = None
        self.clears = 0

    def clear_cache(self):
        self.clears += 1
        if self.on_clear:
            self.on_clear()

    def get_accessible_id(self):
        return self.identity

    def get_role_name(self):
        return self.role

    def get_name(self):
        return self.name

    def get_child_count(self):
        return len(self.children)

    def get_child_at_index(self, index):
        return self.children[index]

    def get_interfaces(self):
        return list(self.interfaces)

    def get_state_set(self):
        return SimpleNamespace(contains=self.states.__contains__)

    def get_extents(self, coordinates):
        if coordinates != API.CoordType.WINDOW:
            raise AssertionError("the locator must use actual WINDOW coordinates")
        return SimpleNamespace(**dict(zip(("x", "y", "width", "height"), self.bounds)))

    def get_hyperlink(self):
        return self.hyperlink

    def get_action_iface(self):
        return self.action


class Hyperlink:
    def __init__(self, uri=URI, valid=True, anchors=1):
        self.uri, self.valid, self.anchors = uri, valid, anchors

    def get_uri(self, index):
        if index != 0:
            raise AssertionError("only the sole anchor may be inspected")
        return self.uri

    def get_n_anchors(self):
        return self.anchors

    def is_valid(self):
        return self.valid


class Action:
    def __init__(self, names=("click",)):
        self.names = names

    def get_n_actions(self):
        return len(self.names)

    def get_action_name(self, index):
        return self.names[index]

    def do_action(self, index):
        raise AssertionError("a pointer locator must never activate an accessibility action")


class Client(native_adapter.NativeClient):
    """Use the real adapter's walker, uniqueness query and point conversion."""

    def __init__(self, fragments=None):
        self.fragment = Node("markdown-link-fragment:source-byte-key-93")
        self.link = Node("link-owner", "link", "Outside file", fragments or [self.fragment],
                         interfaces=("Accessible", "Component", "Hyperlink", "Action"))
        self.link.hyperlink, self.link.action = Hyperlink(), Action()
        self.response = Node(RESPONSE, "paragraph", "Outside file\n", [self.link], bounds=(0, 0, 500, 200))
        self.transcript = Node("transcript", "panel", children=[self.response], bounds=(0, 0, 500, 200))
        self.workspace = Node("workspace", "panel", children=[self.transcript], bounds=(0, 0, 800, 600))
        self._app = Node("app", "application", children=[self.workspace])

    def _window(self):
        return "owned-window"


class LocatorTest(unittest.TestCase):
    def setUp(self):
        self.gi = mock.patch("native_adapter._atspi", return_value=API)
        self.gi.start()
        self.addCleanup(self.gi.stop)
        self.origin = (317, -83)
        self.commands = []

        def read_geometry(command, **kwargs):
            self.commands.append(command)
            self.assertEqual(command, ["xdotool", "getwindowgeometry", "--shell", "owned-window"])
            self.assertTrue(kwargs["check"])
            self.assertTrue(kwargs["capture_output"])
            return SimpleNamespace(stdout=f"X={self.origin[0]}\nY={self.origin[1]}\nWIDTH=800\nHEIGHT=600\n")

        self.process = mock.patch("native_adapter.subprocess.run", side_effect=read_geometry)
        self.process.start()
        self.addCleanup(self.process.stop)
        # Both modules import the same standard-library subprocess module.
        self.client = Client()

    def target(self):
        return native_files._file_link_target(self.client, RESPONSE, URI)

    def rejects(self):
        with self.assertRaises(LookupError):
            self.target()

    def test_exact_message_scopes_the_unique_link_and_uses_real_adapter_centre(self):
        unrelated = Node("message:other", "paragraph", "Outside file", [copy.deepcopy(self.client.link)])
        self.client.transcript.children.insert(0, unrelated)
        self.assertIs(self.target(), self.client.fragment)
        self.assertEqual(self.client._screen_point(self.client.fragment), (367, -54))
        self.assertEqual(self.client._app.clears, 2)
        self.assertEqual(len(self.commands), 3)

    def test_missing_or_ambiguous_exact_assistant_response_is_rejected(self):
        for nodes in ([], [self.client.response, copy.deepcopy(self.client.response)]):
            with self.subTest(count=len(nodes)):
                self.client.transcript.children = nodes
                self.rejects()

    def test_same_label_unrelated_paragraph_cannot_replace_the_exact_response(self):
        self.client.transcript.children = [Node("message:other", "paragraph", "Outside file", [self.client.link])]
        self.rejects()

    def test_message_label_role_and_visibility_are_required(self):
        for field, value in (("name", "Outside file elsewhere"), ("role", "label"),
                             ("states", {"VISIBLE"}), ("states", {"VISIBLE", "SHOWING", "DEFUNCT"})):
            with self.subTest(field=field, value=value):
                old = getattr(self.client.response, field)
                setattr(self.client.response, field, value)
                self.rejects()
                setattr(self.client.response, field, old)

    def test_missing_ambiguous_or_unrelated_links_are_rejected(self):
        for children in ([], [self.client.link, copy.deepcopy(self.client.link)],
                         [Node("not-link", "paragraph", "Outside file")],
                         [Node("message:nested", "paragraph", "Outside file", [self.client.link])]):
            with self.subTest(count=len(children)):
                self.client.response.children = children
                self.rejects()

    def test_wrong_destination_invalid_anchor_and_missing_action_are_rejected(self):
        for hyperlink, action in ((Hyperlink("file:///fixture/other.txt"), Action()),
                                  (Hyperlink(valid=False), Action()), (Hyperlink(anchors=0), Action()),
                                  (Hyperlink(anchors=2), Action()), (None, Action()),
                                  (Hyperlink(), None), (Hyperlink(), Action(())),
                                  (Hyperlink(), Action(("other",)))):
            with self.subTest(hyperlink=hyperlink, action=action):
                self.client.link.hyperlink, self.client.link.action = hyperlink, action
                self.rejects()

    def test_link_interfaces_and_current_state_are_required(self):
        for missing in ("Hyperlink", "Action"):
            with self.subTest(missing=missing):
                self.client.link.interfaces.remove(missing)
                self.rejects()
                self.client.link.interfaces.add(missing)
        for states in ({"VISIBLE"}, {"SHOWING"}, {"VISIBLE", "SHOWING", "DEFUNCT"}):
            self.client.link.states = states
            self.rejects()

    def test_fragment_visibility_passive_role_and_interfaces_are_required(self):
        for field, value in (("role", "paragraph"), ("identity", "unrelated-label"),
                             ("interfaces", {"Accessible"}),
                             ("interfaces", {"Component", "Action"}),
                             ("interfaces", {"Component", "EditableText"}),
                             ("interfaces", {"Component", "Hyperlink"}),
                             ("states", {"VISIBLE", "ENABLED", "SENSITIVE"}),
                             ("states", {"SHOWING", "ENABLED", "SENSITIVE"}),
                             ("states", {"VISIBLE", "SHOWING", "DEFUNCT"}),
                             ("states", {"VISIBLE", "SHOWING", "EDITABLE"}),
                             ("states", {"VISIBLE", "SHOWING", "FOCUSABLE"}),
                             ("states", {"VISIBLE", "SHOWING", "FOCUSED"})):
            with self.subTest(field=field, value=value):
                old = getattr(self.client.fragment, field)
                setattr(self.client.fragment, field, value)
                self.rejects()
                setattr(self.client.fragment, field, old)

    def test_passive_label_without_read_only_state_is_eligible(self):
        fragment = self.client.fragment
        self.assertEqual(fragment.get_role_name(), "label")
        self.assertEqual(fragment.states, {"VISIBLE", "SHOWING", "ENABLED", "SENSITIVE"})
        self.assertFalse(fragment.get_state_set().contains(API.StateType.READ_ONLY))
        self.assertIs(self.target(), fragment)

    def test_editable_fragment_is_rejected_without_read_only_state(self):
        self.client.fragment.states.add("EDITABLE")
        self.assertNotIn("READ_ONLY", self.client.fragment.states)
        self.rejects()

    def test_focusable_fragment_is_rejected_without_read_only_state(self):
        self.client.fragment.states.add("FOCUSABLE")
        self.assertNotIn("READ_ONLY", self.client.fragment.states)
        self.rejects()

    def test_focused_fragment_is_rejected_even_without_focusable_state(self):
        self.client.fragment.states.add("FOCUSED")
        self.assertNotIn("FOCUSABLE", self.client.fragment.states)
        self.assertNotIn("READ_ONLY", self.client.fragment.states)
        self.rejects()

    def test_actionable_fragment_is_rejected_without_read_only_state(self):
        self.client.fragment.interfaces.add("Action")
        self.client.fragment.action = Action()
        self.assertNotIn("READ_ONLY", self.client.fragment.states)
        self.rejects()

    def test_zero_tiny_nonintegral_and_out_of_view_fragment_bounds_are_rejected(self):
        for bounds in ((10, 20, 0, 18), (10, 20, -1, 18), (10, 20, 80, 0),
                       (10, 20, 1, 18), (10, 20, 80, 1), (10.5, 20, 80, 18),
                       (-1, 20, 80, 18), (450, 20, 80, 18), (10, 195, 80, 18)):
            with self.subTest(bounds=bounds):
                self.client.fragment.bounds = bounds
                self.rejects()

    def test_actual_current_clipped_fragment_can_be_selected_without_offset(self):
        self.client.fragment.bounds = (0, 20, 15, 18)
        self.assertIs(self.target(), self.client.fragment)
        self.assertEqual(self.client._screen_point(self.client.fragment), (324, -54))

    def test_wrapped_bidi_fragments_use_an_eligible_stable_key_not_union_centre(self):
        hidden = Node("markdown-link-fragment:source-byte-key-7", states=())
        first = Node("markdown-link-fragment:source-byte-key-93", bounds=(300, 20, 80, 18))
        second = Node("markdown-link-fragment:source-byte-key-12", bounds=(10, 80, 40, 18))
        self.client.link.children = [hidden, first, second]
        self.client.link.bounds = (10, 20, 370, 78)
        self.assertIs(self.target(), first)
        self.assertEqual(self.client._screen_point(first), (657, -54))
        self.assertNotEqual(self.client._screen_point(first), (512, -24))
        hidden.states.add("VISIBLE")
        hidden.states.add("SHOWING")
        hidden.bounds = (0, 0, 1, 18)
        self.assertIs(self.target(), first)
        self.assertEqual(first.get_accessible_id(), "markdown-link-fragment:source-byte-key-93")

    def test_duplicate_supplied_fragment_keys_are_rejected_before_filtering(self):
        stale = copy.deepcopy(self.client.fragment)
        stale.states = set()
        self.client.link.children.append(stale)
        self.rejects()

    def test_refresh_re_resolves_replacement_node_and_current_bounds(self):
        old = self.client.fragment
        fresh = copy.deepcopy(old)

        def replace_on_second_refresh():
            if self.client._app.clears == 2:
                self.client.link.children = [fresh]

        self.client._app.on_clear = replace_on_second_refresh
        self.assertIs(self.target(), fresh)
        self.assertIsNot(fresh, old)

    def test_refresh_rejects_geometry_membership_state_destination_or_action_change(self):
        mutations = [lambda client: setattr(client.fragment, "bounds", (200, 20, 80, 18)),
                     lambda client: setattr(client.link, "children", []),
                     lambda client: client.fragment.states.add("DEFUNCT"),
                     lambda client: client.fragment.states.discard("SHOWING"),
                     lambda client: setattr(client.link.hyperlink, "uri", "file:///wrong"),
                     lambda client: setattr(client.link.action, "names", ()),
                     lambda client: setattr(client.response, "identity", "message:replacement")]
        for change in mutations:
            with self.subTest(change=change):
                self.client = Client()
                self.client._app.on_clear = lambda: change(self.client) if self.client._app.clears == 2 else None
                self.rejects()

    def test_adapter_emission_must_be_the_unmodified_centre_in_the_current_x11_frame(self):
        real_point = self.client._screen_point

        def move_origin_after_emission(node):
            point = real_point(node)
            self.origin = (400, 200)
            return point

        with mock.patch.object(self.client, "_screen_point", side_effect=move_origin_after_emission):
            self.rejects()
        with mock.patch.object(self.client, "_screen_point", return_value=(400, 200)):
            self.rejects()

    def test_missing_or_nonvisible_viewport_geometry_is_rejected(self):
        for node in (self.client.workspace, self.client.transcript):
            with self.subTest(identity=node.identity):
                node.states.discard("SHOWING")
                self.rejects()
                node.states.add("SHOWING")
                node.bounds = (0, 0, 20, 20)
                self.rejects()
                node.bounds = (0, 0, 800, 600)


class ResponseBindingTest(unittest.TestCase):
    def setUp(self):
        self.sessions = [{"id": "ses_new", "location": {"directory": "/fixture/project"}}]
        self.messages = [{"id": "msg_user", "type": "user", "text": PROMPT},
                         {"id": "msg_exact_assistant", "type": "assistant",
                          "content": [{"type": "text", "text": f"[Outside file]({URI})"}]}]
        self.routes = []

        def request(route):
            self.routes.append(route)
            if route == "/api/session?limit=100":
                return {"data": self.sessions}
            if route == "/api/session/ses_new/message?limit=100":
                return {"data": self.messages}
            raise AssertionError(f"unexpected request {route}")

        self.session = SimpleNamespace(project=Path("/fixture/project"), request=request)

    def response(self):
        return native_files._file_link_response(self.session, {"ses_old"}, PROMPT, URI)

    def test_exact_v2_assistant_id_is_bound_without_a_guessed_parent_field(self):
        self.assertEqual(self.response(), RESPONSE)
        self.assertEqual(self.routes, ["/api/session?limit=100", "/api/session/ses_new/message?limit=100"])

    def test_missing_or_ambiguous_new_session_is_rejected(self):
        for sessions in ([], self.sessions * 2, [{"id": "ses_old"}],
                         [{"id": "ses_new", "location": {"directory": "/wrong"}}]):
            with self.subTest(sessions=sessions):
                self.sessions = sessions
                with self.assertRaises(LookupError):
                    self.response()

    def test_missing_ambiguous_wrong_prompt_or_nonassistant_reply_is_rejected(self):
        original = copy.deepcopy(self.messages)
        cases = [[], original[:1], original + [copy.deepcopy(original[1])],
                 original + [copy.deepcopy(original[0])],
                 [dict(original[0], text="different prompt"), original[1]],
                 [original[0], dict(original[1], type="user")],
                 [original[0], dict(original[1], content=[{"type": "text", "text": "Outside file"}])],
                 [original[0], dict(original[1], id="")]]
        for messages in cases:
            with self.subTest(messages=messages):
                self.messages = messages
                with self.assertRaises(LookupError):
                    self.response()

    def test_request_failure_never_becomes_empty_success(self):
        self.session.request = mock.Mock(side_effect=RuntimeError("server unavailable"))
        with self.assertRaisesRegex(RuntimeError, "server unavailable"):
            self.response()


if __name__ == "__main__":
    unittest.main()
