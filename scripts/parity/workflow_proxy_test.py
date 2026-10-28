import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import threading
import unittest

from workflow_proxy import FaultProxy


class WorkflowProxyTest(unittest.TestCase):
    def test_loss_before_or_after_admission_and_read_failures_are_explicit(self):
        for mode in ('before', 'after'):
            with self.subTest(mode=mode):
                received = []

                class Handler(BaseHTTPRequestHandler):
                    def log_message(self, *_):
                        pass

                    def do_POST(self):
                        received.append(self.rfile.read(int(self.headers['Content-Length'])))
                        self.send_response(200)
                        self.send_header('Content-Length', '2')
                        self.end_headers()
                        self.wfile.write(b'{}')

                    def do_GET(self):
                        self.send_response(200)
                        self.send_header('Content-Length', '11')
                        self.end_headers()
                        self.wfile.write(b'{"data":[]}')

                server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                proxy = FaultProxy(f'http://127.0.0.1:{server.server_port}', mode)
                body = json.dumps({'id': 'msg_fixed', 'text': 'synthetic'})
                try:
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    connection.request('POST', '/api/session/ses_test/prompt', body=body)
                    with self.assertRaises(http.client.RemoteDisconnected):
                        connection.getresponse()
                    connection.close()
                    self.assertEqual(len(received), 0 if mode == 'before' else 1)
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    connection.request('GET', '/api/session/ses_test/inbox')
                    response = connection.getresponse()
                    self.assertEqual(response.status, 503)
                    response.read()
                    proxy.pause_reads.clear()
                    connection.request('POST', '/api/session/ses_test/prompt', body=body)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    response.read()
                    connection.close()
                    self.assertEqual(proxy.attempt_ids, ['msg_fixed', 'msg_fixed'])
                finally:
                    proxy.stop()
                    server.shutdown()
                    server.server_close()
                    thread.join(timeout=5)
                self.assertFalse(proxy.thread.is_alive())

    def test_non_loopback_upstreams_are_refused(self):
        for url in ('https://example.com', 'http://192.0.2.1:80', 'http://127.0.0.1:80/path'):
            with self.assertRaises(ValueError):
                FaultProxy(url, 'before')


if __name__ == '__main__':
    unittest.main()
