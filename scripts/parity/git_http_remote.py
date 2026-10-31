"""A loopback smart-HTTP Git remote for the native Git scenarios.

The OpenChamber server plans network operations only for `http`, `https`,
`ssh` and scp-style remotes, so a bare repository on disk is served through
`git http-backend` on 127.0.0.1. `hold_pushes` makes the next receive-pack
request wait until released and refused, which gives a scenario a push in
flight to cancel without permitting a late remote write.
"""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import os
from pathlib import Path
import subprocess
import threading
import urllib.parse
import zlib

MAX_ENCODED_BODY = 16 * 1024 * 1024
MAX_DECODED_BODY = 32 * 1024 * 1024
MAX_TRAILERS = 16 * 1024
BODY_TIMEOUT = 5


class GitHttpRemote:
    """Serves every bare repository under `root` at `/<name>.git`."""

    def __init__(self, root: Path):
        self.root = Path(root)
        self.lock = threading.Lock()
        self.held = threading.Event()
        self.held.set()
        self.holding = False
        self.requests = []
        remote = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def setup(self):
                super().setup()
                self.connection.settimeout(BODY_TIMEOUT)

            def log_message(self, *_):
                pass

            def do_GET(self):
                remote.serve(self)

            def do_POST(self):
                remote.serve(self)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_port}"

    def hold_pushes(self) -> None:
        """Hold receive-pack requests until release refuses them."""
        with self.lock:
            self.holding = True
            self.held = threading.Event()

    def release(self) -> None:
        """Drop held requests without executing their receive-pack bodies."""
        with self.lock:
            self.holding = False
            self.held.set()

    def pushes(self) -> int:
        with self.lock:
            return sum(1 for method, path in self.requests if method == "POST" and path.endswith("git-receive-pack"))

    def serve(self, handler: BaseHTTPRequestHandler) -> None:
        split = urllib.parse.urlsplit(handler.path)
        with self.lock:
            self.requests.append((handler.command, split.path))
            hold = self.holding and split.path.endswith("git-receive-pack")
            held = self.held
        try:
            body = read_body(handler)
        except (ValueError, EOFError, OSError, zlib.error):
            handler.close_connection = True
            try:
                handler.send_error(400, "Invalid request body")
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        if hold:
            held.wait(timeout=120)
            try:
                handler.send_error(503, "Held transfer discarded")
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        env = {
            "PATH": os.environ.get("PATH", ""),
            "GIT_PROJECT_ROOT": str(self.root),
            "GIT_HTTP_EXPORT_ALL": "1",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "HOME": str(self.root),
            "REQUEST_METHOD": handler.command,
            "PATH_INFO": split.path,
            "QUERY_STRING": split.query,
            "CONTENT_TYPE": handler.headers.get("Content-Type", ""),
            "CONTENT_LENGTH": str(len(body)),
            "REMOTE_ADDR": "127.0.0.1",
            # Pushes need an authenticated user unless receive-pack is enabled.
            "REMOTE_USER": "fixture",
            "GIT_PROTOCOL": handler.headers.get("Git-Protocol", ""),
        }
        result = subprocess.run(["git", "http-backend"], input=body, env=env, capture_output=True, timeout=120)
        head, _, payload = result.stdout.partition(b"\r\n\r\n")
        if not _:
            head, _, payload = result.stdout.partition(b"\n\n")
        status = 200
        headers = []
        for line in head.decode("latin-1").splitlines():
            name, _, value = line.partition(":")
            if name.lower() == "status":
                status = int(value.strip().split()[0])
            elif name:
                headers.append((name, value.strip()))
        try:
            handler.send_response(status)
            for name, value in headers:
                handler.send_header(name, value)
            handler.send_header("Content-Length", str(len(payload)))
            handler.end_headers()
            handler.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            # A cancelled transfer closes its connection before the answer.
            pass

    def stop(self) -> None:
        self.release()
        self.server.shutdown()
        self.server.server_close()


def read_body(handler: BaseHTTPRequestHandler) -> bytes:
    """Decode HTTP chunk framing and Git's optional gzip content coding."""
    coding = handler.headers.get("Content-Encoding", "identity").lower().strip()
    if coding not in ("gzip", "identity"):
        raise ValueError("Unsupported content encoding")
    decoder = zlib.decompressobj(16 + zlib.MAX_WBITS) if coding == "gzip" else None
    body = bytearray()
    encoded = 0

    def exact(length):
        data = handler.rfile.read(length)
        if len(data) != length:
            raise EOFError("Truncated request body")
        return data

    def consume(length):
        nonlocal encoded
        encoded += length
        if encoded > MAX_ENCODED_BODY:
            raise ValueError("Encoded body too large")
        while length:
            data = exact(min(length, 65536))
            length -= len(data)
            if decoder is not None:
                data = decoder.decompress(data, MAX_DECODED_BODY - len(body) + 1)
            body.extend(data)
            if len(body) > MAX_DECODED_BODY:
                raise ValueError("Decoded body too large")

    transfer = handler.headers.get("Transfer-Encoding", "").lower().strip()
    if transfer:
        if transfer != "chunked" or handler.headers.get("Content-Length") is not None:
            raise ValueError("Ambiguous or unsupported transfer encoding")
        while True:
            line = handler.rfile.readline(8193)
            encoded += len(line) + 2
            if encoded > MAX_ENCODED_BODY:
                raise ValueError("Chunk framing too large")
            if not line.endswith(b"\r\n") or len(line) > 8192:
                raise ValueError("Invalid chunk header")
            size = line.split(b";", 1)[0].strip()
            if not size or any(byte not in b"0123456789abcdefABCDEF" for byte in size):
                raise ValueError("Invalid chunk size")
            length = int(size, 16)
            if length < 0:
                raise ValueError("Negative chunk size")
            if length == 0:
                trailers = 0
                while True:
                    trailer = handler.rfile.readline(8193)
                    trailers += len(trailer)
                    if trailers > MAX_TRAILERS:
                        raise ValueError("Trailers too large")
                    if trailer == b"\r\n":
                        break
                    if not trailer.endswith(b"\r\n") or len(trailer) > 8192:
                        raise ValueError("Invalid trailer")
                break
            consume(length)
            if exact(2) != b"\r\n":
                raise ValueError("Invalid chunk terminator")
    else:
        length = int(handler.headers.get("Content-Length", "0"))
        if length < 0:
            raise ValueError("Negative content length")
        consume(length)
    if decoder is not None and (not decoder.eof or decoder.unused_data):
        raise ValueError("Incomplete or trailing gzip data")
    return bytes(body)
