from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import http.client
import json
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor

from native_fork_revert import StageFaults, btw_link, message_id, revert_marker, user_prompts


class ReadersTest(unittest.TestCase):
    def test_prompts_follow_server_time_and_skip_other_records(self):
        messages = [
            {"id": "msg_b", "type": "user", "text": "second", "time": {"created": 2}},
            {"id": "msg_x", "type": "assistant", "time": {"created": 3}},
            {"id": "msg_a", "type": "user", "text": "first", "time": {"created": 1}},
            {"id": "msg_s", "type": "synthetic", "text": "ctx", "time": {"created": 0}},
        ]
        self.assertEqual(user_prompts(messages), [("msg_a", "first"), ("msg_b", "second")])

    def test_message_ids_follow_the_native_layout(self):
        identifier = message_id(1_700_000_000_000)
        self.assertEqual(len(identifier), 30)
        self.assertEqual(identifier[4:16], "bcfe56800001")
        self.assertTrue(identifier[16:].isalnum())

    def test_markers_and_links_read_absent_values_as_none(self):
        self.assertEqual(revert_marker({"revert": {"messageID": "msg_2"}}), "msg_2")
        self.assertIsNone(revert_marker({}))
        self.assertIsNone(revert_marker(None))
        self.assertEqual(btw_link({"metadata": {"openchamber": {"btwSessionID": "ses_f"}}}), "ses_f")
        self.assertIsNone(btw_link({"metadata": {"openchamber": {}}}))
        self.assertIsNone(btw_link({"metadata": None}))


class StageFaultsTest(unittest.TestCase):
    def test_prompts_pass_and_only_armed_requests_fail(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_POST(self):
                received.append(self.path)
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        proxy = StageFaults(f"http://127.0.0.1:{upstream.server_port}")
        try:
            proxy.fail_next("POST", r"/api/session/ses_a/revert/stage")

            def post(path):
                body = json.dumps({"id": "msg_1"}).encode()
                connection = http.client.HTTPConnection("127.0.0.1", proxy.server.server_port, timeout=5)
                connection.request("POST", path, body=body, headers={"Content-Length": str(len(body))})
                response = connection.getresponse()
                response.read()
                connection.close()
                return response.status

            self.assertEqual(post("/api/session/ses_a/prompt"), 200, "a prompt's answer arrives")
            self.assertEqual(post("/api/session/ses_a/revert/stage"), 500)
            self.assertEqual(post("/api/session/ses_a/revert/stage"), 200)
            self.assertEqual(received, ["/api/session/ses_a/prompt", "/api/session/ses_a/revert/stage"])
            proxy.hold_next("/api/session/ses_a/fork")
            with ThreadPoolExecutor(max_workers=1) as worker:
                response = worker.submit(post, "/api/session/ses_a/fork")
                try:
                    self.assertTrue(proxy.held.wait(3), "the fork reached the hold")
                    self.assertFalse(response.done(), "no answer before release")
                    self.assertNotIn("/api/session/ses_a/fork", received)
                finally:
                    proxy.release.set()
                self.assertEqual(response.result(timeout=3), 200)
        finally:
            proxy.stop()
            upstream.shutdown()
            upstream.server_close()


if __name__ == "__main__":
    unittest.main()
