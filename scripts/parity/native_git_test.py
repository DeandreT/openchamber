import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import unittest
import gzip
import http.client
import socket
from unittest import mock

from git_http_remote import GitHttpRemote
from native_files import git, porcelain
from native_git import change_id


def run(cwd, *args):
    env = {"PATH": os.environ["PATH"], "HOME": str(cwd), "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null"}
    return subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True, text=True, timeout=60).stdout


class GitHttpRemoteTest(unittest.TestCase):
    def setUp(self):
        self.root = Path(tempfile.mkdtemp(prefix="git-http-remote-"))
        run(self.root, "init", "-q", "--bare", "-b", "main", str(self.root / "remote.git"))
        self.work = self.root / "work"
        self.work.mkdir()
        run(self.work, "init", "-q", "-b", "main")
        run(self.work, "config", "user.name", "Test")
        run(self.work, "config", "user.email", "test@example.invalid")
        (self.work / "a.txt").write_text("a\n")
        run(self.work, "add", "a.txt")
        run(self.work, "commit", "-q", "-m", "first")
        self.remote = GitHttpRemote(self.root)
        run(self.work, "remote", "add", "origin", f"{self.remote.url}/remote.git")

    def tearDown(self):
        self.remote.stop()
        subprocess.run(["rm", "-rf", str(self.root)], check=True)

    def test_a_push_and_a_clone_go_through_smart_http(self):
        run(self.work, "push", "-q", "origin", "main")
        self.assertEqual(self.remote.pushes(), 1)
        self.assertEqual(run(self.root / "remote.git", "rev-parse", "main"), run(self.work, "rev-parse", "HEAD"))
        run(self.root, "clone", "-q", f"{self.remote.url}/remote.git", str(self.root / "clone"))
        self.assertTrue((self.root / "clone" / "a.txt").exists())

    def test_a_held_fetch_continues_on_the_same_request_after_release(self):
        run(self.work, "push", "-q", "origin", "main")
        self.remote.hold_fetches()
        done = threading.Event()
        outcomes = []
        def clone():
            try:
                run(self.root, "clone", "-q", f"{self.remote.url}/remote.git", str(self.root / "held-clone"))
                outcomes.append("accepted")
            finally:
                done.set()
        worker = threading.Thread(target=clone, daemon=True)
        worker.start()
        try:
            deadline = time.monotonic() + 15
            while not self.remote.fetches() and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertGreater(self.remote.fetches(), 0)
            self.assertFalse(done.is_set(), "the fetch waits for the owned gate")
        finally:
            self.remote.release_fetches()
        self.assertTrue(done.wait(15))
        worker.join()
        self.assertEqual(outcomes, ["accepted"])
        self.assertEqual((self.root / "held-clone/a.txt").read_text(), "a\n")

    def test_chunked_gzip_reaches_backend_decoded(self):
        payload = b"Git protocol bytes\x00" * 50
        compressed = gzip.compress(payload)
        result = subprocess.CompletedProcess([], 0, b"Content-Type: text/plain\r\n\r\nok", b"")
        with mock.patch("git_http_remote.subprocess.run", return_value=result) as backend:
            connection = http.client.HTTPConnection("127.0.0.1", self.remote.server.server_port)
            connection.request("POST", "/remote.git/git-upload-pack",
                               body=iter([compressed[:13], compressed[13:]]),
                               headers={"Content-Encoding": "gzip"}, encode_chunked=True)
            response = connection.getresponse()
            self.assertEqual(response.status, 200)
            self.assertEqual(response.read(), b"ok")
            connection.close()
            self.assertEqual(backend.call_args.kwargs["input"], payload)
            self.assertEqual(backend.call_args.kwargs["env"]["CONTENT_LENGTH"], str(len(payload)))

    def test_invalid_framing_and_gzip_never_reach_backend(self):
        for headers, body in [
            ({"Transfer-Encoding": "chunked"}, b"z\r\n"),
            ({"Content-Encoding": "gzip"}, b"\x1f\x8b"),
            ({"Content-Encoding": "br"}, b"unsupported"),
        ]:
            with self.subTest(headers=headers), mock.patch("git_http_remote.subprocess.run") as backend:
                connection = http.client.HTTPConnection("127.0.0.1", self.remote.server.server_port)
                connection.request("POST", "/remote.git/git-upload-pack", body=body, headers=headers)
                response = connection.getresponse()
                self.assertEqual(response.status, 400)
                response.read()
                connection.close()
                backend.assert_not_called()

    def test_a_held_push_is_dropped_on_release(self):
        run(self.work, "push", "-q", "origin", "main")
        before = run(self.root / "remote.git", "rev-parse", "main")
        (self.work / "a.txt").write_text("changed\n")
        run(self.work, "commit", "-qam", "second")
        self.remote.hold_pushes()
        done = threading.Event()
        outcomes = []
        def push():
            try:
                run(self.work, "push", "-q", "origin", "main")
                outcomes.append("accepted")
            except subprocess.CalledProcessError:
                outcomes.append("refused")
            finally:
                done.set()
        worker = threading.Thread(target=push, daemon=True)
        worker.start()
        deadline = time.monotonic() + 30
        while self.remote.pushes() < 2 and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertEqual(self.remote.pushes(), 2, "the push reached the remote")
        self.assertFalse(done.wait(0.5), "the held push does not finish")
        self.remote.release()
        self.assertTrue(done.wait(30), "a released push finishes")
        worker.join()
        self.assertEqual(outcomes, ["refused"])
        self.assertEqual(run(self.root / "remote.git", "rev-parse", "main"), before)

    def test_body_budgets_and_timeout_close_connection(self):
        bomb = gzip.compress(b"x" * 4097)
        cases = [
            (b"Content-Length: 1025\r\n", b"x" * 1025),
            (b"Transfer-Encoding: chunked\r\n", b"401\r\n" + b"x" * 1025 + b"\r\n0\r\n\r\n"),
            (b"Transfer-Encoding: chunked\r\n", b"200\r\n" + b"x" * 512 + b"\r\n201\r\n" + b"x" * 513 + b"\r\n0\r\n\r\n"),
            (b"Transfer-Encoding: chunked\r\n", b"0\r\n" + b"X: value\r\n" * 8 + b"\r\n"),
            (b"Content-Encoding: gzip\r\nContent-Length: " + str(len(bomb)).encode() + b"\r\n", bomb),
            (b"Content-Length: 5\r\n", b"ab"),
        ]
        with mock.patch.multiple("git_http_remote", MAX_ENCODED_BODY=1024,
                                 MAX_DECODED_BODY=4096, MAX_TRAILERS=32, BODY_TIMEOUT=0.1):
            for headers, body in cases:
                result = subprocess.CompletedProcess([], 0, b"Content-Type: text/plain\r\n\r\nok", b"")
                with self.subTest(headers=headers, body=body[:30]), mock.patch("git_http_remote.subprocess.run", return_value=result) as backend:
                    with socket.create_connection(("127.0.0.1", self.remote.server.server_port), timeout=2) as connection:
                        connection.sendall(b"POST /remote.git/git-upload-pack HTTP/1.1\r\nHost: localhost\r\n" + headers + b"\r\n" + body)
                        response = http.client.HTTPResponse(connection)
                        response.begin()
                        self.assertEqual(response.status, 400)
                        self.assertEqual(response.getheader("Connection"), "close")
                        response.read()
                        self.assertEqual(connection.recv(1), b"")
                    backend.assert_not_called()


class HelpersTest(unittest.TestCase):
    def test_porcelain_is_sorted_and_git_ignores_global_configuration(self):
        root = Path(tempfile.mkdtemp(prefix="git-helpers-"))
        try:
            project = root / "project"
            project.mkdir()
            git(project, "init", "-q")
            (project / "b.txt").write_text("b\n")
            (project / "a.txt").write_text("a\n")
            self.assertEqual(porcelain(project), ["?? a.txt", "?? b.txt"])
            with self.assertRaises(RuntimeError):
                git(project, "config", "--global", "user.name", "x")
        finally:
            subprocess.run(["rm", "-rf", str(root)], check=True)

    def test_change_ids_name_their_scope(self):
        self.assertEqual(change_id("staged", "src/app.txt"), "git-change:staged:src/app.txt")


if __name__ == "__main__":
    unittest.main()
