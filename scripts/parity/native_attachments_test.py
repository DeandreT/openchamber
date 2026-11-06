"""Pure attachment-driver contracts. No app, server, display or clipboard starts."""

import copy
import hashlib
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import native_attachments as attachments


class Node:
    """Only metadata for selector/Component-boundary tests, never a workflow."""

    def __init__(self, name="", role="grouping", key="", component=None):
        self.name, self.role, self.key, self.component = name, role, key, component

    def clear_cache(self):
        pass

    def get_name(self):
        return self.name

    def get_role_name(self):
        return self.role

    def get_accessible_id(self):
        return self.key

    def get_state_set(self):
        return self

    def get_states(self):
        return []

    def get_component_iface(self):
        return self.component


def journal():
    expected = {attachments.TEXT_NAME: attachments.TEXT_BYTES, attachments.PNG_NAME: b"synthetic PNG bytes"}
    files, catalog = [], []
    for index, (name, data) in enumerate(expected.items(), start=1):
        identity = f"{index:032x}"
        target = f"/tmp/opencode/fixture/state/tmp/openchamber/{index:016x}/{name}"
        files.append({"id": identity, "name": name, "media_type": "image/png" if name.endswith(".png") else "text/plain",
                      "byte_len": len(data), "source_document_id": None, "content": "prepared"})
        catalog.append({"id": identity, "blob": {"id": identity, "byte_len": len(data), "sha256": list(hashlib.sha256(data).digest())},
                        "staging": {"runtime": "runtime-fixture", "directory": "/tmp/opencode/fixture/project",
                                    "temporary_directory": "/tmp/opencode/fixture/state/tmp", "server_path": target,
                                    "uri": Path(target).as_uri(), "filename": name, "phase": "ready"}})
    return {"version": 7, "drafts": [{"owner": {"runtime": "runtime-fixture", "directory": "/tmp/opencode/fixture/project",
                                               "target": {"session": "ses_fixture"}}, "text": attachments.DRAFT,
                                    "start": 0, "end": 0, "content": {"owned": {"files": files, "agents": [], "skills": [],
                                                                               "metadata_json": None, "context": []}}}],
            "attempts": [], "creations": [], "selections": [], "attachments": catalog,
            "preparations": [], "retired_blobs": [], "starter_captures": []}, expected


class MetadataBoundary(unittest.TestCase):
    def test_text_tree_does_not_request_component_or_geometry(self):
        class NonComponent(Node):
            def get_component_iface(self):
                self.fail_if_queried()

            def fail_if_queried(self):
                raise AssertionError("text evidence must not ask for Component")

        self.assertEqual(attachments.text_node(NonComponent("plain paragraph", "paragraph")),
                         {"id": "", "name": "plain paragraph", "role": "paragraph", "states": []})

    def test_noncomponent_geometry_is_explicitly_unsupported(self):
        class Client:
            def _rect(self, node):
                raise AssertionError("a non-Component node must not reach _rect")

        value = attachments.guarded_geometry(Client(), Node())
        self.assertEqual(value["status"], "unsupported")

    def test_component_query_failure_retains_an_unobserved_outcome(self):
        class StaleNode(Node):
            def get_component_iface(self):
                raise RuntimeError("Component query refused")

        self.assertEqual(attachments.guarded_geometry(None, StaleNode()),
                         {"status": "unobserved", "error": "Component query refused"})

    def test_named_buttons_use_observed_roles_and_toolkit_ids(self):
        button = Node("Add attachment", "push button", "toolkit-generated-identity")
        label = Node("Add attachment", "label", "composer-attach")
        self.assertIs(attachments.named_control([label, button], "Add attachment", attachments.BUTTON_ROLES), button)
        self.assertIsNone(attachments.named_control([label], "Add attachment", attachments.BUTTON_ROLES))
        with self.assertRaises(attachments.Unobserved):
            attachments.named_control([button, button], "Add attachment", attachments.BUTTON_ROLES)

    def test_file_rows_use_exact_filename_and_exclude_action_ids(self):
        name = attachments.TEXT_NAME
        group = Node(name + ". Attachment needs preparation.", key="attachment-" + "1" * 32)
        action = Node(name + ". Attachment needs preparation.", "push button", group.key + "-remove")
        other = Node("prefix-" + name + ". ", key="attachment-input-4")
        self.assertIs(attachments.file_row([action, group, other], name), group)
        pending = Node(name + ". Loading...", key="attachment-input-5")
        self.assertIs(attachments.file_row([pending], name), pending)
        with self.assertRaisesRegex(ValueError, "duplicate rendered"):
            attachments.file_row([pending, group], name)


