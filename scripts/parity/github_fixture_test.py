"""The GitHub fixture's repository answers and its host boundary.

Run from this directory: `python3 -m unittest github_fixture_test`.
"""

import socket
import tempfile
import unittest
from pathlib import Path

import github_fixture


class RepositoryTests(unittest.TestCase):
    def setUp(self):
        self.repository = github_fixture.Repository()

    def numbers(self, query: str) -> list[int]:
        return [node["number"] for node in self.repository.search(query, 30)["search"]["nodes"]]

    def test_search_keeps_kind_state_and_text_apart(self):
        self.assertEqual(self.numbers("repo:acme/widgets is:issue is:open sort:updated-desc"), [1, 2])
        self.assertEqual(self.numbers("repo:acme/widgets is:pr is:open sort:updated-desc"), [3, 4])
        self.assertEqual(self.numbers("repo:acme/widgets is:issue is:open dark"), [2])
        # No closed issue exists, so the Closed filter is empty.
        self.assertEqual(self.numbers("repo:acme/widgets is:issue is:closed"), [])
        self.assertEqual(self.numbers("repo:acme/widgets is:issue author:@me"), [])

    def test_a_pull_request_detail_carries_its_size_review_and_checks(self):
        node = self.repository.detail(3)["repository"]["issueOrPullRequest"]
        self.assertEqual(node["__typename"], "PullRequest")
        self.assertEqual((node["additions"], node["deletions"], node["changedFiles"]), (12, 3, 2))
        self.assertEqual(node["reviewDecision"], "REVIEW_REQUIRED")
        self.repository.items[3]["reviews"].append({"id": 1, "state": "APPROVED", "body": "", "createdAt": None})
        node = self.repository.detail(3)["repository"]["issueOrPullRequest"]
        self.assertEqual(node["reviewDecision"], "APPROVED")
        self.assertIsNone(self.repository.detail(99)["repository"]["issueOrPullRequest"])

    def test_rest_shapes_name_the_project_and_its_head(self):
        pull = self.repository.rest_pull(3)
        self.assertEqual(pull["base"]["repo"]["html_url"], "https://github.com/acme/widgets")
        self.assertEqual(pull["head"]["sha"], self.repository.items[3]["sha"])
        self.assertIn("pull_request", self.repository.rest_issue(3))
        self.assertNotIn("pull_request", self.repository.rest_issue(1))


class BoundaryTests(unittest.TestCase):
    def test_the_proxy_tunnels_only_github(self):
        Path("/tmp/opencode").mkdir(exist_ok=True)
        with tempfile.TemporaryDirectory(dir="/tmp/opencode") as directory:
            fixture = github_fixture.Fixture(Path(directory))
            self.addCleanup(fixture.stop)
            port = fixture.server.server_address[1]
            for host, expected in (("example.com:443", b" 403 "), ("github.com:22", b" 403 "),
                                   ("api.github.com:443", b" 200 ")):
                with socket.create_connection(("127.0.0.1", port), timeout=5) as connection:
                    connection.sendall(f"CONNECT {host} HTTP/1.1\r\nHost: {host}\r\n\r\n".encode())
                    self.assertIn(expected, connection.recv(64), host)


if __name__ == "__main__":
    unittest.main()
