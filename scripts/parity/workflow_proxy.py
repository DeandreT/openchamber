"""Loopback-only response-loss injection for the native workflow check."""

import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import select
import socket
import threading
import urllib.parse


class FaultProxy:
    def __init__(self, upstream, drop_at):
        target = urllib.parse.urlsplit(upstream)
        if target.scheme != "http" or target.hostname != "127.0.0.1" or not target.port or target.path:
            raise ValueError("workflow proxy requires a loopback HTTP origin")
        if drop_at not in ("before", "after"):
            raise ValueError("unknown response-loss mode")
        self.host, self.port = target.hostname, target.port
        self.drop_at = drop_at
        self.pause_reads = threading.Event()
        self.pause_events = threading.Event()
        self.stopped = threading.Event()
        self.attempt_ids = []
        self.lock = threading.Lock()
        self.sockets = set()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.handle(self)

            def do_POST(self):
                owner.handle(self)

            def do_PUT(self):
                owner.handle(self)

            def do_DELETE(self):
                owner.handle(self)

            def do_PATCH(self):
                owner.handle(self)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.server.server_port}"

    def handle(self, handler):
        if not handler.path.startswith("/") or handler.path.startswith("//"):
            handler.send_error(400)
            return
        if handler.headers.get("Upgrade", "").lower() == "websocket":
            self.tunnel(handler)
            return
        path = urllib.parse.urlsplit(handler.path).path
        if self.pause_reads.is_set() and path.startswith("/api/session/") and ("/message" in path or path.endswith("/inbox")):
            handler.send_response(503)
            handler.send_header("Content-Length", "0")
            handler.end_headers()
            return
        length = int(handler.headers.get("Content-Length", 0))
        if not 0 <= length <= 2 * 1024 * 1024:
            handler.send_error(413)
            return
        body = handler.rfile.read(length)
        lose = False
        if handler.command == "POST" and path.endswith("/prompt"):
            identifier = json.loads(body).get("id")
            with self.lock:
                lose = not self.attempt_ids
                self.attempt_ids.append(identifier)
            if lose:
                self.pause_reads.set()
                self.pause_events.set()
                if self.drop_at == "before":
                    handler.close_connection = True
                    handler.connection.shutdown(socket.SHUT_RDWR)
                    return
        headers = {key: value for key, value in handler.headers.items()
                   if key.lower() not in ("host", "connection", "transfer-encoding", "content-length")}
        connection = http.client.HTTPConnection(self.host, self.port, timeout=15)
        try:
            connection.request(handler.command, handler.path, body=body, headers=headers)
            response = connection.getresponse()
            data = response.read()
            if lose:
                handler.close_connection = True
                handler.connection.shutdown(socket.SHUT_RDWR)
                return
            handler.send_response(response.status)
            for key, value in response.getheaders():
                if key.lower() not in ("connection", "transfer-encoding", "content-length"):
                    handler.send_header(key, value)
            handler.send_header("Content-Length", str(len(data)))
            handler.end_headers()
            handler.wfile.write(data)
        except (OSError, http.client.HTTPException):
            handler.close_connection = True
        finally:
            connection.close()

    def tunnel(self, handler):
        upstream = socket.create_connection((self.host, self.port), timeout=10)
        with self.lock:
            self.sockets.update((upstream, handler.connection))
        try:
            headers = "\r\n".join(f"{key}: {value}" for key, value in handler.headers.items() if key.lower() != "host")
            upstream.sendall(f"GET {handler.path} HTTP/1.1\r\nHost: {self.host}:{self.port}\r\n{headers}\r\n\r\n".encode())
            held = bytearray()
            while not self.stopped.is_set():
                if held and not self.pause_events.is_set():
                    handler.connection.sendall(held)
                    held.clear()
                ready, _, _ = select.select([handler.connection, upstream], [], [], 0.1)
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    if source is upstream and self.pause_events.is_set():
                        held.extend(data)
                        if len(held) > 1024 * 1024:
                            return
                    else:
                        target = handler.connection if source is upstream else upstream
                        target.sendall(data)
        except OSError:
            pass
        finally:
            with self.lock:
                self.sockets.discard(upstream)
                self.sockets.discard(handler.connection)
            upstream.close()
            handler.close_connection = True

    def stop(self):
        self.stopped.set()
        with self.lock:
            for connection in self.sockets:
                try:
                    connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