class ByteAndPathContracts(unittest.TestCase):
    def test_byte_checks_retain_both_sides_of_same_size_corruption(self):
        receipt = attachments.byte_check(b"abc", b"abd")
        self.assertFalse(receipt["matches"])
        self.assertEqual(receipt["actualBytes"], receipt["expectedBytes"])
        self.assertNotEqual(receipt["actualSha256"], receipt["expectedSha256"])
        self.assertTrue(attachments.byte_check(b"\x00\xff\r\n", b"\x00\xff\r\n")["matches"])

    def test_file_list_is_local_crlf_uri_bytes_with_percent_encoding(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="attachments-contract-") as temporary:
            root = Path(temporary)
            paths = [root / "text name.txt", root / "你好.png"]
            for path in paths:
                path.write_bytes(b"fixture")
            self.assertEqual(attachments.uri_list(paths, root), (paths[0].as_uri() + "\r\n" + paths[1].as_uri() + "\r\n").encode())
            self.assertIn(b"%20", attachments.uri_list(paths, root))
            self.assertIn(b"%E4%BD%A0", attachments.uri_list(paths, root))
            for selected in ([], [paths[0], paths[0]], [root / "missing"], [root]):
                with self.subTest(selected=selected), self.assertRaises(ValueError):
                    attachments.uri_list(selected, root)
            link = root / "link.txt"
            link.symlink_to(paths[0])
            with self.assertRaises(ValueError):
                attachments.uri_list([link], root)
            with self.assertRaises(ValueError):
                attachments.uri_list([paths[0]], root / "other-owner")

    def test_server_targets_require_the_exact_temporary_root_and_safe_allocation(self):
        temporary = Path("/tmp/opencode/fixture/state/tmp")
        target = temporary / "openchamber/0123456789abcdef/native-attachment.txt"
        self.assertEqual(attachments.owned_server_path(str(target), temporary, attachments.TEXT_NAME), target)
        for value in (str(temporary / "elsewhere/0123456789abcdef/native-attachment.txt"),
                      str(temporary / "openchamber/not-an-allocation/native-attachment.txt"),
                      str(target) + "/../native-attachment.txt", str(target.with_name("other.txt")), "relative/native-attachment.txt"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                attachments.owned_server_path(value, temporary, attachments.TEXT_NAME)

    def test_output_never_reuses_evidence_or_writes_under_source(self):
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="attachments-output-contract-") as temporary:
            with self.assertRaises(ValueError):
                attachments.validate_output(Path(temporary))
        for path in (Path("/tmp/attachments"), Path("/home/deandre/projects/rust/OpenChamber/artifacts/attachments")):
            with self.assertRaises(ValueError):
                attachments.validate_output(path)


