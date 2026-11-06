"""Loopback proxy that applies scenario fault rules between the native client
and its isolated server, the native half of the runner's `fault` step.

The approach of `workflow_proxy.py`, driven by `steps.fault_rule` rules: fail
a matched request with an HTTP status, drop its connection, or hold it until
released, either before the server receives it or after the server answered.
Unmatched requests and WebSocket upgrades pass through unchanged, with the
client's Host and Origin headers; streamed responses are relayed as they
arrive.
"""

from __future__ import annotations

import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import select
import socket
import threading
import urllib.parse

import steps

HOP_BY_HOP = ("connection", "keep-alive", "transfer-encoding", "content-length", "upgrade", "proxy-connection", "te", "trailer")
MAX_BODY = 32 * 1024 * 1024
FAULT_BODY = json.dumps({"error": "parity fault"}).encode()


class RuleFaultProxy:
    def __init__(self, upstream: str):
        target = urllib.parse.urlsplit(upstream)
        if target.scheme != "http" or target.hostname != "127.0.0.1" or not target.port or target.path:
            raise ValueError("the fault proxy requires a loopback HTTP origin")
        self.host, self.port = target.hostname, target.port
        self.lock = threading.Lock()
        self.rules: list[dict] = []
        self.hits: list[dict] = []
        self.releases: dict[str, threading.Event] = {}
        self.stopped = threading.Event()
        self.sockets: set[socket.socket] = set()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_GET(self):
                owner.handle(self)

            do_POST = do_PUT = do_DELETE = do_PATCH = do_HEAD = do_OPTIONS = do_GET

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    # --- rules -------------------------------------------------------------

    def add(self, rule: dict) -> None:
        with self.lock:
            if any(existing["id"] == rule["id"] for existing in self.rules):
                raise ValueError(f"fault {rule['id']} is already declared")
            self.rules.append({**rule, "remaining": rule["times"]})
            self.releases[rule["id"]] = threading.Event()

    def release(self, fault_id: str) -> None:
        with self.lock:
            if fault_id not in self.releases:
                raise LookupError(f"no fault {fault_id}")
            self.releases[fault_id].set()

    def hit_list(self) -> list[dict]:
        with self.lock:
            return [dict(hit) for hit in self.hits]

    def _claim(self, method: str, path: str) -> dict | None:
        """The first matching rule with hits left, counted as hit."""
        with self.lock:
            for rule in self.rules:
                if rule["remaining"] > 0 and steps.fault_matches(rule, method, path):
                    rule["remaining"] -= 1
                    self.hits.append({"id": rule["id"], "method": method, "path": urllib.parse.urlsplit(path).path,
                                      "when": rule["when"], "action": rule["action"]})
                    return rule
        return None

    # --- requests ----------------------------------------------------------

    def handle(self, handler: BaseHTTPRequestHandler) -> None:
        if not handler.path.startswith("/") or handler.path.startswith("//"):
            handler.send_error(400)
            return
        if handler.headers.get("Upgrade", "").lower() == "websocket":
            self.tunnel(handler)
            return
        length = int(handler.headers.get("Content-Length", 0) or 0)
        if not 0 <= length <= MAX_BODY:
            handler.send_error(413)
            return
        body = handler.rfile.read(length) if length else None
        rule = self._claim(handler.command, handler.path)
        if rule is not None and rule["when"] == "before":
            if rule["action"] == "hold":
                if not self._wait_release(rule) or self._client_gone(handler):
                    self._drop(handler)
                    return
            elif rule["action"] == "fail":
                self._fail(handler, rule["status"])
                return
            else:
                self._drop(handler)
                return
        headers = {key: value for key, value in handler.headers.items() if key.lower() not in HOP_BY_HOP}
        connection = http.client.HTTPConnection(self.host, self.port, timeout=60)
        try:
            # The client's Host header stays: http.client adds none of its own.
            connection.request(handler.command, handler.path, body=body, headers=headers)
            response = connection.getresponse()
            if rule is not None and rule["when"] == "after":
                if rule["action"] == "hold":
                    data = response.read()
                    if not self._wait_release(rule) or self._client_gone(handler):
                        self._drop(handler)
                        return
                    self._respond(handler, response, data)
                elif rule["action"] == "fail":
                    response.read()
                    self._fail(handler, rule["status"], response.getheaders())
                else:
                    response.read()
                    self._drop(handler)
                return
            self._relay(handler, response)
        except (OSError, http.client.HTTPException):
            handler.close_connection = True
        finally:
            connection.close()

    def _wait_release(self, rule: dict) -> bool:
        event = self.releases[rule["id"]]
        while not self.stopped.is_set():
            if event.wait(0.1):
                return True
        return False

    @staticmethod
    def _client_gone(handler) -> bool:
        """A held request the client already abandoned is not sent on."""
        readable, _, _ = select.select([handler.connection], [], [], 0)
        if not readable:
            return False
        try:
            return handler.connection.recv(1, socket.MSG_PEEK) == b""
        except OSError:
            return True

    @staticmethod
    def _drop(handler) -> None:
        handler.close_connection = True
        try:
            handler.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass

    @staticmethod
    def _fail(handler, status: int, upstream_headers=()) -> None:
        handler.send_response(status)
        for key, value in upstream_headers:
            if key.lower() not in HOP_BY_HOP and key.lower() not in ("content-type", "content-encoding"):
                handler.send_header(key, value)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(FAULT_BODY)))
        handler.end_headers()
        handler.wfile.write(FAULT_BODY)

    @staticmethod
    def _respond(handler, response, data: bytes) -> None:
        handler.send_response(response.status)
        for key, value in response.getheaders():
            if key.lower() not in HOP_BY_HOP:
                handler.send_header(key, value)
        handler.send_header("Content-Length", str(len(data)))
        handler.end_headers()
        handler.wfile.write(data)

    def _relay(self, handler, response) -> None:
        """Bodies with a length are forwarded whole; streams chunk by chunk."""
        if response.getheader("Content-Length") is not None or handler.command == "HEAD" or response.status in (204, 304):
            self._respond(handler, response, response.read())
            return
        handler.send_response(response.status)
        for key, value in response.getheaders():
            if key.lower() not in HOP_BY_HOP:
                handler.send_header(key, value)
        handler.send_header("Transfer-Encoding", "chunked")
        handler.end_headers()
        while not self.stopped.is_set():
            chunk = response.read1(65536)
            if not chunk:
                break
            handler.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
            handler.wfile.flush()
        handler.wfile.write(b"0\r\n\r\n")

    def tunnel(self, handler) -> None:
        upstream = socket.create_connection((self.host, self.port), timeout=10)
        with self.lock:
            self.sockets.update((upstream, handler.connection))
        try:
            headers = "".join(f"{key}: {value}\r\n" for key, value in handler.headers.items())
            upstream.sendall(f"GET {handler.path} HTTP/1.1\r\n{headers}\r\n".encode())
            while not self.stopped.is_set():
                ready, _, _ = select.select([handler.connection, upstream], [], [], 0.1)
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    (handler.connection if source is upstream else upstream).sendall(data)
        except OSError:
            pass
        finally:
            with self.lock:
                self.sockets.discard(upstream)
                self.sockets.discard(handler.connection)
            upstream.close()
            handler.close_connection = True

    def stop(self) -> None:
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
