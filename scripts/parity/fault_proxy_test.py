"""Rule faults through the native proxy against a loopback upstream that
records what it received. Run with `python3 -m unittest fault_proxy_test`."""

import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading
import time
import unittest

from fault_proxy import RuleFaultProxy
import steps


class Upstream:
    def __init__(self):
        self.received = []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                owner.received.append((self.path, body, self.headers.get("Host")))
                self.send_response(201)
                self.send_header("Content-Type", "application/json")
                self.send_header("Access-Control-Allow-Origin", "openchamber-ui://app")
                self.send_header("Content-Length", "11")
                self.end_headers()
                self.wfile.write(b'{"ok":true}')

            def do_GET(self):
                owner.received.append((self.path, b"", self.headers.get("Host")))
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for index in range(3):
                    self.wfile.write(f"data: {index}\n\n".encode())
                    self.wfile.flush()
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def stop(self):
        self.server.shutdown()
        self.server.server_close()


class FaultProxyTest(unittest.TestCase):
    def setUp(self):
        self.upstream = Upstream()
        self.addCleanup(self.upstream.stop)
        self.proxy = RuleFaultProxy(self.upstream.url)
        self.addCleanup(self.proxy.stop)

    def fault(self, **fields):
        self.proxy.add(steps.fault_rule({"id": "f", "method": "POST", "path": "/api/session/*/message", **fields}, "native"))

    def post(self, path="/api/session/s1/message"):
        connection = http.client.HTTPConnection("127.0.0.1", self.proxy.server.server_port, timeout=10)
        try:
            connection.request("POST", path, body=b'{"id":"m1"}', headers={"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, response.read(), dict(response.getheaders())
        finally:
            connection.close()

    def test_unmatched_requests_pass_with_the_clients_host(self):
        self.assertEqual(self.post("/api/other")[0], 201)
        self.assertEqual(self.upstream.received[0][2], f"127.0.0.1:{self.proxy.server.server_port}")
        self.assertEqual(self.proxy.hit_list(), [])

    def test_fail_before_never_reaches_the_server(self):
        self.fault(when="before", action="fail", status=503)
        status, body, _ = self.post()
        self.assertEqual((status, body), (503, b'{"error": "parity fault"}'))
        self.assertEqual(self.upstream.received, [])
        self.assertEqual(self.post()[0], 201, "one hit by default")
        self.assertEqual(len(self.proxy.hit_list()), 1)

    def test_fail_after_keeps_cors_and_replaces_the_answer(self):
        self.fault(when="after", action="fail")
        status, _, headers = self.post()
        self.assertEqual(status, 500)
        self.assertEqual(headers["Access-Control-Allow-Origin"], "openchamber-ui://app")
        self.assertEqual(len(self.upstream.received), 1)

    def test_drop_before_and_after(self):
        for when, received in (("before", 0), ("after", 1)):
            with self.subTest(when=when):
                self.upstream.received.clear()
                self.proxy.add(steps.fault_rule({"id": when, "method": "POST", "path": "/api/*", "when": when, "action": "drop"}, "native"))
                with self.assertRaises((http.client.HTTPException, OSError)):
                    self.post()
                self.assertEqual(len(self.upstream.received), received)

    def test_hold_waits_for_release(self):
        self.fault(when="before", action="hold")
        result = []
        thread = threading.Thread(target=lambda: result.append(self.post()))
        thread.start()
        time.sleep(0.5)
        self.assertEqual((self.upstream.received, result), ([], []))
        self.proxy.release("f")
        thread.join(10)
        self.assertEqual(result[0][0], 201)
        self.assertEqual(len(self.upstream.received), 1)

    def test_streams_are_relayed(self):
        connection = http.client.HTTPConnection("127.0.0.1", self.proxy.server.server_port, timeout=10)
        connection.request("GET", "/api/event")
        response = connection.getresponse()
        self.assertEqual(response.read(), b"data: 0\n\ndata: 1\n\ndata: 2\n\n")
        connection.close()

    def test_only_loopback_upstreams(self):
        with self.assertRaises(ValueError):
            RuleFaultProxy("http://example.com:80")


if __name__ == "__main__":
    unittest.main()
