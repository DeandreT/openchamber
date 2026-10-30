import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import unittest

from native_session_management import ActionFaults, take_rule


class TakeRuleTest(unittest.TestCase):
    def test_a_rule_matches_its_method_and_whole_path_once(self):
        rules = [("POST", r"/api/openchamber/sessions/ses_a/metadata"), ("DELETE", r"/api/session/ses_a")]
        self.assertIsNone(take_rule(rules, "GET", "/api/openchamber/sessions/ses_a/metadata"))
        self.assertIsNone(take_rule(rules, "POST", "/api/openchamber/sessions/ses_a/metadata/extra"))
        self.assertEqual(take_rule(rules, "POST", "/api/openchamber/sessions/ses_a/metadata")[0], "POST")
        self.assertIsNone(take_rule(rules, "POST", "/api/openchamber/sessions/ses_a/metadata"))
        self.assertEqual(len(rules), 1)


class ActionFaultsTest(unittest.TestCase):
    def test_an_armed_request_fails_once_and_others_pass_through(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
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
        proxy = ActionFaults(f"http://127.0.0.1:{upstream.server_port}")
        try:
            proxy.fail_next("POST", r"/api/x")
            statuses = []
            for _ in range(2):
                connection = http.client.HTTPConnection("127.0.0.1", proxy.server.server_port, timeout=5)
                connection.request("POST", "/api/x", body=b"{}", headers={"Content-Length": "2"})
                response = connection.getresponse()
                response.read()
                statuses.append(response.status)
                connection.close()
            self.assertEqual(statuses, [500, 200])
            self.assertEqual(received, ["/api/x"], "the failed request never reached the server")
            self.assertEqual(proxy.failed, [("POST", "/api/x")])
        finally:
            proxy.stop()
            upstream.shutdown()
            upstream.server_close()


if __name__ == "__main__":
    unittest.main()