class CaptureContract(unittest.TestCase):
    def capture(self, document, expected):
        return attachments.draft_capture(document, "ses_fixture", Path("/tmp/opencode/fixture/project"), expected)

    def test_v7_capture_reads_exact_owned_metadata_without_mutating_the_journal(self):
        document, expected = journal()
        original = copy.deepcopy(document)
        result = self.capture(document, expected)
        self.assertEqual(document, original)
        self.assertEqual(set(result), set(expected))
        self.assertEqual(result[attachments.PNG_NAME]["mediaType"], "image/png")
        self.assertEqual(result[attachments.TEXT_NAME]["byteLen"], len(attachments.TEXT_BYTES))
        self.assertEqual(result[attachments.TEXT_NAME]["staging"]["phase"], "ready")

    def test_v8_capture_reads_unchanged_attachment_ownership_beside_queue_metadata(self):
        document, expected = journal()
        document.update(version=8, queue={"operations": [], "objects": [], "retired": []})
        original = copy.deepcopy(document)
        result = self.capture(document, expected)
        self.assertEqual(set(result), set(expected))
        self.assertEqual(document, original, "capture observation never changes journal ownership")

    def test_v8_snapshot_waits_for_files_and_staging_before_checking_exact_bytes(self):
        document, expected = journal()
        document.update(version=8, queue={"operations": [], "objects": [], "retired": []})
        no_files = copy.deepcopy(document)
        no_files["drafts"][0]["content"]["owned"]["files"] = []
        no_staging = copy.deepcopy(document)
        no_staging["attachments"][0]["staging"] = None
        with tempfile.TemporaryDirectory(dir="/tmp/opencode", prefix="attachment-snapshot-") as temporary:
            root = Path(temporary)
            interaction = object.__new__(attachments.AttachmentInteraction)
            interaction.journal = root / "drafts-and-sends.json"
            interaction.args = SimpleNamespace(output=root)
            interaction.fixture = SimpleNamespace(project=Path("/tmp/opencode/fixture/project"))
            interaction.session = "ses_fixture"
            interaction.report = {}
            spool = root / "attachment-spool"
            spool.mkdir()
            for file in document["drafts"][0]["content"]["owned"]["files"]:
                (spool / f"blob-{file['id']}.bin").write_bytes(expected[file["name"]])

            def observe_until(message, read, timeout):
                for pending in (no_files, no_staging):
                    interaction.journal.write_text(attachments.json.dumps(pending))
                    self.assertIsNone(read(), "incomplete durable publication must remain pending")
                interaction.journal.write_text(attachments.json.dumps(document))
                return read()

            with patch.object(attachments, "until", side_effect=observe_until):
                captures = interaction.snapshot("prepared", expected)
            self.assertEqual(set(captures), set(expected))
            self.assertTrue(all(receipt["matches"] for receipt in interaction.report["byteChecks"].values()))

    def test_missing_metadata_is_unobserved_and_never_invented(self):
        for change in (lambda doc: doc.update(version=6),
                       lambda doc: doc.update(version=9),
                       lambda doc: doc.pop("attachments"),
                       lambda doc: doc["drafts"][0]["content"]["owned"]["files"][0].pop("media_type"),
                       lambda doc: doc["attachments"][0].update(staging=None),
                       lambda doc: doc["attachments"][0]["staging"].pop("temporary_directory")):
            document, expected = journal()
            change(document)
            with self.assertRaises(attachments.Unobserved):
                self.capture(document, expected)

    def test_wrong_owner_changed_bytes_or_conflicting_identity_fail(self):
        for change in (lambda doc: doc["drafts"][0]["owner"].update(directory="/somewhere/else"),
                       lambda doc: doc["drafts"][0].update(text="changed draft"),
                       lambda doc: doc["drafts"].append(copy.deepcopy(doc["drafts"][0])),
                       lambda doc: doc["attachments"].append(copy.deepcopy(doc["attachments"][0])),
                       lambda doc: doc["attachments"][0]["blob"]["sha256"].reverse(),
                       lambda doc: doc["attachments"][0]["staging"].update(runtime="another-runtime"),
                       lambda doc: doc["attachments"][0]["staging"].update(uri="file:///different-target"),
                       lambda doc: doc["attachments"][0]["staging"].update(phase="invented-success"),
                       lambda doc: doc["drafts"][0]["content"]["owned"]["files"][0].update(content={"reference": {"uri": "file:///text"}})):
            document, expected = journal()
            change(document)
            with self.assertRaises(ValueError):
                self.capture(document, expected)

    def test_failed_write_phase_remains_distinct_from_ready(self):
        document, expected = journal()
        document["attachments"][0]["staging"]["phase"] = "uncertain"
        result = self.capture(document, expected)
        self.assertEqual(result[attachments.TEXT_NAME]["staging"]["phase"], "uncertain")
        self.assertEqual(result[attachments.PNG_NAME]["staging"]["phase"], "ready")

    def test_prompt_requires_exact_names_and_retained_server_uris(self):
        document, expected = journal()
        captures = self.capture(document, expected)
        prompt = {"id": "msg_fixture", "text": attachments.DRAFT,
                  "files": [{"name": name, "uri": item["staging"]["uri"]} for name, item in captures.items()]}
        self.assertEqual(attachments.verify_prompt(prompt, captures), "msg_fixture")
        for change in (lambda body: body.update(text="another draft"), lambda body: body.update(id=""),
                       lambda body: body["files"].pop(), lambda body: body["files"][0].update(uri="file:///other"),
                       lambda body: body["files"][0].update(name="invented.txt")):
            body = copy.deepcopy(prompt)
            change(body)
            with self.assertRaises(ValueError):
                attachments.verify_prompt(body, captures)


class ProofAccounting(unittest.TestCase):
    def test_menu_only_and_optional_picker_outcomes_cannot_qualify_the_workflow(self):
        proofs = {"menu": {"status": "passed"}, "picker": {"status": "unsupported"}}
        self.assertEqual(attachments.final_status(proofs, []), "incomplete")
        self.assertEqual(attachments.final_status({}, []), "incomplete")

    def test_all_required_observations_and_successful_cleanup_are_needed(self):
        proofs = {key: {"status": "passed"} for key in attachments.REQUIRED_PROOFS}
        self.assertEqual(attachments.final_status(proofs, []), "passed")
        self.assertEqual(attachments.final_status(proofs, ["fixture process remains"]), "failed")
        for key in attachments.REQUIRED_PROOFS:
            for status, expected in (("failed", "failed"), ("unobserved", "incomplete"), ("unsupported", "incomplete")):
                with self.subTest(key=key, status=status):
                    changed = copy.deepcopy(proofs)
                    changed[key]["status"] = status
                    self.assertEqual(attachments.final_status(changed, []), expected)


if __name__ == "__main__":
    unittest.main()
