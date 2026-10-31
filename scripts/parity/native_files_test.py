import argparse
from pathlib import Path
import unittest
from unittest import mock

import native_files


class GitFixtureServerTest(unittest.TestCase):
    def test_the_server_is_started_on_a_fresh_root_with_the_git_fixture(self):
        args = argparse.Namespace(dist=Path("/dist"), opencode_binary=Path("/opencode"))
        root = Path("/tmp/opencode/native-files-test-root")
        with mock.patch("native_files.subprocess.Popen") as popen, mock.patch("builtins.open", mock.mock_open()):
            server = native_files.GitFixtureServer(root, args)
        command = popen.call_args.args[0]
        self.assertIn("--git-fixture", command)
        self.assertEqual(command[command.index("--root") + 1], str(root))
        self.assertNotIn("--fresh", command, "no seed: the project is the fixture repository")
        self.assertEqual(server.project, root / "project")

    def test_every_file_scenario_is_listed_once(self):
        session = mock.Mock()
        session.project = Path("/p")
        names = [name for name, _ in native_files.file_steps(session)]
        self.assertEqual(len(names), len(set(names)))
        for required in ("edit-save", "external-clean", "external-dirty", "deleted-open", "rename-conflict",
                         "search-contents", "large", "dirty-switch"):
            self.assertIn(required, names)

    def test_rename_waits_for_the_recreated_file_in_the_tree(self):
        session = mock.Mock()
        session.project = Path("/p")
        steps = dict(native_files.file_steps(session))
        with mock.patch.object(Path, "exists", return_value=True), mock.patch("native_files.time.sleep"):
            steps["rename-conflict"]()
        self.assertEqual(session.mock_calls[:5], [
            mock.call.client.click("files-refresh"),
            mock.call.wait_id("files-row:/p/README.md"),
            mock.call.client.click("files-row:/p/notes"),
            mock.call.wait_id("files-row:/p/README.md"),
            mock.call.client.click("files-row:/p/README.md"),
        ])


if __name__ == "__main__":
    unittest.main()
