import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from itertools import product
import json
import socket
import threading
import unittest

from workflow_proxy import FaultProxy
from native_terminal import TerminalProxy


class WorkflowProxyTest(unittest.TestCase):
    def test_websocket_keeps_the_clients_host_and_origin(self):
        for factory in (lambda url: FaultProxy(url, 'before'), TerminalProxy):
            with self.subTest(proxy=factory):
                self.check_websocket_origin(factory)

    def check_websocket_origin(self, factory):
        listener = socket.socket()
        listener.bind(('127.0.0.1', 0))
        listener.listen(1)
        received = []
        def upstream():
            with listener.accept()[0] as connection:
                data = bytearray()
                while not data.endswith(b'\r\n\r\n'):
                    data.extend(connection.recv(1))
                received.append(data.decode())
                connection.sendall(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n')
        thread = threading.Thread(target=upstream)
        thread.start()
        proxy = factory(f'http://127.0.0.1:{listener.getsockname()[1]}')
        try:
            with socket.create_connection(('127.0.0.1', proxy.server.server_port), timeout=5) as client:
                client.sendall(b'GET /api/global/event/ws HTTP/1.1\r\nHost: localhost:43210\r\nOrigin: http://localhost:43210\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n')
                self.assertIn(b'403', client.recv(4096))
            thread.join(timeout=5)
            self.assertEqual(len(received), 1)
            self.assertIn('Host: localhost:43210\r\n', received[0])
            self.assertIn('Origin: http://localhost:43210\r\n', received[0])
        finally:
            proxy.stop()
            listener.close()
            thread.join(timeout=5)

    def test_loss_before_or_after_admission_and_read_failures_are_explicit(self):
        for operation, mode in product(('prompt', 'create'), ('before', 'after')):
            with self.subTest(operation=operation, mode=mode):
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
                proxy = FaultProxy(f'http://127.0.0.1:{server.server_port}', mode, operation)
                identifier = 'msg_fixed' if operation == 'prompt' else 'ses_fixed'
                path = '/api/session/ses_test/prompt' if operation == 'prompt' else '/api/session'
                lookup = '/api/session/ses_test/inbox' if operation == 'prompt' else '/api/session/ses_fixed'
                body = json.dumps({'id': identifier, 'text': 'synthetic'})
                try:
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    other_path = '/api/session' if operation == 'prompt' else '/api/session/ses_test/prompt'
                    connection.request('POST', other_path, body=json.dumps({'id': 'unaffected'}))
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    response.read()
                    self.assertEqual(proxy.attempt_ids, [])
                    connection.request('POST', path, body=body)
                    with self.assertRaises(http.client.RemoteDisconnected):
                        connection.getresponse()
                    connection.close()
                    self.assertEqual(len(received), 1 if mode == 'before' else 2)
                    connection = http.client.HTTPConnection('127.0.0.1', proxy.server.server_port, timeout=5)
                    connection.request('GET', lookup)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 503)
                    response.read()
                    proxy.pause_reads.clear()
                    connection.request('POST', path, body=body)
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    response.read()
                    connection.close()
                    self.assertEqual(proxy.attempt_ids, [identifier, identifier])
                    recorded = proxy.prompt_ids if operation == 'prompt' else proxy.create_ids
                    self.assertEqual(recorded, [identifier, identifier])
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

    def test_unknown_mutation_target_is_refused(self):
        with self.assertRaises(ValueError):
            FaultProxy('http://127.0.0.1:1234', 'before', 'delete')


if __name__ == '__main__':
    unittest.main()
